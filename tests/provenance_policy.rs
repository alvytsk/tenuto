//! Task 6 — §4.2's write rules and R4/R5: an estimated position may drive
//! display and resume, but must never replace an established checkpoint for
//! the same media, and must never bootstrap one into existing either.
//!
//! `tests/session_policy.rs` already owns the `protected` (§10) clearing-exit
//! tests this amendment adds (`an_estimated_seek_does_not_lift_the_protection`
//! and its two siblings) — driven the same way, against the same private
//! helpers, so they live there instead of being duplicated here. This file
//! covers the write-routing rules on their own terms: no `ResumeUnavailable`
//! fallback involved, just an established checkpoint (or its absence) and an
//! estimated landing.

use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::session::{Action, CAPTURE_INTERVAL, LoadTarget, Session};

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

fn progress(
    session: &Session,
    session_rev: u64,
    name: &str,
    secs: u64,
    provenance: PositionProvenance,
) -> Progress {
    Progress {
        session_rev,
        media: Some(media(name)),
        position: Duration::from_secs(secs),
        quality: PositionQuality::Exact,
        provenance,
        buffering: false,
        load: session.adopted().map(|adopted| adopted.request),
    }
}

fn submitted(action: Action) -> PersistedState {
    match action {
        Action::Submit { state, .. } => state,
        Action::None => panic!("expected a submission"),
    }
}

/// A file already carrying one *established* entry.
fn state_with(media: &MediaId, position: Duration, completed: bool) -> PersistedState {
    let mut state = PersistedState::default();
    state.record(
        &PlaybackCheckpoint {
            media: media.clone(),
            position,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        completed,
    );
    state
}

/// A file already carrying one entry that has *only ever* had an estimate
/// written to it — no established position at all.
fn state_with_estimate(media: &MediaId, estimated: Duration, completed: bool) -> PersistedState {
    let mut state = PersistedState::default();
    state.record_estimated(
        media.clone(),
        estimated,
        time::OffsetDateTime::UNIX_EPOCH,
        completed,
    );
    state
}

fn entry_position(session: &Session, media: &MediaId) -> Option<Duration> {
    session
        .state()
        .entry_for(media)
        .and_then(|entry| entry.position)
}

fn entry_estimate(session: &Session, media: &MediaId) -> Option<Duration> {
    session
        .state()
        .entry_for(media)
        .and_then(|entry| entry.estimated)
}

fn entry_completed(session: &Session, media: &MediaId) -> bool {
    session.state().completed_for(media)
}

/// A session already playing `name`, opened on `state`.
fn playing_on(state: PersistedState, name: &str, start: Duration) -> (Session, FakeClock) {
    let clock = FakeClock::new();
    let mut session = Session::new(state);
    let event = loaded(&mut session, 1, name, start);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    (session, clock)
}

// ------------------------------------------------------------- rule 1 (§4.2)

/// "With an established checkpoint present, estimated progress never writes
/// `position`. It writes `estimated` only." Ablation: routing
/// `checkpoint_from_progress` on `self.established` alone (ignoring
/// `position_provenance`) — i.e. always calling `record_current` — makes
/// this fail: `entry_position` would read `Some(45s)` instead of the
/// established `Some(40s)`.
#[test]
fn an_estimated_capture_never_overwrites_an_established_position() {
    let established = Duration::from_secs(40);
    let (mut session, clock) = playing_on(
        state_with(&media("a"), established, false),
        "a",
        established,
    );

    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let state = submitted(session.tick(
        &progress(&session, 1, "a", 45, PositionProvenance::Estimated),
        clock.sample(),
    ));

    assert_eq!(
        state.entry_for(&media("a")).and_then(|e| e.position),
        Some(established),
        "the established position must survive an estimated capture"
    );
    assert_eq!(
        state.entry_for(&media("a")).and_then(|e| e.estimated),
        Some(Duration::from_secs(45)),
        "the estimated location is still worth recording"
    );
}

// ------------------------------------------------------------- rule 2 (§4.2)

/// "With no established checkpoint for the media, an estimate may be
/// persisted — but only in the `estimated` representation. It never
/// bootstraps itself into `position`." Ablation: a `record_estimated` that
/// falls back to writing `position` for a fresh entry (or a routing bug that
/// calls `record_current` when nothing is yet established) makes this fail —
/// `entry_position` would read `Some(45s)` instead of `None`.
#[test]
fn an_estimated_capture_with_nothing_established_never_bootstraps_a_position() {
    let (mut session, clock) = playing_on(PersistedState::default(), "a", Duration::ZERO);

    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(
        &progress(&session, 1, "a", 45, PositionProvenance::Estimated),
        clock.sample(),
    );

    assert_eq!(
        entry_position(&session, &media("a")),
        None,
        "an estimate must never bootstrap a position that was never established"
    );
    assert_eq!(
        entry_estimate(&session, &media("a")),
        Some(Duration::from_secs(45))
    );
}

// ------------------------------------------------------------------- R4

/// "An estimated timeline *can* complete a track, but cannot promote its
/// timestamp." `completed` is real evidence of the body finishing either
/// way, but the terminal position beside it must go to `estimated`, never
/// `position`, when the anchor it was computed from was itself estimated.
/// Ablation: an `EndOfTrack` handler that ignores `provenance` and always
/// calls `record_current` (the pre-Task-6 behaviour) makes the `position`
/// assertion below fail — it would read `Some(3000s)` instead of the
/// established `Some(100s)`.
#[test]
fn an_estimated_end_of_track_sets_completed_without_promoting_its_position() {
    let established = Duration::from_secs(100);
    let (mut session, clock) = playing_on(
        state_with(&media("a"), established, false),
        "a",
        established,
    );

    let _ = session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(3000),
            provenance: PositionProvenance::Estimated,
        },
        clock.sample(),
    );

    assert_eq!(
        entry_position(&session, &media("a")),
        Some(established),
        "the established position must not be promoted by an estimated terminal value"
    );
    assert_eq!(
        entry_estimate(&session, &media("a")),
        Some(Duration::from_secs(3000)),
        "the estimated terminal position is still worth recording"
    );
    assert!(
        entry_completed(&session, &media("a")),
        "reaching the end of the body is real evidence of completion either way"
    );
}

