# Tenuto M11 addendum: MPRIS cover art — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The MPRIS widget shows the playing media's cover through a `file://` PNG in tenuto's cache.

**Architecture:**
- **Model** (`src/application/mpris/cover.rs`): a pointer-compared `CoverImage`, the PNG writer, and a runtime-side `CoverFetcher` wrapping an `ArtworkWorker`.
- **Runtime:** asks the fetcher for a cover only when the playing media, its load or the HTTP gate changes, and attaches the result to each published `Snapshot`.
- **Bus thread:** writes each new cover to `<cache>/tenuto/mpris/cover-<n>.png` and publishes `mpris:artUrl`.

**Tech Stack:** Rust 1.98.1, the `image` crate (jpeg and png features, already a dependency), `artwork::ArtworkWorker` and `default_loader`, `url::Url::from_file_path`, `directories`.

**Spec:** `docs/superpowers/specs/2026-10-18-tenuto-m11-mpris-design.md`, §10 (the addendum); §1–§9 still bind.

## Global Constraints

- Runtime code: no `unsafe`, `unwrap`, `expect`. Tests may use them. Lock poisoning: `lock().unwrap_or_else(PoisonError::into_inner)`.
- Every cargo command uses `--locked`. No new crate.
- `mpris:artUrl` is only ever `Url::from_file_path(<cache>/tenuto/mpris/cover-<n>.png)`. A remote URL or a music-file path must never reach the bus.
- Cover edge cap: `COVER_EDGE: u32 = 512` on the long edge, aspect ratio kept, PNG encoding.
- File writes and image encoding happen on the `tenuto-mpris` thread or the artwork worker, never on the main thread.
- `active_cover`/`cover_for` read store files: the runtime calls them only when its cover key changes, never on every pass.
- A cover is compared by `Arc::ptr_eq`, never by pixels.
- Layering is unchanged: everything sits under `application` (rank 3), which may import `artwork` (rank 3). Leave `tests/m9_layering.rs` as it is.
- Runtime tests wait with `pump_until`, `pump_for` or `step`. Never sleep.
- Commit messages carry no Co-authored-by or Claude trailers.

## Review Focus

1. **A cover that fails to decode or fetch** (corrupt tag, 404 logo): no `artUrl`, no stale cover from the previous track. Test in Task 8.
2. **Switching tracks quickly:** the previous track's late-arriving cover must not attach to the new track (worker results are matched by media). Test in Task 8.
3. **A cache directory that can't be created or written** (read-only cache): no `artUrl`, one debug log line, controls keep working. Test in Task 9.
4. **Old `cover-*.png` files left from a crash:** cleared at the next start. Test in Task 7.
5. **A huge embedded cover** (e.g. 3000×3000): the written PNG is at most 512 px on the long edge. Test in Task 7.

---

### Task 7: Cover model and PNG writer

**Files:**
- Create: `src/application/mpris/cover.rs` (`pub mod cover;` in `mpris/mod.rs`); unit tests in-file.
- Modify: `src/application/mpris/mod.rs` (`Snapshot`, `Published`)

**Interfaces:**
- Produces in `cover.rs`, built on every platform:

```rust
pub const COVER_EDGE: u32 = 512;
#[derive(Clone)]
pub struct CoverImage(pub Arc<image::DynamicImage>);   // PartialEq = Arc::ptr_eq; Debug prints dimensions only
pub fn write_cover(dir: &Path, seq: u64, image: &image::DynamicImage) -> std::io::Result<PathBuf>;
pub fn clear_covers(dir: &Path);                        // removes cover-*.png and cover-*.png.tmp; ignores errors
```

`write_cover` does the following, in order:
- `create_dir_all`;
- shrinks the image with `thumbnail(COVER_EDGE, COVER_EDGE)`, only if either edge is larger;
- encodes a PNG into `cover-<seq>.png.tmp`;
- renames that to `cover-<seq>.png` and returns that path.

An encoding error is mapped to `io::Error::other`.

- In `mod.rs`:
  - `Snapshot` gains `pub cover: Option<CoverImage>`. `snapshot()` sets it to `None`; the runtime fills it in (Task 8).
  - `Published::apply` also reports `Change::Metadata` when `cover` differs, without bumping `seq` when the media is unchanged.
  - New items:

```rust
pub enum CoverUpdate { Unchanged, Cleared, New { seq: u64, image: CoverImage } }
impl Published {
    pub fn take_cover_update(&mut self) -> CoverUpdate;  // reports each cover change once; `seq` from its own counter, starting at 1
    pub fn set_art_url(&mut self, url: Option<String>);
    pub fn art_url(&self) -> Option<&str>;
}
```

