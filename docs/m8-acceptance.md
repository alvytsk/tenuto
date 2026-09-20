# M8 acceptance

`docs/superpowers/specs/2026-09-20-tenuto-playlists-design.md` §12 lists the validation. Test files are `tests/m8_*.rs`; new in-crate test modules ride in the `src/` files whose module doc comments name what they pin.

## What M8 delivered, by spec section

| ID | Spec §12 bullet | Where it's pinned |
| --- | --- | --- |
| P1 | **Playlist and queue.** `splitmix64` vector; `first` pinning; order stable under add/remove/move; boundaries return `None`; ID exhaustion and capacity refusal change nothing; the cap holds across playlists | `tests/m8_playlist.rs` (11 tests): `splitmix64_matches_the_reference_vector`, `seed_42_orders_ids_one_to_five_as_5_1_4_3_2`, `first_is_pinned_ahead_of_the_hashed_order_while_it_is_a_member`, `a_first_that_is_not_a_member_is_ignored`, `removing_or_moving_an_entry_leaves_the_others_relative_order_alone`, `an_unknown_anchor_has_no_neighbor` (boundary `None`), `an_allocator_reserves_a_contiguous_range_or_nothing`, `observing_an_id_never_lowers_the_counter_and_max_exhausts_it`, `two_queues_sharing_an_allocator_never_share_an_id`. The cross-playlist cap itself is pinned in `src/persistence/model.rs`'s in-crate tests (`the_cap_is_global_and_a_refused_batch_changes_nothing`) |
| P2 | **Codec.** Migration 3 → 4; each §6 recovery rule, both truncations and `current_media` re-pointing; every case keeps its checkpoints; exhaustion together with duplicates for both entry and playlist IDs; a repair never mints a later-occurring ID | `tests/m8_state_playlists.rs` (16 tests): schema 4 on disk, migration from schema 3, and the ordered recovery rules (per its module doc comment) |
| P3 | **Session.** Release follows adoption not the cursor (four unowned-cursor cases); scoped invalidation; `playing` changes only at adoption; a failed cross-playlist load changes neither `playing` nor any cursor; `update_display` reaches every playlist | `tests/m8_session_playlists.rs` (15 tests): `enqueue_lands_in_the_named_playlist_and_a_deleted_one_refuses`, `moving_and_loading_find_an_entry_in_any_playlist`, `a_loaded_for_an_entry_outside_the_playing_playlist_is_accepted_and_adopted`, `a_display_update_reaches_every_occurrence_in_every_playlist`, `playing_changes_at_adoption_and_the_old_playlist_keeps_its_cursor`, `a_failed_cross_playlist_load_changes_neither_playing_nor_any_cursor`, `removing_a_cursor_nothing_adopted_only_clears_it`, `with_a_playing_and_b_loading_removing_as_entry_cannot_touch_bs_load`, `clearing_a_playlist_invalidates_only_its_own_pending_loads`, `deleting_the_owning_playlist_releases_and_moves_playing`, `deleting_another_playlist_leaves_playback_alone`, `removing_a_restored_cursor_with_nothing_adopted_only_clears_it`, `removing_the_playing_cursor_while_another_playlist_loads_leaves_that_load_valid`, plus shuffle/advance ordering tests |
| P4 | **Runtime.** Clearing an inactive playlist cancels no pending seek and loses no other playlist's enrichment request | `tests/m8_runtime.rs` (13 tests, per its module doc comment: the runtime's viewed playlist, the playlist commands, and side effects scoped to the playlist they touch) |
| P5 | **Transport.** Phase × command table with viewed ≠ playing, an empty playing playlist, the `Loading` exception; retry precedence with A empty and a failed request in B while viewing C; Space/`p` never load the viewed selection; shuffled manual navigation agrees with automatic advance | `tests/m8_transport.rs` (7 tests): `enter_plays_the_viewed_selection_in_every_phase_even_over_an_empty_playing_playlist`, `space_and_play_never_load_the_viewed_selection`, `the_cursor_outranks_the_first_entry_and_shuffle_decides_what_first_means`, `a_valid_retry_outranks_the_empty_check_whatever_is_viewed`, `next_and_previous_step_from_the_cursor_in_playback_order_and_stop_at_the_ends`, `during_loading_navigation_anchors_on_the_retry_in_its_own_playlist`, `engine_phases_keep_their_engine_commands_over_an_emptied_playlist` |
| P6 | **Browse.** Walk order (root-level and nested audio, `a/1.mp3` before `z.mp3`); truncation; directory-symlink cycle; unreadable directories reported; scan limit; overlapping roots and file-symlink aliases deduplicated; completion applied after the browser closed; completion dropped with a notice after the playlist was deleted; `a` additive where Enter toggles | `tests/m8_tree_walk.rs` (9 tests: walk order, truncation, cycle/dedup, unreadable directories, scan limit) and `tests/m8_browser.rs` (6 tests, per its module doc comment: captured destination, markable directories, the Files-tab `a` key). Completion outliving the browser is pinned in-crate, `src/tui/mod.rs`: `a_tree_lands_once_the_browser_that_asked_for_it_has_closed`, `a_tree_lands_in_its_captured_destination_though_the_browser_moved_elsewhere` |
| P7 | **Resume.** Local file and URL start at zero with a checkpoint present; a podcast still resumes; Stop → Play continues mid-track | `tests/m8_resume.rs` (3 tests) |
| P8 | **View and layout.** Row strings; tab-strip widths in columns, scrolling, one over-long name clipped; hostile names sanitized | `tests/m8_tui.rs` (11 tests, per its module doc comment: playlist keys, overlays that act on their captured target, and the drawn tab strip — border budget, Minimal tier, hostile names). Row strings and the strip's own arithmetic are pinned in-crate: `src/application/view.rs` (`a_tagged_track_reads_artist_dash_title_with_the_album_below`, `without_an_artist_or_without_a_title_the_row_is_as_before`, `control_characters_in_either_tag_never_reach_the_row`), `src/application/runtime.rs` (`now_playing_title_stays_plain_even_though_the_row_combines_artist_and_title`), `src/tui/tabs.rs` (6 tests: mark placement, window scrolling, column-width measurement, ellipsis clipping, zero-width/unknown-viewed-id edge cases, compact naming) |

