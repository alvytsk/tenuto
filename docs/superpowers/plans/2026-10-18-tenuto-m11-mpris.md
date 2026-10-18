# Tenuto M11: MPRIS media controls — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `tenuto tui` and `tenuto play` appear in the desktop's MPRIS widget with title, artist and a working play/pause button.

**Architecture:**
- **Pure model:** `src/application/mpris/mod.rs` is pure and builds on every platform: the snapshot taken from `PlayerView`, the request → `AppCommand` mapping, change detection, and the channel pair.
- **D-Bus side:** `src/application/mpris/bus.rs` (Linux only) runs a `tenuto-mpris` thread with its own current-thread Tokio runtime and an `mpris-server` `Server`.
- **Runtime wiring:** `PlayerRuntime` owns the runtime-side half of the channel. `pump()` drains requests at the start of each pass and publishes a snapshot at the end of every pass.

**Tech Stack:** Rust 1.98.1 (edition 2024), `mpris-server` 0.10 (zbus 5) with the `tokio` feature, `crossbeam-channel`, `tokio::sync::watch`.

**Spec:** `docs/superpowers/specs/2026-10-18-tenuto-m11-mpris-design.md`

## Global Constraints

- Runtime code: no `unsafe`, `unwrap`, `expect`. Tests may use them. Lock poisoning: `lock().unwrap_or_else(PoisonError::into_inner)`.
- Every cargo command uses `--locked`. After Task 5 adds the dependency, `Cargo.lock` is committed with it.
- Dependency: `mpris-server = { version = "0.10", features = ["tokio"] }` under `[target.'cfg(target_os = "linux")'.dependencies]`. No other new crate.
- Bus name `org.mpris.MediaPlayer2.tenuto`; fallback `org.mpris.MediaPlayer2.tenuto.instance<pid>`. `Identity` = `tenuto`.
- `mpris:trackid` = `/org/tenuto/track/<n>`, or `/org/mpris/MediaPlayer2/TrackList/NoTrack` (`TrackId::NO_TRACK`) when `view.now_playing` is `None`.
- Metadata keys: only `mpris:trackid`, `xesam:title`, `xesam:artist`, `mpris:length`. No URL, path, album or art.
- `CanGoNext`, `CanGoPrevious`, `CanSeek`, `CanQuit`, `CanRaise`, `CanSetFullscreen` and `HasTrackList` are false. `CanControl`, `CanPlay` and `CanPause` are true. Rates are 1.0.
- `TENUTO_MPRIS=off` disables media controls; any other value or unset enables them.
- Layering: everything lives under `application` (rank 3); `tests/m9_layering.rs` `LAYERS` is unchanged.
- The tests follow the repo's wait helpers (`pump_until`, `pump_for`, `step`). Never sleep in a runtime test.
- Nothing the D-Bus thread does may block the main thread: `try_send` for requests, `send_replace` for snapshots, no join.

## Review Focus

1. **A click while nothing is loaded** (a fresh profile, empty playlist): no panic, no load, and the widget shows `NoTrack`/Stopped. Test in Task 4.
2. **Quitting while the bus thread hangs in the handshake:** `play` exits normally and promptly. Test in Task 6.
3. **A click flood beyond the request backlog:** the excess is dropped, and neither side blocks. Test in Task 3.
4. **Pause on a radio station that is already paused or stopped:** it stays that way and does not reconnect (`pause_closing` path). Test in Task 1.
5. **Tags carrying NUL or control characters:** the title and artist on the bus contain no NUL. Test in Task 3.

---

### Task 1: `AppCommand::Pause`

**Files:**
- Modify: `src/application/runtime.rs` (`AppCommand` enum ~line 176, `handle` ~line 630)
- Create: `tests/m11_mpris.rs`

**Interfaces:**
- Produces: `AppCommand::Pause`. `handle` routes it with `self.route(PlaybackCommand::Pause)`, not through the transport table. `route` already returns early with no engine, or with no mirror in playlist mode.

- [ ] **Step 1: Write the failing tests in `tests/m11_mpris.rs`**

Use the module header of `tests/m9_3_detached_play.rs` (`mod support; #[path = "support/runtime.rs"] mod runtime;`).

