# Tenuto M8: Playlists, Shuffle and Music-Player Behavior — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the single queue into several named playlists with per-playlist shuffle, start local files and URLs from zero, add folders recursively from the Files tab, and show `Artist – Title` rows.

**Architecture:** A `Playlist` wraps today's `Queue` and lives in `state.json` (schema 4) — still one owner (`Session`), one atomic snapshot, one writer. Entry IDs are globally unique and allocated by `PersistedState`; every add or clear carries an explicit `PlaylistId`. A playlist's `active` entry is only a remembered cursor: releasing or stopping playback is decided from `Session::adopted`, never from a cursor. The *viewed* playlist is transient state held by `PlayerRuntime` (not persisted), so `view()` and transport can read it.

**Tech Stack:** Rust 2024, serde/serde_json, ratatui + crossterm, `unicode-width`, `getrandom` (already a dependency), `tempfile` (dev). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-20-tenuto-playlists-design.md` — read it before any task. Section numbers below (§N) refer to it.

## Global Constraints

- Runtime code forbids `unsafe` and denies `clippy::unwrap_used` / `clippy::expect_used`. Inside a `#[test]` function `expect` is fine; in a test *helper* function use `unwrap_or_else(|error| panic!(...))` (see `tests/m5_session_queue.rs`).
- No new crates. `splitmix64` is written out in the crate; the recursive walk uses `std::fs`.
- `MAX_PLAYLIST_ENTRIES = 4096` across all playlists, replacing `MAX_QUEUE_ENTRIES = 256`. (The spec calls it `MAX_ENTRIES`; `src/persistence/model.rs` already has a `MAX_ENTRIES` for the checkpoint cap, so the code uses the longer name.) `MAX_PLAYLISTS = 32`. A playlist name is 1–40 characters after trimming.
- Entry and playlist IDs are monotonic, never reused, advanced with `checked_add`; an exhausted counter is `None` in memory and `null` on disk.
- No recovery or migration step may lose a checkpoint (P8).
- Every string that reaches the terminal passes through `crate::commands::displayable`.
- Commit messages carry no `Co-authored-by` or tool-attribution trailers.
- CI gates, run before every commit: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked --no-fail-fast`.
- Integration tests live in `tests/` with the milestone prefix: new files are `tests/m8_*.rs`, each starting with `mod support;` when it needs `support::media`.
- Do not mark the Ghostty pass or the snapshot measurement as done unless they were actually performed; `docs/m8-acceptance.md` records real results only.

## File Structure

| File | Responsibility |
|---|---|
| `src/playlist.rs` (new) | `PlaylistId`, `Shuffle`, `Playlist`, `PlaylistError`, `splitmix64`, playback order, `neighbor`, `first_in_order`, `clean_name`, limits |
| `src/queue.rs` | `IdAllocator`; `Queue::enqueue` takes IDs from it; the per-queue cap and `next_id` go away; `MAX_PLAYLIST_ENTRIES` |
| `src/persistence/model.rs` | `PersistedState` holds `playlists`, `playing`, two allocators; owner lookup; global cap; schema 4 |
| `src/persistence/queue_codec.rs` | schema-4 encode, `recover_playlists`, migration wrapper, new `PlaylistProblem` |
| `src/session.rs` | explicit destinations, owner lookup, ownership-by-adoption, scoped invalidation, playlist ops, adoption sets `playing`, traversal via `Playlist::neighbor`, media-aware `resume_intent_for` |
| `src/application/transport.rs` | two-playlist `TransportSituation`, `retry`, Enter-only selection |
| `src/application/runtime.rs` | `viewed`, playlist commands, scoped side effects, tree-result application |
| `src/application/view.rs` | `Artist – Title` rows, `PlaylistTab`s in `PlayerView` |
| `src/application/browse.rs` | `CollectTree` request, `collect_tree` walk, `TreeCollected` result |
| `src/tui/browser.rs` | captured destination, Space marks directories, Files-tab `a` |
| `src/tui/tabs.rs` (new) | pure tab-strip layout in terminal columns |
| `src/tui/{state,input,render,mod}.rs` | overlays with captured targets, keys, strip drawing, routing `TreeCollected` to the runtime |
| `src/app.rs` | `play` path passes the media to `resume_intent_for` |
| `tests/m8_*.rs` | one file per seam, listed in each task |
| `docs/`, `CHANGELOG.md` | Task 17 |

Task order is dependency order. Tasks 1–4 are pure data; 5–8 are `Session`; 9–11 application; 12–15 browse and TUI; 16–17 measurement and docs.

---

### Task 1: `playlist.rs` — identity, shuffle order, traversal

**Files:**
- Create: `src/playlist.rs`
- Modify: `src/lib.rs` (add `pub mod playlist;` beside `pub mod queue;`)
- Test: `tests/m8_playlist.rs`

**Interfaces:**
- Consumes: `crate::queue::{Queue, QueueEntryId, Direction}` as they exist today.
- Produces:
  - `pub const MAX_PLAYLISTS: usize = 32; pub const MAX_NAME_CHARS: usize = 40;`
  - `pub struct PlaylistId(u64)` with `get(self) -> u64`, `pub(crate) fn from_raw(u64) -> Self`
  - `pub struct Shuffle { pub seed: u64, pub first: Option<QueueEntryId> }` (`Clone, Copy, Debug, Eq, PartialEq`)
  - `pub struct Playlist` with `new(id, name: String)`, `from_parts(id, name, shuffle, queue)`, `id()`, `name()`, `shuffle()`, `queue()`, `pub(crate) queue_mut()`, `pub(crate) set_name(String)`, `pub(crate) set_shuffle(Option<Shuffle>)`, `playback_order() -> Vec<QueueEntryId>`, `neighbor(anchor, Direction) -> Option<QueueEntryId>`, `first_in_order() -> Option<QueueEntryId>`
  - `pub fn splitmix64(x: u64) -> u64`
  - `pub fn clean_name(raw: &str) -> Option<String>`
  - `pub enum PlaylistError { TooMany, LastPlaylist, Unknown(PlaylistId), InvalidName, IdExhausted }`

- [ ] **Step 1: Write the failing tests**

`tests/m8_playlist.rs`:

```rust
mod support;

use support::media;
use tenuto::media::id::MediaId;
use tenuto::playlist::{Playlist, PlaylistId, Shuffle, clean_name, splitmix64};
use tenuto::queue::{
    Direction, DisplayMetadata, NewQueueEntry, Queue, QueueEntryId, QueueSource,
};

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else {
        unreachable!()
    };
    NewQueueEntry::new(media(name), QueueSource::LocalFile(path), DisplayMetadata::default())
        .unwrap_or_else(|error| panic!("a literal entry must be valid: {error}"))
}

/// A playlist of five entries whose IDs are 1..=5.
fn five(shuffle: Option<Shuffle>) -> (Playlist, Vec<QueueEntryId>) {
    let mut queue = Queue::default();
    let ids = queue
        .enqueue(["a", "b", "c", "d", "e"].map(entry).to_vec())
        .unwrap_or_else(|error| panic!("fits: {error}"));
    let playlist = Playlist::from_parts(PlaylistId::from_raw_for_tests(1), "P".into(), shuffle, queue);
    (playlist, ids)
}

#[test]
fn splitmix64_matches_the_reference_vector() {
    assert_eq!(splitmix64(0), 0xE220_A839_7B1D_CDAF);
    assert_eq!(splitmix64(0x9E37_79B9_7F4A_7C15), 0x6E78_9E6A_A1B9_65F4);
}

#[test]
fn list_order_is_used_when_shuffle_is_off() {
    let (playlist, ids) = five(None);
    assert_eq!(playlist.playback_order(), ids);
    assert_eq!(playlist.neighbor(ids[0], Direction::Down), Some(ids[1]));
    assert_eq!(playlist.neighbor(ids[0], Direction::Up), None);
    assert_eq!(playlist.neighbor(ids[4], Direction::Down), None);
    assert_eq!(playlist.first_in_order(), Some(ids[0]));
}

#[test]
fn seed_42_orders_ids_one_to_five_as_5_1_4_3_2() {
    // Pinned: sorted by (splitmix64(42 + id), id). Computed independently.
    let (playlist, ids) = five(Some(Shuffle { seed: 42, first: None }));
    let expected = [ids[4], ids[0], ids[3], ids[2], ids[1]];
    assert_eq!(playlist.playback_order(), expected);
    assert_eq!(playlist.first_in_order(), Some(ids[4]));
    assert_eq!(playlist.neighbor(ids[4], Direction::Down), Some(ids[0]));
    assert_eq!(playlist.neighbor(ids[0], Direction::Up), Some(ids[4]));
    assert_eq!(playlist.neighbor(ids[1], Direction::Down), None, "no wrap at the end");
    assert_eq!(playlist.neighbor(ids[4], Direction::Up), None, "no wrap at the start");
}

#[test]
fn first_is_pinned_ahead_of_the_hashed_order_while_it_is_a_member() {
    let (playlist, ids) = five(Some(Shuffle { seed: 42, first: Some(ids_third()) }));
    assert_eq!(
        playlist.playback_order(),
        [ids[2], ids[4], ids[0], ids[3], ids[1]]
    );
}

fn ids_third() -> QueueEntryId {
    five(None).1[2]
}

#[test]
fn a_first_that_is_not_a_member_is_ignored() {
    let (mut playlist, ids) = five(Some(Shuffle { seed: 42, first: Some(ids_third()) }));
    playlist
        .queue_mut_for_tests()
        .remove(ids[2])
        .expect("queued");
    assert_eq!(playlist.playback_order(), [ids[4], ids[0], ids[3], ids[1]]);
}

#[test]
fn removing_or_moving_an_entry_leaves_the_others_relative_order_alone() {
    let (mut playlist, ids) = five(Some(Shuffle { seed: 42, first: None }));
    playlist
        .queue_mut_for_tests()
        .move_entry(ids[0], Direction::Down)
        .expect("queued");
    assert_eq!(
        playlist.playback_order(),
        [ids[4], ids[0], ids[3], ids[2], ids[1]],
        "moving rows does not change the shuffled order"
    );
    playlist.queue_mut_for_tests().remove(ids[3]).expect("queued");
    assert_eq!(playlist.playback_order(), [ids[4], ids[0], ids[2], ids[1]]);
}

#[test]
fn an_unknown_anchor_has_no_neighbor() {
    let (mut playlist, ids) = five(Some(Shuffle { seed: 42, first: None }));
    playlist.queue_mut_for_tests().remove(ids[0]).expect("queued");
    assert_eq!(playlist.neighbor(ids[0], Direction::Down), None);
}

#[test]
fn names_are_trimmed_truncated_to_forty_chars_and_never_empty() {
    assert_eq!(clean_name("  Morning  ").as_deref(), Some("Morning"));
    assert_eq!(clean_name("   "), None);
    let long = "é".repeat(50);
    assert_eq!(clean_name(&long).map(|name| name.chars().count()), Some(40));
}
```

`from_raw_for_tests` and `queue_mut_for_tests` are `#[doc(hidden)] pub` shims (Step 3) because integration tests cannot see `pub(crate)` items; this mirrors how the crate already exposes `QueueEntryId::from_raw` only to itself.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test m8_playlist`
Expected: compile error — `unresolved import tenuto::playlist`.

- [ ] **Step 3: Write `src/playlist.rs`**

```rust
//! A named playlist (M8 §4): today's `Queue` plus an identity, a name and an
//! optional shuffle. Pure data and policy, like `queue.rs`: it never
//! renders, decodes or touches the filesystem, and it changes only through
//! `Session`.

use crate::queue::{Direction, Queue, QueueEntryId};

pub const MAX_PLAYLISTS: usize = 32;
pub const MAX_NAME_CHARS: usize = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PlaylistId(u64);

impl PlaylistId {
    pub fn get(self) -> u64 {
        self.0
    }
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
    #[doc(hidden)]
    pub fn from_raw_for_tests(raw: u64) -> Self {
        Self(raw)
    }
}

/// Shuffle is on exactly while a playlist holds one of these (§7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shuffle {
    pub seed: u64,
    /// Sorts ahead of every hashed key while it is still a member, so the
    /// track playing when shuffle was switched on has the whole list ahead.
    pub first: Option<QueueEntryId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlaylistError {
    #[error("at most {MAX_PLAYLISTS} playlists")]
    TooMany,
    #[error("the last playlist cannot be deleted")]
    LastPlaylist,
    #[error("playlist {} no longer exists", .0.get())]
    Unknown(PlaylistId),
    #[error("a playlist name is 1 to {MAX_NAME_CHARS} characters")]
    InvalidName,
    #[error("playlist IDs are exhausted")]
    IdExhausted,
}

/// SplitMix64's output function over `x + golden gamma`. Written out
/// because the standard library's hasher is not stable across releases and
/// a shuffled order must survive a restart. Pinned by a test vector.
pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Trimmed, cut to [`MAX_NAME_CHARS`] characters; `None` when nothing is left.
pub fn clean_name(raw: &str) -> Option<String> {
    let name: String = raw.trim().chars().take(MAX_NAME_CHARS).collect();
    let name = name.trim_end().to_owned();
    (!name.is_empty()).then_some(name)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Playlist {
    id: PlaylistId,
    name: String,
    shuffle: Option<Shuffle>,
    queue: Queue,
}

impl Playlist {
    pub(crate) fn new(id: PlaylistId, name: String) -> Self {
        Self::from_parts(id, name, None, Queue::default())
    }

    /// Persistence's constructor and the tests'. The caller has validated
    /// the name.
    pub fn from_parts(id: PlaylistId, name: String, shuffle: Option<Shuffle>, queue: Queue) -> Self {
        Self { id, name, shuffle, queue }
    }

    pub fn id(&self) -> PlaylistId {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn shuffle(&self) -> Option<Shuffle> {
        self.shuffle
    }
    pub fn queue(&self) -> &Queue {
        &self.queue
    }
    pub(crate) fn queue_mut(&mut self) -> &mut Queue {
        &mut self.queue
    }
    #[doc(hidden)]
    pub fn queue_mut_for_tests(&mut self) -> &mut Queue {
        &mut self.queue
    }
    pub(crate) fn set_name(&mut self, name: String) {
        self.name = name;
    }
    pub(crate) fn set_shuffle(&mut self, shuffle: Option<Shuffle>) {
        self.shuffle = shuffle;
    }

    /// The order traversal follows: list order, or with shuffle on `first`
    /// then ascending `(splitmix64(seed + id), id)`.
    ///
    /// ponytail: O(n log n) per call, called on a keypress or a track end,
    /// never per frame. Cache the order if n ever grows past the 4,096 cap.
    ///
    /// ponytail: an entry added while shuffle is on lands at its hash
    /// position, which may be behind the current track, so this pass can
    /// miss it. Toggling shuffle off and on reshuffles what is ahead.
    pub fn playback_order(&self) -> Vec<QueueEntryId> {
        let mut ids: Vec<QueueEntryId> = self.queue.entries().iter().map(|e| e.id()).collect();
        if let Some(shuffle) = self.shuffle {
            ids.sort_by_key(|id| {
                (
                    shuffle.first != Some(*id),
                    splitmix64(shuffle.seed.wrapping_add(id.get())),
                    id.get(),
                )
            });
        }
        ids
    }

    /// One traversal policy for manual navigation and automatic advance
    /// (P6). `None` at either boundary: no wrap.
    pub fn neighbor(&self, anchor: QueueEntryId, direction: Direction) -> Option<QueueEntryId> {
        let order = self.playback_order();
        let index = order.iter().position(|id| *id == anchor)?;
        let target = match direction {
            Direction::Up => index.checked_sub(1)?,
            Direction::Down => index + 1,
        };
        order.get(target).copied()
    }

    pub fn first_in_order(&self) -> Option<QueueEntryId> {
        self.playback_order().first().copied()
    }
}
```

Add `pub mod playlist;` to `src/lib.rs` next to `pub mod queue;`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test m8_playlist`
Expected: 9 passed.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add src/playlist.rs src/lib.rs tests/m8_playlist.rs
git commit -m "feat(playlist): playlist identity, seeded shuffle order and one traversal policy"
```

---

### Task 2: `IdAllocator` — IDs come from outside the queue

**Files:**
- Modify: `src/queue.rs` (`MAX_QUEUE_ENTRIES`, `Queue { next_id }`, `enqueue`, `from_parts`, `exhaustion_tests`)
- Modify: every caller of `Queue::enqueue` and `MAX_QUEUE_ENTRIES` — find them with `grep -rn 'MAX_QUEUE_ENTRIES\|\.enqueue(' src tests`
- Test: `tests/m8_playlist.rs` (append)

**Interfaces:**
- Produces:
  - `pub const MAX_PLAYLIST_ENTRIES: usize = 4096;` (replaces `MAX_QUEUE_ENTRIES`; the *global* check moves to `PersistedState` in Task 3 — `Queue` itself no longer checks capacity)
  - `pub struct IdAllocator` (`Clone, Copy, Debug, Eq, PartialEq`, `Default` = next ID 1) with `starting_at(Option<u64>) -> Self`, `next(self) -> Option<u64>`, `reserve(&mut self, count: usize) -> Result<std::ops::RangeInclusive<u64>, QueueError>`, `observe(&mut self, seen: u64)`
  - `Queue::enqueue(&mut self, batch: Vec<NewQueueEntry>, ids: &mut IdAllocator) -> Result<Vec<QueueEntryId>, QueueError>`
  - `QueueError::UnknownPlaylist(u64)` (raw ID, so `queue.rs` need not import `playlist.rs`)

- [ ] **Step 1: Write the failing tests** (append to `tests/m8_playlist.rs`)

```rust
use tenuto::queue::{IdAllocator, QueueError};

#[test]
fn an_allocator_reserves_a_contiguous_range_or_nothing() {
    let mut ids = IdAllocator::default();
    assert_eq!(ids.reserve(3).expect("room"), 1..=3);
    assert_eq!(ids.next(), Some(4));

    let mut last = IdAllocator::starting_at(Some(u64::MAX));
    assert_eq!(last.reserve(2), Err(QueueError::IdExhausted));
    assert_eq!(last.next(), Some(u64::MAX), "a refused batch changes nothing");
    assert_eq!(last.reserve(1).expect("the last ID"), u64::MAX..=u64::MAX);
    assert_eq!(last.next(), None, "handing out u64::MAX exhausts the namespace");
    assert_eq!(last.reserve(1), Err(QueueError::IdExhausted));
}

#[test]
fn observing_an_id_never_lowers_the_counter_and_max_exhausts_it() {
    let mut ids = IdAllocator::default();
    ids.observe(9);
    assert_eq!(ids.next(), Some(10));
    ids.observe(3);
    assert_eq!(ids.next(), Some(10));
    ids.observe(u64::MAX);
    assert_eq!(ids.next(), None);
}

#[test]
fn two_queues_sharing_an_allocator_never_share_an_id() {
    let mut ids = IdAllocator::default();
    let (mut a, mut b) = (Queue::default(), Queue::default());
    let first = a.enqueue(vec![entry("a"), entry("b")], &mut ids).expect("ids");
    let second = b.enqueue(vec![entry("a")], &mut ids).expect("ids");
    assert_eq!(first.iter().map(|id| id.get()).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(second[0].get(), 3);
}
```

Update the `five` helper in the same file to the new signature:

```rust
    let mut ids_alloc = IdAllocator::default();
    let ids = queue
        .enqueue(["a", "b", "c", "d", "e"].map(entry).to_vec(), &mut ids_alloc)
        .unwrap_or_else(|error| panic!("fits: {error}"));
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_playlist`
Expected: compile error — `IdAllocator` not found.

- [ ] **Step 3: Implement in `src/queue.rs`**

Replace `pub const MAX_QUEUE_ENTRIES: usize = 256;` with:

```rust
/// The cap on entries across *all* playlists (M8 §4). Enforced by
/// `PersistedState`, the one place IDs are allocated, never by a `Queue`.
pub const MAX_PLAYLIST_ENTRIES: usize = 4096;

/// The next ID to hand out, or `None` once `u64::MAX` has been handed out:
/// a `u64` alone cannot say "past the end" (M8 §4). Monotonic, never reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdAllocator {
    next: Option<u64>,
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self { next: Some(1) }
    }
}

impl IdAllocator {
    pub fn starting_at(next: Option<u64>) -> Self {
        Self { next }
    }

    pub fn next(self) -> Option<u64> {
        self.next
    }

    /// `count` contiguous IDs, all or nothing. `count` must be nonzero.
    pub fn reserve(&mut self, count: usize) -> Result<std::ops::RangeInclusive<u64>, QueueError> {
        let count = u64::try_from(count).map_err(|_| QueueError::IdExhausted)?;
        let first = self.next.ok_or(QueueError::IdExhausted)?;
        let last = count
            .checked_sub(1)
            .and_then(|span| first.checked_add(span))
            .ok_or(QueueError::IdExhausted)?;
        self.next = last.checked_add(1);
        Ok(first..=last)
    }

    /// Raises the counter past an ID found in a file.
    pub fn observe(&mut self, seen: u64) {
        self.next = match (self.next, seen.checked_add(1)) {
            (Some(next), Some(after)) => Some(next.max(after)),
            _ => None,
        };
    }
}
```

Add to `QueueError`:

```rust
    #[error("playlist {0} no longer exists")]
    UnknownPlaylist(u64),
```

Update the `Capacity` message to `"at most {MAX_PLAYLIST_ENTRIES} entries across all playlists; {requested} requested, {available} free"`.

Remove the `next_id` field from `Queue` (and from `Default` and `from_parts`, which becomes `Self { entries, active }`). Replace `enqueue`:

```rust
    /// Appends `batch` with IDs from `ids`, all or nothing. Capacity is the
    /// caller's check: the cap is global (M8 P7), and a queue cannot see it.
    pub fn enqueue(
        &mut self,
        batch: Vec<NewQueueEntry>,
        ids: &mut IdAllocator,
    ) -> Result<Vec<QueueEntryId>, QueueError> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let range = ids.reserve(batch.len())?;
        Ok(range
            .zip(batch)
            .map(|(raw, new)| {
                let id = QueueEntryId(raw);
                self.entries.push(QueueEntry {
                    id,
                    media: new.media,
                    source: new.source,
                    display: new.display,
                });
                id
            })
            .collect())
    }
```

Rewrite `exhaustion_tests` against the allocator: build `IdAllocator::starting_at(Some(u64::MAX))`, assert a batch of two is `IdExhausted` and leaves both the queue and the allocator equal to their clones, that one entry then gets `u64::MAX`, and that a further enqueue — including after `queue.clear()` — is `IdExhausted` while an empty batch is `Ok`.

