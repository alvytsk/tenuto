use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tenuto::clock::{Clock, FakeClock};
use tenuto::http::channel::{
    ByteChannel, HeaderOutcome, Outcome, ReadOutcome, SourceInterrupt, WaitHook,
};
use tenuto::http::error::{Operation, RemoteFailure};
use tenuto::http::response::{Accepted, FetchAccepted, Headers, Validator};

const STALL: Duration = Duration::from_secs(5);

/// Matches the 64-byte chunk `the_buffer_never_exceeds_its_capacity_and_the_producer_waits`
/// pushes, so that test's second push genuinely cannot fit. Every other push
/// in this file is a handful of bytes, so the same value is generous enough
/// there too.
const CAPACITY: usize = 64;

#[derive(Default)]
struct CountingHook(AtomicU32);

impl WaitHook for CountingHook {
    fn service(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl CountingHook {
    fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

struct NoHook;
impl WaitHook for NoHook {
    fn service(&self) {}
}

#[allow(clippy::unwrap_used)] // A current-thread runtime always builds here.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_read_returns_the_bytes_the_producer_pushed() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    runtime().block_on(channel.push(generation, b"hello"));

    let mut buffer = [0u8; 16];
    assert_eq!(
        channel.read(&mut buffer, &NoHook, STALL),
        ReadOutcome::Bytes(5)
    );
    assert_eq!(&buffer[..5], b"hello");
}

#[test]
fn a_read_never_reports_zero_bytes_for_an_empty_buffer() {
    // Symphonia reads `Ok(0)` as clean EOF. An empty buffer is not EOF, and
    // conflating them turns a stalled network into a silently truncated track.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();

    let reader = {
        let channel = channel.clone();
        std::thread::spawn(move || channel.read(&mut [0u8; 16], &NoHook, STALL))
    };
    // Give the reader time to be genuinely blocked, then satisfy it.
    std::thread::sleep(Duration::from_millis(50));
    runtime().block_on(channel.push(generation, b"x"));

    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Bytes(1)),
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn only_a_clean_finish_reports_eof() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    channel.finish(generation, Outcome::Eof);
    assert_eq!(
        channel.read(&mut [0u8; 4], &NoHook, STALL),
        ReadOutcome::Eof
    );
}

#[test]
fn a_failure_stays_a_failure_and_never_becomes_eof() {
    // H8. A truncated body that read back as EOF would be reported as a
    // completed track.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    let failure = RemoteFailure::TruncatedBody { missing: 42 };
    channel.finish(generation, Outcome::Failed(failure.clone()));
    assert_eq!(
        channel.read(&mut [0u8; 4], &NoHook, STALL),
        ReadOutcome::Failed(failure)
    );
}

#[test]
fn buffered_bytes_are_drained_before_a_pending_outcome_is_reported() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    runtime().block_on(channel.push(generation, b"tail"));
    channel.finish(generation, Outcome::Eof);

    let mut buffer = [0u8; 8];
    assert_eq!(
        channel.read(&mut buffer, &NoHook, STALL),
        ReadOutcome::Bytes(4)
    );
    assert_eq!(&buffer[..4], b"tail");
    assert_eq!(channel.read(&mut buffer, &NoHook, STALL), ReadOutcome::Eof);
}

