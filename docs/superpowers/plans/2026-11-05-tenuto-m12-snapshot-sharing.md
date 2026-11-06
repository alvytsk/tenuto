# M12 Snapshot Sharing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `PersistedState` snapshots share unchanged subtrees (`Arc` copy-on-write over the checkpoint map and each playlist), so a submit costs refcount updates instead of a ~2.2 ms deep clone, with `state.json` byte-identical.

**Architecture:** Two `Arc` boundaries, both hidden inside existing private mutator chains: `checkpoints: Arc<BTreeMap<…>>` in `persistence::model`, `playlists: Vec<Arc<Playlist>>` in `playlist::set`. Mutators keep their signatures; `Arc::make_mut` clones only the touched subtree and only while the writer holds a previous snapshot. No-op mutations (list-edge moves, equal metadata re-sends, equal fallback URLs) run their deciding check before the writable borrow so they stay clone-free.

**Tech Stack:** Rust 1.98.1 (pinned), `std::sync::Arc` only — no new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-05-tenuto-m12-snapshot-sharing-design.md`

## Global Constraints

- Toolchain pinned to 1.98.1 via `rust-toolchain.toml`; **every** cargo command uses `--locked`.
- Gates after each task, all green: `cargo fmt --check` · `cargo clippy --locked --all-targets --all-features -- -D warnings` · `cargo test --locked --no-fail-fast` · `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`.
- Runtime code: no `unsafe`, no `unwrap`, no `expect` (tests may use them; `clippy.toml` already allows).
- `SCHEMA_VERSION` stays 4; `tests/persistence_model.rs` passes **unmodified** (byte-exact JSON pin).
- `tests/m9_layering.rs` passes **unmodified** (`use std::sync::Arc` is invisible to its scanner).
- `Cargo.toml` untouched — no new dependencies.
- Public mutator signatures on `PersistedState`/`PlaylistSet`/`Session` do not change; the only signature change is the private `PersistedState::evict_one`.
- Work on branch `m12-snapshot-sharing` off `main`.

## Review Focus

Failure modes the spec implies but a person using the player would feel, most likely first. Each is pinned by the named task's test.

1. **Held-arrow-key keypress at a playlist edge** must stay a no-op: `Ok(false)`, no submit, and **no playlist clone** (a per-keypress deep clone would be worse than the pre-m12 status quo). → Task 2, `a_move_at_an_edge_clones_nothing`.
2. **Re-sending already-stored metadata, or an already-matching podcast fallback URL** must stay `Ok(false)`/`Action::None` with no clone, and a non-podcast entry must still return `Err(SourceMismatch)` — the probe restructure must not swallow it. → Task 2, `a_matching_metadata_resend_clones_nothing`, `a_matching_fallback_url_clones_nothing`, `a_non_podcast_entry_still_reports_source_mismatch`.
3. **Eviction at the 512 cap while a snapshot is retained** must leave both sides correct: the retained snapshot keeps its evicted victim, the live state evicts the lowest `touch_seq` and inserts. Both recording paths. → Task 1, `record_at_the_cap_leaves_a_retained_snapshot_whole`, `record_estimated_at_the_cap_leaves_a_retained_snapshot_whole`.
4. **The writer's view of a submitted snapshot mid-burst**: mutate after submitting; the earlier snapshot must still carry pre-mutation values. → Task 2, `tests/m12_sharing.rs` boundary test.
5. **Byte drift through the `Arc` derefs** (key order, absent-vs-null `estimated`): any serde slip lands here. → every task re-runs `tests/persistence_model.rs` unmodified.

---

### Task 1: The checkpoint map is shared copy-on-write

**Files:**
- Modify: `src/persistence/model.rs` (field type, `Default`, `RawState::into_state`, `Serialize`, `record`, `record_estimated`, `evict_one`, unit tests)
- Test: `src/persistence/model.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing new.
- Produces: `PersistedState` with private field `checkpoints: Arc<BTreeMap<MediaId, PersistedCheckpoint>>`; associated fn `fn evict_one(map: &mut BTreeMap<MediaId, PersistedCheckpoint>, current_media: Option<&MediaId>, incoming: &MediaId) -> bool` (private, called as `Self::evict_one(...)`). All public signatures unchanged. Task 2 and 3 build on the compiled shape.

- [ ] **Step 1: Create the branch**

