# M9 Event Outbox Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One `playback::outbox::Outbox` owns the engine's outgoing events — the bounded channel's sender, the queue waiting for room, and every send/hold/drop/displace rule — and both the worker and the wait hook emit through it. The hook's separate outbox, the `backlog_empty` mirror and the three drain sites go.

**Architecture:** Four tasks, each green on its own.
- Task 1 adds `src/playback/outbox.rs` with its unit tests. Nothing uses it yet; it is `pub` in a `pub mod`, so clippy sees no dead code. Its constants duplicate `engine.rs`'s until Task 2 deletes those.
- Task 2 wires both emission paths to it: `Worker` emits, flushes, offers the aggregated warning and returns its shutdown report through the outbox; `WaitService` emits its announcements into it and flushes it on every blocked-read slice. The old queue, mirror, drains and constants are deleted.
- Task 3 adds `Worker::sync_facts` so `SessionFacts` are never older than the newest queued event (spec §4.3), test first.
- Task 4 updates the docs, writes the acceptance record and runs the verification, including the load recipe.

**Tech Stack:** Rust 1.98.1, edition 2024, crossbeam-channel, the in-repo `TestEngine` / `TestServer` harness.

**Spec:** `docs/superpowers/specs/2026-10-13-tenuto-m9-event-outbox-design.md`. Read it before starting. §2 (compatibility boundary) is the contract every task keeps; §4.3 is why Task 3 exists.

## Global Constraints

- Every cargo command uses `--locked`; the toolchain is 1.98.1.
- Runtime code has no `unsafe`, `unwrap` or `expect` (the lints deny them). Tests may use them (`clippy.toml`).
- Unchanged values: `EVENT_CAPACITY` 64, `RESERVED_EVENT_SLOTS` 9, `PENDING_CAP` 128, `FAULT_EVENT_BUDGET` 8, `DIAGNOSTIC_INTERVAL` 500 ms. The channel holds 64; an ordinary event may be sent only while more than 9 slots are free, so the channel's ordinary limit is 55.
- Unchanged rules: protected events (load outcomes) are never dropped or displaced and may use the reserve; a terminal event may use the reserve and at the cap displaces the oldest event that is neither terminal nor protected; FIFO within the queue; command admission closes while anything is pending; shutdown answers each undispatched `Load` with `LoadCancelled`.
- The outbox's mutex is a leaf: no `Outbox` method calls out while holding it, no guard leaves `outbox.rs`. Never hold it across a decoder read or a hook call.
- Rustdoc on `pub` items must not intra-doc-link private items (`[`Worker`]` fails `cargo doc -D warnings`): name them in plain backticks.
- Engine tests run on the virtual clock: no sleeps, no fixed-duration waits in new tests (`CLAUDE.md`, Tests).
- Never add to `tests/m9_layering.rs`'s `ALLOWED`.
- Commits carry no `Co-authored-by` or Claude attribution lines.

## Review Focus

