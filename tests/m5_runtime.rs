mod support;

#[path = "support/feeds.rs"]
mod feeds;

#[path = "support/runtime.rs"]
mod runtime;

#[path = "support/tagged_flac.rs"]
mod tagged_flac;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use runtime::{
    enqueue, null_engine, parts, pump_for, pump_until, rig_with, rig_with_parts, rig_with_probe,
    row_ids,
};
use support::server::{DocumentReply, Script, TestServer};
use tenuto::application::enrich::default_probe;
use tenuto::application::runtime::{
    AppCommand, EnqueueItem, FlushReport, LibraryStores, PlayerRuntime,
};
use tenuto::application::transport::PlaybackPhase;
use tenuto::application::view::{NowPlaying, PersistenceStatus, PlayerView};
use tenuto::clock::SystemClock;
use tenuto::feed::cache::CacheStore;
use tenuto::lifecycle::hooks::TestHook;
use tenuto::media::id::{AbsolutePath, EpisodeKey, FeedId, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::writer::WriterHandle;
use tenuto::queue::{NewQueueEntry, QueueEntryId, QueueSource};
use tenuto::session::Session;
use tenuto::station::store::StationStore;
use tenuto::subscription::store::SubscriptionStore;

const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const FIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");
const MISSING: &str = "/nonexistent/m5-missing.flac";

fn now_playing(view: &PlayerView) -> NowPlaying {
    view.now_playing
        .clone()
        .unwrap_or_else(|| panic!("something is active"))
}

fn local_media(path: &Path) -> MediaId {
    MediaId::LocalFile(
        AbsolutePath::new(path.to_path_buf()).unwrap_or_else(|error| panic!("absolute: {error}")),
    )
}

fn local_entry(path: &Path) -> NewQueueEntry {
    let absolute =
        AbsolutePath::new(path.to_path_buf()).unwrap_or_else(|error| panic!("absolute: {error}"));
    NewQueueEntry::new(
        MediaId::LocalFile(absolute.clone()),
        QueueSource::LocalFile(absolute),
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("entry: {error}"))
}

fn seeded(entries: Vec<NewQueueEntry>) -> PersistedState {
    let mut session = Session::new(PersistedState::default());
    session
        .enqueue(session.state().playing(), entries)
        .unwrap_or_else(|error| panic!("fits: {error}"));
    session.state().clone()
}

/// A media directory holding a copy of the 5-second fixture under each name.
fn media_dir(names: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let root = dir
        .path()
        .canonicalize()
        .unwrap_or_else(|error| panic!("canonical tempdir: {error}"));
    for name in names {
        std::fs::copy(FIVE, root.join(name))
            .unwrap_or_else(|error| panic!("copy fixture: {error}"));
    }
    (dir, root)
}

/// The furthest position history holds for `media`, established or estimated.
fn saved_at(runtime: &PlayerRuntime, media: &MediaId) -> Duration {
    runtime
        .session()
        .state()
        .entry_for(media)
        .map(|entry| {
            entry
                .position
                .unwrap_or_default()
                .max(entry.estimated.unwrap_or_default())
        })
        .unwrap_or_default()
}

fn is_playing(view: &PlayerView, id: QueueEntryId) -> bool {
    view.phase == PlaybackPhase::Playing
        && view
            .now_playing
            .as_ref()
            .is_some_and(|now| now.loaded && now.entry == Some(id))
}

#[test]
fn enter_plays_the_selected_entry_and_adopts_only_it() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(
        &mut rig.runtime,
        vec![
            EnqueueItem::Path(SHORT.into()),
            EnqueueItem::Path(SHORT.into()),
        ],
    );
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    // `Loaded` adopts the row while the engine is still paused; the start
    // follows on a later pass.
    pump_until(&mut rig.runtime, "second row active and playing", |view| {
        view.active == Some(ids[1])
            && matches!(view.phase, PlaybackPhase::Playing | PlaybackPhase::Ended)
    });
}

#[test]
fn completion_advances_once_and_the_last_entry_stays_ended() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(
        &mut rig.runtime,
        vec![
            EnqueueItem::Path(SHORT.into()),
            EnqueueItem::Path(SHORT.into()),
        ],
    );
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "advanced to the second row", |view| {
        view.active == Some(ids[1])
    });
    pump_until(&mut rig.runtime, "queue ended", |view| {
        view.phase == PlaybackPhase::Ended
    });
    assert_eq!(
        rig.runtime.view().active,
        Some(ids[1]),
        "no wrap back to the first row"
    );
}