```bash
git checkout -b m12-snapshot-sharing
```

- [ ] **Step 2: Write the failing unit tests** in `src/persistence/model.rs`, `mod tests` (it has an `id(name)` helper already). Use `std::sync::Arc::ptr_eq` on the private field — visible to in-crate tests. New tests:

  - `a_clone_shares_the_checkpoint_map`: default state, `record` one checkpoint; `let snapshot = state.clone();` → `Arc::ptr_eq(&state.checkpoints, &snapshot.checkpoints)` holds.
  - `recording_disturbs_neither_the_retained_snapshot_nor_its_sharing_base`: two checkpoints recorded; `let snapshot = state.clone();` (now the live map is shared); `state.record(...)` a third media → `!Arc::ptr_eq(&state.checkpoints, &snapshot.checkpoints)`; `snapshot.len() == 2` and `snapshot.entry_for(&old)` still returns its old `touch_seq`; `state.entry_for(&new)` is `Some`.
  - `record_at_the_cap_leaves_a_retained_snapshot_whole`: build 512 checkpoints (`0..MAX_ENTRIES`, distinct `id()` media, ascending `touch_seq` via successive `record` calls with varied positions); `let snapshot = state.clone();`; `state.record(...)` a fresh identity (not `current_media`, which is `None` here) → snapshot: `len() == 512`, still contains the victim that had the lowest `touch_seq` among non-current entries; live: `len() == 512`, victim absent, fresh identity present with `touch_seq == 512 + 1`.
  - `record_estimated_at_the_cap_leaves_a_retained_snapshot_whole`: same construction, but the final write is `state.record_estimated(fresh_media, estimated, updated_at, false)`; same assertions on both sides, plus the live entry has `position: None, estimated: Some(...)`.
  - Rewrite `evict_one_reports_no_victim_when_the_only_entry_is_current` for the new signature: build the map and `current_media` by hand, call `PersistedState::evict_one(&mut map, Some(&only), &id("incoming"))` → `!evicted`, map still length 1.

- [ ] **Step 3: Verify the new tests fail**

Run: `cargo test --locked --test persistence_model && cargo test --locked --lib persistence::model`
Expected: the new tests fail to **compile** (field is not yet `Arc`, `evict_one` has the old signature). That is the failure.

- [ ] **Step 4: Implement**

  1. Field: `checkpoints: Arc<BTreeMap<MediaId, PersistedCheckpoint>>`; add `use std::sync::Arc;`. Update the field doc comment to say it is `Arc`-shared copy-on-write against the writer's retained snapshots.
  2. `Default`: `checkpoints: Arc::new(BTreeMap::new())`.
  3. `RawState::into_state`: `checkpoints: Arc::new(self.checkpoints)` (after `next_seq` is derived from it — derive first, then move into the `Arc`).
  4. `Serialize`: the `Out` field stays `checkpoints: &'a BTreeMap<MediaId, PersistedCheckpoint>`; the init `checkpoints: &self.checkpoints` deref-coerces through the `Arc`. No other change — output bytes must not move.
  5. `record`: reads (`contains_key`, `len`) stay on the `Arc` deref; then `let map = Arc::make_mut(&mut self.checkpoints);` **once**, before the cap check; eviction and insert go through `map`:

     ```rust
     let map = Arc::make_mut(&mut self.checkpoints);
     if fresh && map.len() >= MAX_ENTRIES {
         let evicted = Self::evict_one(map, self.current_media.as_ref(), &checkpoint.media);
         if !evicted { /* existing debug_assert + tracing::warn, unchanged; early return */ }
     }
     let touch_seq = self.next_seq;
     self.next_seq = self.next_seq.saturating_add(1);
     map.insert(checkpoint.media.clone(), PersistedCheckpoint { /* unchanged */ });
     ```
     (On the unreachable guard path the map is left writable-but-unshared; harmless.)
  6. `record_estimated`: same pattern — one `make_mut`, then `map.entry(media).or_insert_with(...)` and the field writes, unchanged otherwise.
  7. `evict_one`: restructure to the new signature (static associated fn, no `self`); body unchanged except `self.checkpoints` → `map` and the filter reads `current_media.copied()`. Keep its doc comment, adjusted to say it runs on a writable map the caller obtained.
  8. Read accessors (`len`, `is_empty`, `entry_for`, `completed_for`) deref through the `Arc` — no change.

