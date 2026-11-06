# M12: Structural sharing for `PersistedState` snapshots — design

Issue: #69. Source: `docs/project-review-2026-11-01.md` (Medium, performance) and the
`m8` measurement record (`docs/m8-acceptance.md:69-129`).

## Intent and why

Every `Action::Submit` deep-clones the whole `PersistedState` —
`src/session.rs:1548-1553` — and a submit rides every keypress, volume nudge,
metadata result and the 5-s capture tick. At the caps (4,096 queue entries,
512 checkpoints) that clone costs **~2.2–2.3 ms on the app thread** and the
snapshot is **~3.08 MiB on disk** (`docs/m8-acceptance.md:69ff`). The clone tax
also shaped defensive code far from it: metadata bursts are batched into one
submit (`Session::update_displays`, `src/session.rs:658-673`), enrichment
drains are capped per pump (`MAX_ENRICHMENT_PER_PUMP = 64`,
`src/application/runtime.rs:81-92`), and several "submit only when changed"
gates cite the clone in their rationale.

Goal: snapshots stop deep-cloning unchanged subtrees. The writer keeps
receiving a value it owns end to end; the app thread stops paying for it.

## Goals

- Submitting a snapshot no longer deep-clones unchanged subtrees; the
  dominant recurring allocation disappears.
- `state.json` bytes are identical: every pin (`tests/persistence_model.rs`,
  `tests/m8_roundtrip.rs`, recovery suites) passes **unmodified**.
- The clone-cost rationales in the session layer are revisited: each batch
  point is either removed or re-justified on grounds that survive cheap
  snapshots.

## Non-goals

- No change to the stored format or `SCHEMA_VERSION` (stays 4).
- No change to the writer thread, its 2-s coalesce window, or the
  keep-latest slot.
- No delta/event-sourced submission; no merge logic in persistence.
- No finer-than-playlist sharing, no `Arc<str>` string pooling (see
  "Residual costs").
- No behavior change anywhere: same submits on the same triggers, same
  ordering, same coalescing.

## Anti-goals (what would make this a failure even if it works)

- Any way for `persistence` to mutate or merge what the session handed it —
  the §4 invariant "only `session` builds snapshots; persistence must not
  merge snapshots" still holds.
- Sharing that leaks mutability across the writer boundary (a mutation of
  live state visibly changing an already-submitted snapshot).
- Weakening the private-field discipline (`compile_fail` doctests in
  `src/playlist/set.rs` must still fail to compile).

## Constraints

- Toolchain 1.98.1, `--locked`; lints deny `unsafe`, `unwrap`, `expect`
  outside tests.
- `tests/m9_layering.rs` import-rank test stays green and untouched.
- `PersistedState` crosses exactly one thread boundary, by move
  (`Session` → `tenuto-state` writer via `WriterHandle::submit`), so
  `Arc`-wrapped fields must keep the whole value `Send + Sync` — all inner
  types are plain data, so they do.
- The JSON byte contract: custom `Serialize` via the `Out` DTO struct and
  `Deserialize` via `RawState` (`src/persistence/model.rs:75-193`). The
  in-memory shape is free to change; `to_string(&state)` is not.

## Approaches considered

**A. `Arc` copy-on-write subtrees (chosen).** Wrap the two big subtrees in
`Arc` at the right granularity and clone on write via `Arc::make_mut`.
No new dependencies; matches the crate's existing sharing idiom
(`Arc<Vec<QueueRow>>` + `Arc::ptr_eq` view cache in the TUI); serde paths
deref through the `Arc` and stay byte-identical; `PersistedState::default()`
keeps its signature so ~30 suites keep compiling.

**B. Persistent collections (`im`/`imbl`).** O(log n) sharing everywhere,
but a new dependency and an idiom shift across `persistence` and `playlist`
to save microseconds at caps this small. Rejected.

