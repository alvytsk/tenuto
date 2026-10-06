# M9.3 `play` over `PlayerRuntime` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `tenuto play` becomes a thin front end over `PlayerRuntime` that never touches the saved playlists. This deletes `app.rs`'s duplicate mirror, its two event loops, its persistence opener and `LoadTarget::Legacy`.

**Architecture:** The session gains a *detached* load target. Its `Loaded` adopts the media but leaves the playing playlist and its cursor alone. The runtime gains `play_detached(media, location)`: a detached load forwards transport keys straight to the engine instead of through the playlist decision table, and it builds `NowPlaying` from the mirror instead of from the active entry. `tui` and `play` share one state opener in `application::profile`. `play` keeps its keys, status line, `Loading …` line, signals, exit codes and no-tty behavior. It drives `runtime.handle` / `pump` / `view` / `shutdown` instead of the engine.

**Tech Stack:** Rust 1.98.1, crossterm (raw mode, keys), the existing runtime test rig (`tests/support/runtime.rs`, virtual clock), subprocess tests (`tests/support/process.rs`).

**Spec:** `docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` §4 ("One adopted-playback mirror": its `play` defects and the shared opener) and §5 ("`play` over `PlayerRuntime`"). Decisions made with the user on 2026-10-05:
- `play` is **detached**. It never adds a row, never moves a cursor, and never changes which playlist is playing. It still writes its own checkpoint (resume position) and the volume.
- `play` runs **over `PlayerRuntime`**, keeping its documented keys (`docs/reference.md`: space, arrows, `s`, `p`, `q`; also Home, `-`/`+`, Ctrl-C).

## Global Constraints

- Every cargo command uses `--locked`; toolchain 1.98.1.
- Runtime code has no `unsafe`, `unwrap` or `expect` (tests may use them).
- Every URL in a message or log goes through `redact_url`. Status text from the runtime is already escaped by `displayable`; don't escape it twice.
- Layering: `tui` mutates state only through `Session`; only `session` builds `PersistedState` snapshots. `application` must not import `commands` in new code. (`displayable` is still imported from `commands` elsewhere; that move is out of scope.)
- Runtime-rig tests wait with `pump_until`, `pump_for` or `step`, never `runtime.pump()` plus a sleep.
- Behavior `play` must keep (the subprocess suites `cli_playback`, `m5_signals_play`, `m5_lock_process`, `http_cli`, `m4_cli`, `m4_diagnostics` pin most of it):
  - signals are installed first, then the profile lock, then state is loaded;
  - a rejected source is reported before the lock is ever contended;
  - `Loading …\r\n` is printed before anything loads;
  - a failure (load or mid-play) exits non-zero as `tenuto: <message>`;
  - with no tty there are no keys, and the run ends 0 when the track ends;
  - a signal wins over any other outcome (exit `128+N`);
  - the terminal is restored before the final flush is waited on.
- Commits carry no `Co-authored-by` or tool attribution.

## Review Focus

1. **`play` of a media that is also a row in the playing playlist.** The cursor must stay where it was. The checkpoint, keyed by media, is updated, which is correct: the row resumes there later. *Pinned in Task 1:* `a_detached_load_of_a_queued_media_keeps_the_cursor_and_updates_its_checkpoint`.
2. **A track that ends during a detached play** must not load the next playlist row. *Pinned in Task 1* (`take_advance` is `None`) and *Task 3* (`a_detached_track_that_ends_loads_nothing_else`).
3. **A failure after the track loaded** (a reconnect budget exhausted, a device lost for good) must still end `play` non-zero with the message. Runtime's `phase()` maps a mirrored `Failed` state to `Stopped`, not `LoadFailed`, so the front end has to read `now_playing.state`. *Pinned in Task 4:* `a_failure_after_loading_ends_the_run_with_its_message` (pure `outcome` test).
4. **`s` then `p`** resumes at the logical position: stopping must never reset the position. *Pinned in Task 3:* `stop_then_play_resumes_a_detached_track_where_it_stopped`.
5. **Space after the track ended** keeps today's engine answer: a warning, and no implicit restart. Home restarts. The detached table forwards both raw, exactly as `app.rs` does now. *Pinned in Task 2* (pure table test) and *Task 3* (`home_restarts_an_ended_detached_track`).

Added after plan review (2026-10-05). Each was verified against the code:

6. **A seek while stopped or ended, with an empty playing playlist.** `seeking_allowed()` (runtime.rs:806) gates `KeyRouter::flush` on the *playlist* table, which answers `QUEUE_EMPTY` there. A detached burst would then never flush, and once its deadline passes `poll_budget()` returns zero on every pass, busy-polling. The flush gate must use the same table as `transport`. *Pinned in Task 3:* `a_detached_seek_while_stopped_flushes_with_an_empty_playlist`.
7. **Keys pressed before `Loaded`.** Today `play`'s mirror exists from the start, so Home or an arrow during a slow load is routed and queued behind the `Load`. The runtime's `route()` returns early without a mirror, which would silently drop them, and Home would start at the saved position instead of restarting. A detached `route()` must reach the engine without a mirror. *Pinned in Task 3:* `home_while_a_detached_load_is_pending_restarts_it`.
8. **Stop keeps a stored seek target.** `AppCommand::Stop` calls `router.cancel()`, which clears `stored`; today's `play` routes Stop through `KeyRouter::route` → `drop_burst()`, which keeps it (the worker keeps its pending target across a stop). After Stop → seek → Stop the display falls back to the heard position and the next arrow accumulates from the wrong base. Stop goes through the router in both front ends; the TUI has the same defect today and is fixed with it. *Pinned in Task 3:* `stop_keeps_a_stored_seek_target_for_the_next_arrow`.

---

## File Structure

- `src/session.rs`: `LoadTarget::Legacy` → `LoadTarget::Detached`; its `Loaded` arm no longer clears the cursor.
- `src/playlist/set.rs`: delete `clear_playing_cursor` (no callers left).
- `src/application/profile.rs` (**new**): `open_state`, the one opener of `state.json` into a `Session` + `WriterHandle` for both front ends, plus its load logging.
- `src/application/mod.rs`: `pub mod profile;`.
- `src/application/transport.rs`: `decide_detached`, the detached transport table (pure).
- `src/application/runtime.rs`: `play_detached`, `detached` field, `submit_load` (extracted from `load_entry_starting`), detached `NowPlaying`, mirror `title`, `poll_budget`.
- `src/tui/mod.rs`: `start_runtime` uses `profile::open_state`.
- `src/app.rs`: the `play` loop over the runtime. `Mirror`, `apply_progress`, `Phase`, `finish`, `handle_keys`, `open_persistence`, `Persistence`, `resume_commands`, `submit` and `report_flush` (moved to the shared `FlushReport` logging) are deleted. The status line is rebuilt from `NowPlaying`.
- Tests: `tests/m9_3_detached_play.rs` (**new**, runtime rig), `tests/m9_3_play_leaves_playlists.rs` (**new**, subprocess), unit tests in `session.rs`/`transport.rs`/`profile.rs`/`app.rs`. Mechanical `Legacy` → `Detached` rename in about 20 existing test call sites.
- Docs: `docs/reference.md`, `docs/architecture.md` (§4 table, §12), the roadmap statuses, `CHANGELOG.md`, `docs/m9.3-acceptance.md` (**new**).