#[test]
fn a_failed_load_keeps_the_queue_and_does_not_skip() {
    let state = seeded(vec![local_entry(Path::new(MISSING))]);
    let mut rig = rig_with(state);
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(SHORT.into())]);
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "load failed", |view| {
        view.phase == PlaybackPhase::LoadFailed
    });
    let view = rig.runtime.view();
    assert_eq!(view.rows.len(), 2);
    assert_eq!(view.active, None);
    assert!(view.status.as_deref().is_some_and(|s| !s.is_empty()));
}

#[test]
fn space_before_loading_loads_the_restored_active_entry() {
    let path = std::fs::canonicalize(SHORT).expect("fixture");
    let key = serde_json::to_value(MediaId::LocalFile(
        AbsolutePath::new(path.clone()).expect("absolute"),
    ))
    .expect("key");
    let file = serde_json::json!({ "schema_version": 3, "current_media": key, "volume": 1.0, "checkpoints": {},
        "queue": [{ "id": 1, "media": "local:/music/other.flac", "source": { "kind": "local", "path": "/music/other.flac" } },
                  { "id": 2, "media": key, "source": { "kind": "local", "path": path }}],
        "active_entry": 2 });
    let mut rig = rig_with(serde_json::from_value(file).expect("valid"));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayPause);
    pump_until(&mut rig.runtime, "active entry loaded", |view| {
        view.now_playing
            .as_ref()
            .is_some_and(|now| now.loaded && now.entry == Some(ids[1]))
    });
}

#[test]
fn seeking_before_any_load_is_a_notice_and_opens_nothing() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(SHORT.into())]);
    rig.runtime.handle(AppCommand::SeekBy(10));
    assert_eq!(
        rig.runtime.view().status.as_deref(),
        Some("Play a track before seeking")
    );
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Unloaded);
}

#[test]
fn volume_without_an_engine_is_persisted_at_shutdown() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::AdjustVolume(-0.25));
    let state_path = rig.state_path.clone();
    assert!(matches!(rig.runtime.shutdown(), FlushReport::Written));
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_path).expect("written")).expect("json");
    assert_eq!(written["volume"], 0.75);
}

fn shows_tags(view: &PlayerView) -> bool {
    view.rows.iter().any(|row| {
        row.title == "Harbor – Morning Tide"
            && row
                .subtitle
                .as_deref()
                .is_some_and(|subtitle| subtitle.contains("Coast"))
    })
}

#[test]
fn enqueueing_a_tagged_local_file_fills_title_and_artist_in_the_background() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = tagged_flac::tagged_flac(dir.path(), "Morning Tide", "Harbor", "Coast", None);
    let mut rig = rig_with_probe(PersistedState::default(), default_probe(TestHook::None));
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(path)]);
    assert_eq!(
        rig.runtime.view().rows[0].title,
        "tagged.flac",
        "not yet enriched"
    );
    pump_until(&mut rig.runtime, "tags shown on the row", shows_tags);
    let row = rig.runtime.view().rows[0].clone();
    assert_eq!(row.subtitle.as_deref(), Some("Coast"));
    assert!(row.duration.is_some(), "{row:?}");
}

#[test]
fn restored_untitled_local_entries_are_enriched_on_the_first_pump() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = tagged_flac::tagged_flac(dir.path(), "Morning Tide", "Harbor", "Coast", None);
    let mut rig = rig_with_probe(
        seeded(vec![local_entry(&path)]),
        default_probe(TestHook::None),
    );
    pump_until(&mut rig.runtime, "restored row enriched", shows_tags);
}

#[test]
fn loading_a_tagged_file_fills_artist_and_album_from_the_decoder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = tagged_flac::tagged_flac(dir.path(), "Morning Tide", "Harbor", "Coast", None);
    // Enrichment disabled: only the load itself can supply the tags.
    let mut rig = rig_with(PersistedState::default());
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(path)]);
    let id = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(id));
    pump_until(&mut rig.runtime, "tags adopted with the load", shows_tags);
    let now = now_playing(&rig.runtime.view());
    assert_eq!(
        (now.artist.as_deref(), now.album.as_deref()),
        (Some("Harbor"), Some("Coast"))
    );
}

