//! The checkpoint policy, driven synchronously.

use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::media::provenance::PositionProvenance;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::writer::Urgency;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::resume::resume_candidate;
use tenuto::session::{
    Action, CAPTURE_INTERVAL, LoadTarget, ResumeDecision, Session, decide_resume,
};
use tenuto::volume::Volume;

mod support;
use support::media;

fn loaded(
    session: &mut Session,
    session_rev: u64,
    name: &str,
    position: Duration,
) -> PlaybackEvent {
    let request = session
        .register_load(LoadTarget::Detached, &media(name))
        .unwrap_or_else(|error| panic!("room for a load: {error:?}"));
    PlaybackEvent::Loaded {
        session_rev,
        request,
        media: media(name),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position,
        // The policy under test here never reads `disposition`; every case
        // that cares about resume behaviour lives in `resume_decision.rs` and
        // `engine_contract.rs` instead.
        disposition: StartDisposition::Fresh,
    }
}

fn state_changed(session_rev: u64, state: PlaybackState) -> PlaybackEvent {
    PlaybackEvent::StateChanged {
        session_rev,
        state,
        request: None,
    }
}

/// A `Loaded` for `media`, fixed at the revision every protection test in this
/// file drives (1): the source could not honour the stored checkpoint, so
/// playback falls back to zero and `retained` is what protection must recover.
fn loaded_unavailable(session: &mut Session, media: &MediaId, retained: Duration) -> PlaybackEvent {
    let request = session
        .register_load(LoadTarget::Detached, media)
        .unwrap_or_else(|error| panic!("room for a load: {error:?}"));
    PlaybackEvent::Loaded {
        session_rev: 1,
        request,
        media: media.clone(),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position: Duration::ZERO,
        disposition: StartDisposition::ResumeUnavailable { retained },
    }
}

/// A `Loaded` for `media` with nothing to protect — the counterpart to
/// `loaded_unavailable`, at the same fixed revision.
fn loaded_fresh(session: &mut Session, media: &MediaId) -> PlaybackEvent {
    let request = session
        .register_load(LoadTarget::Detached, media)
        .unwrap_or_else(|error| panic!("room for a load: {error:?}"));
    PlaybackEvent::Loaded {
        session_rev: 1,
        request,
        media: media.clone(),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position: Duration::ZERO,
        disposition: StartDisposition::Fresh,
    }
}

/// Progress for whatever `session` currently has adopted.
fn progress(session: &Session, session_rev: u64, name: &str, secs: u64) -> Progress {
    Progress {
        session_rev,
        media: Some(media(name)),
        position: Duration::from_secs(secs),
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: session.adopted().map(|adopted| adopted.request),
    }
}

fn submitted(action: Action) -> (PersistedState, Urgency) {
    match action {
        Action::Submit { state, urgency } => (state, urgency),
        Action::None => panic!("expected a submission"),
    }
}

fn is_none(action: &Action) -> bool {
    matches!(action, Action::None)
}

/// A file already carrying one entry, for a `Session` that opens on it rather
/// than on `PersistedState::default()`.
fn state_with(media: &MediaId, position: Duration, completed: bool) -> PersistedState {
    let mut state = PersistedState::default();
    state.record(
        &tenuto::resume::PlaybackCheckpoint {
            media: media.clone(),
            position,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        completed,
    );
    state
}

/// The position `session` currently holds for `media`, read directly through
/// `Session::state()` rather than through whichever `Action` a call happened
/// to return — a protected capture can leave that action a `Submit` carrying
/// an unchanged state, so the action alone cannot tell "nothing happened"
/// apart from "something happened and changed nothing."
fn stored_position(session: &Session, media: &MediaId) -> Duration {
    match session.state().entry_for(media) {
        Some(entry) => match entry.position {
            Some(position) => position,
            None => panic!("expected an established position for {media:?}"),
        },
        None => panic!("expected a stored entry for {media:?}"),
    }
}

/// The `estimated` half of a stored entry, read the same way `stored_position`
/// reads `position` — `None` either for no entry at all or for an entry that
/// has never had an estimate written to it (Task 6 does not distinguish the
/// two here; the tests that need to tell them apart check `entry_for`
/// directly).
fn stored_estimate(session: &Session, media: &MediaId) -> Option<Duration> {
    session
        .state()
        .entry_for(media)
        .and_then(|entry| entry.estimated)
}

/// The same read as `stored_position`, against a `PersistedState` already in
/// hand (typically the return of `shutdown_snapshot`) rather than a `Session`.
fn position_in(state: &PersistedState, media: &MediaId) -> Duration {
    match state.entry_for(media) {
        Some(entry) => match entry.position {
            Some(position) => position,
            None => panic!("expected an established position for {media:?}"),
        },
        None => panic!("expected a stored entry for {media:?}"),
    }
}

fn completed_in(session: &Session, media: &MediaId) -> bool {
    session.state().completed_for(media)
}

/// A session already playing `a`, with the clock parked at the moment playback
/// started. Returns the session and the clock that drives it.
fn playing(name: &str) -> (Session, FakeClock) {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let event = loaded(&mut session, 1, name, Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    (session, clock)
}

#[test]
fn five_seconds_of_playback_becomes_an_ordinary_submission() {
    let (mut session, clock) = playing("a");

    clock.advance(Duration::from_millis(4_900));
    assert!(
        is_none(&session.tick(&progress(&session, 1, "a", 4), clock.sample())),
        "4.9 s is not yet due"
    );

    clock.advance(Duration::from_millis(100));
    let (state, urgency) = submitted(session.tick(&progress(&session, 1, "a", 5), clock.sample()));
    assert_eq!(urgency, Urgency::Ordinary);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(5),
        "the position is the tick's own sample"
    );
}

#[test]
fn the_interval_restarts_after_each_capture() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 5), clock.sample()));

    clock.advance(Duration::from_secs(4));
    assert!(is_none(
        &session.tick(&progress(&session, 1, "a", 9), clock.sample())
    ));
    clock.advance(Duration::from_secs(1));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 10), clock.sample()));
}

