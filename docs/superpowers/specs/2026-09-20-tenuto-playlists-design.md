# Tenuto M8: playlists, shuffle and music-player behavior

Status: approved in conversation on 2026-09-20; ready for an implementation plan. The behavior below is the proposed contract, not an assertion that it already exists.

Branch: `feat/m8-playlists`, from `main` at `aac0fa2`.

## 1. Product decision

Tenuto grew up around podcasts: one queue, and every medium resumes where it stopped. Pointed at a folder of music it behaves wrongly — tracks resume mid-song, there is no shuffle, a folder cannot be added, and rows show a bare title.

M8 makes it act like an ordinary music player without giving up the podcast behavior:

1. The single queue becomes several named **playlists**. The listener always plays *a* playlist; there is no separate queue on top.
2. Each playlist has a **shuffle** toggle. The list keeps its order on screen; traversal follows a hidden order.
3. Local files and plain URLs **start from the beginning** on a fresh load. Podcast episodes keep resuming.
4. The Files tab can **add a folder** recursively.
5. Rows read **Artist – Title**.

## 2. Existing foundations, and what they do not give us

- `queue::Queue` is ordered occurrences with stable `QueueEntryId`s, an `active` entry, `enqueue`, `move_entry`, `remove`, `clear`, `neighbor`. It lives in `PersistedState` and changes only through `Session`. It allocates its own IDs and caps itself at `MAX_QUEUE_ENTRIES = 256`.
- `persistence::queue_codec::recover_queue` decodes the queue under ordered recovery rules that can never cost a checkpoint. It validates `active` against the single `current_media`.
- `Session` ties `queue.active` to playback ownership: `remove_entry` (`src/session.rs:454`) and `clear_queue` call `release_active` when the active entry goes, and `clear_queue` invalidates *every* queue-targeted pending load. Real ownership is `adopted: Option<AdoptedLoad>` under the token checks of `owns_adopted`.
- Traversal has two independent callers of `Queue::neighbor`: manual next/previous (`src/application/transport.rs:116`) and end-of-track advance (`src/session.rs:902`).
- `resume_intent_for(entry)` (`src/session.rs:1457`) decides resume from a checkpoint alone. Its callers are `Session::resume_intent(&MediaId)` (used at `src/application/runtime.rs:792`) and the `play` path at `src/app.rs:637`. Pause, and Stop → Play, are engine commands (`transport.rs:206-207`) and never consult it.
- `DisplayMetadata` already carries title, artist and album, filled by the `application::enrich` workers; `Session::update_display` applies a result to the one queue. `application/view.rs` renders the title, and artist · album as a subtitle.
- `application::browse::list_directory` reads one level, follows symlinks when classifying, and sorts directories first then by case-insensitive name. `BrowserState` marks only enqueueable rows with Space; Enter opens a directory, and on an audio row enqueues it — or removes it when it is already queued.
- `state.json` is one pretty-printed JSON snapshot, schema 3, cloned on the application thread and written atomically by the writer thread, every `CAPTURE_INTERVAL` (5 s) during playback.

What they do not give us: more than one list; a destination for an edit; an ID space shared across lists; ownership rules that tell a remembered cursor from the playing entry; a traversal policy; a media-aware resume rule; a recursive walk; a cap that suits a music folder.

## 3. Rules

- **P1.** At least one playlist always exists. `Default` is only the name migration gives the first one; any playlist may be renamed, and deleted while another exists.
- **P2.** `QueueEntryId`s are globally unique and identify entries. They do not identify destinations: every operation that adds to or clears a list carries an explicit `PlaylistId`, captured when the operation starts.
- **P3.** A playlist's `active` entry is its *remembered cursor*. Playback ownership remains `Session::adopted` under the existing token checks. Nothing about an inactive playlist may release or stop playback.
- **P4.** `playing` changes at adoption of a `Loaded`, never at request.
- **P5.** Viewing and editing follow the viewed playlist. Playback, next/previous and advance follow the playing playlist, with one exception: during `Loading`, navigation follows the owner of `last_requested`.
- **P6.** Manual navigation and automatic advance use one traversal policy.
- **P7.** The entry cap is global and enforced where IDs are allocated, so no path — synchronous or asynchronous — can exceed it.
- **P8.** No recovery or migration step may lose a checkpoint.
- The position contract in `architecture.md` §1 is unchanged. Start-from-zero (§7) is a fresh-load policy only.