#[test]
fn an_oversized_enqueue_is_rejected_whole_with_a_visible_message() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(
        &mut rig.runtime,
        (0..4097).map(|_| EnqueueItem::Path(SHORT.into())).collect(),
    );
    assert!(rig.runtime.view().rows.is_empty());
    assert_eq!(
        rig.runtime.view().status.as_deref(),
        Some("Playlists are full (4096 entries in total)")
    );
}

#[test]
fn two_loads_of_one_media_submitted_together_end_on_the_later_row() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(
        &mut rig.runtime,
        vec![
            EnqueueItem::Path(SHORT.into()),
            EnqueueItem::Path(SHORT.into()),
        ],
    );
    let ids = row_ids(&rig.runtime);
    // Both are submitted before a single event is drained.
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    assert_eq!(rig.runtime.session().pending_load_count(), 2);
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.runtime.view().active != Some(ids[1]) {
        rig.runtime.pump();
        if let Some(active) = rig.runtime.view().active
            && seen.last() != Some(&active)
        {
            seen.push(active);
        }
        assert!(Instant::now() < deadline, "never adopted the later row");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(rig.runtime.session().pending_load_count(), 0);
    assert!(
        !seen.windows(2).any(|pair| pair == [ids[1], ids[0]]),
        "never regressed to the earlier row: {seen:?}"
    );
}

struct FailingSink;
impl tenuto::persistence::writer::StateSink for FailingSink {
    fn write(&self, _: &PersistedState) -> Result<(), tenuto::persistence::PersistenceError> {
        Err(tenuto::persistence::PersistenceError::NoStateDirectory)
    }
}

#[test]
fn a_failed_final_flush_is_reported_not_claimed_as_saved() {
    let clock: Arc<dyn tenuto::clock::Clock> = Arc::new(SystemClock);
    let writer = WriterHandle::spawn(Box::new(FailingSink), clock.clone());
    let mut runtime = PlayerRuntime::new(parts(
        PersistedState::default(),
        writer,
        clock,
        None,
        null_engine(),
    ));
    runtime.handle(AppCommand::AdjustVolume(-0.1));
    assert!(matches!(runtime.shutdown(), FlushReport::Failed(_)));
}

#[test]
fn a_failing_writer_is_shown_while_the_session_runs() {
    let clock: Arc<dyn tenuto::clock::Clock> = Arc::new(SystemClock);
    let writer = WriterHandle::spawn(Box::new(FailingSink), clock.clone());
    let mut runtime = PlayerRuntime::new(parts(
        PersistedState::default(),
        writer,
        clock,
        None,
        null_engine(),
    ));
    assert_eq!(runtime.view().persistence, PersistenceStatus::Saving);
    runtime.handle(AppCommand::AdjustVolume(-0.1));
    pump_until(&mut runtime, "the failed write is shown", |view| {
        view.persistence == PersistenceStatus::Failing
    });
    assert!(matches!(runtime.shutdown(), FlushReport::Failed(_)));
}

// ------------------------------------------------------------- regressions

#[derive(Clone, Copy, Debug)]
enum Superseding {
    Load,
    /// A load of a remote B whose response is delayed past the burst's quiet
    /// window, so the burst comes due while B is still loading.
    StalledLoad,
    Stop,
    RemoveActive,
    Clear,
    SeekTo,
}

/// A position a leaked burst would have reached. The burst is `SeekBy(3)`
/// from just after A starts, and every legitimate position these checks
/// read stays well below it.
const LEAKED: Duration = Duration::from_millis(2500);

