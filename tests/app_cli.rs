//! CLI-level acceptance evidence for IMPORTANT 2 (final review): the shipped
//! key-handling loop must actually use the out-of-band submission API §8
//! built for it, not the blocking command queue every command but
//! `Stop`/`Shutdown` used to travel on. `engine_remote.rs`'s
//! `pause_during_a_stalled_read_freezes_output_and_resume_continues` already
//! proves the *engine* can deliver a pause to a blocked read promptly; this
//! proves `app::KeyRouter` - the exact routing a keypress takes - is what
//! actually reaches it, with no tty and no crossterm event in the loop at all.
//!
//! `engine.drain_ring_without_advancing_clock()`, not a short `play_for`, is
//! what makes the worker's read genuinely blocked at the point each test acts
//! (verified by hand): `sine-5s.flac`'s first 32 KiB decode to a little over a
//! second of audio, comfortably more than a short `play_for` consumes, so the
//! worker's ring stays topped up from data already buffered before the stall
//! and a queued command dispatches promptly regardless of which routing is
//! under test - proving nothing about the routing at all. Draining the ring
//! all the way to silence is what forces the worker to actually need bytes
//! the stalled server will never send.

mod support;

use std::time::{Duration, Instant};

use tenuto::app::KeyRouter;
use tenuto::http::limits::Limits;
use tenuto::playback::command::{Admission, PlaybackCommand};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::state::PlaybackState;

use support::TestEngine;
use support::server::{Script, TestServer};

#[test]
fn a_pause_routed_through_the_cli_reaches_a_stalled_read_promptly() {
    // A stall much longer than a working pause has any business taking, so a
    // regression back to the blocking command queue - which would only let
    // the worker dispatch `Pause` once the stall itself timed out - fails
    // this test loudly and quickly, rather than by chance timing.
    let patient_limits = Limits {
        stall: Duration::from_secs(20),
        ..Limits::brisk()
    };
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_limits(&server.url("/audio.flac"), patient_limits);
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the read never blocked"
    );
    // Forces the ring empty, which is what makes the worker's read genuinely
    // blocked rather than merely idling on data already buffered before the
    // stall - see this file's own doc comment for why a short `play_for`
    // alone does not prove this.
    engine.drain_ring_without_advancing_clock();

    // The routing under test: exactly what `handle_keys` does with a
    // decoded `TogglePause` while the mirror shows `Playing`, with no
    // crossterm event and no terminal anywhere in this test.
    let issued = Instant::now();
    KeyRouter::new().route(
        &engine.handle(),
        Duration::ZERO,
        None,
        Instant::now(),
        PlaybackCommand::TogglePause,
    );
    engine.await_state(PlaybackState::Paused);
    let elapsed = issued.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "the pause took {elapsed:?} to land - it waited out the stall instead of \
         reaching it out of band"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn an_arrow_key_seek_routed_through_the_cli_retires_a_stalled_fetch_promptly() {
    // IMPORTANT 2's other half: a `SeekBy` queued the old way published no
    // `SEEK` interrupt and retired no fetch, so a seek issued while a read
    // was already blocked waited out that read instead of replacing it -
    // exactly the case the `SEEK` bit exists for.
    let patient_limits = Limits {
        stall: Duration::from_secs(20),
        ..Limits::brisk()
    };
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_limits(&server.url("/audio.flac"), patient_limits);
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the read never blocked"
    );
    engine.drain_ring_without_advancing_clock();

    let requests_before = server.requests().len();
    let position = engine.progress().position;
    let issued = Instant::now();
    // `SeekBy(3)` from the mirror's position, exactly as the right-arrow key
    // resolves it in `to_command`. The press only accumulates; `flush` past
    // the quiet window is what submits it, so both halves of what one arrow
    // key now does are exercised here.
    let mut router = KeyRouter::new();
    let pressed = Instant::now();
    router.route(
        &engine.handle(),
        position,
        None,
        pressed,
        PlaybackCommand::SeekBy(3),
    );
    router.flush(&engine.handle(), pressed + Duration::from_secs(1));

    // Proof the stalled fetch was actually retired and replaced, rather than
    // the seek merely being queued behind it: a fresh request reaching the
    // server, well inside the stall's own timeout.
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.requests().len() <= requests_before {
        assert!(
            Instant::now() < deadline,
            "the routed seek never replaced the stalled fetch - it waited out \
             the stall instead of reaching it out of band"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        issued.elapsed() < Duration::from_secs(2),
        "the routed seek took {:?} to reach the server",
        issued.elapsed()
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_burst_of_arrow_presses_costs_one_seek_rather_than_one_per_press() {
    // The thrash this coalescing exists for. Before the burst, each press
    // submitted its own `SeekTo` - every one of them retiring the fetch its
    // predecessor had just started - and because all four resolved against
    // the same frozen mirror position, the four of them asked for the very
    // same target: four seeks, four reopens, and one step of movement to show
    // for it.
    //
    // Counted in `SeekCompleted` events rather than server requests on
    // purpose: one Coarse seek is already worth many range requests on its
    // own - a seektable-less FLAC bisects the file - so the request count
    // cannot tell one seek from four.
    let server = TestServer::start(Script::from_fixture("sine-5s.mp3"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.mp3"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    let position = engine.progress().position;
    // Four one-second steps have to land inside a five-second fixture:
    // symphonia refuses a seek past the last frame outright, and a rejection
    // would be indistinguishable here from the coalescing having worked.
    assert!(
        position < Duration::from_millis(500),
        "playback reached {position:?} before the burst; four forward steps \
         from there would be refused for running off the end of the media \
         rather than measuring anything about coalescing"
    );

    let mut router = KeyRouter::new();
    let pressed = Instant::now();
    for _ in 0..4 {
        router.route(
            &engine.handle(),
            position,
            None,
            pressed,
            PlaybackCommand::SeekBy(1),
        );
    }

    // Held, not submitted: the whole point of the quiet window is that
    // nothing starts buffering until the listener stops pressing.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        seeks_completed(&mut engine),
        Vec::<Duration>::new(),
        "a press reached the engine on its own - the window did not hold it"
    );

    router.flush(&engine.handle(), pressed + Duration::from_secs(1));

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut completed = Vec::new();
    while completed.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the flushed burst never reached the engine"
        );
        completed.extend(seeks_completed(&mut engine));
        std::thread::sleep(Duration::from_millis(5));
    }

    // Four presses of one second, composed onto each other rather than each
    // resolved against the position none of them could have moved.
    assert_eq!(completed, vec![position + Duration::from_secs(4)]);

    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        seeks_completed(&mut engine),
        Vec::<Duration>::new(),
        "a second seek followed the first - the burst did not collapse"
    );

    engine.finish();
    server.shutdown();
}

/// The targets of every `SeekCompleted` waiting in the harness's inbox,
/// draining them so a later call reports only what arrived since.
fn seeks_completed(engine: &mut TestEngine) -> Vec<Duration> {
    let mut targets = Vec::new();
    while let Some(event) = engine.try_event() {
        if let PlaybackEvent::SeekCompleted { requested, .. } = event {
            targets.push(requested);
        }
    }
    targets
}
