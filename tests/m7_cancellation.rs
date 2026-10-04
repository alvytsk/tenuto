//! M7 §6, L8: every cancelling command, in every situation a live source can
//! be blocked in. "No request" means none for the cancelled operation; a
//! replacement Load issues its own, to its own server.
//!
//! Automatic recovery exists only while an explicit Play is current. Pause,
//! Stop, a replacement Load and Shutdown must each end it *at once* — not
//! after the attempt in flight finishes, and not after a deadline expires
//! under it. Every cell therefore measures from the cancel, and every
//! arrangement proves it is still blocked when the cancel lands.

mod support;

use std::time::{Duration, Instant};

use url::Url;

use support::TestEngine;
use support::server::{Script, TestServer};
use tenuto::http::limits::Limits;
use tenuto::media::id::{MediaId, NormalizedUrl};
use tenuto::media::source::SourceLocation;
use tenuto::playback::command::{Admission, LoadRequestId, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;

/// About six seconds of this 64 kbps looping fixture: long enough that a
/// connection which plays its cut out never ends the outage by itself.
const CUT: usize = 48 * 1024;

/// The bound every cell holds the engine to, measured from the cancel and
/// nothing else. The handshake deadline (250 ms) plus scheduling slack.
const PROMPT: Duration = Duration::from_millis(450);

/// Where a connection has to stop for the worker to be stuck *inside
/// priming*. Measured against this station fixture: 1024–2560 bytes parks
/// `open_transport`'s priming read, 3072 parks the probe inside `prepare`,
/// and 4096 or more completes the open outright.
const PRIMING_STALL: usize = 2048;

/// Where `Situation::StalledBody`'s connection stops feeding: a second of
/// this 64 kbps fixture, comfortably past the 4 KiB that completes an open
/// and comfortably short of the playback `arrange` then demands of it.
const STALL_AT: usize = 8 * 1024;

#[derive(Clone, Copy, Debug)]
enum Situation {
    Backoff,
    AwaitingHeaders,
    StalledBody,
    DeliveringBody,
    Priming,
}

#[derive(Clone, Copy, Debug)]
enum Cancel {
    Pause,
    Stop,
    Load,
    Shutdown,
}

fn station() -> Script {
    Script::from_fixture("sine-noxing.mp3").icy_station()
}

/// Deadlines far longer than any window measured here, on purpose: what every
/// cell proves is a command waking a blocked worker, not a timeout expiring
/// under it. Under `Limits::brisk()` a stalled header wait (500 ms), a
/// stalled body read (500 ms) and an unfinished open (2 s) all end themselves
/// well inside a cell's own patience — so three of the five situations would
/// silently become a *different* one (a `Timeout{Stall}` is a disconnect,
/// which is a reconnect) and the cells would assert nothing.
fn patient() -> Limits {
    Limits {
        headers: Duration::from_secs(7),
        stall: Duration::from_secs(9),
        open: Duration::from_secs(13),
        ..Limits::brisk()
    }
}

fn script(situation: Situation) -> Script {
    let cut = station().truncate_body_after(CUT);
    match situation {
        // Refused forever, with a backoff long enough to be standing in.
        Situation::Backoff => cut.then(Script::serving(Vec::new()).status(503)),
        Situation::AwaitingHeaders => cut.then(station().stall_headers()),
        Situation::StalledBody => station().stall_body_after(STALL_AT),
        Situation::DeliveringBody => station(),
        // Probes, then stalls before a frame can be primed.
        Situation::Priming => cut.then(station().stall_body_after(PRIMING_STALL)),
    }
}

fn policy(situation: Situation) -> ReconnectPolicy {
    // The first step is always brisk. `Situation::Backoff` needs its first
    // attempt made and refused, so that the long wait the cancel then lands
    // in is a real backoff with a failure behind it rather than the very
    // first gap after the disconnect.
    let mut backoff = [Duration::from_millis(20); 5];
    if matches!(situation, Situation::Backoff) {
        for step in backoff.iter_mut().skip(1) {
            *step = Duration::from_secs(30);
        }
    }
    ReconnectPolicy {
        backoff,
        budget: Duration::from_secs(60),
        stable_after: Duration::from_secs(30),
    }
}

fn is(state: PlaybackState) -> impl Fn(&PlaybackEvent) -> bool {
    move |event| matches!(event, PlaybackEvent::StateChanged { state: s, .. } if *s == state)
}

/// A replacing `Load`, submitted exactly as the application submits one —
/// through `EngineHandle::submit`, which is what retires the source the load
/// replaces — and returning at once rather than waiting for the outcome.
///
/// `TestEngine::load_remote_as` is the same submission with a wait for the
/// replacement's own `Paused` bolted on, and that wait is the whole of the
/// new source's open: connect, probe, decode a FLAC header. None of that is
/// work the cancel is answerable for, so it must not be inside the interval
/// a `Cancel::Load` cell measures.
fn submit_load(engine: &TestEngine, request: LoadRequestId, url: &str) {
    let parsed = match Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => panic!("test URL {url:?} must parse: {error}"),
    };
    let media = match NormalizedUrl::parse(url) {
        Ok(normalized) => MediaId::RemoteUrl(normalized),
        Err(error) => panic!("test URL {url:?} must normalize: {error}"),
    };
    let admission = engine.handle().submit(PlaybackCommand::Load {
        request,
        media,
        source: SourceLocation::Http(parsed),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    });
    assert_eq!(
        admission,
        Admission::Accepted,
        "the engine must accept the replacing load"
    );
}

