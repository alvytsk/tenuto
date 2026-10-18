//! M11: desktop media controls drive the runtime through `AppCommand`.

mod support;

#[path = "support/feeds.rs"]
mod feeds;
#[path = "support/runtime.rs"]
mod runtime;
#[path = "support/tagged_flac.rs"]
mod tagged_flac;

use std::time::Duration;

use runtime::{enqueue, pump_for, pump_until, rig_with, rig_with_library, row_ids, step};
use std::sync::Arc;
use support::server::{Script, TestServer};
use tenuto::application::mpris::{Request, Status, channel};
use tenuto::application::runtime::{AppCommand, EnqueueItem, LibraryStores};
use tenuto::application::transport::PlaybackPhase;
use tenuto::clock::{Clock, SystemClock};
use tenuto::feed::cache::CacheStore;
use tenuto::media::id::{EpisodeKey, FeedId, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::playback::state::PlaybackState;
use tenuto::queue::{NewQueueEntry, QueueSource};
use tenuto::session::Session;
use tenuto::station::store::StationStore;
use tenuto::subscription::store::SubscriptionStore;

const FIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");

#[test]
fn a_pause_after_the_engine_paused_does_not_resume() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(FIVE.into())]);
    let row = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(row));
    pump_until(&mut rig, "playing", |v| v.phase == PlaybackPhase::Playing);
    rig.runtime.handle(AppCommand::PlayPause);
    // The view still says Playing: a toggle here would resume.
    rig.runtime.handle(AppCommand::Pause);
    pump_until(&mut rig, "paused", |v| v.phase == PlaybackPhase::Paused);
    pump_for(&mut rig, Duration::from_secs(1));
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Paused);
    let _ = rig.runtime.shutdown();
}

#[test]
fn pause_does_nothing_when_nothing_is_loaded() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Pause);
    step(&mut rig);
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Unloaded);
    let _ = rig.runtime.shutdown();
}

#[test]
fn pause_on_a_paused_station_leaves_it_paused() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let mut rig = rig_with(PersistedState::default());
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::from_input(&server.url("/radio"))],
    );
    let station = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(station));
    let paused = |v: &tenuto::application::view::PlayerView| {
        v.now_playing
            .as_ref()
            .is_some_and(|now| now.state == PlaybackState::Paused)
    };
    pump_until(&mut rig, "playing", |v| v.phase == PlaybackPhase::Playing);
    rig.runtime.handle(AppCommand::Pause);
    pump_until(&mut rig, "paused", paused);
    let phase = rig.runtime.view().phase;
    let settled = server.requests().len();

    rig.runtime.handle(AppCommand::Pause);
    pump_for(&mut rig, Duration::from_secs(2));
    assert_eq!(rig.runtime.view().phase, phase);
    assert!(!rig.runtime.view().reconnecting);
    assert_eq!(server.requests().len(), settled);
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

fn play_five(rig: &mut runtime::Rig) {
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(FIVE.into())]);
    let row = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(row));
    pump_until(rig, "playing", |v| v.phase == PlaybackPhase::Playing);
}

#[test]
fn a_play_pause_request_reaches_the_runtime() {
    let mut rig = rig_with(PersistedState::default());
    play_five(&mut rig);
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    remote.requests.try_send(Request::PlayPause).unwrap();
    pump_until(&mut rig, "paused", |v| v.phase == PlaybackPhase::Paused);
    let snap = remote.snapshots.borrow().clone().unwrap();
    assert_eq!(snap.status, Status::Paused);
    let _ = rig.runtime.shutdown();
}