---

### Task 1: Session — a detached load leaves the playlists alone

**Files:**
- Modify: `src/session.rs:77-85` (enum), `:411` (`load_target_for` arm), `:610` (doc), `:1052-1055` (`Loaded` arm)
- Modify: `src/playlist/set.rs:324-331` (delete `clear_playing_cursor`)
- Modify (rename only): every `LoadTarget::Legacy` in `src/app.rs`, `tests/http_protocol.rs`, `tests/http_resume.rs`, `tests/http_playback.rs`, `tests/estimated_resume.rs`, `tests/m5_session_adoption.rs`, `tests/m4_playback_identity.rs`, `tests/m7_session.rs`
- Test: `tests/m9_3_detached_session.rs` (**new**)

**Interfaces:**
- Produces: `pub enum LoadTarget { Queue(QueueEntryId), Detached }`. A `Detached` adoption sets `current_media` and checkpoints as before, but never changes `playlists().playing()` or any cursor, and never sets an `Advance`.

- [ ] **Step 1: Write the failing tests**

`tests/m9_3_detached_session.rs`:

```rust
//! M9.3: a detached load (`tenuto play`) adopts its media without touching
//! the saved playlists.

use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueEntryId, QueueSource};
use tenuto::session::{LoadTarget, Session};

fn media(name: &str) -> MediaId {
    MediaId::LocalFile(AbsolutePath::new(format!("/music/{name}.flac").into()).expect("abs"))
}

fn entry(name: &str) -> NewQueueEntry {
    let path = AbsolutePath::new(format!("/music/{name}.flac").into()).expect("abs");
    NewQueueEntry::new(
        MediaId::LocalFile(path.clone()),
        QueueSource::LocalFile(path),
        DisplayMetadata::default(),
    )
    .expect("valid")
}

fn capabilities() -> MediaCapabilities {
    MediaCapabilities {
        continuity: Continuity::Finite,
        seek: SeekSupport::Native,
        ..MediaCapabilities::default()
    }
}

fn loaded(session_rev: u64, request: tenuto::playback::command::LoadRequestId, media: MediaId) -> PlaybackEvent {
    PlaybackEvent::Loaded {
        session_rev,
        request,
        media,
        metadata: MediaMetadata::default(),
        capabilities: capabilities(),
        position: Duration::ZERO,
        disposition: StartDisposition::Fresh,
    }
}

/// A playing playlist of a, b, c with b adopted through a queue load.
fn with_b_active(clock: &FakeClock) -> (Session, Vec<QueueEntryId>) {
    let mut session = Session::new(PersistedState::default());
    let playing = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(playing, vec![entry("a"), entry("b"), entry("c")])
        .expect("fits");
    let request = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("registered");
    session.observe(&loaded(1, request, media("b")), clock.sample());
    assert_eq!(session.state().playlists().playing_playlist().queue().active(), Some(ids[1]));
    (session, ids)
}

#[test]
fn a_detached_load_keeps_the_playing_playlist_and_its_cursor() {
    let clock = FakeClock::new();
    let (mut session, ids) = with_b_active(&clock);
    let playing = session.state().playlists().playing();
    let request = session
        .register_load(LoadTarget::Detached, &media("elsewhere"))
        .expect("registered");
    session.observe(&loaded(2, request, media("elsewhere")), clock.sample());

    assert_eq!(session.adopted().map(|load| load.target), Some(LoadTarget::Detached));
    assert_eq!(session.state().playlists().playing(), playing);
    assert_eq!(session.state().playlists().playing_playlist().queue().active(), Some(ids[1]));
}

#[test]
fn a_detached_load_of_a_queued_media_keeps_the_cursor_and_updates_its_checkpoint() {
    let clock = FakeClock::new();
    let (mut session, ids) = with_b_active(&clock);
    let request = session
        .register_load(LoadTarget::Detached, &media("c"))
        .expect("registered");
    session.observe(&loaded(2, request, media("c")), clock.sample());
    clock.advance(Duration::from_secs(1));
    session.observe(
        &PlaybackEvent::StateChanged { session_rev: 2, state: PlaybackState::Playing },
        clock.sample(),
    );
    clock.advance(Duration::from_secs(6));
    let progress = tenuto::playback::event::Progress {
        session_rev: 2,
        media: Some(media("c")),
        position: Duration::from_secs(6),
        quality: tenuto::playback::event::PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: Some(request),
    };
    let _ = session.tick(&progress, clock.sample());
    let _ = session.shutdown_snapshot(clock.sample());

    assert_eq!(session.state().playlists().playing_playlist().queue().active(), Some(ids[1]));
    let saved = session.state().entry_for(&media("c")).expect("c has a checkpoint");
    assert!(saved.position() >= Duration::from_secs(5), "{saved:?}");
}

#[test]
fn a_detached_track_that_ends_sets_no_advance() {
    let clock = FakeClock::new();
    let (mut session, _) = with_b_active(&clock);
    let request = session
        .register_load(LoadTarget::Detached, &media("elsewhere"))
        .expect("registered");
    session.observe(&loaded(2, request, media("elsewhere")), clock.sample());
    session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 2,
            position: Duration::from_secs(5),
            provenance: PositionProvenance::Established,
        },
        clock.sample(),
    );
    assert!(session.take_advance().is_none());
}
```

Before running, check the exact field lists of `PlaybackEvent::Loaded`, `EndOfTrack` and `Progress` (`src/playback/event.rs`), plus `StartDisposition`'s fresh-start variant, `MediaCapabilities` construction, `Session::shutdown_snapshot`'s name and the checkpoint accessor (`src/persistence/model.rs`, `entry_for`). Adjust the literals to match. The assertions are the contract; the literals are not.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --locked --test m9_3_detached_session`
Expected: compile error, `no variant named Detached`.

- [ ] **Step 3: Implement**

In `src/session.rs`, replace the enum and its doc:

```rust
/// What a `Load` this session sent is for: a playlist occurrence, or media
/// played on its own (`tenuto play`) that belongs to no playlist. A detached
/// load adopts and checkpoints its media like any other, but never moves a
/// cursor, never changes which playlist is playing, and never advances.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadTarget {
    Queue(QueueEntryId),
    Detached,
}
```

In `load_target_for` (around `:411`): `LoadTarget::Detached => Some(pending.target),`.

In the `Loaded` arm (around `:1052`), replace the `Legacy` arm and its comment:

```rust
            // A detached load belongs to no playlist, so it leaves every
            // cursor and the playing playlist where they were (M9.3).
            LoadTarget::Detached => {}
```

Update the doc comment at `:610` (`LoadTarget::Legacy never matches`) to say `LoadTarget::Detached`.

Delete `PlaylistSet::clear_playing_cursor` (`src/playlist/set.rs:324-331`).

Rename every remaining `LoadTarget::Legacy` to `LoadTarget::Detached`:

```bash
grep -rl "LoadTarget::Legacy" src tests | xargs sed -i 's/LoadTarget::Legacy/LoadTarget::Detached/g'
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --locked --test m9_3_detached_session --test m5_session_adoption --test m7_session --test m4_playback_identity --test estimated_resume --test http_resume`
Expected: PASS. If an existing test asserted the *cleared* cursor after a legacy load, it is asserting the bug: change it to assert the cursor is kept, and name the change in the commit message.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "fix(m9.3): a detached load leaves the playing playlist and its cursor alone"
```

