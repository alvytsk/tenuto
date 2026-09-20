mod support;

use std::time::Duration;

use support::media;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::command::LoadRequestId;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::queue::{Direction, DisplayMetadata, NewQueueEntry, QueueEntryId, QueueSource};
use tenuto::session::{Advance, LoadTarget, MAX_PENDING_LOADS, RegisterLoadError, Session};

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else {
        unreachable!()
    };
    NewQueueEntry::new(
        media(name),
        QueueSource::LocalFile(path),
        DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("a literal entry must be valid: {error}"))
}

fn queued(names: &[&str]) -> (Session, Vec<QueueEntryId>) {
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session
        .enqueue(
            session.state().playing(),
            names.iter().map(|n| entry(n)).collect(),
        )
        .unwrap_or_else(|error| panic!("fits: {error}"));
    (session, ids)
}

fn loaded(request: LoadRequestId, rev: u64, name: &str) -> PlaybackEvent {
    PlaybackEvent::Loaded {
        session_rev: rev,
        request,
        media: media(name),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position: Duration::ZERO,
        disposition: StartDisposition::Fresh,
    }
}

fn progress(rev: u64, name: &str, secs: u64, load: Option<LoadRequestId>) -> Progress {
    Progress {
        session_rev: rev,
        media: Some(media(name)),
        position: Duration::from_secs(secs),
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load,
    }
}

fn playing(rev: u64) -> PlaybackEvent {
    PlaybackEvent::StateChanged {
        session_rev: rev,
        state: PlaybackState::Playing,
        request: None,
    }
}

fn end(rev: u64, provenance: PositionProvenance) -> PlaybackEvent {
    PlaybackEvent::EndOfTrack {
        session_rev: rev,
        position: Duration::from_millis(500),
        provenance,
    }
}

#[test]
fn two_pending_loads_of_one_media_adopt_their_own_rows_in_event_order() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b", "a"]);
    let first = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    let second = session
        .register_load(LoadTarget::Queue(ids[2]), &media("a"))
        .expect("registered");
    assert_ne!(first, second);
    assert_eq!(
        session.state().queue().active(),
        None,
        "submitting adopts nothing"
    );

    session.observe(&loaded(first, 1, "a"), clock.sample());
    assert_eq!(session.state().queue().active(), Some(ids[0]));
    session.observe(&loaded(second, 2, "a"), clock.sample());
    assert_eq!(session.state().queue().active(), Some(ids[2]));
    assert_eq!(session.pending_load_count(), 0);
}

#[test]
fn reordering_a_pending_target_does_not_change_what_is_adopted() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b", "c"]);
    let request = session
        .register_load(LoadTarget::Queue(ids[2]), &media("c"))
        .expect("registered");
    session.move_entry(ids[2], Direction::Up).expect("known");
    session.move_entry(ids[2], Direction::Up).expect("known");
    session.observe(&loaded(request, 1, "c"), clock.sample());
    assert_eq!(session.state().queue().active(), Some(ids[2]));
}

#[test]
fn a_removed_pending_target_is_never_resurrected_and_its_playback_is_stopped() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let request = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("registered");
    session
        .remove_entry(ids[1], &progress(0, "b", 0, None), clock.sample())
        .expect("known");
    session.observe(&loaded(request, 1, "b"), clock.sample());
    assert_eq!(session.state().queue().active(), None);
    assert!(session.state().queue().get(ids[1]).is_none());
    assert!(session.take_stop_request());
    assert!(!session.take_stop_request(), "taken once");
}