#[test]
fn the_snapshot_describes_the_playing_track() {
    let dir = tempfile::tempdir().unwrap();
    let path = tagged_flac::tagged_flac(dir.path(), "Song", "Band", "Album", None);
    let mut rig = rig_with(PersistedState::default());
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::Path(path.to_str().unwrap().into())],
    );
    let row = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(row));
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    pump_until(&mut rig, "titled", |v| {
        v.now_playing
            .as_ref()
            .is_some_and(|n| n.title == "Song" && n.duration.is_some())
    });
    let track = remote.snapshots.borrow().clone().unwrap().track.unwrap();
    assert_eq!(track.title, "Song");
    assert_eq!(track.artist.as_deref(), Some("Band"));
    assert!(track.length.is_some());
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_click_with_nothing_loaded_does_nothing() {
    let mut rig = rig_with(PersistedState::default());
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    remote.requests.try_send(Request::Play).unwrap();
    pump_for(&mut rig, Duration::from_millis(500));
    let snap = remote.snapshots.borrow().clone().unwrap();
    assert!(snap.track.is_none());
    assert_eq!(snap.status, Status::Stopped);
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_podcast_episode_shows_the_podcast_name_as_artist() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let enclosure = server.url("/ep.flac");
    let library = feeds::Rig::new().unwrap();
    let rss = format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Radio-T</title><item><title>e1</title><guid>e1</guid><enclosure url="{enclosure}" type="audio/flac"/></item></channel></rss>"#
    );
    library
        .seed(rss.as_bytes(), "https://feeds.example/radio-t.xml")
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let stores = LibraryStores {
        subscriptions: SubscriptionStore::new(
            library.root.path().join("data/tenuto/subscriptions.json"),
            Arc::clone(&clock),
        ),
        cache: CacheStore::new(library.root.path().join("cache/tenuto/feeds")),
        stations: StationStore::new(library.root.path().join("data/tenuto/stations.json"), clock),
    };
    let episode = MediaId::PodcastEpisode {
        feed: FeedId::new(feeds::FEED_ID.into()).unwrap(),
        episode: EpisodeKey::resolve(Some("e1"), None, None).unwrap(),
    };
    let entry = NewQueueEntry::new(
        episode,
        QueueSource::Podcast {
            fallback: enclosure.parse().unwrap(),
        },
        Default::default(),
    )
    .unwrap();
    let mut session = Session::new(PersistedState::default());
    session
        .enqueue(session.state().playlists().playing(), vec![entry])
        .unwrap();
    let mut rig = rig_with_library(session.state().clone(), stores);
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    let row = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(row));
    pump_until(&mut rig, "playing", |v| v.phase == PlaybackPhase::Playing);
    let track = remote.snapshots.borrow().clone().unwrap().track.unwrap();
    assert_eq!(track.artist.as_deref(), Some("Radio-T"));
    let _ = rig.runtime.shutdown();
}

#[test]
fn position_advances_in_successive_snapshots() {
    let mut rig = rig_with(PersistedState::default());
    play_five(&mut rig);
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    step(&mut rig);
    let before = remote.snapshots.borrow().clone().unwrap().position;
    pump_for(&mut rig, Duration::from_secs(1));
    let after = remote.snapshots.borrow().clone().unwrap().position;
    assert!(after > before, "{before:?} then {after:?}");
    let _ = rig.runtime.shutdown();
}

const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");

fn png_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30])))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap_or_else(|e| panic!("encode: {e}"));
    bytes
}

fn png_of(w: u32, h: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(w, h))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap_or_else(|e| panic!("encode: {e}"));
    bytes
}

fn has_cover(remote: &tenuto::application::mpris::Remote) -> bool {
    remote
        .snapshots
        .borrow()
        .as_ref()
        .is_some_and(|s| s.cover.is_some())
}

fn playing(v: &tenuto::application::view::PlayerView) -> bool {
    v.phase == PlaybackPhase::Playing
}

fn play_path(rig: &mut runtime::Rig, path: &std::path::Path) {
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::Path(path.to_path_buf())],
    );
    let row = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(row));
}