```rust
#[test]
fn a_pause_after_the_engine_paused_does_not_resume() {
    // Rig plays SHORT/FIVE as a playlist row (as m9_3's first test does).
    // pump_until playing.
    rig.runtime.handle(AppCommand::PlayPause);   // engine pauses; event not yet drained
    rig.runtime.handle(AppCommand::Pause);       // stale view still says Playing
    pump_until(&mut rig, "paused", |v| v.phase == PlaybackPhase::Paused);
    pump_for(&mut rig, Duration::from_secs(1));
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Paused);
}

#[test]
fn pause_does_nothing_when_nothing_is_loaded() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Pause);
    step(&mut rig);
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Unloaded);
}

#[test]
fn pause_on_a_paused_station_leaves_it_paused() {
    // Live station on support::server::TestServer, set up as tests/m7_runtime.rs does.
    // Play, pump_until playing, Pause, pump_until not Playing; record phase P.
    rig.runtime.handle(AppCommand::Pause);
    pump_for(&mut rig, Duration::from_secs(2));
    assert_eq!(rig.runtime.view().phase, p);
    assert!(!rig.runtime.view().reconnecting);
    // The server saw no new request after the first pause (count connections).
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test --locked --test m11_mpris`
Expected: compile error, `no variant named Pause`.

- [ ] **Step 3: Add `Pause` to `AppCommand` and its `handle` arm.** If the station test fails, fix the idempotency in the engine's `pause()`/`pause_closing` (`src/playback/engine.rs` ~2965). Don't special-case it in the runtime.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test --locked --test m11_mpris`
Expected: 3 passed.

- [ ] **Step 5: Commit** — `feat(runtime): AppCommand::Pause routes to PlaybackCommand::Pause`

---

### Task 2: `play` shows the decoder's title and artist tags

**Files:**
- Modify: `src/application/runtime.rs` (`Mirror` ~line 276, `Mirror::loaded` ~313, `detached_now_playing` ~1582)
- Modify: `CHANGELOG.md` `[Unreleased]` → `Changed`
- Test: `tests/m9_3_detached_play.rs`

**Interfaces:**
- Produces: `Mirror.artist: Option<String>` (from `metadata.artist` of `PlaybackEvent::Loaded`). `detached_now_playing` sets:
  - `title`: podcast episode → unchanged (`episode_name(mirror.title)`); otherwise `mirror.title` when present, else `display_name(media)`.
  - `artist`: `mirror.artist`.
  - Both strings go through `displayable`.

- [ ] **Step 1: Write the failing test in `tests/m9_3_detached_play.rs`**

```rust
#[test]
fn a_detached_tagged_file_shows_its_tag_title_and_artist() {
    let dir = tempfile::tempdir().unwrap();
    let path = support::tagged_flac::tagged_flac(dir.path(), "Song", "Band", "Album", None);
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(path.to_str().unwrap());
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "plays", playing);
    let now = rig.runtime.view().now_playing.unwrap();
    assert_eq!(now.title, "Song");
    assert_eq!(now.artist.as_deref(), Some("Band"));
}
```

The existing `a_detached_play_shows_its_own_media_and_keeps_the_cursor` (an untagged file → `"sine-5s.flac"`) must still pass.

- [ ] **Step 2: Run it and confirm it fails**

Run: `cargo test --locked --test m9_3_detached_play`
Expected: FAIL, the title is `"tagged.flac"`.

- [ ] **Step 3: Implement it per Interfaces.** Add the CHANGELOG line, hard-wrapped: `tenuto play` shows a local file's title tag (and its artist) instead of the file name.

- [ ] **Step 4: Run it and confirm it passes**

Run: `cargo test --locked --test m9_3_detached_play`
Expected: all pass.

- [ ] **Step 5: Commit** — `feat(play): show the decoder's title and artist tags`

---

### Task 3: The pure MPRIS model

**Files:**
- Create: `src/application/mpris/mod.rs` (unit tests in `#[cfg(test)] mod tests`, as `src/application/view.rs` does)
- Modify: `src/application/mod.rs` (`pub mod mpris;`)

**Interfaces:**
- Produces (all `pub`):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status { Playing, Paused, Stopped }