## 4. Model

```rust
pub struct PlaylistId(u64);

pub struct Shuffle { seed: u64, first: Option<QueueEntryId> }

pub struct Playlist {
    id: PlaylistId,
    name: String,
    shuffle: Option<Shuffle>,
    queue: Queue,
}

// PersistedState, schema 4
playlists: Vec<Playlist>,   // never empty (P1)
playing: PlaylistId,
next_entry_id: Option<u64>,     // None = exhausted
next_playlist_id: Option<u64>,
```

`Queue` keeps its type and its tests but stops allocating IDs: `enqueue` receives them. `PersistedState` is the sole allocator.

**Limits.** `MAX_ENTRIES = 4096` across all playlists, replacing `MAX_QUEUE_ENTRIES`; there is no separate per-playlist number. `MAX_PLAYLISTS = 32`. A name is 1–40 characters after trimming; duplicate names are allowed, since `PlaylistId` is the identity.

The code names this cap `MAX_PLAYLIST_ENTRIES`, not `MAX_ENTRIES`: `persistence::model` already has a `MAX_ENTRIES` for checkpoints.

**Allocation.** Both counters are monotonic, advance by `checked_add`, and are never reused — a late asynchronous result for a deleted playlist can therefore never land in a newer one. A batch enqueue reserves all its IDs and checks global capacity first; on exhaustion (`QueueError::IdExhausted`) or lack of capacity (`QueueError::Capacity`) it changes nothing. Each counter is an `Option<u64>` — the next ID to hand out, or `None` for an exhausted namespace — serialized as a number or `null`; a `u64` alone cannot represent "past `u64::MAX`". Handing out `u64::MAX` leaves `None`. On load, each counter is the larger of the stored value and `highest seen + 1`, where `highest seen` covers every well-formed ID in the file, including ones recovery will later repair; if `highest seen` is `u64::MAX`, or the stored value is `null`, the counter is `None`. Such a file loads, and every later allocation is refused.

**Playlist operations** (all through `Session`): `create(name) -> PlaylistId`, `rename(id, name)`, `delete(id)`, `set_shuffle(id, on)`. Creating past `MAX_PLAYLISTS` and deleting the last playlist are errors that change nothing.

## 5. Ownership

`PersistedState::queue()` survives as a convenience for the *playing* playlist, used by transport and advance only. Everything else names its target:

| Path | Target |
|---|---|
| `enqueue`, `clear` | explicit `PlaylistId` |
| `move_entry`, `remove_entry` | `QueueEntryId`, resolved by owner lookup |
| `LoadTarget::Queue(id)` registration and resolution | owner lookup across all playlists, never `queue()` |
| `queue_rows` | the viewed `PlaylistId` |
| `update_display` | every occurrence of the media in every playlist |

**The ownership check.** Today `release_active` clears `adopted` unconditionally — only the checkpoint capture inside it is gated — and its callers decide from the cursor (`queue.active() == id`). With several playlists a cursor is not ownership (P3), so every release decides from adoption instead:

> An entry is *owned* when `adopted.target == LoadTarget::Queue(id)`. A playlist is *owning* when it owns the adopted entry.

Release (`release_active`), `Removal::stop_playback`, and the runtime's pending-seek cancel (`router.cancel()`, today keyed on the cursor in `runtime::remove` and unconditional in `runtime::clear_queue`) happen only for an owned entry or an owning playlist. A cursor that is not owned — a restored cursor with nothing adopted, the old playlist's cursor while another playlist's load is pending, any inactive playlist's cursor — only clears. `apply_removal` keeps its existing guard: no `interrupt_stop` while a load is pending.

