//! Acceptance evidence for Task 10: HTTP sources reaching the decode worker
//! itself, through a real `EngineHandle` over `TestOutput`, against the
//! loopback server. Every test binds `127.0.0.1`; no test touches the public
//! network.

mod support;

use std::time::{Duration, Instant};

use tenuto::media::capabilities::SeekSupport;
use tenuto::playback::command::{Admission, PlaybackCommand};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::state::PlaybackState;

use support::server::{Script, TestServer};
use support::{TestEngine, fixture_path};

/// Blocks until `server` has recorded at least one request, or `patience`
/// elapses. The proof that a header-stalled wait was actually entered: the
/// request line and headers are parsed and recorded before the server ever
/// parks on the stall gate, so a nonempty list means the worker's read is now
/// genuinely blocked on that connection.
fn wait_for_request(server: &TestServer, patience: Duration) -> bool {
    let deadline = Instant::now() + patience;
    loop {
        if !server.requests().is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn remote_playback_reaches_the_test_output_before_the_body_completes() {
    // H1: the whole point of finite HTTP media is that playback starts
    // before the transfer finishes. `sine-5s.flac` never has to arrive in
    // full for 200 ms of it to have been heard.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));

    assert!(
        engine.progress().position >= Duration::from_millis(150),
        "playback did not reach the test output: {:?}",
        engine.progress().position
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_forward_seek_installs_the_media_position_and_requests_that_byte() {
    // H2: a seek's landing must be both a local fact (the engine's own
    // position) and a remote one (the byte range the server actually saw).
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
        Admission::Accepted
    );
    let landed = engine.await_seek_completed(Duration::from_secs(10));
    assert!(
        landed >= Duration::from_secs(2),
        "the seek landed short at {landed:?}"
    );
    assert!(
        engine.progress().position >= Duration::from_secs(2),
        "the engine's own position was not installed at the seek's landing"
    );

    let ranged = server
        .requests()
        .into_iter()
        .rev()
        .find_map(|request| request.range());
    let Some((first, _)) = ranged else {
        panic!("no ranged request for the seek ever reached the server");
    };
    assert!(
        first > 0,
        "the seek's request did not target a nonzero byte offset: {first}"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn stop_closes_the_fetch_and_play_reopens_at_the_preserved_position() {
    // H3: stop preserves the logical position exactly as it does for a local
    // file, even though the decoder over a remote source is dropped rather
    // than merely parked - and the reopen `play` performs lands back there.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));
    let preserved = engine.progress().position;

    let before = server.requests().len();
    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(
        engine.progress().position,
        preserved,
        "stop must not reset the preserved position"
    );

    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    assert!(
        engine.progress().position >= preserved,
        "the reopen did not resume at the preserved position: {:?} < {preserved:?}",
        engine.progress().position
    );
    assert!(
        server.requests().len() > before,
        "H3: play after stop did not open a new request"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_range_less_server_plays_sequentially_and_refuses_every_seek() {
    // H5: a server with no range support is still finite media - it plays -
    // but every seek attempt against it is refused rather than pretending to
    // land somewhere it never actually reached.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    // MINOR 9 (fix round 1): captured before the seek so the request count
    // can prove the gate rejected before ever reopening, not merely that a
    // doomed attempt eventually failed - see IMPORTANT 3's fix.
    let requests_before_seek = server.requests().len();
    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(2))),
        Admission::Accepted
    );
    let rejection = engine.await_event(|e| matches!(e, PlaybackEvent::SeekRejected { .. }));
    let PlaybackEvent::SeekRejected { reason, .. } = rejection else {
        unreachable!("await_event's predicate already matched SeekRejected")
    };
    // MINOR 9: an exact match, not `.contains("seek")` - a doomed real
    // attempt that failed and then also failed to restore renders as
    // "seek failed (...) and the decoder could not be restored (...)",
    // which also contains "seek" and would have passed on the pre-fix
    // behaviour this test exists to rule out.
    assert_eq!(
        reason, "this source cannot seek",
        "unexpected rejection reason: {reason:?}"
    );
    assert_eq!(
        server.requests().len(),
        requests_before_seek,
        "the seek opened a fetch on a source already known to be unsupported"
    );
    // Playback itself is undisturbed by the refused seek.
    assert_eq!(engine.state(), PlaybackState::Playing);

    // "Every seek", not just the first.
    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
        Admission::Accepted
    );
    let second_rejection = engine.await_event(|e| matches!(e, PlaybackEvent::SeekRejected { .. }));
    let PlaybackEvent::SeekRejected {
        reason: second_reason,
        ..
    } = second_rejection
    else {
        unreachable!("await_event's predicate already matched SeekRejected")
    };
    assert_eq!(second_reason, "this source cannot seek");
    assert_eq!(engine.state(), PlaybackState::Playing);

    engine.finish();
    server.shutdown();
}

#[test]
fn an_unsupported_stopped_seek_emits_no_seek_target_stored() {
    // H14: a target M2 would treat as durable must never be stored for a
    // source that has already demonstrated it cannot honour one - the
    // capability gate has to run before `SeekTargetStored`, not after.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(2))),
        Admission::Accepted
    );
    let rejection = engine.await_event(|e| matches!(e, PlaybackEvent::SeekRejected { .. }));
    assert!(matches!(rejection, PlaybackEvent::SeekRejected { .. }));
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::SeekTargetStored { .. })),
        0,
        "an unsupported source stored a seek target it can never honour"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_retired_by_a_stop_reports_cancelled_and_commits_no_target() {
    // §8: an accepted seek always receives an outcome. A stop that retires
    // one in flight must report it as `SeekCancelled`, never as a silent
    // drop and never as `SeekRejected` (which would misreport a
    // cancellation as a validation failure), and the position it preserves
    // must be exactly what playback had already reached.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    let before = engine.progress().position;
    let port = server.port();
    server.shutdown();

    // A second server on the same port - the URL, and so the `MediaId`,
    // never changes - but every request from here on stalls before it is
    // ever answered, which is what lets this test prove the seek's wait was
    // entered before it interrupts.
    let stalling = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").stall_headers());

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
        Admission::Accepted
    );
    assert!(
        wait_for_request(&stalling, Duration::from_secs(5)),
        "the seek's request never reached the server"
    );

    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);

    let cancelled = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCancelled { .. }));
    let PlaybackEvent::SeekCancelled { requested, .. } = cancelled else {
        unreachable!("await_event's predicate already matched SeekCancelled")
    };
    assert_eq!(requested, Duration::from_secs(3));
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::SeekCompleted { .. })),
        0,
        "a cancelled seek must never also report completion"
    );
    assert_eq!(
        engine.progress().position,
        before,
        "the cancelled seek moved the preserved position"
    );

    engine.finish();
    stalling.shutdown();
}

