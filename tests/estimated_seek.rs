//! Acceptance evidence for M3.1 Task 4: the bounded `Coarse` seek that fixes
//! the wedge in `tests/engine_remote.rs::
//! a_short_forward_seek_on_a_no_index_mp3_rescans_the_whole_file_instead_of_landing_quickly`.
//!
//! Every test here binds `127.0.0.1`, against `TestServer` and `TestOutput`,
//! exactly like `engine_remote.rs`. See
//! `docs/superpowers/specs/2026-09-10-tenuto-estimated-seek-design.md`
//! §5 for the mechanism and the measurements these tests pin.

mod support;

use std::time::{Duration, Instant};

use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::media::id::{MediaId, NormalizedUrl};
use tenuto::media::source::SourceLocation;
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use url::Url;

use support::server::{Script, TestServer};
use support::{TestEngine, fixture_path};

/// Blocks until `server` has recorded more than `baseline` requests, or
/// `patience` elapses. The proof that a request was actually issued (and, for
/// a stalling script, that the read blocked genuinely inside the server's
/// response rather than never reaching it at all) — the same technique
/// `engine_remote.rs::wait_for_request` uses, generalised to a baseline count
/// so it can be called more than once per test.
fn wait_for_new_request(server: &TestServer, baseline: usize, patience: Duration) -> bool {
    let deadline = Instant::now() + patience;
    loop {
        if server.requests().len() > baseline {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Waits for `predicate` to match a received event, or `patience` to elapse.
/// Unlike `TestEngine::await_event`, `patience` is the caller's own tight
/// bound rather than the crate's generic 20s `PATIENCE`: several tests here
/// need to prove a seek failed *promptly*, not merely that it eventually
/// did, so a hang has to produce a failure within a few seconds rather than
/// only at the crate-wide ceiling.
fn await_event_within(
    engine: &mut TestEngine,
    patience: Duration,
    predicate: impl Fn(&PlaybackEvent) -> bool,
) -> Option<PlaybackEvent> {
    let deadline = Instant::now() + patience;
    loop {
        if let Some(event) = engine.try_event() {
            if predicate(&event) {
                return Some(event);
            }
            continue;
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The byte offset `Coarse`'s own arithmetic should land near: `target /
/// total * file_len`. Independent of anything the engine reports — it is
/// computed straight from the fixture's own file size, the same ratio
/// `preseek_coarse` uses internally (§5.2).
fn proportional_byte(target: Duration, total: Duration, file_len: u64) -> u64 {
    (target.as_secs_f64() / total.as_secs_f64() * file_len as f64) as u64
}

const NOXING: &str = "sine-long-noxing.mp3"; // 600s, CBR, no Xing/Info/VBRI.
const NOXING_DURATION: Duration = Duration::from_secs(600);

const VBR_NOXING: &str = "sine-long-vbr-noxing.mp3"; // 600s true, ~361s estimated.

fn noxing_len() -> u64 {
    match std::fs::metadata(fixture_path(NOXING)) {
        Ok(metadata) => metadata.len(),
        Err(error) => panic!("the fixture must exist: {error}"),
    }
}

#[test]
fn a_forward_seek_on_a_no_index_mp3_lands_without_rescanning_from_the_first_packet() {
    // §5.2: `Coarse`'s own request always starts at its arithmetic estimate,
    // never at `first_packet_pos`, and its resync-and-walk costs a handful
    // of KB rather than the whole 0..target prefix a rescan would read.
    // Trickled slowly enough that reading that whole prefix (~960 KB for a
    // 60s target) is measurably slow, so landing within the tight deadline
    // below is itself proof the rescan never happened — the same technique
    // `engine_remote.rs`'s own reproduction of the wedge uses.
    let file_len = noxing_len();
    let server =
        TestServer::start(Script::from_fixture(NOXING).trickle(4096, Duration::from_millis(5)));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.mp3"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(300));

    let requests_before = server.requests().len();
    let bytes_before = server.bytes_written();
    let target = Duration::from_secs(60);
    assert_eq!(
        engine.handle().submit(PlaybackCommand::SeekTo(target)),
        Admission::Accepted
    );
    assert!(
        wait_for_new_request(&server, requests_before, Duration::from_secs(5)),
        "the seek's own request never reached the server"
    );

    let seek_byte = server
        .requests()
        .into_iter()
        .skip(requests_before)
        .find_map(|request| request.range())
        .map(|(first, _)| first);
    let Some(seek_byte) = seek_byte else {
        panic!("the seek issued no ranged request at all");
    };
    let estimate = proportional_byte(target, NOXING_DURATION, file_len);
    assert!(
        seek_byte.abs_diff(estimate) < 64 * 1024,
        "seek requested byte {seek_byte}, expected near the arithmetic estimate {estimate}"
    );
    assert!(
        seek_byte > file_len / 20,
        "seek requested byte {seek_byte} landed near first_packet_pos (byte 0) rather than \
         the estimate {estimate} — the demuxer is rescanning from the top"
    );

    let landed = await_event_within(&mut engine, Duration::from_millis(500), |event| {
        matches!(event, PlaybackEvent::SeekCompleted { .. })
    });
    assert!(
        landed.is_some(),
        "the seek did not land within 500ms at this trickle rate; a rescan of the 0..60s prefix \
         (~{estimate} B) would still be under way"
    );

    // §5.3: the cost comparison in bytes, not only in wall clock. A rescan
    // of the 0..60s prefix would write on the order of `estimate` bytes;
    // `Coarse`'s own walk costs a few KB (3,072 B measured in the spike).
    // A generous quarter of the prefix is still decisive against the two
    // orders of magnitude a rescan would cost, without being sensitive to
    // `MediaSourceStream`'s read-ahead overshoot.
    let written = server.bytes_written().saturating_sub(bytes_before);
    assert!(
        (written as u64) < estimate / 4,
        "the server wrote {written} bytes servicing the seek; a rescan of the 0..60s prefix \
         (~{estimate} B) would write far more"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn repeated_seeks_stay_responsive() {
    // The user's actual report: pressing the seek key two or three times in
    // quick succession wedged playback entirely. Five successive forward
    // seeks, each landing before the next is issued, is that scenario
    // directly — trickled so that a rescan (each landing needing more than
    // the last, `O(target - first_packet_pos)`) would grow visibly slower
    // with every step, while `Coarse`'s own cost does not.
    let server =
        TestServer::start(Script::from_fixture(NOXING).trickle(4096, Duration::from_millis(5)));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.mp3"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));

    let mut previous = Duration::ZERO;
    for step in 1..=5u64 {
        let target = Duration::from_secs(step * 30);
        assert_eq!(
            engine.handle().submit(PlaybackCommand::SeekTo(target)),
            Admission::Accepted
        );
        let landed = await_event_within(&mut engine, Duration::from_millis(500), |event| {
            matches!(event, PlaybackEvent::SeekCompleted { .. })
        });
        let Some(PlaybackEvent::SeekCompleted { actual, .. }) = landed else {
            panic!(
                "seek {step} to {target:?} did not land within 500ms — a rescan from the first \
                 packet, not a landing at the estimate, is still under way"
            );
        };
        assert!(
            actual > previous,
            "seek {step} to {target:?} did not advance past the previous landing {previous:?}"
        );
        previous = actual;
    }
    assert_eq!(
        engine.state(),
        PlaybackState::Playing,
        "playback wedged after repeated seeks"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_against_a_stalled_source_fails_within_its_deadline() {
    // §5.3/§5.4: the seek's own operation deadline bounds the underlying
    // reader I/O even though `Coarse`'s own `FormatReader::seek()` call is
    // not eliminated by the mode swap — a server that goes silent mid-seek
    // must not hang the worker. `stall_body_after(256)` is well under
    // `Coarse`'s own measured cost (a few KB), so the read genuinely blocks
    // rather than completing from what little the server delivers first.
    //
    // Issued while stopped, against a source whose seek support is still
    // unproven: `verify_seek_support`'s own trial seek is one isolated
    // `seek_bounded` call with no recovery attempt of its own (there is no
    // preserved position to restore for a validation that never moved
    // anything), so this exercises the deadline directly without a second,
    // confounding attempt.
    let server = TestServer::start(Script::from_fixture(NOXING));
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_limits(&server.url("/audio.mp3"), Limits::brisk());
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    let before = engine.progress().position;
    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    let port = server.port();
    server.shutdown();

    let stalling = TestServer::start_on(port, Script::from_fixture(NOXING).stall_body_after(256));

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(300))),
        Admission::Accepted
    );
    assert!(
        stalling.wait_until_stalled(Duration::from_secs(5)),
        "the seek's own read never blocked"
    );

    let event = await_event_within(&mut engine, Duration::from_secs(2), |event| {
        matches!(event, PlaybackEvent::SeekRejected { .. })
    });
    assert!(
        event.is_some(),
        "the seek against a stalled source did not fail within its deadline"
    );
    assert_eq!(
        engine.progress().position,
        before,
        "the pre-seek position was not restored after the failed seek"
    );
    assert_eq!(
        engine.state(),
        PlaybackState::Stopped,
        "a rejected stopped seek must leave the session stopped, not failed"
    );

    engine.finish();
    stalling.shutdown();
}

#[test]
fn pause_stop_and_quit_are_serviced_during_a_seek() {
    // The wedge this task fixes made pause/resume "appear dead" while a
    // rescan ran, because the worker dispatched no commands at all while
    // parked inside `FormatReader::seek()`. A seek against a source slow
    // enough to still be in flight when pause and stop arrive proves
    // commands are serviced *during* it, not only after — and `finish`
    // below (a real, timeout-free thread join, per its own doc comment)
    // is the proof for quit.
    let server =
        TestServer::start(Script::from_fixture(NOXING).trickle(512, Duration::from_millis(50)));
    let mut engine = TestEngine::start_idle();
    // The trickle is here to keep the *seek* in flight, but it slows the
    // open just as much: symphonia's frame-count estimate for a no-index
    // MP3 reads up to 16 KiB sequentially, which at 512 bytes per 50 ms is
    // 1.6 s of `Limits::brisk`'s 2 s opening budget, and the estimator
    // swallows a read that runs out of budget and returns no count. The
    // macOS leg (2026-09-17, diagnosed on the runner) spent 0.8 s on the
    // first open alone, the deadline expired inside the estimator, the
    // track loaded with `duration: None`, and the seek to 300 s then failed
    // in the demuxer without ever issuing a request - not the failure this
    // test is about. The opening budget is lifted so the open is never the
    // thing under test here; the stall budget stays brisk.
    engine.load_remote_with_limits(
        &server.url("/audio.mp3"),
        Limits {
            open: Duration::from_secs(30),
            ..Limits::brisk()
        },
    );
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));

    let requests_before = server.requests().len();
    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(300))),
        Admission::Accepted
    );
    assert!(
        wait_for_new_request(&server, requests_before, Duration::from_secs(5)),
        "the seek's own request never reached the server"
    );

    let pause_deadline = Instant::now() + Duration::from_millis(750);
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Pause),
        Admission::Accepted
    );
    loop {
        if engine.state() == PlaybackState::Paused {
            break;
        }
        assert!(
            Instant::now() < pause_deadline,
            "pause was not serviced promptly during the seek — the worker is still parked \
             inside it"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    let stop_deadline = Instant::now() + Duration::from_millis(750);
    engine.handle().submit(PlaybackCommand::Stop);
    loop {
        if engine.state() == PlaybackState::Stopped {
            break;
        }
        assert!(
            Instant::now() < stop_deadline,
            "stop was not serviced promptly after the seek was paused mid-flight"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    engine.finish();
    server.shutdown();
}

#[test]
fn an_estimated_landing_reports_estimated_provenance() {
    // §3.1: an MP3 `Coarse` landing is `Estimated` (fix round 1: this is
    // MP3-specific, not every `Coarse` landing regardless of format - see
    // `a_remote_mp3_seek_reports_estimated_and_a_local_flac_seek_reports_
    // established` for the distinction pinned directly), and playing on
    // from it must keep reporting that — decoding forward never converts an
    // estimate into an establishment.
    let server = TestServer::start(Script::from_fixture(NOXING));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.mp3"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(120))),
        Admission::Accepted
    );
    let landed = engine.await_seek_completed(Duration::from_secs(10));
    assert_eq!(
        landed.provenance,
        PositionProvenance::Estimated,
        "a Coarse landing must report Estimated"
    );
    assert_eq!(engine.progress().provenance, PositionProvenance::Estimated);

    engine.play_for(landed.actual + Duration::from_millis(300));
    assert_eq!(
        engine.progress().provenance,
        PositionProvenance::Estimated,
        "provenance stopped reporting Estimated after ordinary playback from an estimated landing"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_remote_mp3_seek_reports_estimated_and_a_local_flac_seek_reports_established() {
    // Fix round 1's real defect: `seek_bounded` originally marked *every*
    // `Coarse` landing `Estimated`, but `Coarse` only changes behaviour for
    // MP3. Across this crate's whole dependency tree, MP3's `MpaReader` is
    // the only `FormatReader::seek` that reads its `mode` argument at all —
    // FLAC's own seek binary-searches on real per-frame sample numbers
    // carried in the frame headers, which is exactly why `Coarse` needs to
    // estimate for MP3 (no such numbers exist in an MP3 frame) and does not
    // for FLAC. Marking a FLAC landing `Estimated` would assert a loss of
    // precision that provably did not occur — and matters downstream: §4
    // forbids an estimated position from replacing an established
    // checkpoint, so that mistake would freeze a local FLAC's checkpoint at
    // its pre-seek value on every seek, a regression M1/M2 never had. Both
    // sides pinned in one test so the distinction cannot regress to
    // "unconditional" in either direction.
    let server = TestServer::start(Script::from_fixture("sine-5s.mp3"));
    let mut remote = TestEngine::start_idle();
    remote.load_remote(&server.url("/audio.mp3"));
    assert_eq!(
        remote.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    remote.await_state(PlaybackState::Playing);
    remote.play_for(Duration::from_millis(100));
    assert_eq!(
        remote
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_millis(3_000))),
        Admission::Accepted
    );
    let remote_landing = remote.await_seek_completed(Duration::from_secs(10));
    assert_eq!(
        remote_landing.provenance,
        PositionProvenance::Estimated,
        "a remote MP3 seek must report Estimated"
    );
    remote.finish();
    server.shutdown();

    let mut local = TestEngine::start("sine-5s.flac");
    local.play_for(Duration::from_millis(100));
    local.send(PlaybackCommand::SeekTo(Duration::from_secs(2)));
    let local_landing = local.await_seek_completed(Duration::from_secs(10));
    assert_eq!(
        local_landing.provenance,
        PositionProvenance::Established,
        "a local FLAC seek must still report Established — Coarse changes nothing for it"
    );
    local.finish();
}

#[test]
fn recovery_after_a_failed_seek_gets_a_fresh_deadline() {
    // §5.4: a fresh attempt gets its own bounded deadline, never an
    // inherited, already-expired one. Two seeks issued back to back while
    // stopped — each is `verify_seek_support`'s own isolated
    // `seek_bounded` call, with no recovery of its own — against the same
    // persistently-stalling server: if the second's deadline were computed
    // once and carried over rather than read fresh on every attempt, it
    // would fail near-instantly instead of taking close to the same
    // near-full deadline the first one did.
    let server = TestServer::start(Script::from_fixture(NOXING));
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_limits(&server.url("/audio.mp3"), Limits::brisk());
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

    let stalling = TestServer::start_on(port, Script::from_fixture(NOXING).stall_body_after(256));

    let first_start = Instant::now();
    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(200))),
        Admission::Accepted
    );
    let first = await_event_within(&mut engine, Duration::from_secs(2), |event| {
        matches!(event, PlaybackEvent::SeekRejected { .. })
    });
    assert!(
        first.is_some(),
        "the first seek did not fail within its deadline"
    );
    let first_elapsed = first_start.elapsed();
    assert!(
        first_elapsed >= Duration::from_millis(300),
        "the first seek failed after only {first_elapsed:?}, too fast to have used its own \
         deadline"
    );
    assert_eq!(
        engine.state(),
        PlaybackState::Stopped,
        "a rejected stopped seek must leave the session stopped, not failed"
    );

    let second_start = Instant::now();
    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(250))),
        Admission::Accepted
    );
    let second = await_event_within(&mut engine, Duration::from_secs(2), |event| {
        matches!(event, PlaybackEvent::SeekRejected { .. })
    });
    assert!(
        second.is_some(),
        "the recovery's second seek did not fail within its deadline"
    );
    let second_elapsed = second_start.elapsed();
    assert!(
        second_elapsed >= Duration::from_millis(300),
        "the recovery's second seek failed after only {second_elapsed:?} — an inherited, \
         already-expired deadline rather than a fresh one"
    );

    engine.finish();
    stalling.shutdown();
}