---

### Task 2: One state opener and the detached transport table

**Files:**
- Create: `src/application/profile.rs`
- Modify: `src/application/mod.rs` (add `pub mod profile;`)
- Modify: `src/tui/mod.rs:256-305` (`start_runtime`)
- Modify: `src/application/transport.rs` (add `decide_detached`, factor the live-seek guard)
- Test: unit tests in `profile.rs` and `transport.rs`

**Interfaces:**
- Consumes: `StateStore::load() -> LoadOutcome { state, writable, queue_repair, reason, .. }`; `DisabledSink`; `WriterHandle::spawn`.
- Produces:
  - `pub struct OpenedState { pub session: Session, pub writer: WriterHandle, pub persisting: bool, pub queue_repair: Option<QueueRepair> }`
  - `pub fn open_state(store: StateStore, loaded: LoadOutcome, clock: &Arc<dyn Clock>) -> OpenedState`, which logs `loaded.reason` and any repair exactly as `app.rs::open_persistence` does today.
  - `pub fn decide_detached(input: TransportInput, phase: PlaybackPhase, live: bool) -> TransportDecision`

- [ ] **Step 1: Write the failing tests**

In `src/application/transport.rs`'s test module (create `#[cfg(test)] mod tests` if absent):

```rust
#[test]
fn a_detached_track_forwards_every_key_to_the_engine() {
    use PlaybackPhase::*;
    for phase in [Loading, Playing, Paused, Stopped, Ended, Reconnecting] {
        assert_eq!(decide_detached(TransportInput::Space, phase, false), TransportDecision::TogglePause);
        assert_eq!(decide_detached(TransportInput::Play, phase, false), TransportDecision::Play);
        assert_eq!(decide_detached(TransportInput::Home, phase, false), TransportDecision::Restart);
        assert_eq!(decide_detached(TransportInput::SeekBy(-10), phase, false), TransportDecision::SeekBy(-10));
    }
}

#[test]
fn a_detached_track_has_no_playlist_to_navigate() {
    for input in [TransportInput::Previous, TransportInput::Next, TransportInput::Enter] {
        assert_eq!(decide_detached(input, PlaybackPhase::Playing, false), TransportDecision::Nothing);
    }
}

#[test]
fn a_detached_live_stream_is_not_seeked() {
    assert_eq!(
        decide_detached(TransportInput::SeekBy(10), PlaybackPhase::Playing, true),
        TransportDecision::Notice(LIVE_NO_SEEK)
    );
    assert_eq!(
        decide_detached(TransportInput::Space, PlaybackPhase::Playing, true),
        TransportDecision::TogglePause
    );
}
```

`TransportDecision` needs `Debug, PartialEq` for `assert_eq!`. Check its derive and add them if missing.

In `src/application/profile.rs`, move the two persistence tests out of `src/app.rs` (`a_state_file_from_a_newer_build_disables_writing_and_is_left_untouched`, `a_disabled_session_never_reports_a_written_checkpoint`) and point them at `open_state`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    fn store_at(path: &std::path::Path) -> (StateStore, Arc<dyn Clock>) {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        (StateStore::new(path.to_path_buf(), Arc::clone(&clock)), clock)
    }

    #[test]
    fn a_state_file_from_a_newer_build_opens_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"version": 999}"#).expect("seed");
        let (store, clock) = store_at(&path);
        let loaded = store.load();
        let opened = open_state(store, loaded, &clock);
        assert!(!opened.persisting);
        assert_eq!(std::fs::read_to_string(&path).expect("kept"), r#"{"version": 999}"#);
    }

    #[test]
    fn a_missing_state_file_opens_writable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let loaded = store.load();
        assert!(open_state(store, loaded, &clock).persisting);
    }
}
```

Copy the exact seed content from the existing `app.rs` test rather than the literal above if it differs.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --lib application::`
Expected: compile errors, `decide_detached` and `open_state` not found.

- [ ] **Step 3: Implement**

`src/application/transport.rs`: factor the live guard out of `decide` and add the detached table:

```rust
/// M7 §3.4: a rejected seek must be harmless, and the cheapest way is not to
/// submit one. Shared by the playlist and the detached tables.
fn live_refuses(input: TransportInput, phase: PlaybackPhase, live: bool) -> bool {
    live && matches!(
        input,
        TransportInput::Home | TransportInput::SeekBy(_) | TransportInput::SeekTo(_)
    ) && !matches!(phase, PlaybackPhase::Unloaded | PlaybackPhase::LoadFailed)
}

/// The transport table for media played on its own (`tenuto play`, M9.3):
/// there is no playlist to start, retry or navigate, so every key goes to
/// the engine as itself — what the engine answers (a warning for Space
/// after the end, say) is the engine's rule, not this table's.
pub fn decide_detached(input: TransportInput, phase: PlaybackPhase, live: bool) -> TransportDecision {
    if live_refuses(input, phase, live) {
        return TransportDecision::Notice(LIVE_NO_SEEK);
    }
    match input {
        TransportInput::Space => TransportDecision::TogglePause,
        TransportInput::Play => TransportDecision::Play,
        TransportInput::Home => TransportDecision::Restart,
        TransportInput::SeekBy(step) => TransportDecision::SeekBy(step),
        TransportInput::SeekTo(target) => TransportDecision::SeekTo(target),
        TransportInput::Enter | TransportInput::Previous | TransportInput::Next => {
            TransportDecision::Nothing
        }
    }
}
```

and in `decide`, replace the inline guard with `if live_refuses(input, situation.phase, situation.live) { return TransportDecision::Notice(LIVE_NO_SEEK); }`.

`src/application/profile.rs`:

```rust
//! Opening the profile's `state.json` into a session and its writer — the
//! one path both front ends take (M9.2). The caller has already taken the
//! profile lock and called `StateStore::load`; the terminal player loads
//! before it redirects stderr, so the load and the open are separate steps.

use std::sync::Arc;

use crate::clock::Clock;
use crate::persistence::store::{LoadOutcome, LoadReason, QueueBackup, QueueRepair, StateStore};
use crate::persistence::writer::{DisabledSink, StateSink, WriterHandle};
use crate::session::Session;

pub struct OpenedState {
    pub session: Session,
    pub writer: WriterHandle,
    /// Whether anything `writer` accepts can reach the disk.
    pub persisting: bool,
    /// A queue repair the load performed, for a front end to report.
    pub queue_repair: Option<QueueRepair>,
}

pub fn open_state(store: StateStore, loaded: LoadOutcome, clock: &Arc<dyn Clock>) -> OpenedState {
    log_load(&store, &loaded);
    let LoadOutcome { state, writable, queue_repair, .. } = loaded;
    let sink: Box<dyn StateSink> = if writable { Box::new(store) } else { Box::new(DisabledSink) };
    OpenedState {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(clock)),
        persisting: writable,
        queue_repair,
    }
}
```

