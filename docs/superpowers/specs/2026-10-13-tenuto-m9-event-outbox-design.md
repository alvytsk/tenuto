# Tenuto M9: one event outbox

Status: design approved section by section in conversation on 2026-10-13; this written spec awaits review.

Branch: `refactor/m9-event-outbox`, from `main` at `6ad2600`.

Context: PR E of the five that finish M9 ([layering test spec](2026-10-08-tenuto-m9-layering-test-design.md), its PR table). E was a spike over the two open M9.4 items of the [roadmap](2026-09-29-tenuto-m9-architecture-deepening.md) §6. Its outcome:

- **Transport owns park: dropped.** `TransportCore` already owns every piece of transport state: handshake, timeline, anchor, tap. `Phase::Park` is acknowledged on every callback fill (`callback.rs`), and `release` republishes `Run` with a valid tap mapping, so both are already idempotent where it matters. `frozen_by_hook` is announcement and thaw policy, not transport state, and none of the worker's five uses of it goes away with an idempotent transport. A `parked` bit on `TransportCore` would mirror the callback's phase with no reader. The spike did find a send-then-thaw race, fixed separately in #47: the freeze arm now runs only for a blocked read.
- **Event outbox: specified here.** The question: can both emission paths use one ordered queue and one delivery policy?

## 1. Goal

One module owns the engine's outgoing events: the bounded channel's sender, the queue of events waiting for room, and every rule that decides what is sent, held, dropped or displaced. The worker and the wait hook both emit through it. The hook's separate outbox, the `backlog_empty` mirror and the three drain sites that keep the two queues ordered all go.

The hook and the worker are two emission *paths*, not two producer threads. The hook runs synchronously on the worker's own thread, inside `pump_audio` → `next_planar` → `ByteChannel::read` (`http/channel.rs`, the wait loop's `service.service()`). It has its own queue today only because it runs under `&mut self.source` and cannot borrow the rest of `Worker`. A shared mutex is therefore never contended, and no audio callback ever takes it.

## 2. Compatibility boundary

Unchanged:
- The `PlaybackEvent` and `PlaybackCommand` types, `EngineHandle`'s public surface, and `Admission`.
- The delivery rules: `EVENT_CAPACITY` (64), `RESERVED_EVENT_SLOTS` (9), `PENDING_CAP` (128); protected events are never dropped or displaced and may use the reserve; a terminal event may use the reserve and, at the cap, displaces the oldest event that is neither terminal nor protected; an ordinary event never occupies the reserve; FIFO order within the queue.
- Command admission closes while any event is pending. `command_waiting` in `service_reconnect` and `FAULT_EVENT_BUDGET` (8) read the same count as today.
- The aggregated diagnostics `Warning`: its counters, its 2 s interval, and the rule that it goes out only into a free ordinary slot with nothing pending.
- Shutdown: every accepted but undispatched `Load` gets `LoadCancelled`, and `run` returns whatever is still queued as the shutdown report.
- No event reaches the application before the progress state it was emitted in. Today pass order guarantees this: step 2 refreshes `SessionFacts` and publishes progress before step 4 flushes. Once the hook flushes too, §4.3 guarantees it.

Changed on purpose (§5): the four behavior changes listed there, and the crate-internal signatures in §3 and §4.

## 3. `playback::outbox`

A new file, `src/playback/outbox.rs`, declared `pub mod outbox;` in `playback/mod.rs`.