// ------------------------------------------------------------------- R5

/// "A stored target inherits the provenance of the position it was computed
/// from." A target struck while the timeline was estimated is itself an
/// estimate, and must not overwrite an established `position`. Ablation: a
/// `SeekTargetStored` handler that always calls `record_current` (ignoring
/// `position_provenance`, which is exactly today's un-amended behaviour)
/// makes this fail — `entry_position` would read `Some(120s)` instead of the
/// established `Some(40s)`.
#[test]
fn a_stored_target_struck_from_an_estimated_position_inherits_its_provenance() {
    let established = Duration::from_secs(40);
    let (mut session, clock) = playing_on(
        state_with(&media("a"), established, false),
        "a",
        established,
    );

    // Land estimated, and let the deferred write resolve so
    // `position_provenance` is what a stopped seek would actually see.
    let _ = session.observe(
        &PlaybackEvent::SeekCompleted {
            session_rev: 1,
            requested: Duration::from_secs(90),
            actual: Duration::from_secs(90),
            refinement_truncated: false,
            provenance: PositionProvenance::Estimated,
        },
        clock.sample(),
    );
    let _ = session.tick(
        &progress(&session, 1, "a", 90, PositionProvenance::Estimated),
        clock.sample(),
    );
    assert_eq!(
        entry_position(&session, &media("a")),
        Some(established),
        "sanity: the seek landing alone must not have promoted the position"
    );

    // Stop, then a listener-initiated seek while stopped stores a target.
    let _ = session.observe(&state_changed(1, PlaybackState::Stopped), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 1,
            target: Duration::from_secs(120),
        },
        clock.sample(),
    );

    assert_eq!(
        entry_position(&session, &media("a")),
        Some(established),
        "the stored target must not overwrite the established position"
    );
    assert_eq!(
        entry_estimate(&session, &media("a")),
        Some(Duration::from_secs(120)),
        "the target itself is still worth recording, as an estimate"
    );
}