- [ ] **Step 4: Fix the callers so the crate compiles, behavior unchanged**

This task keeps one queue. In `src/persistence/model.rs` add a field `entry_ids: IdAllocator` to `PersistedState` (default `IdAllocator::default()`); in `RawState::into_state` build it after recovery:

```rust
        let mut entry_ids = IdAllocator::default();
        for entry in queue.entries() {
            entry_ids.observe(entry.id().get());
        }
```

Add `pub(crate) fn enqueue(&mut self, batch: Vec<NewQueueEntry>) -> Result<Vec<QueueEntryId>, QueueError>` to `PersistedState` that checks `MAX_PLAYLIST_ENTRIES.saturating_sub(self.queue.len())` exactly as `Queue::enqueue` used to (returning `QueueError::Capacity`) and then calls `self.queue.enqueue(batch, &mut self.entry_ids)`. Point `Session::enqueue` at it. In `queue_codec::decode_entries` and `runtime.rs` replace `MAX_QUEUE_ENTRIES` with `MAX_PLAYLIST_ENTRIES`.

In tests that call `Queue::enqueue` directly (`tests/m5_queue.rs`, `tests/m5_transport_rules.rs`, others the grep finds) pass `&mut IdAllocator::default()`. Tests asserting the old 256 cap (`tests/m5_session_queue.rs`, `tests/m5_state_recovery.rs`'s `over_capacity` fixture of 300) move to the new number: enqueue `MAX_PLAYLIST_ENTRIES + 1`, and make the fixture `1..=5000` expecting `OverCapacity { found: 5000 }`.

- [ ] **Step 5: Run everything**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "refactor(queue): entry IDs come from a shared allocator; the cap becomes 4,096 and leaves Queue"
```

---

### Task 3: `PersistedState` holds playlists (in memory; the file format waits for Task 4)

**Files:**
- Modify: `src/persistence/model.rs` (the `queue: Queue` field and its accessors, `Default`, `RawState::into_state`, `Serialize`)
- Test: `tests/m8_state_playlists.rs`

**Interfaces:**
- Consumes: `Playlist`, `PlaylistId`, `PlaylistError`, `clean_name`, `MAX_PLAYLISTS` (Task 1); `IdAllocator`, `MAX_PLAYLIST_ENTRIES`, `QueueError::{Capacity, UnknownPlaylist}` (Task 2).
- Produces, on `PersistedState`:
  - `pub fn playlists(&self) -> &[Playlist]` — never empty (P1)
  - `pub fn playing(&self) -> PlaylistId`
  - `pub fn playlist(&self, id: PlaylistId) -> Option<&Playlist>`
  - `pub fn playing_playlist(&self) -> &Playlist`
  - `pub fn queue(&self) -> &Queue` — the *playing* playlist's queue; for transport and advance only
  - `pub fn owner_of(&self, entry: QueueEntryId) -> Option<PlaylistId>`
  - `pub fn find_entry(&self, entry: QueueEntryId) -> Option<&QueueEntry>`
  - `pub fn total_entries(&self) -> usize`
  - `pub(crate) fn playlist_mut(&mut self, id: PlaylistId) -> Option<&mut Playlist>`
  - `pub(crate) fn queue_mut(&mut self) -> &mut Queue` — the playing playlist's
  - `pub(crate) fn enqueue(&mut self, dest: PlaylistId, batch: Vec<NewQueueEntry>) -> Result<Vec<QueueEntryId>, QueueError>`
  - `pub(crate) fn create_playlist(&mut self, name: &str) -> Result<PlaylistId, PlaylistError>`
  - `pub(crate) fn rename_playlist(&mut self, id: PlaylistId, name: &str) -> Result<(), PlaylistError>`
  - `pub(crate) fn remove_playlist(&mut self, id: PlaylistId) -> Result<Playlist, PlaylistError>` — moves `playing` to the adjacent playlist and re-points `current_media` (§5)
  - `pub(crate) fn set_playing(&mut self, id: PlaylistId)` — no-op for an unknown ID

Because `pub(crate)` mutators are invisible to `tests/`, this task's tests live in a `#[cfg(test)] mod playlist_tests` at the bottom of `src/persistence/model.rs`; `tests/m8_state_playlists.rs` is created in Task 4 for the file format.

- [ ] **Step 1: Write the failing tests** (bottom of `src/persistence/model.rs`)

```rust
#[cfg(test)]
mod playlist_tests {
    use super::*;
    use crate::media::id::AbsolutePath;
    use crate::playlist::{MAX_PLAYLISTS, PlaylistError};
    use crate::queue::{DisplayMetadata, MAX_PLAYLIST_ENTRIES, NewQueueEntry, QueueError, QueueSource};

    fn entry(name: &str) -> NewQueueEntry {
        let path = AbsolutePath::new(format!("/music/{name}.flac").into())
            .unwrap_or_else(|error| panic!("absolute: {error}"));
        NewQueueEntry::new(
            MediaId::LocalFile(path.clone()),
            QueueSource::LocalFile(path),
            DisplayMetadata::default(),
        )
        .unwrap_or_else(|error| panic!("valid: {error}"))
    }

    #[test]
    fn a_fresh_state_has_one_playing_playlist_named_default() {
        let state = PersistedState::default();
        assert_eq!(state.playlists().len(), 1);
        assert_eq!(state.playlists()[0].name(), "Default");
        assert_eq!(state.playing(), state.playlists()[0].id());
    }

    #[test]
    fn entry_ids_are_unique_across_playlists_and_owner_lookup_finds_them() {
        let mut state = PersistedState::default();
        let a = state.playing();
        let b = state.create_playlist("B").expect("room");
        let in_a = state.enqueue(a, vec![entry("x")]).expect("fits");
        let in_b = state.enqueue(b, vec![entry("x")]).expect("fits");
        assert_ne!(in_a[0], in_b[0]);
        assert_eq!(state.owner_of(in_a[0]), Some(a));
        assert_eq!(state.owner_of(in_b[0]), Some(b));
        assert_eq!(state.total_entries(), 2);
        assert!(state.queue().get(in_b[0]).is_none(), "queue() is the playing playlist only");
    }

    #[test]
    fn the_cap_is_global_and_a_refused_batch_changes_nothing() {
        let mut state = PersistedState::default();
        let a = state.playing();
        let b = state.create_playlist("B").expect("room");
        state
            .enqueue(a, (0..MAX_PLAYLIST_ENTRIES - 1).map(|i| entry(&format!("t{i}"))).collect())
            .expect("fits");
        let before = state.clone();
        assert_eq!(
            state.enqueue(b, vec![entry("y"), entry("z")]),
            Err(QueueError::Capacity { requested: 2, available: 1 })
        );
        assert_eq!(state.total_entries(), before.total_entries());
        assert_eq!(state.entry_ids, before.entry_ids, "no ID was burned");
    }

    #[test]
    fn enqueue_into_a_deleted_playlist_is_refused() {
        let mut state = PersistedState::default();
        let b = state.create_playlist("B").expect("room");
        state.remove_playlist(b).expect("another exists");
        assert_eq!(
            state.enqueue(b, vec![entry("x")]),
            Err(QueueError::UnknownPlaylist(b.get()))
        );
    }

    #[test]
    fn playlist_ids_are_never_reused() {
        let mut state = PersistedState::default();
        let b = state.create_playlist("B").expect("room");
        state.remove_playlist(b).expect("another exists");
        let c = state.create_playlist("C").expect("room");
        assert!(c.get() > b.get());
    }

    #[test]
    fn limits_and_names() {
        let mut state = PersistedState::default();
        assert_eq!(state.create_playlist("   "), Err(PlaylistError::InvalidName));
        for i in 1..MAX_PLAYLISTS {
            state.create_playlist(&format!("P{i}")).expect("room");
        }
        assert_eq!(state.create_playlist("one too many"), Err(PlaylistError::TooMany));
        let only = PersistedState::default();
        let mut only = only;
        assert_eq!(
            only.remove_playlist(only.playing()).map(|_| ()),
            Err(PlaylistError::LastPlaylist)
        );
    }

    #[test]
    fn deleting_the_playing_playlist_moves_playing_and_repoints_current_media() {
        let mut state = PersistedState::default();
        let a = state.playing();
        let b = state.create_playlist("B").expect("room");
        let in_b = state.enqueue(b, vec![entry("kept")]).expect("fits");
        let _ = state.playlist_mut(b).map(|p| p.queue_mut().set_active(Some(in_b[0])));
        state.set_current_media(entry_media("old"));

        state.remove_playlist(a).expect("another exists");

        assert_eq!(state.playing(), b);
        assert_eq!(state.queue().active(), Some(in_b[0]), "the cursor is not judged against the old media");
        assert_eq!(state.current_media(), Some(&entry_media("kept")));
    }

    #[test]
    fn deleting_the_playing_playlist_clears_current_media_when_the_next_has_no_cursor() {
        let mut state = PersistedState::default();
        let a = state.playing();
        state.create_playlist("B").expect("room");
        state.set_current_media(entry_media("old"));
        state.remove_playlist(a).expect("another exists");
        assert_eq!(state.current_media(), None);
    }

    #[test]
    fn deleting_another_playlist_touches_neither_playing_nor_current_media() {
        let mut state = PersistedState::default();
        let a = state.playing();
        let b = state.create_playlist("B").expect("room");
        state.set_current_media(entry_media("old"));
        state.remove_playlist(b).expect("another exists");
        assert_eq!(state.playing(), a);
        assert_eq!(state.current_media(), Some(&entry_media("old")));
    }

    fn entry_media(name: &str) -> MediaId {
        entry(name).media().clone()
    }
}
```

Add to `NewQueueEntry` in `src/queue.rs`: `pub fn media(&self) -> &MediaId { &self.media }` — Task 13 uses it too.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib playlist_tests`
Expected: compile errors — `playlists`, `create_playlist` … not found.

- [ ] **Step 3: Implement**

In `PersistedState` replace `queue: Queue` (and Task 2's `entry_ids`) with:

```rust
    /// Never empty (M8 P1).
    playlists: Vec<Playlist>,
    /// Always names a member of `playlists`.
    playing: PlaylistId,
    entry_ids: IdAllocator,
    playlist_ids: IdAllocator,
```

`Default`: one playlist `Playlist::new(PlaylistId::from_raw(1), "Default".into())`, `playing` = that ID, `entry_ids: IdAllocator::default()`, `playlist_ids: IdAllocator::starting_at(Some(2))`.

Methods:

```rust
    pub fn playlists(&self) -> &[Playlist] {
        &self.playlists
    }

    pub fn playing(&self) -> PlaylistId {
        self.playing
    }

    fn index_of(&self, id: PlaylistId) -> Option<usize> {
        self.playlists.iter().position(|playlist| playlist.id() == id)
    }

    /// `playlists` is never empty and `playing` always names a member; index
    /// 0 is the fallback that keeps this total without an `unwrap`.
    fn playing_index(&self) -> usize {
        self.index_of(self.playing).unwrap_or(0)
    }

    pub fn playlist(&self, id: PlaylistId) -> Option<&Playlist> {
        self.index_of(id).map(|index| &self.playlists[index])
    }

    pub(crate) fn playlist_mut(&mut self, id: PlaylistId) -> Option<&mut Playlist> {
        self.index_of(id).map(|index| &mut self.playlists[index])
    }

    pub fn playing_playlist(&self) -> &Playlist {
        &self.playlists[self.playing_index()]
    }

    /// The *playing* playlist's queue: a convenience for transport and
    /// advance. Everything else names its playlist or looks an entry's
    /// owner up (M8 §5).
    pub fn queue(&self) -> &Queue {
        self.playing_playlist().queue()
    }

    pub(crate) fn queue_mut(&mut self) -> &mut Queue {
        let index = self.playing_index();
        self.playlists[index].queue_mut()
    }

    pub fn owner_of(&self, entry: QueueEntryId) -> Option<PlaylistId> {
        self.playlists
            .iter()
            .find(|playlist| playlist.queue().get(entry).is_some())
            .map(Playlist::id)
    }

    pub fn find_entry(&self, entry: QueueEntryId) -> Option<&QueueEntry> {
        self.playlists.iter().find_map(|playlist| playlist.queue().get(entry))
    }

    pub fn total_entries(&self) -> usize {
        self.playlists.iter().map(|playlist| playlist.queue().len()).sum()
    }

    /// All or nothing, against the global cap, with IDs from the one
    /// allocator (M8 P2, P7).
    pub(crate) fn enqueue(
        &mut self,
        dest: PlaylistId,
        batch: Vec<NewQueueEntry>,
    ) -> Result<Vec<QueueEntryId>, QueueError> {
        let index = self
            .index_of(dest)
            .ok_or(QueueError::UnknownPlaylist(dest.get()))?;
        let available = MAX_PLAYLIST_ENTRIES.saturating_sub(self.total_entries());
        if batch.len() > available {
            return Err(QueueError::Capacity { requested: batch.len(), available });
        }
        self.playlists[index].queue_mut().enqueue(batch, &mut self.entry_ids)
    }

    pub(crate) fn create_playlist(&mut self, name: &str) -> Result<PlaylistId, PlaylistError> {
        let name = clean_name(name).ok_or(PlaylistError::InvalidName)?;
        if self.playlists.len() >= MAX_PLAYLISTS {
            return Err(PlaylistError::TooMany);
        }
        let raw = self
            .playlist_ids
            .reserve(1)
            .map_err(|_| PlaylistError::IdExhausted)?;
        let id = PlaylistId::from_raw(*raw.start());
        self.playlists.push(Playlist::new(id, name));
        Ok(id)
    }

    pub(crate) fn rename_playlist(&mut self, id: PlaylistId, name: &str) -> Result<(), PlaylistError> {
        let name = clean_name(name).ok_or(PlaylistError::InvalidName)?;
        self.playlist_mut(id).ok_or(PlaylistError::Unknown(id))?.set_name(name);
        Ok(())
    }

    /// Removing the playing playlist moves `playing` to the next playlist,
    /// else the previous, and re-points `current_media` at that playlist's
    /// cursor — or clears it — so the cursor is never judged against the
    /// deleted playlist's media (M8 §5). Checkpoints are untouched.
    pub(crate) fn remove_playlist(&mut self, id: PlaylistId) -> Result<Playlist, PlaylistError> {
        let index = self.index_of(id).ok_or(PlaylistError::Unknown(id))?;
        if self.playlists.len() == 1 {
            return Err(PlaylistError::LastPlaylist);
        }
        let removed = self.playlists.remove(index);
        if self.playing == id {
            let next = index.min(self.playlists.len() - 1);
            self.playing = self.playlists[next].id();
            self.repoint_current_media();
        }
        Ok(removed)
    }

    pub(super) fn repoint_current_media(&mut self) {
        let queue = self.queue();
        self.current_media = queue
            .active()
            .and_then(|id| queue.get(id))
            .map(|entry| entry.media().clone());
    }

    pub(crate) fn set_playing(&mut self, id: PlaylistId) {
        if self.index_of(id).is_some() {
            self.playing = id;
        }
    }
```

`RawState::into_state` (file format unchanged until Task 4): wrap the recovered `queue` as `Playlist::from_parts(PlaylistId::from_raw(1), "Default".into(), None, queue)`, observe its entry IDs into `entry_ids`, `playlist_ids = IdAllocator::starting_at(Some(2))`. `Serialize` keeps writing `queue`/`active_entry` from `self.queue()`.

Point `Session::enqueue` at `self.state.enqueue(self.state.playing(), batch)` for now; Task 5 gives it a destination.

- [ ] **Step 4: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass, including `playlist_tests` (9).

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src
git commit -m "feat(state): PersistedState holds playlists, a playing ID, a global cap and shared allocators"
```

---

### Task 4: Schema 4 — encode, migration 3 → 4, recovery

**Files:**
- Modify: `src/persistence/queue_codec.rs`, `src/persistence/model.rs` (`SCHEMA_VERSION`, `RawState`, `into_state`, `Serialize`), `src/persistence/store.rs` only if `OLDEST_MIGRATABLE_VERSION` handling needs the new number in a message
- Modify: `tests/m5_state_schema.rs`, `tests/m5_state_recovery.rs`, `tests/persistence_*.rs` where they assert `"schema_version": 3` or the `queue`/`active_entry` keys in *written* output
- Test: `tests/m8_state_playlists.rs`

**Interfaces:**
- Consumes: Task 3's `PersistedState` fields; `recover_queue` and `decode_entries` as they exist.
- Produces:
  - `pub const SCHEMA_VERSION: u32 = 4;`
  - `pub enum PlaylistProblem { Malformed, DuplicatePlaylistId, DuplicateEntryId, IdsExhausted, TooManyPlaylists { found: usize }, OverCapacity { found: usize }, DanglingPlaying, Shuffle }`
  - `QueueReset::Playlists(PlaylistProblem)` — `fields_reset()` returns `"one or more playlists"`
  - `pub(super) struct Recovered { pub playlists: Vec<Playlist>, pub playing: PlaylistId, pub entry_ids: IdAllocator, pub playlist_ids: IdAllocator, pub current_media: Option<MediaId>, pub reset: Option<QueueReset> }`
  - `pub(super) fn recover_playlists(raw: RawPlaylists<'_>, current_media: Option<MediaId>) -> Recovered`
  - `pub(super) fn migrate_queue(queue: Queue, current_media: Option<MediaId>, reset: Option<QueueReset>) -> Recovered`
- On-disk shape (schema 4):

```json
{ "schema_version": 4, "current_media": "...", "volume": 1.0, "checkpoints": {},
  "playlists": [ { "id": 1, "name": "Default",
                   "shuffle": { "seed": 42, "first": 5 },
                   "entries": [ { "id": 5, "media": "...", "source": {}, "display": {} } ],
                   "active_entry": 5 } ],
  "playing": 1, "next_entry_id": 6, "next_playlist_id": 2 }
```

`shuffle`, `first`, `active_entry` may be `null`. `next_*_id` is a number, or `null` for an exhausted namespace. The first recorded problem wins the `reset` slot; the store already backs the original file up byte-for-byte whenever `reset` is `Some` (`QueueBackup`), so the exhausted-ID fallback that drops data is recoverable by hand.

- [ ] **Step 1: Write the failing tests** — `tests/m8_state_playlists.rs`

```rust
use serde_json::{Value, json};
use std::sync::Arc;
use tenuto::clock::FakeClock;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::queue_codec::{ActiveProblem, PlaylistProblem, QueueReset};
use tenuto::persistence::store::{LoadReason, StateStore};

mod support;

fn track(id: u64, name: &str) -> Value {
    json!({ "id": id, "media": format!("local:/music/{name}.flac"),
            "source": { "kind": "local", "path": format!("/music/{name}.flac") } })
}

fn checkpoints() -> Value {
    json!({ "local:/music/a.flac": { "position": { "secs": 5, "nanos": 0 }, "completed": false,
            "touch_seq": 3, "updated_at": "2026-09-14T10:00:00Z" } })
}

fn v4(playlists: Value, playing: Value, extra: Value) -> Value {
    let mut state = json!({ "schema_version": 4, "current_media": "local:/music/a.flac",
        "volume": 0.5, "checkpoints": checkpoints(), "playlists": playlists, "playing": playing });
    if let (Some(state), Some(extra)) = (state.as_object_mut(), extra.as_object()) {
        state.extend(extra.clone());
    }
    state
}

struct Loaded {
    state: PersistedState,
    reset: Option<QueueReset>,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn load(value: Value) -> Loaded {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let path = dir.path().join("state.json");
    let bytes = serde_json::to_vec_pretty(&value).unwrap_or_else(|error| panic!("json: {error}"));
    std::fs::write(&path, bytes).unwrap_or_else(|error| panic!("write: {error}"));
    let outcome = StateStore::new(path.clone(), Arc::new(FakeClock::new())).load();
    assert!(matches!(outcome.reason, LoadReason::Loaded));
    assert!(outcome.writable);
    Loaded { reset: outcome.queue_repair.map(|repair| repair.reset), state: outcome.state, path, _dir: dir }
}

fn kept_checkpoint(state: &PersistedState) {
    assert_eq!(state.len(), 1, "no recovery step may cost a checkpoint (P8)");
}

fn ids(state: &PersistedState, playlist: usize) -> Vec<u64> {
    state.playlists()[playlist].queue().entries().iter().map(|e| e.id().get()).collect()
}

#[test]
fn a_schema_3_queue_becomes_the_default_playlist_with_its_ids_and_cursor() {
    let loaded = load(json!({ "schema_version": 3, "current_media": "local:/music/a.flac",
        "volume": 0.5, "checkpoints": checkpoints(),
        "queue": [track(4, "a"), track(9, "b")], "active_entry": 4 }));
    let state = &loaded.state;
    assert_eq!(loaded.reset, None);
    assert_eq!(state.playlists().len(), 1);
    assert_eq!(state.playlists()[0].name(), "Default");
    assert_eq!(state.playlists()[0].id().get(), 1);
    assert_eq!(state.playing().get(), 1);
    assert_eq!(ids(state, 0), [4, 9]);
    assert_eq!(state.queue().active().map(|id| id.get()), Some(4));
    assert_eq!(state.playlists()[0].shuffle(), None);
    assert_eq!(state.schema_version(), 4);
    kept_checkpoint(state);
}

#[test]
fn a_schema_4_file_round_trips() {
    let value = v4(
        json!([{ "id": 1, "name": "Default", "shuffle": null, "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "Jazz", "shuffle": { "seed": 42, "first": 3 },
                 "entries": [track(2, "b"), track(3, "c")], "active_entry": 3 }]),
        json!(1),
        json!({ "next_entry_id": 4, "next_playlist_id": 3 }),
    );
    let loaded = load(value);
    assert_eq!(loaded.reset, None);
    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["schema_version"], 4);
    assert_eq!(written["playing"], 1);
    assert_eq!(written["next_entry_id"], 4);
    assert_eq!(written["next_playlist_id"], 3);
    assert_eq!(written["playlists"][1]["shuffle"], json!({ "seed": 42, "first": 3 }));
    assert_eq!(written["playlists"][1]["active_entry"], 3);
    assert!(written.get("queue").is_none() && written.get("active_entry").is_none());
    let again: PersistedState = serde_json::from_value(written.clone()).expect("reads back");
    assert_eq!(serde_json::to_value(&again).expect("serializes"), written);
}

#[test]
fn only_the_playing_cursor_is_judged_against_current_media() {
    // current_media is a.flac. Playlist 2's cursor is b.flac and must survive.
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "B", "entries": [track(2, "b")], "active_entry": 2 }]),
        json!(1), json!({})));
    assert_eq!(loaded.reset, None);
    assert_eq!(loaded.state.playlists()[1].queue().active().map(|id| id.get()), Some(2));

    let mismatch = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "z")], "active_entry": 1 }]),
        json!(1), json!({})));
    assert_eq!(mismatch.reset, Some(QueueReset::ActiveReference(ActiveProblem::MediaMismatch)));
    assert_eq!(mismatch.state.queue().active(), None);
    kept_checkpoint(&mismatch.state);
}

#[test]
fn malformed_playlists_become_one_empty_default() {
    let loaded = load(v4(json!("nope"), json!(1), json!({})));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::Malformed)));
    assert_eq!(loaded.state.playlists().len(), 1);
    assert_eq!(loaded.state.playlists()[0].name(), "Default");
    kept_checkpoint(&loaded.state);
}

#[test]
fn a_damaged_playlist_resets_alone_and_keeps_its_id_and_name() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "Good", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "Bad", "entries": [track(5, "b"), track(5, "c")], "active_entry": null }]),
        json!(1), json!({})));
    assert!(matches!(loaded.reset, Some(QueueReset::WholeQueue(_))));
    assert_eq!(ids(&loaded.state, 0), [1]);
    assert_eq!(loaded.state.playlists()[1].name(), "Bad");
    assert_eq!(loaded.state.playlists()[1].id().get(), 2);
    assert!(ids(&loaded.state, 1).is_empty());
    kept_checkpoint(&loaded.state);
}

#[test]
fn a_repeated_playlist_id_gets_a_fresh_one_that_appears_nowhere_in_the_file() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [], "active_entry": null },
               { "id": 1, "name": "B", "entries": [track(1, "b")], "active_entry": null },
               { "id": 7, "name": "C", "entries": [], "active_entry": null }]),
        json!(1), json!({})));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::DuplicatePlaylistId)));
    let got: Vec<u64> = loaded.state.playlists().iter().map(|p| p.id().get()).collect();
    assert_eq!(got, [1, 8, 7], "8, not 2..=7: the counter is computed over the whole file first");
    assert_eq!(ids(&loaded.state, 1), [1], "the repaired playlist keeps its content");
}

#[test]
fn an_entry_id_repeated_across_playlists_is_renumbered_and_its_cursor_clears() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "B", "entries": [track(1, "b"), track(6, "c")], "active_entry": 1 }]),
        json!(1), json!({})));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::DuplicateEntryId)));
    assert_eq!(ids(&loaded.state, 0), [1]);
    assert_eq!(ids(&loaded.state, 1), [7, 6]);
    assert_eq!(loaded.state.playlists()[1].queue().active(), None);
}

#[test]
fn with_entry_ids_exhausted_the_later_duplicate_is_dropped_and_the_repair_is_a_fixed_point() {
    let max = u64::MAX;
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(max, "a")], "active_entry": max },
               { "id": 2, "name": "B", "entries": [track(max, "b"), track(3, "c")], "active_entry": null }]),
        json!(1), json!({})));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::IdsExhausted)));
    assert_eq!(ids(&loaded.state, 0), [max]);
    assert_eq!(ids(&loaded.state, 1), [3]);
    kept_checkpoint(&loaded.state);

    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["next_entry_id"], Value::Null);
    let reopened = load(written.clone());
    assert_eq!(reopened.reset, None, "saving then reopening repairs nothing further");
    assert_eq!(serde_json::to_value(&reopened.state).expect("serializes"), written);
}

#[test]
fn with_playlist_ids_exhausted_the_later_conflicting_playlist_is_dropped() {
    let max = u64::MAX;
    let loaded = load(v4(
        json!([{ "id": max, "name": "A", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": max, "name": "B", "entries": [track(2, "b")], "active_entry": null }]),
        json!(max), json!({})));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::IdsExhausted)));
    assert_eq!(loaded.state.playlists().len(), 1);
    assert_eq!(loaded.state.playlists()[0].name(), "A");
    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["next_playlist_id"], Value::Null);
    assert_eq!(load(written).reset, None);
}

#[test]
fn a_stored_null_counter_stays_exhausted_even_when_no_max_id_remains() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "a")], "active_entry": 1 }]),
        json!(1), json!({ "next_entry_id": null })));
    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["next_entry_id"], Value::Null);
}

#[test]
fn names_are_cleaned_and_an_empty_one_is_named_after_its_id() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": format!("  {}  ", "x".repeat(60)), "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "   ", "entries": [], "active_entry": null },
               { "id": 3, "entries": [], "active_entry": null }]),
        json!(1), json!({})));
    let names: Vec<&str> = loaded.state.playlists().iter().map(|p| p.name()).collect();
    assert_eq!(names[0].chars().count(), 40);
    assert_eq!(&names[1..], ["Playlist 2", "Playlist 3"]);
}

#[test]
fn both_caps_truncate_in_file_order_and_say_so() {
    let many: Vec<Value> = (1..=40)
        .map(|i| json!({ "id": i, "name": format!("P{i}"), "entries": [], "active_entry": null }))
        .collect();
    let loaded = load(v4(json!(many), json!(1), json!({})));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::TooManyPlaylists { found: 40 })));
    assert_eq!(loaded.state.playlists().len(), 32);

    let first: Vec<Value> = (1..=4000).map(|i| track(i, &format!("t{i}"))).collect();
    let second: Vec<Value> = (4001..=4200).map(|i| track(i, &format!("t{i}"))).collect();
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": first, "active_entry": null },
               { "id": 2, "name": "B", "entries": second, "active_entry": 4200 }]),
        json!(1), json!({ "current_media": null })));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::OverCapacity { found: 4200 })));
    assert_eq!(loaded.state.total_entries(), 4096);
    assert_eq!(ids(&loaded.state, 1).last(), Some(&4096));
    assert_eq!(loaded.state.playlists()[1].queue().active(), None, "a truncated cursor clears");
}

#[test]
fn a_dangling_playing_falls_back_to_the_first_playlist_and_repoints_current_media() {
    // current_media is a.flac; the first playlist's cursor is b.flac and must survive.
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "b")], "active_entry": 1 }]),
        json!(99), json!({})));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::DanglingPlaying)));
    assert_eq!(loaded.state.playing().get(), 1);
    assert_eq!(loaded.state.queue().active().map(|id| id.get()), Some(1));
    assert_eq!(loaded.state.current_media(), Some(&support::media("b")));
    kept_checkpoint(&loaded.state);
}

#[test]
fn a_bad_shuffle_turns_shuffle_off_and_a_foreign_first_becomes_none() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "shuffle": "yes", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "B", "shuffle": { "seed": 7, "first": 1 }, "entries": [track(2, "b")], "active_entry": null }]),
        json!(1), json!({})));
    assert_eq!(loaded.reset, Some(QueueReset::Playlists(PlaylistProblem::Shuffle)));
    assert_eq!(loaded.state.playlists()[0].shuffle(), None);
    let shuffle = loaded.state.playlists()[1].shuffle().expect("kept");
    assert_eq!((shuffle.seed, shuffle.first), (7, None));
}
```

`Loaded::path` is unused by some tests; add `#[allow(dead_code)]` on the field rather than removing it — Task 16 reuses this helper.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_state_playlists`
Expected: compile error — `PlaylistProblem` not found.

- [ ] **Step 3: Implement the codec** (`src/persistence/queue_codec.rs`)

Add the problem type and the variant:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistProblem {
    Malformed,
    DuplicatePlaylistId,
    DuplicateEntryId,
    /// A repair needed a fresh ID and the namespace had none left, so the
    /// later conflicting entry or playlist was dropped instead (M8 §6).
    IdsExhausted,
    TooManyPlaylists { found: usize },
    OverCapacity { found: usize },
    DanglingPlaying,
    Shuffle,
}
// enum QueueReset { …, Playlists(PlaylistProblem) }
// fields_reset(): Self::Playlists(_) => "one or more playlists",
```

Remove the `OverCapacity` check from `decode_entries` (the cap is global now) but keep it in `recover_queue`'s schema-3 path by checking `entries.len() > MAX_PLAYLIST_ENTRIES` there.

Encoding:

```rust
#[derive(Serialize)]
pub(crate) struct ShuffleDto {
    seed: u64,
    first: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct PlaylistDto {
    id: u64,
    name: String,
    shuffle: Option<ShuffleDto>,
    entries: Vec<QueueEntryDto>,
    active_entry: Option<u64>,
}

pub(crate) fn encode_playlists(playlists: &[Playlist]) -> Vec<PlaylistDto> {
    playlists
        .iter()
        .map(|playlist| PlaylistDto {
            id: playlist.id().get(),
            name: playlist.name().to_owned(),
            shuffle: playlist.shuffle().map(|shuffle| ShuffleDto {
                seed: shuffle.seed,
                first: shuffle.first.map(QueueEntryId::get),
            }),
            entries: encode(playlist.queue()),
            active_entry: playlist.queue().active().map(QueueEntryId::get),
        })
        .collect()
}
```

Recovery. The rules run in §6's order; `note` keeps the first problem:

```rust
pub(super) struct RawPlaylists<'a> {
    pub playlists: Option<&'a Value>,
    pub playing: Option<&'a Value>,
    /// `None` = the key is absent; `Some(Null)` = an exhausted namespace.
    pub next_entry_id: Option<&'a Value>,
    pub next_playlist_id: Option<&'a Value>,
}

pub(super) struct Recovered {
    pub playlists: Vec<Playlist>,
    pub playing: PlaylistId,
    pub entry_ids: IdAllocator,
    pub playlist_ids: IdAllocator,
    pub current_media: Option<MediaId>,
    pub reset: Option<QueueReset>,
}

fn note(reset: &mut Option<QueueReset>, problem: QueueReset) {
    reset.get_or_insert(problem);
}

/// The stored counter raised past every well-formed ID in the file. Computed
/// before any repair, so a repair never mints an ID that appears later.
fn counter(stored: Option<&Value>, seen: impl Iterator<Item = u64>) -> IdAllocator {
    let start = match stored {
        Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().unwrap_or(1).max(1)),
        None => Some(1),
    };
    let mut ids = IdAllocator::starting_at(start);
    seen.for_each(|id| ids.observe(id));
    ids
}

fn fresh(ids: &mut IdAllocator) -> Option<u64> {
    ids.reserve(1).ok().map(|range| *range.start())
}

fn default_playlists(current_media: Option<MediaId>, reset: Option<QueueReset>) -> Recovered {
    Recovered {
        playlists: vec![Playlist::from_parts(PlaylistId::from_raw(1), "Default".into(), None, Queue::default())],
        playing: PlaylistId::from_raw(1),
        entry_ids: IdAllocator::default(),
        playlist_ids: IdAllocator::starting_at(Some(2)),
        current_media,
        reset,
    }
}

/// Schema 3 → 4: the one queue becomes `Default`, IDs and cursor carried over.
pub(super) fn migrate_queue(
    queue: Queue,
    current_media: Option<MediaId>,
    reset: Option<QueueReset>,
) -> Recovered {
    let mut entry_ids = IdAllocator::default();
    queue.entries().iter().for_each(|entry| entry_ids.observe(entry.id().get()));
    Recovered {
        playlists: vec![Playlist::from_parts(PlaylistId::from_raw(1), "Default".into(), None, queue)],
        playing: PlaylistId::from_raw(1),
        entry_ids,
        playlist_ids: IdAllocator::starting_at(Some(2)),
        current_media,
        reset,
    }
}

pub(super) fn recover_playlists(raw: RawPlaylists<'_>, current_media: Option<MediaId>) -> Recovered {
    let mut reset = None;
    // Rule 1.
    let items = match raw.playlists {
        Some(Value::Array(items)) => items,
        None | Some(Value::Null) => return default_playlists(current_media, None),
        Some(_) => {
            return default_playlists(
                current_media,
                Some(QueueReset::Playlists(PlaylistProblem::Malformed)),
            );
        }
    };

    // Counters first, over the whole file.
    let playlist_seen = items.iter().filter_map(|item| item.get("id")?.as_u64());
    let entry_seen = items
        .iter()
        .filter_map(|item| item.get("entries")?.as_array())
        .flatten()
        .filter_map(|entry| entry.get("id")?.as_u64());
    let mut playlist_ids = counter(raw.next_playlist_id, playlist_seen);
    let mut entry_ids = counter(raw.next_entry_id, entry_seen);

    let found_entries: usize = items
        .iter()
        .filter_map(|item| item.get("entries")?.as_array())
        .map(Vec::len)
        .sum();
    if items.len() > MAX_PLAYLISTS {
        note(&mut reset, QueueReset::Playlists(PlaylistProblem::TooManyPlaylists { found: items.len() }));
    }

    let mut seen_playlists = BTreeSet::new();
    let mut seen_entries = BTreeSet::new();
    let mut total = 0usize;
    let mut playlists = Vec::new();

    for item in items.iter().take(MAX_PLAYLISTS) {
        // Rule 3: a malformed or repeated playlist ID.
        let id = match item.get("id").and_then(Value::as_u64).filter(|id| seen_playlists.insert(*id)) {
            Some(id) => id,
            None => match fresh(&mut playlist_ids) {
                Some(id) => {
                    note(&mut reset, QueueReset::Playlists(PlaylistProblem::DuplicatePlaylistId));
                    seen_playlists.insert(id);
                    id
                }
                None => {
                    note(&mut reset, QueueReset::Playlists(PlaylistProblem::IdsExhausted));
                    continue;
                }
            },
        };

        // Rule 2: this playlist's entries, under the existing per-queue rules.
        let decoded = match item.get("entries") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(entries)) => decode_entries(entries).unwrap_or_else(|problem| {
                note(&mut reset, QueueReset::WholeQueue(problem));
                Vec::new()
            }),
            Some(_) => {
                note(&mut reset, QueueReset::WholeQueue(QueueProblem::Malformed));
                Vec::new()
            }
        };

        // Rules 4 and 6: cross-playlist duplicates, then the global cap.
        let mut cursor = item.get("active_entry").and_then(Value::as_u64);
        let mut entries = Vec::with_capacity(decoded.len());
        for entry in decoded {
            if total >= MAX_PLAYLIST_ENTRIES {
                // Noted here, not after the loop: the truncated cursor's own
                // `Dangling` note below must not win the first-problem slot.
                note(
                    &mut reset,
                    QueueReset::Playlists(PlaylistProblem::OverCapacity { found: found_entries }),
                );
                if cursor == Some(entry.id().get()) {
                    cursor = None;
                }
                continue;
            }
            let raw_id = entry.id().get();
            let entry = if seen_entries.insert(raw_id) {
                entry
            } else {
                if cursor == Some(raw_id) {
                    cursor = None;
                }
                match fresh(&mut entry_ids) {
                    Some(new_id) => {
                        note(&mut reset, QueueReset::Playlists(PlaylistProblem::DuplicateEntryId));
                        seen_entries.insert(new_id);
                        Queue::entry_from_parts(
                            QueueEntryId::from_raw(new_id),
                            entry.media().clone(),
                            entry.source().clone(),
                            entry.display().clone(),
                        )
                    }
                    None => {
                        note(&mut reset, QueueReset::Playlists(PlaylistProblem::IdsExhausted));
                        continue;
                    }
                }
            };
            total += 1;
            entries.push(entry);
        }

        // Rule 9, the membership half: every playlist's cursor must be a member.
        let active = match item.get("active_entry") {
            None | Some(Value::Null) => None,
            Some(_) => match cursor.filter(|id| entries.iter().any(|e| e.id().get() == *id)) {
                Some(id) => Some(QueueEntryId::from_raw(id)),
                None => {
                    note(&mut reset, QueueReset::ActiveReference(ActiveProblem::Dangling));
                    None
                }
            },
        };

        // Rule 10.
        let shuffle = match item.get("shuffle") {
            None | Some(Value::Null) => None,
            Some(value) => match value.get("seed").and_then(Value::as_u64) {
                Some(seed) => {
                    let named = value.get("first").filter(|first| !first.is_null());
                    let first = named
                        .and_then(Value::as_u64)
                        .filter(|id| entries.iter().any(|e| e.id().get() == *id))
                        .map(QueueEntryId::from_raw);
                    if named.is_some() && first.is_none() {
                        note(&mut reset, QueueReset::Playlists(PlaylistProblem::Shuffle));
                    }
                    Some(Shuffle { seed, first })
                }
                None => {
                    note(&mut reset, QueueReset::Playlists(PlaylistProblem::Shuffle));
                    None
                }
            },
        };

        // Rule 5.
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .and_then(clean_name)
            .unwrap_or_else(|| format!("Playlist {id}"));

        playlists.push(Playlist::from_parts(
            PlaylistId::from_raw(id),
            name,
            shuffle,
            Queue::from_parts(entries, active),
        ));
    }
    // Rule 7.
    let Some(first) = playlists.first().map(Playlist::id) else {
        let mut recovered = default_playlists(current_media, reset);
        recovered.entry_ids = entry_ids;
        recovered.playlist_ids = playlist_ids;
        recovered.playlist_ids.observe(1);
        return recovered;
    };

    // Rule 8, then the media half of rule 9 — never both: a re-pointed
    // `playing` re-points `current_media` to match, so its cursor survives.
    let named = raw.playing.and_then(Value::as_u64).map(PlaylistId::from_raw);
    let playing = named.filter(|id| playlists.iter().any(|p| p.id() == *id));
    let (playing, current_media) = match playing {
        Some(playing) => {
            if let Some(playlist) = playlists.iter_mut().find(|p| p.id() == playing) {
                let queue = playlist.queue();
                let cursor_media = queue.active().and_then(|id| queue.get(id)).map(|e| e.media());
                if cursor_media.is_some() && cursor_media != current_media.as_ref() {
                    note(&mut reset, QueueReset::ActiveReference(ActiveProblem::MediaMismatch));
                    let _ = playlist.queue_mut().set_active(None);
                }
            }
            (playing, current_media)
        }
        None => {
            note(&mut reset, QueueReset::Playlists(PlaylistProblem::DanglingPlaying));
            let queue = playlists[0].queue();
            let media = queue.active().and_then(|id| queue.get(id)).map(|e| e.media().clone());
            (first, media)
        }
    };

    Recovered { playlists, playing, entry_ids, playlist_ids, current_media, reset }
}
```

Check the existing rule in `recover_queue`: when `active_entry` is absent it accepts any `current_media`. The playing-cursor check above does the same — it only fires when a cursor exists.

- [ ] **Step 4: Wire the model** (`src/persistence/model.rs`)

`pub const SCHEMA_VERSION: u32 = 4;`. `RawState` gains, beside the schema-3 `queue`/`active_entry` it keeps for migration:

```rust
    #[serde(default)]
    playlists: Option<serde_json::Value>,
    #[serde(default)]
    playing: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present")]
    next_entry_id: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present")]
    next_playlist_id: Option<serde_json::Value>,

/// Keeps `null` apart from an absent key: `Option<Value>` alone folds both
/// into `None`, and a `null` counter means "exhausted" (M8 §4).
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}
```

`into_state`:

```rust
        let recovered = if !accept_queue || self.schema_version < 3 {
            queue_codec::migrate_queue(Queue::default(), self.current_media, None)
        } else if self.schema_version == 3 {
            let (queue, reset) = queue_codec::recover_queue(
                self.queue.as_ref(),
                self.active_entry.as_ref(),
                self.current_media.as_ref(),
            );
            queue_codec::migrate_queue(queue, self.current_media, reset)
        } else {
            queue_codec::recover_playlists(
                queue_codec::RawPlaylists {
                    playlists: self.playlists.as_ref(),
                    playing: self.playing.as_ref(),
                    next_entry_id: self.next_entry_id.as_ref(),
                    next_playlist_id: self.next_playlist_id.as_ref(),
                },
                self.current_media,
            )
        };
```

then build `PersistedState` from `recovered` (its `current_media`, not `self.current_media`) and return `recovered.reset`. `Serialize`'s `Out` drops `queue`/`active_entry` and gains `playlists: queue_codec::encode_playlists(&self.playlists)`, `playing: self.playing.get()`, `next_entry_id: self.entry_ids.next()`, `next_playlist_id: self.playlist_ids.next()` — `Option<u64>` serializes `None` as `null`.

- [ ] **Step 5: Update the schema-3-era tests**

`grep -rn '"schema_version"\|active_entry\|\["queue"\]' tests` — every assertion on *written* output moves to `playlists[0].entries` / `playlists[0].active_entry` / `schema_version == 4`. Fixtures that are *read* stay schema 3: they now exercise migration.

- [ ] **Step 6: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass; `m8_state_playlists` 14 passed.

- [ ] **Step 7: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(state): schema 4 — playlists on disk, migration from 3, ordered recovery that keeps every checkpoint"
```

---

### Task 5: `Session` — explicit destinations and owner lookup

**Files:**
- Modify: `src/session.rs` (`register_load` :314, `enqueue` :421, `move_entry` :434, `update_display` :537, `update_podcast_fallback` :601, `absorb_load_metadata` :624)
- Modify: `src/application/runtime.rs` (the one `session.enqueue` call, :981) and every `session.enqueue(` in `tests/`
- Test: `tests/m8_session_playlists.rs`

**Interfaces:**
- Consumes: Task 3's `PersistedState` API.
- Produces:
  - `Session::enqueue(&mut self, dest: PlaylistId, batch: Vec<NewQueueEntry>) -> Result<(Vec<QueueEntryId>, Action), QueueError>`
  - `Session::create_playlist(&mut self, name: &str) -> Result<(PlaylistId, Action), PlaylistError>` (`Ordinary` submit)
  - `Session::rename_playlist(&mut self, id: PlaylistId, name: &str) -> Result<Action, PlaylistError>` (`Ordinary` submit)
  - `move_entry`, `register_load`, `update_display`, `update_podcast_fallback`, `absorb_load_metadata` resolve entries through the owner, across all playlists

- [ ] **Step 1: Write the failing tests** — `tests/m8_session_playlists.rs`

```rust
mod support;

use std::time::Duration;

use support::media;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::command::LoadRequestId;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::timeline::PositionQuality;
use tenuto::playlist::PlaylistId;
use tenuto::queue::{Direction, DisplayMetadata, NewQueueEntry, QueueEntryId, QueueError, QueueSource};
use tenuto::session::{Advance, DisplayUpdate, LoadTarget, Session};

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else {
        unreachable!()
    };
    NewQueueEntry::new(media(name), QueueSource::LocalFile(path), DisplayMetadata::default())
        .unwrap_or_else(|error| panic!("a literal entry must be valid: {error}"))
}

fn loaded(request: LoadRequestId, rev: u64, name: &str) -> PlaybackEvent {
    PlaybackEvent::Loaded {
        session_rev: rev,
        request,
        media: media(name),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities { continuity: Continuity::Finite, seek: SeekSupport::Native },
        position: Duration::ZERO,
        disposition: StartDisposition::Fresh,
    }
}

fn progress(rev: u64, name: &str, load: Option<LoadRequestId>) -> Progress {
    Progress {
        session_rev: rev,
        media: Some(media(name)),
        position: Duration::from_secs(9),
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load,
    }
}

/// Playlist A (the default, playing) holds a1 a2; playlist B holds b1 b2.
struct Two {
    session: Session,
    a: PlaylistId,
    b: PlaylistId,
    in_a: Vec<QueueEntryId>,
    in_b: Vec<QueueEntryId>,
}

fn two() -> Two {
    let mut session = Session::new(PersistedState::default());
    let a = session.state().playing();
    let (b, _) = session
        .create_playlist("B")
        .unwrap_or_else(|error| panic!("room: {error}"));
    let (in_a, _) = session
        .enqueue(a, vec![entry("a1"), entry("a2")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    let (in_b, _) = session
        .enqueue(b, vec![entry("b1"), entry("b2")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    Two { session, a, b, in_a, in_b }
}

/// Registers and adopts a load of `id`, at engine revision `rev`.
fn adopt(session: &mut Session, id: QueueEntryId, name: &str, rev: u64) -> LoadRequestId {
    let request = session
        .register_load(LoadTarget::Queue(id), &media(name))
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    session.observe(&loaded(request, rev, name), FakeClock::new().sample());
    request
}

#[test]
fn enqueue_lands_in_the_named_playlist_and_a_deleted_one_refuses() {
    let mut two = two();
    assert_eq!(two.session.state().playlist(two.b).expect("B").queue().len(), 2);
    assert_eq!(two.session.state().queue().len(), 2, "A, the playing playlist, is untouched by B's adds");
    let gone = PlaylistId::from_raw_for_tests(99);
    assert_eq!(
        two.session.enqueue(gone, vec![entry("x")]).map(|_| ()),
        Err(QueueError::UnknownPlaylist(99))
    );
}

#[test]
fn moving_and_loading_find_an_entry_in_any_playlist() {
    let mut two = two();
    two.session.move_entry(two.in_b[0], Direction::Down).expect("B's entry is known");
    let order: Vec<_> = two.session.state().playlist(two.b).expect("B").queue().entries().iter().map(|e| e.id()).collect();
    assert_eq!(order, [two.in_b[1], two.in_b[0]]);
    assert!(two.session.register_load(LoadTarget::Queue(two.in_b[0]), &media("b1")).is_ok());
}

#[test]
fn a_display_update_reaches_every_occurrence_in_every_playlist() {
    let mut two = two();
    let (extra, _) = two.session.enqueue(two.b, vec![entry("a1")]).expect("fits");
    two.session.update_display(
        &media("a1"),
        DisplayUpdate { artist: Some("Artist".into()), ..DisplayUpdate::default() },
    );
    let state = two.session.state();
    for id in [two.in_a[0], extra[0]] {
        assert_eq!(state.find_entry(id).expect("queued").display().artist.as_deref(), Some("Artist"));
    }
}
```

`Advance` and `a` are used by Tasks 6–7, which append to this file; add `#[allow(dead_code)]` on `Two` and `#[allow(unused_imports)]` on the import line until then.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_session_playlists`
Expected: compile error — `create_playlist` not found, `enqueue` takes one argument.

- [ ] **Step 3: Implement**

```rust
    pub fn enqueue(
        &mut self,
        dest: PlaylistId,
        batch: Vec<NewQueueEntry>,
    ) -> Result<(Vec<QueueEntryId>, Action), QueueError> {
        let ids = self.state.enqueue(dest, batch)?;
        Ok((ids, self.submit(Urgency::Ordinary)))
    }

    pub fn create_playlist(&mut self, name: &str) -> Result<(PlaylistId, Action), PlaylistError> {
        let id = self.state.create_playlist(name)?;
        Ok((id, self.submit(Urgency::Ordinary)))
    }

    pub fn rename_playlist(&mut self, id: PlaylistId, name: &str) -> Result<Action, PlaylistError> {
        self.state.rename_playlist(id, name)?;
        Ok(self.submit(Urgency::Ordinary))
    }
```

Add one private helper and use it everywhere an entry is reached by ID:

```rust
    /// The queue that holds `id`, in whichever playlist owns it (M8 §5).
    fn owner_queue_mut(&mut self, id: QueueEntryId) -> Option<&mut Queue> {
        let owner = self.state.owner_of(id)?;
        self.state.playlist_mut(owner).map(Playlist::queue_mut)
    }
```

- `register_load`: replace `self.state.queue().get(id)` with `self.state.find_entry(id)`.
- `move_entry`: `self.owner_queue_mut(id).ok_or(QueueError::UnknownEntry(id))?.move_entry(id, direction)?`.
- `update_display`: collect `ids` from `self.state.playlists().iter().flat_map(|p| p.queue().entries())`, then mutate through `owner_queue_mut(id)` instead of `queue_mut()`.
- `update_podcast_fallback`, `absorb_load_metadata`: same substitution — `find_entry` to read, `owner_queue_mut` to write.

In `runtime.rs` pass `self.session.state().playing()` for now (Task 10 replaces it with the captured destination). In `tests/`, `session.enqueue(batch)` becomes `session.enqueue(session.state().playing(), batch)` — a two-phase borrow, so it compiles as written.

- [ ] **Step 4: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(session): enqueue names its playlist; entries are found through their owner"
```

---

### Task 6: `Session` — release follows adoption, invalidation is scoped

**Files:**
- Modify: `src/session.rs` (`remove_entry` :454, `clear_queue` :482, `release_active` :515)
- Modify: `tests/m5_session_queue.rs` — `clear_queue` call (:150) and any assertion that removing a *restored, never adopted* cursor reports `stop_playback: true`; under §5 it now reports `false`
- Test: `tests/m8_session_playlists.rs` (append)

**Interfaces:**
- Produces:
  - `Session::owns_entry(&self, id: QueueEntryId) -> bool` — `adopted.target == LoadTarget::Queue(id)`
  - `Session::owns_playlist(&self, id: PlaylistId) -> bool` — the adopted entry's owner is `id`
  - `Session::clear_playlist(&mut self, id: PlaylistId, progress: &Progress, now: ClockSample) -> Result<Removal, PlaylistError>` (replaces `clear_queue`)
  - `Session::delete_playlist(&mut self, id: PlaylistId, progress: &Progress, now: ClockSample) -> Result<Removal, PlaylistError>`
  - `remove_entry` keeps its signature; `Removal::stop_playback` is true only for an owned entry

- [ ] **Step 1: Write the failing tests** (append)

```rust
#[test]
fn removing_a_cursor_nothing_adopted_only_clears_it() {
    // B's cursor exists because B played earlier; A plays now.
    let mut two = two();
    adopt(&mut two.session, two.in_b[0], "b1", 1);
    let playing = adopt(&mut two.session, two.in_a[0], "a1", 2);
    assert_eq!(two.session.state().playing(), two.a);

    let removal = two
        .session
        .remove_entry(two.in_b[0], &progress(2, "a1", Some(playing)), FakeClock::new().sample())
        .expect("queued");

    assert!(!removal.stop_playback, "an inactive playlist's cursor is not ownership (P3)");
    assert_eq!(two.session.adopted().map(|a| a.request), Some(playing));
    assert_eq!(two.session.state().playlist(two.b).expect("B").queue().active(), None);
}

#[test]
fn with_a_playing_and_b_loading_removing_as_entry_cannot_touch_bs_load() {
    let mut two = two();
    let playing = adopt(&mut two.session, two.in_a[0], "a1", 1);
    let pending = two
        .session
        .register_load(LoadTarget::Queue(two.in_b[0]), &media("b1"))
        .expect("registered");

    let removal = two
        .session
        .remove_entry(two.in_a[0], &progress(1, "a1", Some(playing)), FakeClock::new().sample())
        .expect("queued");
    assert!(removal.stop_playback, "the adopted entry is owned");
    assert_eq!(two.session.adopted(), None);

    two.session.observe(&loaded(pending, 2, "b1"), FakeClock::new().sample());
    assert_eq!(two.session.adopted().map(|a| a.request), Some(pending), "B's load is still valid");
    assert_eq!(two.session.state().playing(), two.b);
}

#[test]
fn clearing_a_playlist_invalidates_only_its_own_pending_loads() {
    let mut two = two();
    let for_a = two.session.register_load(LoadTarget::Queue(two.in_a[0]), &media("a1")).expect("registered");
    let for_b = two.session.register_load(LoadTarget::Queue(two.in_b[0]), &media("b1")).expect("registered");

    let removal = two
        .session
        .clear_playlist(two.b, &progress(0, "a1", None), FakeClock::new().sample())
        .expect("B exists");
    assert!(!removal.stop_playback);

    two.session.observe(&loaded(for_b, 1, "b1"), FakeClock::new().sample());
    assert_eq!(two.session.adopted(), None, "a load into the cleared playlist is never adopted");
    two.session.observe(&loaded(for_a, 2, "a1"), FakeClock::new().sample());
    assert_eq!(two.session.adopted().map(|a| a.request), Some(for_a));
}

#[test]
fn deleting_the_owning_playlist_releases_and_moves_playing() {
    let mut two = two();
    let playing = adopt(&mut two.session, two.in_a[0], "a1", 1);
    let removal = two
        .session
        .delete_playlist(two.a, &progress(1, "a1", Some(playing)), FakeClock::new().sample())
        .expect("another exists");
    assert!(removal.stop_playback);
    assert_eq!(two.session.adopted(), None);
    assert_eq!(two.session.state().playing(), two.b);
    assert!(two.session.state().entry_for(&media("a1")).is_some(), "the outgoing checkpoint was captured");
}

#[test]
fn deleting_another_playlist_leaves_playback_alone() {
    let mut two = two();
    let playing = adopt(&mut two.session, two.in_a[0], "a1", 1);
    let removal = two
        .session
        .delete_playlist(two.b, &progress(1, "a1", Some(playing)), FakeClock::new().sample())
        .expect("another exists");
    assert!(!removal.stop_playback);
    assert_eq!(two.session.adopted().map(|a| a.request), Some(playing));
    assert_eq!(two.session.state().playing(), two.a);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_session_playlists`
Expected: compile error — `clear_playlist`, `delete_playlist` not found.

- [ ] **Step 3: Implement**

```rust
    /// Whether the adopted load is for `id` (M8 §5). A cursor is only a
    /// remembered entry; this is what ownership means.
    pub fn owns_entry(&self, id: QueueEntryId) -> bool {
        self.adopted.is_some_and(|adopted| adopted.target == LoadTarget::Queue(id))
    }

    pub fn owns_playlist(&self, id: PlaylistId) -> bool {
        match self.adopted.map(|adopted| adopted.target) {
            Some(LoadTarget::Queue(entry)) => self.state.owner_of(entry) == Some(id),
            _ => false,
        }
    }

    pub fn remove_entry(
        &mut self,
        id: QueueEntryId,
        progress: &Progress,
        now: ClockSample,
    ) -> Result<Removal, QueueError> {
        if self.state.find_entry(id).is_none() {
            return Err(QueueError::UnknownEntry(id));
        }
        self.invalidate_pending(|target| target == LoadTarget::Queue(id));
        let owned = self.owns_entry(id);
        if owned {
            self.release_active(progress, now);
        }
        // `Queue::remove` clears the cursor itself when `id` was the cursor.
        let removed = self
            .owner_queue_mut(id)
            .ok_or(QueueError::UnknownEntry(id))?
            .remove(id)?;
        Ok(Removal {
            action: self.submit(Urgency::Forced),
            stop_playback: owned,
            selection: removed.selection,
        })
    }

    /// Invalidates the pending loads into `id`, before its entries go, and
    /// releases playback only if `id` owns it (M8 §5).
    fn vacate(&mut self, id: PlaylistId, progress: &Progress, now: ClockSample) -> bool {
        let entries: Vec<QueueEntryId> = self
            .state
            .playlist(id)
            .map(|playlist| playlist.queue().entries().iter().map(|e| e.id()).collect())
            .unwrap_or_default();
        self.invalidate_pending(|target| {
            matches!(target, LoadTarget::Queue(entry) if entries.contains(&entry))
        });
        let owning = self.owns_playlist(id);
        if owning {
            self.release_active(progress, now);
        }
        owning
    }

    pub fn clear_playlist(
        &mut self,
        id: PlaylistId,
        progress: &Progress,
        now: ClockSample,
    ) -> Result<Removal, PlaylistError> {
        if self.state.playlist(id).is_none() {
            return Err(PlaylistError::Unknown(id));
        }
        let owning = self.vacate(id, progress, now);
        if let Some(playlist) = self.state.playlist_mut(id) {
            playlist.queue_mut().clear();
        }
        Ok(Removal { action: self.submit(Urgency::Forced), stop_playback: owning, selection: None })
    }

    pub fn delete_playlist(
        &mut self,
        id: PlaylistId,
        progress: &Progress,
        now: ClockSample,
    ) -> Result<Removal, PlaylistError> {
        if self.state.playlist(id).is_none() {
            return Err(PlaylistError::Unknown(id));
        }
        if self.state.playlists().len() == 1 {
            return Err(PlaylistError::LastPlaylist);
        }
        let owning = self.vacate(id, progress, now);
        self.state.remove_playlist(id)?;
        Ok(Removal { action: self.submit(Urgency::Forced), stop_playback: owning, selection: None })
    }
```

Delete `clear_queue`. Update `release_active`'s doc comment: "called only for an owned entry or an owning playlist — the ownership check lives in its callers". The `LastPlaylist` check precedes `vacate` so a refused delete releases nothing.

- [ ] **Step 4: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass after the `m5_session_queue.rs` adjustments named under Files.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "fix(session): releasing playback follows adoption, not a cursor; invalidation is scoped to one playlist"
```

---

### Task 7: `Session` — adoption sets `playing`, one traversal policy, shuffle toggle

**Files:**
- Modify: `src/session.rs` (`adopt_loaded` :938, the `EndOfTrack` arm :901)
- Test: `tests/m8_session_playlists.rs` (append)

**Interfaces:**
- Produces:
  - `Session::set_shuffle(&mut self, id: PlaylistId, seed: Option<u64>) -> Result<Action, PlaylistError>` — `Some(seed)` turns shuffle on with `first` = that playlist's own cursor; `None` turns it off. The seed is the caller's (Task 10 draws it from `getrandom`), which keeps `Session` deterministic.
  - Adoption of `LoadTarget::Queue(id)` sets `playing = owner(id)` and that playlist's cursor; every other cursor is left alone.
  - `Advance` comes from `Playlist::neighbor` on the adopted entry's owner.

- [ ] **Step 1: Write the failing tests** (append)

```rust
use tenuto::playlist::Shuffle;

fn end(rev: u64) -> PlaybackEvent {
    PlaybackEvent::EndOfTrack {
        session_rev: rev,
        position: Duration::from_millis(500),
        provenance: PositionProvenance::Established,
    }
}

#[test]
fn playing_changes_at_adoption_and_the_old_playlist_keeps_its_cursor() {
    let mut two = two();
    adopt(&mut two.session, two.in_a[1], "a2", 1);
    let pending = two.session.register_load(LoadTarget::Queue(two.in_b[0]), &media("b1")).expect("registered");
    assert_eq!(two.session.state().playing(), two.a, "a request alone changes nothing (P4)");

    two.session.observe(&loaded(pending, 2, "b1"), FakeClock::new().sample());
    let state = two.session.state();
    assert_eq!(state.playing(), two.b);
    assert_eq!(state.queue().active(), Some(two.in_b[0]));
    assert_eq!(state.playlist(two.a).expect("A").queue().active(), Some(two.in_a[1]));
}

#[test]
fn a_failed_cross_playlist_load_changes_neither_playing_nor_any_cursor() {
    let mut two = two();
    adopt(&mut two.session, two.in_a[0], "a1", 1);
    let pending = two.session.register_load(LoadTarget::Queue(two.in_b[0]), &media("b1")).expect("registered");
    two.session.retract_load(pending);
    let state = two.session.state();
    assert_eq!(state.playing(), two.a);
    assert_eq!(state.queue().active(), Some(two.in_a[0]));
    assert_eq!(state.playlist(two.b).expect("B").queue().active(), None);
}

#[test]
fn automatic_advance_follows_the_same_shuffled_order_as_neighbor() {
    let mut two = two();
    let (more, _) = two.session.enqueue(two.a, vec![entry("a3"), entry("a4"), entry("a5")]).expect("fits");
    let all = [two.in_a[0], two.in_a[1], more[0], more[1], more[2]];
    adopt(&mut two.session, all[2], "a3", 1);
    two.session.set_shuffle(two.a, Some(42)).expect("A exists");

    let playlist = two.session.state().playlist(two.a).expect("A").clone();
    assert_eq!(playlist.shuffle(), Some(Shuffle { seed: 42, first: Some(all[2]) }));
    let expected = playlist.neighbor(all[2], Direction::Down).expect("the rest lies ahead of `first`");
    assert_ne!(expected, all[3], "the fixture must actually differ from list order");

    two.session.observe(&end(1), FakeClock::new().sample());
    assert_eq!(two.session.take_advance(), Some(Advance::Next(expected)));
}

#[test]
fn shuffle_on_an_inactive_tab_pins_its_own_cursor_or_nothing() {
    let mut two = two();
    adopt(&mut two.session, two.in_a[0], "a1", 1);
    two.session.set_shuffle(two.b, Some(7)).expect("B exists");
    assert_eq!(
        two.session.state().playlist(two.b).expect("B").shuffle(),
        Some(Shuffle { seed: 7, first: None }),
        "never another playlist's track"
    );
    two.session.set_shuffle(two.b, None).expect("B exists");
    assert_eq!(two.session.state().playlist(two.b).expect("B").shuffle(), None);
    assert_eq!(two.session.adopted().map(|a| a.target), Some(LoadTarget::Queue(two.in_a[0])), "no release");
}
```

If seed 42 happens to put `a4` right after the pinned `a3` for this fixture's IDs, change the seed until `assert_ne!` holds — the assertion exists to keep the test honest.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_session_playlists`
Expected: compile error — `set_shuffle` not found.

- [ ] **Step 3: Implement**

In `adopt_loaded`, replace the `previous_active` / `set_active` block:

```rust
        let previous = (self.state.playing(), self.state.queue().active());
        let switched_media = self.on_loaded(media, position, disposition, capabilities, now);
        match target {
            LoadTarget::Queue(id) => {
                // Validated against its owner a moment ago in `observe`.
                if let Some(owner) = self.state.owner_of(id) {
                    self.state.set_playing(owner);
                }
                let _ = self.state.queue_mut().set_active(Some(id));
                self.absorb_load_metadata(id, metadata);
            }
            // A legacy load belongs to no playlist: it clears the playing
            // playlist's cursor, as it cleared the one queue's before M8.
            LoadTarget::Legacy => {
                let _ = self.state.queue_mut().set_active(None);
            }
        }
        self.adopted = Some(AdoptedLoad { request, target });
        self.adopted_rev_floor = self.session_rev;
        let now_at = (self.state.playing(), self.state.queue().active());
        if switched_media || previous != now_at {
            self.submit(Urgency::Forced)
        } else {
            Action::None
        }
```

In the `EndOfTrack` arm:

```rust
                if let LoadTarget::Queue(id) = adopted.target {
                    let next = self
                        .state
                        .owner_of(id)
                        .and_then(|owner| self.state.playlist(owner))
                        .and_then(|playlist| playlist.neighbor(id, Direction::Down));
                    self.advance = Some(next.map_or(Advance::EndOfQueue, Advance::Next));
                }
```

And:

```rust
    /// `Some(seed)` turns shuffle on, pinning the playlist's *own* cursor
    /// first; `None` turns it off. No engine command either way, so a
    /// playing track keeps playing (M8 §7).
    pub fn set_shuffle(&mut self, id: PlaylistId, seed: Option<u64>) -> Result<Action, PlaylistError> {
        let playlist = self.state.playlist_mut(id).ok_or(PlaylistError::Unknown(id))?;
        let first = playlist.queue().active();
        playlist.set_shuffle(seed.map(|seed| Shuffle { seed, first }));
        Ok(self.submit(Urgency::Ordinary))
    }
```

`grep -n 'neighbor(' src` must now show only `src/playlist.rs`, `src/queue.rs` (the definition and its tests) and `src/application/transport.rs` (Task 9 removes that one).

- [ ] **Step 4: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(session): adoption sets the playing playlist; advance and shuffle share Playlist::neighbor"
```

---

### Task 8: Start from zero for local files and URLs

**Files:**
- Modify: `src/session.rs` (`resume_intent` :1421, `resume_intent_for` :1457), `src/app.rs:637` and the other `resume_intent_for(` call sites in `src/app.rs` tests
- Test: `tests/m8_resume.rs`

**Interfaces:**
- Produces: `pub fn resume_intent_for(media: &MediaId, entry: Option<&PersistedCheckpoint>) -> Option<ResumeIntent>` — `None` for `MediaId::LocalFile` and `MediaId::RemoteUrl` whatever the checkpoint says; unchanged for `MediaId::PodcastEpisode`.

- [ ] **Step 1: Write the failing test** — `tests/m8_resume.rs`

```rust
mod support;

use std::time::Duration;

use support::media;
use tenuto::media::id::{EpisodeKey, FeedId, MediaId, NormalizedUrl};
use tenuto::persistence::model::PersistedState;
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::playback::command::ResumeIntent;
use tenuto::session::Session;
use time::OffsetDateTime;

/// A state whose `media` has an established 90-second checkpoint.
fn remembered(media: &MediaId) -> Session {
    let mut state = PersistedState::default();
    state.record(
        &PlaybackCheckpoint {
            media: media.clone(),
            position: Duration::from_secs(90),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        },
        false,
    );
    Session::new(state)
}

fn remote_media(url: &str) -> MediaId {
    MediaId::RemoteUrl(NormalizedUrl::parse(url).unwrap_or_else(|error| panic!("a literal URL: {error}")))
}

fn episode_media() -> MediaId {
    MediaId::PodcastEpisode {
        feed: FeedId::new("0123456789abcdef0123456789abcdef".into())
            .unwrap_or_else(|error| panic!("a literal feed ID: {error}")),
        episode: EpisodeKey::resolve(Some("guid-1"), None, None)
            .unwrap_or_else(|error| panic!("a literal key: {error}")),
    }
}

#[test]
fn a_local_file_starts_from_zero_even_with_a_checkpoint() {
    let session = remembered(&media("song"));
    assert_eq!(session.resume_intent(&media("song")), ResumeIntent::StartAt(Duration::ZERO));
    assert!(session.state().entry_for(&media("song")).is_some(), "the checkpoint is still kept");
}

#[test]
fn a_plain_url_starts_from_zero_too() {
    let url = remote_media("https://example.test/track.mp3");
    assert_eq!(remembered(&url).resume_intent(&url), ResumeIntent::StartAt(Duration::ZERO));
}

#[test]
fn a_podcast_episode_still_resumes() {
    let episode = episode_media();
    assert_ne!(remembered(&episode).resume_intent(&episode), ResumeIntent::StartAt(Duration::ZERO));
}
```

The constructors mirror `tests/m5_podcast_resolve.rs:53` (`FeedId::new`, `EpisodeKey::resolve`) and `queue_codec`'s `NormalizedUrl::parse`; `PersistedState::record(&PlaybackCheckpoint, completed: bool)` is at `src/persistence/model.rs:250`. If `time` is not already a dev-visible dependency of integration tests, it is — `tests/*.rs` use `time::OffsetDateTime` today.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_resume`
Expected: FAIL — `a_local_file_starts_from_zero…` gets a `Candidate`, not `StartAt(0)`.

- [ ] **Step 3: Implement**

```rust
pub fn resume_intent_for(
    media: &MediaId,
    entry: Option<&PersistedCheckpoint>,
) -> Option<ResumeIntent> {
    // M8 §7: music starts from the beginning. A fresh-load policy only —
    // pause and Stop → Play are engine commands and never come here, so the
    // position contract holds. The checkpoint is still written, which keeps
    // the played marker working.
    if matches!(media, MediaId::LocalFile(_) | MediaId::RemoteUrl(_)) {
        return None;
    }
    // … the existing body, unchanged …
}
```

`Session::resume_intent` passes `media` through. In `src/app.rs:637`: `resume_intent_for(media, state.entry_for(media))`; when it returns `None` the existing caller already falls back to a start of zero — confirm by reading the twenty lines after :637. The unit tests in `src/app.rs` (:1226–:1295) that exercise estimated resume through a local-file `MediaId` must switch their fixture to a podcast episode; a test that asserted "a local file resumes" is now wrong and is rewritten to assert zero.

- [ ] **Step 4: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass. `tests/estimated_resume.rs`, `tests/resume_contract.rs` and `tests/http_resume.rs` are the likeliest to need the same fixture switch; where a test is *about the engine honoring a `ResumeIntent` it was handed*, it does not go through `resume_intent_for` and needs no change.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(resume): local files and plain URLs start from zero; podcasts still resume"
```

---

### Task 9: Transport — two playlists, a validated retry, Enter-only selection

**Files:**
- Modify: `src/application/transport.rs` (`TransportSituation` :42, `selection` :69, `still_queued` :78, `anchor` :99, `navigate` :112, `decide` :138–222)
- Modify: `src/application/runtime.rs` (`decide` :650) — minimal, so the crate compiles; Task 10 finishes it
- Modify: `tests/m5_transport_rules.rs` — port to the new situation; rewrite the cases §7 deliberately changes
- Test: `tests/m8_transport.rs`

**Interfaces:**
- Consumes: `Playlist::{queue, neighbor, first_in_order}`.
- Produces:

```rust
pub struct TransportSituation<'a> {
    /// `playing` — except during `Loading`, the owner of `last_requested`
    /// while that entry is still queued (M8 P5). The caller decides.
    pub navigation: &'a Playlist,
    pub viewed: &'a Playlist,
    /// The selected row of `viewed`. Only Enter reads it.
    pub selected: Option<QueueEntryId>,
    pub phase: PlaybackPhase,
    /// `last_requested`, already validated by the caller through the owner
    /// lookup across *all* playlists: neither playlist here could do it.
    pub retry: Option<QueueEntryId>,
    pub live: bool,
}
```

- [ ] **Step 1: Write the failing tests** — `tests/m8_transport.rs`

```rust
mod support;

use support::media;
use tenuto::application::transport::*;
use tenuto::media::id::MediaId;
use tenuto::playlist::{Playlist, PlaylistId, Shuffle};
use tenuto::queue::{DisplayMetadata, IdAllocator, NewQueueEntry, Queue, QueueEntryId, QueueSource};

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else {
        unreachable!()
    };
    NewQueueEntry::new(media(name), QueueSource::LocalFile(path), DisplayMetadata::default())
        .unwrap_or_else(|error| panic!("a literal entry must be valid: {error}"))
}

/// A playlist of `names`, its cursor on `cursor` (an index), IDs from `ids`.
fn playlist(
    raw_id: u64,
    names: &[&str],
    cursor: Option<usize>,
    shuffle: Option<Shuffle>,
    ids: &mut IdAllocator,
) -> (Playlist, Vec<QueueEntryId>) {
    let mut queue = Queue::default();
    let entries = queue
        .enqueue(names.iter().map(|name| entry(name)).collect(), ids)
        .unwrap_or_else(|error| panic!("fits: {error}"));
    let queue = Queue::from_parts_for_tests(queue.entries().to_vec(), cursor.map(|index| entries[index]));
    (Playlist::from_parts(PlaylistId::from_raw_for_tests(raw_id), "P".into(), shuffle, queue), entries)
}

const ALL_PHASES: [PlaybackPhase; 8] = [
    PlaybackPhase::Unloaded, PlaybackPhase::Loading, PlaybackPhase::LoadFailed, PlaybackPhase::Playing,
    PlaybackPhase::Reconnecting, PlaybackPhase::Paused, PlaybackPhase::Stopped, PlaybackPhase::Ended,
];

#[test]
fn enter_plays_the_viewed_selection_in_every_phase_even_over_an_empty_playing_playlist() {
    let mut ids = IdAllocator::default();
    let (empty, _) = playlist(1, &[], None, None, &mut ids);
    let (viewed, in_viewed) = playlist(2, &["x", "y"], None, None, &mut ids);
    for phase in ALL_PHASES {
        let decision = decide(
            TransportInput::Enter,
            &TransportSituation { navigation: &empty, viewed: &viewed, selected: Some(in_viewed[1]), phase, retry: None, live: false },
        );
        assert_eq!(decision, TransportDecision::Load(in_viewed[1]), "{phase:?}");
    }
}

#[test]
fn space_and_play_never_load_the_viewed_selection() {
    let mut ids = IdAllocator::default();
    let (playing, in_playing) = playlist(1, &["a", "b"], None, None, &mut ids);
    let (viewed, in_viewed) = playlist(2, &["x"], None, None, &mut ids);
    for phase in [PlaybackPhase::Unloaded, PlaybackPhase::Ended, PlaybackPhase::LoadFailed] {
        for input in [TransportInput::Space, TransportInput::Play] {
            let decision = decide(
                input,
                &TransportSituation { navigation: &playing, viewed: &viewed, selected: Some(in_viewed[0]), phase, retry: None, live: false },
            );
            assert_eq!(decision, TransportDecision::Load(in_playing[0]), "{phase:?} {input:?}: first in playback order");
        }
    }
}

#[test]
fn the_cursor_outranks_the_first_entry_and_shuffle_decides_what_first_means() {
    let mut ids = IdAllocator::default();
    let (with_cursor, entries) = playlist(1, &["a", "b", "c"], Some(1), None, &mut ids);
    let situation = |navigation| TransportSituation {
        navigation, viewed: navigation, selected: None, phase: PlaybackPhase::Unloaded, retry: None, live: false,
    };
    assert_eq!(decide(TransportInput::Play, &situation(&with_cursor)), TransportDecision::Load(entries[1]));

    let mut ids = IdAllocator::default();
    let (shuffled, entries) = playlist(1, &["a", "b", "c", "d", "e"], None, Some(Shuffle { seed: 42, first: None }), &mut ids);
    assert_eq!(
        decide(TransportInput::Play, &situation(&shuffled)),
        TransportDecision::Load(entries[4]),
        "seed 42 orders IDs 1..=5 as 5 1 4 3 2"
    );
}

#[test]
fn a_valid_retry_outranks_the_empty_check_whatever_is_viewed() {
    // A is playing and empty; the failed request was in B; the view is on C.
    let mut ids = IdAllocator::default();
    let (a, _) = playlist(1, &[], None, None, &mut ids);
    let (_b, in_b) = playlist(2, &["b1"], None, None, &mut ids);
    let (c, in_c) = playlist(3, &["c1"], None, None, &mut ids);
    for input in [TransportInput::Space, TransportInput::Play] {
        let decision = decide(
            input,
            &TransportSituation { navigation: &a, viewed: &c, selected: Some(in_c[0]), phase: PlaybackPhase::LoadFailed, retry: Some(in_b[0]), live: false },
        );
        assert_eq!(decision, TransportDecision::Load(in_b[0]));
    }
    let nothing_to_retry = decide(
        TransportInput::Play,
        &TransportSituation { navigation: &a, viewed: &c, selected: Some(in_c[0]), phase: PlaybackPhase::LoadFailed, retry: None, live: false },
    );
    assert_eq!(nothing_to_retry, TransportDecision::Notice(QUEUE_EMPTY));
}

#[test]
fn next_and_previous_step_from_the_cursor_in_playback_order_and_stop_at_the_ends() {
    let mut ids = IdAllocator::default();
    let (shuffled, e) = playlist(1, &["a", "b", "c", "d", "e"], Some(0), Some(Shuffle { seed: 42, first: None }), &mut ids);
    let at = |navigation, input| {
        decide(input, &TransportSituation { navigation, viewed: navigation, selected: None, phase: PlaybackPhase::Playing, retry: None, live: false })
    };
    // Order 5 1 4 3 2; the cursor is ID 1.
    assert_eq!(at(&shuffled, TransportInput::Next), TransportDecision::Load(e[3]));
    assert_eq!(at(&shuffled, TransportInput::Previous), TransportDecision::Load(e[4]));

    let mut ids = IdAllocator::default();
    let (at_end, _) = playlist(1, &["a", "b"], Some(1), None, &mut ids);
    assert_eq!(at(&at_end, TransportInput::Next), TransportDecision::Nothing, "a boundary does not disturb playback");
    let mut ids = IdAllocator::default();
    let (no_cursor, _) = playlist(1, &["a", "b"], None, None, &mut ids);
    assert_eq!(at(&no_cursor, TransportInput::Next), TransportDecision::Nothing, "no anchor, no step");
}

#[test]
fn during_loading_navigation_anchors_on_the_retry_in_its_own_playlist() {
    let mut ids = IdAllocator::default();
    let (_a, _) = playlist(1, &["a1", "a2"], Some(0), None, &mut ids);
    let (b, in_b) = playlist(2, &["b1", "b2"], None, None, &mut ids);
    // The caller made B the navigation playlist because last_requested lives there.
    let decision = decide(
        TransportInput::Next,
        &TransportSituation { navigation: &b, viewed: &b, selected: None, phase: PlaybackPhase::Loading, retry: Some(in_b[0]), live: false },
    );
    assert_eq!(decision, TransportDecision::Load(in_b[1]));
}

#[test]
fn engine_phases_keep_their_engine_commands_over_an_emptied_playlist() {
    let mut ids = IdAllocator::default();
    let (empty, _) = playlist(1, &[], None, None, &mut ids);
    for phase in [PlaybackPhase::Playing, PlaybackPhase::Paused, PlaybackPhase::Reconnecting] {
        let situation = TransportSituation { navigation: &empty, viewed: &empty, selected: None, phase, retry: None, live: false };
        assert_eq!(decide(TransportInput::Space, &situation), TransportDecision::TogglePause);
        assert_eq!(decide(TransportInput::Play, &situation), TransportDecision::Play);
        assert_eq!(decide(TransportInput::Enter, &situation), TransportDecision::Notice(QUEUE_EMPTY));
    }
}
```

Add to `Queue` in `src/queue.rs`: `#[doc(hidden)] pub fn from_parts_for_tests(entries: Vec<QueueEntry>, active: Option<QueueEntryId>) -> Self { Self::from_parts(entries, active) }`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_transport`
Expected: compile error — `TransportSituation` has no field `navigation`.

- [ ] **Step 3: Implement** — replace `selection`, `still_queued`, `anchor`, `navigate` and `decide`'s body after the live-seek guard (which stays, unchanged, at the top):

```rust
/// The selected row of the *viewed* playlist while it is still there, else
/// that playlist's first row. Enter is the only command that reads it (M8 §7).
fn selection(situation: &TransportSituation<'_>) -> Option<QueueEntryId> {
    let queue = situation.viewed.queue();
    situation
        .selected
        .filter(|id| queue.get(*id).is_some())
        .or_else(|| queue.first())
}

/// Where Space and `p` start when nothing is held: the navigation playlist's
/// cursor, else its first entry in playback order. Never the viewed tab.
fn start(situation: &TransportSituation<'_>) -> Option<QueueEntryId> {
    let playlist = situation.navigation;
    playlist.queue().active().or_else(|| playlist.first_in_order())
}

/// The entry Previous/Next steps from: the retry while loading, else the
/// cursor. With no anchor there is nothing to step away from.
fn navigate(situation: &TransportSituation<'_>, direction: Direction) -> TransportDecision {
    let cursor = situation.navigation.queue().active();
    let anchor = match situation.phase {
        PlaybackPhase::Loading => situation.retry.or(cursor),
        _ => cursor,
    };
    anchor
        .and_then(|id| situation.navigation.neighbor(id, direction))
        .map_or(TransportDecision::Nothing, TransportDecision::Load)
}
```

`decide`, after the live guard:

```rust
    use PlaybackPhase::{Ended, LoadFailed, Loading, Paused, Playing, Reconnecting, Stopped, Unloaded};
    use TransportInput::{Enter, Home, Next, Play, Previous, SeekBy, SeekTo, Space};

    // Enter follows the view, in every phase, and its emptiness is the
    // viewed playlist's: an empty playing playlist never blocks it.
    if input == Enter {
        return load_or_notice(selection(situation));
    }
    // A valid retry outranks the empty check: the failed request may live in
    // a playlist that is neither playing nor viewed.
    if situation.phase == LoadFailed
        && matches!(input, Space | Play)
        && let Some(id) = situation.retry
    {
        return TransportDecision::Load(id);
    }
    if situation.navigation.queue().is_empty() {
        return match situation.phase {
            Playing | Reconnecting | Paused => decide_engine_with_empty_queue(input),
            _ => match input {
                Previous | Next => TransportDecision::Nothing,
                _ => TransportDecision::Notice(QUEUE_EMPTY),
            },
        };
    }

    match (situation.phase, input) {
        (_, Enter) => load_or_notice(selection(situation)), // unreachable: returned above
        (_, Previous) => navigate(situation, Direction::Up),
        (_, Next) => navigate(situation, Direction::Down),

        (Unloaded | LoadFailed | Ended, Space | Play) => load_or_notice(start(situation)),
        (Unloaded | LoadFailed, Home | SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(PLAY_BEFORE_SEEK),
        (Ended, Home) => TransportDecision::Restart,
        (Ended, SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(TRACK_ENDED),

        (Loading, Space | Play) => TransportDecision::Nothing,
        (Loading, Home | SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(STILL_LOADING),

        (Playing | Reconnecting | Paused | Stopped, Space) => TransportDecision::TogglePause,
        (Playing | Reconnecting | Paused | Stopped, Play) => TransportDecision::Play,
        (Playing | Reconnecting | Paused | Stopped, Home) => TransportDecision::Restart,
        (Playing | Reconnecting | Paused | Stopped, SeekBy(n)) => TransportDecision::SeekBy(n),
        (Playing | Reconnecting | Paused | Stopped, SeekTo(d)) => TransportDecision::SeekTo(d),
    }
```

`decide_engine_with_empty_queue` keeps its `Enter` arm for exhaustiveness. Replace the `(_, Enter)` arm with whatever keeps the match exhaustive without a lint — if clippy flags it as unreachable, fold Enter into the match and delete the early return instead; behavior must be the one the tests pin.

Minimal `runtime::decide` so the crate builds (Task 10 adds `viewed` and the Loading exception):

```rust
        let state = self.session.state();
        let playing = state.playing_playlist();
        decide(input, &TransportSituation {
            navigation: playing,
            viewed: playing,
            selected,
            phase: self.phase(),
            retry: self.last_requested.filter(|id| state.find_entry(*id).is_some()),
            live: self.indefinite(),
        })
```

- [ ] **Step 4: Port `tests/m5_transport_rules.rs`**

Its `run` helper wraps the queue: `let playlist = Playlist::from_parts(PlaylistId::from_raw_for_tests(1), "P".into(), None, queue.clone());` and passes it as both `navigation` and `viewed`, with `retry: last.filter(|id| queue.get(*id).is_some())`. Then, per §7's deliberate narrowing, rewrite exactly these expectations and no others:
- Space/`p` in `Unloaded`/`Ended`/`LoadFailed` with no cursor: `Load(first row)`, not `Load(selected)`.
- Previous/Next with no cursor: `Nothing`, not a step from the selected row.
Everything about Enter, engine phases, seek notices and live media must pass untouched; a failure there is a bug in Step 3, not a test to edit.

- [ ] **Step 5: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass. `tests/m5_runtime.rs::space_before_loading_loads_the_restored_active_entry` must still pass unchanged — a restored cursor still wins.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(transport): navigation and viewed playlists, a validated retry, and Enter as the only reader of the selection"
```

---

### Task 10: Runtime — the viewed playlist, playlist commands, scoped side effects

**Files:**
- Modify: `src/application/runtime.rs` (`AppCommand` :127, `handle` :474, `view` :568, `decide` :650, `enqueue` :965, `remove` :991, `clear_queue` :1005, `request_enrichment` :1018)
- Modify: `src/application/view.rs` (`PlayerView` :105, `queue_rows` :121)
- Modify: `src/tui/input.rs`, `src/tui/mod.rs` — only what the changed `AppCommand` variants force (drop `selected` from four variants; `Enqueue` gains `dest`; `ClearQueue` becomes `ClearPlaylist`). Keys and overlays are Task 15.
- Modify: `tests/support/runtime.rs`, `tests/m5_runtime.rs`, `tests/m5_tui_input.rs`, `tests/m7_runtime.rs` for the same variant changes
- Test: `tests/m8_runtime.rs`

**Interfaces:**
- Consumes: Tasks 5–7 and 9.
- Produces:

```rust
pub enum AppCommand {
    PlayPause,
    Play,
    PlayEntry(QueueEntryId),
    Stop,
    SeekBy(i64),
    SeekTo(Duration),
    Restart,
    AdjustVolume(f32),
    Previous,
    Next,
    Enqueue { dest: PlaylistId, items: Vec<EnqueueItem> },
    Remove(QueueEntryId),
    Move(QueueEntryId, Direction),
    ClearPlaylist(PlaylistId),
    CreatePlaylist(String),
    RenamePlaylist(PlaylistId, String),
    DeletePlaylist(PlaylistId),
    ToggleShuffle(PlaylistId),
    ViewNext,
    ViewPrevious,
}

// application/view.rs
pub struct PlaylistTab { pub id: PlaylistId, pub name: String, pub playing: bool, pub shuffled: bool }
// PlayerView gains:  pub tabs: Vec<PlaylistTab>,  pub viewed: PlaylistId,
pub(crate) fn queue_rows(state: &PersistedState, playlist: PlaylistId) -> Vec<QueueRow>
```

  `PlayerView::rows` are the *viewed* playlist's rows; `active` and `now_playing` stay the *playing* playlist's cursor. `PlaylistTab::name` is already `displayable`.
- `PlayerRuntime::viewed(&self) -> PlaylistId`, `PlayerRuntime::rows_of(&self, playlist: PlaylistId) -> Vec<QueueRow>` (the browser's captured destination, Task 14).

- [ ] **Step 1: Write the failing tests** — `tests/m8_runtime.rs` (header: copy `tests/m5_runtime.rs` lines 1–40 — the `mod` mounts, imports and the `SHORT` constant)

```rust
fn tab_names(runtime: &PlayerRuntime) -> Vec<String> {
    runtime.view().tabs.iter().map(|tab| tab.name.clone()).collect()
}

#[test]
fn a_new_playlist_is_viewed_and_enqueue_goes_where_it_was_told() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::CreatePlaylist("Jazz".into()));
    let jazz = rig.runtime.viewed();
    assert_ne!(jazz, first);
    assert_eq!(tab_names(&rig.runtime), ["Default", "Jazz"]);

    rig.runtime.handle(AppCommand::Enqueue { dest: first, items: vec![EnqueueItem::Path(SHORT.into())] });
    assert!(rig.runtime.view().rows.is_empty(), "the view is on Jazz; the add went to Default");
    assert_eq!(rig.runtime.rows_of(first).len(), 1);
}

#[test]
fn enter_in_another_playlist_moves_playing_only_once_the_load_is_adopted() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::CreatePlaylist("Jazz".into()));
    let jazz = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue { dest: jazz, items: vec![EnqueueItem::Path(SHORT.into())] });
    let id = row_ids(&rig.runtime)[0];
    assert!(rig.runtime.view().tabs.iter().any(|tab| tab.id == first && tab.playing));

    rig.runtime.handle(AppCommand::PlayEntry(id));
    pump_until(&mut rig.runtime, "Jazz is the playing playlist", |view| {
        view.active == Some(id) && view.tabs.iter().any(|tab| tab.id == jazz && tab.playing)
    });
}

#[test]
fn deleting_the_viewed_playlist_moves_the_view_and_the_last_one_is_refused() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::CreatePlaylist("Jazz".into()));
    let jazz = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::DeletePlaylist(jazz));
    assert_eq!(rig.runtime.viewed(), first);
    rig.runtime.handle(AppCommand::DeletePlaylist(first));
    assert_eq!(tab_names(&rig.runtime), ["Default"]);
    assert!(rig.runtime.view().status.is_some_and(|status| status.contains("last playlist")));
}

#[test]
fn clearing_an_inactive_playlist_neither_stops_playback_nor_loses_anothers_enrichment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tagged = tagged_flac::tagged_flac(dir.path(), "Title", "Artist", "Album", None);
    let mut rig = rig_with_probe(PersistedState::default(), default_probe(TestHook::None));
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::CreatePlaylist("Scratch".into()));
    let scratch = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue { dest: scratch, items: vec![EnqueueItem::Path(SHORT.into())] });
    rig.runtime.handle(AppCommand::Enqueue { dest: first, items: vec![EnqueueItem::Path(tagged.clone())] });

    rig.runtime.handle(AppCommand::ClearPlaylist(scratch));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        rig.runtime.pump();
        if rig.runtime.rows_of(first).iter().any(|row| row.title.contains("Title")) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Default's pending tag probe was lost when Scratch was cleared");
}

#[test]
fn toggling_shuffle_marks_the_tab_and_keeps_the_track_playing() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue { dest: first, items: vec![EnqueueItem::Path(SHORT.into())] });
    let id = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(id));
    pump_until(&mut rig.runtime, "playing", |view| view.active == Some(id));

    rig.runtime.handle(AppCommand::ToggleShuffle(first));
    let view = rig.runtime.view();
    assert!(view.tabs[0].shuffled);
    assert_eq!(view.active, Some(id));
    rig.runtime.handle(AppCommand::ToggleShuffle(first));
    assert!(!rig.runtime.view().tabs[0].shuffled);
}
```

`tagged_flac::tagged_flac(dir, title, artist, album, None)` is the helper `tests/m5_runtime.rs:225` uses. `tests/m5_runtime.rs:237` asserts the subtitle `"Harbor · Coast"`; Task 11 changes it.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_runtime`
Expected: compile error — `AppCommand::CreatePlaylist` not found.

- [ ] **Step 3: Implement the view** (`src/application/view.rs`)

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistTab {
    pub id: PlaylistId,
    /// Already sanitized for the terminal.
    pub name: String,
    pub playing: bool,
    pub shuffled: bool,
}

pub(crate) fn playlist_tabs(state: &PersistedState) -> Vec<PlaylistTab> {
    state
        .playlists()
        .iter()
        .map(|playlist| PlaylistTab {
            id: playlist.id(),
            name: displayable(playlist.name()),
            playing: playlist.id() == state.playing(),
            shuffled: playlist.shuffle().is_some(),
        })
        .collect()
}

pub(crate) fn queue_rows(state: &PersistedState, playlist: PlaylistId) -> Vec<QueueRow> {
    let Some(playlist) = state.playlist(playlist) else {
        return Vec::new();
    };
    playlist.queue().entries().iter().map(|entry| QueueRow { /* fields exactly as today */ }).collect()
}
```

- [ ] **Step 4: Implement the runtime**

Field `viewed: PlaylistId`, initialised in `new` from `parts.session.state().playing()`. Accessors `viewed()` and `rows_of(id)` (`queue_rows(self.session.state(), id)`).

`view()`: `rows: queue_rows(state, self.viewed)`, `tabs: playlist_tabs(state)`, `viewed: self.viewed`; `active`/`now_playing` unchanged (they read `state.queue()`, the playing playlist).

`decide()`:

```rust
        let state = self.session.state();
        let phase = self.phase();
        let retry = self.last_requested.filter(|id| state.find_entry(*id).is_some());
        // M8 P5: while loading, navigation follows the request's own playlist.
        let navigation = (phase == PlaybackPhase::Loading)
            .then_some(retry)
            .flatten()
            .and_then(|id| state.owner_of(id))
            .and_then(|owner| state.playlist(owner))
            .unwrap_or_else(|| state.playing_playlist());
        let viewed = state.playlist(self.viewed).unwrap_or(navigation);
        decide(input, &TransportSituation { navigation, viewed, selected, phase, retry, live: self.indefinite() })
```

`handle()` — new and changed arms:

```rust
            AppCommand::PlayPause => self.transport(TransportInput::Space, None),
            AppCommand::Play => self.transport(TransportInput::Play, None),
            AppCommand::Previous => self.transport(TransportInput::Previous, None),
            AppCommand::Next => self.transport(TransportInput::Next, None),
            AppCommand::Enqueue { dest, items } => self.enqueue(dest, items),
            AppCommand::ClearPlaylist(id) => self.vacate(id, false),
            AppCommand::DeletePlaylist(id) => self.vacate(id, true),
            AppCommand::CreatePlaylist(name) => match self.session.create_playlist(&name) {
                Ok((id, action)) => {
                    self.submit(action);
                    self.viewed = id;
                }
                Err(error) => self.status = Some(error.to_string()),
            },
            AppCommand::RenamePlaylist(id, name) => match self.session.rename_playlist(id, &name) {
                Ok(action) => self.submit(action),
                Err(error) => self.status = Some(error.to_string()),
            },
            AppCommand::ToggleShuffle(id) => self.toggle_shuffle(id),
            AppCommand::ViewNext => self.step_view(1),
            AppCommand::ViewPrevious => self.step_view(-1),
```

```rust
    fn step_view(&mut self, step: isize) {
        let playlists = self.session.state().playlists();
        let Some(index) = playlists.iter().position(|playlist| playlist.id() == self.viewed) else {
            return;
        };
        let len = playlists.len() as isize;
        let next = (index as isize + step).rem_euclid(len) as usize;
        self.viewed = playlists[next].id();
    }

    /// Clears or deletes one playlist. Side effects are scoped (M8 §5): a
    /// pending seek is cancelled only if the playlist owns playback, and the
    /// metadata workers — which can only cancel everything — are re-asked
    /// for every entry that still lacks tags.
    fn vacate(&mut self, id: PlaylistId, delete: bool) {
        if self.session.owns_playlist(id) {
            self.router.cancel();
        }
        let index = self.session.state().playlists().iter().position(|playlist| playlist.id() == id);
        let progress = self.latest_progress();
        let now = self.clock.sample();
        let result = if delete {
            self.session.delete_playlist(id, &progress, now)
        } else {
            self.session.clear_playlist(id, &progress, now)
        };
        match result {
            Ok(removal) => {
                if let Some(workers) = &self.metadata {
                    workers.cancel_all();
                }
                self.apply_removal(removal);
                let remaining: Vec<QueueEntryId> = self
                    .session
                    .state()
                    .playlists()
                    .iter()
                    .flat_map(|playlist| playlist.queue().entries())
                    .map(|entry| entry.id())
                    .collect();
                self.request_enrichment(&remaining);
                if delete && self.viewed == id {
                    let playlists = self.session.state().playlists();
                    let next = index.unwrap_or(0).min(playlists.len().saturating_sub(1));
                    self.viewed = playlists[next].id();
                }
            }
            Err(error) => self.status = Some(error.to_string()),
        }
    }

    fn toggle_shuffle(&mut self, id: PlaylistId) {
        let on = self.session.state().playlist(id).is_some_and(|playlist| playlist.shuffle().is_some());
        let seed = if on {
            None
        } else {
            let mut bytes = [0u8; 8];
            if getrandom::fill(&mut bytes).is_err() {
                self.status = Some("Shuffle unavailable: no randomness source".to_owned());
                return;
            }
            Some(u64::from_le_bytes(bytes))
        };
        match self.session.set_shuffle(id, seed) {
            Ok(action) => self.submit(action),
            Err(error) => self.status = Some(error.to_string()),
        }
    }
```

`enqueue(dest, items)`: pass `dest` to `self.session.enqueue`; the capacity message becomes `format!("Playlists are full ({MAX_PLAYLIST_ENTRIES} entries in total)")`. `remove`: the guard becomes `if self.session.owns_entry(id) { self.router.cancel(); }`. `request_enrichment`: look entries up with `state.find_entry(*id)` instead of `queue.get(*id)`. Delete `clear_queue`.

- [ ] **Step 5: Make the TUI compile** (no new behavior yet)

`src/tui/input.rs`: drop `selected` from `PlayPause`/`Play`/`Previous`/`Next` (and from `transport_effect`'s parameter); `input_overlay`'s Enter emits `AppCommand::Enqueue { dest: view.viewed, items }` — thread `view` into `input_overlay`; `confirm_overlay` emits `AppCommand::ClearPlaylist(view.viewed)` for now. `src/tui/mod.rs::apply_browser_effect`: `BrowserEffect::Enqueue(items)` → `AppCommand::Enqueue { dest: front.runtime.viewed(), items }` until Task 14 captures it. Update the tests the compiler points at.

- [ ] **Step 6: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(runtime): a viewed playlist, playlist commands, and side effects scoped to the playlist they touch"
```

---

### Task 11: Rows read `Artist – Title`

**Files:**
- Modify: `src/application/view.rs` (`entry_title` :138, `entry_subtitle` :150, and the unit tests below them)

**Interfaces:**
- Produces: `QueueRow::title` = `Artist – Title` (en dash, U+2013, spaces either side) when the entry is not a podcast episode and has a non-empty artist *and* a non-empty title; `QueueRow::subtitle` = the album alone. Station rows have no artist, so they are unchanged by construction.

- [ ] **Step 1: Write the failing tests** (in `view.rs`'s existing `mod tests`)

```rust
    fn local(title: Option<&str>, artist: Option<&str>, album: Option<&str>) -> QueueEntry {
        let path = AbsolutePath::new("/music/file.flac".into()).unwrap_or_else(|error| panic!("absolute: {error}"));
        let new = NewQueueEntry::new(
            MediaId::LocalFile(path.clone()),
            QueueSource::LocalFile(path),
            DisplayMetadata {
                title: title.map(str::to_owned),
                artist: artist.map(str::to_owned),
                album: album.map(str::to_owned),
                ..DisplayMetadata::default()
            },
        )
        .unwrap_or_else(|error| panic!("valid: {error}"));
        let mut queue = crate::queue::Queue::default();
        let ids = queue
            .enqueue(vec![new], &mut crate::queue::IdAllocator::default())
            .unwrap_or_else(|error| panic!("fits: {error}"));
        queue.get(ids[0]).cloned().unwrap_or_else(|| panic!("just enqueued"))
    }

    #[test]
    fn a_tagged_track_reads_artist_dash_title_with_the_album_below() {
        let entry = local(Some("So What"), Some("Miles Davis"), Some("Kind of Blue"));
        assert_eq!(entry_title(&entry), "Miles Davis – So What");
        assert_eq!(entry_subtitle(&entry).as_deref(), Some("Kind of Blue"));
    }

    #[test]
    fn without_an_artist_or_without_a_title_the_row_is_as_before() {
        assert_eq!(entry_title(&local(Some("So What"), None, None)), "So What");
        assert_eq!(entry_title(&local(Some("So What"), Some("  "), None)), "So What");
        assert_eq!(entry_title(&local(None, Some("Miles Davis"), None)), "file.flac", "no title: the file name, not 'Artist – file.flac'");
        assert_eq!(entry_subtitle(&local(None, Some("Miles Davis"), None)), None);
    }

    #[test]
    fn control_characters_in_either_tag_never_reach_the_row() {
        let entry = local(Some("So\u{1b}[31m What"), Some("Miles\u{7}"), None);
        let title = entry_title(&entry);
        assert!(!title.chars().any(char::is_control), "{title:?}");
    }
```

If `display_name` renders that path differently from `file.flac`, use what it renders — run the test once and read the failure.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib view::tests`
Expected: FAIL — `"So What"` ≠ `"Miles Davis – So What"`.

- [ ] **Step 3: Implement**

```rust
pub(crate) fn entry_title(entry: &QueueEntry) -> String {
    let display = entry.display();
    let filled = |text: &Option<String>| text.as_deref().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned);
    let title = filled(&display.title);
    displayable(&match entry.media() {
        MediaId::PodcastEpisode { .. } => episode_name(display.title.as_deref()),
        media => match (filled(&display.artist), title) {
            (Some(artist), Some(title)) => format!("{artist} – {title}"),
            (_, Some(title)) => title,
            (_, None) => display_name(media),
        },
    })
}

/// The album, escaped. The artist moved up into the title line (M8 §9).
fn entry_subtitle(entry: &QueueEntry) -> Option<String> {
    entry
        .display()
        .album
        .as_deref()
        .map(str::trim)
        .filter(|album| !album.is_empty())
        .map(displayable)
}
```

`NowPlaying` (the player pane) shows the title and the artist on separate lines, and `runtime.rs:1135` builds its title with `entry_title` today — so it would read `Artist – Title` above a second `Artist`. Keep today's behavior there: rename the *old* body to `pub(crate) fn entry_plain_title(entry: &QueueEntry) -> String` (title, else the identity's name; podcasts through `episode_name`), have `entry_title` call it for the `(_, title)` fallbacks, and switch `runtime.rs:1135` and its import (:24) to `entry_plain_title`. Add one test: `now_playing.title` for the tagged track is `"So What"`, not the combined string.

- [ ] **Step 4: Run, fix snapshot expectations**

Run: `cargo test --locked --no-fail-fast`
Expected: `tests/m5_tui_render.rs` and `tests/m5_runtime.rs::shows_tags` may assert the old row text; update them to the new strings.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(view): queue rows read Artist – Title, with the album below"
```

---

### Task 12: The recursive walk — `collect_tree`

**Files:**
- Modify: `src/application/browse.rs` (`BrowseRequest` :114, `BrowseResult` :152, `answer_with` :227, `answer` :253)
- Test: `tests/m8_tree_walk.rs`

**Interfaces:**
- Consumes: `list_directory`, `EntryKind`, `DirEntry`, `PlaylistId`, `MAX_PLAYLIST_ENTRIES`.
- Produces:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeCollected {
    pub dest: PlaylistId,
    /// Audio files in walk order, deduplicated by resolved `MediaId`.
    pub items: Vec<PathBuf>,
    /// Directories that could not be read. Logged, counted in the notice.
    pub unreadable: Vec<PathBuf>,
    /// The walk stopped at the limit: unvisited files may exist, and no
    /// count of them is known.
    pub scan_limit_reached: bool,
}

pub fn collect_tree(roots: &[PathBuf], dest: PlaylistId, limit: usize) -> TreeCollected
// BrowseRequest::CollectTree { roots: Vec<PathBuf>, dest: PlaylistId }
// BrowseResult::TreeCollected(TreeCollected)
```

  Walk order (§8): depth-first, each level in exactly `list_directory`'s order — subdirectories first by case-insensitive name, each walked to the bottom, then the level's own audio files. A root that is itself an audio file is collected directly. A directory symlink is skipped; a file symlink is followed, as `list_directory` does today.

- [ ] **Step 1: Write the failing tests** — `tests/m8_tree_walk.rs`

```rust
use std::path::{Path, PathBuf};

use tenuto::application::browse::{TreeCollected, collect_tree};
use tenuto::playlist::PlaylistId;

fn dest() -> PlaylistId {
    PlaylistId::from_raw_for_tests(1)
}

fn touch(root: &Path, relative: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|error| panic!("mkdir: {error}"));
    }
    std::fs::write(&path, b"").unwrap_or_else(|error| panic!("write: {error}"));
}

fn names(root: &Path, tree: &TreeCollected) -> Vec<String> {
    tree.items
        .iter()
        .map(|path| path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/"))
        .collect()
}

#[test]
fn subdirectories_come_before_a_levels_own_files_each_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    for file in ["z.mp3", "B.flac", "a/2.mp3", "a/1.mp3", "a/deep/0.wav", "c/x.m4a", "notes.txt", "a/cover.jpg"] {
        touch(dir.path(), file);
    }
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 4096);
    assert_eq!(
        names(dir.path(), &tree),
        ["a/deep/0.wav", "a/1.mp3", "a/2.mp3", "c/x.m4a", "B.flac", "z.mp3"],
        "list_directory's order at every level: a/1.mp3 before z.mp3"
    );
    assert!(tree.unreadable.is_empty());
    assert!(!tree.scan_limit_reached);
    assert_eq!(tree.dest, dest());
}

#[test]
fn the_limit_keeps_the_earliest_candidates_in_walk_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    for file in ["z.mp3", "a/1.mp3", "a/2.mp3"] {
        touch(dir.path(), file);
    }
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 2);
    assert_eq!(names(dir.path(), &tree), ["a/1.mp3", "a/2.mp3"]);
    assert!(tree.scan_limit_reached);

    let exact = collect_tree(&[dir.path().to_path_buf()], dest(), 3);
    assert!(!exact.scan_limit_reached, "reaching the limit with nothing left unvisited is not a truncation");
}