#[test]
fn a_wall_clock_that_jumps_backwards_does_not_disturb_the_interval() {
    let (mut session, clock) = playing("a");
    clock.advance_monotonic(Duration::from_secs(5));
    // An hour backwards on the wall, mid-interval.
    clock.set_wall(time::OffsetDateTime::UNIX_EPOCH - Duration::from_secs(3600));

    let (state, _) = submitted(session.tick(&progress(&session, 1, "a", 5), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(5),
        "deadlines read the monotonic hand; only updated_at reads the wall"
    );
}

#[test]
fn no_ordinary_capture_happens_while_paused() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    // Whatever the pause itself is worth, it is worth it once: this first tick
    // resolves the pause's forced checkpoint, and the claim here is only that
    // nothing keeps firing behind it.
    let _ = session.tick(&progress(&session, 1, "a", 5), clock.sample());

    clock.advance(Duration::from_secs(30));
    assert!(is_none(
        &session.tick(&progress(&session, 1, "a", 5), clock.sample())
    ));
}

#[test]
fn a_sample_from_a_session_the_policy_is_not_tracking_is_ignored() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(10));
    assert!(
        is_none(&session.tick(&progress(&session, 7, "a", 10), clock.sample())),
        "a stale revision must not move a checkpoint"
    );
}

#[test]
fn a_revision_is_adopted_from_an_event_the_policy_otherwise_ignores() {
    // §7: a DeviceRecovered can be dropped when the backlog is full, so the
    // revision must be adopted from every event, not only the acted-on ones.
    let (mut session, clock) = playing("a");
    let _ = session.observe(
        &PlaybackEvent::Warning {
            session_rev: 9,
            message: "a device warning".into(),
        },
        clock.sample(),
    );
    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(&session, 9, "a", 5), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(5)
    );
}

#[test]
fn a_volume_change_submits_at_ordinary_urgency_and_touches_no_checkpoint() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));
    let _ = session.tick(&progress(&session, 1, "a", 2), clock.sample());

    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::VolumeChanged {
            session_rev: 1,
            volume: Volume::new(0.25),
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Ordinary);
    assert_eq!(state.volume(), Volume::new(0.25));
    assert!(
        state.entry_for(&media("a")).is_none(),
        "volume is not a position"
    );
}

#[test]
fn end_of_track_records_the_events_own_position_and_marks_completion() {
    let (mut session, clock) = playing("a");
    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(240),
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Forced);
    assert!(state.completed_for(&media("a")));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(240),
        "D1 retains the position"
    );
}

#[test]
fn a_media_switch_produces_one_snapshot_carrying_both_halves() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));

    let event = loaded(&mut session, 2, "b", Duration::ZERO);
    let (state, urgency) = submitted(session.observe(&event, clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(93),
        "the outgoing entry comes from last_sample; load() has already overwritten the engine's position"
    );
    assert_eq!(
        state.current_media(),
        Some(&media("b")),
        "and the move of current_media is the same mutation"
    );
}

#[test]
fn reloading_the_same_media_submits_nothing() {
    let (mut session, clock) = playing("a");
    let event = loaded(&mut session, 2, "a", Duration::from_secs(30));
    assert!(is_none(&session.observe(&event, clock.sample())));
}