**Removing an entry.** Pending loads targeting it are invalidated first, as today. Then the ownership check above; then the entry goes, and if it was its playlist's cursor the cursor clears. So with A's track still playing while B loads, removing A's entry releases A's adoption and cannot touch B's pending load; removing any other entry of A touches neither.

**Clearing or deleting playlist P.** Invalidate exactly the pending loads whose `LoadTarget::Queue(id)` has `owner(id) == P`, evaluated *before* the entries go. Loads for other playlists are untouched. Release, stop and seek-cancel only if P is owning. Deleting the playing playlist then moves `playing` to the adjacent playlist (the next one, else the previous).

**Metadata workers.** `MetadataWorkers` can only `cancel_all`. Clearing or deleting a playlist calls it, then re-requests enrichment for every remaining entry in every playlist that still lacks tags — `request_enrichment` already filters to those — so another playlist's pending probes are never lost. Removing a single entry cancels nothing, as today.

**When `playing` changes without an adoption** — deletion here, or recovery in §6 — the new playlist's cursor is never validated against the old `current_media`. `current_media` is re-pointed to the media of the new playlist's cursor, or cleared when it has none. Checkpoints are untouched (P8).

**Adoption.** When a `Loaded` for `LoadTarget::Queue(id)` is adopted, `playing = owner(id)` and that playlist's `active = id`. The previously playing playlist keeps its cursor.

**A failed load** does not change `playing` or any remembered cursor. Load bookkeeping and the outgoing media's checkpoint may change as they do today. The existing distinction is preserved: a failure before engine admission leaves current playback and its controls in place, and `p` keeps controlling it; the `last_requested` retry applies only when no playback retains those controls.

## 6. Persistence, migration and recovery

Schema 4. Still one file, one atomic snapshot, one writer.

**Migration 3 → 4.** The old queue becomes one playlist named `Default` with `PlaylistId(1)`; entry IDs carry over; `playing` is that playlist; shuffle is off. Schema 1/2 files and read-only snapshots ignore playlist data exactly as they ignore queue data today.

**Recovery**, in order, each step resetting only what it affects and reporting why:

1. `playlists` absent, null or not an array → one empty `Default`.
2. Per playlist, `recover_queue`'s existing rules apply to its entries; a damaged playlist resets to empty and keeps its ID and name. A duplicate entry ID *within* one playlist keeps today's whole-queue reset, scoped to that playlist.
3. A playlist whose ID is malformed or repeats an earlier one keeps its content and receives a fresh ID. If the playlist counter is exhausted, that playlist is dropped instead, and reported.
4. An entry whose ID repeats one in an earlier playlist receives a fresh ID; if it was its playlist's cursor, the cursor clears. If the entry counter is exhausted, that later occurrence is dropped instead, and reported.

   Fresh IDs in steps 3 and 4 come from the counters of §4, which are computed over the whole file *before* any repair, so a repair can never mint an ID that appears later in the file. Repairs run in file order. The result is a fixed point: saving the repaired state and loading it again repairs nothing. The exhausted fallback loses queue data but never a checkpoint (P8); it needs a `u64::MAX` ID in the file, which ordinary use cannot produce.
5. A name is trimmed and truncated to 40 characters; an empty result becomes `Playlist <id>`.
6. Beyond `MAX_PLAYLISTS`, the first 32 are kept. Beyond `MAX_ENTRIES`, entries are kept in playlist order, then list order, up to 4,096. Both truncations are reported.
7. An empty array after the steps above → one empty `Default`.
8. `playing` malformed or dangling → the first playlist, with `current_media` re-pointed as in §5.
9. The `active` ↔ `current_media` check applies to the playing playlist only. Any other playlist's `active` is checked for membership only.
10. A malformed `shuffle` turns shuffle off for that playlist and is reported; a `first` that is not a member becomes `None` *silently*, because §7 makes that a legal state — shuffle on mid-track pins the cursor, and removing that entry leaves the pin behind — so reporting it would warn about damage that never happened.

## 7. Playback behavior