## Automated evidence

Integration test files, `tests/m8_*.rs` (excluding the ignored measurement):

| File | Tests | Pins |
| --- | --- | --- |
| `m8_browser.rs` | 6 | The browser's captured destination, markable directories, the Files-tab `a` key |
| `m8_playlist.rs` | 11 | `splitmix64`, `first` pinning, order stability under add/remove/move, ID allocator behavior |
| `m8_resume.rs` | 3 | Start-from-zero for local files/URLs, podcast resume, Stop → Play continuation |
| `m8_runtime.rs` | 13 | The runtime's viewed playlist, playlist commands, side effects scoped to the playlist they touch |
| `m8_session_playlists.rs` | 15 | Ownership by adoption, scoped invalidation, cross-playlist load and cursor rules |
| `m8_snapshot_size.rs` | 1 (`#[ignore]`d) | The §12 snapshot-cost measurement below — prints, does not assert a threshold |
| `m8_state_playlists.rs` | 16 | Schema 4 on disk, migration from schema 3, the ordered recovery rules |
| `m8_transport.rs` | 7 | The phase × command table, retry precedence, shuffled navigation |
| `m8_tree_walk.rs` | 9 | Walk order, truncation, symlink-cycle and dedup, unreadable directories, scan limit |
| `m8_tui.rs` | 11 | Playlist keys, overlays bound to their captured target, the drawn tab strip |

92 tests across these ten files (91 executing, plus the one `#[ignore]`d measurement).

In-crate `#[cfg(test)]` modules that gained M8 tests (`git diff --stat aac0fa26..HEAD` against `src/`, then inspected by hand):