#[test]
fn a_seek_that_stalls_while_paused_still_fails_within_its_deadline() {
    // R1's regression. The ordinary stall budget is active-demand time,
    // suspended while frozen so a pause is never reported as a server
    // stall — which means a version that bounds the seek only through that
    // budget hangs forever on a seek issued while paused against a source
    // that then goes silent. Only a deadline checked inside `read`'s wait
    // loop regardless of freeze can end this; a version that bounds the
    // seek when playing and hangs when paused passes every other test in
    // this file.
    let server = TestServer::start(Script::from_fixture(NOXING));
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_limits(&server.url("/audio.mp3"), Limits::brisk());
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    assert_eq!(
        engine.handle().submit(PlaybackCommand::Pause),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Paused);
    let before = engine.progress().position;

    let port = server.port();
    server.shutdown();
    let stalling = TestServer::start_on(port, Script::from_fixture(NOXING).stall_body_after(256));

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(300))),
        Admission::Accepted
    );
    // Proves the read was actually entered — before asserting anything about
    // how long it then takes to fail.
    assert!(
        wait_for_new_request(&stalling, 0, Duration::from_secs(5)),
        "the paused seek's own request never reached the server"
    );

    // Issued while playing, this cascades through a recovery attempt that
    // also has to stall against the same server and fail on its own fresh
    // deadline (§5.4), so the total budget here is roughly twice
    // `Limits::brisk().stall` (~1s observed) rather than the ~500ms a
    // single attempt would take — 6s is generous over that, while still far
    // short of "forever", which is the only thing this test needs to rule
    // out.
    let event = await_event_within(&mut engine, Duration::from_secs(6), |event| {
        matches!(event, PlaybackEvent::Failed { .. })
    });
    assert!(
        event.is_some(),
        "a seek that stalled while paused did not fail within 6s — the stall budget's freeze \
         suspension swallowed the deadline and the read hung"
    );
    assert_eq!(
        engine.state(),
        PlaybackState::Failed,
        "the recovery also stalled against the same source and should have failed in turn"
    );
    assert_eq!(
        engine.progress().position,
        before,
        "the pre-seek position was not preserved through the failure"
    );

    engine.finish();
    stalling.shutdown();
}