**Traversal (P6).** `Playlist::neighbor(anchor, direction)` returns list order with shuffle off, shuffled order with it on. Both existing callers use it; nothing calls `Queue::neighbor` directly afterwards. At either boundary it returns `None`: manual next/previous then does nothing and does not disturb a playing track, and automatic advance leaves playback ended. No wrap, no repeat modes.

**Shuffled order.** `first`, while it is still a member, then every other entry ascending by `(splitmix64(seed.wrapping_add(id)), id)`. `splitmix64` is written out in the crate and pinned by a test vector, because the standard library's hasher is not stable across releases. The order is computed on a keypress or a track end, never per frame.

**Toggling** acts on the *viewed* playlist. On: a fresh seed from `getrandom`, and `first` is that playlist's own cursor or `None` — never another playlist's track. If `getrandom` fails, shuffle is left unchanged and a notice is shown. Off: `shuffle = None`, and traversal continues in list order from the current entry. Neither sends an engine command, so a playing track keeps playing. Moving rows does not change the shuffled order; removal leaves the others' relative order alone.

Known ceiling, marked with a `ponytail:` comment: an entry added while shuffle is on lands at its hash position, which may be behind the current track, so this pass can miss it. Toggling off and on reshuffles everything ahead of the current track.

**Transport.** `TransportSituation` carries:

- `navigation: &Playlist` — `playing`, except during `Loading`, when it is `owner(last_requested)` while that entry is still queued (P5);
- `viewed: &Playlist` and the selected row in it;
- `retry: Option<QueueEntryId>` — `last_requested`, already validated by the caller through the owner lookup across *all* playlists. It replaces `still_queued(queue, last_requested)`: after a failed request in B, navigation is A and the view may be C, so neither supplied playlist could validate B's entry.

**Enter is the only command that reads the selection.** It plays the selected row of the viewed playlist, and its emptiness check is the viewed playlist's: an empty playing playlist never blocks Enter in a populated one.

**Space and `p`** never follow the viewed tab. Where today's table falls back to `selection` (`Unloaded`, `Ended`, `LoadFailed`), the chain becomes: `retry` (in `LoadFailed` only, as today) → the navigation playlist's cursor → its first entry in playback order → the empty-queue notice. A valid `retry` therefore outranks the empty check: with A empty and a failed request in B, `p` retries B's entry. Space and `p` are engine commands today (`Playing`, `Paused`, `Reconnecting`, `Stopped`: toggle pause, play) in all four phases when the navigation playlist has entries. Over an *emptied* playlist only `Playing`, `Paused` and `Reconnecting` keep them as engine commands (`decide_engine_with_empty_queue`); `Stopped` shows the empty-queue notice, exactly as it did before M8.

**Next and previous** anchor on `retry` during `Loading`, else on the navigation playlist's cursor; with no anchor they do nothing. Seek, restart and the live-media rules are unchanged.

This is a deliberate narrowing of today's table, where Space, `p`, next and previous fall back to the selected row: with one list that row was always in the playing queue; with several it may belong to another playlist, and one rule is simpler than a viewed-equals-navigation special case. Visible difference with a single playlist: on a playlist that has never played, `p` starts the first entry in playback order rather than the highlighted row, and next/previous wait for a cursor. Enter on the highlighted row is unchanged.

**Start from zero.** `resume_intent_for(media, entry)` gains the identity: `MediaId::LocalFile` and `MediaId::RemoteUrl` yield no resume, so the load starts at zero; `MediaId::PodcastEpisode` is unchanged; live media already never resumes. Both callers (§2) pass the media, so `tenuto play song.mp3` starts from zero too. Checkpoints are still written, so the played marker keeps working. Pause and Stop → Play stay engine commands and continue mid-track.

## 8. Folder add

**Keys, Files tab.** Space marks directory rows as well as audio rows. With nothing marked, Enter is unchanged: it opens a directory, and toggles an audio row in or out of the destination (`src/tui/browser.rs:514`). A new key `a` adds the marked rows, or the cursor row when nothing is marked — files and directories alike — and is strictly additive: a row already in the destination is skipped, never removed. With marks present, Enter on an audio row does exactly what `a` does, so a marked directory is never silently ignored; Enter on a directory still opens it. Marks clear when they are added and when the directory changes, as today. `a` is scoped to the Files tab; the Radio tab's `a` (add station) is untouched.