#[test]
fn overlapping_roots_and_a_root_that_is_a_file_are_deduplicated() {
    let dir = tempfile::tempdir().expect("tempdir");
    for file in ["a/1.mp3", "a/2.mp3"] {
        touch(dir.path(), file);
    }
    let roots = [dir.path().join("a/2.mp3"), dir.path().to_path_buf(), dir.path().join("a")];
    let tree = collect_tree(&roots, dest(), 4096);
    assert_eq!(names(dir.path(), &tree), ["a/2.mp3", "a/1.mp3"], "roots in the order given; each file once");
}

#[cfg(unix)]
#[test]
fn a_directory_symlink_is_skipped_so_a_cycle_cannot_recurse() {
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "a/1.mp3");
    std::os::unix::fs::symlink(dir.path(), dir.path().join("a/loop")).expect("symlink");
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 4096);
    assert_eq!(names(dir.path(), &tree), ["a/1.mp3"]);
}

#[cfg(unix)]
#[test]
fn a_file_symlink_alias_of_a_collected_file_is_not_added_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "a/1.mp3");
    std::os::unix::fs::symlink(dir.path().join("a/1.mp3"), dir.path().join("alias.mp3")).expect("symlink");
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 4096);
    assert_eq!(tree.items.len(), 1, "{:?}", tree.items);
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_reported_and_the_walk_goes_on() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    touch(dir.path(), "locked/1.mp3");
    touch(dir.path(), "open/2.mp3");
    let locked = dir.path().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let tree = collect_tree(&[dir.path().to_path_buf()], dest(), 4096);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
    if std::fs::read_dir(&locked).is_ok() && tree.unreadable.is_empty() {
        return; // running as root: nothing is unreadable
    }
    assert_eq!(tree.unreadable, [locked]);
    assert_eq!(names(dir.path(), &tree), ["open/2.mp3"]);
}
```

Whether the alias test dedupes depends on `resolve_path` canonicalizing. Read `src/application/source.rs:27` first: if it does not resolve symlinks, dedupe in `collect_tree` on `std::fs::canonicalize(path)` as well as on the `MediaId` — the spec asks for aliases to collapse, and the test is the contract.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_tree_walk`
Expected: compile error — `collect_tree` not found.

