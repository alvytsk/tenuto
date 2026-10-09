//! Contract tests worded directly from the M0 invariant:
//! "Stop and transport recreation preserve position; restoration, media
//! selection, explicit restart, and successful seeks establish a new position."

use std::time::Duration;

use tenuto::media::id::MediaId;
use tenuto::media::provenance::PositionProvenance;
use tenuto::media::source::SourceLocation;
use tenuto::playback::command::{LoadRequestId, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::playback::state::PlaybackState;
use tenuto::resume::ResumeCandidate;
use tenuto::volume::Volume;

mod support;
use support::TestEngine;

/// Long enough that no margin in this file depends on how far the harness
/// clock ran while a loaded machine had the test thread descheduled. A pause
/// lands wherever the ring and the scheduler leave it, and the test then has to
/// play on past that point; half a second of media has no room for either.
const TRACK: &str = "sine-5s.flac";

/// Short on purpose, for the tests whose subject *is* the end of the track.
const SHORT_TRACK: &str = "sine.flac";

#[test]
fn stop_preserves_the_logical_position() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(engine.position(), before, "stop must not reset position");
}

#[test]
fn an_ordinary_local_seek_reports_an_established_landing() {
    // The M1 path is unchanged: a local FLAC file's refined seek lands where
    // it says, and nothing about this milestone may make it claim otherwise.
    //
    // M3.1 Task 4's `SeekMode::Coarse` swap has exactly one call site,
    // shared by local and remote sources - but provenance follows the
    // demuxer that actually *ran* `Coarse`, not the mode this call requests
    // uniformly (fix round 1). MP3's `MpaReader` is the only
    // `FormatReader::seek` in this crate's dependency tree that reads `mode`
    // at all; FLAC's own seek binary-searches on real per-frame sample
    // numbers carried in the frame headers, so `Coarse` and `Accurate`
    // execute byte-identical code for it. `TRACK` (`sine-5s.flac`) is FLAC,
    // so this landing is exactly as decoder-confirmed as it always was. An
    // earlier round of this task flipped this assertion to `Estimated`
    // unconditionally; that was wrong, and this is the corrected test - see
    // `a_remote_mp3_seek_reports_estimated_and_a_local_flac_seek_reports_
    // established` for the two pinned side by side.
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::SeekTo(Duration::from_secs(2)));
    let completed = engine.await_seek_completed(Duration::from_secs(10));
    assert_eq!(completed.provenance, PositionProvenance::Established);
    engine.finish();
}

#[test]
fn an_established_duration_still_clamps_a_seek() {
    // M1/M2 behaviour, unchanged: a seek requested past a known (established)
    // duration is clamped to it before the engine ever attempts anything.
    // Stopped rather than playing, so this observes `clamp_target`'s output
    // directly through `SeekTargetStored` without also depending on whether
    // the decoder accepts a real seek to the exact last instant of the file
    // (a separate, unrelated concern `SeekMode`/`max_ts` own). Without this
    // test, an implementation that simply deleted the clamp entirely (rather
    // than skipping it only for an estimated duration) would still look
    // correct.
    let mut engine = TestEngine::start(TRACK); // sine-5s.flac: 5s, established.
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    engine.send(PlaybackCommand::SeekTo(Duration::from_secs(100)));
    let event = engine.await_event(|e| matches!(e, PlaybackEvent::SeekTargetStored { .. }));
    let PlaybackEvent::SeekTargetStored { target, .. } = event else {
        unreachable!("await_event's predicate already matched SeekTargetStored")
    };
    assert_eq!(
        target,
        Duration::from_secs(5),
        "a seek past a known duration must be clamped to it before being stored, not kept as-is"
    );
}

#[test]
fn play_from_stopped_resumes_at_the_preserved_position_without_resetting() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    let preserved = engine.position();
    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);
    // Exact, not `>=`: the clock is frozen across the transition, so any
    // movement at all is a bug rather than playback. A `>=` here would accept
    // the forward jump a mis-ordered generation reset produces.
    assert_eq!(
        engine.position(),
        preserved,
        "resume must continue from the preserved position, not rewind or jump"
    );
}

#[test]
fn pause_holds_the_position_still_and_resume_continues_from_it() {
    // Pause and resume are the same transport generation: the callback keeps
    // its cumulative counter and the timeline keeps its floor. If either were
    // voided, resuming would jump the position by everything played since the
    // generation was installed.
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::Pause);
    engine.await_state(PlaybackState::Paused);
    // The frames already handed to the device still play out after the park,
    // so let that settle before taking the reading that must not move.
    engine.let_time_pass(Duration::from_millis(300));
    let paused_at = engine.position();
    engine.let_time_pass(Duration::from_millis(300));
    assert_eq!(
        engine.position(),
        paused_at,
        "a parked transport must not advance the position"
    );
    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);
    assert_eq!(
        engine.position(),
        paused_at,
        "resume must continue from the paused position, not jump or rewind"
    );
    engine.play_for(paused_at + Duration::from_millis(100));
    assert!(
        engine.position() > paused_at,
        "playback must actually run again after resuming"
    );
}