```rust
/// The engine's event channel: the receiver for the application, the
/// outbox for the worker and the wait hook.
pub fn channel() -> (Arc<Outbox>, Receiver<PlaybackEvent>);

pub struct Outbox {
    inner: Mutex<Inner>,
}

struct Inner {
    sender: Sender<PlaybackEvent>,
    pending: VecDeque<PlaybackEvent>,
    dropped: u64,
    disconnected: bool,
}

impl Outbox {
    /// Queue `event` behind everything pending. At `PENDING_CAP`: a
    /// protected event is queued anyway; a terminal one displaces the oldest
    /// event that is neither terminal nor protected, or is dropped when there
    /// is none; an ordinary one is dropped. Every drop or displacement adds
    /// one to `dropped`. Never sends.
    pub fn emit(&self, event: PlaybackEvent);

    /// Send from the front while there is room: an ordinary event only while
    /// more than `RESERVED_EVENT_SLOTS` slots are free, a terminal or
    /// protected one into any free slot. Stops at the first event that does
    /// not fit, so order is kept. On a disconnected receiver, puts the event
    /// back at the front and latches `disconnected`.
    pub fn flush(&self);

    /// Send `event` only if nothing is pending and an ordinary slot is free;
    /// otherwise discard it and return `false`. For the aggregated warning,
    /// which is a snapshot of counters rather than a queued event. A
    /// disconnected receiver latches `disconnected`.
    pub fn offer(&self, event: PlaybackEvent) -> bool;

    /// Whether nothing is pending: command admission and `command_waiting`.
    pub fn is_empty(&self) -> bool;
    /// Whether `extra` more events fit under `PENDING_CAP`: the fault
    /// budget's check, so the cap stays private.
    pub fn fits(&self, extra: usize) -> bool;
    pub fn dropped(&self) -> u64;
    pub fn disconnected(&self) -> bool;

    /// Everything still pending, in order, leaving the queue empty.
    pub fn take_remaining(&self) -> Vec<PlaybackEvent>;
}
```

There is no `len`: every reader needs only one of the two questions above. A public `len` without `is_empty` would also fail `clippy::len_without_is_empty` under the gate.

**Moved in from `engine.rs`:** `EVENT_CAPACITY`, `RESERVED_EVENT_SLOTS` and `PENDING_CAP`, now private to the module; the reserve-budget comment that derives 9; `displacement_victim` and its two unit tests. The `pub(crate)` exports that Ruling 7 added for `wait.rs` go. `EngineHandle::spawn*` builds the channel with `outbox::channel()` instead of `crossbeam_channel::bounded(EVENT_CAPACITY)`. The `bounded(1)` channel in the handle's test-only constructor is not an engine channel and stays.

**Lock rule.** `Inner`'s mutex is a leaf: no method calls out of the module while holding it, and no guard leaves the module, so it cannot be held across a decoder read or a hook call. `try_send` never blocks. A poisoned lock is taken over as `wait.rs`'s `lock` does. The hook takes this lock while it holds `SessionFacts` (§4.2). That is safe because the outbox lock is taken last and never held while taking another.

**Visibility.** `pub`, for the same reason `TransportCore::new` is: `tests/wait_service.rs` builds a `WaitService` with no `Worker`, and needs an `Outbox` to do it.

## 4. The two emission paths

### 4.1 Worker

- `Worker` loses `pending_events`, `events: Sender<PlaybackEvent>`, `dropped_events` and `backlog_empty`, and gains `outbox: Arc<Outbox>`.
- `emit` keeps its name and becomes `self.sync_facts(); self.outbox.emit(event)` (§4.3). `cancel_load`, `warn` and every other emitter are unchanged.
- `flush_events` becomes `self.outbox.flush()` followed by the disconnect check below. `free_event_slots`, `note_backlog` and `drain_outbox` are deleted.
- `emit_aggregated_warning_if_slot_free` keeps its counters and `DIAGNOSTIC_INTERVAL` gate. Its total reads `self.outbox.dropped()` in place of `dropped_events`. It sends through `self.outbox.offer(..)`, and updates `reported_diagnostics` and `last_diagnostic` only when the offer returns `true`. This matches today: the worker checks for room before it sends, and the application thread can only free slots in between, so the `Full` result today's code ignores never occurs.
- `run` step 0 and its comment go, along with the drains in `check_end_of_track` and `shutdown` and their comments. With one queue, nothing can reorder ahead of the hook's events.
- `admit` and `command_waiting` read `self.outbox.is_empty()`. The fault budget defers when `!self.outbox.fits(Self::FAULT_EVENT_BUDGET)`, and `FAULT_EVENT_BUDGET` stays the worker's: it is the worker's own count of what a fault emits.
- `receivers_gone` stays the worker's own flag, set as today by the liveness `select!` and by `RecvError`. It is also set from `self.outbox.disconnected()` after each `flush` and `offer`. That covers a disconnect the hook latched (§4.2), because the worker flushes every pass.
- Every `return Vec::from(self.pending_events)` becomes `return self.outbox.take_remaining()`.

### 4.2 Wait hook