- [ ] **Step 3: Implement** (`src/application/browse.rs`)

```rust
/// Every audio file under `roots`, for a folder add (M8 §8). Depth-first,
/// each level in `list_directory`'s own order, so the walk and the listing
/// the listener is looking at agree; that order also decides what a
/// truncated walk keeps. Filename order, not album track order.
pub fn collect_tree(roots: &[PathBuf], dest: PlaylistId, limit: usize) -> TreeCollected {
    let mut walk = Walk { limit, seen: HashSet::new(), tree: TreeCollected {
        dest, items: Vec::new(), unreadable: Vec::new(), scan_limit_reached: false,
    } };
    for root in roots {
        let is_dir = std::fs::metadata(root).is_ok_and(|metadata| metadata.is_dir());
        if is_dir {
            walk.directory(root);
        } else if is_audio(root) {
            walk.file(root);
        }
    }
    walk.tree
}

struct Walk {
    limit: usize,
    seen: HashSet<MediaId>,
    tree: TreeCollected,
}

impl Walk {
    fn file(&mut self, path: &Path) {
        let Ok((media, _)) = resolve_path(path) else {
            return;
        };
        if self.seen.contains(&media) {
            return;
        }
        if self.tree.items.len() >= self.limit {
            // Only now is something known to be left out.
            self.tree.scan_limit_reached = true;
            return;
        }
        self.seen.insert(media);
        self.tree.items.push(path.to_path_buf());
    }

    fn directory(&mut self, path: &Path) {
        if self.tree.scan_limit_reached {
            return;
        }
        let entries = match list_directory(path) {
            Ok(entries) => entries,
            Err(_) => {
                self.tree.unreadable.push(path.to_path_buf());
                return;
            }
        };
        for entry in entries {
            if self.tree.scan_limit_reached {
                return;
            }
            match entry.kind {
                // `list_directory` follows symlinks when classifying, so ask
                // again without following: a directory symlink is skipped,
                // which is what rules out a cycle.
                EntryKind::Directory => {
                    let is_link = std::fs::symlink_metadata(&entry.path)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink());
                    if !is_link {
                        self.directory(&entry.path);
                    }
                }
                EntryKind::Audio => self.file(&entry.path),
                EntryKind::Other => {}
            }
        }
    }
}
```