#[test]
fn pause_during_a_stalled_read_freezes_output_and_resume_continues() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the read never blocked"
    );

    // The read is pending inside the decoder. Pause must reach it without
    // returning a destructive error to the demuxer.
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Pause),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Paused);
    // The frames already handed to the device still play out after the
    // park, so let that settle before taking the reading that must not
    // move (matches `pause_holds_the_position_still_and_resume_continues_
    // from_it` in engine_contract.rs).
    engine.let_time_pass(Duration::from_millis(300));
    let frozen = engine.position();
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(
        engine.position(),
        frozen,
        "output kept running while paused"
    );

    assert!(server.release());
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    // `play_for`'s argument is an absolute target position, not a relative
    // step - `frozen` is already past 100ms from the play span before the
    // pause, so the target has to be stated relative to it or this call is
    // a no-op that proves nothing.
    engine.play_for(frozen + Duration::from_millis(200));
    assert!(
        engine.progress().position > frozen,
        "playback did not continue"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn quit_while_paused_wakes_every_source_wait() {
    // H10's other half: a shutdown must reach a read blocked inside the byte
    // channel, not only one waiting on the command queue or the tick.
    // `finish` blocks on the worker's real thread join with no timeout of
    // its own, so returning here at all is the proof that it woke.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the read never blocked"
    );

    assert_eq!(
        engine.handle().submit(PlaybackCommand::Pause),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Paused);

    engine.finish();
    server.shutdown();
}

