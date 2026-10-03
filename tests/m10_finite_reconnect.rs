//! M10: a range-capable finite HTTP episode that drops mid-play recovers and
//! keeps playing from where the listener was (spec
//! `docs/superpowers/specs/2026-09-29-tenuto-m10-finite-reconnect-design.md`).

mod support;

use std::time::{Duration, Instant};

use support::server::{Script, TestServer};
use support::wav::{RATE, frame_index_wav, frame_indices};
use support::{TestEngine, UNHEARD_FRAMES};
use tenuto::app::KeyRouter;
use tenuto::http::limits::Limits;
use tenuto::media::capabilities::SeekSupport;
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;

/// Four seconds: long enough to drop, recover and keep playing.
const FRAMES: u32 = 4 * RATE;

/// Where every session starts. Nonzero, so the load's own resume seek proves
/// the demuxer seekable and the episode is eligible (§3).
const RESUME: Duration = Duration::from_millis(500);

fn episode() -> Script {
    Script::serving(frame_index_wav(FRAMES))
}

/// Requests one open of the episode costs: a probe, the open, Symphonia's
/// tail-tag read and the rewind. A seek costs one more.
const OPEN: usize = 4;
/// The connection that plays after `start()`: the load's open, then its
/// resume seek.
const PLAYING: usize = OPEN + 1;

/// A server for the episode that answers connection `n` (1-based) with the
/// script paired with `n` in `faults`, and every other connection with
/// `episode()`.
fn server(faults: &[(usize, Script)]) -> TestServer {
    server_then(faults, episode())
}

/// [`server`], except that every connection past the last fault is answered
/// by `tail` rather than by `episode()`.
fn server_then(faults: &[(usize, Script)], tail: Script) -> TestServer {
    assert!(
        faults.iter().all(|(n, _)| *n >= 2),
        "connection 1 is always the episode; a fault keyed there is ignored"
    );
    let last = faults.iter().map(|(n, _)| *n).max().unwrap_or(1);
    let mut script = episode();
    for n in 2..=last {
        let next = faults.iter().find(|(at, _)| *at == n);
        script = script.then(next.map_or_else(episode, |(_, fault)| fault.clone()));
    }
    TestServer::start(script.then(tail))
}

fn quick() -> ReconnectPolicy {
    ReconnectPolicy {
        backoff: [Duration::from_millis(20); 5],
        // Far past what any test's failed attempts take, even under load.
        budget: Duration::from_secs(10),
        stable_after: Duration::from_secs(10),
    }
}

fn state(wanted: PlaybackState) -> impl Fn(&PlaybackEvent) -> bool {
    move |event| matches!(event, PlaybackEvent::StateChanged { state, .. } if *state == wanted)
}

fn stored(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::SeekTargetStored { .. })
}

fn stored_target(event: PlaybackEvent) -> Duration {
    let PlaybackEvent::SeekTargetStored { target, .. } = event else {
        unreachable!("the caller matched SeekTargetStored")
    };
    target
}

fn start(server: &TestServer, policy: ReconnectPolicy) -> TestEngine {
    start_with(server, policy, None)
}

/// Load at [`RESUME`] and play: an open costs [`OPEN`] requests and the
/// resume seek one more. `limits` replaces the harness's brisk ones.
fn start_with(server: &TestServer, policy: ReconnectPolicy, limits: Option<Limits>) -> TestEngine {
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(policy);
    let url = server.url("/episode.wav");
    match limits {
        Some(limits) => {
            engine.load_remote_with_resume_and_limits(&url, ResumeIntent::StartAt(RESUME), limits);
        }
        None => engine.load_remote_with_resume(&url, ResumeIntent::StartAt(RESUME)),
    }
    assert_eq!(
        engine.await_loaded().capabilities.seek,
        SeekSupport::Native,
        "the resume seek must prove the episode seekable"
    );
    // Consumed here, so a later wait can only match a transition the test
    // itself caused.
    engine.await_event(state(PlaybackState::Paused));
    engine.send(PlaybackCommand::Play);
    engine.await_event(state(PlaybackState::Playing));
    engine
}

#[test]
fn a_reopen_keeps_a_seek_the_session_already_proved() {
    let server = TestServer::start(episode());
    let mut engine = start(&server, quick());
    let before = server.requests().len();
    let capabilities =
        |event: &PlaybackEvent| matches!(event, PlaybackEvent::CapabilitiesChanged { .. });
    let announced = engine.count_events(capabilities);

    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    assert_eq!(
        stored_target(engine.await_event(stored)),
        Duration::from_secs(2)
    );

    assert_eq!(
        engine.count_events(capabilities),
        announced,
        "a reopen of a proven episode must not fall back to Unknown"
    );
    // The reopen itself costs four requests (probe, open, WAV tail, seek
    // read); a trial seek would make it five.
    assert_eq!(
        server.requests().len() - before,
        4,
        "the stopped seek must reopen once and run no trial seek"
    );
    engine.finish();
    server.shutdown();
}

const PATIENCE: Duration = Duration::from_secs(20);

fn failed(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::Failed { .. })
}