**C. Delta submission.** Send mutations, let the writer apply them. Kills
the clone entirely but moves state-merging into persistence — against the
§4 invariant and the "There is no merge algorithm" rule in §9.1 — and makes
the byte pins fragile on error paths. Rejected; also against the issue's
stated shape ("immutable, shared-subtree state").

## The design

### Type shape

```rust
// src/persistence/model.rs
pub struct PersistedState {
    schema_version: u32,
    current_media: Option<MediaId>,   // one string clone per submit; keep plain
    volume: f32,
    checkpoints: Arc<BTreeMap<MediaId, PersistedCheckpoint>>,
    playlists: PlaylistSet,           // small fields only; sharing lives below
    next_seq: u64,
}

// src/playlist/set.rs
pub struct PlaylistSet {
    playlists: Vec<Arc<Playlist>>,    // the per-playlist sharing unit
    playing: PlaylistId,
    entry_ids: IdAllocator,
    playlist_ids: IdAllocator,
}
```

`Playlist` itself is unchanged: `name`, `shuffle`, `Queue` (which keeps its
plain `Vec<QueueEntry>`). No `Arc` around `PlaylistSet` — its non-playlist
fields are four scalars, and cloning the 32-pointer `Vec` per submit is
already noise. `Queue`'s internals are untouched.

Clone cost per submit becomes: one `Arc` bump for the checkpoint map, a
32-element `Vec` of pointer copies, one `Option<MediaId>` string clone, and
scalars — a few hundred nanoseconds against 2.2 ms.

### Mutation mechanics

Every mutation goes through the existing private mutator chain
(`PersistedState::*` → `PlaylistSet::*` → `Queue::*`); signatures stay.
The two boundaries acquire `Arc::make_mut`:

- In `PlaylistSet`, the helpers that take a playlist writable
  (`playlist_mut`, `entry_mut`, `enqueue`'s indexed access,
  `Recovery::finish`'s cursor fix) use
  `Arc::make_mut(&mut self.playlists[index])`. Editing playlist *k* clones
  playlist *k* alone — and only while the writer still holds an earlier
  snapshot (refcount > 1); in place otherwise. `delete` is *not* a
  make_mut site: it moves `Arc`s out of the vec and never touches a
  playlist's contents.
- In `PersistedState`, `record` and `record_estimated` obtain **one**
  writable map — `let map = Arc::make_mut(&mut self.checkpoints)` — and
  use it for both the eviction decision and the insertion: eviction is
  itself a map mutation, so it must not be left outside the writable
  borrow. `evict_one` is restructured to take that `&mut BTreeMap<…>`
  (plus `current_media`, which its current-entry filter reads) instead of
  `&mut self`; a second `make_mut` inside it would double the share check
  and muddy the borrow story. A 5-s capture touch while the writer holds
  the previous snapshot clones the ≤512-entry map — the cost is pinned by
  the new m12 measurement (see Testing) — instead of the whole state;
  when nothing is retained, `make_mut` finds refcount 1 and the touch
  mutates in place.

`set_volume`, `set_current_media` and the version stamp touch no `Arc`:
volume nudges clone nothing.

**No-op mutations stay clone-free.** Several paths legitimately discover
"nothing changed" and today cost nothing; copy-on-write must not make them
pay. The deciding checks run *before* `make_mut` takes the writable
borrow:

- `move_entry` at a list edge (`Queue::move_entry` returns `Ok(false)`):
  the edge test happens before the owning playlist is cloned — a held
  arrow key at the top or bottom of a full playlist must not clone it per
  keypress.
- `update_display` when `DisplayUpdate::apply_to` would change nothing:
  a `would_change`-style probe (or equivalent reorder) precedes the clone.
- `set_podcast_fallback` when the URL already matches: the equality check
  precedes the clone.

Whether that means a probe method on `Queue`/`Playlist`, a reordering
inside the helper, or a check in `PlaylistSet` is the builder's call — the
requirement is the cost, and the sharing unit tests pin it ("no-op
mutation shares" alongside "real mutation does not").

### Serialization and loading

- `Serialize`'s `Out` struct keeps `checkpoints: &'a BTreeMap<…>`, fed by
  `&self.checkpoints` (deref through the `Arc`); `encode_playlists`
  iterates `&PlaylistSet` as before, now dereferencing each `Arc<Playlist>`.
  Not one output byte changes.
- `Deserialize`/`RawState::into_state` keeps parsing into owned containers
  and wraps once at construction: `checkpoints: Arc::new(…)`.
  `Recovery` keeps building `Vec<Playlist>` internally and wraps in `Arc`
  when `finish` assembles the set.

### Semantics to preserve

The submit handoff is unchanged in law: `self.state.clone()` is shallow now,
but the writer still observes an immutable snapshot — every possible
mutation of live state goes through `Arc::make_mut`, which clones exactly
when the writer might still be looking. The `session.rs:177-179` doc
comment ("carries a clone, which becomes the writer's property") is updated
to say this: the submit hands the writer an owned, shared-subtree snapshot;
mutation is copy-on-write against the writer's claim.

### The rationing workarounds (my call, veto here)

**Kept, with their rationale comments rewritten** — the clone was never
their only justification:

- `update_displays`' one-submit-per-batch: kept to bound writer-thread
  churn and slot pokes when a folder add yields hundreds of results, and to
  keep `apply_display`'s "mutations submit nothing" split intact.
- `MAX_ENRICHMENT_PER_PUMP = 64`: already justified by the per-result scan
  cost, not the clone; its comment stands, the `// ponytail:` note can
  drop the clone framing.
- `update_display`'s change-detection no-op and the
  `checkpoint_from_progress` gate: kept — they suppress pointless writer
  wake-ups and disk writes, not just clones.
- `adopt_loaded`'s submit-after-all-mutations ordering: kept — it is a
  correctness ordering (D19), the clone-cost phrasing is reworded.

If the partner prefers actually deleting the batching, that is a follow-up
issue, not part of m12.

### Residual costs (stated honestly)

- A checkpoint touch (every 5 s while playing) clones the whole
  checkpoints map. If the m12 measurement says that matters, the pre-agreed
  knob is `BTreeMap<MediaId, Arc<PersistedCheckpoint>>` — one more `Arc`
  layer in one module; not in scope now.
- A real mutation of a shared playlist clones that playlist's entries on
  the **first mutating call after a submit** — i.e. once per mutating batch
  while the writer holds the previous snapshot, not once per 2-s window.
  In m8's worst-pump fixture (`the_worst_enrichment_pump_costs_this_much`:
  one playlist holding all 4,096 entries, one submit per pump) that means
  one deep playlist clone per pump either way: **the worst-pump number is
  expected to stay ≈ the m8 baseline, and the acceptance must say so.**
  The headline wins the measurement records are the `clone (app thread)`
  line of `a_full_snapshot_costs_this_much` (2.2 ms → sub-µs) and the
  multi-playlist case; the builder may add a spread-across-playlists
  variant of the drain fixture to show that win directly. Realistic sets
  spread entries across playlists; per-entry sharing stays out of scope,
  revisit only with a measurement.

## Testing

- **Sharing unit tests** (in-crate `#[cfg(test)]`, where the private fields
  are visible): clone a populated state — all subtrees `Arc::ptr_eq`;
  mutate one playlist — its `Arc` differs, the others still match;
  record a checkpoint — the map `Arc` differs, playlists unaffected; volume
  nudge — nothing differs; **no-op mutations** (list edge, matching
  metadata re-send, equal podcast fallback) — nothing differs.
- **Retained-snapshot behavior at capacity** (both recording paths): hold
  a clone of a state at `MAX_ENTRIES` checkpoints, then `record` — and
  separately `record_estimated` — a fresh identity: the retained snapshot
  keeps the evicted victim and every other entry untouched, while the
  live state evicts the lowest-`touch_seq` entry (never the current
  media, never the incoming identity) and inserts the new checkpoint.
- **Boundary immutability**: through the existing `rig_with_sink` harness,
  submit S1, mutate, submit S2, and assert S1 (held by the fake sink) still
  carries pre-mutation values.
- **Unmodified pins must pass**: `tests/persistence_model.rs` (byte-exact),
  `tests/m8_roundtrip.rs` (fixed point), `m5_state_*` recovery suites,
  `m8_state_playlists.rs`, `tests/session_policy.rs`,
  `tests/m9_layering.rs`.
- **Measurement**: `m8_snapshot_size` is re-run by hand (`cargo test
  --release --test m8_snapshot_size -- --ignored --nocapture`) before and
  after; the after-numbers land in `docs/m12-acceptance.md` against the m8
  baseline. `the_worst_enrichment_pump_costs_this_much` likewise (expected
  ≈ baseline; see Residual costs).
- **Checkpoint-touch cost, newly measured (m12)**: the m8 fixtures never
  time a checkpoint mutation under a retained snapshot — the clone
  benchmark populates checkpoints before timing, and the enrichment drain
  mutates playlists — so the cost this design defers onto the 5-s capture
  has no numbers yet. A new hand-run measurement suite (`tests/m12_*`,
  reusing the m8 fixture shapes so the m8 record stays historical) times,
  with 512 representative checkpoints:
  - a `record` touch of an existing identity **while a snapshot is
    retained** — the steady-state shared cost;
  - an insertion that **evicts at `MAX_ENTRIES`** while retained — the
    capacity worst case;
  - the same touches with **nothing retained** — expected in-place
    (~zero), pinning that an unshared capture does not copy.
  The m12 acceptance records all three alongside the clone and pump
  numbers, so the deferred cost is validated, not assumed.

## Docs to update

- `docs/architecture.md`: §9.1 gains a sentence that snapshots share
  unchanged subtrees (`Arc` copy-on-write; per-playlist and whole-map
  checkpoints); the `session` row's "Build every state snapshot" and the
  `persistence` row's "must not merge snapshots" stay as they are — still
  true.
- `src/session.rs` submit-handoff doc comment (§Semantics above).
- `CHANGELOG.md` `[Unreleased]`, Keep a Changelog, hard-wrapped.
- New `docs/m12-acceptance.md` records the acceptance run and measurements.
  Historical docs (`m8-acceptance`, the M2 spec's clone assumption) are
  not rewritten.

## What's left to the builder

- `PlaylistSet`'s `Deref<Target = [Playlist]>` cannot survive as-is over
  `Vec<Arc<Playlist>>`: pick either `Deref<Target = [Arc<Playlist>]>` (call
  sites auto-deref for reads) or an explicit `iter()`/`len()`/`Index`
  surface — whichever leaves `encode_playlists` and the read sites in
  `application/runtime.rs` cleanest, without a `DerefMut`.
- `Recovery` wrap-at-finish vs. building `Arc`s throughout — builder's
  choice; `finish`'s cursor fix mutates a playlist, so wrap after it.
- The in-crate tests that poke the private fields mechanically follow the
  new shape: `evict_one_reports_no_victim_when_the_only_entry_is_current`
  inserts into the map directly and `evict_one`'s signature changes — both
  get the `make_mut` treatment inside `#[cfg(test)]`.
- Batch order, one green landing each, following the crate's history:
  1. `Arc` checkpoints map in `persistence` (+ sharing unit tests);
  2. `Vec<Arc<Playlist>>` in `playlist` (+ plumbing, + sharing unit tests);
  3. rationale rewrites, docs, CHANGELOG, m12 acceptance + measurements.

Milestone label: m12 (acceptance `docs/m12-acceptance.md`, new integration
suites `tests/m12_*` if the harness tests need a home outside the unit
tests).