- `WaitService` loses `events`, `outbox` and `backlog_empty`, and gains `outbox: Arc<Outbox>`. `WaitService::new` takes that one argument in their place, which brings it from eight arguments to six, so its `too_many_arguments` allowance and that comment go. `take_outbox` is deleted.
- `announce` keeps its name, so the freeze arm reads as before, and becomes `self.outbox.emit(event)`. The ordering check, the room check and the direct `try_send` go.
- `service_as` ends, after `publish_progress`, with `self.outbox.flush()` when `servicing == Servicing::BlockedRead`. So a blocked read flushes on every slice. A `WorkerLoop` pass does not flush, because the worker's step 4 does right after it.

### 4.3 Progress before delivery

The hook's `publish_progress` builds `Progress` from `SessionFacts`, and only the worker writes the fields it mirrors there. Until now that was safe because only the worker delivered worker events, and it refreshed the facts first (step 2 before step 4). Once a blocked read can flush the worker's events, a read that blocks *during* a command would deliver them against facts from before the command.

The case that breaks: a finite remote track is stopped, a seek target is stored, and Play runs `restore()`. `restore` keeps `session_rev`, lands at the target and queues `StateChanged{Playing}` and `SeekCompleted`. Then step 7's `pump_audio` blocks. The hook would flush both events while the facts still describe the stopped position. `Session` arms `pending_force` on `SeekCompleted` and checkpoints the next progress sample of that revision, which would be the stopped position, written over the seek the listener made.

The invariant: **`SessionFacts` are never older than the newest event in the outbox.**

