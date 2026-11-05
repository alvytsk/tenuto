# M12 Snapshot sharing — acceptance

Scope: structural sharing for `PersistedState` snapshots
(`docs/superpowers/specs/2026-11-05-tenuto-m12-snapshot-sharing-design.md`,
issue #69). The checkpoint map and each playlist are `Arc`-shared and
mutated copy-on-write: a submit costs refcount updates instead of a
multi-millisecond copy, `state.json` stays byte-for-byte what it was, and a
held snapshot never changes underneath the writer.

Branch `m12-snapshot-sharing`, base commit `17507bb` — the sharing is in
`8950874` (the checkpoint map) and `17507bb` (each playlist); the runs
below are taken on that code.

## Automated evidence

| Area | Where | Tests |
|---|---|---|
| Checkpoint-map sharing and the at-capacity record paths | `src/persistence/model.rs` | `a_clone_shares_the_checkpoint_map`, `recording_disturbs_neither_the_retained_snapshot_nor_its_sharing_base`, `record_at_the_cap_leaves_a_retained_snapshot_whole`, `record_estimated_at_the_cap_leaves_a_retained_snapshot_whole`, `evict_one_reports_no_victim_when_the_only_entry_is_current` |
| Playlist sharing; no-op mutations clone nothing | `src/playlist/set.rs` | `a_clone_shares_every_playlist`, `a_move_at_an_edge_clones_nothing`, `a_real_move_clones_the_owner_and_shares_the_rest`, `a_matching_metadata_resend_clones_nothing`, `a_matching_fallback_url_clones_nothing` |
| No-op detection the no-op pins rest on | `src/playlist/queue.rs` | `move_would_change_agrees_with_move_entry`, `would_change_agrees_with_apply_to`, `exhaustion_rejects_the_whole_batch_without_reusing_an_id` |
| Boundary: a submitted snapshot never changes underneath the writer | `tests/m12_sharing.rs` | `a_submitted_snapshot_does_not_change_underneath_the_writer` |
| Unmodified pins, passed unchanged | `tests/persistence_model.rs`, `tests/m8_roundtrip.rs`, `tests/m5_state_recovery.rs`, `tests/m5_state_schema.rs`, `tests/m8_state_playlists.rs`, `tests/session_policy.rs`, `tests/m9_layering.rs` | whole suites, no file modified |

## Measurements (hand-run, three runs each)

`cargo test --release --test m12_snapshot_size -- --ignored --nocapture` —
the new suite; its fixture mirrors m8's (8 playlists, 4,096 mixed entries,
512 checkpoints):

```
Run 1:
touch, snapshot retained (steady state)  56.23µs
touch again, still retained              310ns
insert with eviction, retained           4.5µs
touch, nothing retained (in place)       440ns
entries                        4096
checkpoints                    512
bytes on disk                  3226621
clone (app thread)             90ns

Run 2:
touch, snapshot retained (steady state)  82.031µs
touch again, still retained              440ns
insert with eviction, retained           5.45µs
touch, nothing retained (in place)       670ns
entries                        4096
checkpoints                    512
bytes on disk                  3226621
clone (app thread)             90ns

Run 3:
touch, snapshot retained (steady state)  59.44µs
touch again, still retained              340ns
insert with eviction, retained           4.33µs
touch, nothing retained (in place)       450ns
entries                        4096
checkpoints                    512
bytes on disk                  3226621
clone (app thread)             90ns
```

Reading of these numbers:

- `clone (app thread)` — **90 ns in every run** against m8's 2.19–2.29 ms
  baseline. The headline win: a submit is refcount updates, four orders of
  magnitude under the old floor.
- `touch, snapshot retained (steady state)` — 56–82 µs: the copy-on-write
  split the first mutation after a snapshot pays. This is the residual cost
  the spec's Residual costs section states for the checkpoint map ("a
  checkpoint touch clones the whole checkpoints map"), and it lands once
  per retained snapshot, not per touch: the next line shows it.
- `touch again, still retained` — 310–440 ns: once the split has happened,
  later touches mutate in place even while the snapshot lives.
- `insert with eviction, retained` — 4.3–5.5 µs: the capacity worst case,
  an unshared map paying evict + insert.
- `touch, nothing retained (in place)` — 440–670 ns: with no snapshot
  retained, a capture does not copy. That is the steady state the player
  actually lives in — the 5-second capture costs sub-µs unless a submit's
  snapshot is still outstanding, and even then it costs tens of µs.

`cargo test --release --test m8_snapshot_size -- --ignored --nocapture` —
the existing suite, unchanged, re-run for the after-numbers:

