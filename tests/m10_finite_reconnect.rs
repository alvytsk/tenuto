//! M10: a range-capable finite HTTP episode that drops mid-play recovers and
//! keeps playing from where the listener was (spec
//! `docs/superpowers/specs/2026-09-29-tenuto-m10-finite-reconnect-design.md`).

mod support;

use std::time::Duration;

use support::TestEngine;
use support::server::{Script, TestServer};
use support::wav::{RATE, frame_index_wav};
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

/// Load at [`RESUME`] and play. Connection 1 is the probe; connection 2 is
/// the resume seek, and it is the connection that plays.
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