- `src/persistence/model.rs` — 9 new tests: a fresh state's default playlist, cross-playlist entry-ID uniqueness and owner lookup, the cap being global, enqueue-into-deleted-playlist refusal, playlist-ID non-reuse, name limits, and the three delete-the-playing-playlist cases (moves `playing`, clears `current_media` when the next has no cursor, leaves both alone when another playlist is deleted).
- `src/application/view.rs` — 3 new tests: the `Artist – Title` row, its fallback when either tag is missing, and control-character sanitization.
- `src/application/runtime.rs` — 1 new test: `now_playing_title` stays plain even though the row itself combines artist and title.
- `src/tui/mod.rs` — 2 new tests: a folder-walk result landing after the browser that asked for it closed, and landing in its captured destination even though the browser moved elsewhere.
- `src/tui/tabs.rs` — new file, 6 tests: playing/shuffled marks, window scrolling, column-width measurement (not bytes or chars), one over-long name's ellipsis, zero-width and unknown-viewed-id edge cases, and the compact "name n/m" form.

## Gates

`cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings` — both exit 0, no warnings.

`cargo test --locked --no-fail-fast` — 106 binaries (103 integration test binaries, the crate's `src/lib.rs` and `src/main.rs` unit-test binaries, and its doc-tests): 1312 passed, 0 failed, 2 ignored (`device_smoke`'s hardware-only test, and `m8_snapshot_size`'s measurement).

## Known issues observed during implementation

One full-suite run saw `tests/http_cancellation.rs::every_wait_wakes_and_stale_responses_cannot_repopulate` fail once, on a timing assertion. It passed on every re-run, including with this branch's test changes stashed. `git diff --stat aac0fa26..HEAD -- src/http tests/http_cancellation.rs tests/m7_cancellation.rs` is empty — this branch touches none of those files. Recorded as observed; the diagnosis is left to the reader.

## Snapshot cost at the 4,096-entry cap

Measured by an automated agent during implementation on 2026-09-20, on:

- `uname -srm`: `Linux 7.0.0-31-generic x86_64`
- CPU: AMD Ryzen 7 7800X3D 8-Core Processor
- Build profile: `--release`
- Filesystems (`df -T`): `/tmp` is `tmpfs`; the crate's own `target/` directory (`/dev/nvme1n1p4`, mounted at `/`) is `ext4` — the same disk `$XDG_STATE_HOME/state.json` lives on, since `$XDG_STATE_HOME` is unset on this machine and falls back under `$HOME`, also on `/`

The fixture is a mix of all three `MediaId` kinds, as spec §12 asks for ("representative paths, URLs and tags"): local files with realistic album/artist/year tags, plain URLs with a path and a query string, and podcast episodes built the way the runtime builds them (`MediaId::PodcastEpisode { feed, episode }`, `QueueSource::Podcast { fallback }`). Roughly 80% local, 10% URL, 10% podcast — **entries: local=3,277 url=410 podcast=409** (out of 4,096). The 512-entry checkpoint map draws from all three kinds too — **checkpoints: local=306 url=103 podcast=103** — with a varied position, timestamp and completion flag per entry rather than one constant for all of them.

The atomic write is measured twice: once against `/tmp` (kept as a labelled comparison — `tmpfs` never touches a physical block device, so its `fsync` is close to free) and once against a temp directory created under this crate's own `target/` (`ext4`, real disk) — that second number is the headline.

Command: `cargo test --release --test m8_snapshot_size -- --ignored --nocapture`, run three times in a row, output pasted verbatim (not averaged or rounded):

```
Run 1:
entries                        4096
entries by kind                local=3277 url=410 podcast=409
checkpoints                    512
checkpoints by kind            local=306 url=103 podcast=103
bytes on disk                  3226621
clone (app thread)             2.189623ms
serialize                      8.042946ms
atomic write, tmpfs (/tmp)     7.442192ms  fstype=tmpfs  (comparison only)
atomic write, on-disk (target) 11.540636ms  fstype=ext4  path=/home/alvy/projects/tenuto/target/.tmplxeX9i

Run 2:
entries                        4096
entries by kind                local=3277 url=410 podcast=409
checkpoints                    512
checkpoints by kind            local=306 url=103 podcast=103
bytes on disk                  3226621
clone (app thread)             2.288873ms
serialize                      8.001535ms
atomic write, tmpfs (/tmp)     7.463563ms  fstype=tmpfs  (comparison only)
atomic write, on-disk (target) 11.729976ms  fstype=ext4  path=/home/alvy/projects/tenuto/target/.tmpVKzQch

Run 3:
entries                        4096
entries by kind                local=3277 url=410 podcast=409
checkpoints                    512
checkpoints by kind            local=306 url=103 podcast=103
bytes on disk                  3226621
clone (app thread)             2.271233ms
serialize                      8.098536ms
atomic write, tmpfs (/tmp)     7.265611ms  fstype=tmpfs  (comparison only)
atomic write, on-disk (target) 11.560266ms  fstype=ext4  path=/home/alvy/projects/tenuto/target/.tmpsObzuH
```

`checkpoints` reads 512, not 1,024 (one in four of 4,096 tracks): `persistence::model::MAX_ENTRIES`, the checkpoint cap, is currently 512, and recording more than that evicts. The test steps by 8 rather than 4 to land exactly on what the cap allows, and says so in its own comment.

The "about 1 MB" figure used while designing was an estimate. The mixed-kind snapshot lands at 3,226,621 bytes — 3.077 MiB, about 3.23× that estimate. (This is smaller than round 1's all-local 3,400,329 bytes: the URL and podcast entries here carry shorter paths and fewer tags — no album for a plain URL, for instance — than every entry being the same long local path.)

Reading of these numbers: cloning the state on the application thread costs about 2.2–2.3 ms, serializing it to pretty JSON about 8.0–8.1 ms, and the atomic write on the real filesystem (`ext4`, under `target/`; serialize + write the temp file + `fsync` + rename + best-effort parent `fsync`, measured inside `StateStore::write` itself) costs 11.5–11.7 ms — noticeably slower than the `tmpfs` comparison's 7.3–7.5 ms, which is the point of measuring it separately. Summed on the real filesystem, the whole path — clone, serialize, atomic write — costs about 21.8–22.0 ms per run, roughly 227–230× faster than the 5-second `CAPTURE_INTERVAL` that gates how often a submission is even attempted. That margin is comfortable even after moving off `tmpfs` onto the disk the real state file lives on: `ext4` on this NVMe drive would need to be two orders of magnitude slower before it approached the 5-second cadence. No follow-up design (§11, §12) is warranted at the 4,096-entry cap; the numbers, now measured on the real filesystem and across a representative mix of media kinds, do not call the cap into question.

## Manual pass in Ghostty — PENDING

Not yet performed — this requires a human at a real terminal; it cannot be run by an agent. To do, with a real music folder:

- [ ] add a folder with `a`; rows fill in as `Artist – Title`
- [ ] tracks start from 0:00; `tenuto play <local file>` starts from 0:00 even with a saved checkpoint; a podcast episode still resumes
- [ ] `z` shuffles; next/previous and end-of-track follow one order; turning it off continues in list order
- [ ] `n`, `r`, `D`, `Tab`; delete the playing playlist while it plays
- [ ] `D` on the only playlist is refused and does not disturb a seek in progress
- [ ] Enter in another playlist while a track plays; the old tab keeps its cursor
- [ ] quit and restart: playlists, cursors and shuffle survive
- [ ] the strip at 80×28, at <50 columns, and with a very long name
- [ ] the strip's alignment against the right-aligned track count at odd widths and large track counts; the highlighted (cream/bold) vs muted contrast of tabs on the border row; whether the Minimal tier's `name n/m` first row reads as a header rather than a track
- [ ] the help overlay at a terminal 22–23 rows tall: it grew to 24 rows and may lose its last lines, including `q / Ctrl-C`
- [ ] closing the browser right after `a` on a large folder: the add still lands, and the status line reports it