#[test]
fn playing_clears_a_completed_flag_carried_in_from_the_file() {
    let mut opening = PersistedState::default();
    opening.record(
        &tenuto::resume::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let event = loaded(&mut session, 1, "a", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());

    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(&session, 1, "a", 5), clock.sample()));
    assert!(
        !state.completed_for(&media("a")),
        "§12: a successful establishment after a completed state clears it"
    );
}

// --------------------------------------------------- pending forces (D13)

#[test]
fn a_pause_from_playing_is_resolved_by_the_same_iterations_tick() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));

    // The application drains events first...
    assert!(is_none(&session.observe(
        &state_changed(1, PlaybackState::Paused),
        clock.sample()
    )));
    // ...then samples once, and that sample is newer than the transition.
    let (state, urgency) = submitted(session.tick(&progress(&session, 1, "a", 2), clock.sample()));

    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(2)
    );
}

#[test]
fn a_pause_that_interrupts_no_playback_raises_nothing() {
    // Every launch emits a StateChanged{Paused} nobody asked for, before the
    // queued Play is dispatched. Ungated, that would checkpoint the resume
    // landing on every launch — and zero it for a completed entry.
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let event = loaded(&mut session, 1, "a", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());

    assert!(is_none(&session.observe(
        &state_changed(1, PlaybackState::Paused),
        clock.sample()
    )));
    assert!(is_none(
        &session.tick(&progress(&session, 1, "a", 0), clock.sample())
    ));
}

#[test]
fn a_pause_after_a_stop_raises_nothing_either() {
    // D13 raises a force from `Playing` only, and an established session
    // reaches the same arm: a stop resolves its own force, and the `Paused` the
    // engine settles into afterwards must not force a second checkpoint.
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = submitted(session.tick(&progress(&session, 2, "a", 93), clock.sample()));

    assert!(is_none(&session.observe(
        &state_changed(2, PlaybackState::Paused),
        clock.sample()
    )));
    assert!(is_none(
        &session.tick(&progress(&session, 2, "a", 93), clock.sample())
    ));
}

#[test]
fn a_stop_raises_a_force_that_the_tick_resolves() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));
    assert!(is_none(&session.observe(
        &state_changed(2, PlaybackState::Stopped),
        clock.sample()
    )));

    let (state, urgency) = submitted(session.tick(&progress(&session, 2, "a", 93), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(93)
    );
}

#[test]
fn a_seek_persists_the_canonical_position_never_the_events_actual() {
    let (mut session, clock) = playing("a");
    let seek = PlaybackEvent::SeekCompleted {
        session_rev: 1,
        requested: Duration::from_secs(60),
        // A landing the M1 debt entry says can disagree with the position.
        actual: Duration::from_secs(59),
        refinement_truncated: false,
        provenance: PositionProvenance::Established,
    };
    assert!(is_none(&session.observe(&seek, clock.sample())));

    let (state, urgency) = submitted(session.tick(&progress(&session, 1, "a", 60), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(60),
        "D6 sidesteps the debt by never persisting SeekCompleted.actual"
    );
}

#[test]
fn a_force_is_rekeyed_across_a_device_recovery_and_still_resolves() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    // rebuild bumps the revision, with the position continuous across it.
    let _ = session.observe(
        &PlaybackEvent::DeviceRecovered { session_rev: 2 },
        clock.sample(),
    );

    let (state, urgency) = submitted(session.tick(&progress(&session, 2, "a", 40), clock.sample()));
    assert_eq!(
        urgency,
        Urgency::Forced,
        "a real pause must not be lost to an unrelated fault"
    );
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(40)
    );
}

#[test]
fn a_load_retires_a_force_raised_against_the_previous_media() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());

    // The load replaces the media; its own handling already recorded `a`.
    let event = loaded(&mut session, 2, "b", Duration::ZERO);
    let (state, _) = submitted(session.observe(&event, clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(93)
    );

    assert!(
        is_none(&session.tick(&progress(&session, 2, "b", 1), clock.sample())),
        "the retired force must not fire against the new media"
    );
}

// --------------------------------------------- the outstanding target (D17)

/// play → 93 s → stop → seek to 30 s → quit. The sequence D7 exists for.
#[test]
fn a_stopped_seek_target_survives_the_shutdown_force() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(&session, 2, "a", 93), clock.sample());

    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(30)
    );

    // The engine's canonical position still reads the pre-seek value, because a
    // stopped seek deliberately does not move it.
    let final_state = session.shutdown_snapshot(&progress(&session, 2, "a", 93), clock.sample());
    assert_eq!(
        final_state
            .entry_for(&media("a"))
            .unwrap()
            .position
            .unwrap(),
        Duration::from_secs(30),
        "the target supersedes Progress.position until the engine resolves it"
    );
}