/// Starts A playing with a forward burst standing, supersedes it with `how`
/// before the burst's quiet window closes, then pumps well past that window.
fn supersede_a_burst(how: Superseding) {
    let (_media, root) = media_dir(&["a.flac", "b.flac"]);
    let (a_path, b_path) = (root.join("a.flac"), root.join("b.flac"));
    // Answers only after the burst's quiet window has long passed.
    let server = delayed_fixture("/b.flac", Duration::from_millis(600));
    let b_item = match how {
        Superseding::StalledLoad => EnqueueItem::Url(server.url("/b.flac")),
        _ => EnqueueItem::Path(b_path.clone()),
    };
    let mut rig = rig_with(PersistedState::default());
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::Path(a_path.clone()), b_item],
    );
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "A playing with a duration", |view| {
        is_playing(view, ids[0])
            && view
                .now_playing
                .as_ref()
                .is_some_and(|n| n.duration.is_some())
    });

    rig.runtime.handle(AppCommand::SeekBy(3));
    match how {
        Superseding::Load | Superseding::StalledLoad => {
            rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
        }
        Superseding::Stop => rig.runtime.handle(AppCommand::Stop),
        Superseding::RemoveActive => rig.runtime.handle(AppCommand::Remove(ids[0])),
        Superseding::Clear => {
            let viewed = rig.runtime.viewed();
            rig.runtime.handle(AppCommand::ClearPlaylist(viewed));
        }
        Superseding::SeekTo => rig
            .runtime
            .handle(AppCommand::SeekTo(Duration::from_secs(1))),
    }

    match how {
        Superseding::Load | Superseding::StalledLoad => {
            if let Superseding::StalledLoad = how {
                pump_for(&mut rig.runtime, Duration::from_millis(400));
                assert_eq!(rig.runtime.view().phase, PlaybackPhase::Loading);
            }
            pump_until(&mut rig.runtime, "B playing", |view| {
                is_playing(view, ids[1])
            });
            pump_for(&mut rig.runtime, Duration::from_millis(300));
            let view = rig.runtime.view();
            assert!(
                is_playing(&view, ids[1]),
                "{how:?}: B stays playing: {view:?}"
            );
            assert!(
                now_playing(&view).position < LEAKED,
                "{how:?}: B near zero: {view:?}"
            );
            let b_media = rig
                .runtime
                .session()
                .state()
                .current_media()
                .cloned()
                .unwrap_or_else(|| panic!("B is current"));
            assert!(saved_at(&rig.runtime, &b_media) < LEAKED, "{how:?}");
        }
        Superseding::Stop => {
            pump_for(&mut rig.runtime, Duration::from_millis(400));
            let view = rig.runtime.view();
            assert_eq!(view.phase, PlaybackPhase::Stopped, "{how:?}: {view:?}");
            assert!(now_playing(&view).position < LEAKED, "{how:?}: {view:?}");
        }
        Superseding::RemoveActive => {
            pump_for(&mut rig.runtime, Duration::from_millis(400));
            let view = rig.runtime.view();
            assert_eq!(view.phase, PlaybackPhase::Unloaded, "{how:?}: {view:?}");
            assert_eq!(view.rows.len(), 1);
        }
        Superseding::Clear => {
            pump_for(&mut rig.runtime, Duration::from_millis(400));
            let view = rig.runtime.view();
            assert!(view.rows.is_empty());
            assert_eq!(view.phase, PlaybackPhase::Unloaded, "{how:?}: {view:?}");
        }
        Superseding::SeekTo => {
            pump_until(&mut rig.runtime, "landed at the absolute target", |view| {
                view.now_playing
                    .as_ref()
                    .is_some_and(|now| now.position >= Duration::from_secs(1))
            });
            pump_for(&mut rig.runtime, Duration::from_millis(300));
            let view = rig.runtime.view();
            assert!(
                is_playing(&view, ids[0]),
                "{how:?}: A still playing: {view:?}"
            );
            assert!(now_playing(&view).position < LEAKED, "{how:?}: {view:?}");
        }
    }
    assert!(
        saved_at(&rig.runtime, &local_media(&a_path)) < LEAKED,
        "{how:?}: A's history never advanced to the discarded target"
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

#[test]
fn seek_then_load_discards_the_old_target() {
    for how in [
        Superseding::Load,
        Superseding::StalledLoad,
        Superseding::Stop,
        Superseding::RemoveActive,
        Superseding::Clear,
        Superseding::SeekTo,
    ] {
        supersede_a_burst(how);
    }
}

/// A remote document of the 5-second fixture whose headers arrive only
/// after `delay`, so its load stays pending that long.
fn delayed_fixture(path: &str, delay: Duration) -> TestServer {
    TestServer::start(Script::documents(vec![DocumentReply {
        path: path.into(),
        status: 200,
        headers: vec![("Content-Type".into(), "audio/flac".into())],
        body: std::fs::read(FIVE).unwrap_or_else(|error| panic!("fixture: {error}")),
        conditional: false,
        header_delay: delay,
    }]))
}

#[test]
fn a_refused_load_drained_with_a_later_adoption_does_not_stop_the_adopted_track() {
    let (_media, root) = media_dir(&["a.flac", "b.flac"]);
    let mut rig = rig_with(seeded(vec![
        local_entry(&root.join("a.flac")),
        local_entry(&root.join("b.flac")),
    ]));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    // Invalidates A's pending load; its `Loaded` asks for a stop.
    rig.runtime.handle(AppCommand::Remove(ids[0]));
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    // Both outcomes are waiting when the first pump drains them together.
    std::thread::sleep(Duration::from_secs(1));
    pump_until(&mut rig.runtime, "B playing", |view| {
        is_playing(view, ids[1])
    });
    pump_for(&mut rig.runtime, Duration::from_millis(300));
    let view = rig.runtime.view();
    assert!(is_playing(&view, ids[1]), "B keeps playing: {view:?}");
    let _ = rig.runtime.shutdown();
}

#[test]
fn removing_the_active_entry_does_not_cancel_a_newer_load_in_flight() {
    let server = delayed_fixture("/b.flac", Duration::from_millis(600));
    let (_media, root) = media_dir(&["a.flac"]);
    let mut rig = rig_with(seeded(vec![local_entry(&root.join("a.flac"))]));
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::Url(server.url("/b.flac"))],
    );
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "A playing", |view| {
        is_playing(view, ids[0])
    });

    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    pump_for(&mut rig.runtime, Duration::from_millis(200));
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Loading);
    rig.runtime.handle(AppCommand::Remove(ids[0]));
    pump_until(&mut rig.runtime, "B playing", |view| {
        is_playing(view, ids[1])
    });
    assert_eq!(rig.runtime.view().active, Some(ids[1]));
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