`log_load` is the two `match` blocks moved verbatim from `app.rs::open_persistence` (`LoadReason` and `queue_repair` logging), taking `(&StateStore, &LoadOutcome)`. Fix the import paths to wherever `LoadOutcome`, `LoadReason`, `QueueBackup`, `QueueRepair` and `DisabledSink` actually live (`grep -rn "pub struct LoadOutcome\|pub enum LoadReason\|pub struct DisabledSink" src`).

`src/tui/mod.rs::start_runtime`: replace the destructuring, sink and `Session::new`/`WriterHandle::spawn` with:

```rust
    let OpenedState { session, writer, persisting, queue_repair } = open_state(store, loaded, &clock);
    let mut runtime = PlayerRuntime::new(RuntimeParts {
        session,
        writer,
        persisting,
        clock,
        engine_factory: Box::new(EngineHandle::spawn_for_environment),
        library: library_stores(),
        http_limits: Limits::default(),
        metadata_probe: Some(default_probe(hook)),
        hook,
    });
```

and keep the status computation, reading `queue_repair` and `persisting` in place of `writable`. Drop the now-unused imports.

- [ ] **Step 4: Run tests**

Run: `cargo test --locked --lib application:: && cargo test --locked --test m5_tui_process --test m8_tui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(m9.2): one state opener for both front ends, and a detached transport table"
```

---

### Task 3: The runtime plays detached media

**Files:**
- Modify: `src/application/runtime.rs` (fields, `play_detached`, `submit_load`, `transport`, `view`, `Mirror::title`, `poll_budget`)
- Test: `tests/m9_3_detached_play.rs` (**new**, runtime rig)

**Interfaces:**
- Consumes: `LoadTarget::Detached` (Task 1); `decide_detached` (Task 2).
- Produces:
  - `pub fn play_detached(&mut self, media: MediaId, location: SourceLocation)`: registers a detached load, submits `Load` + `PlayLoaded`, and remembers the media.
  - `pub fn poll_budget(&self, cap: Duration) -> Duration`: how long a front end may block waiting for a key before a seek burst comes due.
  - Behavior change for both front ends: `AppCommand::Stop` keeps a stored seek target (it routes through `KeyRouter` instead of cancelling it).
  - `view().now_playing` while detached: `Some(NowPlaying { entry: None, title, .. })` from the mirror, with the title `episode_name(metadata title)` for a podcast and `display_name(media)` otherwise, through `displayable`.

- [ ] **Step 1: Write the failing tests**

`tests/m9_3_detached_play.rs`:

```rust
//! M9.3: the runtime plays media on its own (`tenuto play`) without touching
//! the saved playlists.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

use std::time::Duration;

use runtime::{enqueue, pump_for, pump_until, rig_with, row_ids};
use tenuto::application::runtime::{AppCommand, EnqueueItem};
use tenuto::application::transport::PlaybackPhase;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::media::source::SourceLocation;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::state::PlaybackState;

const FIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");
const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");

fn local(path: &str) -> (MediaId, SourceLocation) {
    let path = AbsolutePath::new(path.into()).expect("absolute");
    (MediaId::LocalFile(path.clone()), SourceLocation::LocalPath(path.as_path().to_path_buf()))
}

fn playing(view: &tenuto::application::view::PlayerView) -> bool {
    view.now_playing.as_ref().is_some_and(|now| now.loaded && now.state == PlaybackState::Playing)
}

#[test]
fn a_detached_play_shows_its_own_media_and_keeps_the_cursor() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(SHORT.into())]);
    let row = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(row));
    pump_until(&mut rig, "the row plays", playing);
    rig.runtime.handle(AppCommand::Stop);

    let (media, location) = local(FIVE);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "the detached track plays", playing);
    let view = rig.runtime.view();
    let now = view.now_playing.expect("now playing");
    assert_eq!(now.entry, None);
    assert_eq!(now.title, "sine-5s.flac");
    assert_eq!(view.active, Some(row), "the cursor stays on the row");
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_detached_track_that_ends_loads_nothing_else() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(FIVE.into())]);
    let (media, location) = local(SHORT);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "ended", |view| view.phase == PlaybackPhase::Ended);
    pump_for(&mut rig, Duration::from_millis(300));
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Ended);
    assert_eq!(rig.runtime.session().pending_load_count(), 0, "no row was loaded");
    let _ = rig.runtime.shutdown();
}

#[test]
fn stop_then_play_resumes_a_detached_track_where_it_stopped() {
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(FIVE);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "past one second", |view| {
        view.now_playing.as_ref().is_some_and(|now| now.position >= Duration::from_secs(1))
    });
    rig.runtime.handle(AppCommand::Stop);
    pump_until(&mut rig, "stopped", |view| view.phase == PlaybackPhase::Stopped);
    let stopped_at = rig.runtime.view().now_playing.expect("now").position;
    rig.runtime.handle(AppCommand::Play);
    pump_until(&mut rig, "playing again", playing);
    let resumed = rig.runtime.view().now_playing.expect("now").position;
    assert!(resumed >= stopped_at, "{resumed:?} < {stopped_at:?}");
    let _ = rig.runtime.shutdown();
}

#[test]
fn home_restarts_an_ended_detached_track() {
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(SHORT);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "ended", |view| view.phase == PlaybackPhase::Ended);
    rig.runtime.handle(AppCommand::Restart);
    pump_until(&mut rig, "back at the start", |view| {
        view.phase != PlaybackPhase::Ended
            && view.now_playing.as_ref().is_some_and(|now| now.position < Duration::from_secs(1))
    });
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_detached_seek_while_stopped_flushes_with_an_empty_playlist() {
    // Review Focus 6: the playing playlist is empty, so the playlist table
    // would refuse the flush.
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(FIVE);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "past one second", |view| {
        view.now_playing.as_ref().is_some_and(|now| now.position >= Duration::from_secs(1))
    });
    rig.runtime.handle(AppCommand::Stop);
    pump_until(&mut rig, "stopped", |view| view.phase == PlaybackPhase::Stopped);
    rig.runtime.handle(AppCommand::SeekBy(2));
    // Past the 250 ms quiet window: the burst has flushed, so no deadline
    // is left to shorten the poll.
    pump_for(&mut rig, Duration::from_millis(400));
    assert_eq!(rig.runtime.poll_budget(Duration::from_millis(100)), Duration::from_millis(100));
    let _ = rig.runtime.shutdown();
}

#[test]
fn home_while_a_detached_load_is_pending_restarts_it() {
    // Review Focus 7. A first run leaves a checkpoint past three seconds.
    let mut first = rig_with(PersistedState::default());
    let (media, location) = local(FIVE);
    first.runtime.play_detached(media.clone(), location.clone());
    pump_until(&mut first, "past three seconds", |view| {
        view.now_playing.as_ref().is_some_and(|now| now.position >= Duration::from_secs(3))
    });
    let _ = first.runtime.shutdown();
    let clock: std::sync::Arc<dyn tenuto::clock::Clock> =
        std::sync::Arc::new(tenuto::clock::FakeClock::new());
    let saved = tenuto::persistence::store::StateStore::new(first.state_path.clone(), clock)
        .load()
        .state;

    // Home before a single pump: no `Loaded` has been drained, no mirror.
    let mut rig = rig_with(saved);
    rig.runtime.play_detached(media, location);
    rig.runtime.handle(AppCommand::Restart);
    pump_until(&mut rig, "playing", playing);
    let position = rig.runtime.view().now_playing.expect("now").position;
    assert!(position < Duration::from_secs(2), "resumed at {position:?} instead of restarting");
    let _ = rig.runtime.shutdown();
}

#[test]
fn stop_keeps_a_stored_seek_target_for_the_next_arrow() {
    // Review Focus 8: Stop → seek → wait → Stop → arrow.
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local(FIVE);
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "past one second", |view| {
        view.now_playing.as_ref().is_some_and(|now| now.position >= Duration::from_secs(1))
    });
    rig.runtime.handle(AppCommand::Stop);
    pump_until(&mut rig, "stopped", |view| view.phase == PlaybackPhase::Stopped);
    rig.runtime.handle(AppCommand::SeekBy(2));
    pump_until(&mut rig, "the target is stored", |view| {
        view.now_playing.as_ref().is_some_and(|now| now.position >= Duration::from_secs(3))
    });
    let stored = rig.runtime.view().now_playing.expect("now").position;
    rig.runtime.handle(AppCommand::Stop);
    pump_for(&mut rig, Duration::from_millis(100));
    assert_eq!(rig.runtime.view().now_playing.expect("now").position, stored);
    rig.runtime.handle(AppCommand::SeekBy(1));
    assert_eq!(
        rig.runtime.view().now_playing.expect("now").position,
        stored + Duration::from_secs(1),
        "the arrow accumulates from the stored target"
    );
    rig.runtime.handle(AppCommand::Play);
    pump_until(&mut rig, "playing", playing);
    assert!(rig.runtime.view().now_playing.expect("now").position >= stored + Duration::from_secs(1));
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_detached_load_failure_reports_load_failed_with_its_message() {
    let mut rig = rig_with(PersistedState::default());
    let (media, location) = local("/nonexistent/definitely-not-here.flac");
    rig.runtime.play_detached(media, location);
    pump_until(&mut rig, "failed", |view| view.phase == PlaybackPhase::LoadFailed);
    assert!(rig.runtime.view().status.is_some_and(|status| !status.is_empty()));
    let _ = rig.runtime.shutdown();
}
```