#[test]
fn a_failed_resume_keeps_the_stored_target_for_the_next_play() {
    // 1..=PLAYING: start(). Then the stopped seek reopens (OPEN requests) and
    // Space's range request is the next one, refused. The second Space lands.
    let server = server(&[(PLAYING + OPEN + 1, Script::serving(Vec::new()).status(503))]);
    let mut engine = start(&server, quick());
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    let target = Duration::from_secs(2);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(failed);

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(
        landed.actual.abs_diff(target) < Duration::from_millis(1),
        "the second Space resumed at {:?}, not the stored {target:?}",
        landed.actual
    );
    engine.await_event(state(PlaybackState::Playing));
    engine.finish();
    server.shutdown();
}

#[test]
fn seek_by_while_stopped_accumulates_on_the_stored_target() {
    let server = server(&[]);
    let mut engine = start(&server, quick());
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    engine.await_event(stored);
    engine.send(PlaybackCommand::SeekBy(1));
    assert_eq!(
        stored_target(engine.await_event(stored)),
        Duration::from_secs(3),
        "SeekBy must base on the stored target, not the stopped position"
    );
    engine.finish();
    server.shutdown();
}

/// Bytes per second of the fixture: stereo, 16-bit.
const BYTES_PER_SEC: usize = RATE as usize * 4;

/// The playing connection drops one second of audio past [`RESUME`].
const CUT: usize = BYTES_PER_SEC;

fn frame_at(position: Duration) -> u32 {
    (position.as_secs_f64() * f64::from(RATE)).round() as u32
}

fn assert_consecutive(indices: &[u32]) {
    assert!(!indices.is_empty(), "nothing was rendered");
    if let Some(at) = indices.windows(2).position(|pair| pair[1] != pair[0] + 1) {
        panic!(
            "frame {} followed frame {} (render index {at}): frames were replayed or skipped",
            indices[at + 1],
            indices[at]
        );
    }
}

/// Checks a render log that played from [`RESUME`], lost its connection once
/// and resumed, and returns the last frame rendered.
///
/// The log is not what the listener heard: the device renders a buffer one
/// output latency ahead, and the frames still in flight at the freeze were
/// never heard. The capture counts exactly what was heard, so a resume with
/// nothing repeated or skipped renders exactly [`UNHEARD_FRAMES`] again, once,
/// and the log is consecutive on both sides of that seam.
fn assert_resumed_where_heard(indices: &[u32]) -> u32 {
    assert_eq!(indices.first().copied(), Some(frame_at(RESUME)));
    let Some(seam) = indices.windows(2).position(|pair| pair[1] != pair[0] + 1) else {
        panic!(
            "the log never resumed: {} frames, all consecutive",
            indices.len()
        );
    };
    let (heard, resumed) = indices.split_at(seam + 1);
    assert_eq!(
        resumed[0] + UNHEARD_FRAMES,
        heard[heard.len() - 1] + 1,
        "the resume replayed or skipped heard frames"
    );
    assert_consecutive(resumed);
    resumed[resumed.len() - 1]
}

/// A backoff long enough that `play_until_event` has stopped the clock
/// before the attempt's capture, and that a test's own round trips (a seek
/// stored, a device silenced) finish inside it even on a loaded machine.
/// The freeze is then answered at a fixed instant, so exactly
/// [`UNHEARD_FRAMES`] are in flight; answered while the clock runs, it would
/// advance one period first.
fn frozen_attempts() -> ReconnectPolicy {
    ReconnectPolicy {
        backoff: [Duration::from_secs(1); 5],
        ..quick()
    }
}

/// The transport's own in-place resume of the playing connection: past one
/// chunk, `http::service` re-requests the rest of a cut body itself.
const RESUMED: usize = PLAYING + 1;

/// The first request of the first recovery attempt.
const ATTEMPT: usize = RESUMED + 1;

/// The playing connection's script: cut [`CUT`] bytes in.
fn cut() -> Script {
    episode().truncate_body_after(CUT)
}

/// A body that ends 10 bytes in. Short of one chunk, so the transport
/// reports `TruncatedBody` rather than resuming: the drop is the engine's.
fn short() -> Script {
    episode().truncate_body_after(10)
}