1. A pause submitted while a read is held must reach the application while the read is still held (the hook now delivers through the outbox's flush, not a direct `try_send`). Pinned by the existing `engine_remote::pause_during_a_stalled_read_freezes_output_and_resume_continues`, which Task 2 runs explicitly.
2. The application drops its receiver while the worker is stuck in a read: the worker must still end once the read returns, not keep pumping for nobody. Task 2 adds `a_receiver_dropped_during_a_held_read_still_ends_the_worker` to `tests/engine_remote.rs`.
3. An aggregated warning refused for want of an ordinary slot must be offered again at the next interval, not silently counted as reported. Task 2 adds `a_refused_warning_is_offered_again_at_the_next_interval` to `engine.rs`'s tests.
4. Events the application never drained must still come back, in order, in the shutdown report. Pinned by the existing `engine_shutdown::join_returns_the_events_the_application_never_drained`, run in Task 2.
5. Flushing on every blocked-read slice must never put an ordinary event into the reserve. Pinned by Task 1's `flush_keeps_ordinary_events_out_of_the_reserve_and_keeps_order` and Task 2's `a_blocked_read_flushes_a_backlog_the_worker_left`.

---

### Task 1: `playback::outbox`

**Files:**
- Create: `src/playback/outbox.rs`
- Modify: `src/playback/mod.rs` (add `pub mod outbox;` between `output` and `prepare`)

**Interfaces:**
- Consumes: `PlaybackEvent::{is_terminal, is_protected}` (`src/playback/event.rs`).
- Produces (used by Tasks 2 and 3):
  - `pub fn channel() -> (Arc<Outbox>, Receiver<PlaybackEvent>)`
  - `impl Outbox { pub fn emit(&self, event: PlaybackEvent); pub fn flush(&self); pub fn offer(&self, event: PlaybackEvent) -> bool; pub fn is_empty(&self) -> bool; pub fn fits(&self, extra: usize) -> bool; pub fn dropped(&self) -> u64; pub fn disconnected(&self) -> bool; pub fn take_remaining(&self) -> Vec<PlaybackEvent> }`

- [ ] **Step 1: Write the module with its tests, implementation stubbed**

Create `src/playback/outbox.rs`. Write the whole file below, but with every `Outbox` method body replaced by `todo!()` for this step only (the tests must fail on behavior, not on compilation). `todo!()` is fine in an intermediate step; Step 3 replaces every one.

```rust
//! The engine's outgoing events: one ordered queue and one delivery policy,
//! shared by the decode worker and the wait hook (M9.4).
//!
//! The hook runs on the worker's own thread, inside a decoder read that
//! blocked on the network, so the two never emit at the same time. The lock
//! exists because the hook cannot borrow `Worker` from inside that read, not
//! because anything contends for it. It is a leaf: no method here calls out
//! while holding it and no guard leaves this module, so it is never held
//! across a decoder read or a hook call.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam_channel::{Receiver, Sender, TrySendError};

use super::event::PlaybackEvent;

const EVENT_CAPACITY: usize = 64;
/// Ordinary events may not occupy these; terminal and protected ones may.
const RESERVED_EVENT_SLOTS: usize = 9;
const PENDING_CAP: usize = 128;

// Reserve budget. Terminal or protected outcomes may occupy the reserved
// tail; ordinary events may not. The worst case is one loop iteration
// emitting, at most:
//
//   stop interrupt        1  StateChanged{Stopped}
//   a serviced fault      2  Failed + StateChanged, or DeviceRecovered + StateChanged
//   a dispatched command  5  Load is the widest: StateChanged{Loading},
//                            CapabilitiesChanged, Loaded, then either
//                            StateChanged{Paused} (`open_transport` succeeds)
//                            or Failed + StateChanged{Failed} (it does not) -
//                            the failure tail is one event wider than the
//                            success one, so 5 is this row's true worst case.
//   end of track          2  EndOfTrack + StateChanged{Ended}
//                        --
//                        10 (naive union)
//
// 10 looks like it breaks `RESERVED_EVENT_SLOTS == 9`, but the reserve only
// has to be as large as the TERMINAL-OR-PROTECTED share of that union: an
// ordinary event that cannot flush is merely held in the queue (bounded
// separately, by `PENDING_CAP`) until the reserve clears - never lost, and
// so never in need of reservation. Only a terminal or protected outcome,
// which the reserved tail exists to guarantee delivery for even under a full
// ordinary backlog, must actually fit (Decision 4, Decision 5). Row by row:
//
//   stop interrupt        1  StateChanged{Stopped} (terminal)
//   a fatal fault          2  Failed + StateChanged{Failed} (both terminal)
//   a dispatched load      3  Loaded (protected, M5 §6) + Failed +
//                             StateChanged{Failed} (both terminal)
//   end of track           2  EndOfTrack + StateChanged{Ended} (both terminal)
//                        --
//                         8 <= RESERVED_EVENT_SLOTS (9)
//
// A cancelled load - a stop or shutdown landing during the open - emits only
// `LoadCancelled` (1, protected), well under the dispatched-load row's own
// share, so it never raises the bound. These four rows are still not
// mutually exclusive in a single pass, so the union is the bound rather than
// the maximum of them; command admission closes while a backlog exists, and
// `service_faults` defers a fault whose events would not fit, so neither
// source can outrun the drain.
//
// `RestartEstablished` and `SeekCancelled` both belong to commands narrower
// than `Load` (their own event plus a `StateChanged`, at most 2, and neither
// terminal), so neither raises the bound. The resume-unavailable warning does
// not add a sixth to the dispatched-command row either: it rides on
// `Loaded.disposition` rather than an event of its own. The wait hook's
// `Paused`/`Playing` announcements are ordinary and need no reserve.

/// The engine's event channel: the receiver for the application, the outbox
/// for the worker and the wait hook.
pub fn channel() -> (Arc<Outbox>, Receiver<PlaybackEvent>) {
    let (sender, receiver) = crossbeam_channel::bounded(EVENT_CAPACITY);
    let outbox = Outbox {
        inner: Mutex::new(Inner {
            sender,
            pending: VecDeque::new(),
            dropped: 0,
            disconnected: false,
        }),
    };
    (Arc::new(outbox), receiver)
}

/// Every event the engine sends, and the rules for when it goes.
pub struct Outbox {
    inner: Mutex<Inner>,
}

struct Inner {
    sender: Sender<PlaybackEvent>,
    pending: VecDeque<PlaybackEvent>,
    dropped: u64,
    disconnected: bool,
}

impl Inner {
    fn free_slots(&self) -> usize {
        EVENT_CAPACITY.saturating_sub(self.sender.len())
    }
}

impl Outbox {
    /// A poisoned lock means a thread already panicked while holding it;
    /// carry on with the state it left, as `wait.rs`'s `lock` does.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Queue `event` behind everything pending. At `PENDING_CAP`: a
    /// protected event is queued anyway; a terminal one displaces the oldest
    /// event that is neither terminal nor protected, or is dropped when there
    /// is none; an ordinary one is dropped. Every drop or displacement counts
    /// in `dropped`. Never sends.
    pub fn emit(&self, event: PlaybackEvent) {
        let mut inner = self.lock();
        // Protected events — load outcomes — bypass the cap entirely
        // (Decision 4): correlation cannot be inferred from anything later.
        if inner.pending.len() >= PENDING_CAP && !event.is_protected() {
            if !event.is_terminal() {
                inner.dropped += 1;
                return;
            }
            // A terminal outcome displaces the oldest ordinary event rather
            // than being dropped: nothing that follows implies it.
            match displacement_victim(&inner.pending) {
                Some(index) => {
                    inner.pending.remove(index);
                    inner.dropped += 1;
                }
                None => {
                    inner.dropped += 1;
                    return;
                }
            }
        }
        inner.pending.push_back(event);
    }

    /// Send from the front while there is room: an ordinary event only while
    /// more than `RESERVED_EVENT_SLOTS` slots are free, a terminal or
    /// protected one into any free slot. Stops at the first event that does
    /// not fit, so order is kept. A disconnected receiver puts the event back
    /// at the front, so the shutdown report still carries it, and latches
    /// `disconnected`.
    pub fn flush(&self) {
        let mut inner = self.lock();
        loop {
            let Some(front) = inner.pending.front() else {
                return;
            };
            let free = inner.free_slots();
            let may_use_reserve = front.is_terminal() || front.is_protected();
            if free == 0 || (!may_use_reserve && free <= RESERVED_EVENT_SLOTS) {
                return;
            }
            let Some(event) = inner.pending.pop_front() else {
                return;
            };
            match inner.sender.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    inner.pending.push_front(event);
                    return;
                }
                Err(TrySendError::Disconnected(event)) => {
                    inner.pending.push_front(event);
                    inner.disconnected = true;
                    return;
                }
            }
        }
    }

    /// Send `event` only if nothing is pending and an ordinary slot is free;
    /// otherwise discard it and return `false`. For the aggregated warning,
    /// which is a snapshot of counters, not a queued event: one refused now
    /// is rebuilt from the same counters next time. A disconnected receiver
    /// latches `disconnected`.
    pub fn offer(&self, event: PlaybackEvent) -> bool {
        let mut inner = self.lock();
        if !inner.pending.is_empty() || inner.free_slots() <= RESERVED_EVENT_SLOTS {
            return false;
        }
        match inner.sender.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                inner.disconnected = true;
                false
            }
        }
    }

    /// Whether nothing is pending: command admission and `command_waiting`.
    pub fn is_empty(&self) -> bool {
        self.lock().pending.is_empty()
    }

    /// Whether `extra` more events fit under `PENDING_CAP`: the fault
    /// budget's check, which keeps the cap private to this module.
    pub fn fits(&self, extra: usize) -> bool {
        self.lock().pending.len() + extra <= PENDING_CAP
    }

    /// Events dropped or displaced at the cap, for the aggregated warning.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Whether a send has found the receiver gone.
    pub fn disconnected(&self) -> bool {
        self.lock().disconnected
    }

    /// Everything still pending, in order, leaving the queue empty: the
    /// shutdown report's tail.
    pub fn take_remaining(&self) -> Vec<PlaybackEvent> {
        self.lock().pending.drain(..).collect()
    }
}

/// The oldest event `emit` may displace to make room for a terminal arrival
/// at `PENDING_CAP`: neither terminal (already guaranteed delivery some other
/// way) nor protected (never displaced — Decision 4).
fn displacement_victim(pending: &VecDeque<PlaybackEvent>) -> Option<usize> {
    pending
        .iter()
        .position(|event| !event.is_terminal() && !event.is_protected())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::media::provenance::PositionProvenance;
    use crate::playback::command::LoadRequestId;
    use crate::playback::state::PlaybackState;

    /// The channel's ordinary limit: an ordinary event needs more than the
    /// reserve free.
    const ORDINARY: usize = EVENT_CAPACITY - RESERVED_EVENT_SLOTS;

    fn ordinary(tag: usize) -> PlaybackEvent {
        PlaybackEvent::Warning {
            session_rev: 1,
            message: tag.to_string(),
        }
    }

    fn terminal() -> PlaybackEvent {
        PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::ZERO,
            provenance: PositionProvenance::Established,
        }
    }

    fn protected() -> PlaybackEvent {
        PlaybackEvent::LoadCancelled {
            session_rev: 1,
            request: LoadRequestId::from_raw(1),
        }
    }

    fn tag(event: &PlaybackEvent) -> Option<usize> {
        match event {
            PlaybackEvent::Warning { message, .. } => message.parse().ok(),
            _ => None,
        }
    }

    #[test]
    fn an_ordinary_event_at_the_cap_is_dropped_and_counted() {
        let (outbox, _events) = channel();
        for n in 0..PENDING_CAP {
            outbox.emit(ordinary(n));
        }
        outbox.emit(ordinary(PENDING_CAP));
        assert_eq!(outbox.dropped(), 1);
        let remaining = outbox.take_remaining();
        assert_eq!(remaining.len(), PENDING_CAP);
        assert_eq!(remaining.last().and_then(tag), Some(PENDING_CAP - 1));
    }

    #[test]
    fn a_terminal_event_at_the_cap_displaces_the_oldest_ordinary_one() {
        let (outbox, _events) = channel();
        outbox.emit(protected());
        for n in 0..PENDING_CAP - 1 {
            outbox.emit(ordinary(n));
        }
        outbox.emit(terminal());
        assert_eq!(outbox.dropped(), 1);
        let remaining = outbox.take_remaining();
        assert_eq!(remaining.len(), PENDING_CAP);
        assert!(remaining[0].is_protected(), "{:?}", remaining[0]);
        assert_eq!(tag(&remaining[1]), Some(1), "ordinary 0 was the victim");
        assert!(matches!(remaining.last(), Some(PlaybackEvent::EndOfTrack { .. })));
    }

    /// A terminal, non-protected event may be displaced to make room for
    /// another terminal event at `PENDING_CAP`; a protected one — a load
    /// outcome — never may, since correlation cannot be inferred from
    /// anything later (Decision 4).
    #[test]
    fn a_terminal_event_never_displaces_a_protected_one() {
        let mut pending = VecDeque::new();
        pending.push_back(protected());
        pending.push_back(terminal());
        pending.push_back(ordinary(0));
        assert_eq!(displacement_victim(&pending), Some(2));
        pending.pop_back();
        assert_eq!(displacement_victim(&pending), None);
    }

    #[test]
    fn a_protected_event_is_queued_past_the_cap() {
        let (outbox, _events) = channel();
        for n in 0..PENDING_CAP {
            outbox.emit(ordinary(n));
        }
        outbox.emit(protected());
        assert_eq!(outbox.dropped(), 0);
        assert_eq!(outbox.take_remaining().len(), PENDING_CAP + 1);
    }

    #[test]
    fn flush_keeps_ordinary_events_out_of_the_reserve_and_keeps_order() {
        let (outbox, events) = channel();
        for n in 0..EVENT_CAPACITY {
            outbox.emit(ordinary(n));
        }
        outbox.emit(terminal());
        outbox.flush();
        let sent: Vec<Option<usize>> = events.try_iter().map(|e| tag(&e)).collect();
        assert_eq!(sent, (0..ORDINARY).map(Some).collect::<Vec<_>>());
        // The terminal event waits behind the ordinary ones it followed.
        let rest = outbox.take_remaining();
        assert_eq!(rest.len(), EVENT_CAPACITY - ORDINARY + 1);
        assert!(matches!(rest.last(), Some(PlaybackEvent::EndOfTrack { .. })));
    }

    #[test]
    fn a_terminal_or_protected_event_may_use_the_reserve() {
        let (outbox, events) = channel();
        for n in 0..ORDINARY {
            outbox.emit(ordinary(n));
        }
        outbox.flush();
        assert_eq!(events.len(), ORDINARY);
        outbox.emit(ordinary(ORDINARY));
        outbox.flush();
        assert_eq!(events.len(), ORDINARY, "an ordinary event took a reserved slot");
        assert!(!outbox.is_empty());
        let _ = outbox.take_remaining();
        outbox.emit(terminal());
        outbox.emit(protected());
        outbox.emit(PlaybackEvent::StateChanged {
            session_rev: 1,
            state: PlaybackState::Stopped,
            request: None,
        });
        outbox.flush();
        assert_eq!(events.len(), ORDINARY + 3);
        assert!(outbox.is_empty());
    }

    #[test]
    fn offer_sends_only_into_a_free_ordinary_slot_with_nothing_pending() {
        let (outbox, events) = channel();
        assert!(outbox.offer(ordinary(0)));
        assert_eq!(events.len(), 1);

        outbox.emit(ordinary(1));
        assert!(!outbox.offer(ordinary(2)), "jumped a pending event");
        assert_eq!(events.len(), 1);

        let (full, full_events) = channel();
        for n in 0..ORDINARY {
            full.emit(ordinary(n));
        }
        full.flush();
        assert!(!full.offer(ordinary(ORDINARY)), "took a reserved slot");
        assert_eq!(full_events.len(), ORDINARY);
    }

    #[test]
    fn a_disconnect_latches_and_keeps_the_unsent_event() {
        let (outbox, events) = channel();
        drop(events);
        outbox.emit(ordinary(7));
        outbox.flush();
        assert!(outbox.disconnected());
        let remaining = outbox.take_remaining();
        assert_eq!(remaining.iter().map(tag).collect::<Vec<_>>(), vec![Some(7)]);
    }

    #[test]
    fn offer_latches_a_disconnect() {
        let (outbox, events) = channel();
        drop(events);
        assert!(!outbox.offer(ordinary(0)));
        assert!(outbox.disconnected());
    }

    #[test]
    fn take_remaining_returns_events_in_order_and_empties() {
        let (outbox, _events) = channel();
        for n in 0..3 {
            outbox.emit(ordinary(n));
        }
        let remaining = outbox.take_remaining();
        assert_eq!(
            remaining.iter().map(tag).collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2)]
        );
        assert!(outbox.is_empty());
    }

    #[test]
    fn fits_counts_room_under_the_cap() {
        let (outbox, _events) = channel();
        for n in 0..PENDING_CAP - 8 {
            outbox.emit(ordinary(n));
        }
        assert!(outbox.fits(8));
        assert!(!outbox.fits(9));
    }
}
```

Add `pub mod outbox;` to `src/playback/mod.rs` between `pub mod output;` and `pub mod prepare;`.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --locked --lib playback::outbox`
Expected: FAIL — every test panics with `not yet implemented` except `a_terminal_event_never_displaces_a_protected_one` (it only calls `displacement_victim`, which is real). If anything fails to *compile*, fix that before going on.

- [ ] **Step 3: Replace every `todo!()` with the bodies shown in Step 1**

- [ ] **Step 4: Run the tests to see them pass, then the gate for this file**

Run: `cargo test --locked --lib playback::outbox`
Expected: PASS, 11 tests.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`
Expected: clean. (`EVENT_CAPACITY`, `RESERVED_EVENT_SLOTS` and `PENDING_CAP` exist in both `engine.rs` and `outbox.rs` until Task 2; that is expected.)

- [ ] **Step 5: Commit**

```bash
git add src/playback/outbox.rs src/playback/mod.rs
git commit -m "refactor(m9): playback::outbox - one queue and one delivery policy for the engine's events"
```

---

### Task 2: Both emission paths through the outbox

**Files:**
- Modify: `src/playback/engine.rs` (constants, `EngineHandle::assemble`, `Worker` fields and `new`, `run`, `emit`, `drain_outbox`, `note_backlog`, `free_event_slots`, `flush_events`, `emit_aggregated_warning_if_slot_free`, `service_faults`, `shutdown`, the reconnect `command_waiting` check, `check_end_of_track`, `displacement_victim` and its test, the `tests` module)
- Modify: `src/playback/wait.rs` (imports, `WaitService` fields, `new`, `take_outbox`, `announce`, `service_as`, the `inert_service` unit helper)
- Modify: `tests/wait_service.rs`
- Modify: `tests/engine_remote.rs` (one new test)

**Interfaces:**
- Consumes: everything Task 1 produced.
- Produces:
  - `WaitService::new(transport: Arc<Mutex<Option<TransportCore>>>, progress: Arc<Mutex<Progress>>, facts: Arc<Mutex<SessionFacts>>, interrupt: Arc<SourceInterrupt>, outbox: Arc<Outbox>, clock: Arc<dyn Fn() -> Nanos + Send + Sync>) -> Arc<Self>`
  - `Worker::new(output, faults, progress, commands, outbox: Arc<Outbox>, liveness, wake, interrupt, source_interrupt, traits, http, reconnect_policy, device_clock, spectrum)` — `outbox` takes the place `events: Sender<PlaybackEvent>` had.
  - In `engine.rs`'s `tests` module: `fn idle_worker() -> (Worker, Arc<Mutex<Progress>>, Receiver<PlaybackEvent>)` (Task 3 reuses it).

- [ ] **Step 1: Write the new tests**

In `tests/wait_service.rs`:

1. Replace the imports block's `use tenuto::playback::wait::{SessionFacts, WaitService};` line with:

```rust
use tenuto::playback::outbox::{self, Outbox};
use tenuto::playback::wait::{SessionFacts, WaitService};
```

2. Replace the `inert` helper and its doc comment with:

```rust
/// The wiring only the freeze tests care about, in the inert configuration:
/// nothing frozen, nothing queued, and nobody reading. The receiver is
/// dropped, so a flush latches a disconnect and keeps its events queued,
/// which these tests never look at.
fn inert() -> (Arc<SourceInterrupt>, Arc<Outbox>) {
    let (outbox, _events) = outbox::channel();
    (SourceInterrupt::new(1024), outbox)
}
```

3. In every test that does `let (interrupt, events, outbox, backlog_empty) = inert();`, change it to `let (interrupt, outbox) = inert();` and in its `WaitService::new(..)` call replace the three lines `events,` `outbox,` `backlog_empty,` with the single line `outbox,`. (Tests: `servicing_publishes_the_position_the_timeline_reports`, `a_degraded_quality_does_not_imply_estimated_provenance`, `servicing_with_no_transport_is_harmless_and_repeatable`, `servicing_a_blocked_read_advances_the_position_as_the_clock_advances`.)

4. In `servicing_a_freeze_parks_the_output_and_announces_paused`, replace the four setup lines

```rust
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
    let interrupt = SourceInterrupt::new(1024);
    let outbox = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let backlog_empty = Arc::new(std::sync::atomic::AtomicBool::new(true));
```

with

```rust
    let (outbox, events_rx) = outbox::channel();
    let interrupt = SourceInterrupt::new(1024);
```

and in its `WaitService::new(..)` call replace `events_tx,` `Arc::clone(&outbox),` `Arc::clone(&backlog_empty),` with `Arc::clone(&outbox),`. Its assertions stay as they are: the slice's own flush now delivers `Paused` and later `Playing`.

5. Replace the whole test `a_hook_announcement_goes_to_the_outbox_when_the_workers_backlog_is_not_empty` with these two:

```rust
fn quiet_progress() -> Arc<Mutex<Progress>> {
    Arc::new(Mutex::new(Progress {
        session_rev: 1,
        media: None,
        position: Duration::ZERO,
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: None,
    }))
}

fn playing_facts() -> Arc<Mutex<SessionFacts>> {
    Arc::new(Mutex::new(SessionFacts {
        session_rev: 1,
        media: None,
        position: Duration::ZERO,
        degraded: false,
        playing: true,
        provenance: PositionProvenance::Established,
        frozen_by_hook: false,
        load: None,
    }))
}

fn warning(message: &str) -> PlaybackEvent {
    PlaybackEvent::Warning {
        session_rev: 1,
        message: message.into(),
    }
}

#[test]
fn a_hook_announcement_queues_behind_the_workers_backlog_and_a_blocked_read_delivers_both_in_order() {
    // One queue: what the worker emitted before its read blocked goes out
    // first, and the hook's Paused after it, from the same slice's flush.
    let (outbox, events_rx) = outbox::channel();
    let interrupt = SourceInterrupt::new(1024);
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        quiet_progress(),
        playing_facts(),
        Arc::clone(&interrupt),
        Arc::clone(&outbox),
        Arc::new(|| Nanos(0)),
    );
    outbox.emit(warning("first"));
    outbox.emit(warning("second"));

    interrupt.freeze();
    service.service();

    let received: Vec<PlaybackEvent> = events_rx.try_iter().collect();
    assert!(
        matches!(
            received.as_slice(),
            [
                PlaybackEvent::Warning { message: first, .. },
                PlaybackEvent::Warning { message: second, .. },
                PlaybackEvent::StateChanged {
                    state: PlaybackState::Paused,
                    ..
                },
            ] if first == "first" && second == "second"
        ),
        "{received:?}"
    );
}

#[test]
fn a_blocked_read_flushes_a_backlog_the_worker_left() {
    // The worker queued more than the channel's ordinary slots hold (64,
    // less the 9 reserved for terminal and protected outcomes: 55), then
    // blocked in a read. As the application drains, each slice of that read
    // sends what now fits, instead of waiting for the read to return.
    let (outbox, events_rx) = outbox::channel();
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        quiet_progress(),
        playing_facts(),
        SourceInterrupt::new(1024),
        Arc::clone(&outbox),
        Arc::new(|| Nanos(0)),
    );
    for n in 0..56 {
        outbox.emit(warning(&n.to_string()));
    }
    outbox.flush();
    assert_eq!(events_rx.len(), 55);
    assert!(!outbox.is_empty());

    let _ = events_rx.try_recv();
    service.service();

    assert_eq!(events_rx.len(), 55);
    assert!(outbox.is_empty(), "the held event was not sent");
}
```

In `tests/engine_remote.rs`, add after `pause_during_a_stalled_read_freezes_output_and_resume_continues`:

```rust
#[test]
fn a_receiver_dropped_during_a_held_read_still_ends_the_worker() {
    // The hook flushes from inside a blocked read now, so it may be the one
    // that meets the disconnect. Whoever meets it, the worker must end once
    // the read returns rather than keep pumping for nobody.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
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

    engine.drop_event_receiver();
    assert!(server.release());

    assert!(
        engine.join_within(Duration::from_secs(5)),
        "the worker outlived its event receiver"
    );
    server.shutdown();
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --locked --test wait_service`
Expected: compile errors — `WaitService::new` still takes eight arguments. That is the failure this task resolves. (`a_receiver_dropped_during_a_held_read_still_ends_the_worker` exercises today's code too and may already pass; it is a regression guard for this task's change.)

- [ ] **Step 3: Move `wait.rs` onto the outbox**

In `src/playback/wait.rs`:

1. Imports: delete `use std::collections::VecDeque;`, `use std::sync::atomic::{AtomicBool, Ordering};` and `use crossbeam_channel::{Sender, TrySendError};`. Change `use super::engine::{DEADLINE, EVENT_CAPACITY, PUMP_NAP, RESERVED_EVENT_SLOTS, TransportCore};` to `use super::engine::{DEADLINE, PUMP_NAP, TransportCore};`. Add `use super::outbox::Outbox;`. (Let the compiler confirm nothing else used the removed names.)

2. The `WaitService` doc comment's second paragraph becomes:

```rust
/// Four jobs, and no others: drain spans into the timeline, publish the
/// keep-latest progress snapshot, act on the freeze level — park the output
/// when a pause arrives, release it when a play does, and announce each —
/// and, while the worker is stuck in the read, deliver what the outbox
/// holds.
```

3. Fields: replace `events: Sender<PlaybackEvent>,` `outbox: Arc<Mutex<VecDeque<PlaybackEvent>>>,` `backlog_empty: Arc<AtomicBool>,` with:

```rust
    /// The engine's one event queue, shared with the worker.
    outbox: Arc<Outbox>,
```

4. `new`: delete the `#[allow(clippy::too_many_arguments)]` line and its comment; replace the three parameters `events`, `outbox`, `backlog_empty` with `outbox: Arc<Outbox>,`, and the three struct-literal fields with `outbox,`.

5. Delete `take_outbox` and its doc comment.

6. Replace `announce` (doc comment and body) with:

```rust
    /// Queue one event behind everything the worker emitted before it. The
    /// slice's own flush, at the end of `service_as`, delivers it.
    fn announce(&self, event: PlaybackEvent) {
        self.outbox.emit(event);
    }
```

7. In `service_as`, after the existing `self.publish_progress(servicing);` line, add:

```rust
        // Progress first, then the events: whatever goes out from here was
        // emitted no later than the facts that progress was built from
        // (outbox spec §4.3). A worker pass flushes at its own step 4.
        if blocked {
            self.outbox.flush();
        }
```

8. In the `#[cfg(test)] mod tests` at the bottom, `inert_service` becomes:

```rust
    /// An inert `WaitService`: no transport, nothing queued, and a receiver
    /// nobody holds. Enough to exercise `service_as` without a real decoder
    /// or a real network read anywhere behind it.
    fn inert_service() -> Arc<WaitService> {
        let (outbox, _events) = crate::playback::outbox::channel();
        WaitService::new(
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(Progress {
                session_rev: 1,
                media: None,
                position: Duration::ZERO,
                quality: PositionQuality::Exact,
                provenance: PositionProvenance::Established,
                buffering: false,
                load: None,
            })),
            Arc::new(Mutex::new(SessionFacts {
                session_rev: 1,
                media: None,
                position: Duration::ZERO,
                degraded: false,
                playing: false,
                provenance: PositionProvenance::Established,
                frozen_by_hook: false,
                load: None,
            })),
            SourceInterrupt::new(1024),
            outbox,
            Arc::new(|| Nanos(0)),
        )
    }
```

- [ ] **Step 4: Move `engine.rs` onto the outbox**

In `src/playback/engine.rs`:

1. Delete `RESERVED_EVENT_SLOTS`, `EVENT_CAPACITY`, `PENDING_CAP` and their doc comments, and the whole `// Reserve budget.` comment block after them (Task 1 moved it to `outbox.rs`).

2. Add `use super::outbox::{self, Outbox};` beside the other `super::` imports.

3. `EngineHandle::assemble`: replace `let (events_tx, events_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);` with `let (events_out, events_rx) = outbox::channel();`, and in the `Worker::new(..)` call pass `events_out,` where `events_tx,` was.

4. `Worker` fields: delete `pending_events`, `events: Sender<PlaybackEvent>`, `dropped_events` and `backlog_empty` (with its doc comment). Add, where `events` was:

```rust
    /// The engine's one event queue and its delivery rules, shared with the
    /// wait hook (`playback::outbox`).
    outbox: Arc<Outbox>,
```

5. `Worker::new`: parameter `events: Sender<PlaybackEvent>,` becomes `outbox: Arc<Outbox>,`. Delete `let backlog_empty = Arc::new(AtomicBool::new(true));` and `let outbox = Arc::new(Mutex::new(VecDeque::new()));`. In `WaitService::new(..)` replace `events.clone(),` `outbox,` `Arc::clone(&backlog_empty),` with `Arc::clone(&outbox),`. In the struct literal delete `pending_events: VecDeque::new(),`, `events,`, `dropped_events: 0,`, `backlog_empty,` and add `outbox,`.

6. `run`: delete step 0 — the `// 0. Drain whatever the hook announced …` comment and `self.drain_outbox();`. Replace each `return Vec::from(self.pending_events);` (four of them) with `return self.outbox.take_remaining();`. Replace `let admit = self.pending_events.is_empty();` with `let admit = self.outbox.is_empty();`. In the comment above `if self.receivers_gone {`, replace "`SendError` when flushing events" with "a disconnect the outbox latched when flushing or offering events".

7. `emit` becomes:

```rust
    fn emit(&mut self, event: PlaybackEvent) {
        self.outbox.emit(event);
    }
```

8. Delete `drain_outbox`, `note_backlog` and `free_event_slots` with their doc comments. `flush_events` becomes:

```rust
    fn flush_events(&mut self) {
        self.outbox.flush();
        self.receivers_gone |= self.outbox.disconnected();
    }
```

9. `emit_aggregated_warning_if_slot_free` becomes:

```rust
    /// Counters, not a queue: an aggregated warning goes out only when an
    /// ordinary slot is free and nothing is pending (`Outbox::offer`), and
    /// it reports the running totals. A receiver that stays connected
    /// without draining therefore bounds diagnostics by the channel itself,
    /// not by a rate limit alone. A refused offer leaves the totals
    /// unreported, so the next interval offers them again.
    fn emit_aggregated_warning_if_slot_free(&mut self) {
        let dropped = self.outbox.dropped();
        let total = self.xruns + self.lost_spans + dropped + self.device_warnings;
        if total == self.reported_diagnostics || self.last_diagnostic.elapsed() < DIAGNOSTIC_INTERVAL
        {
            return;
        }
        let message = format!(
            "audio glitches: {} underruns, {} lost spans, {} dropped events, {} device warnings",
            self.xruns, self.lost_spans, dropped, self.device_warnings
        );
        let event = PlaybackEvent::Warning {
            session_rev: self.session_rev,
            message,
        };
        if self.outbox.offer(event) {
            self.reported_diagnostics = total;
            self.last_diagnostic = Instant::now();
        }
        self.receivers_gone |= self.outbox.disconnected();
    }
```

10. `service_faults`: replace `if self.pending_events.len() + Self::FAULT_EVENT_BUDGET > PENDING_CAP {` with `if !self.outbox.fits(Self::FAULT_EVENT_BUDGET) {`.

11. In `service_reconnect` (the `command_waiting` line): replace `self.pending_events.is_empty()` with `self.outbox.is_empty()`. In the comment above it, "a blocked outbox" stays accurate.

12. `check_end_of_track`: delete the leading `// Symmetric with \`shutdown()\` …` comment and its `self.drain_outbox();`.

13. `shutdown`: delete the leading `// Ruling 5: drained here too …` comment and its `self.drain_outbox();`.

14. Delete the free function `displacement_victim` and its doc comment, and the unit test `a_terminal_event_never_displaces_a_protected_one` (both moved to `outbox.rs` in Task 1).

15. Remove imports the compiler now reports unused (expect `TrySendError`, `VecDeque`, and possibly `AtomicBool`; keep whatever is still used).

- [ ] **Step 5: Add the worker test helper and the refused-warning test**

In `engine.rs`'s `#[cfg(test)] mod tests`, add:

```rust
    /// A worker over `NullOutput` whose loop never runs, the progress slot
    /// its hook publishes to, and the receiver its events reach. The spectrum
    /// thread is stopped at once: the worker only needs the port.
    fn idle_worker() -> (Worker, Arc<Mutex<Progress>>, Receiver<PlaybackEvent>) {
        let (events_out, events) = outbox::channel();
        let progress = Arc::new(Mutex::new(Progress {
            session_rev: 0,
            media: None,
            position: Duration::ZERO,
            quality: PositionQuality::Exact,
            provenance: PositionProvenance::Established,
            buffering: false,
            load: None,
        }));
        let (_faults_tx, faults) = crossbeam_channel::unbounded();
        let (_commands_tx, commands) = crossbeam_channel::bounded(1);
        let (liveness, _liveness_rx) = crossbeam_channel::bounded(0);
        let (_wake_tx, wake) = crossbeam_channel::bounded(1);
        let device_clock = Arc::new(AtomicU64::new(0));
        let (_spectrum, port, mut thread) = spectrum_worker::spawn(Arc::clone(&device_clock));
        thread.stop();
        let worker = Worker::new(
            Box::new(NullOutput::new()),
            faults,
            Arc::clone(&progress),
            commands,
            events_out,
            liveness,
            wake,
            Arc::new(AtomicU8::new(0)),
            SourceInterrupt::new(1024),
            Arc::new(SourceTraits::default()),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(ReconnectPolicy::default())),
            device_clock,
            port,
        );
        (worker, progress, events)
    }

    /// Review focus 3: a warning `offer` refuses is not counted as reported,
    /// so the next interval offers the same totals again.
    #[test]
    fn a_refused_warning_is_offered_again_at_the_next_interval() {
        let (mut worker, _progress, events) = idle_worker();
        // Fill the channel to its ordinary limit (64 - 9 reserved = 55).
        for n in 0..55 {
            worker.outbox.emit(PlaybackEvent::Warning {
                session_rev: 0,
                message: n.to_string(),
            });
        }
        worker.outbox.flush();
        worker.xruns = 1;
        let long_ago = Instant::now().checked_sub(DIAGNOSTIC_INTERVAL).unwrap();
        worker.last_diagnostic = long_ago;

        worker.emit_aggregated_warning_if_slot_free();
        assert_eq!(worker.reported_diagnostics, 0, "a refused warning counted as reported");
        assert_eq!(events.len(), 55);

        let _ = events.try_recv();
        worker.emit_aggregated_warning_if_slot_free();
        assert_eq!(worker.reported_diagnostics, 1);
        let last = events.try_iter().last();
        assert!(
            matches!(&last, Some(PlaybackEvent::Warning { message, .. }) if message.starts_with("audio glitches: 1 underruns")),
            "{last:?}"
        );
    }
```

If any of `NullOutput`, `SourceInterrupt`, `SourceTraits`, `ReconnectPolicy`, `spectrum_worker`, `Progress`, `PositionQuality`, `AtomicU8`, `AtomicU64` is not already in scope through `use super::*;`, add the matching `use` inside the test module (they are all imported at the top of `engine.rs` today).

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked --lib playback && cargo test --locked --test wait_service`
Expected: PASS, including `a_refused_warning_is_offered_again_at_the_next_interval` and both new `wait_service` tests.

Run the review-focus and backlog suites:
`cargo test --locked --test engine_remote -- pause_during_a_stalled_read_freezes_output_and_resume_continues a_receiver_dropped_during_a_held_read_still_ends_the_worker`
`cargo test --locked --test engine_contract && cargo test --locked --test engine_shutdown`
Expected: PASS.

Then the whole gate: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast`
Expected: clean, 0 failed.

- [ ] **Step 7: Commit**

```bash
git add src/playback/engine.rs src/playback/wait.rs tests/wait_service.rs tests/engine_remote.rs
git commit -m "refactor(m9): the worker and the wait hook emit through one outbox; a blocked read flushes it"
```

---

### Task 3: Facts before delivery (`Worker::sync_facts`)

**Files:**
- Modify: `src/playback/engine.rs` (`publish_progress`, `emit`, a new `sync_facts`, the `tests` module)

**Interfaces:**
- Consumes: `idle_worker()` (Task 2), `WaitService::service_as(Servicing)`.
- Produces: `fn sync_facts(&self)` on `Worker`, called by `publish_progress` and `emit`.

- [ ] **Step 1: Write the regression**

In `engine.rs`'s `tests` module, add:

```rust
    /// Outbox spec §4.3. A read that blocks after a command emitted must not
    /// deliver that event against facts from before the command: here a
    /// stored seek's `SeekCompleted` would reach the application with the
    /// stopped position still published under the same revision, and the
    /// session would checkpoint that position over the seek.
    #[test]
    fn an_event_flushed_by_a_blocked_read_carries_progress_from_its_emission() {
        let (mut worker, progress, events) = idle_worker();
        worker.state = PlaybackState::Stopped;
        worker.session_rev = 7;
        worker.position = Duration::from_secs(1);
        worker.publish_progress();

        // As `restore` leaves the worker: same revision, landed and playing,
        // with no loop pass since.
        worker.state = PlaybackState::Playing;
        worker.position = Duration::from_secs(3);
        worker.position_provenance = PositionProvenance::Established;
        worker.emit(PlaybackEvent::SeekCompleted {
            session_rev: 7,
            requested: Duration::from_secs(3),
            actual: Duration::from_secs(3),
            refinement_truncated: false,
            provenance: PositionProvenance::Established,
        });

        // What a read blocking in step 7 runs.
        worker.service.service_as(Servicing::BlockedRead);

        assert!(
            matches!(
                events.try_recv(),
                Ok(PlaybackEvent::SeekCompleted { session_rev: 7, .. })
            ),
            "the blocked read did not deliver the event"
        );
        let published = lock(&progress).clone();
        assert_eq!(published.session_rev, 7);
        assert_eq!(published.position, Duration::from_secs(3));
    }
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --locked --lib an_event_flushed_by_a_blocked_read_carries_progress_from_its_emission`
Expected: FAIL on the position assertion: `left: 1s`, `right: 3s`.

- [ ] **Step 3: Implement `sync_facts`**

Move the facts block out of `publish_progress` into a new method just above it, and call it from both places. `publish_progress` becomes:

```rust
    fn publish_progress(&mut self) {
        self.device_clock
            .fetch_max(self.output.now().0, Ordering::Relaxed);
        self.sync_facts();
        self.service.service_as(Servicing::WorkerLoop);
        // Read back whatever the shared recompute settled on, so
        // `self.position` - the field every command (`SeekBy` among them)
        // reads as "the current position" - stays live while playing, exactly
        // as this method used to keep it before the recompute moved into
        // `WaitService` so the hook could share it.
        self.position = lock(&self.facts).position;
    }
```

(Keep `publish_progress`'s existing doc comment unchanged above it.) The new method, with the moved block verbatim inside:

```rust
    /// Copy the fields `SessionFacts` mirror from the worker. Runs on every
    /// `publish_progress` and before every `emit` (outbox spec §4.3): a read
    /// that blocks later in the same command flushes the event from the
    /// hook, and the progress the hook publishes first must not be older
    /// than that event. `frozen_by_hook` is the hook's own and stays as it
    /// is.
    fn sync_facts(&self) {
        let mut facts = lock(&self.facts);
        facts.session_rev = self.session_rev;
        facts.media = self.media.clone();
        facts.position = self.position;
        facts.degraded = self.degraded;
        // Mirrored, never derived from `degraded` or `playing` (§3.1's
        // stickiness): playing on from an estimated landing must keep
        // reporting it estimated for as long as the landing stands.
        facts.provenance = self.position_provenance;
        // Paused counts as well as Playing: parking silences the
        // callback, but the frames it already handed to the device still
        // play out, so the position goes on rising for one output
        // latency after the park and only then settles. Freezing the
        // number at the instant of the park would report a position
        // slightly behind what the listener actually heard.
        // `Reconnecting` too (M7 §7): the transport is left running so
        // the ring plays out after a disconnect, and that audio is heard,
        // so it must be counted.
        facts.playing = matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Reconnecting
        );
        facts.load = self.adopted_load;
    }
```

`emit` becomes:

```rust
    fn emit(&mut self, event: PlaybackEvent) {
        // Facts first (outbox spec §4.3): the hook may flush this event from
        // a read that blocks before the next loop pass.
        self.sync_facts();
        self.outbox.emit(event);
    }
```

- [ ] **Step 4: Run the regression and the suite**

Run: `cargo test --locked --lib an_event_flushed_by_a_blocked_read_carries_progress_from_its_emission`
Expected: PASS.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast`
Expected: clean, 0 failed.

- [ ] **Step 5: Commit**

```bash
git add src/playback/engine.rs
git commit -m "fix(m9): an event the hook flushes never outruns the facts its progress is built from"
```

---

### Task 4: Docs, acceptance record, verification

**Files:**
- Modify: `docs/architecture.md` (§7.1 event paragraph; §12 M9 line)
- Modify: `docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` (§6 status lines)
- Modify: `CHANGELOG.md` (`[Unreleased]` / `### Internal`)
- Create: `docs/m9-e-acceptance.md`

**Interfaces:** none (documentation and verification).

- [ ] **Step 1: `architecture.md` §7.1**

In the paragraph beginning "Lifecycle events, seek outcomes, end of track, and errors are ordered and lossless.", replace its last sentence ("Ordinary events that do not fit wait in `pending_events` under the `PENDING_CAP` drop-and-displace policy.") with:

```markdown
Ordinary events that do not fit wait in the engine's one `Outbox` (`playback::outbox`) under the `PENDING_CAP` drop-and-displace policy. The worker and the wait hook both emit into it: the hook runs on the worker's thread inside a read blocked on the network, flushes the outbox on every slice of that read, and publishes progress first, so an event never reaches the application ahead of the progress it was emitted with. The outbox's lock is a leaf, never held across a read.
```

- [ ] **Step 2: `architecture.md` §12 M9 line**

In the "**Architecture deepening (M9).**" bullet, replace the sentence

"Still open: M9.1's versioned-file module under the three JSON stores and moving resume intent into `resume.rs`; the engine's transport and event-outbox items (M9.4; the submission door and the landing have shipped)."

with

"M9.1 has shipped: the versioned JSON file module under the three stores, and resume intent in `resume.rs`. M9.4 has shipped: the submission door, the landing and one event outbox (`playback::outbox`); its transport-owns-park item was dropped, for the reason the roadmap's §6 records."

- [ ] **Step 3: Roadmap status lines**

In `docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` §6, append to the "**Transport owns park.**" item:

```markdown
- Status: dropped (M9 PR E). `TransportCore` already owns the transport state (handshake, timeline, anchor, tap); `Phase::Park` is acknowledged on every callback fill and `release` republishes `Run` with a valid tap mapping, so both are idempotent where it matters. `frozen_by_hook` is announcement and thaw policy, not transport state, so a `parked` bit would have no reader. The spike found a send-then-thaw race instead, fixed in #47: the freeze arm runs only for a blocked read.
```

and to the "**Event outbox.**" item:

```markdown
- Status: shipped (M9 PR E, `docs/m9-e-acceptance.md`). `playback::outbox::Outbox` owns the sender, the queue, the reserve, displacement and the dropped count; the worker and the wait hook emit into it, and a blocked read flushes it on every slice. `Worker::emit` syncs `SessionFacts` first, so the hook never delivers an event ahead of the progress it was emitted with.
```

- [ ] **Step 4: CHANGELOG**

Under `## [Unreleased]` → `### Internal`, add as the last bullet (hard-wrapped at 78 columns like its neighbours):

```markdown
- The playback engine's events leave through one queue that the engine and
  a read stalled on the network share. Events held back while the
  application was not reading now go out during a long network read instead
  of after it.
```

- [ ] **Step 5: Verification**

Run the gate:

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

Expected: all clean; record the test totals (passed / failed / ignored).

Run the greps:

```bash
grep -rn "backlog_empty\|take_outbox\|drain_outbox\|note_backlog\|pending_events" src/ || echo "none"
grep -rn "RESERVED_EVENT_SLOTS\|EVENT_CAPACITY\|PENDING_CAP" src/ | grep -v "^src/playback/outbox.rs"  || echo "none"
grep -n "FAULT_EVENT_BUDGET" src/playback/engine.rs
```

Expected: `none`, `none`, and `FAULT_EVENT_BUDGET` used only in its definition and in `self.outbox.fits(Self::FAULT_EVENT_BUDGET)`. A hit inside a comment counts: reword it to name the outbox (for example "while anything is pending in the outbox"), then rerun.

Run the load recipe (`CLAUDE.md`, Tests) on the two engine suites, 10 runs each:

```bash
bins=$(cargo test --locked --test engine_contract --test wait_service --no-run 2>&1 | grep -o "target/debug/deps/\(engine_contract\|wait_service\)-[0-9a-f]*" | sort -u)
for i in $(seq 8); do (yes > /dev/null &); done
for bin in $bins; do
  for i in $(seq 10); do taskset -c 0-1 "$bin" -q 2>&1 | grep -E "test result|FAILED|panicked"; done
done
pkill -x yes
```

Expected: 20 `test result: ok` lines, no `FAILED`.

- [ ] **Step 6: Acceptance record**

Create `docs/m9-e-acceptance.md`, filling the bracketed numbers from Step 5's actual output (do not commit it with brackets left in):

```markdown
# M9 PR E: one event outbox — acceptance

Scope: the M9.4 "Transport owns park" and "Event outbox" items of
`docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` §6.
Spec: `docs/superpowers/specs/2026-10-13-tenuto-m9-event-outbox-design.md`.
Plan: `docs/superpowers/plans/2026-10-13-m9-event-outbox.md`.

Branch `refactor/m9-event-outbox`, from `main` at `6ad2600`.

## Decisions

Made with the user on 2026-10-13:

- Transport owns park is dropped: `TransportCore` already owns the
  transport, park and release are idempotent where it matters, and
  `frozen_by_hook` is announcement policy. The spike's race went out as #47.
- `Outbox` owns the queue and the delivery rules; the worker keeps its glitch
  counters and offers the aggregated warning through `Outbox::offer`.
- A blocked read flushes the outbox on every slice, after publishing
  progress.
- `Worker::emit` syncs `SessionFacts` first, so a flushed event never
  outruns its progress (review P1). The regression is Worker-level, because
  the harness cannot advance the device inside the window between
  `start_running` and step 7's first read.
- The fault budget asks `Outbox::fits`; there is no `len` (review P2, P3).

## Gate

Run on this branch after the last code change.

- `cargo fmt --check`, `clippy -D warnings`, `cargo doc -D warnings`: clean.
- `cargo test --locked --no-fail-fast`: [passed] passed, 0 failed,
  [ignored] ignored.
- Greps (plan Task 4 Step 5): no old names in `src/`; the three capacities
  only in `outbox.rs`; `FAULT_EVENT_BUDGET` reaches the cap only through
  `fits`.
- Under load (`taskset -c 0-1`, 8 `yes` loops): `engine_contract` and
  `wait_service`, 10 runs each, all green.

## Behavior changes

As spec §5: a backlog drains during a blocked read; the hook's events meet
the cap when emitted; a disconnect the hook sees is latched; the event in
flight at a disconnect stays for the shutdown report.
```

- [ ] **Step 7: Commit**

```bash
git add docs/architecture.md docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md CHANGELOG.md docs/m9-e-acceptance.md
git commit -m "docs(m9): the event outbox in architecture, roadmap, changelog; PR E acceptance"
```