- The half of `Worker::publish_progress` that copies the worker's fields into `SessionFacts` (`session_rev`, `media`, `position`, `degraded`, `playing`, `provenance`, `load`) becomes `Worker::sync_facts`. `publish_progress` calls it and then `service_as(WorkerLoop)` as today.
- `Worker::emit` calls `sync_facts` before `outbox.emit`. Emit is the one point every event passes through, whereas a blocked read can happen in many places during a command (`land`'s seek, an open and its probe, priming, the pump). So one call covers reads during command execution as well as the loop's own.
- In `service_as(BlockedRead)` the order is: freeze arm, then `publish_progress`, then `flush`. Whatever the hook delivers was emitted no later than the facts its progress was built from.
- Cost: one uncontended facts lock and one `MediaId` clone per event. Events are per command, not per sample.

## 5. Behavior changes

1. **A backlog drains during a blocked read.** Events the worker queued before it blocked reach the application as room frees, not when the read returns. Command admission still reopens only at the worker's next pass, after the read returns.
2. **The hook's events meet the cap when emitted.** Today they wait in an unbounded outbox and meet `PENDING_CAP` when `drain_outbox` routes them through `emit`. The drop and displacement policy is the same, applied earlier.
3. **A disconnect the hook sees is latched.** Today the hook discards the event and the worker learns only from its own next send. Now the worker reads the latch after its next flush.
4. **The event in flight at a disconnect is kept.** Today `flush_events` loses the event whose send failed with `Disconnected`. Now it goes back to the front, so the shutdown report carries it, consistent with D19's losslessness.
Nothing else observable changes. The hook's former fast path, a direct `try_send` when the queue was empty and there was room, gives the same result as emit-then-flush under the same reserve rule.

## 6. Tests and documentation

### 6.1 `outbox.rs` unit tests

Built from `channel()`, with the receiver held and drained only when a test says so:
- At `PENDING_CAP`, an ordinary event is dropped and `dropped()` counts it.
- At the cap, a terminal event displaces the oldest ordinary event and never a protected one. The two `displacement_victim` tests move here unchanged.
- A protected event is queued past the cap.
- `flush` holds an ordinary event once only `RESERVED_EVENT_SLOTS` slots are free, lets a terminal or protected event into them, and never sends past a held event (FIFO).
- `offer` returns `false` and sends nothing while anything is pending, or while only reserved slots are free; it returns `true` and sends otherwise.
- With the receiver dropped, `flush` latches `disconnected()` and the unsent event is still first in `take_remaining()`.
- `take_remaining` returns events in emission order and leaves `is_empty()` true.
- `fits(n)` is true up to `PENDING_CAP - pending` and false past it.

### 6.2 `tests/wait_service.rs`

- Every constructor takes `outbox::channel()` in place of a sender, an outbox and a `backlog_empty` flag.
- `a_hook_announcement_goes_to_the_outbox_when_the_workers_backlog_is_not_empty` becomes `a_hook_announcement_queues_behind_the_workers_backlog_and_a_blocked_read_delivers_both_in_order`: emit two ordinary events into the outbox, raise the freeze, call `service()`, and receive the two events and then `StateChanged{Paused}`.
- New: `a_blocked_read_flushes_a_backlog_the_worker_left`: fill the channel to its ordinary limit, emit one more (held), drain one from the receiver, call `service()`, and receive the held event.
- The freeze/thaw announcement test keeps its assertions, with its constructor updated.

### 6.3 Engine level

No new tests. These pin the end-to-end rules and must pass unchanged:
- `engine_contract`: `stop_travels_out_of_band_past_a_pending_backlog`, `a_backlog_closes_command_admission_until_it_drains`, `diagnostics_aggregate_rather_than_accumulating_events`, `a_disconnected_event_receiver_terminates_the_worker`, and #47's `a_play_dispatched_before_its_thaw_is_not_parked_again`.
- `engine_shutdown`.

One new engine regression, in `tests/engine_remote.rs`: `a_stored_seek_delivered_during_a_held_read_reports_the_target_not_the_stopped_position`. Steps:
1. The server script serves the load, then `.then(..)` a reopen whose body stalls (`stall_body_after(N)`). N is chosen so the reopen's priming completes and the read that blocks is the loop's own pump after `restore` has queued its events. The plan fixes N against the fixture.
2. Load, play for a moment, `Stop`, and `SeekTo` a target past the stopped position. Await `SeekTargetStored`, then submit `Play`.
3. `wait_until_stalled`, then await `SeekCompleted` while the read is still held. Only hook-side flushing can deliver it then.
4. Assert that the published `Progress` with the event's `session_rev` is at the target, not the stopped position. Then `release`.

Against hook flushing without §4.3 it fails on that assertion; before hook flushing it cannot reach step 3. The plan orders the work so the test is seen to fail before `sync_facts` lands.

### 6.4 Documentation

- `docs/architecture.md` §7.1: the event paragraph names `Outbox` (`playback::outbox`) as the one queue both emission paths share, replaces "wait in `pending_events`" accordingly, and states that its lock is a leaf.
- `docs/architecture.md` §12, the M9 line: the event outbox has shipped; the transport item is dropped, with a pointer to the roadmap's reason.
- The [roadmap](2026-09-29-tenuto-m9-architecture-deepening.md) §6: a status line under "Transport owns park" (dropped, with the reason above and #47) and under "Event outbox" (shipped, `playback::outbox`).
- `CHANGELOG.md` `[Unreleased]` / Internal: one line. The engine's events leave through one queue, and events held back while the application was not reading now go out during a long network read rather than after it.
- `docs/m9-e-acceptance.md`: the acceptance record.

## 7. Verification

- The full gate in `CLAUDE.md`: `fmt --check`, `clippy -D warnings`, `cargo test --locked --no-fail-fast`, `RUSTDOCFLAGS="-D warnings" cargo doc`.
- `tests/m9_layering.rs` passes. `outbox` is inside `playback`, so it adds no edge between layers.
- A `grep` over `src/` finds no `backlog_empty`, `take_outbox`, `drain_outbox`, `note_backlog` or `pending_events`, and finds `RESERVED_EVENT_SLOTS`, `EVENT_CAPACITY` and `PENDING_CAP` defined only in `outbox.rs` and used nowhere else. `FAULT_EVENT_BUDGET` stays in `engine.rs` and reaches the cap only through `fits`.
- The engine suites under the load recipe in `CLAUDE.md` (the test binary pinned with `taskset -c 0-1` beside about 8 `yes` loops): `engine_contract`, `wait_service` and `engine_remote`'s new regression, 10 runs each, all green.
- Lock cost is not measured. One uncontended mutex per emit and per slice replaces an atomic plus, on the hook path, a mutex. Measure only if a representative probe shows it.

## 8. Out of scope

- Transport ownership of park (dropped, see Context).
- The duplicate `Paused`/`Playing` pair replayed when a pause and a play both land during one blocked read. It comes from the worker dispatching both commands after the read returns, not from event delivery.
- Changing any capacity, the reserve size or the drop policy.
- Progress delivery: it stays the keep-latest `Progress` slot and never enters the outbox.