#[test]
fn a_dropped_connection_resumes_with_no_frame_repeated_or_skipped() {
    // 1..=PLAYING: start(). PLAYING is cut; RESUMED, the transport's own
    // re-request of the rest, ends short and the drop reaches the engine.
    // ATTEMPT..ATTEMPT+OPEN-1: the attempt's reopen; ATTEMPT+OPEN: its
    // reseek, which plays on.
    // The clock stops at `Reconnecting`, so the attempt runs with audio
    // still queued: the capture must account for exactly what was heard and
    // discard exactly the rest.
    let server = server(&[(PLAYING, cut()), (RESUMED, short())]);
    let mut engine = start(&server, frozen_attempts());
    engine.clear_rendered();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.await_event(state(PlaybackState::Playing));
    // The decoder reads a ring and more ahead of what is heard, so the
    // landing sits a few hundred milliseconds short of the cut. A second
    // past it is well past the cut.
    let landed = engine.position();
    engine.play_for(landed + Duration::from_secs(1));

    let last = assert_resumed_where_heard(&frame_indices(&engine.rendered()));
    let cut = frame_at(RESUME) + RATE;
    assert!(
        last > cut + RATE / 4,
        "playback did not carry on past the cut: last frame {last}"
    );
    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_truncated_reopen_or_reseek_is_retried_inside_the_same_outage() {
    // 1..=RESUMED: start(), the cut and the short re-request.
    // `first`: the first attempt's reopen ends 20 bytes into its probe,
    //   inside the WAV header, and the open fails with `TruncatedBody` at
    //   that one request.
    // `second`..: the second attempt reopens (OPEN), then its reseek's range
    //   response ends 10 bytes in (`TruncatedBody`, from the reseek).
    // reseek+1..: the third attempt reopens (OPEN) and reseeks (1), and lands.
    let first = ATTEMPT;
    let second = first + 1;
    let reseek = second + OPEN;
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (first, episode().truncate_body_after(20)),
        (reseek, short()),
    ]);
    let mut engine = start(&server, quick());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(server.requests().len(), reseek + OPEN + 1);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_restart_whose_priming_drops_is_landed_by_the_recovery() {
    // 1..=PLAYING: start(). The restart's reseek to zero is PLAYING+1
    // (`bytes=44-`), and its priming read ends short there, while Playing.
    // The attempt reopens (OPEN) and lands the restart with no reseek: a
    // reopened decoder already sits at zero.
    let server = server(&[(PLAYING + 1, short())]);
    let mut engine = start(&server, quick());
    engine.send(PlaybackCommand::Restart);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let restarted =
        |event: &PlaybackEvent| matches!(event, PlaybackEvent::RestartEstablished { .. });
    let first =
        engine.play_until_event(|event| state(PlaybackState::Playing)(event) || restarted(event));
    assert!(
        state(PlaybackState::Playing)(&first),
        "the restart was reported before the recovery landed it: {first:?}"
    );
    assert_eq!(engine.await_restart_established(), Duration::ZERO);
    let landed = engine.position();
    engine.play_for(landed + Duration::from_millis(500));
    assert_eq!(engine.state(), PlaybackState::Playing);
    assert!(engine.position() >= landed + Duration::from_millis(500));
    assert_eq!(
        engine.count_events(restarted),
        0,
        "the restart was reported twice"
    );
    assert_eq!(server.requests().len(), PLAYING + 1 + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_range_less_server_still_fails() {
    let server = TestServer::start(episode().without_ranges().truncate_body_after(CUT));
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(quick());
    engine.load_remote(&server.url("/episode.wav"));
    engine.send(PlaybackCommand::Play);
    engine.play_until_terminal(PATIENCE);
    assert_eq!(engine.state(), PlaybackState::Failed);
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn an_episode_never_proved_seekable_still_fails() {
    // Ranged, but loaded at zero: nothing ever demonstrated a seek, so the
    // resume capability is Undetermined (§3). With no resume seek, the
    // open's last request (OPEN) is the playing one; OPEN+1 is the
    // transport's re-request, which ends short.
    let server = server(&[(OPEN, cut()), (OPEN + 1, short())]);
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(quick());
    engine.load_remote(&server.url("/episode.wav"));
    engine.send(PlaybackCommand::Play);
    engine.play_until_terminal(PATIENCE);
    assert_eq!(engine.state(), PlaybackState::Failed);
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_failure_while_priming_is_the_attempts_not_the_sessions() {
    // 1..=RESUMED: start(), the cut and the short re-request.
    // ATTEMPT..: the first attempt reopens (OPEN) and reseeks, then that
    //   range response ends 24 KiB in: past the reseek, inside the priming
    //   fill, and short of a chunk, so the transport does not resume it.
    // reseek+1..: the second attempt reopens (OPEN) and reseeks (1), and lands.
    let reseek = ATTEMPT + OPEN;
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (reseek, episode().truncate_body_after(24 * 1024)),
    ]);
    let mut engine = start(&server, frozen_attempts());
    engine.clear_rendered();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.await_event(state(PlaybackState::Playing));
    let landed = engine.position();
    engine.play_for(landed + Duration::from_millis(300));

    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(
        engine.count_events(state(PlaybackState::Playing)),
        0,
        "a failed attempt announced Playing"
    );
    assert_eq!(server.requests().len(), reseek + OPEN + 1);
    assert_resumed_where_heard(&frame_indices(&engine.rendered()));
    engine.finish();
    server.shutdown();
}

#[test]
fn a_priming_failure_counts_against_the_budget() {
    // As above, but every attempt's reseek response ends inside the priming
    // fill. Read as a cancellation, that would retry at once, forever; read
    // as final, it would fail after one attempt.
    // Attempt 1 runs 20 ms after the drop and fails well inside the 2 s
    // budget; attempt 2 waits 3 s, and its failure is past the budget. Each
    // costs a reopen (OPEN) and a reseek (1).
    let server = server_then(
        &[(PLAYING, cut()), (RESUMED, short())],
        episode().truncate_body_after(24 * 1024),
    );
    let mut backoff = [Duration::from_secs(3); 5];
    backoff[0] = Duration::from_millis(20);
    let mut engine = start(
        &server,
        ReconnectPolicy {
            backoff,
            budget: Duration::from_secs(2),
            ..quick()
        },
    );
    engine.play_until_terminal(PATIENCE);
    assert_eq!(engine.state(), PlaybackState::Failed);
    assert_eq!(engine.count_events(state(PlaybackState::Playing)), 0);
    assert_eq!(
        server.requests().len(),
        RESUMED + 2 * (OPEN + 1),
        "a priming failure must be retried, with backoff, until the budget"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn past_the_budget_it_fails_and_space_tries_exactly_once() {
    // 1..=PLAYING: start(); PLAYING is cut. Every later request is refused:
    // the transport's re-request, which hands the engine the drop, then each
    // attempt's probe, until the budget runs out. Space is one more probe.
    let server = server_then(&[(PLAYING, cut())], Script::serving(Vec::new()).status(503));
    let mut engine = start(
        &server,
        ReconnectPolicy {
            budget: Duration::from_millis(300),
            ..quick()
        },
    );
    engine.play_until_terminal(PATIENCE);
    assert_eq!(engine.state(), PlaybackState::Failed);
    assert_eq!(
        engine.count_events(state(PlaybackState::Reconnecting)),
        1,
        "the drop never entered one outage"
    );
    let before = server.requests().len();
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Failed);
    assert_eq!(server.requests().len(), before + 1, "Space is one attempt");
    engine.finish();
    server.shutdown();
}

#[test]
fn a_device_that_will_not_open_fails_the_attempt_at_once() {
    // 1..=RESUMED: start(), the cut and the short re-request. The attempt
    // reopens (OPEN) and reseeks (1) before it meets the dead device, and
    // nothing after it touches the network.
    let server = server(&[(PLAYING, cut()), (RESUMED, short())]);
    let mut engine = start(&server, frozen_attempts());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.silence_the_device();
    engine.await_state(PlaybackState::Failed);
    assert_eq!(
        engine.count_events(state(PlaybackState::Reconnecting)),
        0,
        "a device failure was retried against the network budget"
    );
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

/// A backoff long enough that no attempt runs while a test acts.
fn parked() -> ReconnectPolicy {
    ReconnectPolicy {
        backoff: [Duration::from_secs(30); 5],
        budget: Duration::from_secs(60),
        stable_after: Duration::from_secs(10),
    }
}

/// Splits a render log after its first discontinuity: before and after.
fn split_at_gap(indices: &[u32]) -> (&[u32], &[u32]) {
    match indices.windows(2).position(|pair| pair[1] != pair[0] + 1) {
        Some(at) => indices.split_at(at + 1),
        None => (indices, &[]),
    }
}

fn seek_completed(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::SeekCompleted { .. })
}

fn restart_established(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::RestartEstablished { .. })
}

/// 1..=RESUMED: start(), the cut and the short re-request; the drop reaches
/// the engine there.
fn dropped() -> TestServer {
    server(&[(PLAYING, cut()), (RESUMED, short())])
}

#[test]
fn a_seek_during_recovery_is_stored_offline_and_the_landing_anchors_at_it() {
    // 1..=RESUMED: dropped(). The seek is stored with no request. The attempt
    // reopens (ATTEMPT..ATTEMPT+OPEN-1) and reseeks to the target
    // (ATTEMPT+OPEN), which plays on.
    let server = dropped();
    let mut engine = start(&server, frozen_attempts());
    engine.clear_rendered();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(server.requests().len(), RESUMED);
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    assert_eq!(stored_target(engine.await_event(stored)), target);
    // Exact, so it also proves no attempt has run yet.
    assert_eq!(
        server.requests().len(),
        RESUMED,
        "a seek during recovery touched the network"
    );

    engine.await_event(state(PlaybackState::Playing));
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    // §5 step 5: progress counts from the landing, not from the capture. The
    // clock is frozen, so nothing has been heard since.
    assert_eq!(engine.position(), target);
    engine.play_for(target + Duration::from_millis(300));
    let indices = frame_indices(&engine.rendered());
    let (_, after) = split_at_gap(&indices);
    assert_eq!(after.first().copied(), Some(frame_at(target)));
    assert_consecutive(after);
    engine.finish();
    server.shutdown();
}

#[test]
fn seek_by_presses_during_recovery_accumulate() {
    // 1..=RESUMED: dropped(). Nothing after it: every press is stored.
    let server = dropped();
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(engine.handle().submit_seek(RESUME), Admission::Accepted);
    engine.await_event(stored);
    let mut targets = Vec::new();
    for _ in 0..4 {
        engine.send(PlaybackCommand::SeekBy(1));
        targets.push(stored_target(engine.await_event(stored)));
    }
    let second = Duration::from_secs(1);
    assert_eq!(
        targets,
        [
            RESUME + second,
            RESUME + 2 * second,
            RESUME + 3 * second,
            // Clamped to the duration cached on entry: the decoder is gone.
            DURATION,
        ]
    );
    assert_eq!(server.requests().len(), RESUMED);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_failed_attempt_keeps_the_stored_target_for_the_next() {
    // 1..=RESUMED: dropped(). ATTEMPT: the first attempt's probe is refused.
    // ATTEMPT+1..ATTEMPT+OPEN: the second attempt reopens; ATTEMPT+OPEN+1:
    // its reseek to the target, which lands.
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (ATTEMPT, Script::serving(Vec::new()).status(503)),
    ]);
    let mut engine = start(&server, frozen_attempts());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);
    engine.await_event(state(PlaybackState::Playing));
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    assert_eq!(server.requests().len(), ATTEMPT + OPEN + 1);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_restart_during_recovery_lands_at_zero_as_a_restart() {
    // 1..=RESUMED: dropped(). ATTEMPT: the first attempt's probe is refused.
    // ATTEMPT+1..ATTEMPT+OPEN: the second attempt reopens, and lands with no
    // reseek: a reopened decoder already sits at zero.
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (ATTEMPT, Script::serving(Vec::new()).status(503)),
    ]);
    let mut engine = start(&server, frozen_attempts());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Restart);
    assert_eq!(stored_target(engine.await_event(stored)), Duration::ZERO);
    // Ordered, not counted: a count would race the running backoff.
    let first = engine
        .await_event(|event| state(PlaybackState::Playing)(event) || restart_established(event));
    assert!(
        state(PlaybackState::Playing)(&first),
        "the restart was reported before the recovery landed it: {first:?}"
    );
    assert_eq!(engine.await_restart_established(), Duration::ZERO);
    assert_eq!(engine.count_events(seek_completed), 0);
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_after_a_restart_supersedes_it() {
    // 1..=RESUMED: dropped(). The attempt reopens (ATTEMPT..) and reseeks
    // to the seek's target (ATTEMPT+OPEN).
    let server = dropped();
    let mut engine = start(&server, frozen_attempts());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Restart);
    engine.await_event(stored);
    let target = Duration::from_secs(1);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    assert_eq!(stored_target(engine.await_event(stored)), target);
    engine.await_event(state(PlaybackState::Playing));
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    assert_eq!(engine.count_events(restart_established), 0);
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_restart_stored_during_recovery_survives_stop_and_space() {
    // 1..=RESUMED: dropped(). The restart is stored and the stop costs
    // nothing. Space reopens (ATTEMPT..ATTEMPT+OPEN-1) and lands at zero
    // with no reseek.
    let server = dropped();
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Restart);
    engine.await_event(stored);
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(server.requests().len(), RESUMED);
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    assert_eq!(engine.await_restart_established(), Duration::ZERO);
    engine.await_event(state(PlaybackState::Playing));
    assert_eq!(server.requests().len(), RESUMED + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_priming_failure_on_space_fails_honestly_and_keeps_the_target() {
    // 1..=RESUMED: dropped(). The seek is stored and the stop costs nothing.
    // ATTEMPT..: the first Space reopens (OPEN) and reseeks, and that range
    // response ends 24 KiB in, inside the priming fill. reseek+1..: the
    // second Space reopens (OPEN) and reseeks (1), and lands.
    let reseek = ATTEMPT + OPEN;
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (reseek, episode().truncate_body_after(24 * 1024)),
    ]);
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(failed);
    assert_eq!(engine.count_events(state(PlaybackState::Playing)), 0);
    assert_eq!(engine.count_events(seek_completed), 0);
    assert_eq!(server.requests().len(), reseek);

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    engine.await_event(state(PlaybackState::Playing));
    assert_eq!(server.requests().len(), reseek + OPEN + 1);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_near_the_end_during_recovery_plays_out_and_ends() {
    // 1..=RESUMED: dropped(). The attempt reopens (ATTEMPT..) and reseeks
    // (ATTEMPT+OPEN) 200 ms short of the end, which plays out.
    let server = dropped();
    let mut engine = start(&server, frozen_attempts());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(
        engine.handle().submit_seek(Duration::from_millis(3800)),
        Admission::Accepted
    );
    engine.await_event(stored);
    engine.play_until_terminal(PATIENCE);
    assert!(engine.saw_end_of_track(), "the episode did not end");
    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

/// Deadlines far past the harness's 20 s patience: what these tests prove is
/// a command waking a blocked attempt, never a timeout expiring under it.
fn patient() -> Limits {
    Limits {
        headers: Duration::from_secs(60),
        stall: Duration::from_secs(60),
        open: Duration::from_secs(60),
        ..Limits::brisk()
    }
}

/// The episode's length, which a seek in a recovery-pause is clamped to.
const DURATION: Duration = Duration::from_secs((FRAMES / RATE) as u64);

#[test]
fn pausing_during_recovery_keeps_the_target_and_space_resumes_at_it() {
    // 1..=RESUMED: dropped(). The seek, the pause and the stored target cost
    // nothing. Space reopens (ATTEMPT..ATTEMPT+OPEN-1) and reseeks to the
    // target (ATTEMPT+OPEN).
    let server = dropped();
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);

    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Paused));
    assert!(
        !engine.handle().source_interrupt().is_frozen(),
        "a recovery pause must leave no freeze level standing"
    );
    assert_eq!(server.requests().len(), RESUMED);

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    engine.await_event(state(PlaybackState::Playing));
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_in_a_recovery_pause_is_stored_offline_and_space_lands_at_it() {
    // Decision 6. 1..=RESUMED: dropped(). The pause leaves no source and no
    // transport, so both seeks are stored with no request. Space reopens
    // (ATTEMPT..ATTEMPT+OPEN-1) and reseeks to the target (ATTEMPT+OPEN).
    let server = dropped();
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Paused));

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(10)),
        Admission::Accepted
    );
    assert_eq!(
        stored_target(engine.await_event(stored)),
        DURATION,
        "clamped to the duration cached when the connection was lost"
    );
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    assert_eq!(stored_target(engine.await_event(stored)), target);
    assert_eq!(
        server.requests().len(),
        RESUMED,
        "a seek in a recovery pause touched the network"
    );

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    engine.await_event(state(PlaybackState::Playing));
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn stopping_during_recovery_keeps_the_target_and_space_resumes_at_it() {
    // 1..=RESUMED: dropped(). The seek and the stop cost nothing. Space
    // reopens (ATTEMPT..ATTEMPT+OPEN-1) and reseeks (ATTEMPT+OPEN).
    let server = dropped();
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(server.requests().len(), RESUMED);
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_restart_stored_during_recovery_survives_pause_and_space() {
    // 1..=RESUMED: dropped(). The restart and the pause cost nothing. Space
    // reopens (ATTEMPT..ATTEMPT+OPEN-1) and lands at zero with no reseek.
    let server = dropped();
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Restart);
    engine.await_event(stored);
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Paused));
    assert_eq!(server.requests().len(), RESUMED);
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    assert_eq!(engine.await_restart_established(), Duration::ZERO);
    assert_eq!(server.requests().len(), RESUMED + OPEN);
    engine.finish();
    server.shutdown();
}