#[test]
fn failure_before_adoption_keeps_the_previous_entry_and_its_checkpoint() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let first = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(first, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());
    clock.advance(Duration::from_secs(6));
    let _ = session.tick(&progress(1, "a", 30, Some(first)), clock.sample());
    let saved = session
        .state()
        .entry_for(&media("a"))
        .and_then(|e| e.position);

    let second = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("registered");
    // Progress for the unadopted load arrives before its outcome.
    let _ = session.tick(&progress(2, "b", 0, Some(second)), clock.sample());
    session.observe(
        &PlaybackEvent::Failed {
            session_rev: 2,
            message: "gone".into(),
            cause: None,
            request: Some(second),
        },
        clock.sample(),
    );

    assert_eq!(session.state().queue().active(), Some(ids[0]));
    assert_eq!(
        session
            .state()
            .entry_for(&media("a"))
            .and_then(|e| e.position),
        saved
    );
    assert!(session.state().entry_for(&media("b")).is_none());
    assert_eq!(session.pending_load_count(), 0);
}

#[test]
fn pending_registrations_are_bounded_and_retractable() {
    let (mut session, ids) = queued(&["a"]);
    let tokens: Vec<_> = (0..MAX_PENDING_LOADS)
        .map(|_| {
            session
                .register_load(LoadTarget::Queue(ids[0]), &media("a"))
                .expect("room")
        })
        .collect();
    assert_eq!(
        session.register_load(LoadTarget::Queue(ids[0]), &media("a")),
        Err(RegisterLoadError::Busy)
    );
    session.retract_load(tokens[0]);
    session.retract_load(tokens[1]);
    assert!(
        session
            .register_load(LoadTarget::Queue(ids[0]), &media("a"))
            .is_ok()
    );
    assert_eq!(session.pending_load_count(), MAX_PENDING_LOADS - 1);
    assert_eq!(
        session.register_load(LoadTarget::Queue(ids[0]), &media("zzz")),
        Err(RegisterLoadError::MediaMismatch)
    );
}

#[test]
fn device_recovery_keeps_the_adopted_load_while_another_is_pending() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let first = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(first, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());
    let _pending = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("registered");
    session.observe(
        &PlaybackEvent::DeviceRecovered { session_rev: 2 },
        clock.sample(),
    );
    clock.advance(Duration::from_secs(6));
    let action = session.tick(&progress(2, "a", 12, Some(first)), clock.sample());
    assert!(matches!(action, tenuto::session::Action::Submit { .. }));
    assert_eq!(session.adopted().map(|a| a.request), Some(first));
}

#[test]
fn completion_advances_once_for_either_provenance_and_never_from_a_stale_revision() {
    for provenance in [
        PositionProvenance::Established,
        PositionProvenance::Estimated,
    ] {
        let clock = FakeClock::new();
        let (mut session, ids) = queued(&["a", "b"]);
        let first = session
            .register_load(LoadTarget::Queue(ids[0]), &media("a"))
            .expect("registered");
        session.observe(&loaded(first, 3, "a"), clock.sample());

        let before_stale = serde_json::to_value(session.state()).expect("snapshot");
        session.observe(&end(2, provenance), clock.sample());
        assert_eq!(session.take_advance(), None, "stale revision");
        assert_eq!(
            serde_json::to_value(session.state()).expect("snapshot"),
            before_stale,
            "a stale-revision EndOfTrack must write nothing, including touch_seq/updated_at"
        );

        session.observe(&end(3, provenance), clock.sample());
        assert_eq!(session.take_advance(), Some(Advance::Next(ids[1])));
        let after = serde_json::to_value(session.state()).expect("snapshot");
        assert_ne!(after, before_stale, "the completion must actually write");

        let recorded = after.clone();
        session.observe(&end(3, provenance), clock.sample());
        assert_eq!(session.take_advance(), None, "deduplicated");
        assert_eq!(
            serde_json::to_value(session.state()).expect("snapshot"),
            recorded,
            "a duplicate completion, including its touch_seq/updated_at, must not write again"
        );
    }
}

#[test]
fn the_last_entry_ends_the_queue_without_wrapping() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a"]);
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());
    session.observe(&end(1, PositionProvenance::Established), clock.sample());
    assert_eq!(session.take_advance(), Some(Advance::EndOfQueue));
}