#[test]
fn transport_recreation_preserves_position() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.force_device_loss();
    engine.await_event(|e| matches!(e, PlaybackEvent::DeviceRecovered { .. }));
    assert_eq!(
        engine.position(),
        before,
        "recreation must preserve the position exactly, not merely not reset it"
    );
}

#[test]
fn a_discard_that_times_out_does_not_rebuild_ahead_of_where_playback_was() {
    // Regression. `reinstall` used to move the anchor onto the freshly seeked
    // target *before* asking the callback to discard the old generation's ring.
    // A discard that timed out dropped into `rebuild`, whose capture then ran
    // against a timeline that had never been reset and still held the previous
    // generation's spans: it read back `new anchor + everything played since
    // the last install`, and the transport came back that far ahead of where
    // the media actually was. Nothing anywhere reported it.
    //
    // Only `Discard` goes unanswered, so the recovery still meets a live device
    // and really does capture - which is the whole point. A wholly dead device
    // takes the rescue path instead and never touches the stale timeline.
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    let played_to = engine.position();
    engine.stop_answering_discards();
    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(50)));
    engine.await_event(|e| matches!(e, PlaybackEvent::DeviceRecovered { .. }));
    engine.answer_discards_again();
    let landed = engine.position();
    assert!(
        landed <= played_to,
        "a rebuild may fall back to where playback was, but never jump past it; \
         playback was at {played_to:?} and the rebuild landed at {landed:?}"
    );
}

#[test]
fn a_recovery_that_never_completes_fails_once_and_keeps_the_position() {
    // The device stops answering, so every handshake in the recovery runs to
    // its deadline: the capture times out, and so does the install that would
    // have brought the transport back. This is the milestone's invariant on its
    // worst path - the pipeline is gone and cannot be rebuilt, and the logical
    // position still has to survive it.
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.silence_the_device();
    engine.force_device_loss();
    engine.await_state(PlaybackState::Failed);
    assert_eq!(
        engine.position(),
        before,
        "a failed recovery must still preserve the position"
    );
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::Failed { .. })),
        1,
        "a failure is reported once, not on every pass of the loop"
    );
}

#[test]
fn a_recovery_cancelled_by_stop_ends_stopped_rather_than_failed() {
    // Regression: a stop arriving during a recovery's re-seek used to come back
    // as a rebuild failure, which turned the stop the user asked for into a
    // spurious `Failed` that `do_stop` then refused to correct.
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.silence_the_device();
    engine.force_device_loss();
    engine.await_recovery_capture();
    engine.interrupt_stop();
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::Failed { .. })),
        0,
        "a cancelled recovery is not a failure"
    );
    assert_eq!(
        engine.position(),
        before,
        "a cancelled recovery preserves the position too"
    );
}

#[test]
fn a_successful_seek_establishes_a_new_position() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(100));
    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(300)));
    let event = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCompleted { .. }));
    let PlaybackEvent::SeekCompleted { actual, .. } = event else {
        unreachable!()
    };
    assert!(actual.as_millis().abs_diff(300) <= 5);
    // The event alone only shows what the decoder did. The engine must also
    // have adopted it as the position it reports.
    assert!(
        engine.position() >= Duration::from_millis(295),
        "a successful seek must establish the new position, got {:?}",
        engine.position()
    );
}

#[test]
fn a_seek_while_stopped_stores_a_target_and_does_not_claim_completion() {
    // Regression: emitting SeekCompleted only after Play made Restart stall.
    let mut engine = TestEngine::start(TRACK);
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(300)));
    let event = engine.await_event(|e| {
        matches!(
            e,
            PlaybackEvent::SeekTargetStored { .. } | PlaybackEvent::SeekCompleted { .. }
        )
    });
    assert!(matches!(event, PlaybackEvent::SeekTargetStored { .. }));
}

#[test]
fn restart_works_from_stopped_and_from_ended() {
    for setup in [PlaybackState::Stopped, PlaybackState::Ended] {
        let mut engine = TestEngine::start(SHORT_TRACK);
        match setup {
            PlaybackState::Stopped => {
                engine.send(PlaybackCommand::Stop);
                engine.await_state(PlaybackState::Stopped);
            }
            _ => engine.play_to_end(),
        }
        engine.send(PlaybackCommand::Restart);
        engine.await_state(PlaybackState::Playing);
        assert!(engine.position() < Duration::from_millis(100));
    }
}