/// stop → seek to 30 s → the same iteration's tick. Both the stop's force and
/// the target are outstanding, and the target is what the checkpoint uses.
#[test]
fn a_force_that_resolves_under_an_outstanding_target_records_the_target() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );

    // The stop raised its force before the seek stored anything, and the seek
    // re-keyed rather than retired it.
    let (state, urgency) = submitted(session.tick(&progress(&session, 2, "a", 93), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(30),
        "the pre-seek sample the engine still reports must not win over the target"
    );
}

/// stop → seek to 30 s → Home → play → quit. `restart()` discards the target
/// and announces Playing with no SeekCompleted, so Playing has to clear it.
#[test]
fn a_restart_clears_the_target_it_discarded() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(&session, 2, "a", 93), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );

    // Home: restart() seeks to zero, clears its own target and announces
    // Playing. No SeekCompleted is emitted for it, ever.
    let _ = session.observe(&state_changed(2, PlaybackState::Playing), clock.sample());
    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(&session, 2, "a", 5), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(5),
        "an ordinary checkpoint after a restart uses the sample, not the discarded target"
    );

    let final_state = session.shutdown_snapshot(&progress(&session, 2, "a", 7), clock.sample());
    assert_eq!(
        final_state
            .entry_for(&media("a"))
            .unwrap()
            .position
            .unwrap(),
        Duration::from_secs(7)
    );
}