**One destination rule (P2).** The browser captures the viewed `PlaylistId` when it opens. Every request copies that ID, and the browser's queued indicators and Enter-to-remove refer to that same playlist.

**Request.** `BrowseRequest::CollectTree { roots: Vec<PathBuf>, dest: PlaylistId }` on the existing browse worker.

**Walk.** Recursive `std::fs::read_dir`; no new dependency. Depth-first, visiting each level in exactly `list_directory`'s order: subdirectories first, by case-insensitive name, each walked to the bottom, then the directory's own audio files by case-insensitive name. So a root holding `z.mp3` and `a/1.mp3` yields `a/1.mp3`, then `z.mp3`. Reusing the listing order keeps the walk and the browser's display in agreement. It guarantees a deterministic filename order, not album track order, and it decides what survives truncation: the earliest candidates in this order. Several roots are walked in their listing order. Only `AUDIO_EXTENSIONS` files are collected. A directory symlink is skipped (`symlink_metadata` before descending), which rules out cycles; a file symlink is followed as today. Candidates are deduplicated by resolved `MediaId` within the batch, which covers overlapping roots and file-symlink aliases. The walk stops once it holds `MAX_ENTRIES` candidates.

**Result.** `BrowseResult::TreeCollected { dest, items, unreadable: Vec<PathBuf>, scan_limit_reached: bool }`. It is routed through the application, not through `BrowserState`: closing the browser or changing its directory does not discard an explicitly requested addition. On apply: if `dest` no longer exists, the result is dropped with a notice; otherwise items already in the destination are skipped, global capacity is rechecked (P7), and the rest are enqueued in walk order up to the free capacity.

**Notice.** Counts are reported for what is known and nothing else: `added 212 · 7 already queued · 3 unreadable · 12 did not fit · scan limit reached`, each part only when nonzero or true. `did not fit` counts collected candidates rejected for capacity; `scan limit reached` says unvisited files may exist and gives no number. Because already-queued items are skipped after the walk, a truncated scan can leave free capacity; the notice does not claim otherwise. Unreadable paths go to the log.

New entries go to the existing `enrich` workers, so tags arrive after the rows do.

## 9. Rows

In `application/view.rs`: the title line is `Artist – Title` when the artist is known and non-empty, else the title as today. The subtitle becomes the album alone. Podcast and station rows are unchanged. The combined string passes through the existing `displayable` sanitization, as each part does today.

## 10. Playlist UI

**Tab strip.** One row above the list: `Default  Morning  ▶Workout ⤮`. `▶` marks the playing playlist, `⤮` a shuffled one; the viewed one is highlighted. Names pass through `displayable`. Widths are measured in terminal columns, not bytes or chars; the strip scrolls to keep the viewed tab visible, and a single name wider than the strip is clipped. The Minimal tier shows only the viewed name and `n/m`.

**Viewed playlist.** TUI state, not persisted. It starts as `playing`. When the viewed playlist is deleted, the view moves to the adjacent one.

**Keys, player screen only** — the browser's own `Tab` binding is untouched:

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | view the next / previous playlist |
| `n` | new playlist: name input, then view it |
| `r` | rename the viewed playlist |
| `D` | delete the viewed playlist after a `y`; refused for the last one |
| `z` | toggle shuffle on the viewed playlist |

`n` and `r` reuse `Overlay::Input` with a purpose; `D` follows `ConfirmClear`. Every overlay that acts on a playlist captures its target `PlaylistId` when it opens. The existing `a` (add URL), `c` (clear), `d`, `J`, `K` act on the viewed playlist.

## 11. Out of scope

Repeat modes; a transient "play next" queue; M3U import or export; playlist CLI subcommands; per-entry or per-playlist resume overrides; sorting; drag between playlists; persisting the viewed tab; splitting `state.json`. The last follows measured trouble (§12), not the cap.

