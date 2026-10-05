//! M9.3: the runtime plays media on its own (`tenuto play`) without touching
//! the saved playlists.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

use std::sync::Arc;
use std::time::Duration;

use runtime::{enqueue, pump_for, pump_until, rig_with, row_ids};
use support::server::{Script, TestServer};
use tenuto::application::runtime::{AppCommand, EnqueueItem};
use tenuto::application::transport::PlaybackPhase;
use tenuto::application::view::PlayerView;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::id::{AbsolutePath, EpisodeKey, FeedId, MediaId};
use tenuto::media::source::SourceLocation;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::state::PlaybackState;
use url::Url;

const FIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");
const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");

fn local(path: &str) -> (MediaId, SourceLocation) {
    let path = AbsolutePath::new(path.into()).unwrap_or_else(|error| panic!("absolute: {error}"));
    (
        MediaId::LocalFile(path.clone()),
        SourceLocation::LocalPath(path.as_path().to_path_buf()),
    )
}

fn playing(view: &PlayerView) -> bool {
    view.now_playing
        .as_ref()
        .is_some_and(|now| now.loaded && now.state == PlaybackState::Playing)
}

fn position(view: &PlayerView) -> Duration {
    view.now_playing
        .as_ref()
        .map_or(Duration::ZERO, |now| now.position)
}

fn past(seconds: u64) -> impl Fn(&PlayerView) -> bool {
    move |view| position(view) >= Duration::from_secs(seconds)
}

#[test]
fn a_detached_play_shows_its_own_media_and_keeps_the_cursor() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(SHORT.into())]);
    let row = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(row));
    pump_until(&mut rig, "the row plays", playing);
    rig.runtime.handle(AppCommand::Stop);

    let (media, location) = local(FIVE);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "the detached track plays", playing);
    let view = rig.runtime.view();
    let now = view.now_playing.unwrap_or_else(|| panic!("now playing"));
    assert_eq!(now.entry, None);
    assert_eq!(now.title, "sine-5s.flac");
    assert_eq!(view.active, Some(row), "the cursor stays on the row");
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_detached_track_that_ends_loads_nothing_else() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(FIVE.into())]);
    let (media, location) = local(SHORT);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "ended", |view| view.phase == PlaybackPhase::Ended);
    pump_for(&mut rig, Duration::from_millis(300));
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Ended);
    assert_eq!(
        rig.runtime.session().pending_load_count(),
        0,
        "no row was loaded"
    );
    let _ = rig.runtime.shutdown();
}

#[test]
fn stop_then_play_resumes_a_detached_track_where_it_stopped() {
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(FIVE);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "past one second", past(1));
    rig.runtime.handle(AppCommand::Stop);
    pump_until(&mut rig, "stopped", |view| {
        view.phase == PlaybackPhase::Stopped
    });
    let stopped_at = position(&rig.runtime.view());
    rig.runtime.handle(AppCommand::Play);
    pump_until(&mut rig, "playing again", playing);
    let resumed = position(&rig.runtime.view());
    assert!(resumed >= stopped_at, "{resumed:?} < {stopped_at:?}");
    let _ = rig.runtime.shutdown();
}

#[test]
fn home_restarts_an_ended_detached_track() {
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(SHORT);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "ended", |view| view.phase == PlaybackPhase::Ended);
    rig.runtime.handle(AppCommand::Restart);
    pump_until(&mut rig, "restarted", |view| {
        view.phase != PlaybackPhase::Ended
    });
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_detached_seek_while_stopped_flushes_with_an_empty_playlist() {
    // The playing playlist is empty, so the playlist table would refuse the
    // flush.
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(FIVE);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "past one second", past(1));
    rig.runtime.handle(AppCommand::Stop);
    pump_until(&mut rig, "stopped", |view| {
        view.phase == PlaybackPhase::Stopped
    });
    rig.runtime.handle(AppCommand::SeekBy(2));
    // Past the 250 ms quiet window: the burst has flushed, so no deadline is
    // left to shorten the poll.
    pump_for(&mut rig, Duration::from_millis(400));
    assert_eq!(
        rig.runtime.poll_budget(Duration::from_millis(100)),
        Duration::from_millis(100)
    );
    let _ = rig.runtime.shutdown();
}