#[test]
fn clearing_during_a_load_never_adopts_the_invalidated_load() {
    let server = delayed_fixture("/b.flac", Duration::from_millis(600));
    let (_media, root) = media_dir(&["a.flac"]);
    let mut rig = rig_with(seeded(vec![local_entry(&root.join("a.flac"))]));
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::Url(server.url("/b.flac"))],
    );
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "A playing", |view| {
        is_playing(view, ids[0])
    });

    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    pump_for(&mut rig.runtime, Duration::from_millis(200));
    let viewed = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::ClearPlaylist(viewed));
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.runtime.session().pending_load_count() > 0 {
        rig.runtime.pump();
        assert!(Instant::now() < deadline, "B's load never resolved");
        std::thread::sleep(Duration::from_millis(10));
    }
    pump_for(&mut rig.runtime, Duration::from_millis(300));
    let view = rig.runtime.view();
    assert_eq!(view.phase, PlaybackPhase::Unloaded, "{view:?}");
    assert!(view.rows.is_empty());
    assert!(rig.runtime.session().adopted().is_none());
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

/// Plays A, requests the missing B (or two occurrences of it), waits for the
/// failure, creates B, then presses `retry` with the selection still on A.
fn retry_a_failed_switch(retry: impl Fn(QueueEntryId) -> AppCommand, duplicates: bool) {
    let (_media, root) = media_dir(&["a.flac"]);
    let (a_path, b_path) = (root.join("a.flac"), root.join("b.flac"));
    let mut entries = vec![local_entry(&a_path), local_entry(&b_path)];
    if duplicates {
        entries.push(local_entry(&b_path));
    }
    let mut rig = rig_with(seeded(entries));
    let ids = row_ids(&rig.runtime);
    let a = ids[0];
    let b = *ids.last().unwrap_or_else(|| panic!("B"));

    rig.runtime.handle(AppCommand::PlayEntry(a));
    pump_until(&mut rig.runtime, "A playing", |view| is_playing(view, a));
    let a_token = now_playing(&rig.runtime.view())
        .load
        .unwrap_or_else(|| panic!("A's token"));

    if duplicates {
        rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    }
    rig.runtime.handle(AppCommand::PlayEntry(b));
    pump_until(&mut rig.runtime, "B failed", |view| {
        view.phase == PlaybackPhase::LoadFailed
    });
    assert_eq!(rig.runtime.session().pending_load_count(), 0);
    assert_eq!(rig.runtime.view().active, Some(a), "active stays A");
    assert_eq!(rig.runtime.view().last_requested, Some(b));

    std::fs::copy(FIVE, &b_path).unwrap_or_else(|error| panic!("create B: {error}"));
    rig.runtime.handle(retry(a));
    // The retry is a load of B itself, not of the selected or active A (A
    // ending and advancing into B would otherwise reach B too).
    assert_eq!(rig.runtime.view().last_requested, Some(b));
    assert_eq!(rig.runtime.session().pending_load_count(), 1);
    pump_until(&mut rig.runtime, "B adopted", |view| {
        view.now_playing
            .as_ref()
            .is_some_and(|now| now.loaded && now.entry == Some(b))
    });
    let failed_attempts = if duplicates { 2 } else { 1 };
    let b_token = now_playing(&rig.runtime.view())
        .load
        .unwrap_or_else(|| panic!("B's token"));
    assert!(
        b_token.get() > a_token.get() + failed_attempts,
        "a fresh token, not a reuse of a failed one: {a_token:?} -> {b_token:?}"
    );
    let _ = rig.runtime.shutdown();
}

