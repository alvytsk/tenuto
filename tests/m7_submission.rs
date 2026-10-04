//! M7 §6.1: what the handle may do before the worker looks.

mod support;

use std::time::Duration;

use support::TestEngine;
use support::server::{Script, TestServer};
use tenuto::http::limits::Limits;
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::state::PlaybackState;

#[test]
fn a_rejected_seek_leaves_a_range_less_stream_playing_on_its_one_connection() {
    let server = TestServer::start(Script::from_fixture("sine-5s.mp3").without_ranges());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/a.mp3"));
    engine.send(PlaybackCommand::Play);
    engine.play_for(Duration::from_millis(300));

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(2))),
        Admission::Accepted
    );
    engine.await_event(|event| matches!(event, PlaybackEvent::SeekRejected { .. }));

    // Audio continues, on the same connection.
    engine.play_for(Duration::from_millis(900));
    assert_eq!(
        server.requests().len(),
        1,
        "the seek must not cost the connection"
    );
    assert_eq!(
        engine.count_events(|event| matches!(event, PlaybackEvent::Failed { .. })),
        0
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_replacement_load_interrupts_a_stalled_read_instead_of_waiting_it_out() {
    let stalled =
        TestServer::start(Script::from_fixture("sine-5s.mp3").stall_body_after(16 * 1024));
    let next = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    // A stall budget far longer than the assertion below, on purpose: what is
    // being measured is the handle waking the blocked read, not the stall
    // timer expiring under it. Under the brisk 500 ms the read would time out
    // on its own within the measured window and the test would pass whatever
    // `submit` did.
    engine.load_remote_with_limits(
        &stalled.url("/a.mp3"),
        Limits {
            stall: Duration::from_secs(5),
            ..Limits::brisk()
        },
    );
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    // Drain the ring so the decoder has to go back to the byte channel, and
    // give it more than the stalled body ever delivers: only then is the
    // worker genuinely parked inside a remote read rather than on a full ring.
    engine.play_for(Duration::from_millis(500));
    // Then run the clock past everything the stalled body can supply, without
    // a round trip the parked worker could not answer: the ring empties, the
    // decoder goes back to the byte channel, and there it sits.
    engine.let_time_pass_while_unresponsive(Duration::from_millis(1500));
    assert!(
        stalled.wait_until_stalled(Duration::from_secs(2)),
        "the read never blocked"
    );

    let started = std::time::Instant::now();
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &next.url("/b.flac"),
        ResumeIntent::StartAt(Duration::ZERO),
    );
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "the load waited {:?} behind the stalled read (its stall budget is 5 s)",
        started.elapsed()
    );
    engine.finish();
    stalled.release();
    stalled.shutdown();
    next.shutdown();
}
