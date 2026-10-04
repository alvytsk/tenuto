//! M7 §5: a station loads, plays, and refuses what it cannot do harmlessly.

mod support;

use std::time::Duration;

use support::TestEngine;
use support::server::{Script, TestServer};
use tenuto::media::capabilities::{Continuity, SeekSupport};
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::resume::ResumeCandidate;

pub fn station() -> TestServer {
    TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station())
}

#[test]
fn a_station_loads_fresh_at_zero_as_indefinite_and_plays() {
    let server = station();
    let mut engine = TestEngine::start_idle();
    // `PlayLoaded`, not `Play`: a later task makes a bare `Play` on a station
    // open a fresh connection to rejoin the live edge, while `PlayLoaded`
    // releases the transport this load already primed. The single-request
    // assertion in the test below has to keep holding once it does.
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        ResumeIntent::StartAt(Duration::ZERO),
    );
    let loaded = engine.await_loaded();
    assert_eq!(loaded.capabilities.continuity, Continuity::Indefinite);
    assert_eq!(loaded.capabilities.seek, SeekSupport::Unsupported);
    assert_eq!(loaded.position, Duration::ZERO);
    assert_eq!(loaded.disposition, StartDisposition::Fresh);

    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.play_for(Duration::from_millis(800));
    engine.finish();
    server.shutdown();
}

#[test]
fn a_stale_candidate_on_a_station_starts_fresh_rather_than_unavailable() {
    let server = station();
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_resume(
        &server.url("/radio"),
        ResumeIntent::Candidate(ResumeCandidate {
            position: Duration::from_secs(30),
            completed: false,
        }),
    );
    let loaded = engine.await_loaded();
    // Not `ResumeUnavailable`: a station has no position it was denied.
    assert_eq!(loaded.disposition, StartDisposition::Fresh);
    assert_eq!(loaded.position, Duration::ZERO);
    engine.finish();
    server.shutdown();
}

#[test]
fn seeks_and_restart_are_rejected_and_the_stream_is_untouched() {
    let server = station();
    let mut engine = TestEngine::start_idle();
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        ResumeIntent::StartAt(Duration::ZERO),
    );
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.play_for(Duration::from_millis(300));

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(9))),
        Admission::Accepted
    );
    engine.await_event(|event| matches!(event, PlaybackEvent::SeekRejected { .. }));
    engine.send(PlaybackCommand::SeekBy(10));
    engine.await_event(|event| matches!(event, PlaybackEvent::SeekRejected { .. }));
    engine.send(PlaybackCommand::Restart);
    engine.await_event(|event| {
        matches!(event, PlaybackEvent::SeekRejected { reason, .. } if reason.contains("live"))
    });

    engine.play_for(Duration::from_millis(900));
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
        engine.count_events(|event| matches!(
            event,
            PlaybackEvent::RestartEstablished { .. } | PlaybackEvent::Failed { .. }
        )),
        0
    );
    engine.finish();
    server.shutdown();
}
