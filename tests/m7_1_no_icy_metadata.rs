//! M7.1 §3 R2: Tenuto never asks a station for metadata framing.
//!
//! `src/http/response.rs` refuses any response carrying `icy-metaint`
//! (`RemoteFailure::IcyFramingUnsupported`), and a real Icecast mount sends
//! that header only when the client sends `Icy-MetaData: 1`. Tenuto is
//! therefore playable against such a mount purely because it never asks.
//! This file makes that load-bearing. **Lifting it is M7 §12's job** — when
//! ICY demultiplexing lands, delete these assertions deliberately.

mod support;

use std::sync::Arc;
use std::time::Duration;

use support::TestEngine;
use support::server::{Script, TestServer};
use tenuto::clock::SystemClock;
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::library::{AddStationOutcome, add_station};
use tenuto::playback::command::{PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;
use tenuto::station::store::StationStore;

/// Panics naming the offending request if any request carried the header.
fn assert_never_asked_for_metadata(server: &TestServer, situation: &str) {
    for (index, request) in server.requests().iter().enumerate() {
        assert!(
            request.header("icy-metadata").is_none(),
            "request #{index} during {situation} asked for ICY metadata framing: {:?}\n\
             A station that answers this header sends icy-metaint, which \
             http::response refuses as IcyFramingUnsupported (M7.1 §3 R2).",
            request.header("icy-metadata"),
        );
    }
}

fn station() -> Script {
    Script::from_fixture("sine-noxing.mp3").icy_station()
}

#[test]
fn an_initial_open_never_asks_for_metadata() {
    let server = TestServer::start(station());
    // Open and immediately drop: the request is what matters.
    let source = support::open_station(&server);
    drop(source);
    assert_eq!(
        server.requests().len(),
        1,
        "expected exactly the opening request"
    );
    assert_never_asked_for_metadata(&server, "an initial open");
    server.shutdown();
}

// --- a_reconnect_never_asks_for_metadata: shapes reused from
// `tests/m7_reconnect.rs::a_disconnect_reconnects_and_never_ends_the_track`.

/// The station loops a 5 s, 64 kbps fixture, so this is about six seconds of
/// audio before the connection dies - comfortably short of `quick()`'s
/// `stable_after`, matching `m7_reconnect.rs`'s own `CUT`.
const CUT: usize = 48 * 1024;

fn quick() -> ReconnectPolicy {
    ReconnectPolicy {
        backoff: [20, 20, 20, 20, 20].map(Duration::from_millis),
        budget: Duration::from_millis(600),
        stable_after: Duration::from_secs(10),
    }
}

fn state(wanted: PlaybackState) -> impl Fn(&PlaybackEvent) -> bool {
    move |event| matches!(event, PlaybackEvent::StateChanged { state, .. } if *state == wanted)
}

/// Load a station under `quick()` and start it on the connection the load
/// primed.
///
/// `PlayLoaded`, never a plain `Play`: a plain `Play` on a station opens a
/// *fresh* connection, which would cost every request count below an extra
/// connection before the test began.
fn start_reconnecting(server: &TestServer) -> TestEngine {
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(quick());
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        ResumeIntent::StartAt(Duration::ZERO),
    );
    engine.await_event(state(PlaybackState::Paused));
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(state(PlaybackState::Playing));
    engine
}

#[test]
fn a_reconnect_never_asks_for_metadata() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .truncate_only_first_response(),
    );
    let mut engine = start_reconnecting(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(
        server.requests().len(),
        2,
        "expected the reconnect's second request"
    );
    assert_never_asked_for_metadata(&server, "a reconnect");
    engine.finish();
    server.shutdown();
}

// --- a_pause_then_play_rejoin_never_asks_for_metadata: shapes reused from
// `tests/m7_live_recovery.rs::pause_closes_the_connection_and_play_rejoins_
// with_listening_time_kept`.

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
fn start(server: &TestServer) -> TestEngine {
    let mut engine = TestEngine::start_idle();
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        ResumeIntent::StartAt(Duration::ZERO),
    );
    engine.await_event(paused);
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(playing);
    engine
}

#[test]
fn a_pause_then_play_rejoin_never_asks_for_metadata() {
    let server = TestServer::start(station());
    let mut engine = start(&server);
    engine.play_for(Duration::from_millis(500));

    engine.handle().submit(PlaybackCommand::Pause);
    engine.await_event(paused);
    engine.handle().submit(PlaybackCommand::Play);
    engine.await_event(playing);
    assert_eq!(server.requests().len(), 2, "Play opens a fresh request");
    assert_never_asked_for_metadata(&server, "a pause/play rejoin");
    engine.finish();
    server.shutdown();
}

#[test]
fn a_station_probe_never_asks_for_metadata() {
    let server = TestServer::start(station());
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let store = StationStore::new(root.path().join("stations.json"), Arc::new(SystemClock));
    let http =
        HttpService::spawn(Limits::brisk()).unwrap_or_else(|error| panic!("service: {error}"));

    let outcome = http
        .handle()
        .block_on(add_station(&http, &store, &server.url("/radio")));
    assert!(
        matches!(outcome, Ok(AddStationOutcome::Verified { .. })),
        "{outcome:?}"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "expected exactly the probe request"
    );
    assert_never_asked_for_metadata(&server, "a station probe");
    server.shutdown();
}