- [ ] **Step 1: Write the failing unit tests**
  - `cover.rs`:
    - `cover_images_compare_by_pointer`: two `Arc`s of identical pixels are unequal; clones of one `Arc` are equal.
    - `write_cover_caps_the_long_edge_and_keeps_aspect`: a 2000×1000 input gives a 512×256 PNG that `image::open` can read, and no `.tmp` file remains.
    - `write_cover_keeps_a_small_image_size`: 100×80 stays 100×80.
    - `clear_covers_removes_only_cover_files`: `cover-3.png`, `cover-4.png.tmp` and `keep.txt` leave only `keep.txt`.
  - `mod.rs`:
    - `same_media_new_cover_reports_metadata_and_keeps_track_id`
    - `take_cover_update_reports_each_change_once`: New{seq:1}, then Unchanged, then a new Arc gives New{seq:2}, then `cover: None` gives Cleared, then Unchanged.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test --locked --lib application::mpris`
Expected: compile errors.

- [ ] **Step 3: Implement per Interfaces.** Update the existing `snap(...)` test helper and every `Snapshot { .. }` literal with `cover: None`.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test --locked --lib application::mpris && cargo clippy --locked --all-targets --all-features -- -D warnings`
Expected: pass, no warnings.

- [ ] **Step 5: Commit** — `feat(mpris): cover image model and PNG writer`

---

### Task 8: Runtime cover fetching

**Files:**
- Modify: `src/application/mpris/cover.rs` (`CoverFetcher`)
- Modify: `src/application/runtime.rs` (`attach_media_controls`, `publish_snapshot`, a new `playing_cover`)
- Test: `tests/m11_mpris.rs`

**Interfaces:**
- Consumes: Task 7's `CoverImage`; `artwork::worker::{ArtworkWorker, CoverSource, CoverLoader, default_loader}`.
- Produces:

```rust
pub struct CoverFetcher { /* worker, requested: Option<(MediaId, CoverSource)>, current: Option<(MediaId, CoverImage)> */ }
impl CoverFetcher {
    pub fn new(loader: CoverLoader) -> Self;
    /// `wanted` changed: request it (or clear on None). Drops a cover whose media differs from `wanted`'s.
    pub fn want(&mut self, wanted: Option<(MediaId, CoverSource)>);
    /// Drain finished jobs; keep a result only if its media is the requested one (Ok → current, Err → None).
    pub fn poll(&mut self);
    pub fn cover_for(&self, media: &MediaId) -> Option<CoverImage>;
}
```

- **Runtime fields:**
  - `covers: Option<CoverFetcher>`, built in `attach_media_controls` with `default_loader(TestHook::None)`;
  - `cover_wanted_key: Option<(Option<MediaId>, Option<LoadRequestId>, bool)>`, holding the playing media, `mirror.load` and `http.is_some()`.
- **`publish_snapshot`:**
  - computes the cheap key every pass and calls `covers.want(self.playing_cover())` only when the key changes;
  - calls `covers.poll()` every pass;
  - sets `snap.cover = media.and_then(|m| covers.cover_for(&m))`.
- **`fn playing_cover(&self) -> Option<(MediaId, CoverSource)>`:**
  - Detached `MediaId::LocalFile(path)`: `CoverSource::Local(path)`.
  - Other detached media: the mirror's `front_cover` as `CoverSource::Embedded`, if its media matches.
  - Otherwise: `self.active_cover()`.
- **`ponytail:` comment** on `CoverFetcher`: in `tui`, a remote cover is fetched again here beside `tui::Artwork`. If that matters, share the fetch by having the TUI hand its decoded image to the runtime.

- [ ] **Step 1: Write the failing tests in `tests/m11_mpris.rs`.** Copy the `png_bytes()` helper from `tests/m7_1_station_artwork.rs:72`.

