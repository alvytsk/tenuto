//! M7 §7: disconnect is not completion; reconnect is bounded and cancellable.

mod support;

use std::time::Duration;

use support::TestEngine;
use support::server::{Script, TestServer};
use tenuto::http::error::RemoteFailure;
use tenuto::playback::command::PlaybackCommand;
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;

/// The station loops a 5 s, 64 kbps fixture, so this is about six seconds of
/// audio before the connection dies - comfortably short of `quick()`'s
/// `stable_after`, so a connection that plays out its cut never ends the
/// outage by itself.
const CUT: usize = 48 * 1024;

fn station() -> Script {
    Script::from_fixture("sine-noxing.mp3").icy_station()
}

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

fn failed(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::Failed { .. })
}

fn start(server: &TestServer) -> TestEngine {
    start_with(server, quick())
}

/// Load a station under `policy` and start it on the connection the load
/// primed.
///
/// `PlayLoaded`, never a plain `Play`: a plain `Play` on a station opens a
/// *fresh* connection, which would cost every request count below an extra
/// connection before the test began.
fn start_with(server: &TestServer, policy: ReconnectPolicy) -> TestEngine {
    start_on(TestEngine::start_idle(), server, policy)
}

/// [`start_with`] on a held network clock (M9.5): no backoff or budget moves
/// until the test steps the clock or runs it.
fn start_held(server: &TestServer, policy: ReconnectPolicy) -> TestEngine {
    start_on(TestEngine::start_on_fake_clock(), server, policy)
}

fn start_on(mut engine: TestEngine, server: &TestServer, policy: ReconnectPolicy) -> TestEngine {
    engine.handle().set_reconnect_policy(policy);
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        tenuto::playback::command::ResumeIntent::StartAt(Duration::ZERO),
    );
    // The load's own `Paused`, consumed here so that a later
    // `await_event(state(Paused))` can only be answered by a pause a test
    // actually performed.
    engine.await_event(state(PlaybackState::Paused));
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(state(PlaybackState::Playing));
    engine
}

#[test]
fn a_disconnect_reconnects_and_never_ends_the_track() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .truncate_only_first_response(),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let during = engine.handle().progress().position;
    engine.play_until_event(state(PlaybackState::Playing));
    let after = engine.handle().progress().position;
    assert!(
        after >= during,
        "anchored below the captured value: {after:?} < {during:?}"
    );
    engine.play_for(after + Duration::from_millis(300));
    assert!(!engine.saw_end_of_track());
    assert_eq!(
        engine.count_events(|event| matches!(
            event,
            PlaybackEvent::EndOfTrack { .. }
                | PlaybackEvent::StateChanged {
                    state: PlaybackState::Ended,
                    ..
                }
        )),
        0
    );
    assert_eq!(server.requests().len(), 2);
    engine.finish();
    server.shutdown();
}

#[test]
fn listening_time_counts_drained_audio_and_then_stands_still() {
    // Every reconnect is refused, so the engine stays in Reconnecting - and
    // this policy's own backoff and budget are long enough that neither an
    // attempt nor a give-up can land while the position is being measured.
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::serving(Vec::new()).status(503)),
    );
    let mut engine = start_with(
        &server,
        ReconnectPolicy {
            backoff: [Duration::from_secs(30); 5],
            budget: Duration::from_secs(60),
            ..quick()
        },
    );
    engine.play_until_event(state(PlaybackState::Reconnecting));
    // The callback coalesces spans it cannot publish while the harness clock
    // runs ahead of the worker, and only hands them over on its next
    // invocation - so let the clock tick on once before reading the position
    // the disconnect left behind.
    engine.let_time_pass(Duration::from_millis(400));
    let drained = engine.handle().progress().position;
    // CUT is about six seconds of this fixture, and every one of them was
    // counted: audio the listener hears after the source is gone is still
    // listening time. Without that the position would stand where the decoder
    // stopped, a ring's worth short of what was actually played.
    assert!(
        drained >= Duration::from_secs(5),
        "drained audio was not counted: {drained:?}"
    );
    // Nothing is playing now, and silence is not listening time.
    engine.let_time_pass(Duration::from_millis(400));
    assert_eq!(
        engine.handle().progress().position,
        drained,
        "silence advanced listening time"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_retryable_refusal_exhausts_the_budget_and_play_then_tries_exactly_once() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::serving(Vec::new()).status(503)),
    );
    let mut engine = start(&server);
    let event = engine.play_until_event(failed);
    let PlaybackEvent::Failed { cause, .. } = event else {
        unreachable!("the predicate above already matched Failed")
    };
    assert!(
        matches!(cause, Some(RemoteFailure::Status { status: 503, .. })),
        "{cause:?}"
    );
    let attempts = server.requests().len();
    assert!(attempts > 2, "the budget allowed no retries");

    engine.send(PlaybackCommand::Play);
    engine.await_event(failed);
    assert_eq!(
        server.requests().len(),
        attempts + 1,
        "an explicit Play is one attempt"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_gone_station_fails_on_the_first_reconnect() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::serving(Vec::new()).status(404)),
    );
    let mut engine = start(&server);
    let event = engine.play_until_event(failed);
    let PlaybackEvent::Failed { cause, .. } = event else {
        unreachable!("the predicate above already matched Failed")
    };
    assert!(
        matches!(cause, Some(RemoteFailure::Status { status: 404, .. })),
        "{cause:?}"
    );
    assert_eq!(server.requests().len(), 2);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_server_that_keeps_closing_early_is_one_outage() {
    // Every connection dies after a second of audio: never `stable_after` of
    // played audio, so one outage's budget is what judges all of them, and it
    // runs out having tried several times rather than once.
    //
    // A shorter cut than `CUT`, and a longer budget than `quick()`'s,
    // deliberately: every attempt here *succeeds* and then plays its cut out
    // before dying, so an attempt costs a connect, an open and a second of
    // playout in wall time - which a loaded machine stretches several-fold.
    // `CUT` at 600 ms of budget fits barely two attempts idle and one under
    // load, and one attempt cannot show that a short connection failed to
    // reset the outage.
    let server = TestServer::start(station().truncate_body_after(8 * 1024));
    let mut engine = start_with(
        &server,
        ReconnectPolicy {
            budget: Duration::from_secs(3),
            ..quick()
        },
    );
    engine.play_until_event(failed);
    assert!(server.requests().len() > 2);
    engine.finish();
    server.shutdown();
}