- [ ] **Step 5: Verify tests pass**

Run: `cargo test --locked --lib persistence::model` → PASS (all, new and old).
Run: `cargo test --locked --test persistence_model --test m8_roundtrip --test m5_state_schema --test m5_state_recovery --test m8_state_playlists --test m4_state_snapshot` → PASS, unmodified.

- [ ] **Step 6: Full gates**

Run the four Global Constraints gate commands. All green.

- [ ] **Step 7: Commit**

```bash
git add src/persistence/model.rs
git commit -m "perf(persistence): the checkpoint map is shared copy-on-write"
```

---

### Task 2: Each playlist is shared copy-on-write; no-op mutations stay clone-free

**Files:**
- Modify: `src/playlist/set.rs` (`PlaylistSet.playlists` type, `Default`, `Deref`, `playlist_mut`, `entry_mut` via `playlist_mut`, `enqueue`, `create`, `delete` — no `make_mut` there — `move_entry`, `update_display`, `set_podcast_fallback`, `Recovery`, doc-comment adjustments)
- Modify: `src/playlist/queue.rs` (`Queue::move_would_change`, `DisplayUpdate::would_change`, their tests in the existing `#[cfg(test)]` module at queue.rs:381)
- Modify: `src/persistence/queue_codec.rs:179` (`encode_playlists` parameter type)
- Modify: `src/persistence/model.rs` (`#[cfg(test)] mod playlist_tests` — mechanical `[0]` indexing keeps working via the new `Deref`; add clone-sharing assertions)
- Test: `tests/m12_sharing.rs` (new integration suite)