#[test]
fn a_seek_on_an_indexed_remote_mp3_is_bounded_too() {
    // The deadline is R2's routing decision, applied to every remote seek —
    // not conditioned on whether the track carries a Xing/Info index.
    // `sine-5s.mp3` (unlike every `-noxing` fixture elsewhere in this file)
    // keeps ffmpeg's default Xing/LAME header, and the same bound applies to
    // it regardless.
    let server = TestServer::start(Script::from_fixture("sine-5s.mp3"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_limits(&server.url("/audio.mp3"), Limits::brisk());
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    let before = engine.progress().position;
    engine.handle().submit(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    let port = server.port();
    server.shutdown();

    let stalling = TestServer::start_on(
        port,
        Script::from_fixture("sine-5s.mp3").stall_body_after(64),
    );

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_millis(3_500))),
        Admission::Accepted
    );
    assert!(
        stalling.wait_until_stalled(Duration::from_secs(5)),
        "the seek's own read never blocked"
    );

    let event = await_event_within(&mut engine, Duration::from_secs(2), |event| {
        matches!(event, PlaybackEvent::SeekRejected { .. })
    });
    assert!(
        event.is_some(),
        "the seek against an indexed MP3 did not fail within its deadline"
    );
    assert_eq!(
        engine.progress().position,
        before,
        "the pre-seek position was not restored"
    );
    assert_eq!(
        engine.state(),
        PlaybackState::Stopped,
        "a rejected stopped seek must leave the session stopped, not failed"
    );

    engine.finish();
    stalling.shutdown();
}