`MediaId` must be `Hash` for `HashSet` — `BrowserState::queued` is already a `HashMap<MediaId, _>`, so it is. Add `BrowseRequest::CollectTree { roots, dest }`, `BrowseResult::TreeCollected(TreeCollected)`, the `answer` arm `BrowseRequest::CollectTree { roots, dest } => BrowseResult::TreeCollected(collect_tree(&roots, dest, MAX_PLAYLIST_ENTRIES))`, and the `answer_with` arm returning `TreeCollected { dest, items: Vec::new(), unreadable: roots, scan_limit_reached: false }`.

ponytail: recursion depth equals directory depth; a pathological tree thousands deep would overflow the worker's stack. An explicit stack is the upgrade; note it in a `// ponytail:` comment on `directory`.

- [ ] **Step 4: Run**

Run: `cargo test --test m8_tree_walk && cargo test --locked --no-fail-fast`
Expected: all pass. `BrowserState::apply` needs a `BrowseResult::TreeCollected(_) => None` arm to compile; Task 13 routes it properly.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(browse): a recursive, cycle-free, deduplicated audio walk in listing order"
```

---

### Task 13: Apply a collected tree through the application, not the browser

**Files:**
- Modify: `src/application/runtime.rs` (new `AppCommand::AddTree`, `fn add_tree`)
- Modify: `src/tui/mod.rs` (`Browsing::poll` :639 and its call site)
- Test: `tests/m8_runtime.rs` (append)

**Interfaces:**
- Consumes: `TreeCollected` (Task 12), `Session::enqueue(dest, …)`, `request_enrichment`.
- Produces: `AppCommand::AddTree(TreeCollected)`; `Browsing::poll(&mut self) -> Vec<TreeCollected>` — tree results are returned to the loop instead of being handed to `BrowserState`, so closing the browser or changing its directory cannot discard an explicitly requested add.
- Notice format, each part only when nonzero/true, joined by ` · `: `added N`, `N already queued`, `N unreadable`, `N did not fit`, `scan limit reached`. When nothing was added and nothing else applies: `nothing to add`. A deleted destination: `Playlist was deleted; nothing added`.

- [ ] **Step 1: Write the failing tests** (append to `tests/m8_runtime.rs`)

```rust
use tenuto::application::browse::TreeCollected;