// ----------------------------------------------- established writes resume

/// "Established-checkpoint writes resume only once the absolute position is
/// independently established again." An entry that only ever carried an
/// estimate gets a real `position` — and the `record()` path (unchanged
/// from M1/M2) clears the stale estimate beside it, since it always replaces
/// the entry wholesale. Ablation: routing an `Established` capture to
/// `record_current_estimated` instead of `record_current` (the branches
/// swapped) makes both assertions fail — `entry_position` would stay `None`
/// and `entry_estimate` would become `Some(50s)` instead of clearing.
#[test]
fn an_established_capture_writes_the_position_and_clears_a_stale_estimate() {
    let (mut session, clock) = playing_on(
        state_with_estimate(&media("a"), Duration::from_secs(97), false),
        "a",
        Duration::ZERO,
    );

    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(
        &progress(&session, 1, "a", 50, PositionProvenance::Established),
        clock.sample(),
    );

    assert_eq!(
        entry_position(&session, &media("a")),
        Some(Duration::from_secs(50)),
        "the position is now independently established"
    );
    assert_eq!(
        entry_estimate(&session, &media("a")),
        None,
        "a stale estimate does not survive an established write"
    );
}

// --------------------------------------------------------- the second gate

/// "Where the gates go... find both." `record_outgoing` writes the outgoing
/// media's entry straight from `last_sample`, never through
/// `checkpoint_from_progress` / `record_current`, so it needs its own
/// routing decision exactly as it already needs its own `protected` gate
/// (§10). Ablation: a `record_outgoing` that always calls `state.record()`
/// (ignoring `position_provenance`, i.e. only the first gate was fixed) makes
/// this fail — the outgoing entry's `position` would read `Some(105s)`
/// instead of staying at the established `Some(40s)`.
#[test]
fn a_media_switch_writes_only_the_estimate_for_an_estimated_outgoing_entry() {
    let established = Duration::from_secs(40);
    let (mut session, clock) = playing_on(
        state_with(&media("a"), established, false),
        "a",
        established,
    );

    // Land estimated and let the deferred write resolve, recording
    // `estimated` once already.
    let _ = session.observe(
        &PlaybackEvent::SeekCompleted {
            session_rev: 1,
            requested: Duration::from_secs(90),
            actual: Duration::from_secs(90),
            refinement_truncated: false,
            provenance: PositionProvenance::Estimated,
        },
        clock.sample(),
    );
    let _ = session.tick(
        &progress(&session, 1, "a", 90, PositionProvenance::Estimated),
        clock.sample(),
    );

    // A further tick that samples a new position but is not yet due for a
    // capture: `last_sample` and `position_provenance` move, no checkpoint is
    // written. This is what lets `record_outgoing`'s own write below be
    // distinguished from the one above.
    assert!(matches!(
        session.tick(
            &progress(&session, 1, "a", 105, PositionProvenance::Estimated),
            clock.sample()
        ),
        Action::None
    ));

    // Switch media: `record_outgoing` fires for "a".
    let event = loaded(&mut session, 2, "b", Duration::ZERO);
    let _ = session.observe(&event, clock.sample());

    assert_eq!(
        entry_position(&session, &media("a")),
        Some(established),
        "record_outgoing must not promote an estimated sample to position"
    );
    assert_eq!(
        entry_estimate(&session, &media("a")),
        Some(Duration::from_secs(105)),
        "record_outgoing's own write must still land, as an estimate"
    );
}
