//! The bounded encoded-byte channel between one asynchronous fetch task and
//! the synchronous decoder, plus the out-of-band interrupt that reaches both.
//!
//! Three wake channels, because three different kinds of waiter must be
//! reachable: the decoder thread (a `Condvar` — it must not spin), the
//! producer task inside `push` (a `Notify` — it must not block a runtime
//! worker), and the fetch task's own header and body awaits (a second
//! `Notify` — a retirement has to close the request, not merely stop feeding
//! it). All three hang off one `Mutex`, and every flag change is made under
//! that `Mutex` before notifying, which is what makes a lost wake impossible.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::clock::{Clock, SystemClock};

use super::error::{Phase, RemoteFailure};
use super::response::FetchAccepted;

/// How long a single wait slice lasts before the hook runs again. Short enough
/// that position and checkpoints stay current while the network is quiet;
/// long enough that a stalled read is not a spin loop.
const SLICE: Duration = Duration::from_millis(20);

/// What a blocked source read may do on the worker's behalf while it waits.
///
/// Deliberately a bare `service()`: an implementation is handed only shared
/// state, so it structurally cannot re-enter a decoder read or seek — the
/// property §8 states as a rule is here a consequence of the type.
pub trait WaitHook: Send + Sync {
    fn service(&self);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The body ended exactly where it said it would.
    Eof,
    Failed(RemoteFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadOutcome {
    /// Never zero for a non-empty request — a zero-length request is
    /// answered with zero. Symphonia reads a zero-byte result as EOF, so a
    /// nonzero request must never be answered with `Bytes(0)`.
    Bytes(usize),
    Eof,
    /// Stop, seek or shutdown retired this read. Not a failure and not EOF.
    Retired,
    Failed(RemoteFailure),
}

/// The outcome of waiting for a generation's headers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeaderOutcome {
    /// Stop, seek or shutdown retired the request. Not a failure.
    Retired,
    Failed(RemoteFailure),
}

#[derive(Debug)]
struct State {
    bytes: VecDeque<u8>,
    capacity: usize,
    outcome: Option<Outcome>,
    /// Set by the fetch task once headers are validated, or once they fail.
    /// Cleared by `retire` and `begin`, like everything else belonging to a
    /// generation.
    headers: Option<Result<FetchAccepted, RemoteFailure>>,
    /// Bumped by `retire`. A push or a finish carrying an older value belongs
    /// to a superseded response and is discarded before it can enter the
    /// buffer.
    generation: u64,
    /// One-shot: this generation is over. `begin` clears it for the next one.
    retired: bool,
    /// A *level*, not an edge. A pause persists until a play, and a read that
    /// blocks after the edge would have passed must still observe it (G2).
    frozen: bool,
    /// An absolute deadline for one operation, checked inside `read`'s wait
    /// loop regardless of `frozen` — see `SourceInterrupt::
    /// set_operation_deadline`.
    ///
    /// Deliberately **not** cleared by `retire` or `begin`, unlike every
    /// other field here: one `seek_refined` call can span more than one
    /// generation internally (`HttpMediaSource::seek`'s own `Seek::seek`
    /// impl calls `begin` every time it re-requests a byte range), and the
    /// deadline has to bound the *whole* seek, not just the first byte-level
    /// leg of it. Its one setter (the engine's seek routing) sets this
    /// before the call and clears it back to `None` itself once the whole
    /// call returns, success or failure alike — that is the only place this
    /// field is ever written to `None` again.
    operation_deadline: Option<Instant>,
}

/// The out-of-band wake shared by the application, the worker, every source
/// wait and the fetch task.
///
/// Also the network's clock (M9.5): every budget a wait or the fetch task
/// charges - stall, headers, the operation deadline, the opening deadline -
/// reads time from `clock`, so a test can hold the network still or step it
/// past a budget instead of racing wall time. Waits still slice on real time;
/// each slice re-reads the clock, so an advanced fake clock is seen within one.
pub struct SourceInterrupt {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    reader_wake: Condvar,
    producer_wake: Notify,
    /// Wakes the fetch task's header and body awaits. Waking the reader alone
    /// leaves the request open and the server still streaming into a buffer
    /// nobody will drain.
    fetch_wake: Notify,
}

/// A poisoned lock means a thread already panicked while holding it; there is
/// nothing better to do than carry on with the state it left.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl std::fmt::Debug for SourceInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceInterrupt")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl SourceInterrupt {
    pub fn new(capacity: usize) -> Arc<Self> {
        Self::with_clock(capacity, Arc::new(SystemClock))
    }