**Interfaces:**
- Consumes: Task 1's `PersistedState` (compiles unchanged against it).
- Produces: `PlaylistSet` with private field `playlists: Vec<Arc<Playlist>>`; `impl Deref for PlaylistSet { type Target = [Arc<Playlist>]; }` (still no `DerefMut`); `pub(crate) fn encode_playlists(playlists: &[Arc<Playlist>]) -> Vec<PlaylistDto>`; `pub(in crate::playlist) fn Queue::move_would_change(&self, id: QueueEntryId, direction: Direction) -> bool`; `pub(in crate::playlist) fn DisplayUpdate::would_change(&self, display: &DisplayMetadata) -> bool`. External read sites (`.iter()`, `playlist()`, `playing_playlist()`, `find_entry()`, `runtime.rs:1625`'s `playlists[next].id()`) are transparent under the new `Deref` — verified; expect **zero** churn outside the files listed.

- [ ] **Step 1: Write the failing probe tests** in `queue.rs`'s test module:

  - `move_would_change_agrees_with_move_entry`: a queue with three entries — top: `!move_would_change(Up)` and `move_entry(Up) == Ok(false)`; middle: both `true`/`Ok(true)` (and the order actually swaps); bottom: `!move_would_change(Down)` and `move_entry(Down) == Ok(false)`; absent id: `move_would_change == false` while `move_entry` is `Err(UnknownEntry)` (the probe never swallows the error path — `PlaylistSet` checks membership via `owner_of` before probing).

- [ ] **Step 2: Write the failing set-sharing tests** in a new `#[cfg(test)] mod tests` in `set.rs`. Build sets via `PlaylistSet::default()` + `create` + `enqueue` (`NewQueueEntry::new`, mirroring the `entry(name)` helper in `model.rs`'s `playlist_tests`). Tests:

  - `a_clone_shares_every_playlist`: clone → `Arc::ptr_eq` holds for every index.
  - `renaming_one_playlist_clones_only_that_playlist`: hold clone; `rename(p1, ...)` → index of `p1` no longer `ptr_eq`, every other index still is.
  - `enqueuing_clones_only_the_destination`: hold clone; `enqueue(p2, batch)` → only `p2`'s slot differs.
  - `a_move_at_an_edge_clones_nothing`: enqueue one entry into `p1`; hold clone; `set.move_entry(that_id, Direction::Up)` → `Ok(false)` **and** every index still `ptr_eq` (the pin from Review Focus 1). Also `Ok(false)` at the bottom with `Down` on a one-entry set.
  - `a_real_move_clones_the_owner_and_shares_the_rest`: two entries, `move_entry(bottom, Up)` → `Ok(true)`, owner slot differs, others `ptr_eq`, and the held snapshot still iterates in the old order (value-level check through the clone).
  - `a_matching_metadata_resend_clones_nothing`: `update_display(id, &update)` once (changes, `Ok(true)`); hold clone; re-send the same `update` → `Ok(false)` and all slots still `ptr_eq`.
  - `a_matching_fallback_url_clones_nothing`: a podcast entry (`QueueSource::Podcast { fallback }`, media `MediaId::PodcastEpisode`); set fallback U (Ok(true)); hold clone; set U again → `Ok(false)`, all `ptr_eq`.
  - `a_non_podcast_entry_still_reports_source_mismatch`: `set_podcast_fallback` on a `LocalFile` entry → `Err(QueueError::SourceMismatch)` (Review Focus 2's error path).
  - `deletion_moves_arcs_without_make_mut`: hold clone; `delete(p2)` → succeeds, no panic, surviving slots' contents unchanged in the clone.

- [ ] **Step 3: Write the failing boundary test** — create `tests/m12_sharing.rs`:

  - Test `a_submitted_snapshot_does_not_change_underneath_the_writer`, through the real writer path (`rig_with_sink`, per the spec): a `RetainingSink` holding `Arc<Mutex<Vec<PersistedState>>>`, cloned in `write(&self, state: &PersistedState)` and always `Ok(())` (test code may `unwrap` the mutex). Build a rig on a state with one queue entry (`rig_with_sink` from `tests/support/runtime.rs`; build the initial state through a `Session` and take `session.state().clone()`, as `m8_snapshot_size.rs` does). Then: submit once (a first `enqueue` via the `support::enqueue` helper), `pump_until` the sink holds ≥ 1 write (the 2-s coalesce window passes during the stepping) — that write is S1; submit again (second `enqueue`), `pump_until` ≥ 2 writes — S2. Assert: `sink[0]` still has exactly the first entry and none of the second; `sink[1]` has both; and `sink[0]`'s entry order still matches what was submitted before any later mutation. No manifest edit — `tests/*.rs` files are auto-discovered.

- [ ] **Step 4: Verify the new tests fail**

Run: `cargo test --locked --lib playlist` and `cargo test --locked --test m12_sharing`
Expected: compile failures (`move_would_change`, `would_change` not defined; set tests reference `Arc::ptr_eq` on non-`Arc` slots).

- [ ] **Step 5: Implement the queue probes**

  1. `Queue::move_would_change` as declared; body:

     ```rust
     let Some(index) = self.index_of(id) else { return false };
     match direction {
         Direction::Up => index > 0,
         Direction::Down => index + 1 < self.entries.len(),
     }
     ```
     `Queue::move_entry` itself is **unchanged** (it keeps its own presence error).
  2. `DisplayUpdate::would_change` as declared; read-only mirror of `apply_to`'s comparisons: a present string field differs (`update.is_some() && field.as_ref() != update.as_ref()`), or `self.duration.is_some() && display.duration != self.duration`. `apply_to` unchanged. A test pins agreement: `would_change_agrees_with_apply_to` (equal fields → `(false, false)`; differing title → `(true, true)`; absent update fields leave both `false`).

- [ ] **Step 6: Implement the set changes**

  1. Field `playlists: Vec<Arc<Playlist>>`, `use std::sync::Arc;`.
  2. `Default` and both `Recovery::finish` exits wrap in `Arc::new(...)`; `Recovery` keeps building `Vec<Playlist>` internally and wraps at `finish` (after the cursor fix, which mutates a playlist).
  3. `Deref` target becomes `[Arc<Playlist>]`; doc comment keeps "There is no `DerefMut`".
  4. `playlist_mut`: `Some(Arc::make_mut(&mut self.playlists[index]))` — the only new clone site behind `entry_mut`, `rename`, `set_shuffle`, `clear`, `adopt` (all genuinely-mutating paths).
  5. `enqueue`: pre-checks stay first (capacity via `total_entries()`); then `let playlist = Arc::make_mut(&mut self.playlists[index]);`; the rest of the body is unchanged.
  6. `create`: `self.playlists.push(Arc::new(Playlist::new(id, name)));`
  7. `delete`: unchanged — `Vec::remove` moves the `Arc` out; no `make_mut` (Review Focus: deleting must not clone the deleted playlist). Keep/extend its doc comment to say so.
  8. `move_entry` restructure — probe before the writable borrow:

     ```rust
     let owner = self.owner_of(id).ok_or(QueueError::UnknownEntry(id))?;
     let playlist = self.playlist(owner).ok_or(QueueError::UnknownEntry(id))?;
     if !playlist.queue().move_would_change(id, direction) {
         return Ok(false);
     }
     self.playlist_mut(owner)
         .ok_or(QueueError::UnknownEntry(id))?
         .queue
         .move_entry(id, direction)
     ```
  9. `update_display` restructure: `find_entry` (immutable) → `!update.would_change(found.display())` → `Ok(false)`; else `entry_mut` + `apply_to`. `set_podcast_fallback` restructure the same way: immutable equality probe → `Ok(false)`; else `entry_mut` + `set_source` (so `SourceMismatch` for a non-podcast is untouched).
  10. `queue_codec::encode_playlists(playlists: &[Arc<Playlist>])`: the body's `playlist` bindings deref transparently; only the signature and the call in `model.rs` (already passing `&self.playlists`, which coerces) change.
  11. `model.rs` `playlist_tests`: existing `[0]`-indexing and accessor reads work unchanged; extend `a_fresh_state_has_one_playing_playlist_named_default` with a clone-and-`ptr_eq` assertion if convenient — optional.

- [ ] **Step 7: Verify tests pass**

Run: `cargo test --locked --lib playlist persistence::model` → PASS.
Run: `cargo test --locked --test m12_sharing --test persistence_model --test m8_roundtrip --test m5_state_schema --test m5_state_recovery --test m8_state_playlists --test m4_state_snapshot --test session_policy` → PASS.
Run the compile_fail doctests: `cargo test --locked --doc` → PASS (the `set.rs` doctests still fail to compile as required).

- [ ] **Step 8: Full gates**

All four gate commands green, `--no-fail-fast` on tests.

- [ ] **Step 9: Commit**

```bash
git add src/playlist/ src/persistence/ tests/m12_sharing.rs
git commit -m "perf(playlist): each playlist is shared copy-on-write and no-op mutations stay clone-free"
```

---

### Task 3: Rationale rewrites, docs, measurement, acceptance

**Files:**
- Modify: `src/session.rs` (comments only: the `state` field doc at :177-179, `update_displays` doc at :655-662, `adopt_loaded` doc at :1024-1029)
- Modify: `src/application/runtime.rs` (comment only: `MAX_ENRICHMENT_PER_PUMP` doc at :80-84)
- Modify: `docs/architecture.md` (§9.1, one sentence)
- Modify: `CHANGELOG.md` (`[Unreleased]`)
- Create: `tests/m12_snapshot_size.rs` (hand-run measurements, `#[ignore]`d, mirroring `tests/m8_snapshot_size.rs`'s self-contained fixture style)
- Create: `docs/m12-acceptance.md`

**Interfaces:**
- Consumes: Tasks 1–2 (the sharing is in place — the measurements measure it).
- Produces: the acceptance record.

- [ ] **Step 1: Rewrite the clone-cost rationales** (exact texts; nothing else in these files changes):

  1. `session.rs:177-179` (`state` field):
     > The authoritative state. `Action::Submit` hands the writer a shared-subtree snapshot: unchanged subtrees are `Arc`-shared, and every later mutation copies on write against the writer's claim, so what the writer observes never changes underneath it. The session never shares a reference into the live state.
  2. `session.rs` `update_displays` doc, last sentence replaced:
     > …with **one** submit for the whole of it rather than one per update: the metadata workers' results arrive in bursts of hundreds after a folder add, and each submit pokes the writer's keep-latest slot and can wake the writer thread. Snapshots are cheap to clone now, but one submit per burst keeps that churn bounded.
  3. `session.rs` `adopt_loaded` doc, the clause `so it is never a snapshot cloned before them (D19)` becomes `so the submission observes every one of them (D19)`.
  4. `runtime.rs:80-84`, the sentence `The batch shares a single submit, so what this bounds is the per-result scan of every queued entry, not the clone.` becomes `The batch shares a single submit, and what this bounds is the per-result scan of every queued entry.` The `// ponytail:` note under it keeps its text.

- [ ] **Step 2: Write the m12 measurement suite** `tests/m12_snapshot_size.rs` — module comment copied in spirit from m8's ("they measure, they do not assert; run by hand and copy into docs/m12-acceptance.md"), command line `cargo test --release --test m12_snapshot_size -- --ignored --nocapture`. Both tests `#[ignore]`. Fixture: duplicate m8's `kind_of`/`representative` shape (self-contained by design; do not import from the m8 file) and build the same state through `Session` (`create_playlist` returns `Result<(PlaylistId, Action), _>`; `enqueue`; `state.record(...)` for 512 checkpoints stepping `medias` by 8, exactly as `tests/m8_snapshot_size.rs:205-236` does).

  - `a_full_snapshot_clone_costs_this_much`: build the m8-shaped state (8 playlists, 4,096 mixed entries, 512 checkpoints); print `entries`, `checkpoints`, `bytes on disk` (serialize with `serde_json::to_vec_pretty` and `.len()`), and time `session.state().clone()` — the headline number (m8 baseline: **2.19–2.29 ms**).
  - `a_checkpoint_touch_costs_this_much`: from the same state, print four labelled durations, one `record`-call each:
    - `touch, snapshot retained (steady state)`: `let retained = state.clone();` then `state.record(existing_media, false)`;
    - `touch again, still retained`: another `record` of a different existing media;
    - `insert with eviction, retained`: `record` of a fresh identity — the capacity worst case (512 held, evict + insert);
    - `touch, nothing retained (in place)`: `drop(retained);` then `record` of an existing media — expected ≈ sub-µs, pinning that an unshared capture does not copy.

- [ ] **Step 3: Verify the suite compiles and the gates hold** (the `#[ignore]`d tests must still compile under `--all-targets`)

Run: `cargo clippy --locked --all-targets --all-features -- -D warnings` → clean.
Run: `cargo test --locked --test m12_snapshot_size` → "0 tests run" (ignored by default).

- [ ] **Step 4: Update the docs**

  1. `docs/architecture.md` §9.1, end of the snapshot-mechanics paragraph ("Session captures a checkpoint every 5 s …"), add:
     > Snapshots share unchanged subtrees: the checkpoint map and each playlist are `Arc`-backed and mutated copy-on-write, so a submit costs refcount updates and a mutation clones only the subtree it touches — never while no snapshot is retained.
  2. `CHANGELOG.md` `[Unreleased]`, new `### Changed` bullet (Keep a Changelog, hard-wrapped):
     > - Submitting state no longer deep-clones it: the checkpoint map and each playlist are `Arc`-shared copy-on-write, so a snapshot costs refcount updates instead of a multi-millisecond copy on the app thread, while `state.json` stays byte-for-byte what it was. The one-submit-per-burst metadata batching stays, now justified by writer-thread churn rather than clone cost.

- [ ] **Step 5: Run the measurements by hand**

Run: `cargo test --release --test m12_snapshot_size -- --ignored --nocapture` (three runs, output kept verbatim).
Run: `cargo test --release --test m8_snapshot_size -- --ignored --nocapture` (three runs — the after-numbers for the re-run; expected: clone sub-µs, worst pump ≈ m8's 1.2–1.6 ms, per the spec's Residual costs).

- [ ] **Step 6: Write the acceptance record** `docs/m12-acceptance.md`, following the m11 format: scope line (spec + issue #69), branch and base commit, an automated-evidence table (sharing/at-capacity unit tests → `src/persistence/model.rs`, `src/playlist/queue.rs`, `src/playlist/set.rs`; boundary → `tests/m12_sharing.rs`; unmodified pins → `tests/persistence_model.rs`, `tests/m8_roundtrip.rs`, `tests/m5_state_*`, `tests/m8_state_playlists.rs`, `tests/session_policy.rs`, `tests/m9_layering.rs`), the measurement section pasting the verbatim runs against the m8 baselines (clone 2.19–2.29 ms → here; worst pump 1.17–1.58 ms ≈ unchanged, explained per the spec's Residual costs), and a small manual check (pending, user): play a local track ~10 s, pause, quit, relaunch — position resumes; enqueue/move/rename still behave.

- [ ] **Step 7: Full gates, then commit**

Run: all four gate commands → green.

```bash
git add src/session.rs src/application/runtime.rs docs/architecture.md CHANGELOG.md tests/m12_snapshot_size.rs docs/m12-acceptance.md
git commit -m "docs(m12): sharing measured, clone-cost rationales reworded"
```
