//! M7 §9: a station never checkpoints, and the gate changes hands in order.

mod support;

use std::time::Duration;

use support::media;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueSource};
use tenuto::session::{CAPTURE_INTERVAL, LoadTarget, Session};

const FINITE: MediaCapabilities = MediaCapabilities {
    continuity: Continuity::Finite,
    seek: SeekSupport::Native,
};
const LIVE: MediaCapabilities = MediaCapabilities {
    continuity: Continuity::Indefinite,
    seek: SeekSupport::Unsupported,
};

/// Queues `id` in the playing playlist and loads it there, the way the TUI
/// plays a station: a playlist load, whose media becomes the saved current one.
fn load(session: &mut Session, rev: u64, id: &MediaId, caps: MediaCapabilities) -> Progress {
    let MediaId::LocalFile(path) = id else {
        panic!("these tests load local files: {id:?}");
    };
    let entry = NewQueueEntry::new(
        id.clone(),
        QueueSource::LocalFile(path.clone()),
        DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("valid: {error}"));
    let playing = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(playing, vec![entry])
        .unwrap_or_else(|error| panic!("room: {error}"));
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), id)
        .unwrap_or_else(|error| panic!("room: {error:?}"));
    let clock = FakeClock::new();
    session.observe(
        &PlaybackEvent::Loaded {
            session_rev: rev,
            request,
            media: id.clone(),
            metadata: Default::default(),
            capabilities: caps,
            position: Duration::ZERO,
            disposition: StartDisposition::Fresh,
        },
        clock.sample(),
    );
    session.observe(
        &PlaybackEvent::StateChanged {
            session_rev: rev,
            state: PlaybackState::Playing,
            request: None,
        },
        clock.sample(),
    );
    Progress {
        session_rev: rev,
        media: Some(id.clone()),
        position: Duration::ZERO,
        quality: PositionQuality::Estimated,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: Some(request),
    }
}

fn listen(session: &mut Session, clock: &FakeClock, progress: &mut Progress, to: Duration) {
    progress.position = to;
    clock.advance(CAPTURE_INTERVAL + Duration::from_secs(1));
    session.tick(progress, clock.sample());
}

fn checkpoint(session: &Session, id: &MediaId) -> Option<Duration> {
    session
        .state()
        .entry_for(id)
        .and_then(|entry| entry.position)
}

#[test]
fn a_station_is_never_checkpointed_by_tick_pause_stop_or_shutdown() {
    let station = media("radio");
    let mut session = Session::new(PersistedState::default());
    let clock = FakeClock::new();
    let mut progress = load(&mut session, 1, &station, LIVE);
    listen(&mut session, &clock, &mut progress, Duration::from_secs(40));
    for state in [PlaybackState::Paused, PlaybackState::Stopped] {
        session.observe(
            &PlaybackEvent::StateChanged {
                session_rev: 1,
                state,
                request: None,
            },
            clock.sample(),
        );
        session.tick(&progress, clock.sample());
    }
    let snapshot = session.shutdown_snapshot(&progress, clock.sample());
    assert_eq!(checkpoint(&session, &station), None);
    assert!(snapshot.entry_for(&station).is_none());
    assert_eq!(
        snapshot.current_media(),
        Some(&station),
        "the station is still current"
    );
}

#[test]
fn loading_a_station_still_checkpoints_the_finite_track_on_its_way_out() {
    let (track, station) = (media("a"), media("radio"));
    let mut session = Session::new(PersistedState::default());
    let clock = FakeClock::new();
    let mut progress = load(&mut session, 1, &track, FINITE);
    listen(&mut session, &clock, &mut progress, Duration::from_secs(12));
    // Advance past the last capture so only `record_outgoing` can write this.
    progress.position = Duration::from_secs(14);
    session.tick(&progress, clock.sample());
    load(&mut session, 2, &station, LIVE);
    assert_eq!(checkpoint(&session, &track), Some(Duration::from_secs(14)));
    assert_eq!(checkpoint(&session, &station), None);
}

#[test]
fn loading_a_finite_track_writes_nothing_for_the_station_on_its_way_out() {
    let (station, track) = (media("radio"), media("a"));
    let mut session = Session::new(PersistedState::default());
    let clock = FakeClock::new();
    let mut progress = load(&mut session, 1, &station, LIVE);
    listen(&mut session, &clock, &mut progress, Duration::from_secs(90));
    let mut next = load(&mut session, 2, &track, FINITE);
    assert_eq!(checkpoint(&session, &station), None);
    listen(&mut session, &clock, &mut next, Duration::from_secs(7));
    assert_eq!(
        checkpoint(&session, &track),
        Some(Duration::from_secs(7)),
        "the gate reopened"
    );
}

#[test]
fn a_url_that_was_finite_and_is_now_live_keeps_its_old_checkpoint_untouched() {
    let id = media("was-a-file");
    let mut session = Session::new(PersistedState::default());
    let clock = FakeClock::new();
    let mut progress = load(&mut session, 1, &id, FINITE);
    listen(&mut session, &clock, &mut progress, Duration::from_secs(33));
    let before = session.state().entry_for(&id).cloned();

    let mut live = load(&mut session, 2, &id, LIVE);
    listen(&mut session, &clock, &mut live, Duration::from_secs(500));
    session.shutdown_snapshot(&live, clock.sample());
    assert_eq!(session.state().entry_for(&id).cloned(), before);
}