#[test]
fn every_seek_entry_point_is_routed() {
    // R2: `load` (launch resume), `seek_to`'s stop→play resume path
    // (`restore`), device recovery and stopped-seek validation all reach
    // `seek_refined` only through the shared routing. Asserted on observable
    // behaviour — a landing near the arithmetic estimate rather than near
    // `first_packet_pos` — not on which function called which.
    let file_len = noxing_len();

    // (a) Launch resume: `load`'s own resume-at-a-stored-position seek.
    {
        let server = TestServer::start(Script::from_fixture(NOXING));
        let mut engine = TestEngine::start_idle();
        let target = Duration::from_secs(300);
        engine.load_remote_with_resume(&server.url("/audio.mp3"), ResumeIntent::StartAt(target));
        let seek_byte = server
            .requests()
            .into_iter()
            .rev()
            .find_map(|request| request.range())
            .map(|(first, _)| first);
        let Some(seek_byte) = seek_byte else {
            panic!("launch resume issued no ranged request at all");
        };
        assert!(
            seek_byte > file_len / 20,
            "launch resume requested byte {seek_byte}, near first_packet_pos rather than the \
             estimate for {target:?}"
        );
        engine.finish();
        server.shutdown();
    }

    // (b) Stop -> play: `restore`'s reseek at the preserved position.
    {
        let server = TestServer::start(Script::from_fixture(NOXING));
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&server.url("/audio.mp3"));
        assert_eq!(
            engine.handle().submit(PlaybackCommand::Play),
            Admission::Accepted
        );
        engine.await_state(PlaybackState::Playing);
        engine.play_for(Duration::from_millis(100));
        let target = Duration::from_secs(300);
        assert_eq!(
            engine.handle().submit(PlaybackCommand::SeekTo(target)),
            Admission::Accepted
        );
        engine.await_seek_completed(Duration::from_secs(10));

        engine.handle().submit(PlaybackCommand::Stop);
        engine.await_state(PlaybackState::Stopped);
        let requests_before_play = server.requests().len();
        assert_eq!(
            engine.handle().submit(PlaybackCommand::Play),
            Admission::Accepted
        );
        engine.await_state(PlaybackState::Playing);

        // The reopen `ensure_source_open` performs first requests byte 0,
        // same as any fresh open — that is not what this asserts. The
        // largest offset among the requests this `Play` issued is the
        // reseek re-establishing the preserved position.
        let seek_byte = server
            .requests()
            .into_iter()
            .skip(requests_before_play)
            .filter_map(|request| request.range())
            .map(|(first, _)| first)
            .max();
        let Some(seek_byte) = seek_byte else {
            panic!("stop->play issued no ranged request re-establishing the preserved position");
        };
        assert!(
            seek_byte > file_len / 20,
            "stop->play requested byte {seek_byte}, near first_packet_pos rather than the \
             estimate for the preserved {target:?}"
        );
        engine.finish();
        server.shutdown();
    }

    // (c) Device recovery: `rebuild`'s reseek at the position the callback
    // preserved, after ordinary playback has run far enough that the
    // decoder's own read-ahead (the PCM ring plus `MediaSourceStream`'s
    // buffering, §5.1 of the parent milestone's root cause) has carried it
    // measurably ahead of what has actually been heard — the same drift
    // that made the original bug reachable at all.
    {
        let server = TestServer::start(Script::from_fixture(NOXING));
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&server.url("/audio.mp3"));
        assert_eq!(
            engine.handle().submit(PlaybackCommand::Play),
            Admission::Accepted
        );
        engine.await_state(PlaybackState::Playing);
        let target = Duration::from_secs(300);
        assert_eq!(
            engine.handle().submit(PlaybackCommand::SeekTo(target)),
            Admission::Accepted
        );
        engine.await_seek_completed(Duration::from_secs(10));
        engine.play_for(target + Duration::from_millis(500));

        let requests_before_fault = server.requests().len();
        engine.force_device_loss();
        // Not `await_state(Playing)`: a rebuild that never actually leaves
        // `Playing` at the worker's own state-machine level (this one
        // doesn't - it recovers underneath the same logical state) emits no
        // second `StateChanged{Playing}` for `await_state` to find, and it
        // would wait out its own patience for an event that was never
        // coming. `DeviceRecovered` is what actually marks the recovery
        // done.
        engine.await_event(|event| matches!(event, PlaybackEvent::DeviceRecovered { .. }));
        assert_eq!(engine.state(), PlaybackState::Playing);

        let seek_byte = server
            .requests()
            .into_iter()
            .skip(requests_before_fault)
            .filter_map(|request| request.range())
            .map(|(first, _)| first)
            .max();
        if let Some(seek_byte) = seek_byte {
            assert!(
                seek_byte > file_len / 20,
                "device recovery requested byte {seek_byte}, near first_packet_pos rather than \
                 near where playback actually was"
            );
        }
        // A recovery whose preserved position exactly matched the decoder's
        // own cursor issues no new request at all (nothing to reseek to) —
        // not a failure of this test, just a byte-level coincidence this
        // assertion cannot force. What matters is that if a request *was*
        // issued, it did not rescan from the top; `capabilities` proving the
        // recovery itself succeeded is asserted structurally by reaching
        // `Playing` above.
        engine.finish();
        server.shutdown();
    }

    // (d) Stopped-seek validation: `verify_seek_support`'s trial seek,
    // reached the first time a seek is requested while stopped against a
    // source whose demuxer seek support is still unproven. A short, indexed
    // fixture rather than `NOXING`: `play_for` paces the virtual clock close
    // to real time, and reaching far enough into a 600s file this way would
    // itself take minutes — playing nearly to the end of a 5s file gives the
    // same "far from byte 0" shape far faster.
    {
        let short_len = match std::fs::metadata(fixture_path("sine-5s.mp3")) {
            Ok(metadata) => metadata.len(),
            Err(error) => panic!("the fixture must exist: {error}"),
        };
        let server = TestServer::start(Script::from_fixture("sine-5s.mp3"));
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&server.url("/audio.mp3"));
        assert_eq!(
            engine.handle().submit(PlaybackCommand::Play),
            Admission::Accepted
        );
        engine.await_state(PlaybackState::Playing);
        // Ordinary continuous playback only, never a seek: capabilities stay
        // `Unknown` right up to the stopped seek below, exactly the case
        // `verify_seek_support` exists for.
        engine.play_for(Duration::from_millis(4_500));
        engine.handle().submit(PlaybackCommand::Stop);
        engine.await_state(PlaybackState::Stopped);

        let requests_before_validation = server.requests().len();
        assert_eq!(
            engine
                .handle()
                .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
            Admission::Accepted
        );
        engine.await_event(|event| matches!(event, PlaybackEvent::SeekTargetStored { .. }));

        let seek_byte = server
            .requests()
            .into_iter()
            .skip(requests_before_validation)
            .filter_map(|request| request.range())
            .map(|(first, _)| first)
            .max();
        if let Some(seek_byte) = seek_byte {
            assert!(
                seek_byte > short_len / 20,
                "stopped-seek validation requested byte {seek_byte}, near first_packet_pos \
                 rather than near where playback actually was"
            );
        }
        engine.finish();
        server.shutdown();
    }
}