#[test]
fn space_retries_a_failed_switch_with_a_fresh_token() {
    retry_a_failed_switch(|_| AppCommand::PlayPause, false);
    retry_a_failed_switch(|_| AppCommand::Play, false);
    retry_a_failed_switch(|_| AppCommand::PlayPause, true);
}

const FEED_URL: &str = "https://feeds.example/radio-t.xml";

fn podcast_rss(enclosure: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Radio-T</title><item><title>e1</title><guid>e1</guid><enclosure url="{enclosure}" type="audio/flac"/></item></channel></rss>"#
    )
    .into_bytes()
}

/// A subscribed feed whose cache file is corrupt, the queued episode it
/// lists, and the library stores reading it: resolving that episode fails
/// before any load reaches the engine.
fn corrupt_podcast(enclosure: &str) -> (feeds::Rig, NewQueueEntry, LibraryStores) {
    let library = feeds::Rig::new().unwrap_or_else(|error| panic!("feeds rig: {error}"));
    let subscription = library
        .seed(&podcast_rss(enclosure), FEED_URL)
        .unwrap_or_else(|error| panic!("seed: {error}"));
    let cache_path = library
        .cache
        .path_for(&subscription.feed_id)
        .unwrap_or_else(|error| panic!("cache path: {error}"));
    std::fs::write(&cache_path, b"{").unwrap_or_else(|error| panic!("corrupt the cache: {error}"));

    let episode = MediaId::PodcastEpisode {
        feed: FeedId::new(feeds::FEED_ID.into()).unwrap_or_else(|error| panic!("feed id: {error}")),
        episode: EpisodeKey::resolve(Some("e1"), None, None)
            .unwrap_or_else(|error| panic!("episode key: {error}")),
    };
    let podcast = NewQueueEntry::new(
        episode,
        QueueSource::Podcast {
            fallback: enclosure
                .parse()
                .unwrap_or_else(|error| panic!("url: {error}")),
        },
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("podcast entry: {error}"));
    let clock: Arc<dyn tenuto::clock::Clock> = Arc::new(SystemClock);
    let stores = LibraryStores {
        subscriptions: SubscriptionStore::new(
            library.root.path().join("data/tenuto/subscriptions.json"),
            Arc::clone(&clock),
        ),
        cache: CacheStore::new(library.root.path().join("cache/tenuto/feeds")),
        stations: StationStore::new(library.root.path().join("data/tenuto/stations.json"), clock),
    };
    (library, podcast, stores)
}