#[test]
fn a_resumed_stopped_seek_clears_the_target_through_its_seek_completed() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );
    // `restore()` validates the stored target and emits the SeekCompleted the
    // caller has been waiting for.
    let _ = session.observe(
        &PlaybackEvent::SeekCompleted {
            session_rev: 2,
            requested: Duration::from_secs(30),
            actual: Duration::from_secs(30),
            refinement_truncated: false,
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    );
    let (state, _) = submitted(session.tick(&progress(&session, 2, "a", 31), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(31)
    );
}

/// A stopped seek is the same listener intent as a completed one, only earlier:
/// §12 has `SeekCompleted` clear completion, and a target persisted alongside
/// `completed: true` would be discarded by the resume decision that reads it.
#[test]
fn a_stopped_seek_clears_the_completion_its_target_supersedes() {
    let mut opening = PersistedState::default();
    opening.record(
        &tenuto::resume::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let event = loaded(&mut session, 1, "a", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 1,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    ));

    assert_eq!(urgency, Urgency::Forced);
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(entry.position.unwrap(), Duration::from_secs(30));
    assert!(
        !entry.completed,
        "the listener asked for 30 s; a retained completion would throw the target away"
    );

    let final_state = session.shutdown_snapshot(&progress(&session, 1, "a", 0), clock.sample());
    let entry = final_state.entry_for(&media("a")).unwrap();
    assert_eq!(entry.position.unwrap(), Duration::from_secs(30));
    assert!(!entry.completed);

    // The other half of the round trip: read back through the resume
    // decision, the target is what the listener gets, not what the earlier
    // `completed: true` would have discarded.
    assert_eq!(
        decide_resume(
            resume_candidate(entry.position, entry.completed),
            Some(Duration::from_secs(240).into())
        ),
        ResumeDecision::Resume(Duration::from_secs(30))
    );
}

// ------------------------------------------- the establishment gate (D20)

#[test]
fn a_launch_that_never_establishes_writes_no_checkpoint() {
    // A completed entry: §11 resumes it at 0, load() emits Loaded before
    // opening the device, and the device refuses to open.
    let mut opening = PersistedState::default();
    opening.record(
        &tenuto::resume::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let event = loaded(&mut session, 1, "a", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(
        &PlaybackEvent::Failed {
            session_rev: 1,
            message: "cannot open the audio device".into(),
            cause: None,
            request: None,
        },
        clock.sample(),
    );
    let _ = session.tick(&progress(&session, 1, "a", 0), clock.sample());

    let final_state = session.shutdown_snapshot(&progress(&session, 1, "a", 0), clock.sample());
    assert_eq!(
        final_state
            .entry_for(&media("a"))
            .unwrap()
            .position
            .unwrap(),
        Duration::from_secs(240),
        "a failed establishment must not overwrite the position D1 retains"
    );
    assert!(final_state.completed_for(&media("a")));
}

/// §11 maps `position == duration` to a start of zero while retaining the
/// position, with `completed` false — so the guard that skips a completed
/// outgoing entry is not what covers this one. `Loaded` reports that zero
/// before the device is opened, and
/// a switch would otherwise carry it out as the outgoing media's final word.
#[test]
fn a_switch_away_from_a_media_that_never_established_records_nothing_for_it() {
    let mut opening = PersistedState::default();
    opening.record(
        &tenuto::resume::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(300),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        false,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let event = loaded(&mut session, 1, "a", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(
        &PlaybackEvent::Failed {
            session_rev: 1,
            message: "cannot open the audio device".into(),
            cause: None,
            request: None,
        },
        clock.sample(),
    );

    // The listener gives up on `a` and picks another track.
    let event = loaded(&mut session, 2, "b", Duration::ZERO);
    let (state, _) = submitted(session.observe(&event, clock.sample()));
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(
        entry.position.unwrap(),
        Duration::from_secs(300),
        "nothing validated the zero the load reported, so the retained position stands"
    );
    assert!(!entry.completed);
}

/// A switch onto a completed entry, in an iteration where the engine's
/// `StateChanged{Loading}` never arrived — `emit` drops non-terminal events at
/// the backlog cap, which is exactly the pressure a switch is most likely
/// under. The session therefore sees `Loaded` while it still believes playback
/// is running, and the ordinary capture that follows would be a `Progress`
/// position for a media nothing has established.
#[test]
fn a_switch_onto_a_completed_entry_does_not_capture_its_unvalidated_zero() {
    let mut opening = PersistedState::default();
    opening.record(
        &tenuto::resume::PlaybackCheckpoint {
            media: media("b"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let event = loaded(&mut session, 1, "a", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));

    // The switch, with no StateChanged in front of it.
    let event = loaded(&mut session, 2, "b", Duration::ZERO);
    let _ = submitted(session.observe(&event, clock.sample()));
    assert!(
        is_none(&session.tick(&progress(&session, 2, "b", 0), clock.sample())),
        "nothing has established `b`, so the sample the engine reports for it is not a checkpoint"
    );

    let final_state = session.shutdown_snapshot(&progress(&session, 2, "b", 0), clock.sample());
    let entry = final_state.entry_for(&media("b")).unwrap();
    assert_eq!(
        entry.position.unwrap(),
        Duration::from_secs(240),
        "§11 resumes a completed entry at zero while retaining the position D1 keeps"
    );
    assert!(entry.completed);
}

/// The same failure as the shutdown force, one trigger earlier: §11 resumes a
/// completed entry at zero, `load()` reports `Loaded` before it opens the
/// device, and `do_stop` runs from the launch pause. The stop's force would
/// otherwise write that unvalidated zero over the position D1 retains.
#[test]
fn a_stop_before_anything_establishes_writes_no_checkpoint() {
    let mut opening = PersistedState::default();
    opening.record(
        &tenuto::resume::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let event = loaded(&mut session, 1, "a", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    // The launch pause nobody asked for, then a stop before the queued Play is
    // dispatched.
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Stopped), clock.sample());

    assert!(
        is_none(&session.tick(&progress(&session, 1, "a", 0), clock.sample())),
        "the force is answered by recording nothing, not by writing a position nothing validated"
    );

    let final_state = session.shutdown_snapshot(&progress(&session, 1, "a", 0), clock.sample());
    let entry = final_state.entry_for(&media("a")).unwrap();
    assert_eq!(entry.position.unwrap(), Duration::from_secs(240));
    assert!(entry.completed);
}

#[test]
fn a_launch_that_never_establishes_still_writes_volume_and_current_media() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let event = loaded(&mut session, 1, "a", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(
        &PlaybackEvent::VolumeChanged {
            session_rev: 1,
            volume: Volume::new(0.25),
        },
        clock.sample(),
    );

    let final_state = session.shutdown_snapshot(&progress(&session, 1, "a", 0), clock.sample());
    assert_eq!(final_state.volume(), Volume::new(0.25));
    assert_eq!(final_state.current_media(), Some(&media("a")));
    assert!(
        final_state.entry_for(&media("a")).is_none(),
        "neither of those is a position claim"
    );
}

#[test]
fn a_second_load_cannot_inherit_the_first_ones_establishment() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));

    // A new media that fails to open after Loaded.
    let event = loaded(&mut session, 2, "b", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let final_state = session.shutdown_snapshot(&progress(&session, 2, "b", 0), clock.sample());
    assert!(final_state.entry_for(&media("b")).is_none());
    assert_eq!(
        final_state
            .entry_for(&media("a"))
            .unwrap()
            .position
            .unwrap(),
        Duration::from_secs(93),
        "and the outgoing entry the load recorded stands"
    );
}

/// The gate asks whether anything established, not whether it established under
/// the revision now current: `rebuild` bumps the revision on device recovery
/// with the position continuous across it.
#[test]
fn a_device_recovery_does_not_re_gate_an_established_session() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(
        &PlaybackEvent::DeviceRecovered { session_rev: 2 },
        clock.sample(),
    );

    let final_state = session.shutdown_snapshot(&progress(&session, 2, "a", 40), clock.sample());
    assert_eq!(
        final_state
            .entry_for(&media("a"))
            .unwrap()
            .position
            .unwrap(),
        Duration::from_secs(40),
        "a revision bump the position is continuous across must not close the gate again"
    );
}

#[test]
fn a_media_switch_carries_the_outgoing_stopped_seek_target_out_with_it() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(&session, 2, "a", 93), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );

    let event = loaded(&mut session, 3, "b", Duration::ZERO);
    let (state, _) = submitted(session.observe(&event, clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position.unwrap(),
        Duration::from_secs(30),
        "the outgoing entry is recorded from the effective position, not the pre-seek sample"
    );
}

#[test]
fn a_media_switch_does_not_walk_a_completed_entry_backwards() {
    let (mut session, clock) = playing("a");
    let _ = submitted(session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(240),
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    ));
    // A tick after the end can only report a position at or behind the one
    // EndOfTrack already recorded.
    let _ = session.tick(&progress(&session, 1, "a", 239), clock.sample());

    let event = loaded(&mut session, 2, "b", Duration::ZERO);
    let (state, _) = submitted(session.observe(&event, clock.sample()));
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(
        entry.position.unwrap(),
        Duration::from_secs(240),
        "D1 retains what it retained"
    );
    assert!(entry.completed, "and the switch does not clear it either");
}

#[test]
fn the_shutdown_snapshot_refuses_a_position_from_a_session_it_was_not_tracking() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(&session, 1, "a", 93), clock.sample()));

    // A final Progress carrying a revision the policy never learned.
    let final_state = session.shutdown_snapshot(&progress(&session, 99, "a", 5), clock.sample());
    assert_eq!(
        final_state
            .entry_for(&media("a"))
            .unwrap()
            .position
            .unwrap(),
        Duration::from_secs(93),
        "it falls back to last_sample rather than trusting the stranger"
    );
}