```rust
#[test]
fn a_playlist_track_with_an_embedded_picture_publishes_a_cover() {
    // tagged_flac(dir, "Song", "Band", "Album", Some(&png_bytes())); enqueue Path; PlayEntry; attach channel()
    pump_until(&mut rig, "cover", |_| remote.snapshots.borrow().as_ref().is_some_and(|s| s.cover.is_some()));
}

#[test]
fn a_detached_track_with_an_embedded_picture_publishes_a_cover() { /* play_detached same file; same wait */ }

#[test]
fn an_untagged_track_publishes_no_cover() {
    // play SHORT (sine.flac); pump_until playing; pump_for 1 s; assert snapshot.cover.is_none()
}

#[test]
fn a_late_cover_does_not_attach_to_the_next_track() {
    // enqueue tagged-with-picture A then untagged B; play A, then immediately PlayEntry(B) before pumping;
    // pump_until B playing; pump_for 1 s; assert snapshot.track.media == B and snapshot.cover.is_none()
}

#[test]
fn a_cover_that_fails_to_decode_publishes_none() {
    // tagged_flac with Some(b"\x89PNG\r\n\x1a\ngarbage"); play; pump_until playing; pump_for 1 s; cover.is_none()
}
```

`pump_until` takes `Fn(&PlayerView) -> bool`. The cover predicate ignores the view and reads `remote.snapshots`. That is fine, since `pump_until` steps the runtime between checks.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test --locked --test m11_mpris`
Expected: the first two time out at the "cover" wait (or fail to compile until `Snapshot.cover` exists, which Task 7 adds).

- [ ] **Step 3: Implement per Interfaces.**

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test --locked --test m11_mpris --test m5_runtime --test m9_3_detached_play --test m5_artwork`
Expected: all pass.

- [ ] **Step 5: Commit** — `feat(runtime): fetch the playing cover for media controls`

---

### Task 9: Bus `artUrl`, cache directory and docs

**Files:**
- Modify: `src/application/mpris/bus.rs`, `src/application/mpris/mod.rs` (`spawn` signature), `src/application/runtime.rs` (`enable_media_controls`)
- Modify: `docs/reference.md`, `docs/architecture.md`, `CHANGELOG.md`, `docs/m11-acceptance.md`

**Interfaces:**
- Consumes: Task 7's `write_cover`, `clear_covers`, `CoverUpdate`, `Published::{take_cover_update, set_art_url, art_url}`.
- Produces:
  - **`spawn(failure, cover_dir: Option<PathBuf>)`:** `None` means no covers at all.
  - **`enable_media_controls(&mut self, failure)`:** keeps its signature and computes `directories::ProjectDirs::from("", "", "tenuto").map(|d| d.cache_dir().join("mpris"))` itself. The front-end call sites don't change.
  - **In `bus.rs`:**

```rust
/// Apply one pending cover update: New → write_cover, delete `previous`, set art_url to the file:// URL;
/// Cleared → delete `previous`, art_url None; write error → debug log via `failure`, art_url None.
fn write_pending_cover(dir: &Path, published: &Mutex<Published>, previous: &mut Option<PathBuf>, failure: FailureLog);
```

  - **The serve loop:**
    - calls `clear_covers(dir)` once before connecting;
    - after each `apply`, calls `write_pending_cover` before emitting, with no lock held during the file write;
    - `metadata()` adds `.art_url(url)` when `art_url()` is `Some`;
    - on exit, deletes the last written file (best effort).

- [ ] **Step 1: Write the failing unit tests in `bus.rs`**, using a tempdir:
  - `a_new_cover_is_written_and_published_as_a_file_url`: the file exists, `art_url` == `Url::from_file_path(path)`, and `metadata()` carries `mpris:artUrl`.
  - `the_next_cover_deletes_the_previous_file`
  - `a_cleared_cover_deletes_the_file_and_drops_art_url`
  - `an_unwritable_cover_dir_leaves_no_art_url`: point `dir` at a path under a regular file.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test --locked --lib application::mpris::bus`
Expected: compile errors.

- [ ] **Step 3: Implement per Interfaces.**

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test --locked --lib application::mpris && cargo test --locked --test m11_mpris_process`
Expected: all pass.

- [ ] **Step 5: Docs**
  - `reference.md` Desktop media controls: covers per media kind (spec §10.2), the cache path, and that `play` covers music files only.
  - `architecture.md`: the `tenuto-mpris` row now also writes cover PNGs into the cache; the runtime's cover worker is listed beside the `tenuto-artwork` worker. Check the thread name `ArtworkWorker::spawn` uses and name it.
  - `CHANGELOG.md` `[Unreleased]` Added: extend the MPRIS entry with covers.
  - `docs/m11-acceptance.md`: add the spec §10.4 manual check and the new test names.

- [ ] **Step 6: Full verification**

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```
Expected: all clean.

- [ ] **Step 7: Commit** — `feat(mpris): publish the cover as mpris:artUrl`