#[test]
fn a_failure_before_admission_leaves_the_transport_with_the_playing_track() {
    let (_library, podcast, stores) = corrupt_podcast("https://media.example/ep.flac");
    let (_media, root) = media_dir(&["a.flac"]);
    let mut rig = rig_with_parts(
        seeded(vec![local_entry(&root.join("a.flac")), podcast]),
        Some(stores),
        null_engine(),
    );
    let ids = row_ids(&rig.runtime);
    let (a, b) = (ids[0], ids[1]);
    rig.runtime.handle(AppCommand::PlayEntry(a));
    pump_until(&mut rig.runtime, "A playing with a duration", |view| {
        is_playing(view, a)
            && view
                .now_playing
                .as_ref()
                .is_some_and(|now| now.duration.is_some())
    });
    let a_token = now_playing(&rig.runtime.view()).load;

    // A forward burst is standing when B's resolution fails.
    rig.runtime.handle(AppCommand::SeekBy(1));
    let target = now_playing(&rig.runtime.view()).position;
    rig.runtime.handle(AppCommand::PlayEntry(b));
    let view = rig.runtime.view();
    let failure = view
        .status
        .clone()
        .expect("the resolution failure is shown");
    assert_eq!(view.last_requested, Some(b));
    assert_eq!(rig.runtime.session().pending_load_count(), 0);
    assert!(is_playing(&view, a), "A keeps the transport: {view:?}");

    // The burst aimed before the switch is gone: the very next pump shows
    // the worker's own position rather than holding the burst's target, and
    // progress keeps reaching the display.
    rig.runtime.pump();
    let now = now_playing(&rig.runtime.view());
    assert!(
        !now.estimated_position && now.position < target,
        "the burst was cancelled: {now:?}"
    );
    pump_for(&mut rig.runtime, Duration::from_millis(400));
    let before = now_playing(&rig.runtime.view()).position;
    pump_until(&mut rig.runtime, "A's position advancing", |view| {
        view.now_playing
            .as_ref()
            .is_some_and(|now| now.position >= before + Duration::from_millis(200))
    });

    // Space pauses A rather than retrying B, and `p` resumes it.
    rig.runtime.handle(AppCommand::PlayPause);
    assert_eq!(rig.runtime.session().pending_load_count(), 0, "no retry");
    pump_until(&mut rig.runtime, "A paused", |view| {
        view.phase == PlaybackPhase::Paused
    });
    rig.runtime.handle(AppCommand::Play);
    assert_eq!(rig.runtime.session().pending_load_count(), 0, "no retry");
    pump_until(&mut rig.runtime, "A playing again", |view| {
        is_playing(view, a)
    });

    // A seek is accepted, lands, and progress continues from it.
    let from = now_playing(&rig.runtime.view()).position;
    rig.runtime.handle(AppCommand::SeekBy(2));
    pump_until(&mut rig.runtime, "the seek landed", |view| {
        view.now_playing.as_ref().is_some_and(|now| {
            !now.estimated_position && now.position >= from + Duration::from_millis(1500)
        })
    });
    let landed = now_playing(&rig.runtime.view()).position;
    pump_until(
        &mut rig.runtime,
        "position advancing after the seek",
        |view| {
            view.now_playing
                .as_ref()
                .is_some_and(|now| now.position >= landed + Duration::from_millis(200))
        },
    );

    let view = rig.runtime.view();
    assert!(is_playing(&view, a), "{view:?}");
    assert_eq!(now_playing(&view).load, a_token, "A was never reloaded");
    assert_eq!(view.last_requested, Some(b));
    assert_eq!(
        view.status.as_deref(),
        Some(failure.as_str()),
        "the failure stays visible and no seek notice replaced it"
    );
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_resolution_failure_is_retryable_without_losing_the_previous_adoption() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let enclosure = server.url("/ep.flac");
    let (library, podcast, stores) = corrupt_podcast(&enclosure);
    let (_media, root) = media_dir(&["a.flac"]);
    let mut rig = rig_with_parts(
        seeded(vec![
            local_entry(&root.join("a.flac")),
            podcast,
            local_entry(&root.join("missing.flac")),
        ]),
        Some(stores),
        null_engine(),
    );
    let ids = row_ids(&rig.runtime);
    let (a, b, missing) = (ids[0], ids[1], ids[2]);

    rig.runtime.handle(AppCommand::PlayEntry(a));
    pump_until(&mut rig.runtime, "A playing", |view| is_playing(view, a));

    // Two older loads are still pending when B's resolution fails: one of A
    // that will succeed, and one of a missing file that will fail.
    rig.runtime.handle(AppCommand::PlayEntry(a));
    rig.runtime.handle(AppCommand::PlayEntry(missing));
    rig.runtime.handle(AppCommand::PlayEntry(b));
    let view = rig.runtime.view();
    assert_eq!(view.last_requested, Some(b));
    let resolution_error = view.status.expect("the resolution error is shown");
    assert!(!resolution_error.is_empty());
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.runtime.session().pending_load_count() > 0 {
        rig.runtime.pump();
        assert!(Instant::now() < deadline, "older loads never resolved");
        std::thread::sleep(Duration::from_millis(10));
    }
    pump_for(&mut rig.runtime, Duration::from_millis(100));
    let view = rig.runtime.view();
    assert_eq!(
        view.phase,
        PlaybackPhase::LoadFailed,
        "an older outcome cannot clear the later failure: {view:?}"
    );
    assert_eq!(
        view.status.as_deref(),
        Some(resolution_error.as_str()),
        "an older failure does not replace the later one's report"
    );
    assert_eq!(view.active, Some(a), "the previous adoption is kept");

    library
        .seed(&podcast_rss(&enclosure), FEED_URL)
        .expect("repair the cache");
    rig.runtime.handle(AppCommand::Play);
    pump_until(&mut rig.runtime, "B loaded", |view| {
        view.active == Some(b) && view.now_playing.as_ref().is_some_and(|now| now.loaded)
    });
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

/// A and B submitted before a single event is drained: each `Loaded` adopts
/// its own occurrence, in submission order, and B ends up playing.
fn both_loads_adopt_their_own_occurrence_in_order() {
    let (_media, root) = media_dir(&["a.flac", "b.flac"]);
    let mut rig = rig_with(seeded(vec![
        local_entry(&root.join("a.flac")),
        local_entry(&root.join("b.flac")),
    ]));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    assert_eq!(rig.runtime.session().pending_load_count(), 2);
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut settled = 0;
    // Keeps recording for a while after B plays, so a late regression to A
    // would show up too.
    while settled < 20 {
        rig.runtime.pump();
        let view = rig.runtime.view();
        if let Some(active) = view.active
            && seen.last() != Some(&active)
        {
            seen.push(active);
        }
        if is_playing(&view, ids[1]) {
            settled += 1;
        }
        assert!(Instant::now() < deadline, "B never played: {view:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        seen == vec![ids[1]] || seen == vec![ids[0], ids[1]],
        "adopted in submission order and B stays adopted: {seen:?}"
    );
    assert_eq!(rig.runtime.session().pending_load_count(), 0);
    let b_token = now_playing(&rig.runtime.view())
        .load
        .unwrap_or_else(|| panic!("B's token"));
    assert_eq!(b_token.get(), 2, "B's own token, the second one issued");
    let _ = rig.runtime.shutdown();
}

fn a_failed_second_load_is_not_retried_implicitly() {
    // A failed remote load keeps its source for one explicit reopen, so an
    // unrestricted start queued behind the load would retry it; every such
    // retry would reach the server again.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").status(404));
    let (_media, root) = media_dir(&["a.flac"]);
    let mut rig = rig_with(seeded(vec![local_entry(&root.join("a.flac"))]));
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::Url(server.url("/b.flac"))],
    );
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    pump_until(&mut rig.runtime, "B failed", |view| {
        view.phase == PlaybackPhase::LoadFailed
    });
    let adopted = rig
        .runtime
        .session()
        .adopted()
        .unwrap_or_else(|| panic!("A adopted"))
        .request;
    assert_eq!(rig.runtime.view().active, Some(ids[0]));
    // The start command is dispatched right behind the failed load, so an
    // implicit reopen would already have reached the server by now.
    assert_eq!(server.requests().len(), 1, "B was fetched once");
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        rig.runtime.pump();
        let view = rig.runtime.view();
        assert_eq!(
            rig.runtime.session().pending_load_count(),
            0,
            "no retry registered"
        );
        assert_eq!(view.phase, PlaybackPhase::LoadFailed, "{view:?}");
        assert!(!now_playing(&view).loaded, "nothing reopened: {view:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.requests().len(), 1, "B was never fetched again");
    assert_eq!(
        rig.runtime.session().adopted().map(|load| load.request),
        Some(adopted)
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

#[test]
fn automatic_start_handles_intervening_outcomes() {
    // A start command delivered late for A (tests/m5_engine_load_outcomes.rs)
    // and a refused start command (the runtime's own unit tests) cannot be
    // staged through a real engine here.
    both_loads_adopt_their_own_occurrence_in_order();
    a_failed_second_load_is_not_retried_implicitly();
}

#[test]
fn the_spectrum_exists_once_the_engine_does_and_labels_the_adopted_revision() {
    let mut rig = rig_with(PersistedState::default());
    assert!(rig.runtime.spectrum().is_none(), "no engine yet");
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(FIVE.into())]);
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "playing", |view| is_playing(view, ids[0]));
    let spectrum = rig.runtime.spectrum().expect("the load created the engine");
    spectrum.set_enabled(true);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        rig.runtime.pump();
        let view = rig.runtime.view();
        if let (Some(frame), Some(now)) = (spectrum.latest(), view.now_playing.as_ref())
            && frame.session_rev == now.session_rev
        {
            assert!(!frame.levels.is_empty());
            break;
        }
        assert!(Instant::now() < deadline, "no spectrum frame while playing");
        std::thread::sleep(Duration::from_millis(10));
    }
    spectrum.set_enabled(false);
    assert!(spectrum.latest().is_none());
    assert!(matches!(rig.runtime.shutdown(), FlushReport::Written));
}
