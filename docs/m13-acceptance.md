# M13 Engine split — acceptance

Scope: the 2026-11-01 project review's Low (structural) finding, item 5 —
`src/playback/engine.rs` (4,270 lines) split into focused modules with
single-responsibility names (issue #68, plan
`docs/superpowers/plans/2026-11-08-engine-split.md`). No behavior change;
each batch landed green independently; the public API paths
(`playback::engine::{EngineHandle, EventStream, TransportCore, DEADLINE,
PUMP_NAP}`) never moved.

Branch `fix/70-low-sweep` (shared with the #70 sweep that preceded it).
Batches, each a single commit, each gated by fmt + clippy `-D warnings` +
the full suite (116 test binaries, 0 failures):

| Batch | Commit | Move |
|---|---|---|
| 1 | 17da38a | `engine.rs` → `engine/{mod.rs, handle.rs, worker.rs}`: module root + consts + re-exports; the front-of-house handle; the decode worker and its command state machine |
| 2 | 4b7dd0e | `TransportCore`/`TransportTap` → `engine/transport.rs` (wait.rs's import path unchanged via the mod.rs re-export) |
| 3 | df11f50 | the reconnect loop (`enter_reconnecting`, `service_reconnect`, `reconnect_finite`) → `engine/reconnect.rs` |
| 4 | ecc0321 | the glitch counters (`GlitchCounters`) and the reporting cluster (event emission, progress, facts, faults, capture/teardown, `severity`/`retire_faults`) → `engine/diagnostics.rs` |

## Acceptance criteria

- [x] Engine concerns live in separate modules with single-responsibility names —
  `engine/{mod,handle,worker,transport,reconnect,diagnostics}.rs`.
- [x] No behavior change: all gates green after every batch; the layering
  test (`tests/m9_layering.rs`) is untouched and passes unchanged (the new
  modules are children of `playback::engine` and import nothing new
  outside the crate); `rustdoc` warnings gate clean on the final batch.
- [x] Each batch lands green independently (no big-bang reopen).

## Notes

- Module-scope privacy stayed inside the engine tree: Worker fields and
  helpers the sibling modules drive are `pub(super)`, never `pub`. The
  public surface is byte-identical (`playback::engine::` re-exports in
  `mod.rs`).
- The reconnect loop's unit tests stay in `worker.rs`'s test module: they
  drive the whole `Worker` state machine, not the loop in isolation. The
  aggregated-warning test now reaches the counters through
  `worker.glitches`.
- Batch 4 is narrower than the plan table sketched in one respect: the
  diagnostics module holds the glitch counters and the reporting/fault
  cluster as an `impl Worker` block plus the `severity`/`retire_faults`
  free fns; `sync_facts` moved with it. `pump_audio` and the command
  state machine stay in `worker.rs`, which keeps the worker's run loop
  readable rather than scattering it across six files.