#[test]
fn a_retirement_wakes_a_blocked_read_within_one_second() {
    // H9, and §8's stated bound. The server is never released: the wake must
    // come from the interrupt, not from bytes arriving.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let hook = Arc::new(CountingHook::default());

    let reader = {
        let channel = channel.clone();
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || {
            let started = Instant::now();
            let outcome = channel.read(&mut [0u8; 16], hook.as_ref(), STALL);
            (outcome, started.elapsed())
        })
    };
    // Prove the wait was entered before interrupting it: the hook runs only
    // from inside the wait loop, so a nonzero count is that proof. Sleeping
    // and hoping for the race is what §12 forbids.
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(
            entered.elapsed() < Duration::from_secs(5),
            "the read never blocked"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.retire();

    match reader.join() {
        Ok((outcome, elapsed)) => {
            assert_eq!(outcome, ReadOutcome::Retired);
            assert!(elapsed < Duration::from_secs(1), "woke after {elapsed:?}");
        }
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn a_freeze_never_errors_a_read_and_never_starves_one() {
    // H10, and §9's "service the freeze without returning a destructive read
    // error to the demuxer" — but note what the freeze does *not* do. Gating
    // delivery on it deadlocks reopening and seek refinement, both of which
    // are legitimate while playback is paused and both of which need bytes.
    // A pause parks the output; it does not starve the decoder.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    interrupt.freeze();
    runtime().block_on(channel.push(generation, b"ready"));

    let mut buffer = [0u8; 16];
    assert_eq!(
        channel.read(&mut buffer, &NoHook, STALL),
        ReadOutcome::Bytes(5),
        "a frozen read was starved; a seek taken while paused would hang here"
    );
    assert_eq!(&buffer[..5], b"ready");
}

#[test]
fn a_frozen_read_with_no_bytes_stays_pending_rather_than_erroring() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let hook = Arc::new(CountingHook::default());
    let generation = channel.generation();
    interrupt.freeze();

    let reader = {
        let channel = channel.clone();
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || channel.read(&mut [0u8; 16], hook.as_ref(), STALL))
    };
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(
            entered.elapsed() < Duration::from_secs(5),
            "the read never blocked"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!reader.is_finished(), "an empty frozen read returned early");
    runtime().block_on(channel.push(generation, b"later"));
    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Bytes(5)),
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn a_retirement_reaches_a_frozen_read_too() {
    // Quitting while paused must still wake every source wait (H10).
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let hook = Arc::new(CountingHook::default());
    interrupt.freeze();

    let reader = {
        let channel = channel.clone();
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || channel.read(&mut [0u8; 16], hook.as_ref(), STALL))
    };
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(
            entered.elapsed() < Duration::from_secs(5),
            "the read never blocked"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.retire();
    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Retired),
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn the_buffer_never_exceeds_its_capacity_and_the_producer_waits() {
    // H13. A fast server against a slow consumer must not accumulate.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    let chunk = [7u8; 64];

    let runtime = runtime();
    let pushed = {
        let channel = channel.clone();
        runtime.block_on(async move { channel.push(generation, &chunk).await })
    };
    assert!(pushed);
    assert_eq!(channel.buffered(), 64);

    // A second push cannot fit; it must wait rather than grow the buffer.
    let producer = {
        let channel = channel.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            rt.block_on(async move { channel.push(generation, &[9u8; 64]).await })
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        channel.buffered(),
        64,
        "the producer grew the buffer past its cap"
    );

    assert_eq!(
        channel.read(&mut [0u8; 64], &NoHook, STALL),
        ReadOutcome::Bytes(64)
    );
    match producer.join() {
        Ok(pushed) => assert!(pushed, "the producer never resumed after room appeared"),
        Err(_) => panic!("the producer thread panicked"),
    }
    assert_eq!(channel.buffered(), 64);
}

#[test]
fn a_stale_generation_can_neither_push_bytes_nor_end_the_stream() {
    // H9's second half: a superseded response's bytes and its outcome must
    // both be rejected before they can enter the new generation.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let stale = channel.generation();
    channel.retire();
    let fresh = interrupt.begin();
    assert_ne!(stale, fresh);

    let runtime = runtime();
    assert!(!runtime.block_on(channel.push(stale, b"stale")));
    channel.finish(stale, Outcome::Eof);
    channel.finish(
        stale,
        Outcome::Failed(RemoteFailure::Transport {
            operation: Operation::Read,
            detail: "old".into(),
        }),
    );
    assert_eq!(channel.buffered(), 0);

    runtime.block_on(channel.push(fresh, b"fresh"));
    let mut buffer = [0u8; 8];
    assert_eq!(
        channel.read(&mut buffer, &NoHook, STALL),
        ReadOutcome::Bytes(5)
    );
    assert_eq!(&buffer[..5], b"fresh");
}