#[test]
fn a_truncated_tail_cannot_become_end_of_track() {
    // H8/H9: a body that ends early must fail the attempt, never drain to a
    // clean `EndOfTrack` that would mark the episode complete on less audio
    // than was actually recorded.
    let server =
        TestServer::start(Script::from_fixture("sine-5s.flac").truncate_body_after(16 << 10));
    let mut engine = TestEngine::start_without_recovery();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.play_until_terminal(Duration::from_secs(10));

    assert_eq!(
        engine.state(),
        PlaybackState::Failed,
        "a truncated body was reported as {:?}",
        engine.state()
    );
    assert!(
        !engine.saw_end_of_track(),
        "EndOfTrack was emitted for a body that never fully arrived"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_capability_change_carries_the_current_session_rev() {
    // H14: the event that announces a capability change must carry the
    // session revision current at the moment it fires, not the one current
    // when the source was first opened. MINOR 9 (fix round 1): nothing
    // between load and an ordinary playing seek ever bumps the revision, so
    // the original version of this test compared against a value that
    // could not have been wrong - it passed whether or not the event
    // actually read the current revision. A stop in between is what makes
    // the two revisions genuinely different, so the comparison below can
    // fail for the reason it names.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    let rev_at_load = engine.progress().session_rev;

    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    let rev_after_stop = engine.progress().session_rev;
    assert_ne!(
        rev_after_stop, rev_at_load,
        "the stop must bump the session revision for this test to distinguish anything"
    );

    // A stopped seek on a source whose demuxer seek support is still
    // `Unknown` (the reopen this triggers has proven nothing yet) runs the
    // trial that promotes it - the `CapabilitiesChanged` this produces must
    // carry `rev_after_stop`, never the stale `rev_at_load`.
    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(2))),
        Admission::Accepted
    );
    let event = engine.await_event(|e| matches!(e, PlaybackEvent::CapabilitiesChanged { .. }));
    let PlaybackEvent::CapabilitiesChanged {
        session_rev: event_rev,
        capabilities,
    } = event
    else {
        unreachable!("await_event's predicate already matched CapabilitiesChanged")
    };
    assert_eq!(
        event_rev, rev_after_stop,
        "a capability change must carry the session's current revision"
    );
    assert_eq!(
        capabilities.seek,
        SeekSupport::Native,
        "a successful seek must promote Unknown to Native"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_after_a_stop_reopens_rather_than_reporting_nothing_is_loaded() {
    // #7: a stopped seek's `source.is_none()` guard must not reject a seek
    // on a source whose identity, descriptor and position the worker is
    // still holding - it has to reopen first.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
        Admission::Accepted
    );
    let event = engine.await_event(|e| {
        matches!(
            e,
            PlaybackEvent::SeekTargetStored { .. } | PlaybackEvent::SeekRejected { .. }
        )
    });
    match event {
        PlaybackEvent::SeekTargetStored { target, .. } => {
            assert_eq!(target, Duration::from_secs(3));
        }
        PlaybackEvent::SeekRejected { reason, .. } => {
            panic!("a seek after a stop was rejected as {reason:?} instead of reopening")
        }
        _ => unreachable!("await_event's predicate matched neither expected event"),
    }

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_after_a_retired_seek_reopens_and_lands() {
    // #7's other caller: after a stop retires a seek in flight - dropping
    // the decoder along with it (§8) - the very next seek must still reopen
    // and, once played, land where it asked, not fail on a source that is
    // very much loaded.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    let port = server.port();
    server.shutdown();

    let stalling = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").stall_headers());
    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
        Admission::Accepted
    );
    assert!(
        wait_for_request(&stalling, Duration::from_secs(5)),
        "the seek's request never reached the server"
    );
    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    let _ = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCancelled { .. }));
    stalling.shutdown();

    // A healthy server, same port, so the retired seek's fetch left nothing
    // behind that a healthy fetch could stumble over.
    let healed = TestServer::start_on(port, Script::from_fixture("sine-5s.flac"));

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(2))),
        Admission::Accepted
    );
    let stored = engine.await_event(|e| matches!(e, PlaybackEvent::SeekTargetStored { .. }));
    let PlaybackEvent::SeekTargetStored { target, .. } = stored else {
        unreachable!("await_event's predicate already matched SeekTargetStored")
    };
    assert_eq!(target, Duration::from_secs(2));

    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    let landed = engine.await_seek_completed(Duration::from_secs(10));
    assert!(
        landed >= Duration::from_secs(1),
        "the reopened seek landed short at {landed:?}"
    );

    engine.finish();
    healed.shutdown();
}

