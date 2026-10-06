//! M9.3 end to end: `tenuto play` leaves the saved playlists as it found them.
#![cfg(target_os = "linux")]

#[path = "support/process.rs"]
mod process;

use std::sync::Arc;
use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueSource};
use tenuto::session::{LoadTarget, Session};

const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const OTHER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");

#[test]
fn play_keeps_the_playing_playlist_and_its_cursor() {
    let profile = process::Profile::new().unwrap_or_else(|error| panic!("profile: {error}"));
    std::fs::create_dir_all(profile.state_dir()).unwrap_or_else(|error| panic!("{error}"));
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());

    // A saved playlist of one row, adopted, the way the TUI leaves it.
    let path = AbsolutePath::new(OTHER.into()).unwrap_or_else(|error| panic!("{error}"));
    let media = MediaId::LocalFile(path.clone());
    let mut session = Session::new(PersistedState::default());
    let playing = session.state().playlists().playing();
    let entry = NewQueueEntry::new(
        media.clone(),
        QueueSource::LocalFile(path),
        DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let (ids, _) = session
        .enqueue(playing, vec![entry])
        .unwrap_or_else(|error| panic!("{error}"));
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), &media)
        .unwrap_or_else(|error| panic!("{error:?}"));
    session.observe(
        &PlaybackEvent::Loaded {
            session_rev: 1,
            request,
            media,
            metadata: Default::default(),
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::ZERO,
            disposition: StartDisposition::Fresh,
        },
        clock.sample(),
    );
    StateStore::new(profile.state_file(), Arc::clone(&clock))
        .write(session.state())
        .unwrap_or_else(|error| panic!("seeded: {error}"));

    let output = profile
        .command()
        .env("TENUTO_AUDIO_OUTPUT", "null")
        .args(["play", SHORT])
        .output()
        .unwrap_or_else(|error| panic!("ran: {error}"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reloaded = StateStore::new(profile.state_file(), clock).load();
    assert!(
        reloaded.queue_repair.is_none(),
        "the state play left needed no repair: {:?}",
        reloaded.queue_repair
    );
    let reloaded = reloaded.state;
    assert_eq!(reloaded.playlists().playing(), playing);
    assert_eq!(
        reloaded.playlists().playing_playlist().queue().active(),
        Some(ids[0])
    );
    assert_eq!(reloaded.playlists().playing_playlist().queue().len(), 1);
}