/// Pauses while the first attempt is blocked at the server on the request
/// `stalled`. Under `patient()` limits only the pause can end the wait inside
/// the harness's patience. Space then reopens (OPEN) and reseeks (1).
fn pause_cancels_a_blocked_attempt(stalled: usize, fault: Script) {
    let server = server(&[(PLAYING, cut()), (RESUMED, short()), (stalled, fault)]);
    let mut engine = start_with(&server, quick(), Some(patient()));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert!(
        server.wait_until_stalled(PATIENCE),
        "the attempt never reached its stall"
    );
    assert_eq!(server.requests().len(), stalled);
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Paused));
    assert!(
        !engine.handle().source_interrupt().is_frozen(),
        "a recovery pause must leave no freeze level standing"
    );
    // The outage is gone: no attempt can run while these counts are taken.
    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(
        engine.count_events(state(PlaybackState::Paused)),
        0,
        "Paused was announced twice"
    );
    server.release();
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Playing));
    assert_eq!(server.requests().len(), stalled + OPEN + 1);
    engine.finish();
    server.shutdown();
}

#[test]
fn pause_cancels_a_blocked_reopen() {
    // The attempt's probe (ATTEMPT) never gets its headers.
    pause_cancels_a_blocked_attempt(ATTEMPT, episode().stall_headers());
}