#[test]
fn corrupt_audio_over_a_complete_body_fails_and_never_ends() {
    // H8: "malformed audio cannot become successful completion". The
    // transfer is perfect - full Content-Length, clean EOF, valid ETag -
    // and the bytes are garbage. Keying the failure on a `RemoteFailure`
    // alone lets this drain to `EndOfTrack` and mark the episode complete,
    // destroying the checkpoint.
    let mut body = match std::fs::read(fixture_path("sine-5s.flac")) {
        Ok(body) => body,
        Err(error) => panic!("the fixture must exist: {error}"),
    };
    // Corrupt from the middle to the end, leaving the header intact so it
    // still opens. A small corrupted window is not reliable here: FLAC's
    // frame sync search treats a handful of garbage bytes as noise to skip
    // past rather than a fault, and decoding drains cleanly through it to
    // `EndOfTrack` - exactly the outcome H8 forbids. Corrupting through to
    // the end removes every remaining valid frame sync, which is what
    // actually forces a decode error rather than a lucky resync.
    let middle = body.len() / 2;
    for byte in &mut body[middle..] {
        *byte = 0xFF;
    }
    let server = TestServer::start(Script::serving(body));

    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.play_until_terminal(Duration::from_secs(10));

    assert_eq!(
        engine.state(),
        PlaybackState::Failed,
        "corrupt audio was reported as {:?}",
        engine.state()
    );
    assert!(
        !engine.saw_end_of_track(),
        "EndOfTrack was emitted for a recording that never decoded through"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn play_after_a_remote_failure_reopens_once_at_the_preserved_position() {
    // §9's last transition. The first attempt dies mid-body; the position
    // the listener actually reached survives, and one explicit Play - not
    // an automatic retry - brings it back there.
    let server =
        TestServer::start(Script::from_fixture("sine-5s.flac").truncate_body_after(16 << 10));
    let mut engine = TestEngine::start_without_recovery();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(40));
    // Drive the clock further, tolerating a mid-way failure: `play_for`
    // assumes success and panics if the position stalls, but the ring the
    // priming read already staged means nothing further is even attempted
    // from the network until draining opens room again - `let_time_pass`
    // does not care whether the engine is still progressing, so it is what
    // gives the worker's own pump loop the chance to reach the truncation.
    engine.let_time_pass(Duration::from_secs(2));
    engine.await_state(PlaybackState::Failed);
    let heard = engine.progress().position;
    assert!(
        heard > Duration::ZERO,
        "the failure was reported before any audio was heard"
    );
    let port = server.port();
    server.shutdown();

    // A healthy server for the retry, at the same URL.
    let healed = TestServer::start_on(port, Script::from_fixture("sine-5s.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    assert!(
        engine.progress().position >= heard,
        "the reopen restarted from zero: {:?} < {heard:?}",
        engine.progress().position
    );
    engine.finish();
    healed.shutdown();
}

#[test]
fn play_after_a_remote_failure_on_an_unseekable_source_fails_honestly() {
    // "fail honestly if restoration is unavailable" - never a silent
    // restart from zero, which is the reset the invariant forbids.
    let server = TestServer::start(
        Script::from_fixture("sine-5s.flac")
            .without_ranges()
            .truncate_body_after(16 << 10),
    );
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(40));
    // See the sibling test's comment: `let_time_pass` drives the clock
    // without assuming the engine keeps progressing, which is what gives
    // the worker's pump loop room to reach the truncation.
    engine.let_time_pass(Duration::from_secs(2));
    engine.await_state(PlaybackState::Failed);
    let heard = engine.progress().position;
    assert!(
        heard > Duration::ZERO,
        "the failure was reported before any audio was heard"
    );

    // Fix round 1, IMPORTANT 3: the `Unsupported` gate has to run before any
    // reopen, not after one it then abandons live. Captured here, right
    // before the retry, so a fetch the fix's own gate should never have
    // opened is caught even though the very first `Load` already made one
    // request of its own.
    let requests_before_retry = server.requests().len();
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(
        engine.progress().position,
        heard,
        "an unseekable retry moved the position"
    );
    assert_eq!(
        server.requests().len(),
        requests_before_retry,
        "the retry opened a fetch on a source already known to be unseekable, \
         instead of rejecting before ever reopening it"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_second_explicit_play_after_a_still_broken_server_fails_again_rather_than_silently() {
    // Fix round 1, IMPORTANT 4: `fail_with`'s own "a repeating fatal fault
    // must not re-announce itself" guard returns early once `self.state ==
    // Failed`. A listener-driven retry that also fails is not a repeating
    // fault - it is one explicit action - so landing back in `Failed`
    // without ever leaving it must not swallow that action into silence.
    //
    // Both attempts drive the clock with `play_until_terminal` rather than
    // a fixed `let_time_pass`. A truncation only surfaces once the decoder
    // has consumed every byte buffered ahead of it - the byte channel
    // drains what arrived before it reports the ending - and the decoder
    // only reads once the 300 ms ring has room. A fixed advance on a
    // loaded machine (CI on main, 2026-09-17: 762 underruns in the 2 s the
    // clock ran) leaves the ring full and the clock frozen with the
    // truncation still unread, and the `Failed` this test waits for never
    // comes. `play_until_terminal` keeps the clock running until it does.
    let server =
        TestServer::start(Script::from_fixture("sine-5s.flac").truncate_body_after(16 << 10));
    let mut engine = TestEngine::start_without_recovery();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(40));
    engine.play_until_terminal(Duration::from_secs(10));
    assert_eq!(
        engine.state(),
        PlaybackState::Failed,
        "the first attempt against the truncated body did not fail"
    );
    let heard = engine.progress().position;
    // Drain the first failure out of the inbox: otherwise `await_event`
    // below would match it immediately and never actually observe whether
    // the retry produced anything of its own.
    while engine.try_event().is_some() {}

    // Same server, same URL, still truncated at the same relative point -
    // the retry must discover that failure for itself.
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_until_terminal(Duration::from_secs(10));
    let failed = engine.await_event(|e| matches!(e, PlaybackEvent::Failed { .. }));
    let PlaybackEvent::Failed { message, .. } = failed else {
        unreachable!("await_event's predicate already matched Failed")
    };
    assert!(
        !message.is_empty(),
        "the retry's failure carried no message"
    );
    assert_eq!(engine.state(), PlaybackState::Failed);
    // The retry resumes at `heard` and may legitimately play whatever the
    // re-fetched body delivered before the truncation - how much is heard
    // before the failure lands is a matter of scheduling, not of contract
    // (the macOS leg heard about half a second of it). What the failure
    // must never do is lose ground: reset to zero, or land short of what
    // the first attempt already reported.
    let after_retry = engine.progress().position;
    assert!(
        after_retry >= heard,
        "the second failure moved the position back from {heard:?} to {after_retry:?}"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_taken_while_paused_completes_rather_than_hanging() {
    // Reopening and seek refinement both need bytes, and both are legitimate
    // while playback is paused. A freeze that gated delivery would hang here
    // forever with no error to report.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Pause),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Paused);
    // Clear the inbox so only events from here on are visible below - the
    // legitimate `Playing` from the play span before this pause would
    // otherwise contaminate the count.
    while engine.try_event().is_some() {}

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
        Admission::Accepted
    );
    let landed = engine.await_seek_completed(Duration::from_secs(10));
    assert!(
        landed >= Duration::from_secs(3),
        "landed short at {landed:?}"
    );
    // Fix round 1, IMPORTANT 1: a seek taken while paused must not audibly
    // resume the device for the duration of its network wait. A stray
    // `StateChanged{Playing}` here is exactly what a freeze gate on fetch
    // *delivery* produced - the wait hook, invoked from inside the blocked
    // read, released the parked device once the (globally thawed) fetch
    // delivered bytes, then re-froze and re-announced `Paused` on return.
    // Checking only the final state (as this test originally did) misses
    // that entirely, since the last event still reads `Paused`.
    assert_eq!(
        engine.count_events(|e| matches!(
            e,
            PlaybackEvent::StateChanged {
                state: PlaybackState::Playing,
                ..
            }
        )),
        0,
        "a seek taken while paused emitted a StateChanged{{Playing}} it never asked for"
    );
    // And it is still paused: a seek does not resume playback.
    assert_eq!(engine.state(), PlaybackState::Paused);

    engine.finish();
    server.shutdown();
}

#[test]
fn a_stop_during_a_play_after_stop_reopen_leaves_the_session_stopped_not_failed() {
    // Final review, IMPORTANT 1: `ensure_source_open` manufactures
    // `PlaybackError::Cancelled` specifically so a stop landing during a
    // reopen can be told apart from a genuine failure - but `restore`, the
    // one caller a play-after-stop reopen actually goes through, used to
    // report it as `Failed` regardless. `Failed` is one of the three states
    // `do_stop` refuses to act on, so the very stop that caused the
    // cancellation was then swallowed on the next loop pass: the session
    // sat in `Failed`, never `Stopped`, and in the CLI a `Failed` breaks the
    // key loop and exits nonzero - pressing "stop" quit the player with an
    // error.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    // The first stop, ordinary and uncontested - the reopen this test is
    // actually about is the *next* one, triggered by the `Play` below.
    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);

    let port = server.port();
    server.shutdown();
    // Same URL, same `MediaId`, but every request from here on stalls before
    // it is ever answered - the proof this test needs that the reopen's
    // header wait was actually entered before the second stop interrupts it.
    let stalling = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").stall_headers());

    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    assert!(
        wait_for_request(&stalling, Duration::from_secs(5)),
        "the reopen's request never reached the server"
    );

    engine.handle().submit(PlaybackCommand::Stop);

    // Not `await_state`: `restore()` never left `Stopped` in the first place
    // while blocked in the reopen (it only sets `Loading` on the *other*
    // caller, `play`'s `Failed` arm), so this second stop's `do_stop` is a
    // correct no-op that emits no fresh `StateChanged` for `await_state`'s
    // history-consuming wait to find - polling the *current* state directly
    // is what this settle actually needs, with a hard failure the moment it
    // sees the bug this test exists to catch.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = engine.state();
        assert_ne!(
            state,
            PlaybackState::Failed,
            "a stop cancelling a reopen was reported as Failed, not left as Stopped"
        );
        if state == PlaybackState::Stopped {
            break;
        }
        if Instant::now() >= deadline {
            panic!("the engine never settled back to Stopped; last state {state:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        engine.count_events(|e| matches!(
            e,
            PlaybackEvent::Failed { .. }
                | PlaybackEvent::StateChanged {
                    state: PlaybackState::Failed,
                    ..
                }
        )),
        0,
        "a stop cancelling a reopen must never be reported as a failure"
    );

    engine.finish();
    stalling.shutdown();
}

#[test]
fn a_seek_cancelled_while_reopening_from_stopped_reports_cancelled_not_rejected() {
    // Final review, IMPORTANT 1, site 4: `seek_to`'s own call to
    // `ensure_source_open` used to flatten a cancellation into
    // `SeekRejected(format!("{error}"))` - misreporting a stop as a
    // validation failure, exactly what §8 says a `SeekRejected` must never
    // do. A seek issued while stopped is the one path that actually reaches
    // `ensure_source_open` from `seek_to`, since a live session never has a
    // decoder to reopen.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);

    let port = server.port();
    server.shutdown();
    let stalling = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").stall_headers());

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
        Admission::Accepted
    );
    assert!(
        wait_for_request(&stalling, Duration::from_secs(5)),
        "the stopped seek's reopen never reached the server"
    );

    engine.handle().submit(PlaybackCommand::Stop);

    let cancelled = engine.await_event(|e| {
        matches!(
            e,
            PlaybackEvent::SeekCancelled { .. } | PlaybackEvent::SeekRejected { .. }
        )
    });
    let PlaybackEvent::SeekCancelled { requested, .. } = cancelled else {
        panic!(
            "a seek cancelled while reopening from stopped must report SeekCancelled, not {cancelled:?}"
        );
    };
    assert_eq!(requested, Duration::from_secs(3));
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::SeekTargetStored { .. })),
        0,
        "a cancelled reopen must never also store the seek target it never validated"
    );

    engine.finish();
    stalling.shutdown();
}

