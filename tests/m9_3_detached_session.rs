//! M9.3: a detached load (`tenuto play`) adopts its media without touching
//! the saved playlists.

use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::playback::command::LoadRequestId;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueEntryId, QueueSource};
use tenuto::session::{LoadTarget, Session};

fn path(name: &str) -> AbsolutePath {
    AbsolutePath::new(format!("/music/{name}.flac").into())
        .unwrap_or_else(|error| panic!("absolute: {error}"))
}

fn media(name: &str) -> MediaId {
    MediaId::LocalFile(path(name))
}

fn entry(name: &str) -> NewQueueEntry {
    NewQueueEntry::new(
        media(name),
        QueueSource::LocalFile(path(name)),
        DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("valid: {error}"))
}

fn loaded(session_rev: u64, request: LoadRequestId, media: MediaId) -> PlaybackEvent {
    PlaybackEvent::Loaded {
        session_rev,
        request,
        media,
        metadata: Default::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position: Duration::ZERO,
        disposition: StartDisposition::Fresh,
    }
}

/// A playing playlist of a, b, c with b adopted through a queue load.
fn with_b_active(clock: &FakeClock) -> (Session, Vec<QueueEntryId>) {
    let mut session = Session::new(PersistedState::default());
    let playing = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(playing, vec![entry("a"), entry("b"), entry("c")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    let request = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    session.observe(&loaded(1, request, media("b")), clock.sample());
    assert_eq!(
        session
            .state()
            .playlists()
            .playing_playlist()
            .queue()
            .active(),
        Some(ids[1])
    );
    (session, ids)
}

fn detached(session: &mut Session, clock: &FakeClock, name: &str) -> LoadRequestId {
    let request = session
        .register_load(LoadTarget::Detached, &media(name))
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    session.observe(&loaded(2, request, media(name)), clock.sample());
    request
}

#[test]
fn a_detached_load_keeps_the_playing_playlist_and_its_cursor() {
    let clock = FakeClock::new();
    let (mut session, ids) = with_b_active(&clock);
    let playing = session.state().playlists().playing();
    detached(&mut session, &clock, "elsewhere");

    assert_eq!(
        session.adopted().map(|load| load.target),
        Some(LoadTarget::Detached)
    );
    assert_eq!(session.state().playlists().playing(), playing);
    assert_eq!(
        session
            .state()
            .playlists()
            .playing_playlist()
            .queue()
            .active(),
        Some(ids[1])
    );
}

#[test]
fn a_detached_load_of_a_queued_media_keeps_the_cursor_and_updates_its_checkpoint() {
    let clock = FakeClock::new();
    let (mut session, ids) = with_b_active(&clock);
    let request = detached(&mut session, &clock, "c");
    session.observe(
        &PlaybackEvent::StateChanged {
            session_rev: 2,
            state: PlaybackState::Playing,
            request: None,
        },
        clock.sample(),
    );
    clock.advance(Duration::from_secs(6));
    let progress = Progress {
        session_rev: 2,
        media: Some(media("c")),
        position: Duration::from_secs(6),
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: Some(request),
    };
    let _ = session.tick(&progress, clock.sample());
    let state = session.shutdown_snapshot(&progress, clock.sample());

    assert_eq!(
        state.playlists().playing_playlist().queue().active(),
        Some(ids[1])
    );
    let saved = state
        .entry_for(&media("c"))
        .unwrap_or_else(|| panic!("c has a checkpoint"));
    assert_eq!(saved.position, Some(Duration::from_secs(6)), "{saved:?}");
}

#[test]
fn a_detached_track_that_ends_sets_no_advance() {
    let clock = FakeClock::new();
    let (mut session, _) = with_b_active(&clock);
    detached(&mut session, &clock, "elsewhere");
    session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 2,
            position: Duration::from_secs(5),
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    );
    assert!(session.take_advance().is_none());
}

/// The saved state ties the playing playlist's cursor to `current_media`:
/// loading clears a cursor whose media is not the current one
/// (`CursorMediaMismatch`). A detached load therefore leaves the saved
/// current media to the cursor, and the state reloads with its place kept.
#[test]
fn a_detached_load_leaves_the_saved_current_media_to_the_cursor() {
    use std::sync::Arc;
    use tenuto::persistence::store::StateStore;

    let clock = FakeClock::new();
    let (mut session, ids) = with_b_active(&clock);
    detached(&mut session, &clock, "elsewhere");
    assert_eq!(session.state().current_media(), Some(&media("b")));

    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let store = StateStore::new(dir.path().join("state.json"), Arc::new(FakeClock::new()));
    store
        .write(session.state())
        .unwrap_or_else(|error| panic!("write: {error}"));
    let reloaded = store.load();
    assert!(
        reloaded.queue_repair.is_none(),
        "{:?}",
        reloaded.queue_repair
    );
    assert_eq!(
        reloaded
            .state
            .playlists()
            .playing_playlist()
            .queue()
            .active(),
        Some(ids[1])
    );
}