#[derive(Clone, Debug, PartialEq)]
pub struct Track { pub media: MediaId, pub title: String, pub artist: Option<String>, pub length: Option<Duration> }

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot { pub track: Option<Track>, pub status: Status, pub volume: f64, pub position: Duration }

pub fn snapshot(view: &PlayerView, media: Option<&MediaId>, podcast: Option<&str>) -> Snapshot;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request { PlayPause, Play, Pause, Stop, SetVolume(f64) }

pub fn command(request: Request, volume: Volume) -> Option<AppCommand>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change { Metadata, PlaybackStatus, Volume }

#[derive(Default)]
pub struct Published { /* last: Option<Snapshot>, seq: u64, volume_written: bool */ }
impl Published {
    pub fn apply(&mut self, next: Snapshot) -> Vec<Change>;
    pub fn track_id(&self) -> String;          // "/org/tenuto/track/<seq>" or NO_TRACK path
    pub fn snapshot(&self) -> Option<&Snapshot>;
    pub fn mark_volume_written(&mut self);
}

pub const REQUEST_BACKLOG: usize = 16;
pub struct MediaControls { /* crossbeam Receiver<Request>, watch::Sender<Option<Snapshot>> */ }
impl MediaControls {
    pub fn try_recv(&self) -> Option<Request>;
    pub fn publish(&self, snapshot: Snapshot);  // watch::Sender::send_replace
}
pub struct Remote { pub requests: crossbeam_channel::Sender<Request>, pub snapshots: tokio::sync::watch::Receiver<Option<Snapshot>> }
pub fn channel() -> (MediaControls, Remote);   // requests: bounded(REQUEST_BACKLOG)
```

Rules the tests pin:
- **`snapshot`, track:** `track` is `None` when `view.now_playing` or `media` is `None`.
- **`snapshot`, artist:** `podcast` when it is `Some`, else `now.artist`.
- **`snapshot`, length:** `None` if `view.live`, else `now.duration.map(|d| d.value)`.
- **`snapshot`, strings:** NUL is stripped from the title and artist.
- **`snapshot`, volume and position:** `volume` is `view.volume.as_gain() as f64`; `position` is `now.position`, or zero with no `now_playing`.
- **`snapshot`, status:** Playing, Reconnecting and Loading map to `Playing`; Paused maps to `Paused`; everything else maps to `Stopped`. An active entry with `loaded: false` has the phase `Unloaded`, so it maps to `Stopped` with its track present.
- **`command`:** the four transport requests map 1:1 (`Pause` → `AppCommand::Pause`). `SetVolume(x)`: NaN → `None`; clamp to 0..=1; `delta = x − gain`; `None` if `delta == 0.0`, else `AdjustVolume(delta)`.
- **`Published::apply`:**
  - The first call returns all three changes.
  - `seq` increments whenever `track.media` differs from the last one (including `None` ↔ `Some`).
  - `Metadata` is reported when `track` differs in any field, so the same media with a new length or artist reports `Metadata` and keeps `seq`.
  - `PlaybackStatus` is reported when `status` differs.
  - `Volume` is reported when `volume` differs or `volume_written` was set; `apply` clears the flag.
  - A position-only difference returns no changes, but the stored snapshot (and so `snapshot().position`) is updated.

- [ ] **Step 1: Write the failing unit tests** (one `#[test]` per bullet):
  - `snapshot_maps_each_phase` covers all 8 phases.
  - `snapshot_prefers_podcast_name_over_tag_artist`
  - `snapshot_omits_length_for_live_and_unknown_durations`
  - `snapshot_strips_nul_from_title_and_artist`: the title `"a\0b"` becomes `"ab"`.
  - `snapshot_without_now_playing_has_no_track`
  - `command_maps_transport_requests` and `command_volume_rejects_nan_and_clamps`: `SetVolume(1.5)` at gain 1.0 → `None`; `SetVolume(0.25)` at 0.75 → `AdjustVolume(-0.5)`.
  - `first_apply_reports_everything`
  - `position_only_change_reports_nothing_but_updates_position`
  - `new_media_bumps_track_id`
  - `same_media_new_length_keeps_track_id_and_reports_metadata`
  - `volume_write_reports_volume_even_when_unchanged`
  - `track_id_is_no_track_without_a_track`
  - `click_flood_drops_beyond_backlog`: 20 `try_send`s on `Remote.requests` → exactly 16 `Ok`, and `MediaControls::try_recv` yields 16, then `None`.

