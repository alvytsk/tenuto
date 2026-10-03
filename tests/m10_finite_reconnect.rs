//! M10: a range-capable finite HTTP episode that drops mid-play recovers and
//! keeps playing from where the listener was (spec
//! `docs/superpowers/specs/2026-09-29-tenuto-m10-finite-reconnect-design.md`).

mod support;

use std::time::Duration;

use support::server::{Script, TestServer};
use support::wav::{RATE, frame_index_wav, frame_indices};
use support::{TestEngine, UNHEARD_FRAMES};
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
        budget: Duration::from_secs(2),
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

/// Load at [`RESUME`] and play: an open costs [`OPEN`] requests and the
/// resume seek one more.
fn start(server: &TestServer, policy: ReconnectPolicy) -> TestEngine {
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(policy);
    engine.load_remote_with_resume(&server.url("/episode.wav"), ResumeIntent::StartAt(RESUME));
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
    // The backoff (20 ms) is shorter than the 300 ms ring, so the attempt
    // runs with audio still queued: the capture must account for exactly
    // what was heard and discard exactly the rest.
    let server = server(&[(PLAYING, cut()), (RESUMED, short())]);
    let mut engine = start(&server, quick());
    engine.clear_rendered();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
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
    let mut engine = start(&server, quick());
    engine.clear_rendered();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
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
    // fill. Read as a cancellation, that would retry at once, forever.
    let server = server_then(
        &[(PLAYING, cut()), (RESUMED, short())],
        episode().truncate_body_after(24 * 1024),
    );
    let mut engine = start(
        &server,
        ReconnectPolicy {
            budget: Duration::from_millis(300),
            ..quick()
        },
    );
    engine.play_until_terminal(PATIENCE);
    assert_eq!(engine.state(), PlaybackState::Failed);
    assert_eq!(engine.count_events(state(PlaybackState::Playing)), 0);
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
    let mut engine = start(
        &server,
        ReconnectPolicy {
            backoff: [Duration::from_millis(300); 5],
            ..quick()
        },
    );
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