fn tree(dest: PlaylistId, items: Vec<PathBuf>) -> TreeCollected {
    TreeCollected { dest, items, unreadable: Vec::new(), scan_limit_reached: false }
}

/// `count` copies of the fixture under distinct names, so each is its own media.
fn copies(dir: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|i| {
            let path = dir.join(format!("{i:04}.flac"));
            std::fs::copy(SHORT, &path).unwrap_or_else(|error| panic!("copy: {error}"));
            path
        })
        .collect()
}

#[test]
fn a_tree_lands_in_its_captured_destination_whatever_is_viewed_now() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::CreatePlaylist("Elsewhere".into()));
    rig.runtime.handle(AppCommand::AddTree(tree(first, copies(dir.path(), 3))));
    assert_eq!(rig.runtime.rows_of(first).len(), 3);
    assert!(rig.runtime.view().rows.is_empty());
    assert_eq!(rig.runtime.view().status.as_deref(), Some("added 3"));
}

#[test]
fn already_queued_files_are_skipped_and_counted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let files = copies(dir.path(), 3);
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue { dest: first, items: vec![EnqueueItem::Path(files[1].clone())] });
    rig.runtime.handle(AppCommand::AddTree(tree(first, files)));
    assert_eq!(rig.runtime.rows_of(first).len(), 3);
    assert_eq!(rig.runtime.view().status.as_deref(), Some("added 2 · 1 already queued"));
}