// ------------------------------------------- checkpoint protection (§10)

#[test]
fn a_protected_entry_survives_every_capture_path() {
    // §10/H16. Periodic, pause, stop, the shutdown snapshot and a media
    // switch's outgoing entry all reach the state through record_current or
    // record_outgoing; one gate covers all five, and this test is what proves
    // none of them slipped past it.
    //
    // The shutdown leg is asserted *before* the switch below, while `ep1` is
    // still `current_media` — taking it after the switch would sample `ep2`
    // instead, which never establishes and so never reaches `record_current`
    // at all; that assertion would then just be re-checking what the switch's
    // own outgoing-entry write had already fixed one line above it.
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));

    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());

    // Periodic.
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(&session, 1, "ep1", 120), clock.sample());
    assert_eq!(stored_position(&session, &media("ep1")), retained);

    // Pause, then stop.
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.tick(&progress(&session, 1, "ep1", 130), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(&session, 1, "ep1", 130), clock.sample());
    assert_eq!(stored_position(&session, &media("ep1")), retained);

    // The shutdown snapshot, taken while `ep1` is still current, still
    // protected and still established.
    let state = session.shutdown_snapshot(&progress(&session, 1, "ep1", 130), clock.sample());
    assert_eq!(position_in(&state, &media("ep1")), retained);

    // A media switch carries the outgoing entry out — but not over this one.
    let event = loaded_fresh(&mut session, &media("ep2"));
    let _ = session.observe(&event, clock.sample());
    assert_eq!(stored_position(&session, &media("ep1")), retained);
}

#[test]
fn an_established_restart_lifts_the_protection() {
    // G1: without RestartEstablished this can never happen, and a listener who
    // deliberately started over would be unable to save that fact.
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());

    let _ = session.observe(
        &PlaybackEvent::RestartEstablished {
            session_rev: 1,
            position: Duration::ZERO,
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    );
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(&session, 1, "ep1", 30), clock.sample());

    assert_eq!(
        stored_position(&session, &media("ep1")),
        Duration::from_secs(30)
    );
}

#[test]
fn an_established_seek_lifts_the_protection() {
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());

    let _ = session.observe(
        &PlaybackEvent::SeekCompleted {
            session_rev: 1,
            requested: Duration::from_secs(60),
            actual: Duration::from_secs(60),
            refinement_truncated: false,
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    );
    let _ = session.tick(&progress(&session, 1, "ep1", 60), clock.sample());
    assert_eq!(
        stored_position(&session, &media("ep1")),
        Duration::from_secs(60)
    );
}