/// A podcast episode: the one kind of media that resumes at its checkpoint.
fn episode(server: &TestServer) -> (MediaId, SourceLocation) {
    let media = MediaId::PodcastEpisode {
        feed: FeedId::new("detached-feed".into()).unwrap_or_else(|error| panic!("feed: {error}")),
        episode: EpisodeKey::resolve(Some("e1"), None, None)
            .unwrap_or_else(|error| panic!("episode: {error}")),
    };
    let url = Url::parse(&server.url("/ep.flac")).unwrap_or_else(|error| panic!("url: {error}"));
    (media, SourceLocation::Http(url))
}

#[test]
fn home_while_a_detached_load_is_pending_restarts_it() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    // A first run leaves a checkpoint past three seconds.
    let mut first = rig_with(PersistedState::default());
    let (media, location) = episode(&server);
    first.runtime.play_detached(media.clone(), location.clone());
    pump_until(&mut first, "past three seconds", past(3));
    let _ = first.runtime.shutdown();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
    let saved = StateStore::new(first.state_path.clone(), clock)
        .load()
        .state;
    assert!(
        saved
            .entry_for(&media)
            .and_then(|entry| entry.position)
            .is_some_and(|position| position >= Duration::from_secs(3)),
        "the first run saved a checkpoint past three seconds"
    );

    // Home before a single pump: no `Loaded` has been drained, no mirror.
    let mut rig = rig_with(saved);
    rig.runtime.play_detached(media, location);
    rig.runtime.handle(AppCommand::Restart);
    // `PlayLoaded` plays from the resumed position first; the queued restart
    // lands once its reopen completes. Without the fix it never does, and
    // this times out at the rig's patience.
    pump_until(&mut rig, "restarted and playing", |view| {
        playing(view) && position(view) < Duration::from_secs(2)
    });
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

#[test]
fn stop_keeps_a_stored_seek_target_for_the_next_arrow() {
    // Stop → seek → wait → Stop → arrow.
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(FIVE);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "past one second", past(1));
    rig.runtime.handle(AppCommand::Stop);
    pump_until(&mut rig, "stopped", |view| {
        view.phase == PlaybackPhase::Stopped
    });
    rig.runtime.handle(AppCommand::SeekBy(2));
    // Past the 250 ms quiet window, so the seek is submitted and the engine
    // has stored it; before then a Stop rightly drops the unsent burst.
    pump_for(&mut rig, Duration::from_millis(400));
    let stored = position(&rig.runtime.view());
    assert!(stored >= Duration::from_secs(3), "{stored:?}");
    rig.runtime.handle(AppCommand::Stop);
    pump_for(&mut rig, Duration::from_millis(100));
    assert_eq!(position(&rig.runtime.view()), stored);
    rig.runtime.handle(AppCommand::SeekBy(1));
    assert_eq!(
        position(&rig.runtime.view()),
        stored + Duration::from_secs(1),
        "the arrow accumulates from the stored target"
    );
    // Play resumes at the stored target; the arrow's seek lands once its
    // quiet window passes.
    rig.runtime.handle(AppCommand::Play);
    let target = stored + Duration::from_secs(1);
    pump_until(
        &mut rig,
        "playing from the accumulated target",
        move |view| playing(view) && position(view) >= target,
    );
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_detached_load_failure_reports_load_failed_with_its_message() {
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local("/nonexistent/definitely-not-here.flac");
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "failed", |view| {
        view.phase == PlaybackPhase::LoadFailed
    });
    assert!(
        rig.runtime
            .view()
            .status
            .is_some_and(|status| !status.is_empty())
    );
    let _ = rig.runtime.shutdown();
}