Build a `PlayerView` by hand (all its fields are `pub`; start `now_playing` from `NowPlaying::unloaded()`), through one local `fn view(phase, now) -> PlayerView` helper in the test module.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test --locked --lib application::mpris`
Expected: compile errors (the items don't exist).

- [ ] **Step 3: Implement `mod.rs` per Interfaces and rules.**

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test --locked --lib application::mpris`
Expected: all pass.

- [ ] **Step 5: Commit** — `feat(mpris): pure snapshot, request and change model`

---

### Task 4: Runtime wiring

**Files:**
- Modify: `src/application/runtime.rs`
- Modify: `src/application/podcast.rs`
- Test: `tests/m11_mpris.rs`

**Interfaces:**
- Consumes: Task 3's `MediaControls`, `channel`, `snapshot`, `command`; Task 1's `AppCommand::Pause`.
- Produces:
  - `pub fn podcast_name(subs: &SubscriptionStore, cache: &CacheStore, media: &MediaId) -> Option<String>` in `podcast.rs`: the subscription's `title`, else the cached feed's `title`. It reads the files the way `podcast_artwork` does.
  - `PlayerRuntime::attach_media_controls(&mut self, controls: MediaControls)` (`pub`; the tests use it with `channel()`).
  - New private fields: `media_controls: Option<MediaControls>` and `podcast_name: Option<(MediaId, Option<String>)>`. The second is a cache, refreshed only when the playing media changes.
  - **`pump()` start:** drain `try_recv` into a local `Vec`, then call `handle(cmd)` for each `command(req, self.volume)` that is `Some`.
  - **`pump()` end:** publish `snapshot(&self.view(), media, podcast)`, where `media` is `self.detached` else the playing playlist's active entry media. Publishing happens on every pass, including the passes that return early with no engine; restructure the early `return` so it does. Skip it entirely when `media_controls` is `None`, so runtime suites pay nothing.

- [ ] **Step 1: Write the failing tests in `tests/m11_mpris.rs`**

```rust
#[test]
fn a_play_pause_request_reaches_the_runtime() {
    // rig + enqueue SHORT + PlayEntry + pump_until playing (as Task 1)
    let (controls, remote) = tenuto::application::mpris::channel();
    rig.runtime.attach_media_controls(controls);
    remote.requests.try_send(Request::PlayPause).unwrap();
    pump_until(&mut rig, "paused", |v| v.phase == PlaybackPhase::Paused);
    let snap = remote.snapshots.borrow().clone().unwrap();
    assert_eq!(snap.status, Status::Paused);
}

#[test]
fn the_snapshot_describes_the_playing_track() {
    // tagged_flac("Song","Band",…) enqueued as a Path, played; attach; step
    // assert track.title == "Song", artist == Some("Band"), length is Some
}

#[test]
fn a_click_with_nothing_loaded_does_nothing() {
    let mut rig = rig_with(PersistedState::default());
    let (controls, remote) = tenuto::application::mpris::channel();
    rig.runtime.attach_media_controls(controls);
    remote.requests.try_send(Request::Play).unwrap();
    pump_for(&mut rig, Duration::from_millis(500));
    let snap = remote.snapshots.borrow().clone().unwrap();
    assert!(snap.track.is_none());
    assert_eq!(snap.status, Status::Stopped);
}

#[test]
fn a_podcast_episode_shows_the_podcast_name_as_artist() {
    // rig_with_library + support::feeds seed a feed titled "Radio-T"
    // enqueue the episode, attach, step: artist == Some("Radio-T")
}

#[test]
fn position_advances_in_successive_snapshots() {
    // playing, attach; record position; pump_for 1 s; newer position > older
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test --locked --test m11_mpris`
Expected: compile error, `attach_media_controls` not found.

- [ ] **Step 3: Implement it per Interfaces.**

- [ ] **Step 4: Run them, plus the runtime suites, and confirm they pass**

Run: `cargo test --locked --test m11_mpris --test m5_runtime --test m7_runtime --test m9_3_detached_play`
Expected: all pass.

- [ ] **Step 5: Commit** — `feat(runtime): drain MPRIS requests and publish snapshots each pass`