#[test]
fn a_playlist_track_with_an_embedded_picture_publishes_a_cover() {
    let dir = tempfile::tempdir().unwrap();
    let path = tagged_flac::tagged_flac(dir.path(), "Song", "Band", "Album", Some(&png_bytes()));
    let mut rig = rig_with(PersistedState::default());
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    play_path(&mut rig, &path);
    pump_until(&mut rig, "cover", |_| has_cover(&remote));
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_detached_track_with_an_embedded_picture_publishes_a_cover() {
    let dir = tempfile::tempdir().unwrap();
    let path = tagged_flac::tagged_flac(dir.path(), "Song", "Band", "Album", Some(&png_bytes()));
    let mut rig = rig_with(PersistedState::default());
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    let abs = tenuto::media::id::AbsolutePath::new(path.clone()).unwrap();
    rig.runtime.play_detached(
        MediaId::LocalFile(abs),
        tenuto::media::source::SourceLocation::LocalPath(path),
    );
    pump_until(&mut rig, "cover", |_| has_cover(&remote));
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_large_embedded_cover_reaches_the_snapshot_shrunk() {
    let dir = tempfile::tempdir().unwrap();
    let big = png_of(3000, 2000);
    let path = tagged_flac::tagged_flac(dir.path(), "Song", "Band", "Album", Some(&big));
    let mut rig = rig_with(PersistedState::default());
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    play_path(&mut rig, &path);
    pump_until(&mut rig, "cover", |_| has_cover(&remote));
    let cover = remote.snapshots.borrow().clone().unwrap().cover.unwrap();
    assert!(cover.0.width().max(cover.0.height()) <= 512, "{cover:?}");
    let _ = rig.runtime.shutdown();
}

#[test]
fn an_untagged_track_publishes_no_cover() {
    let mut rig = rig_with(PersistedState::default());
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    play_path(&mut rig, std::path::Path::new(SHORT));
    pump_until(&mut rig, "playing", playing);
    pump_for(&mut rig, Duration::from_secs(1));
    assert!(!has_cover(&remote));
    let _ = rig.runtime.shutdown();
}

#[test]
fn the_previous_cover_is_dropped_on_track_change() {
    let dir = tempfile::tempdir().unwrap();
    let a = tagged_flac::tagged_flac(dir.path(), "A", "Band", "Album", Some(&png_bytes()));
    let mut rig = rig_with(PersistedState::default());
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    enqueue(
        &mut rig.runtime,
        vec![
            EnqueueItem::Path(a.clone()),
            EnqueueItem::Path(SHORT.into()),
        ],
    );
    let rows = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(rows[0]));
    pump_until(&mut rig, "A's cover", |_| has_cover(&remote));
    rig.runtime.handle(AppCommand::PlayEntry(rows[1]));
    let b = MediaId::LocalFile(tenuto::media::id::AbsolutePath::new(SHORT.into()).unwrap());
    pump_until(&mut rig, "B's snapshot", |_| {
        remote
            .snapshots
            .borrow()
            .as_ref()
            .and_then(|s| s.track.as_ref())
            .is_some_and(|t| t.media == b)
    });
    pump_for(&mut rig, Duration::from_secs(1));
    let snap = remote.snapshots.borrow().clone().unwrap();
    assert_eq!(snap.track.map(|t| t.media), Some(b));
    assert!(snap.cover.is_none());
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_cover_that_fails_to_decode_publishes_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = tagged_flac::tagged_flac(
        dir.path(),
        "Song",
        "Band",
        "Album",
        Some(b"\x89PNG\r\n\x1a\ngarbage"),
    );
    let mut rig = rig_with(PersistedState::default());
    let (controls, remote) = channel();
    rig.runtime.attach_media_controls(controls);
    play_path(&mut rig, &path);
    pump_until(&mut rig, "playing", playing);
    pump_for(&mut rig, Duration::from_secs(1));
    assert!(!has_cover(&remote));
    let _ = rig.runtime.shutdown();
}