```
Run 1:
entries                        4096
entries by kind                local=3277 url=410 podcast=409
checkpoints                    512
checkpoints by kind            local=306 url=103 podcast=103
bytes on disk                  3226621
clone (app thread)             120ns
serialize                      7.662668ms
atomic write, tmpfs (/tmp)     6.594652ms  fstype=tmpfs  (comparison only)
atomic write, on-disk (target) 11.11825ms  fstype=ext4  path=/home/alvy/.local/share/opencode/worktree/6d5efe/m12-snapshot-sharing/target/.tmpReAuw0
entries                        4096
results applied per pump (cap) 64
add_tree (app thread)          39.445309ms
rows titled                    4096
pumps to drain                 64
worst single pump              1.934722ms
all pumps, summed              104.173809ms
drain, wall clock              161.462367ms  (includes this test's own row checks)

Run 2:
entries                        4096
entries by kind                local=3277 url=410 podcast=409
checkpoints                    512
checkpoints by kind            local=306 url=103 podcast=103
bytes on disk                  3226621
clone (app thread)             100ns
serialize                      7.799249ms
atomic write, tmpfs (/tmp)     7.075835ms  fstype=tmpfs  (comparison only)
atomic write, on-disk (target) 11.075169ms  fstype=ext4  path=/home/alvy/.local/share/opencode/worktree/6d5efe/m12-snapshot-sharing/target/.tmpPaFl2B
entries                        4096
results applied per pump (cap) 64
add_tree (app thread)          39.499278ms
rows titled                    4096
pumps to drain                 64
worst single pump              1.913592ms
all pumps, summed              104.257867ms
drain, wall clock              161.517017ms  (includes this test's own row checks)

Run 3:
entries                        4096
entries by kind                local=3277 url=410 podcast=409
checkpoints                    512
checkpoints by kind            local=306 url=103 podcast=103
bytes on disk                  3226621
clone (app thread)             110ns
serialize                      7.629868ms
atomic write, tmpfs (/tmp)     6.977604ms  fstype=tmpfs  (comparison only)
atomic write, on-disk (target) 10.922149ms  fstype=ext4  path=/home/alvy/.local/share/opencode/worktree/6d5efe/m12-snapshot-sharing/target/.tmpw6DGVV
entries                        4096
results applied per pump (cap) 64
add_tree (app thread)          40.524736ms
rows titled                    4096
pumps to drain                 64
worst single pump              1.954052ms
all pumps, summed              103.759379ms
drain, wall clock              160.630642ms  (includes this test's own row checks)
```

Against the m8 record's baselines:

- `clone (app thread)`: 2.189623 / 2.288873 / 2.271233 ms →
  120 / 100 / 110 ns. The same win the m12 suite shows, now measured by the
  m8 fixture itself.
- `worst single pump`: the record has 1.227277 / 1.166267 / 1.583689 ms;
  this re-run has 1.91–1.95 ms — **≈ unchanged, as the spec's Residual
  costs predicts**: in this fixture one playlist holds all 4,096 entries
  and every pump mutates it while the writer holds the previous snapshot,
  so one deep playlist clone per pump remains either way. The higher
  reading is not a regression, and two facts pin that. First, the drain
  shape moved: 64 pumps here against the record's 249–254, so each pump
  now carries the full 64-result batch (four times the per-result scan the
  record's pumps did). Second, the same fixture run on the pre-m12 base
  commit `8708081` — a one-off probe, not one of the three runs — prints
  the same shape and level: 64 pumps, worst single pump 2.070452 ms,
  summed 101.869631 ms. The shape and the level both predate m12; the
  sharing's own effect on this fixture is within run-to-run noise, and the
  total drain cost fell (all pumps summed 101.9–104.3 ms here against the
  record's 172.9–178.3 ms).
- `serialize` and the atomic writes: 7.63–7.80 / 6.59–7.08 / 10.9–11.1 ms
  against the record's 8.0–8.1 / 7.3–7.5 / 11.5–11.7 ms — the machine is
  marginally faster today. `bytes on disk` is identical at 3,226,621, and
  the file's byte-for-byte format is pinned by `tests/persistence_model.rs`.

## Manual check (pending, user)

- [ ] Play a local track for ~10 s, pause, quit, relaunch — the position
  resumes where it was paused. (Capture and resume semantics are pinned by
  the `tests/m5_state_*` suites; the feel on a real device is what is
  left.)
- [ ] Enqueue, move and rename still behave. (The operations are pinned by
  `tests/m8_state_playlists.rs` and the sharing tests above; the keys and
  the screen are what is left.)