---

### Task 5: The D-Bus thread

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`
- Create: `src/application/mpris/bus.rs` (`#[cfg(target_os = "linux")] mod bus;` in `mod.rs`)
- Modify: `src/application/mpris/mod.rs`

**Interfaces:**
- Consumes: Task 3's `Remote`, `Published`, `Request`, `Change`.
- Produces, in `mod.rs` (built on every platform):

```rust
#[derive(Clone, Copy, Debug)]
pub enum FailureLog { Warn, Debug }
/// `None` when `TENUTO_MPRIS=off`, off Linux, or the thread could not be spawned.
pub fn spawn(failure: FailureLog) -> Option<MediaControls>;
```

- **On Linux, `spawn`:** calls `channel()`, then starts a thread named `tenuto-mpris` with `std::thread::Builder` that runs `bus::serve(remote, failure)`, and returns the `MediaControls` at once. A spawn error is logged per `failure` and gives `None`.
- **`bus::serve`:**
  - Builds a current-thread Tokio runtime and `block_on`s the rest.
  - Calls `Server::new("tenuto", imp)`; on error, `Server::new(&format!("tenuto.instance{}", std::process::id()), imp)`. A second error is logged once per `failure` (`tracing::warn!` or `tracing::debug!`), then it returns.
  - Loop: `while remote.snapshots.changed().await.is_ok()`, clone the borrowed snapshot, `apply` it under the lock, map the `Change`s to `Property::Metadata` / `PlaybackStatus` / `Volume` values, and send them with `server.properties_changed`. An error there is logged once, then it returns. The connection doesn't come back.
  - It returns when the runtime drops `MediaControls` (`changed()` errs), which drops the server and releases the name.
- **`Imp { published: Arc<Mutex<Published>>, requests: Sender<Request> }`** implements `RootInterface` and `PlayerInterface`:
  - Getters read `published`. `metadata()` builds `trackid`/`title`/`artist`/`length` from `snapshot().track` and `track_id()`.
  - `play_pause`/`play`/`pause`/`stop` `try_send` their `Request`; a full channel is ignored.
  - `set_volume(v)`: NaN → `Err` carrying `fdo::Error::InvalidArgs`. Otherwise it calls `mark_volume_written()`, then `try_send(SetVolume(v))`.
  - `next`, `previous`, `seek`, `set_position`, `open_uri`, `raise`, `quit`, `set_rate`, `set_loop_status`, `set_shuffle` and `set_fullscreen` return `Ok(())`.
  - `loop_status` returns `None`, `shuffle` returns false, and `desktop_entry` returns `Err(fdo::Error::NotSupported)`.
- The optional `#[ignore]`d `dbus-run-session` round-trip test from spec §7 is **not** added (my call): Task 6's subprocess test and the manual acceptance cover the bus path.

- [ ] **Step 1: Add the dependency** and run `cargo tree --locked -e features -i zbus | head -30`. Record in the commit message whether `async-io` is pulled in (spec §4.1). `mpris-server` 0.10 depends on zbus with default features, so expect it.

- [ ] **Step 2: Write the failing unit tests in `bus.rs`'s `#[cfg(test)] mod tests`.** They call `Imp` methods directly on a `tokio::runtime::Builder::new_current_thread()` runtime, with no bus:
  - `set_volume_rejects_nan`: `Err`, nothing queued.
  - `set_volume_queues_and_marks_written`: one `Request::SetVolume(0.5)` is received, and the next `apply` reports `Change::Volume`.
  - `pause_queues_pause_not_toggle`
  - `navigation_and_seek_are_ignored`: nothing queued; `can_go_next`, `can_go_previous` and `can_seek` are all false.
  - `position_reads_the_latest_snapshot`: apply two snapshots that differ only in position; `position()` returns the second.
  - `metadata_has_no_track_without_a_track`: `trackid` == `TrackId::NO_TRACK`.

- [ ] **Step 3: Run them and confirm they fail**

Run: `cargo test --locked --lib application::mpris::bus`
Expected: compile errors.

- [ ] **Step 4: Implement `bus.rs` and `spawn` per Interfaces.**

- [ ] **Step 5: Run them, and the full lint, and confirm both pass**