Check `AbsolutePath::new`'s argument type and the fixture names (`ls tests/fixtures`). Use whichever short fixture `m5_runtime.rs` calls `SHORT`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m9_3_detached_play`
Expected: compile error, `no method named play_detached`.

- [ ] **Step 3: Implement**

In `PlayerRuntime`, add a field after `last_requested`:

```rust
    /// The media of the latest detached load (`tenuto play`, M9.3), while it
    /// is what this runtime plays: transport keys then bypass the playlist
    /// table and `NowPlaying` describes it rather than the active entry.
    detached: Option<MediaId>,
```

initialized to `None` in `new`. Add `title: Option<String>` to `Mirror` (doc: "The decoder's title; a detached podcast episode is named by it."), set in `Mirror::loaded` from `metadata.title.clone()`.

Extract everything in `load_entry_starting` after the `register_load` match, from `self.ensure_engine();` to the end, into:

```rust
    /// Admits a registered load: engine, HTTP when remote, resume intent,
    /// `Load`, then the token-scoped start. Shared by playlist and detached
    /// loads.
    fn submit_load(
        &mut self,
        request: LoadRequestId,
        media: MediaId,
        location: SourceLocation,
        start: impl FnOnce(&EngineHandle, LoadRequestId) -> Admission,
    ) {
        // body moved verbatim from `load_entry_starting`
    }
```

and make `load_entry_starting` call `self.detached = None;` first, then `self.submit_load(request, media, location, start)` after registering. Then:

```rust
    /// Plays `media` from `location` on its own, belonging to no playlist
    /// (`tenuto play`, M9.3). Its checkpoint and the volume are saved as for
    /// any load; no cursor, no playing playlist and no row changes.
    pub fn play_detached(&mut self, media: MediaId, location: SourceLocation) {
        self.last_requested = None;
        self.last_attempt = None;
        self.status = None;
        self.detached = Some(media.clone());
        match self.session.register_load(LoadTarget::Detached, &media) {
            Ok(request) => self.submit_load(request, media, location, |engine, request| {
                engine.submit(PlaybackCommand::PlayLoaded { request })
            }),
            Err(RegisterLoadError::Busy) => self.status = Some(TOO_MANY_PENDING_LOADS.to_owned()),
            // A detached target names no entry, so neither can happen.
            Err(RegisterLoadError::UnknownEntry | RegisterLoadError::MediaMismatch) => {}
        }
    }

    /// How long a front end may block on input before the open seek burst
    /// comes due, capped at `cap`.
    pub fn poll_budget(&self, cap: Duration) -> Duration {
        self.router.poll_budget(self.clock.sample().monotonic, cap)
    }
```

`KeyRouter::poll_budget` is `pub(crate)`, which is fine inside the crate.

Choose the table in one place, and use it for both the transport and the flush gate (Review Focus 6):

```rust
    /// The transport table in force: the detached one while a detached load
    /// is what this runtime plays, otherwise the playlist one.
    fn decision(&self, input: TransportInput, selected: Option<QueueEntryId>) -> TransportDecision {
        if self.detached.is_some() {
            decide_detached(input, self.phase(), self.indefinite())
        } else {
            self.decide(input, selected)
        }
    }

    fn seeking_allowed(&self) -> bool {
        matches!(self.decision(TransportInput::SeekBy(0), None), TransportDecision::SeekBy(_))
    }

    fn transport(&mut self, input: TransportInput, selected: Option<QueueEntryId>) {
        match self.decision(input, selected) { /* unchanged arms */ }
    }
```

`route` reaches the engine without a mirror while detached (Review Focus 7). This matches today's `play`, whose mirror starts at zero with no duration. The playlist path keeps its early return, so the TUI is unchanged:

```rust
    fn route(&mut self, command: PlaybackCommand) {
        let now = self.clock.sample().monotonic;
        let Some(engine) = &self.engine else {
            return;
        };
        let (position, duration) = match (&self.mirror, self.detached.is_some()) {
            (Some(mirror), _) => (mirror.position, mirror.duration),
            // Before `Loaded`, a detached key still queues behind the load,
            // as `tenuto play`'s always did.
            (None, true) => (Duration::ZERO, None),
            (None, false) => return,
        };
        let optimistic = self.router.route(engine, position, duration, now, command);
        // A prediction until the seek lands, so it reaches only the display.
        if let Some(position) = optimistic
            && let Some(mirror) = &mut self.mirror
        {
            mirror.position = position;
            mirror.provenance = PositionProvenance::Estimated;
        }
    }
```

Stop goes through the router in both front ends (Review Focus 8). `KeyRouter::route(Stop)` drops the burst and the submitted wait but keeps a stored target, then submits `Stop`. In `handle`:

