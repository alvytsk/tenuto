//! M7 §5.1–5.3, §6.2: recovery preserves listening time and never seeks.

mod support;

use std::time::{Duration, Instant};

use support::TestEngine;
use support::server::{Script, TestServer};
use tenuto::http::error::RemoteFailure;
use tenuto::http::limits::Limits;
use tenuto::media::capabilities::Continuity;
use tenuto::playback::command::{PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::state::PlaybackState;

fn station() -> Script {
    Script::from_fixture("sine-noxing.mp3").icy_station()
}

fn playing(event: &PlaybackEvent) -> bool {
    matches!(
        event,
        PlaybackEvent::StateChanged {
            state: PlaybackState::Playing,
            ..
        }
    )
}

fn paused(event: &PlaybackEvent) -> bool {
    matches!(
        event,
        PlaybackEvent::StateChanged {
            state: PlaybackState::Paused,
            ..
        }
    )
}

/// Load a station and start it on the connection the load primed.
///
/// `PlayLoaded`, never a plain `Play`: this milestone makes a plain `Play` on
/// a station open a *fresh* connection, so starting that way would cost every
/// request count below an extra connection before the test even begins.
fn start(server: &TestServer) -> TestEngine {
    let mut engine = TestEngine::start_idle();
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        ResumeIntent::StartAt(Duration::ZERO),
    );
    // The load's own `Paused`, consumed here so that a later
    // `await_event(paused)` can only be answered by the pause a test performs.
    engine.await_event(paused);
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(playing);
    engine
}

fn no_range_above_zero(server: &TestServer) {
    for request in server.requests() {
        if let Some((first, _)) = request.range() {
            assert_eq!(first, 0, "listening time must never become a byte range");
        }
    }
}