#[test]
fn a_stop_after_begin_supersedes_the_operation_begin_opened() {
    // A seek opens generation N on the decode thread; a stop lands from the
    // application thread a moment later. The seek must lose, and must be able
    // to tell that it lost — this is what replaces the guarded `arm` that
    // every call site defeated by passing `generation()` back in.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let seek = interrupt.begin();
    assert!(interrupt.is_current(seek));

    interrupt.retire();
    assert!(
        !interrupt.is_current(seek),
        "the seek did not notice the stop"
    );
    assert!(interrupt.is_retired());

    // And the next operation opens cleanly, with a generation of its own.
    let reopen = interrupt.begin();
    assert_ne!(reopen, seek);
    assert!(interrupt.is_current(reopen));
    assert!(!interrupt.is_current(seek));
}

#[test]
fn a_retirement_wakes_a_blocked_producer_too() {
    // The first draft woke only the synchronous reader, leaving the producer
    // parked on its Notify and the fetch still running. A stop that does not
    // close the fetch is not a stop.
    let interrupt = SourceInterrupt::new(64);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    assert!(runtime().block_on(channel.push(generation, &[1u8; 64])));
    assert_eq!(channel.buffered(), 64);

    let producer = {
        let channel = channel.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            let started = Instant::now();
            let accepted = rt.block_on(async move { channel.push(generation, &[2u8; 64]).await });
            (accepted, started.elapsed())
        })
    };
    // The buffer is full, so the producer is provably parked.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(channel.buffered(), 64);
    // `buffered() == 64` alone is equally true if the producer thread never
    // even ran: prove it is actually parked on `producer_wake`, not merely
    // that nobody has grown the buffer yet.
    assert!(!producer.is_finished(), "the producer never parked");
    interrupt.retire();

    match producer.join() {
        Ok((accepted, elapsed)) => {
            assert!(!accepted, "a retired push reported success");
            assert!(
                elapsed < Duration::from_secs(1),
                "the producer woke after {elapsed:?}"
            );
        }
        Err(_) => panic!("the producer thread panicked"),
    }
}

#[test]
fn a_retirement_resolves_the_fetch_tasks_cancellation() {
    // The third waiter. Without it the request stays open after a stop and the
    // server goes on streaming into a buffer nobody will drain.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let generation = interrupt.generation();
    let woken = {
        let interrupt = Arc::clone(&interrupt);
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            let started = Instant::now();
            rt.block_on(interrupt.cancelled(generation));
            started.elapsed()
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    // Without this, a waiter that only entered `cancelled` after `retire()`
    // already returned would make the test pass green even with `fetch_wake`
    // entirely dead — the immediate-return path below would mask it.
    assert!(!woken.is_finished(), "the waiter never parked");
    interrupt.retire();
    match woken.join() {
        Ok(elapsed) => assert!(elapsed < Duration::from_secs(1), "woke after {elapsed:?}"),
        Err(_) => panic!("the waiter thread panicked"),
    }
    // And it resolves at once for a generation that is already superseded.
    runtime().block_on(interrupt.cancelled(generation));
}

#[test]
fn paused_time_is_not_charged_against_the_stall_budget() {
    // §8: "Time spent paused or waiting for local buffer space does not count
    // as a server stall." A deadline computed once at entry keeps running
    // through the pause and fails the very next read with a stall the server
    // never caused.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    let hook = Arc::new(CountingHook::default());
    let stall = Duration::from_millis(300);
    interrupt.freeze();

    let reader = {
        let channel = channel.clone();
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || channel.read(&mut [0u8; 16], hook.as_ref(), stall))
    };
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(
            entered.elapsed() < Duration::from_secs(5),
            "the read never blocked"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Stay frozen for several times the stall budget.
    std::thread::sleep(stall * 4);
    assert!(!reader.is_finished(), "the read timed out while paused");

    interrupt.thaw();
    runtime().block_on(channel.push(generation, b"resumed"));
    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Bytes(7)),
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn a_delivery_resets_the_stall_budget() {
    // A trickling server that keeps delivering is not stalled, however long the
    // whole transfer takes. §8: ordinary playback has no whole-response
    // deadline, so the budget must measure the gap between deliveries.
    // On a fake network clock the feeder steps by exactly the gap, so a
    // runner that oversleeps cannot turn a 150 ms gap into a 300 ms one.
    let clock = Arc::new(FakeClock::new());
    let interrupt = SourceInterrupt::with_clock(CAPACITY, Arc::clone(&clock) as Arc<dyn Clock>);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    let stall = Duration::from_millis(300);

    let feeder = {
        let channel = channel.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            for _ in 0..6 {
                std::thread::sleep(Duration::from_millis(20));
                clock.advance_monotonic(Duration::from_millis(150));
                rt.block_on(channel.push(generation, b"x"));
            }
        })
    };
    let mut seen = 0;
    let mut buffer = [0u8; 4];
    for _ in 0..6 {
        match channel.read(&mut buffer, &NoHook, stall) {
            ReadOutcome::Bytes(n) => seen += n,
            other => panic!("a trickle was reported as a stall: {other:?}"),
        }
    }
    assert_eq!(seen, 6);
    if feeder.join().is_err() {
        panic!("the feeder thread panicked");
    }
}