```rust
            AppCommand::Stop => {
                if let Some(engine) = &self.engine {
                    // Through the router, not `cancel`: the worker keeps a
                    // stored seek target across a stop, so the display and
                    // the next arrow must too.
                    let _ = self.router.route(
                        engine,
                        Duration::ZERO,
                        None,
                        self.clock.sample().monotonic,
                        PlaybackCommand::Stop,
                    );
                }
            }
```

`position` and `duration` are unread for `Stop` (`KeyRouter::route` reads them only for `SeekBy`).

In `view`, build `now_playing` from the detached media when there is one:

```rust
            now_playing: match &self.detached {
                Some(media) => Some(self.detached_now_playing(media)),
                None => active
                    .and_then(|id| queue.get(id))
                    .map(|entry| self.now_playing(entry, state)),
            },
```

Split `now_playing` so both paths share the mirrored half:

```rust
    fn now_playing(&self, entry: &QueueEntry, state: &PersistedState) -> NowPlaying {
        let display = entry.display();
        self.mirrored(NowPlaying {
            entry: Some(entry.id()),
            title: entry_plain_title(entry),
            artist: display.artist.as_deref().map(displayable),
            album: display.album.as_deref().map(displayable),
            year: display.year.as_deref().map(displayable),
            duration: display.duration,
            saved: saved_history(state.entry_for(entry.media())),
            ..NowPlaying::unloaded()
        })
    }

    fn detached_now_playing(&self, media: &MediaId) -> NowPlaying {
        let mirror = self.mirror.as_ref().filter(|mirror| &mirror.media == media);
        let title = match media {
            MediaId::PodcastEpisode { .. } => {
                episode_name(mirror.and_then(|mirror| mirror.title.as_deref()))
            }
            other => display_name(other),
        };
        let unloaded = NowPlaying {
            title: displayable(&title),
            saved: saved_history(self.session.state().entry_for(media)),
            ..NowPlaying::unloaded()
        };
        if mirror.is_some() { self.mirrored(unloaded) } else { unloaded }
    }

    /// `unloaded` overlaid with the adopted playback, when there is one.
    fn mirrored(&self, unloaded: NowPlaying) -> NowPlaying {
        let Some(mirror) = &self.mirror else {
            return unloaded;
        };
        NowPlaying {
            loaded: true,
            state: mirror.state,
            position: mirror.position,
            duration: if mirror.capabilities.continuity == Continuity::Indefinite {
                None
            } else {
                mirror
                    .duration
                    .map(|value| DisplayDuration {
                        value,
                        source: DurationSource::Decoded(mirror.duration_provenance),
                    })
                    .or(unloaded.duration)
            },
            estimated_position: mirror.provenance == PositionProvenance::Estimated,
            degraded: mirror.quality == PositionQuality::Degraded,
            buffering: mirror.buffering,
            seek: Some(mirror.capabilities.seek),
            session_rev: mirror.session_rev,
            load: Some(mirror.load),
            ..unloaded
        }
    }
```

Add `NowPlaying::unloaded()` in `src/application/view.rs`, holding every field's empty value (`entry: None`, `title: String::new()`, `artist/album/year: None`, `loaded: false`, `state: PlaybackState::Idle`, `position: Duration::ZERO`, `duration: None`, `estimated_position/degraded/buffering: false`, `seek: None`, `saved: None`, `session_rev: 0`, `load: None`). Import `episode_name` and `display_name` from `crate::media::display`, and `decide_detached` from `transport`.

- [ ] **Step 4: Run tests**

Run: `cargo test --locked --test m9_3_detached_play --test m5_runtime --test m8_runtime --test m7_runtime --test m10_finite_reconnect --lib`

The Stop change touches the TUI too: if an existing test asserted that Stop clears a stored target, it pinned the defect in Review Focus 8. Change the assertion and say so in the commit message.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(m9.3): the runtime plays media on its own, outside every playlist

Stop now goes through the key router in both front ends, keeping a stored
seek target the worker also keeps."
```

---

### Task 4: `tenuto play` over the runtime

**Files:**
- Modify: `src/app.rs`: rewrite `run_resolved_locked`. Delete `Mirror`, `apply_progress`, `Phase`, `finish`, `handle_keys`, `Persistence`, `open_persistence`, `resume_commands`, `submit` and their tests. Rebuild `status_parts` over `NowPlaying`.
- Test: `src/app.rs` unit tests (`outcome`, the status line); `tests/m9_3_play_leaves_playlists.rs` (**new**, subprocess, Linux-only)

**Interfaces:**
- Consumes: `open_state` (Task 2); `PlayerRuntime::{play_detached, poll_budget, handle, pump, view, shutdown}` (Task 3); `FlushReport`.
- Produces: no new public API. `fn outcome(view: &PlayerView) -> Option<Result<(), PlaybackError>>` and `fn status_parts(now: &NowPlaying, live: bool, volume: Volume) -> (String, String)` are private.

- [ ] **Step 1: Write the failing tests**

In `src/app.rs` tests:

```rust
fn view_with(phase: PlaybackPhase, state: PlaybackState, status: Option<&str>) -> PlayerView {
    PlayerView {
        phase,
        status: status.map(str::to_owned),
        now_playing: Some(NowPlaying { loaded: true, state, ..NowPlaying::unloaded() }),
        ..PlayerView::default()
    }
}

#[test]
fn a_load_failure_ends_the_run_with_its_message() {
    let view = view_with(PlaybackPhase::LoadFailed, PlaybackState::Failed, Some("cannot open media"));
    assert!(matches!(outcome(&view), Some(Err(PlaybackError::Failed(message))) if message == "cannot open media"));
}

#[test]
fn a_failure_after_loading_ends_the_run_with_its_message() {
    // `phase()` maps a mirrored `Failed` to `Stopped`; the state is what says so.
    let view = view_with(PlaybackPhase::Stopped, PlaybackState::Failed, Some("the server went quiet"));
    assert!(matches!(outcome(&view), Some(Err(PlaybackError::Failed(message))) if message == "the server went quiet"));
}

#[test]
fn playing_paused_and_ended_do_not_end_the_run_by_themselves() {
    for (phase, state) in [
        (PlaybackPhase::Loading, PlaybackState::Loading),
        (PlaybackPhase::Playing, PlaybackState::Playing),
        (PlaybackPhase::Paused, PlaybackState::Paused),
        (PlaybackPhase::Stopped, PlaybackState::Stopped),
        (PlaybackPhase::Ended, PlaybackState::Ended),
    ] {
        assert!(outcome(&view_with(phase, state, None)).is_none(), "{phase:?}");
    }
}
```

If `PlayerView` has no `Default`, add `#[derive(Default)]` where every field allows it, or build the view with a small helper in the test.