#[test]
fn a_tree_for_a_deleted_playlist_is_dropped_with_a_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::CreatePlaylist("Doomed".into()));
    let doomed = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::DeletePlaylist(doomed));
    rig.runtime.handle(AppCommand::AddTree(tree(doomed, copies(dir.path(), 2))));
    assert_eq!(rig.runtime.view().status.as_deref(), Some("Playlist was deleted; nothing added"));
    assert!(rig.runtime.view().rows.is_empty());
}

#[test]
fn capacity_is_rechecked_on_apply_and_the_notice_counts_only_what_is_known() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    // Fill all but two slots with URLs: cheap, and each is its own media.
    let fill = (0..MAX_PLAYLIST_ENTRIES - 2)
        .map(|i| EnqueueItem::Url(format!("https://example.test/{i}.mp3")))
        .collect();
    rig.runtime.handle(AppCommand::Enqueue { dest: first, items: fill });
    let mut collected = tree(first, copies(dir.path(), 5));
    collected.unreadable = vec![dir.path().join("locked")];
    collected.scan_limit_reached = true;
    rig.runtime.handle(AppCommand::AddTree(collected));
    assert_eq!(
        rig.runtime.view().status.as_deref(),
        Some("added 2 · 1 unreadable · 3 did not fit · scan limit reached")
    );
}
```

Import `tenuto::queue::MAX_PLAYLIST_ENTRIES` and `tenuto::playlist::PlaylistId` at the top of the file.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_runtime`
Expected: compile error — `AppCommand::AddTree` not found.

- [ ] **Step 3: Implement `add_tree`** (`src/application/runtime.rs`; `handle` arm `AppCommand::AddTree(tree) => self.add_tree(tree)`)

```rust
    /// Applies a finished folder walk to the destination captured when it
    /// was requested (M8 §8, P2). Capacity is rechecked here (P7): the
    /// playlists may have grown while the worker walked.
    fn add_tree(&mut self, tree: TreeCollected) {
        for path in &tree.unreadable {
            tracing::warn!(path = %path.display(), "folder add: directory could not be read");
        }
        let Some(playlist) = self.session.state().playlist(tree.dest) else {
            self.status = Some("Playlist was deleted; nothing added".to_owned());
            return;
        };
        let queued: HashSet<&MediaId> = playlist.queue().entries().iter().map(|entry| entry.media()).collect();
        let mut fresh = Vec::new();
        let mut already = 0usize;
        for path in &tree.items {
            match new_entry(EnqueueItem::Path(path.clone())) {
                Ok(entry) if queued.contains(entry.media()) => already += 1,
                Ok(entry) => fresh.push(entry),
                Err(_) => {}
            }
        }
        let free = MAX_PLAYLIST_ENTRIES.saturating_sub(self.session.state().total_entries());
        let did_not_fit = fresh.len().saturating_sub(free);
        fresh.truncate(free);
        let added = fresh.len();
        if added > 0 {
            match self.session.enqueue(tree.dest, fresh) {
                Ok((ids, action)) => {
                    self.submit(action);
                    self.request_enrichment(&ids);
                }
                Err(error) => {
                    self.status = Some(error.to_string());
                    return;
                }
            }
        }
        let mut parts = Vec::new();
        if added > 0 {
            parts.push(format!("added {added}"));
        }
        if already > 0 {
            parts.push(format!("{already} already queued"));
        }
        if !tree.unreadable.is_empty() {
            parts.push(format!("{} unreadable", tree.unreadable.len()));
        }
        if did_not_fit > 0 {
            parts.push(format!("{did_not_fit} did not fit"));
        }
        if tree.scan_limit_reached {
            parts.push("scan limit reached".to_owned());
        }
        self.status = Some(if parts.is_empty() { "nothing to add".to_owned() } else { parts.join(" · ") });
    }
```

`NewQueueEntry::media()` is the accessor Task 3 added. Use whatever logging macro the file already uses — `grep -n 'warn!\|tracing::' src/application/runtime.rs` — and pass the path through `displayable` if the existing calls do.

Because already-queued files are skipped *after* the walk, a truncated scan can leave free capacity. The notice says `scan limit reached` and claims nothing more; do not "top up" with a second walk.

- [ ] **Step 4: Route results past the browser** (`src/tui/mod.rs`)

```rust
    /// Hands every finished read to the open browser and returns the folder
    /// walks, which belong to the application: an add the listener asked for
    /// must land even if the browser has since closed or moved (M8 §8).
    fn poll(&mut self) -> Vec<TreeCollected> {
        let Some(worker) = &self.worker else {
            return Vec::new();
        };
        let mut trees = Vec::new();
        while let Some(result) = worker.try_result() {
            match result {
                BrowseResult::TreeCollected(tree) => trees.push(tree),
                result => {
                    if let Some(state) = &mut self.state
                        && let Some(follow_up) = state.apply(result)
                    {
                        worker.request(follow_up);
                    }
                }
            }
        }
        trees
    }
```

At `poll`'s call site in the loop: `for tree in browsing.poll() { runtime.handle(AppCommand::AddTree(tree)); }`, then reconcile the selection the way `apply_effect` does after any `Effect::App` (`take_selection_hint` + `ui.reconcile`). Remove the placeholder `TreeCollected(_) => None` arm Task 12 added to `BrowserState::apply` only if the match stays exhaustive; otherwise leave it with a comment that the loop intercepts the variant first.

- [ ] **Step 5: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(runtime): folder adds apply to their captured playlist, outliving the browser that asked"
```

---

### Task 14: Browser — a captured destination, marked directories, and `a`

**Files:**
- Modify: `src/tui/browser.rs` (`BrowserState` :50, `BrowserEffect` :84, `new` :95, the Space arm :269, `enqueueable` :326, `enqueue_selection` :513, `handle_key`'s Files-tab keys)
- Modify: `src/tui/mod.rs` (`Browsing::open` :614, `handle_event` :756 `sync_queue`, `apply_browser_effect` :787), `src/tui/render/browser.rs` (mark glyph on directory rows; the Files-tab key hints)
- Test: `tests/m8_browser.rs`

**Interfaces:**
- Consumes: `BrowseRequest::CollectTree`, `AppCommand::Enqueue { dest, items }`, `PlayerRuntime::{viewed, rows_of}`.
- Produces:
  - `BrowserState::new(cwd: PathBuf, dest: PlaylistId)`, `pub dest: PlaylistId`
  - `BrowserEffect::Enqueue { dest: PlaylistId, items: Vec<EnqueueItem> }`
  - `BrowserState::markable(&self, index: usize) -> bool` — `enqueueable`, or a directory row on the Files tab
  - Files tab: Space marks directories too; `a` adds marks-or-cursor, strictly additive; Enter with marks on an audio row = `a`; Enter with no marks unchanged (opens a directory; toggles a single audio row)

- [ ] **Step 1: Write the failing tests** — `tests/m8_browser.rs` (copy the imports and the key/entry helpers from the top of `tests/m5_browser.rs`; the snippets below assume its `key(KeyCode)` helper and a `files(&[(&str, EntryKind)]) -> BrowserState` builder that applies a `BrowseResult::Directory` — write `files` if `m5_browser.rs` has no equivalent)

```rust
const DEST: u64 = 7;

fn browser(rows: &[(&str, EntryKind)]) -> BrowserState {
    let mut state = BrowserState::new("/music".into(), PlaylistId::from_raw_for_tests(DEST));
    let entries = rows
        .iter()
        .map(|(name, kind)| DirEntry {
            name: (*name).to_owned(),
            path: format!("/music/{name}").into(),
            kind: *kind,
            media: (*kind == EntryKind::Audio).then(|| support::media(name)),
        })
        .collect();
    state.apply(BrowseResult::Directory { path: "/music".into(), entries: Ok(entries) });
    state
}

fn requests(effects: &[BrowserEffect]) -> Vec<BrowseRequest> {
    effects.iter().filter_map(|effect| match effect {
        BrowserEffect::Request(request) => Some(request.clone()),
        _ => None,
    }).collect()
}

#[test]
fn a_on_a_directory_asks_for_its_tree_with_the_captured_destination() {
    let mut state = browser(&[("Album", EntryKind::Directory), ("loose.mp3", EntryKind::Audio)]);
    let effects = state.handle_key(key(KeyCode::Char('a')));
    assert_eq!(
        requests(&effects),
        [BrowseRequest::CollectTree { roots: vec!["/music/Album".into()], dest: PlaylistId::from_raw_for_tests(DEST) }]
    );
}

#[test]
fn space_marks_directories_and_a_sends_files_and_directories_together_in_listing_order() {
    let mut state = browser(&[("Album", EntryKind::Directory), ("loose.mp3", EntryKind::Audio), ("notes.txt", EntryKind::Other)]);
    for _ in 0..3 {
        state.handle_key(key(KeyCode::Char(' ')));
        state.handle_key(key(KeyCode::Down));
    }
    assert_eq!(state.marked.iter().copied().collect::<Vec<_>>(), [0, 1], "a text file is not markable");
    let effects = state.handle_key(key(KeyCode::Char('a')));
    assert_eq!(
        requests(&effects),
        [BrowseRequest::CollectTree { roots: vec!["/music/Album".into(), "/music/loose.mp3".into()], dest: PlaylistId::from_raw_for_tests(DEST) }]
    );
    assert!(state.marked.is_empty(), "marks clear once they are added");
}

#[test]
fn a_is_additive_where_enter_toggles() {
    let mut state = browser(&[("loose.mp3", EntryKind::Audio)]);
    state.sync_queue(&[queued_row(support::media("loose.mp3"), 5)]);
    assert!(matches!(state.handle_key(key(KeyCode::Enter))[..], [BrowserEffect::Remove(_)]));
    assert!(state.handle_key(key(KeyCode::Char('a'))).is_empty(), "already queued: skipped, never removed");
}

#[test]
fn enter_on_an_audio_row_with_marks_does_what_a_does_and_enter_on_a_directory_still_opens_it() {
    let mut state = browser(&[("Album", EntryKind::Directory), ("loose.mp3", EntryKind::Audio)]);
    state.handle_key(key(KeyCode::Char(' ')));
    state.handle_key(key(KeyCode::Down));
    let effects = state.handle_key(key(KeyCode::Enter));
    assert_eq!(
        requests(&effects),
        [BrowseRequest::CollectTree { roots: vec!["/music/Album".into()], dest: PlaylistId::from_raw_for_tests(DEST) }],
        "the marked directory is not silently ignored"
    );

    let mut state = browser(&[("Album", EntryKind::Directory)]);
    assert_eq!(requests(&state.handle_key(key(KeyCode::Enter))), [BrowseRequest::Directory("/music/Album".into())]);
}

#[test]
fn a_single_file_with_no_marks_still_enqueues_directly_with_the_destination() {
    let mut state = browser(&[("loose.mp3", EntryKind::Audio)]);
    let effects = state.handle_key(key(KeyCode::Char('a')));
    assert!(matches!(
        &effects[..],
        [BrowserEffect::Enqueue { dest, items }] if dest.get() == DEST && items.len() == 1
    ));
}
```

`queued_row(media, raw_id)` builds a `QueueRow` — copy the literal from `tests/m5_browser.rs` if it has one, else construct `QueueRow { id, media, title: String::new(), subtitle: None, duration: None, saved: None }` with `QueueEntryId` obtained by enqueuing into a scratch `Queue`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test m8_browser`
Expected: compile error — `BrowserState::new` takes one argument.

- [ ] **Step 3: Implement**

```rust
    /// Whether Space may mark row `index`: anything enqueueable, and on the
    /// Files tab a directory too, which `a` adds recursively (M8 §8).
    pub fn markable(&self, index: usize) -> bool {
        self.enqueueable(index)
            || (self.tab == BrowserTab::Files
                && self.entries.get(index).is_some_and(|entry| entry.kind == EntryKind::Directory))
    }

    /// `a` on the Files tab: the marked rows, or the cursor row when nothing
    /// is marked; files and directories alike; strictly additive. Any
    /// directory makes it a worker walk, which also dedupes overlapping
    /// picks; files alone enqueue directly, as Enter does.
    fn add_selection(&mut self) -> Vec<BrowserEffect> {
        let indices: Vec<usize> = if self.marked.is_empty() {
            vec![self.cursor]
        } else {
            std::mem::take(&mut self.marked).into_iter().collect()
        };
        let picked: Vec<&DirEntry> = indices
            .into_iter()
            .filter(|index| self.markable(*index) && self.queued_at(*index).is_none())
            .filter_map(|index| self.entries.get(index))
            .collect();
        if picked.is_empty() {
            return Vec::new();
        }
        if picked.iter().any(|entry| entry.kind == EntryKind::Directory) {
            let roots = picked.iter().map(|entry| entry.path.clone()).collect();
            return vec![BrowserEffect::Request(BrowseRequest::CollectTree { roots, dest: self.dest })];
        }
        let items = picked.iter().map(|entry| EnqueueItem::Path(entry.path.clone())).collect();
        vec![BrowserEffect::Enqueue { dest: self.dest, items }]
    }
```

- Space arm: `self.markable(self.cursor)` instead of `self.enqueueable(self.cursor)`. Update the `marked` field's doc comment ("only ever of markable rows").
- `handle_key`: on the Files tab, with no prompt or confirm open, `KeyCode::Char('a') => self.add_selection()`. The Podcasts and Radio tabs' `a` stays exactly as it is — put the new arm behind `self.tab == BrowserTab::Files` so it cannot shadow them.
- `activate` (Enter), Files tab, audio row: `if self.marked.is_empty() { self.enqueue_selection() } else { self.add_selection() }`. Directory row: unchanged.
- `enqueue_selection`: its `BrowserEffect::Enqueue(items)` becomes `BrowserEffect::Enqueue { dest: self.dest, items }` (Podcasts and Radio too).
- Marks already clear when the directory changes; confirm with the existing test in `tests/m5_browser.rs` and add one if there is none.

`src/tui/mod.rs`: `Browsing::open` passes `runtime.viewed()` to `BrowserState::new`; `handle_event` syncs with the *destination's* rows — `browser.sync_queue(&front.runtime.rows_of(browser.dest))` — so the ticks and Enter-to-remove refer to the playlist the browser adds to; `apply_browser_effect` forwards `BrowserEffect::Enqueue { dest, items }` as `AppCommand::Enqueue { dest, items }`. `render/browser.rs`: draw the mark glyph for a marked directory row the way it is drawn for a marked file, and add `a add` to the Files-tab hint line.

- [ ] **Step 4: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass; `tests/m5_browser.rs` needs only the constructor and effect-shape updates.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(browser): mark directories, add with a, and one captured destination for every add"
```

---

### Task 15: TUI — the tab strip, playlist keys, overlays that capture their target

**Files:**
- Create: `src/tui/tabs.rs`; Modify: `src/tui/mod.rs` (`mod tabs;`)
- Modify: `src/tui/state.rs` (`Overlay` :9), `src/tui/input.rs` (`handle_key` :54, `input_overlay` :196, `confirm_overlay` :230, `no_overlay` :245), `src/tui/render.rs` (`draw_overlay` :190, `draw_confirm_overlay` :223, `draw_input_overlay` :261, `HELP_LINES`, `draw_queue` :769)
- Modify: `tests/support/views.rs` (`view` :57 gains `tabs` and `viewed`), `tests/m5_tui_input.rs`, `tests/m5_tui_render.rs` for the overlay variants
- Test: `tests/m8_tui.rs`, plus unit tests inside `src/tui/tabs.rs`

**Interfaces:**
- Consumes: `PlayerView::{tabs, viewed}`, `PlaylistTab`, the Task 10 `AppCommand` variants.
- Produces:

```rust
// tui/state.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputPurpose { AddUrl(PlaylistId), NewPlaylist, Rename(PlaylistId) }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Overlay { None, Help, ConfirmClear(PlaylistId), ConfirmDelete(PlaylistId), Input(InputPurpose), Browser }

// tui/tabs.rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabLabel { pub text: String, pub viewed: bool, pub playing: bool }
pub const PLAYING_MARK: &str = "▶";
pub const SHUFFLE_MARK: &str = " ⤮";
pub const GAP: &str = "  ";
pub fn strip(tabs: &[PlaylistTab], viewed: PlaylistId, width: usize) -> Vec<TabLabel>
pub fn compact(tabs: &[PlaylistTab], viewed: PlaylistId, width: usize) -> String
```

  Every overlay that acts on a playlist carries the `PlaylistId` captured when it opened (§10). The strip draws in the queue box's top border — the row above the list — replacing the fixed ` QUEUE ` title, so no layout region changes; the Minimal tier, which has no border, draws `compact` on the list's first row.

- [ ] **Step 1: Write the failing strip tests** (bottom of the new `src/tui/tabs.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn tab(id: u64, name: &str, playing: bool, shuffled: bool) -> PlaylistTab {
        PlaylistTab { id: PlaylistId::from_raw_for_tests(id), name: name.to_owned(), playing, shuffled }
    }

    fn texts(labels: &[TabLabel]) -> Vec<&str> {
        labels.iter().map(|label| label.text.as_str()).collect()
    }

    fn columns(labels: &[TabLabel]) -> usize {
        labels.iter().map(|label| label.text.width()).sum::<usize>() + GAP.width() * labels.len().saturating_sub(1)
    }

    #[test]
    fn marks_sit_on_the_playing_and_the_shuffled_tab() {
        let tabs = [tab(1, "Default", false, false), tab(2, "Morning", false, true), tab(3, "Workout", true, true)];
        let labels = strip(&tabs, PlaylistId::from_raw_for_tests(2), 80);
        assert_eq!(texts(&labels), ["Default", "Morning ⤮", "▶Workout ⤮"]);
        assert_eq!(labels.iter().map(|l| (l.viewed, l.playing)).collect::<Vec<_>>(), [(false, false), (true, false), (false, true)]);
    }

    #[test]
    fn the_window_scrolls_to_keep_the_viewed_tab_and_never_exceeds_the_width() {
        let tabs: Vec<_> = (1..=9).map(|i| tab(i, &format!("Playlist{i}"), false, false)).collect();
        for viewed in 1..=9 {
            let labels = strip(&tabs, PlaylistId::from_raw_for_tests(viewed), 34);
            assert!(labels.iter().any(|label| label.viewed), "viewed {viewed} is on screen");
            assert!(columns(&labels) <= 34, "viewed {viewed}: {} columns", columns(&labels));
        }
    }

    #[test]
    fn width_is_measured_in_columns_not_bytes_or_chars() {
        // Each CJK character is two columns and three bytes.
        let tabs = [tab(1, "音楽音楽", false, false), tab(2, "ab", false, false)];
        let labels = strip(&tabs, PlaylistId::from_raw_for_tests(1), 10);
        assert_eq!(texts(&labels), ["音楽音楽"], "8 columns + gap + 2 = 12 > 10");
    }

    #[test]
    fn a_single_over_long_name_is_clipped_with_an_ellipsis() {
        let tabs = [tab(1, "An extraordinarily long playlist name", true, true)];
        let labels = strip(&tabs, PlaylistId::from_raw_for_tests(1), 12);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].text.width(), 12);
        assert!(labels[0].text.ends_with('…'));
    }

    #[test]
    fn zero_width_and_an_unknown_viewed_id_are_harmless() {
        let tabs = [tab(1, "Default", true, false)];
        assert!(strip(&tabs, PlaylistId::from_raw_for_tests(1), 0).is_empty());
        assert_eq!(texts(&strip(&tabs, PlaylistId::from_raw_for_tests(9), 40)), ["▶Default"]);
    }

    #[test]
    fn compact_names_the_viewed_playlist_and_its_place() {
        let tabs = [tab(1, "Default", false, false), tab(2, "Morning", true, true)];
        assert_eq!(compact(&tabs, PlaylistId::from_raw_for_tests(2), 40), "▶Morning ⤮ 2/2");
        assert!(compact(&tabs, PlaylistId::from_raw_for_tests(2), 8).width() <= 8);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib tui::tabs`
Expected: compile error — `strip` not found.

- [ ] **Step 3: Implement `src/tui/tabs.rs`**

```rust
//! The playlist tab strip (M8 §10): which labels fit, in terminal columns.
//! Pure — `render` draws what this returns. Names arrive already sanitized
//! (`PlaylistTab::name`).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::application::view::PlaylistTab;
use crate::playlist::PlaylistId;

pub const PLAYING_MARK: &str = "▶";
pub const SHUFFLE_MARK: &str = " ⤮";
pub const GAP: &str = "  ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabLabel {
    pub text: String,
    pub viewed: bool,
    pub playing: bool,
}

fn label(tab: &PlaylistTab) -> String {
    format!(
        "{}{}{}",
        if tab.playing { PLAYING_MARK } else { "" },
        tab.name,
        if tab.shuffled { SHUFFLE_MARK } else { "" },
    )
}

/// `text` cut to `width` columns, ending in `…` when anything was cut.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// The labels that fit in `width` columns, always including the viewed tab:
/// as many tabs before it as fit with it, then as many after.
pub fn strip(tabs: &[PlaylistTab], viewed: PlaylistId, width: usize) -> Vec<TabLabel> {
    if width == 0 || tabs.is_empty() {
        return Vec::new();
    }
    let at = tabs.iter().position(|tab| tab.id == viewed).unwrap_or(0);
    let texts: Vec<String> = tabs.iter().map(label).collect();
    let cost = |index: usize| texts[index].width() + GAP.width();

    let mut used = texts[at].width().min(width);
    let (mut start, mut end) = (at, at + 1);
    while end < tabs.len() && used + cost(end) <= width {
        used += cost(end);
        end += 1;
    }
    while start > 0 && used + cost(start - 1) <= width {
        used += cost(start - 1);
        start -= 1;
    }
    (start..end)
        .map(|index| TabLabel {
            text: clip(&texts[index], width),
            // `at` is the viewed tab, or the first tab for an ID that is gone.
            viewed: index == at,
            playing: tabs[index].playing,
        })
        .collect()
}