#[test]
fn verified_completion_lifts_the_protection_and_records_the_completion() {
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());

    let _ = session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(3000),
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    );
    assert_eq!(
        stored_position(&session, &media("ep1")),
        Duration::from_secs(3000)
    );
    assert!(completed_in(&session, &media("ep1")));
}

#[test]
fn a_capability_change_alone_never_lifts_the_protection() {
    // §10: "Capability changes alone never delete, clear or replace
    // checkpoints." A server that starts advertising ranges mid-session must
    // not be able to discard the entry by saying so.
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());

    let _ = session.observe(
        &PlaybackEvent::CapabilitiesChanged {
            session_rev: 1,
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
        },
        clock.sample(),
    );
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(&session, 1, "ep1", 30), clock.sample());

    assert_eq!(stored_position(&session, &media("ep1")), retained);
}

#[test]
fn a_fresh_sequential_session_with_nothing_to_protect_records_normally() {
    // §10's last paragraph: with no positive checkpoint to protect, heard
    // progress is recorded normally even though it cannot currently be
    // resumed.
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let event = loaded_fresh(&mut session, &media("ep1"));
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(&session, 1, "ep1", 45), clock.sample());
    assert_eq!(
        stored_position(&session, &media("ep1")),
        Duration::from_secs(45)
    );
}

#[test]
fn a_cancelled_seek_commits_no_target_and_leaves_an_outstanding_one_alone() {
    // A `Playing` transition would resolve the outstanding target itself
    // (D17's `restart()` case), which would mask whatever `SeekCancelled`
    // does or does not do to it — so this drives the target through the
    // pending-force path instead: `Stopped` raises a force without resolving
    // anything, and the tick that answers the force is what actually reads
    // `outstanding_target`.
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let event = loaded_fresh(&mut session, &media("ep1"));
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 1,
            target: Duration::from_secs(90),
        },
        clock.sample(),
    );
    let _ = session.observe(
        &PlaybackEvent::SeekCancelled {
            session_rev: 1,
            requested: Duration::from_secs(200),
        },
        clock.sample(),
    );
    let _ = session.observe(&state_changed(1, PlaybackState::Stopped), clock.sample());
    // `position_for` reads `outstanding_target.unwrap_or(sampled)`: 90 if
    // `SeekCancelled` left the target alone as it must, 5 (the sampled
    // position below) if it wrongly resolved it. 5 is the failure this test
    // is looking for.
    let _ = session.tick(&progress(&session, 1, "ep1", 5), clock.sample());
    assert_eq!(
        stored_position(&session, &media("ep1")),
        Duration::from_secs(90)
    );
}

#[test]
fn a_protected_tick_answers_none_rather_than_resubmitting_unchanged_state() {
    // §10: `checkpoint_from_progress` mirrors the `established` gate right
    // above it and returns `false` while protected, so `tick` answers
    // `Action::None` rather than resubmitting a state that did not change.
    // Without this, `CAPTURE_INTERVAL` (5 s) exceeds the writer's coalescing
    // window (2 s), so a protected run would rewrite the state file with
    // byte-identical content roughly every 5 s for as long as it stayed
    // protected — none of this file's other checks are sensitive to that
    // distinction, since they all discard the returned `Action`.
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());

    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    assert!(is_none(
        &session.tick(&progress(&session, 1, "ep1", 120), clock.sample())
    ));
}

#[test]
fn a_protected_entry_survives_a_stopped_seek_while_still_protected() {
    // Found while verifying the fix above: `checkpoint_from_progress`'s new
    // gate sits ahead of every tick- and shutdown-driven call into
    // `record_current`, so none of those paths still exercise
    // `record_current`'s own gate independently. `SeekTargetStored` is the
    // one caller that reaches `record_current` directly, never through
    // `checkpoint_from_progress` — so this is the only test in the file that
    // fails if `record_current`'s own gate is removed. A stopped seek is not
    // one of the three things that lift protection (§10), so its target must
    // not be committed while a fallback checkpoint is still protected.
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());

    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 1,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );
    assert_eq!(stored_position(&session, &media("ep1")), retained);
}

// ----------------------------------- estimated provenance (Task 6, §4.2-4.4)