Port every existing status-line test (`a_long_name_gives_way…`, `a_row_narrower…`, `a_wide_name…`, `a_row_that_fits…`, `a_title_carrying_a_terminal_escape…`, `a_live_source_reads_live…`, `an_unresolved_seek_capability…`, `a_seekable_source_carries_no_seek_note…`, `buffering_is_shown_only_as_a_detail_of_playing`, `degraded_quality_does_not_imply_buffering`) from a `Mirror` to a `NowPlaying` with the same field values: `name` → `title` (already escaped, so the escape test asserts the escaped title reaches the row unchanged); `capabilities.seek` → `seek`; `continuity == Indefinite` → the `live` argument; `provenance == Estimated` → `estimated_position`; `quality == Degraded` → `degraded`. Delete the `Mirror` tests whose subject no longer exists in `app.rs` (`capabilities_changed_updates…`, `a_fresh_load_clears…`, the three `progress…burst` tests, `a_resumed_position_is_shown…`, `the_resume_sequence_restores_volume…`, the `a_stored_…_candidate` tests, `a_submitted_snapshot_reaches…`). They are covered by `runtime.rs`'s mirror, `session.rs`'s `resume_intent` and the writer's own tests. Before deleting each one, grep that a counterpart exists (`resume_intent` / `resume_intent_for` tests in `session.rs`, `apply_progress` gating in `runtime.rs`). If one has none, move it into `tests/m9_3_detached_play.rs` as a rig test instead of deleting it.

`tests/m9_3_play_leaves_playlists.rs`:

```rust
//! M9.3 end to end: `tenuto play` leaves the saved playlists as it found them.
#![cfg(target_os = "linux")]

#[path = "support/process.rs"]
mod process;

use std::sync::Arc;
use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::MediaCapabilities;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueSource};
use tenuto::session::{LoadTarget, Session};

const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const OTHER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");

#[test]
fn play_keeps_the_playing_playlist_and_its_cursor() {
    let profile = process::Profile::new().expect("profile");
    std::fs::create_dir_all(profile.state_dir()).expect("state dir");
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());

    // A saved playlist of one row, adopted, the way the TUI leaves it.
    let path = AbsolutePath::new(OTHER.into()).expect("abs");
    let media = MediaId::LocalFile(path.clone());
    let mut session = Session::new(PersistedState::default());
    let playing = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(
            playing,
            vec![NewQueueEntry::new(media.clone(), QueueSource::LocalFile(path), DisplayMetadata::default()).expect("valid")],
        )
        .expect("fits");
    let request = session.register_load(LoadTarget::Queue(ids[0]), &media).expect("registered");
    session.observe(
        &PlaybackEvent::Loaded {
            session_rev: 1,
            request,
            media,
            metadata: MediaMetadata::default(),
            capabilities: MediaCapabilities::default(),
            position: Duration::ZERO,
            disposition: StartDisposition::Fresh,
        },
        clock.sample(),
    );
    StateStore::new(profile.state_file(), Arc::clone(&clock))
        .write(session.state())
        .expect("seeded");

    let output = profile
        .command()
        .env("TENUTO_AUDIO_OUTPUT", "null")
        .args(["play", SHORT])
        .output()
        .expect("ran");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let reloaded = StateStore::new(profile.state_file(), clock).load().state;
    assert_eq!(reloaded.playlists().playing(), playing);
    assert_eq!(reloaded.playlists().playing_playlist().queue().active(), Some(ids[0]));
    assert_eq!(reloaded.playlists().playing_playlist().queue().entries().count(), 1);
}
```

Match the `Loaded` literal to Task 1's verified field list.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --lib app::`
Expected: compile error, `outcome` not found.

`m9_3_play_leaves_playlists` already passes at this point, because Task 1 switched `play` to `Detached`. It is here to guard the rewrite, so run it before and after Step 3.

- [ ] **Step 3: Implement**

