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
- `cargo test --locked --no-fail-fast`: 1501 passed, 0 failed, 3 ignored.
- Greps (plan Task 4 Step 5): no old names in `src/`; the three capacities
  only in `outbox.rs` (a substring grep also hits `spectrum/worker.rs`'s
  unrelated `PENDING_CAPACITY`; a word grep finds nothing);
  `FAULT_EVENT_BUDGET` reaches the cap only through `fits`.
- Under load (`taskset -c 0-1`, 8 `yes` loops): `engine_contract` and
  `wait_service`, 10 runs each, all green (20 of 20).

## Behavior changes

As spec §5: a backlog drains during a blocked read; the hook's events meet
the cap when emitted; a disconnect the hook sees is latched; the event in
flight at a disconnect stays for the shutdown report.