/// The counterpart to `an_established_seek_lifts_the_protection`: an
/// *estimated* landing is not one of the two acts that earns the right to
/// overwrite an established checkpoint (§4.4), so `protected` must survive
/// it untouched. Ablation: deleting the `provenance == Established` guard on
/// `SeekCompleted` (reverting to the old unconditional `self.protected =
/// None`) makes this fail — `stored_position` would still read `retained`
/// either way (an estimated write can never touch `position`), which is
/// exactly why this asserts `stored_estimate` instead: with `protected`
/// wrongly cleared, the estimated write that follows would no longer be
/// gated and `estimated` would become `Some(60)`.
#[test]
fn an_estimated_seek_does_not_lift_the_protection() {
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());

    let _ = session.observe(
        &PlaybackEvent::SeekCompleted {
            session_rev: 1,
            requested: Duration::from_secs(60),
            actual: Duration::from_secs(60),
            refinement_truncated: false,
            provenance: PositionProvenance::Estimated,
        },
        clock.sample(),
    );
    let mut estimated_progress = progress(&session, 1, "ep1", 60);
    estimated_progress.provenance = PositionProvenance::Estimated;
    let _ = session.tick(&estimated_progress, clock.sample());

    assert_eq!(
        stored_position(&session, &media("ep1")),
        retained,
        "the established position must survive"
    );
    assert_eq!(
        stored_estimate(&session, &media("ep1")),
        None,
        "protected must still be in force, so the estimated write is gated too"
    );
}

/// The counterpart to `an_established_restart_lifts_the_protection`, for the
/// same reason: `RestartEstablished`'s name notwithstanding, an estimated
/// landing from it must not clear `protected` either. Ablation: as above —
/// deleting the guard clears `protected` and lets the estimated write through,
/// which only `stored_estimate` (not `stored_position`) can see.
#[test]
fn an_estimated_restart_does_not_lift_the_protection() {
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());

    let _ = session.observe(
        &PlaybackEvent::RestartEstablished {
            session_rev: 1,
            position: Duration::ZERO,
            provenance: PositionProvenance::Estimated,
        },
        clock.sample(),
    );
    let mut estimated_progress = progress(&session, 1, "ep1", 0);
    estimated_progress.provenance = PositionProvenance::Estimated;
    let _ = session.tick(&estimated_progress, clock.sample());

    assert_eq!(stored_position(&session, &media("ep1")), retained);
    assert_eq!(stored_estimate(&session, &media("ep1")), None);
}

/// §4.4: "verified completion is not a third exit." An earlier draft of this
/// plan disagreed; this test pins the corrected rule directly against
/// `protected` so it cannot creep back in. Ablation: making the `EndOfTrack`
/// handler clear `protected` unconditionally (the pre-Task-6 behaviour,
/// applied without regard to `provenance`) makes this fail on both
/// assertions — `stored_estimate` would become `Some(3000)` and
/// `completed_in` would flip to `true`, since an unprotected
/// `record_current_estimated` writes both fields.
#[test]
fn an_estimated_completion_does_not_lift_the_protection() {
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let event = loaded_unavailable(&mut session, &media("ep1"), retained);
    let _ = session.observe(&event, clock.sample());

    let _ = session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(3000),
            provenance: PositionProvenance::Estimated,
        },
        clock.sample(),
    );

    assert_eq!(
        stored_position(&session, &media("ep1")),
        retained,
        "the established position must survive an estimated completion"
    );
    assert_eq!(
        stored_estimate(&session, &media("ep1")),
        None,
        "protected must still gate the estimated write EndOfTrack raises"
    );
    assert!(
        !completed_in(&session, &media("ep1")),
        "the write that would have carried `completed` is itself gated"
    );
}

/// R8/§4.3: a `Loaded` reporting `ResumedEstimated` landed the listener at
/// the estimated location itself, not a fallback zero — there is no earlier
/// point to protect the way `ResumeUnavailable` protects one. Ablation: a
/// wrong `on_loaded` that treats `ResumedEstimated { established }` like
/// `ResumeUnavailable { retained }` (`self.protected = Some(established)`,
/// plausible from copying the match arm) makes this fail: the ordinary
/// capture below would then be gated and record nothing.
#[test]
fn a_resumed_estimated_load_sets_up_no_fallback_protection() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let request = session
        .register_load(LoadTarget::Detached, &media("ep1"))
        .expect("registered");
    let _ = session.observe(
        &PlaybackEvent::Loaded {
            session_rev: 1,
            request,
            media: media("ep1"),
            metadata: MediaMetadata::default(),
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::from_secs(97),
            disposition: StartDisposition::ResumedEstimated {
                established: Some(Duration::from_secs(40)),
            },
        },
        clock.sample(),
    );
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(&session, 1, "ep1", 100), clock.sample());

    assert_eq!(
        stored_position(&session, &media("ep1")),
        Duration::from_secs(100),
        "an ordinary established capture must not be gated by ResumedEstimated"
    );
}