    pub fn with_clock(capacity: usize, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            state: Mutex::new(State {
                bytes: VecDeque::with_capacity(capacity.min(1 << 16)),
                capacity: capacity.max(1),
                outcome: None,
                headers: None,
                generation: 1,
                retired: false,
                frozen: false,
                operation_deadline: None,
            }),
            reader_wake: Condvar::new(),
            producer_wake: Notify::new(),
            fetch_wake: Notify::new(),
        })
    }

    /// End the current generation: drop the buffer and any pending outcome,
    /// and wake all three kinds of waiter.
    ///
    /// Returns nothing, deliberately. `retire` bumps the generation but also
    /// leaves `retired` set, so the bumped value is dead on arrival for
    /// anyone who might try to carry it forward: `push` rejects it on the
    /// retired check and `is_current` reports it superseded. Only `begin`'s
    /// return value is a generation a caller may use — the next operation
    /// calls `begin` for it, exactly as this module's own tests do.
    pub fn retire(&self) {
        {
            let mut state = lock(&self.state);
            Self::retire_locked(&mut state);
        }
        self.wake_all();
    }

    /// `retire`, aimed at one generation: ends `generation` only if it is
    /// still the live one, and reports whether it did.
    ///
    /// For a caller that publishes a retirement out of band from the command
    /// queue and cannot know what the worker has done since - `EngineHandle::
    /// submit_seek` reads the live generation, enqueues `SeekTo`, and only
    /// then retires, so a worker blocked in a remote read wakes to service
    /// the command. Preempted between the send and the retirement, it can
    /// land after the worker has already dispatched the seek and `begin`-ed
    /// the seek's own generation; an unaimed `retire` there cancels the very
    /// seek it was published for (CI on main, 2026-09-17: `SeekCancelled`
    /// nothing had asked for). Aimed at the generation observed before the
    /// send, the late retirement finds it superseded and does nothing, which
    /// is exactly right: the worker is no longer blocked in anything that
    /// needs waking.
    pub fn retire_generation(&self, generation: u64) -> bool {
        {
            let mut state = lock(&self.state);
            if state.generation != generation || state.retired {
                return false;
            }
            Self::retire_locked(&mut state);
        }
        self.wake_all();
        true
    }

    fn retire_locked(state: &mut State) {
        state.bytes.clear();
        state.outcome = None;
        state.headers = None;
        state.retired = true;
        state.generation += 1;
    }

    /// Open a new generation: bump the counter, clear the retirement, and
    /// return the generation the caller must carry.
    ///
    /// **Always moves forward, and takes no generation argument.** An earlier
    /// draft had `arm(generation) -> bool`, guarded against a stale caller —
    /// and every call site then passed `interrupt.generation()`, which is
    /// whatever is current *including a newer stop's*, so the guard could
    /// never fire and the whole thing was decoration. A guard a caller can
    /// trivially satisfy by reading the value back is not a guard.
    ///
    /// Monotonic beats guarded here. A stop that lands after `begin` bumps
    /// again and sets `retired`, so the operation `begin` opened finds its
    /// pushes rejected on generation mismatch and its reads returning
    /// `Retired`. The operation loses the race and knows it, which is what
    /// `is_current` is for.
    pub fn begin(&self) -> u64 {
        let generation = {
            let mut state = lock(&self.state);
            state.generation += 1;
            state.retired = false;
            state.bytes.clear();
            state.outcome = None;
            state.headers = None;
            state.generation
        };
        self.wake_all();
        generation
    }

    /// Whether the generation this caller was admitted under is still the live
    /// one. Every operation that spans a wait re-checks before committing.
    pub fn is_current(&self, generation: u64) -> bool {
        let state = lock(&self.state);
        state.generation == generation && !state.retired
    }

    pub fn freeze(&self) {
        lock(&self.state).frozen = true;
        self.wake_all();
    }

    pub fn thaw(&self) {
        lock(&self.state).frozen = false;
        self.wake_all();
    }

    /// An absolute deadline for one operation, checked inside `read`'s wait
    /// loop **regardless of the freeze level**.
    ///
    /// Distinct from the stall budget on purpose. The stall budget is
    /// suspended while frozen — deliberately, so a pause is never reported as
    /// a server stall — which means a paused, stalled read waits forever. A
    /// deadline expressed as "remaining time, passed as a stall budget"
    /// inherits that suspension and bounds nothing at all.
    ///
    /// `None` clears it. A wake is not needed to make a set or a clear take
    /// effect: `read`'s wait loop re-checks every `SLICE` regardless, the
    /// same way it already re-checks `frozen` and the stall budget without
    /// being woken for either.
    pub fn set_operation_deadline(&self, deadline: Option<Instant>) {
        lock(&self.state).operation_deadline = deadline;
    }

    /// The network clock's monotonic hand.
    pub fn now(&self) -> Instant {
        self.clock.sample().monotonic
    }

    /// Resolves once `span` has passed on the network clock. Polls it every
    /// `SLICE`, so a fake clock stepped past the span is seen within one.
    pub async fn sleep(&self, span: Duration) {
        let until = self.now() + span;
        while self.now() < until {
            tokio::time::sleep(SLICE).await;
        }
    }

    pub fn is_retired(&self) -> bool {
        lock(&self.state).retired
    }

    pub fn is_frozen(&self) -> bool {
        lock(&self.state).frozen
    }

    pub fn generation(&self) -> u64 {
        lock(&self.state).generation
    }

    fn wake_all(&self) {
        self.reader_wake.notify_all();
        self.producer_wake.notify_waiters();
        self.fetch_wake.notify_waiters();
    }

    /// Async cancellation for the fetch task. Resolves as soon as the current
    /// generation is retired, and never resolves otherwise.
    pub async fn cancelled(&self, generation: u64) {
        loop {
            // Create the future before testing, so a notify that lands between
            // the test and the await is still delivered.
            let notified = self.fetch_wake.notified();
            {
                let state = lock(&self.state);
                if state.retired || state.generation != generation {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Hand the validated response — or the failure that replaced it — to the
    /// thread blocked in `wait_for_headers`. A stale generation's outcome is
    /// discarded, exactly as `finish` discards a stale body outcome.
    pub fn publish_headers(&self, generation: u64, outcome: Result<FetchAccepted, RemoteFailure>) {
        {
            let mut state = lock(&self.state);
            if state.generation != generation || state.retired || state.headers.is_some() {
                return;
            }
            state.headers = Some(outcome);
        }
        // Only the synchronous header wait below consumes this: it hangs off
        // the same condvar `read` uses, not off either `Notify`.
        self.reader_wake.notify_all();
    }

    /// Block until this generation's headers are validated, fail, or the
    /// source is retired.
    ///
    /// The same slice-and-service loop `ByteChannel::read` uses, on the same
    /// condvar, for the same reason: a retirement has to wake this wait, and
    /// only waits hanging off `wake_all` are woken.
    pub fn wait_for_headers(
        &self,
        generation: u64,
        service: &dyn WaitHook,
        deadline: Duration,
    ) -> Result<FetchAccepted, HeaderOutcome> {
        let start = self.now();
        let mut state = lock(&self.state);
        loop {
            if state.retired || state.generation != generation {
                return Err(HeaderOutcome::Retired);
            }
            if let Some(outcome) = state.headers.clone() {
                return outcome.map_err(HeaderOutcome::Failed);
            }
            // Unlike `read`'s stall budget, this is plain elapsed time, not
            // active demand: a freeze cannot arrive before the source that
            // would be frozen exists, so there is no paused interval to
            // exclude here.
            if self.now().duration_since(start) >= deadline {
                return Err(HeaderOutcome::Failed(RemoteFailure::Timeout {
                    phase: Phase::Headers,
                }));
            }
            let (guard, _) = match self.reader_wake.wait_timeout(state, SLICE) {
                Ok(pair) => pair,
                Err(poisoned) => poisoned.into_inner(),
            };
            state = guard;
            // Dropped across the call and re-taken after, exactly as `read`
            // does, and for the same reason: the hook may itself need a lock
            // this interrupt's holder must not be seen to hold.
            drop(state);
            service.service();
            state = lock(&self.state);
        }
    }
}

/// A handle onto the interrupt's buffer. Holds no state of its own — it is a
/// thin `Clone` wrapper over the same `Arc<SourceInterrupt>` its owner holds —
/// so there is no second lock and no way for the two to disagree.
#[derive(Clone, Debug)]
pub struct ByteChannel(Arc<SourceInterrupt>);

impl ByteChannel {
    pub fn new(interrupt: Arc<SourceInterrupt>) -> Self {
        Self(interrupt)
    }

    pub fn interrupt(&self) -> &Arc<SourceInterrupt> {
        &self.0
    }

    pub fn generation(&self) -> u64 {
        self.0.generation()
    }

    pub fn buffered(&self) -> usize {
        lock(&self.0.state).bytes.len()
    }

    pub fn retire(&self) {
        self.0.retire();
    }

    /// Wait for bytes, an ending or a retirement.
    ///
    /// `stall` is a budget of *active demand*, not a fixed deadline: only
    /// slices spent unfrozen are charged against it, and any delivery resets
    /// it. A fixed `Instant::now() + stall` computed once — which is what this
    /// first did — expires during a long pause and fails the very next read
    /// with a server stall the server never caused.
    ///
    /// Buffered bytes are always drained before a pending outcome is reported,
    /// so a body that ends mid-buffer still plays what it delivered.
    pub fn read(&self, out: &mut [u8], service: &dyn WaitHook, stall: Duration) -> ReadOutcome {
        if out.is_empty() {
            return ReadOutcome::Bytes(0);
        }
        let mut demanded = Duration::ZERO;
        // Charged from here to each pass's end, so time spent in the hook
        // counts too: a fake clock stepped there must not be lost.
        let mut last = self.0.now();
        let mut state = lock(&self.0.state);
        // Captured at entry, and re-tested on every pass. A read blocked
        // across a `retire()` + `begin()` pair would otherwise wake into the
        // *new* generation and hand the decoder bytes from a different byte
        // offset — silent corruption rather than an error. Today that pair
        // only ever runs on the decode thread, which is the thread already
        // inside this call, but nothing enforces that and the failure mode is
        // far too quiet to rest on a scheduling accident.
        let entered = state.generation;
        loop {
            if state.retired || state.generation != entered {
                return ReadOutcome::Retired;
            }
            // Deliberately *not* gated on `frozen`. A freeze pauses playback
            // by parking the output, not by starving the decoder — and gating
            // delivery here deadlocks the two operations that must still do
            // I/O while paused: reopening a retired source, and a seek's
            // refinement reads. Both are legitimate while playback is paused,
            // and both would block forever waiting for bytes the channel is
            // holding back. What the freeze does gate is the stall budget.
            if !state.bytes.is_empty() {
                let count = state.bytes.len().min(out.len());
                // A whole-batch `drain` rather than a per-byte `pop_front`:
                // it lowers to a memcpy pair instead of `count` bounds-checked
                // pops, and it removes any need for a byte-substituting hedge
                // if that count were ever wrong.
                for (slot, byte) in out.iter_mut().zip(state.bytes.drain(..count)) {
                    *slot = byte;
                }
                drop(state);
                self.0.producer_wake.notify_waiters();
                return ReadOutcome::Bytes(count);
            }
            if let Some(outcome) = state.outcome.clone() {
                return match outcome {
                    Outcome::Eof => ReadOutcome::Eof,
                    Outcome::Failed(failure) => ReadOutcome::Failed(failure),
                };
            }
            if !state.frozen && demanded >= stall {
                return ReadOutcome::Failed(RemoteFailure::Timeout {
                    phase: Phase::Stall,
                });
            }
            // Checked regardless of `frozen`, unlike the stall budget just
            // above (R1). A seek's own operation deadline must still expire a
            // read blocked on a stalled server even while playback is
            // paused: the stall budget's suspension while frozen is
            // deliberate for ordinary playback (a pause must never be
            // reported as a server stall), but inherited unchanged by a
            // seek's wait it would let a paused, stalled seek hang forever —
            // bounded on paper, wedged in fact.
            if let Some(deadline) = state.operation_deadline
                && self.0.now() >= deadline
            {
                return ReadOutcome::Failed(RemoteFailure::Timeout { phase: Phase::Seek });
            }
            let frozen_before = state.frozen;
            let (guard, _) = match self.0.reader_wake.wait_timeout(state, SLICE) {
                Ok(pair) => pair,
                Err(poisoned) => poisoned.into_inner(),
            };
            state = guard;
            // Outside the predicate but inside the loop: the hook runs on every
            // slice, which is what keeps position and checkpoints current while
            // the network is quiet (§8), what services a freeze (Task 9), and
            // what a test uses to prove the wait was entered.
            //
            // The lock is dropped across the call, and retaken afterwards. The
            // hook takes the facts lock and then the transport lock, and a
            // worker that already holds the transport lock may reach this
            // interrupt; holding both here would close that cycle.
            drop(state);
            service.service();
            // Only unfrozen time is demand. A slice that began frozen is not
            // charged, whatever the flag says by the time it ends.
            let now = self.0.now();
            if !frozen_before {
                demanded += now.duration_since(last);
            }
            last = now;
            state = lock(&self.0.state);
        }
    }

    /// Push one chunk, waiting asynchronously for room.
    ///
    /// Returns `false` when the generation was superseded or retired, which is
    /// the fetch task's signal to stop.
    pub async fn push(&self, generation: u64, chunk: &[u8]) -> bool {
        let mut offset = 0;
        while offset < chunk.len() {
            // Create the future *before* re-checking, so a notify that lands
            // between the check and the await is still delivered.
            let notified = self.0.producer_wake.notified();
            let accepted = {
                let mut state = lock(&self.0.state);
                if state.generation != generation || state.retired {
                    return false;
                }
                let room = state.capacity.saturating_sub(state.bytes.len());
                let take = room.min(chunk.len() - offset);
                if take > 0 {
                    state.bytes.extend(&chunk[offset..offset + take]);
                }
                take
            };
            if accepted > 0 {
                offset += accepted;
                self.0.reader_wake.notify_all();
                continue;
            }
            notified.await;
        }
        let state = lock(&self.0.state);
        state.generation == generation && !state.retired
    }

    /// End the stream. A stale generation's outcome is discarded: a superseded
    /// response must not be able to report EOF into the live generation.
    pub fn finish(&self, generation: u64, outcome: Outcome) {
        {
            let mut state = lock(&self.0.state);
            if state.generation != generation || state.retired || state.outcome.is_some() {
                return;
            }
            state.outcome = Some(outcome);
        }
        self.0.reader_wake.notify_all();
    }
}