#[test]
fn a_launch_resume_past_an_estimated_ceiling_preserves_the_checkpoint() {
    // §5.5's retained limitation, pinned: `sine-long-vbr-noxing.mp3`'s true
    // duration is 600s, but symphonia's own `estimate_num_mpeg_frames` (no
    // Xing/Info/VBRI tag) extrapolates ~361s from its first ~16 frames.
    // `MpaReader::seek`'s `max_ts` bounds check derives from that same wrong
    // estimate and runs *before* the `SeekMode` dispatch
    // (`demuxer.rs:268-272` precedes `:292-296`), so it refuses a resume to
    // 400s — real audio that exists there — mode-independently. The seek
    // failing is expected and correct; what must not happen is the stored
    // position being discarded because of it.
    let server = TestServer::start(Script::from_fixture(VBR_NOXING));
    let mut engine = TestEngine::start_idle();
    let service = match HttpService::spawn(Limits::brisk()) {
        Ok(service) => service,
        Err(error) => panic!("the test HttpService must start: {error}"),
    };
    engine.handle().set_http(Some(service));
    let url = server.url("/audio.mp3");
    let parsed = match Url::parse(&url) {
        Ok(parsed) => parsed,
        Err(error) => panic!("test URL {url:?} must parse: {error}"),
    };
    let media = match NormalizedUrl::parse(&url) {
        Ok(normalized) => MediaId::RemoteUrl(normalized),
        Err(error) => panic!("test URL {url:?} must normalize: {error}"),
    };
    let target = Duration::from_secs(400);
    let request = engine.next_request();
    engine.send(PlaybackCommand::Load {
        request,
        media,
        source: SourceLocation::Http(parsed),
        resume: ResumeIntent::StartAt(target),
    });
    engine.await_state(PlaybackState::Failed);

    assert_eq!(
        engine.progress().position,
        target,
        "a resume past the estimated ceiling discarded the stored checkpoint instead of \
         failing around it — a position that could not be reached is not one that may be \
         discarded"
    );

    engine.finish();
    server.shutdown();
}