/// The instant `server` received its first request. Where a `Cancel::Load`
/// cell's interval ends: the worker can only have sent it after it stopped
/// waiting on whatever the load replaced.
fn first_request_reached(server: &TestServer) -> Instant {
    let deadline = Instant::now() + Duration::from_secs(15);
    while server.requests().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the replacing load never left for its own server"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    Instant::now()
}

fn arrange(situation: Situation) -> (TestServer, TestEngine) {
    let server = TestServer::start(script(situation));
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(policy(situation));
    let request = engine.load_remote_with_limits(&server.url("/radio"), patient());
    // The load's own `Paused`, consumed here before anything else can happen.
    // Left in the inbox it would answer a `Cancel::Pause` cell's wait in
    // microseconds and the cell would test nothing at all.
    engine.await_event(is(PlaybackState::Paused));
    // `PlayLoaded`, never a plain `Play`: a plain `Play` on a station opens a
    // *fresh* connection, which would cost every request count below an extra
    // connection before the test began.
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(is(PlaybackState::Playing));
    match situation {
        Situation::DeliveringBody => {
            engine.play_for(Duration::from_millis(200));
            assert!(
                engine.handle().progress().position > Duration::ZERO,
                "nothing was being delivered"
            );
            assert_eq!(server.requests().len(), 1);
        }
        Situation::StalledBody => {
            // Drain what the stalled body supplied, then run the clock past
            // everything it can ever supply without a round trip a parked
            // worker could not answer: only then is the worker genuinely
            // inside a byte-channel read rather than sitting on a full output
            // ring, which the frozen device clock would otherwise leave it on.
            // `STALL_AT` is a second of this fixture, and 1.7 s of playback is
            // demanded of it, so the margin is the better part of a second
            // rather than the hundred milliseconds a stall point three times
            // further in would leave.
            engine.play_for(Duration::from_millis(200));
            engine.let_time_pass_while_unresponsive(Duration::from_millis(1500));
            assert!(
                server.wait_until_stalled(Duration::from_secs(5)),
                "the read never blocked"
            );
            assert_eq!(
                server.requests().len(),
                1,
                "the stall became a disconnect and a reconnect: this is no longer a stalled read"
            );
        }
        Situation::Backoff => {
            engine.play_until_event(is(PlaybackState::Reconnecting));
            // Let the first (20 ms) attempt be made and refused; the next one
            // is then half a minute away.
            let deadline = Instant::now() + Duration::from_secs(5);
            while server.requests().len() < 2 && Instant::now() < deadline {
                engine.let_time_pass(Duration::from_millis(10));
            }
            assert_eq!(
                server.requests().len(),
                2,
                "the first attempt was never made and refused"
            );
        }
        Situation::AwaitingHeaders | Situation::Priming => {
            engine.play_until_event(is(PlaybackState::Reconnecting));
            assert!(
                server.wait_until_stalled(Duration::from_secs(5)),
                "the attempt never reached the point it is meant to stick at"
            );
            assert_eq!(server.requests().len(), 2, "the attempt was never made");
            if matches!(situation, Situation::Priming) {
                // Real time for the worker to finish probing and reach the
                // priming read it cannot complete. Nothing is asked of the
                // worker here, because it is exactly the thing meant to be
                // stuck.
                engine.let_time_pass_while_unresponsive(Duration::from_secs(1));
            }
        }
    }
    (server, engine)
}