/// The Minimal tier's one-row form: the viewed label and `n/m`.
pub fn compact(tabs: &[PlaylistTab], viewed: PlaylistId, width: usize) -> String {
    let at = tabs.iter().position(|tab| tab.id == viewed).unwrap_or(0);
    let Some(tab) = tabs.get(at) else {
        return String::new();
    };
    let place = format!(" {}/{}", at + 1, tabs.len());
    let room = width.saturating_sub(place.width());
    clip(&format!("{}{place}", clip(&label(tab), room)), width)
}
```

The window always contains `at` and starts from its width, so it extends forward, then backward, and can never exceed `width`; for the last tab the forward loop adds nothing and the backward loop fills the row.

- [ ] **Step 4: Write the failing key tests** — `tests/m8_tui.rs` (imports and the `key` helper from `tests/m5_tui_input.rs:1-30`; `views::view` from `tests/support/views.rs`, extended in Step 5 to fill `tabs` with one playing `Default` tab of ID 1 and `viewed` = that ID)

```rust
fn two_tab_view() -> PlayerView {
    let mut view = views::sample_view();
    view.tabs.push(PlaylistTab { id: PlaylistId::from_raw_for_tests(2), name: "Jazz".into(), playing: false, shuffled: false });
    view.viewed = PlaylistId::from_raw_for_tests(2);
    view
}

fn commands(effects: Vec<Effect>) -> Vec<String> {
    effects.into_iter().map(|effect| format!("{effect:?}")).collect()
}

#[test]
fn tab_and_shift_tab_cycle_the_view_and_z_toggles_shuffle_on_the_viewed_playlist() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    assert_eq!(commands(handle_key(key(KeyCode::Tab), &mut ui, &view)), ["App(ViewNext)"]);
    assert_eq!(commands(handle_key(key(KeyCode::BackTab), &mut ui, &view)), ["App(ViewPrevious)"]);
    assert_eq!(commands(handle_key(key(KeyCode::Char('z')), &mut ui, &view)), ["App(ToggleShuffle(PlaylistId(2)))"]);
}

#[test]
fn n_names_a_new_playlist_and_r_renames_the_captured_one_prefilled() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    handle_key(key(KeyCode::Char('n')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::Input(InputPurpose::NewPlaylist));
    for c in "Dusk".chars() {
        handle_key(key(KeyCode::Char(c)), &mut ui, &view);
    }
    assert_eq!(commands(handle_key(key(KeyCode::Enter), &mut ui, &view)), [r#"App(CreatePlaylist("Dusk"))"#]);

    handle_key(key(KeyCode::Char('r')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::Input(InputPurpose::Rename(PlaylistId::from_raw_for_tests(2))));
    assert_eq!(ui.input, "Jazz");
    // The view moves on before Enter: the rename still targets playlist 2.
    let mut moved = view.clone();
    moved.viewed = PlaylistId::from_raw_for_tests(1);
    handle_key(key(KeyCode::Char('!')), &mut ui, &moved);
    assert_eq!(commands(handle_key(key(KeyCode::Enter), &mut ui, &moved)), [r#"App(RenamePlaylist(PlaylistId(2), "Jazz!"))"#]);
}

#[test]
fn delete_and_clear_confirm_against_the_playlist_captured_when_they_opened() {
    let view = two_tab_view();
    let mut moved = view.clone();
    moved.viewed = PlaylistId::from_raw_for_tests(1);
    let mut ui = UiState::new(false);

    handle_key(key(KeyCode::Char('D')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::ConfirmDelete(PlaylistId::from_raw_for_tests(2)));
    assert_eq!(commands(handle_key(key(KeyCode::Char('y')), &mut ui, &moved)), ["App(DeletePlaylist(PlaylistId(2)))"]);

    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert_eq!(commands(handle_key(key(KeyCode::Char('y')), &mut ui, &moved)), ["App(ClearPlaylist(PlaylistId(2)))"]);

    handle_key(key(KeyCode::Char('D')), &mut ui, &view);
    assert!(handle_key(key(KeyCode::Char('n')), &mut ui, &view).is_empty(), "anything but y cancels");
    assert_eq!(ui.overlay, Overlay::None);
}

#[test]
fn the_last_playlist_cannot_even_open_the_delete_confirmation() {
    let view = views::sample_view();
    let mut ui = UiState::new(false);
    let effects = handle_key(key(KeyCode::Char('D')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::None);
    assert!(matches!(effects[..], [Effect::Notice(_)]));
}

#[test]
fn an_added_url_goes_to_the_playlist_viewed_when_a_was_pressed() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::Input(InputPurpose::AddUrl(PlaylistId::from_raw_for_tests(2))));
}

#[test]
fn space_p_next_and_previous_carry_no_selection() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    ui.selected = view.rows.first().map(|row| row.id);
    assert_eq!(commands(handle_key(key(KeyCode::Char(' ')), &mut ui, &view)), ["App(PlayPause)"]);
    assert_eq!(commands(handle_key(key(KeyCode::Char('p')), &mut ui, &view)), ["App(Play)"]);
    assert_eq!(commands(handle_key(key(KeyCode::Char(']')), &mut ui, &view)), ["App(Next)"]);
}
```

The `Debug` strings assume `#[derive(Debug)]` tuple-struct output for `PlaylistId`; if the crate formats it differently, match on the `AppCommand` variants instead of comparing strings.

- [ ] **Step 5: Implement state, input, render**

`state.rs`: the `Overlay` and `InputPurpose` above. `tests/support/views.rs::view` fills `tabs: vec![PlaylistTab { id: PlaylistId::from_raw_for_tests(1), name: "Default".into(), playing: true, shuffled: false }]` and `viewed: PlaylistId::from_raw_for_tests(1)`.

`input.rs` — `handle_key` dispatch:

```rust
        Overlay::Input(purpose) => input_overlay(key, ui, purpose),
        Overlay::ConfirmClear(id) => confirm_overlay(key, ui, AppCommand::ClearPlaylist(id)),
        Overlay::ConfirmDelete(id) => confirm_overlay(key, ui, AppCommand::DeletePlaylist(id)),
```

`confirm_overlay(key, ui, on_yes: AppCommand)` returns `vec![Effect::App(on_yes)]` on `y`, as today. `input_overlay`'s Enter:

```rust
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![Effect::App(match purpose {
                    InputPurpose::AddUrl(dest) => AppCommand::Enqueue {
                        dest,
                        items: vec![EnqueueItem::from_input(&trimmed)],
                    },
                    InputPurpose::NewPlaylist => AppCommand::CreatePlaylist(trimmed),
                    InputPurpose::Rename(id) => AppCommand::RenamePlaylist(id, trimmed),
                })]
            }
```

`no_overlay` — new and changed arms (the four transport arms lost `selected` in Task 10):

```rust
        KeyCode::Tab => vec![Effect::App(AppCommand::ViewNext)],
        KeyCode::BackTab => vec![Effect::App(AppCommand::ViewPrevious)],
        KeyCode::Char('z') => vec![Effect::App(AppCommand::ToggleShuffle(view.viewed))],
        KeyCode::Char('n') => open_input(ui, InputPurpose::NewPlaylist, ""),
        KeyCode::Char('r') => {
            let name = view.tabs.iter().find(|tab| tab.id == view.viewed).map_or("", |tab| tab.name.as_str());
            open_input(ui, InputPurpose::Rename(view.viewed), name)
        }
        KeyCode::Char('a') => open_input(ui, InputPurpose::AddUrl(view.viewed), ""),
        KeyCode::Char('c') => {
            ui.overlay = Overlay::ConfirmClear(view.viewed);
            Vec::new()
        }
        KeyCode::Char('D') if view.tabs.len() <= 1 => {
            vec![Effect::Notice("The last playlist cannot be deleted")]
        }
        KeyCode::Char('D') => {
            ui.overlay = Overlay::ConfirmDelete(view.viewed);
            Vec::new()
        }
```

```rust
fn open_input(ui: &mut UiState, purpose: InputPurpose, initial: &str) -> Vec<Effect> {
    ui.overlay = Overlay::Input(purpose);
    ui.input.clear();
    ui.input.push_str(initial);
    Vec::new()
}
```

`BackTab` arrives with `SHIFT` set and `D` as `Char('D')` with `SHIFT`; `blocks_ordinary_bindings` only blocks CONTROL and ALT (`K`/`J` already rely on this). After `ViewNext`/`ViewPrevious`, `apply_effect` already calls `ui.reconcile(&view, hint)`, which moves the selection to a row of the new view; reset `ui.queue_offset = 0` there when `view.viewed` changed.

`render.rs`:
- `draw_queue`, bordered tiers: replace the ` QUEUE ` title with the strip. Available width = `area.width - 2` (corners) minus the right-aligned count's width; build `Line` from `tabs::strip(&view.tabs, view.viewed, width)`, labels joined by `tabs::GAP`, styled `theme.cream` + `BOLD` when `viewed`, `theme.text` when `playing`, `theme.muted` otherwise, with one space of padding at each end as ` QUEUE ` had. Minimal tier: draw `tabs::compact(...)` on the first row of `area` in `theme.muted` and give the list the remaining rows.
- `draw_overlay`: `ConfirmClear(_)` → the existing box; `ConfirmDelete(id)` → the same box with the text `Delete playlist "<name>"? y to confirm` (name from `view.tabs`, already sanitized); `Input(purpose)` → the existing input box with its title chosen by purpose: ` add a file or URL `, ` new playlist `, ` rename playlist `.
- `HELP_LINES`: add `Tab/S-Tab  next / previous playlist`, `n / r / D   new / rename / delete playlist`, `z           shuffle this playlist`, and reword `c` as `clear this playlist`. Keep every line ASCII — `draw_help_overlay` sizes the box with `line.len()`, so write `Shift-Tab` rather than a glyph.
- `EMPTY_QUEUE`'s text stays; it now describes the viewed playlist.

- [ ] **Step 6: Run**

Run: `cargo test --locked --no-fail-fast`
Expected: all pass. `tests/m5_tui_render.rs` snapshots that contain ` QUEUE ` now contain ` Default ` (the one tab, unmarked `▶Default` when playing — update the expected strings from the actual render, after checking they match this task's contract).

- [ ] **Step 7: Build and look at it**

Run: `cargo build --release && ./target/release/tenuto tui` in a terminal at least 80×28, then narrow it below 50 columns. Check: the strip sits in the queue border; `n`, `r`, `D`, `z`, `Tab` behave; the Minimal tier shows `name n/m`. This is a developer smoke check, not the acceptance pass — do not record it in `m8-acceptance.md`.

- [ ] **Step 8: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add -A src tests
git commit -m "feat(tui): a playlist tab strip, keys to manage playlists, and overlays that capture their target"
```

---

### Task 16: Measure a full snapshot; open the acceptance record

**Files:**
- Test: `tests/m8_snapshot_size.rs`
- Create: `docs/m8-acceptance.md`

**Interfaces:**
- Consumes: `Session::{create_playlist, enqueue}`, `StateStore`, `PersistedState: Clone + Serialize`.
- Produces: an `#[ignore]`d test that prints the four numbers §12 asks for; an acceptance record whose two manual items are explicitly **pending**.

- [ ] **Step 1: Write the measurement** — `tests/m8_snapshot_size.rs`

```rust
//! M8 §12: what a full 4,096-entry state costs. Ignored by default — it
//! measures, it does not assert. Run it by hand and copy the output into
//! docs/m8-acceptance.md:
//!
//!   cargo test --release --test m8_snapshot_size -- --ignored --nocapture

use std::sync::Arc;
use std::time::Instant;

use tenuto::clock::FakeClock;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::persistence::writer::StateSink;
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::queue::{DisplayMetadata, MAX_PLAYLIST_ENTRIES, NewQueueEntry, QueueSource};
use tenuto::session::Session;

/// A path, tags and a duration of the length a real library has.
fn representative(i: usize) -> (MediaId, NewQueueEntry) {
    let path = AbsolutePath::new(
        format!("/home/listener/Music/Various Artists/Some Fairly Long Album Title (Deluxe Edition) [2019]/{:02} - A Track Title of Ordinary Length {i}.flac", i % 20 + 1).into(),
    )
    .unwrap_or_else(|error| panic!("absolute: {error}"));
    let media = MediaId::LocalFile(path.clone());
    let entry = NewQueueEntry::new(
        media.clone(),
        QueueSource::LocalFile(path),
        DisplayMetadata {
            title: Some(format!("A Track Title of Ordinary Length {i}")),
            artist: Some("An Artist With a Reasonable Name".into()),
            album: Some("Some Fairly Long Album Title (Deluxe Edition)".into()),
            year: Some("2019".into()),
            duration: None,
        },
    )
    .unwrap_or_else(|error| panic!("valid: {error}"));
    (media, entry)
}

#[test]
#[ignore = "a measurement, run by hand; see the module comment"]
fn a_full_snapshot_costs_this_much() {
    let mut session = Session::new(PersistedState::default());
    let mut playlists = vec![session.state().playing()];
    for i in 1..8 {
        playlists.push(session.create_playlist(&format!("Playlist {i}")).expect("room").0);
    }
    let mut medias = Vec::new();
    for (index, chunk) in (0..MAX_PLAYLIST_ENTRIES).collect::<Vec<_>>().chunks(MAX_PLAYLIST_ENTRIES / 8).enumerate() {
        let (ms, entries): (Vec<_>, Vec<_>) = chunk.iter().map(|i| representative(*i)).unzip();
        medias.extend(ms);
        session.enqueue(playlists[index], entries).expect("fits");
    }
    // A realistic checkpoint map: one in four tracks has been played.
    let mut state = session.state().clone();
    for media in medias.iter().step_by(4) {
        state.record(
            &PlaybackCheckpoint { media: media.clone(), position: std::time::Duration::from_secs(95), updated_at: time::OffsetDateTime::UNIX_EPOCH },
            true,
        );
    }
    assert_eq!(state.total_entries(), MAX_PLAYLIST_ENTRIES);

    let started = Instant::now();
    let cloned = state.clone();
    let clone_time = started.elapsed();

    let started = Instant::now();
    let bytes = serde_json::to_vec_pretty(&cloned).expect("serializes");
    let serialize_time = started.elapsed();

    let dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::new(dir.path().join("state.json"), Arc::new(FakeClock::new()));
    let started = Instant::now();
    store.write(&cloned).expect("writes");
    let write_time = started.elapsed();

    println!("entries            {}", state.total_entries());
    println!("checkpoints        {}", state.len());
    println!("bytes on disk      {}", bytes.len());
    println!("clone (app thread) {clone_time:?}");
    println!("serialize          {serialize_time:?}");
    println!("atomic write       {write_time:?}  (serialize + write + rename, as the writer thread does it)");
}
```

If `PersistedState::record` evicts when the checkpoint cap is below 1,024, lower `step_by` until `state.len()` is what the cap allows and say so in the printed label. `StateStore` implements `StateSink::write` (`src/persistence/store.rs`); if its write entry point has another name, use that.

- [ ] **Step 2: Run it**

Run: `cargo test --release --test m8_snapshot_size -- --ignored --nocapture`
Expected: six printed lines. Keep the output for Step 3.

- [ ] **Step 3: Write `docs/m8-acceptance.md`** (follow the structure of `docs/m7.1-acceptance.md`)

Sections: what M8 delivered, by spec section; the automated evidence (test files `tests/m8_*.rs` and what each pins); then:

```markdown
## Snapshot cost at the 4,096-entry cap

Measured on <machine, OS, filesystem, date> with
`cargo test --release --test m8_snapshot_size -- --ignored --nocapture`:

<paste the six lines verbatim>

The "about 1 MB" figure used while designing was an estimate. Reading of
these numbers: <one paragraph — is a 5-second cadence comfortable? If not,
the response is a follow-up design (spec §11, §12), not a change here.>

## Manual pass in Ghostty — PENDING

Not yet performed. To do, with a real music folder:
- [ ] add a folder with `a`; rows fill in as `Artist – Title`
- [ ] tracks start from 0:00; a podcast episode still resumes
- [ ] `z` shuffles; next/previous and end-of-track follow one order; turning it off continues in list order
- [ ] `n`, `r`, `D`, `Tab`; delete the playing playlist while it plays
- [ ] Enter in another playlist while a track plays; the old tab keeps its cursor
- [ ] quit and restart: playlists, cursors and shuffle survive
- [ ] the strip at 80×28, at <50 columns, and with a very long name
```

If Step 2 was not run on real hardware by whoever writes this file, the snapshot section is headed `— PENDING` too and contains the command, not numbers. Never write a number that was not measured.

- [ ] **Step 4: Commit**

```bash
cargo fmt && cargo clippy --locked --all-targets --all-features -- -D warnings
git add tests/m8_snapshot_size.rs docs/m8-acceptance.md
git commit -m "test(m8): measure a full snapshot; open the acceptance record with its manual items pending"
```

---

### Task 17: Documentation and changelog

**Files:**
- Modify: `CHANGELOG.md`, `docs/reference.md`, `docs/architecture.md`, `README.md` (only if it lists keys or features)

This is spec §13 and the user's standing instruction: a branch is not finished until the docs and the changelog describe it.

- [ ] **Step 1: `CHANGELOG.md`**, under `## [Unreleased]`

Append to `### Added`:

```markdown
- Playlists. The queue is now one of several named playlists, shown as tabs
  above the list: `Tab` and `Shift-Tab` switch the view, `n` creates one,
  `r` renames and `D` deletes it after a `y`. Playback stays on the playing
  playlist while you look at another; Enter in any playlist plays from it.
  An existing queue becomes a playlist named `Default`.
- Shuffle, per playlist, with `z`. The list keeps its order on screen;
  next, previous and end-of-track follow one shuffled order that survives a
  restart. Turning it off continues in list order from the current track.
- Adding a folder from the Files tab: Space now marks directories too, and
  `a` adds the marked rows — or the row under the cursor — recursively, in
  the browser's own listing order. Directory symlinks are skipped.
- Queue rows read `Artist – Title`, with the album below.
```

Add a `### Changed` section (or append to it):

```markdown
- Local files and plain URLs start from the beginning each time they are
  loaded, from `tenuto play` as well as the player. Podcast episodes still
  resume, and pausing or stopping a track still continues where it was.
- The entry limit is 4,096 across all playlists, replacing 256 per queue.
- On a playlist that has never played, `p` and Space start its first track
  in playback order rather than the highlighted row, and next/previous wait
  for a current track. Enter on the highlighted row is unchanged.
- `state.json` is schema 4. A schema 3 file is migrated on first load; an
  older build cannot read the new file.
```

- [ ] **Step 2: `docs/reference.md`**

Read the file first and follow its existing tables. Add: the five playlist keys and `z` to the player key table; `a` and "Space marks directories" to the Files-tab keys; a short "Playlists" section (viewed vs. playing, the tab marks `▶` and `⤮`, limits of 32 playlists / 4,096 entries / 40-character names); the resume rule by source kind; the folder-add notice and what each part means, including that `scan limit reached` gives no count. Change every sentence that says "the queue" where it now means "the viewed playlist" or "the playing playlist" — say which.

- [ ] **Step 3: `docs/architecture.md`**

- Intro paragraph: "as built through milestone 8"; add `m8-acceptance.md` to the list of acceptance records.
- §1: add the fresh-load resume rule as a fixed rule, stated so it cannot be read as weakening the position contract.
- §3 container table: `state.json` row → "checkpoints, playlists, volume — schema 4".
- §4 components: `queue.rs, resume.rs` box gains `playlist.rs`; add two or three sentences on the model — a playlist wraps a queue; IDs are global and allocated by `PersistedState`; a cursor is remembered, ownership is adoption; the viewed playlist is transient runtime state.
- Wherever the runtime sequence mentions enqueue or advance, name the playlist.
Keep Mermaid diagrams valid: change labels, not structure, unless a node is genuinely new.

- [ ] **Step 4: Check the docs against the code**

Run: `grep -n "256\|MAX_QUEUE_ENTRIES\|schema 3\|ConfirmClear\|clear_queue" -r docs README.md src | grep -v superpowers`
Expected: no stale mention outside historical specs and plans. Then `cargo doc --locked --no-deps` — expected: no broken intra-doc links (CI runs this).

- [ ] **Step 5: Final gates**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast`
Expected: clean, all pass.

- [ ] **Step 6: Commit**

```bash
git add CHANGELOG.md docs README.md
git commit -m "docs: playlists, shuffle, folder add and the resume rule in the reference, architecture and changelog"
```

Then use superpowers:finishing-a-development-branch. The PR description must say that the Ghostty pass (and the snapshot measurement, if it was not run) is pending in `docs/m8-acceptance.md`.

---

## Spec coverage

| Spec | Task |
|---|---|
| §3 P1 at least one playlist | 3 (`LastPlaylist`), 4 (rule 7), 6, 15 (`D` refused) |
| §3 P2 explicit destinations | 5, 10, 13, 14, 15 |
| §3 P3 cursor ≠ ownership | 6 |
| §3 P4 `playing` at adoption | 7, 10 |
| §3 P5 view vs. playing, Loading exception | 9, 10 |
| §3 P6 one traversal policy | 1, 7, 9 |
| §3 P7 global cap where IDs are allocated | 2, 3, 13 |
| §3 P8 no lost checkpoint | 4 (`kept_checkpoint` in every recovery test) |
| §4 model, limits, allocation, exhaustion | 1, 2, 3 |
| §5 ownership check, scoped invalidation, metadata workers, re-pointing | 3, 6, 10 |
| §6 migration and the ten recovery rules | 4 |
| §7 traversal, shuffled order, toggling, transport, start from zero | 1, 7, 8, 9, 10 |
| §8 folder add: keys, destination, walk, result routing, notice | 12, 13, 14 |
| §9 rows | 11 |
| §10 tab strip, viewed playlist, keys, overlays | 10, 15 |
| §12 validation incl. pending items | every task; 16 |
| §13 documentation | 17 |

## Known soft spots for the implementer

These are places where the plan names real code it could not compile against. Resolve them by reading the file named, not by guessing:

- Task 4: whether `store.rs` needs any change beyond `SCHEMA_VERSION` — read `load` around :65–:100.
- Task 8: which existing resume tests go through `resume_intent_for` with a local-file fixture.
- Task 12: whether `resolve_path` canonicalizes symlinks (decides how aliases dedupe).
- Task 13: the logging macro `runtime.rs` already uses.
- Task 15: exact expected strings in `tests/m5_tui_render.rs` after the strip replaces ` QUEUE `.
- Task 16: `StateStore`'s write entry point and the checkpoint cap's value.