Run: `cargo test --locked --lib application::mpris && cargo clippy --locked --all-targets --all-features -- -D warnings`
Expected: pass, no warnings.

- [ ] **Step 6: Commit** — `feat(mpris): D-Bus thread on mpris-server`

---

### Task 6: Front ends, the off switch, process tests and docs

**Files:**
- Modify: `src/application/runtime.rs` (`enable_media_controls`)
- Modify: `src/app.rs` (after `runtime.play_detached`, ~line 206)
- Modify: `src/tui/mod.rs` (in `run_loop` or just before it, after the fd-2 redirect)
- Modify: `tests/support/process.rs` (`command_in`) and `tests/support/pty.rs` (`command`)
- Create: `tests/m11_mpris_process.rs`
- Modify: `docs/architecture.md` §4 table and §5 thread table; `docs/reference.md`; `CHANGELOG.md` `[Unreleased]` → `Added`
- Create: `docs/m11-acceptance.md`

**Interfaces:**
- Consumes: Task 5's `spawn` and `FailureLog`; Task 4's `attach_media_controls`.
- Produces: `PlayerRuntime::enable_media_controls(&mut self, failure: FailureLog)`, which calls `attach_media_controls` when `spawn` returns `Some`. `play` passes `FailureLog::Debug`, `tui` passes `FailureLog::Warn`.
- Harness: `command_in` adds `.env("TENUTO_MPRIS", "off")`. `pty::command` sets `TENUTO_MPRIS=off` before applying the test's `env`, so a test can override it. Process tests that want it on call `.env_remove("TENUTO_MPRIS")`.

- [ ] **Step 1: Write the failing process tests in `tests/m11_mpris_process.rs`** (`#![cfg(target_os = "linux")]`, using `support::process::Profile`):

```rust
#[test]
fn play_runs_and_exits_with_an_unresponsive_bus() {
    // UnixListener at tmp/bus; a thread accepts and keeps every stream, never reads.
    // profile.command().args(["play", SHORT_FIXTURE]).env("TENUTO_AUDIO_OUTPUT","null")
    //   .env_remove("TENUTO_MPRIS").env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={sock}"))
    // spawn; wait_exit (copy m5_signals_play's helper) with 20 s patience
    assert!(status.success());
    assert!(accepted.load(SeqCst) >= 1, "media controls tried the bus");
    assert!(!stderr.to_lowercase().contains("mpris"));
}

#[test]
fn play_without_a_bus_writes_nothing_new_to_stderr() {
    // baseline: same command with TENUTO_MPRIS=off → stderr lines B
    // run: env_remove TENUTO_MPRIS, DBUS_SESSION_BUS_ADDRESS=unix:path=<tmp>/nonexistent
    assert!(run.status.success());
    assert_eq!(run_stderr.lines().count(), baseline_stderr.lines().count());
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test --locked --test m11_mpris_process`
Expected: the first test fails on `accepted >= 1`, since nothing connects yet.

- [ ] **Step 3: Implement** `enable_media_controls`, the two front-end calls and the harness defaults.

- [ ] **Step 4: Run them and confirm they pass**

Run: `cargo test --locked --test m11_mpris_process`
Expected: 2 passed.

- [ ] **Step 5: Write the docs:**
  - **`architecture.md`:** an `application::mpris` row in §4 (responsibility: map `PlayerView` to MPRIS and MPRIS requests to `AppCommand`s; must not: touch the engine or block the main thread). A `tenuto-mpris` row in §5 (owns: the bus connection and its current-thread Tokio runtime; must not: call into the runtime or `Session`).
  - **`reference.md`:** a "Desktop media controls" section with what shows per media kind (spec §5.1), that only play/pause/stop work, `TENUTO_MPRIS=off`, and Linux only.
  - **`CHANGELOG.md` `Added`:** the MPRIS entry.
  - **`docs/m11-acceptance.md`:** the manual checklist from spec §7, verbatim, with unchecked boxes, and a section for the automated evidence (test names from Tasks 1–6).

- [ ] **Step 6: Full verification**

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```
Expected: all clean. `tests/m4_diagnostics.rs` and `tests/m9_layering.rs` pass unchanged.

- [ ] **Step 7: Commit** — `feat(m11): MPRIS media controls in tui and play`