## 12. Validation

Test-first, on the existing pure seams.

- **Playlist and queue.** The `splitmix64` vector; `first` pinning; order stable under add, remove and move; boundaries return `None`; ID exhaustion and capacity refusal change nothing; the cap holds across playlists.
- **Codec.** Migration 3 → 4; each recovery rule in §6, including both truncations and `current_media` re-pointing; every case keeps its checkpoints. Exhaustion together with duplicates: a file holding entry ID `u64::MAX` plus a cross-playlist duplicate, and the same for playlist IDs — the later occurrence is dropped, the counter is `null`, and saving then reopening the repaired state repairs nothing further. A repair never mints an ID that appears later in the file.
- **Session.** Release follows adoption, not the cursor: removing an unowned cursor (restored with nothing adopted; an inactive playlist's; the playing playlist's while another playlist's load is pending) clears it and neither releases nor stops; with A playing and B loading, removing A's owned entry leaves B's pending load valid; invalidation scoped to one playlist; `playing` changes only at adoption; a failed cross-playlist load does not change `playing` or any remembered cursor; `update_display` reaches every playlist.
- **Runtime.** Clearing an inactive playlist cancels no pending seek and loses no other playlist's enrichment request.
- **Transport.** A phase × command table with viewed ≠ playing, with an empty playing playlist, and with the `Loading` exception; with A empty and a failed request in B, `p` and Space retry B's entry while the view is on C; Space and `p` never load the viewed playlist's selection; shuffled manual navigation agrees with automatic advance.
- **Browse.** Walk order pinned by a fixture with root-level and nested audio (`a/1.mp3` before `z.mp3`), which also fixes what truncation keeps; directory-symlink cycle fixture; unreadable directories reported; scan limit; overlapping roots and file-symlink aliases deduplicated; completion applied after the browser closed; completion dropped with a notice after the playlist was deleted; `a` additive where Enter toggles.
- **Resume.** Local file and URL start at zero with a checkpoint present; a podcast still resumes; Stop → Play continues mid-track.
- **View and layout.** Row strings; tab-strip widths in columns, scrolling, one over-long name clipped; hostile names sanitized.

**Pending until performed**, with actual results recorded in `docs/m8-acceptance.md`:

- A manual pass in Ghostty with a real music folder.
- A full 4,096-entry snapshot with representative paths, URLs and tags, plus a realistic checkpoint map: bytes on disk, clone time on the application thread, serialization time, atomic-write time. The "about 1 MB" figure used in discussion is an estimate; pretty-printed JSON and variable-length fields make it uncertain. If the numbers are bad, the response is a follow-up design, not a silent change here.

## 13. Documentation

Done when the branch is finished, before it is proposed for merge:

- `CHANGELOG.md`, under Unreleased: playlists, shuffle, folder add, Artist – Title rows under Added; start-from-zero for local files and URLs, and the 4,096 global cap replacing 256, under Changed.
- `docs/reference.md`: the new keys, the Files-tab `a`, the tab strip, the resume rule.
- `docs/architecture.md`: schema 4 in the container table, the playlist model and ownership rules in components, the milestone list.
- The in-app help overlay.
- `docs/m8-acceptance.md`.

## 14. Modules

- `src/playlist.rs` (new): `PlaylistId`, `Shuffle`, `Playlist`, traversal, `splitmix64`.
- `src/queue.rs`: IDs supplied by the caller; the cap moves out.
- `src/persistence/model.rs`, `queue_codec.rs`: schema 4, allocators, migration, recovery.
- `src/session.rs`: explicit destinations, owner lookup, scoped invalidation, adoption sets `playing`, media-aware `resume_intent_for`.
- `src/application/transport.rs`, `runtime.rs`, `view.rs`: two-playlist situation, playlist commands, tree-result application, rows.
- `src/application/browse.rs`: `CollectTree` and the walk.
- `src/tui/`: tab strip, overlays, keys, the browser's captured destination and `a`.
- `src/app.rs`: the `play` path passes the media to `resume_intent_for`.