#[test]
fn an_initial_open_that_fails_is_not_retried() {
    let server = TestServer::start(Script::serving(Vec::new()).status(503));
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(quick());
    engine.load_remote_expecting_failure(&server.url("/radio"));
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(server.requests().len(), 1);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_reconnect_to_different_audio_parameters_plays() {
    // sine-22k-mono.mp3: 22050 Hz mono, against the 44.1 kHz stereo station.
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::from_fixture("sine-22k-mono.mp3").icy_station()),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    let resumed = engine.handle().progress().position;
    engine.play_for(resumed + Duration::from_millis(500));
    assert!(engine.captured_is_audible());
    engine.finish();
    server.shutdown();
}

#[test]
fn a_source_that_probes_but_dies_while_priming_is_a_failed_attempt() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            // Enough to probe, not enough to prime a frame past it.
            .then(station().truncate_body_after(2 * 1024))
            .then(station()),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert!(
        server.requests().len() >= 3,
        "the priming failure was not retried"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_station_that_keeps_dying_while_priming_backs_off_and_exhausts_the_budget() {
    // Every reconnect probes and then dies before a frame primes. Each one is
    // a failed attempt: it waits out the backoff, and the outage's budget
    // ends them, rather than retrying at once and forever.
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(station().truncate_body_after(2 * 1024)),
    );
    let mut engine = start_with(
        &server,
        ReconnectPolicy {
            backoff: [100; 5].map(Duration::from_millis),
            ..quick()
        },
    );
    engine.play_until_event(failed);
    // One playing connection, then at most one attempt per 100 ms backoff
    // step inside the 600 ms budget. No lower bound: a loaded machine may
    // spend the whole budget on one attempt.
    let requests = server.requests().len();
    assert!(
        requests <= 8,
        "{requests} requests: the attempts did not back off"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_reconnect_that_comes_back_finite_fails_as_resource_changed() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::from_fixture("sine-5s.mp3")),
    );
    let mut engine = start(&server);
    let event = engine.play_until_event(failed);
    let PlaybackEvent::Failed { cause, .. } = event else {
        unreachable!("the predicate above already matched Failed")
    };
    assert_eq!(cause, Some(RemoteFailure::ResourceChanged));
    engine.finish();
    server.shutdown();
}

#[test]
fn toggle_pauses_a_reconnect_and_play_during_one_changes_nothing() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::serving(Vec::new()).status(503)),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Play);
    engine.let_time_pass(Duration::from_millis(50));
    assert_eq!(engine.count_events(state(PlaybackState::Playing)), 0);

    engine.send(PlaybackCommand::TogglePause);
    engine.await_event(state(PlaybackState::Paused));
    let settled = server.requests().len();
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(
        server.requests().len(),
        settled,
        "attempts continued after Pause"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn sustained_playback_ends_the_outage_so_a_later_drop_gets_a_fresh_budget() {
    // Connections 1 and 2 are cut; 3 plays on.
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(station().truncate_body_after(CUT))
            .then(station()),
    );
    let mut engine = start_held(
        &server,
        ReconnectPolicy {
            stable_after: Duration::from_millis(500),
            ..quick()
        },
    );
    engine.run_network();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    let rejoined = engine.handle().progress().position;
    // Half a second of *played* audio ends the outage...
    engine.play_for(rejoined + Duration::from_millis(700));
    // ...so network time well past the 600 ms budget no longer matters.
    engine.advance_network(Duration::from_millis(700));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(
        engine.count_events(failed),
        0,
        "the second drop was judged against the first outage's budget"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_failure_during_an_outage_leaves_no_budget_behind_for_the_next_one() {
    // Connections 1 and 2 are cut; 3 plays on. Connection 2 is the one an
    // explicit Play opens out of `Failed`.
    let server = TestServer::start(
        station()
            .truncate_body_after(8 * 1024)
            .then(station().truncate_body_after(8 * 1024))
            .then(station()),
    );
    // Held, so the engine is still sitting in `Reconnecting` when the device
    // fault lands, rather than already back on connection 2.
    let policy = ReconnectPolicy {
        budget: Duration::from_millis(300),
        ..quick()
    };
    let mut engine = start_held(&server, policy);
    engine.play_until_event(state(PlaybackState::Reconnecting));

    // A fatal device fault: a failure that is none of the four paths which end
    // the listener's request, landing while the outage is open.
    engine.force_fatal_device_fault();
    engine.await_event(failed);

    // Network time past the budget, with the device clock frozen - so an
    // outage carried over from before would already be spent.
    engine.advance_network(Duration::from_millis(400));
    engine.run_network();
    // §9's one explicit reopen: connection 2, which then drops in its turn.
    engine.send(PlaybackCommand::Play);
    engine.await_event(state(PlaybackState::Playing));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(
        engine.count_events(failed),
        0,
        "the new outage was judged against a budget the failed session left behind"
    );
    engine.finish();
    server.shutdown();
}
