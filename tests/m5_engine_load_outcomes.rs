mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use support::TestEngine;
use support::server::{Script, TestServer};
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::media::id::{MediaId, NormalizedUrl};
use tenuto::media::source::SourceLocation;
use tenuto::playback::command::{LoadRequestId, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;

fn local_load(request: u64) -> PlaybackCommand {
    let path = support::fixture("sine.flac");
    PlaybackCommand::Load {
        request: LoadRequestId::from_raw(request),
        media: MediaId::LocalFile(path.clone()),
        source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    }
}

// A bare helper, not `#[test]` itself, so `clippy::expect_used`'s test
// exemption does not cover it (`clippy.toml`'s `allow-expect-in-tests`);
// panic with context instead, as the rest of this crate's test support does.
fn remote_load(request: u64, url: &str) -> PlaybackCommand {
    let normalized = match NormalizedUrl::parse(url) {
        Ok(normalized) => normalized,
        Err(error) => panic!("test URL {url:?} must normalize: {error}"),
    };
    let parsed = match url.parse() {
        Ok(parsed) => parsed,
        Err(error) => panic!("test URL {url:?} must parse: {error}"),
    };
    PlaybackCommand::Load {
        request: LoadRequestId::from_raw(request),
        media: MediaId::RemoteUrl(normalized),
        source: SourceLocation::Http(parsed),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    }
}

#[test]
fn every_accepted_load_has_exactly_one_ordered_outcome_under_saturation() {
    let mut engine = TestEngine::start_idle();
    engine.stop_draining_events();
    for request in 1..=40 {
        engine.send(local_load(request));
    }
    std::thread::sleep(Duration::from_millis(500));
    let report = engine.shutdown_report().expect("report");

    let mut outcomes: BTreeMap<u64, usize> = BTreeMap::new();
    let mut order = Vec::new();
    for event in &report.events {
        if let Some(request) = event.load_outcome() {
            *outcomes.entry(request.get()).or_default() += 1;
            order.push(request.get());
        }
    }
    assert_eq!(
        outcomes.len(),
        40,
        "every load has an outcome: {outcomes:?}"
    );
    assert!(
        outcomes.values().all(|count| *count == 1),
        "never two: {outcomes:?}"
    );
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "outcomes keep command order"
    );
    assert!(
        report
            .events
            .iter()
            .any(|e| matches!(e, PlaybackEvent::Loaded { .. }))
    );
    assert!(
        report
            .events
            .iter()
            .any(|e| matches!(e, PlaybackEvent::LoadCancelled { .. }))
    );

    // A load's own outcome precedes the later events of its revision.
    for (index, event) in report.events.iter().enumerate() {
        if let PlaybackEvent::Loaded { session_rev, .. } = event {
            assert!(!report.events[..index].iter().any(|earlier| matches!(earlier,
                PlaybackEvent::StateChanged { session_rev: rev, state: tenuto::playback::state::PlaybackState::Paused, .. } if rev == session_rev)));
        }
    }
}

#[test]
fn a_stop_during_a_stalled_open_cancels_that_load() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let mut engine = TestEngine::start_idle();
    engine
        .handle()
        .set_http(Some(HttpService::spawn(Limits::brisk()).expect("http")));
    engine.send(remote_load(7, &server.url("/audio.mp3")));
    assert!(server.wait_until_stalled(Duration::from_secs(5)));
    engine.interrupt_stop();
    let event = engine.await_event(|e| e.load_outcome().is_some());
    assert!(matches!(event, PlaybackEvent::LoadCancelled { request, .. } if request.get() == 7));
    engine.finish();
    server.shutdown();
}

#[test]
fn shutdown_during_a_stalled_open_reports_the_cancellation() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let mut engine = TestEngine::start_idle();
    engine
        .handle()
        .set_http(Some(HttpService::spawn(Limits::brisk()).expect("http")));
    engine.send(remote_load(8, &server.url("/audio.mp3")));
    assert!(server.wait_until_stalled(Duration::from_secs(5)));
    let report = engine.shutdown_report().expect("report");
    let outcomes: Vec<_> = report
        .events
        .iter()
        .filter_map(PlaybackEvent::load_outcome)
        .collect();
    assert_eq!(outcomes, [LoadRequestId::from_raw(8)]);
    assert!(
        report
            .events
            .iter()
            .any(|e| matches!(e, PlaybackEvent::LoadCancelled { .. }))
    );
    server.shutdown();
}

#[test]
fn automatic_start_does_not_reopen_a_failed_remote_load() {
    use tenuto::volume::Volume;
    let server = TestServer::start(Script::serving(Vec::new()).status(404));
    let mut engine = TestEngine::start_idle();
    engine
        .handle()
        .set_http(Some(HttpService::spawn(Limits::brisk()).expect("http")));
    engine.send(remote_load(71, &server.url("/missing.mp3")));
    engine.send(PlaybackCommand::PlayLoaded {
        request: LoadRequestId::from_raw(71),
    });
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    engine.await_event(
        |event| matches!(event, PlaybackEvent::VolumeChanged { volume, .. } if volume.percent() == 25),
    );
    assert_eq!(
        server.requests().len(),
        1,
        "failure waits for explicit retry"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn an_older_automatic_start_cannot_play_a_newer_load() {
    use tenuto::playback::state::PlaybackState;
    use tenuto::volume::Volume;
    let mut engine = TestEngine::start_idle();
    engine.send(local_load(81));
    engine.send(local_load(82));
    engine.send(PlaybackCommand::PlayLoaded {
        request: LoadRequestId::from_raw(81),
    });
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = engine.try_event() {
            assert!(!matches!(
                event,
                PlaybackEvent::StateChanged {
                    state: PlaybackState::Playing,
                    ..
                }
            ));
            if matches!(event, PlaybackEvent::VolumeChanged { volume, .. } if volume.percent() == 25)
            {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "volume barrier never arrived"
        );
        std::thread::yield_now();
    }
    assert_eq!(engine.progress().load, Some(LoadRequestId::from_raw(82)));
    engine.finish();
}

#[test]
fn a_same_token_device_failure_survives_play_loaded_with_no_second_open() {
    let (events, negotiations) = support::failed_device_session_with_play_loaded("sine.flac", 6);
    let failed_count = events
        .iter()
        .filter(|e| matches!(e, PlaybackEvent::Failed { .. }))
        .count();
    assert_eq!(failed_count, 1, "exactly one Failed: {events:?}");
    assert!(
        !events.iter().any(|e| matches!(
            e,
            PlaybackEvent::StateChanged {
                state: tenuto::playback::state::PlaybackState::Playing,
                ..
            }
        )),
        "never reaches Playing: {events:?}"
    );
    assert_eq!(negotiations, 1, "no second open attempt");
}