#[test]
fn pause_cancels_a_blocked_reseek() {
    // The attempt reopens (ATTEMPT..ATTEMPT+OPEN-1); its reseek never gets
    // its headers.
    pause_cancels_a_blocked_attempt(ATTEMPT + OPEN, episode().stall_headers());
}

#[test]
fn pause_cancels_a_blocked_priming_read() {
    // The reseek needs a few KiB; priming wants the 300 ms ring (about
    // 56 KiB), so the stall at 16 KiB lands inside priming.
    pause_cancels_a_blocked_attempt(ATTEMPT + OPEN, episode().stall_body_after(16 * 1024));
}

#[test]
fn a_seek_cancels_a_blocked_attempt_and_the_next_runs_at_once() {
    // 1..=RESUMED: dropped(). The first attempt's probe (ATTEMPT) never gets
    // its headers. The seek retires it and is stored; the next attempt
    // reopens (ATTEMPT+1..ATTEMPT+OPEN) and reseeks to the target
    // (ATTEMPT+OPEN+1). Every backoff after the first is 30 s, past the
    // harness's patience: a cancellation that spent one would never land.
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (ATTEMPT, episode().stall_headers()),
    ]);
    let mut backoff = [Duration::from_secs(30); 5];
    backoff[0] = Duration::from_millis(20);
    let policy = ReconnectPolicy {
        backoff,
        budget: Duration::from_secs(60),
        ..quick()
    };
    let mut engine = start_with(&server, policy, Some(patient()));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert!(server.wait_until_stalled(PATIENCE));
    assert_eq!(server.requests().len(), ATTEMPT);
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    assert_eq!(stored_target(engine.await_event(stored)), target);
    engine.await_event(state(PlaybackState::Playing));
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(server.requests().len(), ATTEMPT + OPEN + 1);
    engine.finish();
    server.release();
    server.shutdown();
}