#[test]
fn a_stall_deadline_that_elapses_fails_rather_than_returning_eof() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let outcome = channel.read(&mut [0u8; 16], &NoHook, Duration::from_millis(100));
    assert_eq!(
        outcome,
        ReadOutcome::Failed(RemoteFailure::Timeout {
            phase: tenuto::http::error::Phase::Stall
        })
    );
}

// --- Ruling 1: the header handoff lives here, on the same lock and condvar
// as every other wait, because a private mutex/condvar for it would be a
// fourth waiter that `wake_all` never reaches. ---

fn sample_accepted() -> FetchAccepted {
    FetchAccepted {
        accepted: Accepted::Sequential { len: Some(100) },
        validator: Validator::default(),
        headers: Headers::from_pairs(&[]),
        redirects: 0,
    }
}

#[test]
fn a_published_header_outcome_is_returned_to_a_blocked_waiter() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let generation = interrupt.generation();
    let hook = Arc::new(CountingHook::default());
    let accepted = sample_accepted();

    let waiter = {
        let interrupt = Arc::clone(&interrupt);
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || interrupt.wait_for_headers(generation, hook.as_ref(), STALL))
    };
    // Prove the wait was entered before publishing, the same way every other
    // concurrency test here does: the hook only runs from inside the wait
    // loop, so a nonzero count is proof, not a guess.
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(
            entered.elapsed() < Duration::from_secs(5),
            "the wait never blocked"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.publish_headers(generation, Ok(accepted.clone()));

    match waiter.join() {
        Ok(outcome) => assert_eq!(outcome, Ok(accepted)),
        Err(_) => panic!("the waiter thread panicked"),
    }
}

#[test]
fn a_retirement_wakes_a_blocked_header_wait_within_one_second() {
    // The header wait is the fourth thing a retirement must reach. Nothing is
    // ever published for this generation — the wake has to come from the
    // interrupt, exactly as it does for the reader and the producer.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let generation = interrupt.generation();
    let hook = Arc::new(CountingHook::default());

    let waiter = {
        let interrupt = Arc::clone(&interrupt);
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || {
            let started = Instant::now();
            let outcome = interrupt.wait_for_headers(generation, hook.as_ref(), STALL);
            (outcome, started.elapsed())
        })
    };
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(
            entered.elapsed() < Duration::from_secs(5),
            "the wait never blocked"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.retire();

    match waiter.join() {
        Ok((outcome, elapsed)) => {
            assert_eq!(outcome, Err(HeaderOutcome::Retired));
            assert!(elapsed < Duration::from_secs(1), "woke after {elapsed:?}");
        }
        Err(_) => panic!("the waiter thread panicked"),
    }
}