fn run(situation: Situation, cancel: Cancel) {
    let (server, mut engine) = arrange(situation);
    let before = server.requests().len();
    // The window is asserted rather than assumed: no `Playing` has been
    // announced since the one `arrange` consumed, so the attempt (or the
    // connection) the cancel is about to land on is genuinely unfinished.
    // This is also the baseline the count after the cancel is read against.
    assert_eq!(
        engine.count_events(is(PlaybackState::Playing)),
        0,
        "{situation:?}: the situation resolved itself before the cancel landed"
    );
    let other = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let started = Instant::now();
    let ended = match cancel {
        Cancel::Pause => {
            engine.handle().submit(PlaybackCommand::Pause);
            engine.await_event(is(PlaybackState::Paused));
            Instant::now()
        }
        Cancel::Stop => {
            engine.interrupt_stop();
            engine.await_event(is(PlaybackState::Stopped));
            Instant::now()
        }
        Cancel::Load => {
            let request = engine.next_request();
            submit_load(&engine, request, &other.url("/b.flac"));
            let reached = first_request_reached(&other);
            // Then let the replacement finish, outside the measurement, so
            // the checks below run against a settled engine. The
            // arrangement's own `Paused` was consumed in `arrange` and
            // nothing has paused since, so only this load can answer.
            engine.await_event(is(PlaybackState::Paused));
            reached
        }
        Cancel::Shutdown => {
            // Interrupts, joins the worker and hands back what it captured.
            // The handle is taken with it, so `finish()` below is a no-op and
            // nothing may touch `engine.handle()` afterwards.
            assert!(
                engine.shutdown_report().is_some(),
                "the worker never handed its report back"
            );
            Instant::now()
        }
    };
    let took = ended.duration_since(started);
    assert!(
        took < PROMPT,
        "{situation:?}/{cancel:?}: took {took:?}; requests {} -> {}",
        before,
        server.requests().len()
    );
    if !matches!(cancel, Cancel::Shutdown) {
        engine.let_time_pass(Duration::from_millis(150));
    }
    assert_eq!(
        server.requests().len(),
        before,
        "{situation:?}/{cancel:?}: the cancelled operation issued another request"
    );
    // Counted against the zero established before the cancel. A replacing
    // Load is held to this too: it ends at `Paused` on its own media, so a
    // `Playing` here could only be the attempt the cancel was supposed to
    // have ended. Only a shutdown is exempt, and only because its worker is
    // already gone and what it captured on the way out goes to its report
    // rather than to the inbox this counts.
    if !matches!(cancel, Cancel::Shutdown) {
        assert_eq!(
            engine.count_events(is(PlaybackState::Playing)),
            0,
            "{situation:?}/{cancel:?}: Playing announced after the cancel"
        );
    }
    engine.finish();
    server.release();
    server.shutdown();
    other.shutdown();
}

macro_rules! cell {
    ($name:ident, $situation:ident, $cancel:ident) => {
        #[test]
        fn $name() {
            run(Situation::$situation, Cancel::$cancel);
        }
    };
}

cell!(backoff_pause, Backoff, Pause);
cell!(backoff_stop, Backoff, Stop);
cell!(backoff_load, Backoff, Load);
cell!(backoff_shutdown, Backoff, Shutdown);
cell!(headers_pause, AwaitingHeaders, Pause);
cell!(headers_stop, AwaitingHeaders, Stop);
cell!(headers_load, AwaitingHeaders, Load);
cell!(headers_shutdown, AwaitingHeaders, Shutdown);
cell!(stalled_pause, StalledBody, Pause);
cell!(stalled_stop, StalledBody, Stop);
cell!(stalled_load, StalledBody, Load);
cell!(stalled_shutdown, StalledBody, Shutdown);
cell!(delivering_pause, DeliveringBody, Pause);
cell!(delivering_stop, DeliveringBody, Stop);
cell!(delivering_load, DeliveringBody, Load);
cell!(delivering_shutdown, DeliveringBody, Shutdown);
cell!(priming_pause, Priming, Pause);
cell!(priming_stop, Priming, Stop);
cell!(priming_load, Priming, Load);
cell!(priming_shutdown, Priming, Shutdown);