#[test]
fn play_from_ended_does_not_restart_implicitly() {
    let mut engine = TestEngine::start(SHORT_TRACK);
    engine.play_to_end();
    engine.send(PlaybackCommand::Play);
    engine.await_event(|e| matches!(e, PlaybackEvent::Warning { .. }));
    assert_eq!(engine.state(), PlaybackState::Ended);
}

#[test]
fn end_of_track_waits_for_the_final_frames_predicted_play_time() {
    let mut engine = TestEngine::start(SHORT_TRACK);
    engine.drain_ring_without_advancing_clock();
    // Asserted on the two things that would *be* a premature end of track, not
    // on the inbox being empty: a contended machine also makes the worker miss
    // spans, and the diagnostic warning that reports the loss says nothing
    // about when EOF fires. `count_events` pumps for a further 200 ms of real
    // time while the virtual clock stays where the drain left it, so a
    // too-eager EOF has more room to show up here than a bare peek gave it.
    let premature = engine.count_events(|e| matches!(e, PlaybackEvent::EndOfTrack { .. }));
    assert_eq!(
        premature, 0,
        "EOF must not fire when the ring merely empties"
    );
    assert_eq!(
        engine.state(),
        PlaybackState::Playing,
        "an empty ring is not the end of the track"
    );
    engine.advance_past_output_latency();
    engine.await_event(|e| matches!(e, PlaybackEvent::EndOfTrack { .. }));
}

#[test]
fn progress_carries_the_session_revision_it_belongs_to() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(100));
    let first = engine.progress();
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    assert_ne!(engine.progress().session_rev, first.session_rev);
}

#[test]
fn stop_travels_out_of_band_past_a_pending_backlog() {
    // Stop travels on the interrupt flag plus the wake channel, so neither a
    // queue of commands nor a backlog of undelivered events can delay it.
    // Note that this does not exercise the admission limit: each TogglePause
    // costs two handshakes, so Stop arrives long before enough events pile up
    // to saturate the channel. The test below covers admission.
    let mut engine = TestEngine::start(TRACK);
    engine.stop_draining_events();
    for _ in 0..256 {
        engine.send(PlaybackCommand::TogglePause);
    }
    engine.interrupt_stop();
    engine.resume_draining_events();
    engine.await_state(PlaybackState::Stopped);
}

#[test]
fn a_backlog_closes_command_admission_until_it_drains() {
    // The test above cannot reach saturation on its own: each TogglePause
    // costs two handshakes, so Stop arrives long before 64 events pile up.
    // This one uses a command whose whole cost is the event it emits, which
    // is what makes the admission limit observable - the command channel
    // still holds most of the batch because the worker stopped taking from it.
    let mut engine = TestEngine::start(TRACK);
    engine.stop_draining_events();
    for step in 0..256u32 {
        engine.send(PlaybackCommand::SetVolume(Volume::new(step as f32 / 256.0)));
    }
    std::thread::sleep(Duration::from_millis(300));
    let pending = engine.pending_commands();
    assert!(
        pending > 128,
        "admission must halt while a backlog exists; only {pending} of 256 commands were left unread"
    );
    engine.interrupt_stop();
    engine.resume_draining_events();
    engine.await_state(PlaybackState::Stopped);
}

/// The underrun count an aggregated diagnostic warning reports, or zero if the
/// message is not one.
fn reported_underruns(message: &str) -> usize {
    let words: Vec<&str> = message.split_whitespace().collect();
    words
        .windows(2)
        .find(|pair| pair[1] == "underruns,")
        .and_then(|pair| pair[0].parse().ok())
        .unwrap_or(0)
}

#[test]
fn diagnostics_aggregate_rather_than_accumulating_events() {
    // A bare upper bound on the warning count proves nothing: the event channel
    // holds 64 slots with 8 held back, so no more than 56 warnings can ever be
    // emitted even with coalescing deleted outright. What distinguishes
    // coalescing from its absence is that a handful of events account for every
    // single injected xrun - so assert both halves.
    const XRUNS: usize = 10_000;
    let mut engine = TestEngine::start(TRACK);
    engine.stop_draining_events();
    engine.inject_xruns(XRUNS);
    engine.resume_draining_events();
    let aggregate = engine.await_event(|e| match e {
        PlaybackEvent::Warning { message, .. } => reported_underruns(message) >= XRUNS,
        _ => false,
    });
    let PlaybackEvent::Warning { message, .. } = &aggregate else {
        panic!("await_event returned {aggregate:?}, which is not a warning");
    };
    assert!(
        reported_underruns(message) >= XRUNS,
        "one warning must account for all {XRUNS} xruns, got {message:?}"
    );
    // `await_event` took the aggregate out of the inbox; the rest stayed.
    let others = engine.count_events(|e| matches!(e, PlaybackEvent::Warning { .. }));
    let total = others + 1;
    assert!(
        total < 10,
        "{XRUNS} xruns must coalesce into a handful of events, got {total} warnings"
    );
}