#[test]
fn a_legacy_adoption_clears_the_active_entry_and_keeps_the_queue() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());
    let legacy = session
        .register_load(LoadTarget::Legacy, &media("x"))
        .expect("registered");
    session.observe(&loaded(legacy, 2, "x"), clock.sample());
    assert_eq!(session.state().queue().active(), None);
    assert_eq!(session.state().queue().len(), 2);
    assert_eq!(session.state().current_media(), Some(&media("x")));
}

#[test]
fn unknown_and_duplicate_outcomes_select_nothing() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a"]);
    session.observe(
        &loaded(LoadRequestId::from_raw(999), 1, "a"),
        clock.sample(),
    );
    assert_eq!(session.state().queue().active(), None);
    assert!(!session.take_stop_request());
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(request, 2, "a"), clock.sample());
    session.observe(&loaded(request, 3, "a"), clock.sample());
    assert_eq!(session.adopted().map(|a| a.request), Some(request));
}

#[test]
fn an_adoption_snapshot_contains_the_new_media_active_entry_and_metadata() {
    use tenuto::session::Action;
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let a = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("register");
    session.observe(&loaded(a, 1, "a"), clock.sample());
    let b = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("register");
    let mut event = loaded(b, 2, "b");
    if let PlaybackEvent::Loaded { metadata, .. } = &mut event {
        metadata.title = Some("B title".into());
    }
    let Action::Submit { state, .. } = session.observe(&event, clock.sample()) else {
        panic!("snapshot")
    };
    assert_eq!(state.current_media(), Some(&media("b")));
    assert_eq!(state.queue().active(), Some(ids[1]));
    assert_eq!(
        state
            .queue()
            .get(ids[1])
            .and_then(|e| e.display().title.as_deref()),
        Some("B title")
    );
    let legacy = session
        .register_load(LoadTarget::Legacy, &media("x"))
        .expect("register");
    let Action::Submit { state, .. } = session.observe(&loaded(legacy, 3, "x"), clock.sample())
    else {
        panic!("snapshot")
    };
    assert_eq!(state.current_media(), Some(&media("x")));
    assert_eq!(state.queue().active(), None);
}

#[test]
fn completion_for_a_removed_pending_load_cannot_write_the_previous_history() {
    use tenuto::session::Action;
    for provenance in [
        PositionProvenance::Established,
        PositionProvenance::Estimated,
    ] {
        let clock = FakeClock::new();
        let (mut session, ids) = queued(&["a", "b"]);
        let a = session
            .register_load(LoadTarget::Queue(ids[0]), &media("a"))
            .expect("register");
        session.observe(&loaded(a, 1, "a"), clock.sample());
        session.observe(&playing(1), clock.sample());
        clock.advance(Duration::from_secs(6));
        session.tick(&progress(1, "a", 30, Some(a)), clock.sample());
        let b = session
            .register_load(LoadTarget::Queue(ids[1]), &media("b"))
            .expect("register");
        session
            .remove_entry(ids[1], &progress(1, "a", 30, Some(a)), clock.sample())
            .expect("remove");
        let before = serde_json::to_value(session.state()).expect("snapshot");
        session.observe(&loaded(b, 2, "b"), clock.sample());
        session.observe(&playing(2), clock.sample());
        assert!(matches!(
            session.observe(&end(2, provenance), clock.sample()),
            Action::None
        ));
        assert_eq!(
            serde_json::to_value(session.state()).expect("snapshot"),
            before
        );
        assert_eq!(session.take_advance(), None);
        assert!(session.take_stop_request());
    }
}