#[test]
fn pause_closes_the_connection_and_play_rejoins_with_listening_time_kept() {
    let server = TestServer::start(station());
    let mut engine = start(&server);
    engine.play_for(Duration::from_millis(500));

    engine.handle().submit(PlaybackCommand::Pause);
    engine.await_event(paused);
    let at_pause = engine.handle().progress().position;
    let written = server.bytes_written();
    engine.let_time_pass(Duration::from_millis(300));
    assert!(
        server.bytes_written() - written < 256 * 1024,
        "the server kept streaming into a paused client: the connection was not closed"
    );

    engine.handle().submit(PlaybackCommand::Play);
    engine.await_event(playing);
    assert_eq!(server.requests().len(), 2, "Play opens a fresh request");
    let resumed = engine.handle().progress().position;
    assert!(
        resumed >= at_pause,
        "listening time went backwards: {resumed:?} < {at_pause:?}"
    );
    engine.play_for(at_pause + Duration::from_millis(400));
    no_range_above_zero(&server);
    assert_eq!(
        engine.count_events(|event| matches!(
            event,
            PlaybackEvent::SeekCompleted { .. } | PlaybackEvent::RestartEstablished { .. }
        )),
        0
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn pause_wakes_a_stalled_live_read_and_thaw_announces_nothing() {
    let server = TestServer::start(station().stall_body_after(24 * 1024));
    let mut engine = TestEngine::start_idle();
    // A stall budget far longer than the window measured below, on purpose:
    // what is being proven is the pause waking the blocked read, not the stall
    // timer expiring under it.
    let request = engine.load_remote_with_limits(
        &server.url("/radio"),
        Limits {
            stall: Duration::from_secs(5),
            ..Limits::brisk()
        },
    );
    engine.await_event(paused);
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(playing);
    // Drain the ring, then run the clock past everything the stalled body can
    // supply without a round trip a parked worker could not answer: only then
    // is the worker genuinely inside a byte-channel read rather than sitting
    // on a full output ring.
    engine.play_for(Duration::from_millis(300));
    engine.let_time_pass_while_unresponsive(Duration::from_secs(3));
    assert!(
        server.wait_until_stalled(Duration::from_secs(2)),
        "the read never blocked"
    );

    let started = Instant::now();
    engine.handle().submit(PlaybackCommand::Pause);
    engine.await_event(paused);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the pause waited {:?} behind the stalled read (its stall budget is 5 s)",
        started.elapsed()
    );
    // Thaw with no source open: nothing may be released or announced.
    engine.handle().source_interrupt().thaw();
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(engine.count_events(playing), 0);
    engine.finish();
    server.release();
    server.shutdown();
}

#[test]
fn a_stop_during_a_fresh_opens_priming_is_a_cancellation_not_a_lost_stream() {
    // The second connection probes fine and then stops feeding, so the fresh
    // open parks inside `pump_audio`'s priming read rather than racing through
    // it. That is the one window in which a Stop lands on a source that is
    // opened but not yet adopted.
    let server = TestServer::start(station().then(station().stall_body_after(2 * 1024)));
    let mut engine = TestEngine::start_idle();
    // Long enough that the stall timer is never what ends the priming read:
    // what is being proven is how a Stop is classified, not a timeout.
    let request = engine.load_remote_with_limits(
        &server.url("/radio"),
        Limits {
            stall: Duration::from_secs(5),
            ..Limits::brisk()
        },
    );
    engine.await_event(paused);
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(playing);
    engine.play_for(Duration::from_millis(300));

    engine.handle().submit(PlaybackCommand::Pause);
    engine.await_event(paused);
    let kept = engine.handle().progress().position;

    engine.handle().submit(PlaybackCommand::Play);
    assert!(
        server.wait_until_stalled(Duration::from_secs(2)),
        "the second connection never stopped feeding"
    );
    // Real time for the worker to finish probing and reach the priming read it
    // cannot complete. Nothing is asked of the worker here, because it is
    // exactly the thing that is meant to be stuck.
    engine.let_time_pass_while_unresponsive(Duration::from_secs(1));

    // The window this test exists for: the fresh open must still be inside
    // priming. If it ever completes here, the `Playing` it announces makes the
    // assertions below vacuous, so the window is asserted rather than assumed.
    assert_eq!(
        engine.count_events(playing),
        0,
        "the fresh open finished; the stop no longer lands during priming"
    );
    engine.interrupt_stop();
    engine.await_event(|event| {
        matches!(
            event,
            PlaybackEvent::StateChanged {
                state: PlaybackState::Stopped,
                ..
            }
        )
    });
    assert_eq!(
        engine.count_events(|event| matches!(event, PlaybackEvent::Failed { .. })),
        0,
        "a stop during priming was reported as a dead stream"
    );
    assert_eq!(engine.handle().progress().position, kept);
    engine.finish();
    server.release();
    server.shutdown();
}

#[test]
fn stop_then_play_reopens_without_a_seek() {
    let server = TestServer::start(station());
    let mut engine = start(&server);
    engine.play_for(Duration::from_millis(400));
    engine.interrupt_stop();
    engine.await_event(|event| {
        matches!(
            event,
            PlaybackEvent::StateChanged {
                state: PlaybackState::Stopped,
                ..
            }
        )
    });
    let at_stop = engine.handle().progress().position;
    assert!(at_stop > Duration::ZERO);

    engine.send(PlaybackCommand::Play);
    engine.await_event(playing);
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::SeekRejected { .. })),
        0
    );
    engine.play_for(at_stop + Duration::from_millis(300));
    no_range_above_zero(&server);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_device_fault_recovers_a_station_without_seeking() {
    let server = TestServer::start(station());
    let mut engine = start(&server);
    engine.play_for(Duration::from_millis(400));
    let before = engine.handle().progress().position;

    engine.force_device_loss();
    engine.await_event(|event| matches!(event, PlaybackEvent::DeviceRecovered { .. }));
    engine.play_for(before + Duration::from_millis(300));
    assert_eq!(
        server.requests().len(),
        1,
        "a device fault keeps the open decoder"
    );
    no_range_above_zero(&server);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_station_url_that_turns_finite_fails_every_play_path_without_leaking_finite_capabilities() {
    for path in ["pause", "stop"] {
        let server = TestServer::start(station().then(Script::from_fixture("sine-5s.mp3")));
        let mut engine = start(&server);
        engine.play_for(Duration::from_millis(300));
        let kept = if path == "pause" {
            engine.handle().submit(PlaybackCommand::Pause);
            engine.await_event(paused);
            engine.handle().progress().position
        } else {
            engine.interrupt_stop();
            engine.await_event(|event| {
                matches!(
                    event,
                    PlaybackEvent::StateChanged {
                        state: PlaybackState::Stopped,
                        ..
                    }
                )
            });
            engine.handle().progress().position
        };

        engine.handle().submit(PlaybackCommand::Play);
        let failed = engine.await_event(|event| matches!(event, PlaybackEvent::Failed { .. }));
        let PlaybackEvent::Failed { cause, .. } = failed else {
            unreachable!()
        };
        assert_eq!(cause, Some(RemoteFailure::ResourceChanged), "{path}");
        assert_eq!(
            engine.count_events(|event| matches!(
                event,
                PlaybackEvent::CapabilitiesChanged { capabilities, .. }
                    if capabilities.continuity == Continuity::Finite
            )),
            0,
            "{path}: a finite capability event escaped"
        );
        assert_eq!(engine.handle().progress().position, kept, "{path}");
        engine.finish();
        server.shutdown();
    }
}

#[test]
fn a_plain_play_after_load_opens_fresh_but_play_loaded_releases_the_primed_transport() {
    let server = TestServer::start(station());
    let mut engine = start(&server);
    assert_eq!(
        server.requests().len(),
        1,
        "PlayLoaded releases what Load primed"
    );
    engine.finish();
    server.shutdown();

    let server = TestServer::start(station());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/radio"));
    engine.send(PlaybackCommand::Play);
    engine.await_event(playing);
    assert_eq!(
        server.requests().len(),
        2,
        "a plain Play opens at the live edge"
    );
    engine.finish();
    server.shutdown();
}