#[test]
fn a_device_with_more_than_two_channels_is_refused_rather_than_played_wrong() {
    // M1 is scoped to mono and stereo. A 6-channel default is routine on HDMI
    // and PipeWire, and the converter would fill only two of every six slots:
    // playback three times too fast, position inflated by the same factor, and
    // not a word of it anywhere. Refusing is the contract until downmixing
    // lands.
    let message = support::load_failure_on_device(SHORT_TRACK, 6);
    assert!(
        message.contains('6') && message.contains("channels"),
        "the refusal must name the negotiated channel count, got {message:?}"
    );
}

#[test]
fn a_disconnected_event_receiver_terminates_the_worker() {
    let engine = TestEngine::start(TRACK);
    engine.drop_event_receiver();
    assert!(engine.join_within(Duration::from_secs(2)));
}

#[test]
fn a_seek_stored_while_stopped_reports_completion_when_it_is_finally_validated() {
    // Regression: a seek taken while stopped stores an unvalidated target and
    // emits SeekTargetStored. Resuming consumed that target and started playing
    // without ever emitting SeekCompleted, so anything waiting for the
    // acknowledgement of that seek waited forever.
    let mut engine = TestEngine::start(TRACK);
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);

    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(1_000)));
    engine.await_event(|e| matches!(e, PlaybackEvent::SeekTargetStored { .. }));

    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);

    let event = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCompleted { .. }));
    let PlaybackEvent::SeekCompleted {
        requested, actual, ..
    } = event
    else {
        unreachable!()
    };
    assert_eq!(requested, Duration::from_millis(1_000));
    assert!(
        actual.as_millis().abs_diff(1_000) <= 50,
        "validated landing was {actual:?}"
    );
}

#[test]
fn a_failed_load_keeps_the_position_that_was_asked_for() {
    // Regression: `load` zeroed the position before opening the source, so a
    // load that failed reported zero instead of the requested start. A retry
    // then resumed from the beginning rather than where the caller asked.
    let missing = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/definitely-not-here.flac");
    let start_at = Duration::from_secs(123);
    let (state, position) = support::failed_load_position(&missing, start_at);
    assert_eq!(state, PlaybackState::Failed);
    assert_eq!(
        position, start_at,
        "a failed load lost the requested resume position"
    );
}

// ----------------------------------------------- resume intent and disposition

#[test]
fn a_load_that_resumes_reports_a_resumed_disposition() {
    let mut engine = TestEngine::start(TRACK);
    engine.load_with_resume(
        support::fixture(TRACK),
        ResumeIntent::Candidate(ResumeCandidate {
            position: Duration::from_secs(2),
            completed: false,
        }),
    );
    let loaded = engine.await_loaded();
    assert_eq!(loaded.disposition, StartDisposition::Resumed);
    assert!(loaded.position >= Duration::from_secs(2));
}

#[test]
fn a_load_of_a_completed_entry_replays_from_zero_and_says_so() {
    let mut engine = TestEngine::start(TRACK);
    engine.load_with_resume(
        support::fixture(TRACK),
        ResumeIntent::Candidate(ResumeCandidate {
            position: Duration::from_secs(2),
            completed: true,
        }),
    );
    let loaded = engine.await_loaded();
    assert_eq!(loaded.disposition, StartDisposition::CompletedReplay);
    assert_eq!(loaded.position, Duration::ZERO);
}

// --------------------------------------- §4.3 restart preference (Task 6 fix)