#[test]
fn a_published_header_failure_is_returned_to_the_waiter() {
    // The failure half of the header handoff: Task 5 depends on this path to
    // report a rejected response rather than hanging or reporting success.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let generation = interrupt.generation();
    let failure = RemoteFailure::Status {
        status: 404,
        operation: Operation::Open,
    };
    interrupt.publish_headers(generation, Err(failure.clone()));
    assert_eq!(
        interrupt.wait_for_headers(generation, &NoHook, STALL),
        Err(HeaderOutcome::Failed(failure))
    );
}

#[test]
fn a_header_wait_deadline_that_elapses_fails_rather_than_hanging() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let generation = interrupt.generation();
    let outcome = interrupt.wait_for_headers(generation, &NoHook, Duration::from_millis(100));
    assert_eq!(
        outcome,
        Err(HeaderOutcome::Failed(RemoteFailure::Timeout {
            phase: tenuto::http::error::Phase::Headers
        }))
    );
}

#[test]
fn a_targeted_retirement_ignores_a_generation_begin_has_moved_past() {
    // `EngineHandle::submit_seek` retires the fetch that was live when it
    // enqueued the seek, so a worker blocked in a read wakes to service the
    // command. It reads that generation *before* the send and the worker may
    // dispatch the seek - and `begin()` the seek's own generation - before
    // the retirement lands. Aimed at the generation it observed, a late
    // retirement is a no-op; aimed at "whatever is current" it cancels the
    // very seek it was published for.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let observed = interrupt.begin();
    let seeks_own = interrupt.begin();

    assert!(
        !interrupt.retire_generation(observed),
        "a retirement aimed at a superseded generation must report doing nothing"
    );
    assert!(interrupt.is_current(seeks_own));
    assert!(!interrupt.is_retired());
    assert_eq!(interrupt.generation(), seeks_own);
}

#[test]
fn a_targeted_retirement_of_the_live_generation_wakes_a_blocked_read() {
    // The case the targeting must not weaken: the observed generation is
    // still the live one, so the retirement has to land exactly as an
    // untargeted `retire` would, and reach a reader blocked inside it.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let live = interrupt.begin();

    let reader = {
        let channel = channel.clone();
        std::thread::spawn(move || channel.read(&mut [0u8; 16], &NoHook, STALL))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(interrupt.retire_generation(live));
    assert!(interrupt.is_retired());
    assert!(!interrupt.is_current(live));

    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Retired),
        Err(_) => panic!("the reader thread panicked"),
    }
    assert!(
        !interrupt.retire_generation(live),
        "an already-retired generation is not retired twice"
    );
}

/// Steps a fake network clock past the stall budget on its third slice.
struct StepsTheClock {
    clock: Arc<FakeClock>,
    slices: AtomicU32,
}

impl WaitHook for StepsTheClock {
    fn service(&self) {
        if self.slices.fetch_add(1, Ordering::Relaxed) == 2 {
            self.clock.advance_monotonic(STALL);
        }
    }
}

#[test]
fn the_stall_budget_is_charged_on_the_network_clock() {
    // M9.5: real slices pass while the fake clock holds, and charge nothing;
    // the step charges the whole budget at once.
    let clock = Arc::new(FakeClock::new());
    let interrupt = SourceInterrupt::with_clock(CAPACITY, Arc::clone(&clock) as Arc<dyn Clock>);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let hook = StepsTheClock {
        clock,
        slices: AtomicU32::new(0),
    };
    let started = Instant::now();
    let mut buffer = [0u8; 16];
    assert_eq!(
        channel.read(&mut buffer, &hook, STALL),
        ReadOutcome::Failed(RemoteFailure::Timeout {
            phase: tenuto::http::error::Phase::Stall
        })
    );
    assert_eq!(hook.slices.load(Ordering::Relaxed), 3);
    assert!(started.elapsed() < STALL);
}