/// The invalidated-load sequence from
/// `completion_for_a_removed_pending_load_cannot_write_the_previous_history`,
/// replayed through `reconcile_shutdown` instead of `observe` directly — the
/// same ownership gates must hold in shutdown reconciliation (D20). `a`
/// stays incomplete at its retained 30 s sample and provenance; no `b`
/// checkpoint appears at all.
#[test]
fn reconcile_shutdown_replays_an_invalidated_load_sequence_without_writing_its_history() {
    use tenuto::playback::event::ShutdownReport;
    for provenance in [
        PositionProvenance::Established,
        PositionProvenance::Estimated,
    ] {
        let clock = FakeClock::new();
        let (mut session, ids) = queued(&["a", "b"]);
        let a = session
            .register_load(LoadTarget::Queue(ids[0]), &media("a"))
            .expect("register");
        session.observe(&loaded(a, 1, "a"), clock.sample());
        session.observe(&playing(1), clock.sample());
        clock.advance(Duration::from_secs(6));
        session.tick(&progress(1, "a", 30, Some(a)), clock.sample());
        let b = session
            .register_load(LoadTarget::Queue(ids[1]), &media("b"))
            .expect("register");
        session
            .remove_entry(ids[1], &progress(1, "a", 30, Some(a)), clock.sample())
            .expect("remove");

        let report = ShutdownReport {
            progress: progress(2, "b", 0, Some(b)),
            events: vec![loaded(b, 2, "b"), playing(2), end(2, provenance)],
        };
        let final_state = session.reconcile_shutdown(&report, clock.sample());
        let entry_a = final_state.entry_for(&media("a")).expect("a retained");
        assert_eq!(entry_a.position, Some(Duration::from_secs(30)));
        assert!(!entry_a.completed, "a is not the media that completed");
        assert!(
            final_state.entry_for(&media("b")).is_none(),
            "the invalidated load's history must never reach the file"
        );
    }
}

/// The other direct checkpoint-writing handler besides `EndOfTrack`:
/// `SeekTargetStored` from the rejected revision must be refused by the same
/// gate, rather than writing through unconditionally as it did before the
/// ownership check existed.
#[test]
fn a_seek_target_stored_from_a_rejected_revision_writes_nothing() {
    use tenuto::session::Action;
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let a = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("register");
    session.observe(&loaded(a, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());
    clock.advance(Duration::from_secs(6));
    session.tick(&progress(1, "a", 30, Some(a)), clock.sample());
    let b = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("register");
    session
        .remove_entry(ids[1], &progress(1, "a", 30, Some(a)), clock.sample())
        .expect("remove");
    // The rejected `Loaded` moves `last_loaded` off `a`'s token before its
    // stop lands, exactly as in the completion regression above.
    session.observe(&loaded(b, 2, "b"), clock.sample());

    let before = serde_json::to_value(session.state()).expect("snapshot");
    let action = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(90),
        },
        clock.sample(),
    );
    assert!(matches!(action, Action::None));
    assert_eq!(
        serde_json::to_value(session.state()).expect("snapshot"),
        before
    );
}

/// Fix round 1, Important 1: the engine bumps `session_rev` only on
/// `rebuild`, `load` and `do_stop` — not on `EndOfTrack`, a restart or a
/// seek. A listener who finishes a track, presses Home and plays it to the
/// end again produces two `EndOfTrack`s under the *same* `(adopted token,
/// session_rev)` key. The dedup must not treat the second as a duplicate of
/// the first: every accepted event that re-establishes playback
/// (`SeekCompleted`, `RestartEstablished`, `Playing` in `on_state`,
/// `SeekTargetStored` — everywhere `self.completed` is cleared) must also
/// clear `completion_seen`.
#[test]
fn a_restart_after_completion_lets_the_second_playthrough_complete_and_advance() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());

    session.observe(&end(1, PositionProvenance::Established), clock.sample());
    assert!(
        session.state().completed_for(&media("a")),
        "the first playthrough's completion must be recorded"
    );
    assert_eq!(
        session.take_advance(),
        Some(Advance::Next(ids[1])),
        "the first completion must advance"
    );

    // Home: an explicit restart re-establishes playback at the very same
    // revision the engine never bumped for EndOfTrack alone.
    session.observe(
        &PlaybackEvent::RestartEstablished {
            session_rev: 1,
            position: Duration::ZERO,
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    );
    session.observe(&playing(1), clock.sample());

    session.observe(&end(1, PositionProvenance::Established), clock.sample());
    assert!(
        session.state().completed_for(&media("a")),
        "the second playthrough's completion must be recorded, not swallowed by a stale dedup key"
    );
    assert_eq!(
        session.take_advance(),
        Some(Advance::Next(ids[1])),
        "the second playthrough must advance the queue too"
    );
}

