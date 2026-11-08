# Engine split (#68) — plan

Date: 2026-11-08. Issue: #68. Source: the 2026-11-01 project review's Low
(structural) finding and suggested order of attack, item 5; the 2026-11-07
architecture review's C2.

## Goal

`src/playback/engine.rs` (4,270 lines) mixes four concerns behind a
40+-field worker struct. Split it into focused modules with
single-responsibility names, no behavior change, each batch landing green
independently — the crate's ordinary batch discipline.

## Module shape

`engine.rs` becomes a directory module so the public path
`crate::playback::engine` never moves:

| Module | Concern | Moves |
|---|---|---|
| `engine/mod.rs` | module wiring, re-exports | declarations only |
| `engine/handle.rs` | the front-of-house handle: `EventStream`, `SourceTraits`, `EngineHandle`, admission and interrupts | lines ~99–569 |
| `engine/worker.rs` | the worker state machine: `Worker` + fields, `new`, `run`, command dispatch, load/play/restore/pause/stop/seek/restart/land, `Approach`/`Landing`/`Landed`/`PendingResume`, `pump_audio`, `check_end_of_track`, the shared free fns (`stop_or_shutdown`, `seek_refined`, `clamp_target`, `established_duration`) | lines ~685–3557, 3558–3950 |
| `engine/transport.rs` | `TransportCore`, `TransportTap` | lines ~572–684 |
| `engine/reconnect.rs` | the reconnect cluster: `fresh_open`, `abandon_attempt`, `reconnect_finite`, `prime_attempt`, `settle_heard`, `source_ended`, `recoverable`, `connection_lost`, `enter_reconnecting`, `service_reconnect`, `confirm_remote_completion`, `rebuild`/`open_transport`/`reinstall` transport-lifecycle glue stays with transport | see batches |
| `engine/diagnostics.rs` | `emit`, `flush_events`, warning aggregation, `sync_facts`, `publish_progress`, `collect_diagnostics`, `service_faults`, `capture_position`, `capture_and_teardown`, `teardown`, free fns `severity`/`retire_faults` | lines ~1077–1493, 1960–2050 |

Public API stability: `engine::{EngineHandle, EventStream, TransportCore,
DEADLINE, PUMP_NAP, …}` keep their paths through `mod.rs` re-exports —
`wait.rs` imports `TransportCore`, `DEADLINE` and `PUMP_NAP` from
`super::engine`, and `app.rs`/`tui` import `EngineHandle`; none of them
may change.

## Batches (each lands green independently)

1. **State machine** — `engine.rs` → `engine/{mod.rs, worker.rs,
   handle.rs}`: `Worker` and the command state machine move to
   `worker.rs`; the handle moves to `handle.rs`; `mod.rs` keeps the rest
   temporarily. Worker fields become `pub(super)` where sibling batches
   will need them.
2. **Transport** — `TransportCore`/`TransportTap` move to
   `engine/transport.rs`, re-exported from `mod.rs`.
3. **Reconnect** — the reconnect cluster moves to `engine/reconnect.rs`.
4. **Diagnostics** — events/progress/faults move to
   `engine/diagnostics.rs`.

## Gate per batch

`cargo fmt --check`, `cargo clippy --locked --all-targets --all-features
-- -D warnings`, `cargo test --locked --no-fail-fast`, plus
`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` on the final
batch. The layering test (`tests/m9_layering.rs`) is untouched and must
pass unchanged: the new modules are children of `playback::engine` and
import nothing new outside the crate.

## Acceptance (issue #68)

- [ ] Engine concerns live in separate modules with single-responsibility names
- [ ] No behavior change: all gates green, layering test untouched
- [ ] Each batch lands green independently (no big-bang reopen)

Record: `docs/m13-acceptance.md`.