/// The end-to-end wiring §4.2/§4.3 was missing: a `ResumeIntent` carrying
/// both a stored `estimated` location and its established fallback must
/// land the load *at the estimate*, not at `position` — and report which
/// established value it kept beside it. Ablation: routing
/// `ResumeIntent::EstimatedCandidate` through the same branch as `Candidate`
/// (i.e. seeking to `established` instead of `target`) makes the position
/// assertion fail — `loaded.position` would land near 1 s instead of 3 s.
#[test]
fn a_load_with_both_locations_resumes_at_the_estimate_and_reports_the_kept_fallback() {
    let mut engine = TestEngine::start(TRACK);
    engine.load_with_resume(
        support::fixture(TRACK),
        ResumeIntent::EstimatedCandidate {
            target: Duration::from_secs(3),
            established: Some(Duration::from_secs(1)),
        },
    );
    let loaded = engine.await_loaded();
    assert_eq!(
        loaded.disposition,
        StartDisposition::ResumedEstimated {
            established: Some(Duration::from_secs(1))
        }
    );
    assert!(
        loaded.position >= Duration::from_secs(3),
        "the load must land at the estimate, not the established fallback: {:?}",
        loaded.position
    );
}

/// R8: an estimate-only entry (nothing ever established) must report
/// `established: None`, never a fabricated fallback — and must still resume
/// at the estimate. Ablation: a wrong `resume_intent_for` (or a wrong
/// engine-side pass-through) that substitutes `Duration::ZERO` for the
/// absent established position would be indistinguishable from a genuine
/// zero-established entry, which is exactly the "never established" vs.
/// "established at the start" conflation R8 forbids — caught here because
/// `established` is asserted as `None`, not merely "not `Some(1s)`".
#[test]
fn an_estimate_only_load_resumes_at_the_estimate_and_reports_no_established_fallback() {
    let mut engine = TestEngine::start(TRACK);
    engine.load_with_resume(
        support::fixture(TRACK),
        ResumeIntent::EstimatedCandidate {
            target: Duration::from_secs(3),
            established: None,
        },
    );
    let loaded = engine.await_loaded();
    assert_eq!(
        loaded.disposition,
        StartDisposition::ResumedEstimated { established: None }
    );
    assert!(loaded.position >= Duration::from_secs(3));
}

#[test]
fn an_explicit_restart_announces_that_it_established() {
    // G1. Without this event `Session` cannot tell a restart from any other
    // establishment, and §10's protection can never be lifted.
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::Restart);
    let established = engine.await_restart_established();
    assert_eq!(established, Duration::ZERO);
}

fn local_load(request: u64, name: &str) -> PlaybackCommand {
    let path = support::fixture(name);
    PlaybackCommand::Load {
        request: LoadRequestId::from_raw(request),
        media: MediaId::LocalFile(path.clone()),
        source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    }
}

#[test]
fn a_load_echoes_its_request_on_loading_loaded_and_progress() {
    let mut engine = TestEngine::start_idle();
    engine.send(local_load(41, "sine.flac"));
    let loading = engine.await_event(|e| {
        matches!(
            e,
            PlaybackEvent::StateChanged {
                state: PlaybackState::Loading,
                ..
            }
        )
    });
    assert!(
        matches!(loading, PlaybackEvent::StateChanged { request: Some(r), .. } if r.get() == 41)
    );
    let loaded = engine.await_event(|e| matches!(e, PlaybackEvent::Loaded { .. }));
    assert!(matches!(loaded, PlaybackEvent::Loaded { request, .. } if request.get() == 41));
    engine.await_state(PlaybackState::Paused);
    assert_eq!(engine.progress().load.map(LoadRequestId::get), Some(41));
    engine.interrupt_stop();
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(
        engine.progress().load.map(LoadRequestId::get),
        Some(41),
        "stop keeps the adopted load"
    );
    engine.finish();
}

#[test]
fn an_open_failure_is_the_load_outcome_for_its_request() {
    let mut engine = TestEngine::start_idle();
    let missing = std::env::temp_dir().join("tenuto-m5-definitely-missing.flac");
    engine.send(PlaybackCommand::Load {
        request: LoadRequestId::from_raw(42),
        media: MediaId::LocalFile(
            tenuto::media::id::AbsolutePath::new(missing.clone()).expect("absolute"),
        ),
        source: SourceLocation::LocalPath(missing),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    });
    let failed = engine.await_event(|e| matches!(e, PlaybackEvent::Failed { .. }));
    assert!(matches!(failed, PlaybackEvent::Failed { request: Some(r), .. } if r.get() == 42));
    engine.finish();
}

#[test]
fn a_device_failure_after_loaded_is_not_a_load_outcome() {
    let report = support::failed_device_session("sine.flac", 6, Duration::ZERO);
    let loaded = report
        .events
        .iter()
        .position(|e| matches!(e, PlaybackEvent::Loaded { .. }))
        .expect("loaded");
    let failed = report
        .events
        .iter()
        .position(|e| matches!(e, PlaybackEvent::Failed { .. }))
        .expect("failed");
    assert!(loaded < failed);
    assert!(matches!(
        report.events[failed],
        PlaybackEvent::Failed { request: None, .. }
    ));
}