/// Fix round 1, Important 2: `self.playback` must track the `Loading`
/// announcement for a registered-but-not-yet-adopted load, or the launch
/// `Paused` that follows the new load misreads `previous == Playing` (left
/// over from the *previous* adopted media) and raises a pending force
/// nothing asked for — breaking the invariant
/// `a_pause_that_interrupts_no_playback_raises_nothing` pins in
/// `session_policy.rs`.
#[test]
fn a_loading_announcement_for_a_registered_load_does_not_raise_a_spurious_pause_force() {
    use tenuto::session::Action;
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let a = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(a, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());

    let b = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("registered");
    session.observe(
        &PlaybackEvent::StateChanged {
            session_rev: 2,
            state: PlaybackState::Loading,
            request: Some(b),
        },
        clock.sample(),
    );

    // Ownership moved off "a": a tick at its old revision is rejected, and
    // its own EndOfTrack at the new revision changes nothing.
    assert!(matches!(
        session.tick(&progress(1, "a", 40, Some(a)), clock.sample()),
        Action::None
    ));
    assert!(matches!(
        session.observe(&end(2, PositionProvenance::Established), clock.sample()),
        Action::None
    ));

    // B's own load lands, then the launch Paused nobody asked for.
    session.observe(&loaded(b, 2, "b"), clock.sample());
    session.observe(
        &PlaybackEvent::StateChanged {
            session_rev: 2,
            state: PlaybackState::Paused,
            request: None,
        },
        clock.sample(),
    );
    // Without the fix, `self.playback` would have stayed `Playing` across
    // the `Loading` announcement, the Paused above would misread
    // `previous == Playing` and raise a pending force, and this tick would
    // write B's still-unvalidated launch position as a forced checkpoint.
    assert!(matches!(
        session.tick(&progress(2, "b", 0, Some(b)), clock.sample()),
        Action::None
    ));
}

/// A `Loading` announcement whose token this session never registered (or
/// has already retired) must not be mistaken for one of its own in-flight
/// loads: only the early-retirement branch for a *still-pending*
/// registration may clear `last_loaded`. Ownership of the already-adopted
/// media must survive it untouched.
#[test]
fn a_loading_announcement_for_an_unregistered_token_does_not_break_ownership() {
    use tenuto::session::Action;
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a"]);
    let first = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(first, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());

    session.observe(
        &PlaybackEvent::StateChanged {
            session_rev: 1,
            state: PlaybackState::Loading,
            request: Some(LoadRequestId::from_raw(999)),
        },
        clock.sample(),
    );

    let action = session.observe(&end(1, PositionProvenance::Established), clock.sample());
    assert!(
        matches!(action, Action::Submit { .. }),
        "ownership of the adopted media must still stand"
    );
}

/// Fix round 1, Important 3: `accepts_media_event` must require currency,
/// not only a valid registration, for `Loaded` — matching the brief's "a
/// current, valid registered target/media."
#[test]
fn accepts_media_event_rejects_a_loaded_from_a_stale_revision() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let first = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(first, 3, "a"), clock.sample());

    let second = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("registered");
    let stale = loaded(second, 2, "b");
    assert!(
        !session.accepts_media_event(&stale),
        "a known Loaded behind latest_engine_rev must not read as accepted"
    );
}