#[test]
fn stop_cancels_a_blocked_priming_read_and_space_resumes() {
    // As `pause_cancels_a_blocked_priming_read`, with a stop.
    let reseek = ATTEMPT + OPEN;
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (reseek, episode().stall_body_after(16 * 1024)),
    ]);
    let mut engine = start_with(&server, quick(), Some(patient()));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert!(server.wait_until_stalled(PATIENCE));
    assert_eq!(server.requests().len(), reseek);
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(engine.count_events(failed), 0);
    server.release();
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Playing));
    assert_eq!(server.requests().len(), reseek + OPEN + 1);
    engine.finish();
    server.shutdown();
}

/// Waits until the worker is inside a blocked source read: only the wait
/// hook publishes `buffering`.
fn await_blocked_read(engine: &mut TestEngine) {
    let deadline = Instant::now() + PATIENCE;
    while !engine.progress().buffering {
        assert!(
            Instant::now() < deadline,
            "the worker never blocked in a read"
        );
        std::thread::yield_now();
    }
}

#[test]
fn a_pause_raced_by_a_dropped_connection_lands_paused_not_reconnecting() {
    // §3. PLAYING stalls a second of audio in and ends one byte after its
    // release; RESUMED, the transport's re-request of the rest, ends short,
    // so the drop reaches a worker blocked in that read with the pause
    // submitted but not yet dispatched. Both seeks in the recovery-pause are
    // stored with no request. Space reopens (ATTEMPT..ATTEMPT+OPEN-1) and
    // reseeks (ATTEMPT+OPEN).
    let server = server(&[
        (
            PLAYING,
            episode()
                .stall_body_after(BYTES_PER_SEC)
                .truncate_body_after(BYTES_PER_SEC + 1),
        ),
        (RESUMED, short()),
    ]);
    let mut engine = start_with(&server, quick(), Some(patient()));
    // Drain what the stalled body supplied, then demand far more without a
    // round trip a blocked worker could not answer (m7_cancellation's
    // StalledBody arrangement).
    engine.play_for(RESUME + Duration::from_millis(200));
    engine.let_time_pass_while_unresponsive(Duration::from_millis(1500));
    assert!(server.wait_until_stalled(PATIENCE));
    await_blocked_read(&mut engine);
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    // The hook announces this from inside the blocked read.
    engine.await_event(state(PlaybackState::Paused));
    server.release();
    // A round trip queued behind the pause: the worker answers it only once
    // the drop has reached the read it is blocked in. A seek sent before it
    // would retire that read and race the drop.
    engine.position();

    // Seeks are stored only offline, so the stored clamp also proves the
    // drop landed in a recovery-pause, at the duration it cached (Decision 6).
    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(10)),
        Admission::Accepted
    );
    assert_eq!(stored_target(engine.await_event(stored)), DURATION);
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    assert_eq!(stored_target(engine.await_event(stored)), target);
    assert_eq!(server.requests().len(), RESUMED);
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    assert!(!engine.handle().source_interrupt().is_frozen());

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    engine.await_event(state(PlaybackState::Playing));
    assert_eq!(
        server.requests().len(),
        ATTEMPT + OPEN,
        "Space must reopen: a recovery pause holds no connection"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_new_load_during_recovery_drops_the_outage_and_the_target() {
    // 1..=RESUMED: dropped(). The seek is stored with no request. The new
    // load opens (ATTEMPT..ATTEMPT+OPEN-1) and resume-seeks (ATTEMPT+OPEN),
    // which plays.
    let server = dropped();
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(
        engine.handle().submit_seek(Duration::from_millis(2500)),
        Admission::Accepted
    );
    engine.await_event(stored);
    engine.load_remote_with_resume(&server.url("/episode.wav"), ResumeIntent::StartAt(RESUME));
    engine.send(PlaybackCommand::Play);
    engine.await_event(state(PlaybackState::Playing));
    engine.play_for(RESUME + Duration::from_millis(200));
    assert_eq!(engine.count_events(seek_completed), 0);
    assert!(engine.position() < Duration::from_millis(2500));
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    assert_eq!(server.requests().len(), ATTEMPT + OPEN);
    engine.finish();
    server.shutdown();
}

#[test]
fn shutdown_during_a_blocked_attempt_joins_promptly() {
    // 1..=RESUMED: dropped(). The attempt's probe (ATTEMPT) never gets its
    // headers.
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (ATTEMPT, episode().stall_headers()),
    ]);
    let mut engine = start_with(&server, quick(), Some(patient()));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert!(server.wait_until_stalled(PATIENCE));
    assert_eq!(server.requests().len(), ATTEMPT);
    engine.handle().submit_shutdown();
    assert!(
        engine.join_within(Duration::from_secs(5)),
        "shutdown waited on a blocked attempt"
    );
    server.release();
    server.shutdown();
}