// Root cause, confirmed by reading `symphonia-bundle-mp3` 0.6.1's own source
// (`~/.cargo/registry/.../symphonia-bundle-mp3-0.6.1/src/demuxer.rs`) after
// an earlier pass at this investigation guessed wrong about the mechanism:
// this engine pins every seek to `SeekMode::Accurate` (`src/playback/decode.rs:290`; see
// `docs/m1-known-debt.md`'s M3 capability-evidence entry). `preseek_accurate`
// only rewinds to `first_packet_pos` when `required_ts < self.next_packet_ts`
// - it is conditional, not unconditional, and it does not consult whether an
// Xing/VBRI index exists at all (the `Accurate` branch never reads
// `num_frames`; only `Coarse` does). The trigger is that `next_packet_ts` is
// the demuxer's own read-ahead position, which runs ahead of audible
// playback by the PCM ring plus `MediaSourceStream`'s own read-ahead - so a
// seek that is forward in audible terms can still be backward relative to
// `next_packet_ts`, and that comparison is what fires the rewind. Once fired,
// the forward rescan parses frame headers and skips frame bodies without
// decoding them - cheap on a local disk, expensive over HTTP, because the
// bytes still have to come across the byte channel in order. This test's
// deliberately-tiny fixture and short forward step exist to make that rewind
// trigger reliably and cheaply, not because a small step is somehow special;
// on a real file the same comparison can just as easily fire tens of seconds
// in, which is what the field report below hit.
//
// This is a live defect, not a testing limitation: the whole
// rewind-and-rescan executes inside one uncancellable, unbounded call to
// `FormatReader::seek`. `SEEK_BUDGET` (`engine.rs:116`, 5s) does not help -
// it only bounds `seek_refined`'s own residual-alignment loop *after*
// `reader.seek()` returns, so it never applies to the scan itself. While the
// worker thread is parked inside that call it cannot dispatch queued
// commands, which is why pause/resume appear dead and position captures go
// `Degraded` during a long rescan.
//
// Manual-acceptance bug report this reproduces: a user seeking forward
// (right-arrow, +10s) twice in quick succession on a real 2h15m podcast MP3
// over a real CDN saw playback "stuck" (two range requests at the same byte,
// 37849 - `first_packet_pos`, past a large ID3v2 cover-art tag - 160ms
// apart, then ten seconds of silence before giving up), and pause/play
// afterward never reached `Playing` again; `Stop` does recover it, because
// it retires out-of-band and interrupts the scan, but Pause/Play do not.
//
// `sine-noxing.mp3` is 5.04s so the test runs in well under a second of real
// time; `trickle` stands in for the CDN's finite throughput, slow enough
// that redelivering nearly the whole file is measurable and landing
// near-instantly (what an efficient short forward seek should cost) is not
// confused with it.
//
// Fixed by M3.1 Task 4: `seek_refined` (`src/playback/decode.rs`) now seeks
// `SeekMode::Coarse` rather than `Accurate`, which computes a byte offset
// directly from the track's own duration arithmetic instead of asking the
// demuxer to scan - see `docs/superpowers/specs/
// 2026-09-10-tenuto-estimated-seek-design.md` §5.2 for the measurements.
// This is now the regression test for that fix. It was committed
// `#[ignore]`d as a failing reproduction and un-ignored when Task 4 made it
// pass; the assertions below are unchanged from that failing version, which
// is what makes them evidence rather than a description of current
// behaviour.
#[test]
fn a_short_forward_seek_on_a_no_index_mp3_lands_quickly_without_rescanning() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3").trickle(2048, Duration::from_millis(80)),
    );
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.mp3"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    // Close to the end of this 5.04s fixture, so a short forward step from
    // here is unambiguously "a little further", not "still near the start".
    engine.play_for(Duration::from_millis(3_600));
    let before = engine.progress().position;
    assert!(
        before >= Duration::from_millis(3_000),
        "playback did not reach near the fixture's end before the seek: {before:?}"
    );

    let requests_before_seek = server.requests().len();
    let target = before + Duration::from_millis(300);
    assert_eq!(
        engine.handle().submit(PlaybackCommand::SeekTo(target)),
        Admission::Accepted
    );

    // Prove the seek's own request actually reached the server (the
    // project's own rule for any wait a test is about to reason about),
    // before reading anything from `server.requests()` about it.
    let request_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if server.requests().len() > requests_before_seek {
            break;
        }
        assert!(
            Instant::now() < request_deadline,
            "the seek's own request never reached the server"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    let seek_byte = server
        .requests()
        .into_iter()
        .skip(requests_before_seek)
        .find_map(|request| request.range())
        .map(|(first, _)| first);
    let Some(seek_byte) = seek_byte else {
        panic!("the seek issued no ranged request at all");
    };

    // `sine-noxing.mp3` carries no ID3v2 tag (see `tests/fixtures/README.
    // md`), so its `first_packet_pos` sits at, or a few bytes past, byte 0.
    // The correct behaviour this asserts: a seek this close to where
    // playback already sits should request a byte in that same
    // neighbourhood (proportionally, ~3.9s of 5.04s in a 40_377-byte file
    // is ~byte 31_000), not one back at the very start of the file. This
    // failed before Task 4 - the request landed within a few bytes of 0 -
    // which was the concrete, byte-level proof that every seek rescanned
    // from the top rather than continuing from where playback had reached.
    let proportional_estimate =
        (target.as_secs_f64() / Duration::from_millis(5_041).as_secs_f64() * 40_377.0) as u64;
    assert!(
        seek_byte + 8192 >= proportional_estimate,
        "a seek {:?} past {before:?} requested byte {seek_byte}, near the \
         file's very first byte, instead of somewhere near byte {proportional_estimate} \
         (the current position's own neighbourhood) - the demuxer is \
         rescanning from the top rather than continuing from where \
         playback already reached",
        target.saturating_sub(before)
    );

    // The wedge itself, asserted as the correct behaviour it violated before
    // the fix: a step of only 300ms has no honest reason to take anywhere near
    // as long as rescanning from the top does.
    //
    // This deadline was 250ms, which is not what the original comment called
    // it. Measured on this fixture and trickle, an efficient short step is not
    // near-instant: it lands in 206-212ms on a 28-core machine and 214-240ms
    // with the suite pinned to two cores, because the cost is bound by the
    // trickle's 80ms cadence rather than by CPU. That left 10-40ms of headroom,
    // which a shared CI runner's sleep overshoot spends — GitHub Actions failed
    // here with the seek already at the correct byte, i.e. with no wedge at all.
    //
    // The broken behaviour is not in the same neighbourhood. With `seek_refined`
    // put back to `SeekMode::Accurate` (the pre-Task-4 code) the seek requests
    // byte 44 and does not land inside an 8-second window. Two seconds is ~8x
    // the slowest measured efficient step and at least 4x short of the wedge, so
    // it still separates the two by a wide margin. That was re-verified against
    // the reintroduced bug before this change was committed: the assertion
    // still fails there, so it remains evidence rather than a description of
    // whatever the current code happens to do.
    let landing_deadline = Instant::now() + Duration::from_secs(2);
    let landed = loop {
        match engine.try_event() {
            Some(event @ PlaybackEvent::SeekCompleted { .. }) => break Some(event),
            Some(_) => continue,
            None if Instant::now() >= landing_deadline => break None,
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    // The diagnosis is derived from the byte actually requested rather than
    // written in. The message this replaced asserted a rescan from whatever
    // byte it was handed, so a CI run that had already requested the correct
    // byte still reported itself as the wedge.
    let diagnosis = if seek_byte + 8192 >= proportional_estimate {
        "The request targeted the right neighbourhood, so this is not the \
         rescan-from-the-top wedge: landing is being delayed after the request"
    } else {
        "The request targeted the file's first frame: this is the \
         rescan-from-the-top wedge"
    };
    assert!(
        landed.is_some(),
        "a 300ms forward seek from byte {seek_byte} (near byte \
         {proportional_estimate} expected) did not land within 2s. {diagnosis}"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_submitters_late_retirement_does_not_cancel_the_seek_it_enqueued() {
    // CI on main, 2026-09-17: `a_capability_change_carries_the_current_
    // session_rev` and two `estimated_seek.rs` tests failed on the shared
    // runner with a `SeekCancelled` nothing had asked for. `submit_seek`
    // enqueues `SeekTo` and then retires the source interrupt so a worker
    // blocked in a remote read wakes to service it. Preempted for a
    // millisecond between the two, the submitter's retirement landed after
    // the worker had already dispatched the seek and `begin()`-ed its own
    // fetch generation for it - killing the reopen or trial in flight and
    // misreporting the seek as cancelled.
    //
    // The two halves of `submit_seek` are replayed here by hand with the
    // preemption made total: the queue half goes first, the worker is given
    // until it has opened a generation of its own, and only then does the
    // retirement half land. Aimed at the generation that was live at the
    // send, it must find nothing to retire.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);

    let interrupt = engine.handle().source_interrupt();
    let observed = interrupt.generation();
    engine.send(PlaybackCommand::SeekTo(Duration::from_secs(2)));
    let deadline = Instant::now() + Duration::from_secs(5);
    while interrupt.generation() == observed {
        assert!(
            Instant::now() < deadline,
            "the worker never opened a generation of its own for the seek"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !interrupt.retire_generation(observed),
        "a retirement aimed at the pre-seek generation must not touch the seek's own"
    );

    let event = engine.await_event(|e| {
        matches!(
            e,
            PlaybackEvent::SeekTargetStored { .. } | PlaybackEvent::SeekCancelled { .. }
        )
    });
    assert!(
        matches!(event, PlaybackEvent::SeekTargetStored { .. }),
        "the seek was cancelled by its own submitter's retirement: {event:?}"
    );

    engine.finish();
    server.shutdown();
}