Rewrite `run_resolved_locked` (keep `run_resolved`, `RawModeGuard`, `to_command`'s key table, `render`, `fit_status`, `run_probe_only` and the signal handling):

```rust
fn run_resolved_locked(
    media: MediaId,
    location: SourceLocation,
    clock: Arc<dyn Clock>,
    store: StateStore,
    signals: &ShutdownSignals,
) -> Result<(), PlaybackError> {
    let loaded = store.load();
    let OpenedState { session, writer, persisting, .. } = open_state(store, loaded, &clock);
    let mut runtime = PlayerRuntime::new(RuntimeParts {
        session,
        writer,
        persisting,
        clock,
        engine_factory: Box::new(EngineHandle::spawn_for_environment),
        // A podcast episode arrives already resolved; nothing else here
        // reads the library.
        library: None,
        http_limits: Limits::default(),
        metadata_probe: None,
        hook: TestHook::None,
    });
    runtime.play_detached(media, location);

    // Entered only now (R5, Ruling 1): every fallible step above can still
    // fail before a key is read, which keeps a rejected source reportable
    // with no raw terminal. `None` means there is no tty.
    let raw = RawModeGuard::enable();
    print!("Loading \u{2026}\r\n");
    let _ = std::io::stdout().flush();

    let outcome = loop {
        if signals.requested() {
            break Ok(());
        }
        match next_key(&runtime, keys(&raw), signals) {
            Key::Quit => break Ok(()),
            Key::Command(command) => runtime.handle(command),
            Key::None => {}
        }
        runtime.pump();
        let view = runtime.view();
        if let Some(outcome) = outcome(&view) {
            break outcome;
        }
        if let Some(now) = view.now_playing.as_ref().filter(|now| now.loaded)
            && let Err(error) = render(now, view.live, view.volume)
        {
            // D18: a terminal write failure must not skip the final flush.
            break Err(error);
        }
        // With no tty no key can end the run, so the end of the one track does.
        if raw.is_none() && view.phase == PlaybackPhase::Ended {
            break Ok(());
        }
    };

    // Restore the terminal before waiting on the engine and the disk, and
    // before the caller prints a diagnostic on `outcome` (Ruling 1).
    drop(raw);
    report_flush(runtime.shutdown());
    outcome
}

/// What ends the run, read off the view: a load that failed, or a track
/// that failed after it loaded. `None` keeps the run going.
fn outcome(view: &PlayerView) -> Option<Result<(), PlaybackError>> {
    let failed = view.phase == PlaybackPhase::LoadFailed
        || view.now_playing.as_ref().is_some_and(|now| now.state == PlaybackState::Failed);
    failed.then(|| {
        Err(PlaybackError::Failed(
            view.status.clone().unwrap_or_else(|| "playback failed".to_owned()),
        ))
    })
}

enum Key {
    Quit,
    Command(AppCommand),
    None,
}

/// One wait for a key, capped by the runtime's seek-burst deadline. With no
/// raw terminal there are no keys: the wait races the shutdown signal's wake
/// instead of sleeping blind, so a signal during a stalled open is seen at
/// once.
fn next_key(runtime: &PlayerRuntime, input: Option<&InputReader>, signals: &ShutdownSignals) -> Key {
    let budget = runtime.poll_budget(Duration::from_millis(100));
    let Some(input) = input else {
        let _ = signals.wake().recv_timeout(budget);
        return Key::None;
    };
    match input.next(budget) {
        Ok(Some(Event::Key(key))) => to_command(key),
        Ok(_) => Key::None,
        // The input stream ended or failed: shut down as cleanly as `q`.
        Err(_) => Key::Quit,
    }
}

fn to_command(key: KeyEvent) -> Key {
    if key.kind != KeyEventKind::Press {
        return Key::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Key::Quit;
    }
    Key::Command(match key.code {
        KeyCode::Char(' ') => AppCommand::PlayPause,
        KeyCode::Left => AppCommand::SeekBy(-SEEK_STEP_SECS),
        KeyCode::Right => AppCommand::SeekBy(SEEK_STEP_SECS),
        KeyCode::Home => AppCommand::Restart,
        KeyCode::Char('-' | '_') => AppCommand::AdjustVolume(-VOLUME_STEP),
        KeyCode::Char('+' | '=') => AppCommand::AdjustVolume(VOLUME_STEP),
        KeyCode::Char('s') => AppCommand::Stop,
        KeyCode::Char('p') => AppCommand::Play,
        KeyCode::Char('q') => return Key::Quit,
        _ => return Key::None,
    })
}

fn report_flush(report: FlushReport) {
    match report {
        FlushReport::Written => tracing::debug!("final checkpoint written"),
        FlushReport::Failed(error) => tracing::warn!(%error, "final checkpoint failed"),
        FlushReport::Unconfirmed => tracing::warn!("final checkpoint UNCONFIRMED"),
        FlushReport::Disabled => {
            tracing::debug!("persistence is disabled for this session; no checkpoint was written");
        }
    }
}
```

`render(now, live, volume)` keeps today's two-row repaint and calls `status_parts(now, live, volume)`:

```rust
fn status_parts(now: &NowPlaying, live: bool, volume: Volume) -> (String, String) {
    // Already escaped by the runtime (`displayable`).
    let name = now.title.clone();
    let position = format_hms(now.position);
    let mut suffix = String::new();
    if now.degraded {
        suffix.push_str(" ~");
    }
    if now.estimated_position {
        suffix.push_str(" ~est");
    }
    let duration = if live {
        "live".to_owned()
    } else {
        now.duration
            .map(|duration| format_hms(duration.value))
            .unwrap_or_else(|| "--:--:--".to_string())
    };
    let seek_note = match now.seek {
        Some(SeekSupport::Unknown) => " seek?",
        Some(SeekSupport::Unsupported) => " no-seek",
        _ => "",
    };
    let mut label = now.state.label().to_string();
    if now.buffering && now.state == PlaybackState::Playing {
        label.push_str(" buffering");
    }
    let fields = format!(" [{label}]{seek_note} {position}{suffix} / {duration}  vol {}%", volume.percent());
    (name, fields)
}
```

Keep the existing explanatory comments from today's `status_parts` (two independent marks; unresolved vs unsupported; buffering a detail of Playing) on the matching lines. Delete the dead code listed under **Files** and the now-unused imports. Check that `crate::commands::displayable` is no longer imported by `app.rs` for the status row.

- [ ] **Step 4: Run tests**

Run:
```bash
cargo test --locked --lib app::
cargo test --locked --test m9_3_play_leaves_playlists --test cli_playback --test m5_signals_play --test m5_lock_process --test http_cli --test m4_cli --test m4_diagnostics --test m5_tui_process
```
Expected: PASS. A subprocess failure here is a behavior change in `play`: compare against the Global Constraints list and fix the loop, not the test.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(m9.3): tenuto play runs over the player runtime"
```

---

### Task 5: Gate, docs and acceptance record

**Files:**
- Modify: `docs/reference.md` (the `play` row and its keys paragraph: add that `play` leaves your playlists as they are)
- Modify: `docs/architecture.md` (§4 component table rows for `app`/`application`; the §4 text naming two mirrors or two openers; §12 M9 status: M9.2's mirror and opener items and M9.3's `play` item shipped)
- Modify: `docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` (Status lines under "One adopted-playback mirror" and "`play` over `PlayerRuntime`")
- Modify: `CHANGELOG.md` `[Unreleased]`
- Create: `docs/m9.3-acceptance.md`

- [ ] **Step 1: Full gate**

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo test --locked --no-fail-fast
```
Expected: all clean. Then the load check on the touched suites:

```bash
for i in $(seq 8); do (taskset -c 0-1 yes > /dev/null &); done
for t in m9_3_detached_play m9_3_play_leaves_playlists cli_playback m5_signals_play m5_runtime; do taskset -c 0-1 cargo test --locked -q --test $t || echo "FAIL $t"; done
pkill -x yes
```
Expected: no `FAIL`.

- [ ] **Step 2: Docs**

CHANGELOG `[Unreleased]`:
- under **Fixed**:
  ```
  - `tenuto play` no longer loses your place in the TUI: it plays the file or
    URL on its own and leaves your playlists, their cursors and the playing
    playlist exactly as they were. Its resume position and the volume are
    still saved.
  - Pressing Stop twice no longer forgets a seek you made while stopped: the
    position shown and the next arrow press start from the target, and Play
    resumes there.
  ```
- under **Internal**:
  ```
  - `tenuto play` runs on the same player runtime as the TUI, so both share
    one view of playback, one transport and one way of opening `state.json`.
    Its keys, status line and exit codes are unchanged.
  ```

Roadmap status lines:
- Under "One adopted-playback mirror": `Status: shipped through M9.3. With `play` over the runtime there is one mirror (`application/runtime.rs`) and one opener (`application::profile::open_state`); `LoadTarget::Legacy` became `Detached`, which leaves the playlists alone. The provenance defect went with `app.rs`'s mirror.`
- Under "`play` over `PlayerRuntime`": `Status: shipped. `PlayerRuntime::play_detached` plays media outside every playlist; `play` drives `handle`/`pump`/`view`/`shutdown` with its keys, status line and exit codes unchanged.`

`docs/m9.3-acceptance.md`: the decisions (detached; keys kept), each Review Focus line with the test that pins it, the gate results, the load-check result, and a **Manual** section: release build, `tenuto play` on a Radio-T episode in Ghostty, then `tenuto tui` opens on the same playlist row as before. Mark the manual check as pending until the user runs it.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "docs(m9.3): play over the runtime — reference, architecture, changelog, acceptance"
```

---

## Self-review notes

- **Spec coverage:**
  - §4 "one mirror": Task 4 deletes the second one.
  - §4 "one open profile state function": Task 2.
  - §4 defect "SeekCompleted/EndOfTrack/RestartEstablished ignore provenance": removed with `app.rs`'s mirror in Task 4; the runtime's mirror already applies provenance.
  - §4 defect "Legacy clears the cursor": Task 1.
  - §5 "`play` over `PlayerRuntime`, Legacy retires": Tasks 3–4.
  - Not covered (other roadmap items): enricher, cover tracker, navigator, feed operations below `commands`, moving `displayable`.
- **Types:**
  - `LoadTarget::Detached`: Tasks 1, 3, 4.
  - `decide_detached(TransportInput, PlaybackPhase, bool) -> TransportDecision`: Tasks 2, 3.
  - `open_state(StateStore, LoadOutcome, &Arc<dyn Clock>) -> OpenedState`: Tasks 2, 4.
  - `play_detached(MediaId, SourceLocation)` and `poll_budget(Duration) -> Duration`: Tasks 3, 4.
  - `NowPlaying::unloaded()`: Tasks 3, 4.
- **Plan review (2026-10-05):** three gaps verified against the code and fixed in Task 3 (Review Focus 6–8): the flush gate now uses the detached table, a detached `route` works before `Loaded`, and Stop keeps a stored target.
- **Literals to verify at implementation time:** event and `Progress` field lists, `StartDisposition`'s fresh variant, the checkpoint accessor and fixture names. Each step that uses them says so.