/// Any second failure in the same outage gives up at once (the budget is 1
/// ms); a fresh outage reconnects. One heard second ends the window.
fn one_shot() -> ReconnectPolicy {
    ReconnectPolicy {
        budget: Duration::from_millis(1),
        stable_after: Duration::from_secs(1),
        ..quick()
    }
}

/// The request a seek issued right after [`dropped`]'s recovery lands makes.
/// 1..=RESUMED: dropped(). ATTEMPT..ATTEMPT+OPEN-1: the reopen; ATTEMPT+OPEN:
/// the reseek, which lands. The seek is the next request, and the transport's
/// re-request of its cut body the one after.
const AFTER_LANDING: usize = ATTEMPT + OPEN + 1;

/// A server whose recovery lands, then whose seek's range response is cut
/// `after` bytes in, with the transport's re-request ending short so the
/// drop reaches the engine.
fn dropped_after_landing(after: usize) -> TestServer {
    server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (AFTER_LANDING, episode().truncate_body_after(after)),
        (AFTER_LANDING + 1, short()),
    ])
}

#[test]
fn a_forward_seek_does_not_end_the_outage() {
    // The seek's response is cut half a second in: about 200 ms heard after
    // the landing (the decoder runs a 300 ms ring ahead of what is heard).
    let server = dropped_after_landing(BYTES_PER_SEC / 2);
    let mut engine = start(&server, one_shot());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    let landed = engine.position();
    // Further than stable_after: position growth would call this stable.
    assert_eq!(
        engine
            .handle()
            .submit_seek(landed + Duration::from_millis(1200)),
        Admission::Accepted
    );
    engine.await_seek_completed(PATIENCE);
    engine.play_until_terminal(PATIENCE);
    assert_eq!(
        engine.state(),
        PlaybackState::Failed,
        "the seek ended the outage, so the drop after it started a fresh one"
    );
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    assert_eq!(server.requests().len(), AFTER_LANDING + 1);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_backward_seek_does_not_stop_heard_playback_ending_the_outage() {
    // The seek's response is cut 1.5 s in: about 1.2 s heard after the
    // landing, past stable_after.
    let server = dropped_after_landing(BYTES_PER_SEC * 3 / 2);
    // A long backoff, so the exact request count below is read inside it.
    let policy = ReconnectPolicy {
        backoff: [Duration::from_secs(1); 5],
        ..one_shot()
    };
    let mut engine = start(&server, policy);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(engine.handle().submit_seek(RESUME), Admission::Accepted);
    engine.await_seek_completed(PATIENCE);
    // A fresh outage: the budget did not carry over.
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(server.requests().len(), AFTER_LANDING + 1);
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(engine.count_events(failed), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_pause_inside_the_stability_window_ends_the_outage() {
    // 1..=RESUMED: dropped(). ATTEMPT..ATTEMPT+OPEN-1: the reopen; `landing`:
    // the reseek, which plays and is cut 0.75 s in (about 450 ms heard, short
    // of `stable_after`); `landing`+1: the transport's re-request, which ends
    // short, so the second drop reaches the engine inside the first window.
    // The second outage reopens (OPEN) and reseeks (1).
    let landing = ATTEMPT + OPEN;
    let server = server(&[
        (PLAYING, cut()),
        (RESUMED, short()),
        (
            landing,
            episode().truncate_body_after(BYTES_PER_SEC * 3 / 4),
        ),
        (landing + 1, short()),
    ]);
    let mut engine = start(&server, one_shot());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    // A park, not a close: the recovered connection is healthy.
    engine.send(PlaybackCommand::Pause);
    engine.await_event(state(PlaybackState::Paused));
    engine.send(PlaybackCommand::Play);
    engine.await_event(state(PlaybackState::Playing));
    // The budget is 1 ms: an outage left standing across the pause gives up
    // at the second drop; a fresh one reconnects.
    let next =
        engine.play_until_event(|event| state(PlaybackState::Reconnecting)(event) || failed(event));
    assert!(
        state(PlaybackState::Reconnecting)(&next),
        "the pause left the first outage standing: {next:?}"
    );
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(server.requests().len(), landing + 1 + OPEN + 1);
    engine.finish();
    server.shutdown();
}

#[test]
fn arrow_bursts_during_recovery_accumulate_across_progress() {
    let server = dropped();
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let mut router = KeyRouter::new();
    // Past the router's 250 ms quiet window.
    let quiet = Duration::from_millis(300);
    let mut targets = Vec::new();
    for _ in 0..2 {
        let now = Instant::now();
        let mirror = engine.handle().progress().position;
        router.route(
            &engine.handle(),
            true,
            mirror,
            None,
            now,
            PlaybackCommand::SeekBy(1),
        );
        router.flush(&engine.handle(), now + quiet);
        let event = engine.await_event(stored);
        router.observe(&event);
        targets.push(stored_target(event));
        // Progress ticks: the ring drains during backoff, so the mirror moves.
        engine.let_time_pass(Duration::from_millis(100));
    }
    assert_eq!(targets[1], targets[0] + Duration::from_secs(1));
    engine.finish();
    server.shutdown();
}
