# M10 Finite HTTP Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A range-capable finite HTTP episode that stalls or drops mid-play recovers on its own through the live `Reconnecting` machinery, and resumes at the listener's position or stored intent with no frame repeated or skipped.

**Architecture:**
- All engine work is in `src/playback/engine.rs` (the worker) and `src/playback/reconnect.rs` (pure timing data).
- A finite attempt (`reconnect_finite`) captures and tears down first, then reopens, reseeks and installs the landing as the anchor. It primes inside an attempt-local failure capture (`prime_attempt`) and commits only after a final cancellation check. `restore()` (Space) shares the same commit check.
- A pause during finite recovery is cancelled at submission (`submit_pause` retires the source instead of freezing it).
- The frontend `KeyRouter` holds a stored target until the seek resolves, so arrow bursts accumulate across progress ticks.

**Tech Stack:** Rust 2024, toolchain 1.98.1 (`rust-toolchain.toml`), Symphonia, rtrb, crossbeam-channel. Integration suites in `tests/` with the shared harness in `tests/support/`.

**Spec:** `docs/superpowers/specs/2026-09-29-tenuto-m10-finite-reconnect-design.md` (amended in `667c0b4`). Read §2–§7 before starting; this plan cites them as §N. Also read `docs/architecture.md` §1 (invariants), §4–5 (layers, thread ownership) and §7.5 (live media), as `CLAUDE.md` requires before touching playback code.

## Global Constraints

- **Build gate, after every task:**
  - `cargo fmt --check`
  - `cargo clippy --locked --all-targets --all-features -- -D warnings`
  - the task's own suites
- **Whole tree,** at the end of Tasks 4, 6 and 9: `cargo test --locked --no-fail-fast`. Plain `cargo test` stops at the first failing binary.
- **Runtime code** has no `unsafe`, `unwrap` or `expect`; the lints deny them. Tests use `unwrap_or_else(|error| panic!(...))` or `assert!`, as the existing suites do.
- **Logging:** every URL in a message or log goes through `redact_url`. The new log lines log only `reason = %failure` and `position`; `RemoteFailure`'s `Display` is already redacted.
- **No automatic network:** nothing plays, fetches or refreshes on its own. Recovery attempts continue a current Play: they are cancellable at once, bounded by the budget, and never start after a process restart.
- **Position:** stopping or recreating the transport must never reset the logical position.
- **Layering:** `playback` never imports persistence and never blocks on Tokio; `tui` mutates state only through `Session`.
- **Engine tests:** step the virtual clock only with `play_for`, `play_until_event`, `play_until_terminal`, `let_time_pass` or `let_time_pass_while_unresponsive`. Never a sleep or a fixed-duration advance. Reconnect backoff runs on real `Instant`s, as in the m7 suites, so keep test backoffs at 20 ms, or at 300–400 ms where a test must act inside the backoff.
- **Unused items in test crates fail clippy.** `tests/m10_finite_reconnect.rs` gains each helper and each `use` in the task that first needs it. `tests/support/` is exempt (`#![allow(dead_code)]`).
- **Commits:** no `Co-Authored-By` trailer, no session link. Style: `feat(m10): …`, `fix(m10): …`, `test(m10): …`, `docs(m10): …`.
- **Unchanged suites:** `m7_reconnect`, `m7_live_recovery`, `m7_live_playback` and `m7_cancellation` must pass with no edits, and so must `m4_diagnostics`.

### Decisions beyond the spec

Task 9 records these in the spec.

1. **The seek proof carries across a reopen** (Task 2). A reopened remote source comes back `SeekSupport::Unknown`, which would make every recovery after the first ineligible (§3 needs `ResumeCapability::Supported`). `ensure_source_open` keeps `Native` when the session had already proven it for that location.
2. **Seek frame conversion rounds** (Task 4). `DecodedSource::duration_to_frames` truncates, and positions come from `Duration::from_secs_f64`, which rounds to the nanosecond. At 48 kHz, one frame in three converts back one frame short, so a resume replays one frame. It now rounds, and `adopt_preserved` accepts a sub-tolerance difference in either direction.
3. **The render log already exists.** `TestOutput::captured()` holds every rendered sample since the last `clear_captured()`, not just the last buffer. The harness only exposes it (`TestEngine::rendered` and `clear_rendered`); there is no new opt-in log.
4. **Per-connection faults reuse `Script::then`.** There is no `stall_only_first_response`.
5. **The frame-index fixture encodes `index / 1024` and `index % 1024 + 1`,** not base 32767. The values stay small enough to decode exactly whichever i16→f32 scale Symphonia uses.
6. **A recovery-pause (`Paused`, finite remote, no source, no transport) stores seeks and restarts offline,** exactly as `Reconnecting` does. This is the §7 closing line ("every state where `pending` can be set") made concrete.
7. **Test coverage substitutions:**
   - The Session checkpoint of a stored target is already pinned by `tests/session_policy.rs` (`SeekTargetStored` handling is state-agnostic, `session.rs:915`).
   - The non-retryable priming case is covered through Space's one-attempt path (Task 5).
   - The ring-drain case of spec test 9 is covered by Task 1's unit tests.

## Review Focus

These conditions are implied by the spec but no spec test exercises them, so each gets a test in the task named.

1. **A seek near the end during recovery.** The episode should land, play out and end: `EndOfTrack`, not `Failed` and not a reconnect loop. Task 5: `a_seek_near_the_end_during_recovery_plays_out_and_ends`.
2. **The audio device fails during an attempt.** The attempt should fail at once and land in `Failed`; it must never be retried against the network budget. Task 4: `a_device_that_will_not_open_fails_the_attempt_at_once`.
3. **A new load during recovery.** It should drop the outage and the stored target; no `SeekCompleted` for the old track's target, ever. Task 6: `a_new_load_during_recovery_drops_the_outage_and_the_target`.
4. **Quitting while an attempt is blocked at the server.** The engine should join promptly instead of waiting out a deadline. Task 6: `shutdown_during_a_blocked_attempt_joins_promptly`.
5. **Stop while the attempt is priming.** It should land in `Stopped` once with no `Failed`, and Space should resume. Task 6: `stop_cancels_a_blocked_priming_read_and_space_resumes`.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/playback/reconnect.rs` | modify | `Outage` counts heard time; new `retryable(failure, continuity)` |
| `src/playback/engine.rs` | modify | `PendingResume`, heard settlement, finite entry and attempt, `prime_attempt`, offline intent, recovery pause, `SourceTraits::recovering` |
| `src/playback/decode.rs` | modify | `duration_to_frames` rounds |
| `src/application/seek.rs` | modify | `KeyRouter` stored hold |
| `tests/support/wav.rs` | create | frame-index WAV builder and decoder |
| `tests/support/mod.rs` | modify | `pub mod wav;`, `rendered`/`clear_rendered`, `load_remote_with_resume_and_limits` |
| `tests/m10_finite_reconnect.rs` | create | spec §9 tests and Review Focus tests |
| `docs/architecture.md`, `docs/reference.md`, `CHANGELOG.md`, `docs/m10-acceptance.md`, the spec | modify / create | Task 9 |

---

### Task 1: Heard-time outage accounting

`Outage::is_over(position)` measures position growth since the reconnect, which moves with every seek. This task replaces it with a heard-time counter fed per generation (§6). It changes nothing for stations: heard time equals listening-time growth there, and the m7 suites pin that.

**Files:**
- Modify: `src/playback/reconnect.rs` (`Outage` and its unit tests)
- Modify: `src/playback/engine.rs`: the `Worker` struct and `Worker::new`, `capture_position` (near line 1861), `service_reconnect` (near line 2174)

**Interfaces:**
- Produces:
  - `Outage::playing_from(&mut self)`, which no longer takes a position
  - `Outage::add_heard(&mut self, heard: Duration)`
  - `Outage::is_over(&self, policy: &ReconnectPolicy) -> bool`
  - `Worker::settle_heard(&mut self)`
  - field `heard_mark: Option<(u16, Duration)>`

- [ ] **Step 1: Write the failing unit tests**

In `src/playback/reconnect.rs`'s `mod tests`, replace `the_budget_is_judged_only_when_something_fails` and `short_connections_stay_one_outage_and_thirty_played_seconds_end_it` with the following, and add the two new tests:

```rust
    #[test]
    fn the_budget_is_judged_only_when_something_fails() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        assert!(matches!(outage.failed(t0, &policy()), Next::AttemptAt(_)));
        let late = t0 + Duration::from_secs(301);
        assert!(!outage.is_over(&policy()), "time alone ends nothing");
        assert_eq!(outage.failed(late, &policy()), Next::GiveUp);
    }

    #[test]
    fn short_connections_stay_one_outage_and_thirty_heard_seconds_end_it() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        outage.playing_from();
        assert!(
            !outage.due(t0 + Duration::from_secs(60)),
            "no attempt while playing"
        );
        outage.add_heard(Duration::from_secs(2));
        assert!(!outage.is_over(&policy()));
        // It closed after two seconds: same outage, next backoff step.
        assert_eq!(
            outage.failed(t0 + Duration::from_secs(3), &policy()),
            Next::AttemptAt(t0 + Duration::from_secs(5))
        );
        outage.playing_from();
        outage.add_heard(Duration::from_secs(29));
        assert!(!outage.is_over(&policy()));
        outage.add_heard(Duration::from_secs(1));
        assert!(outage.is_over(&policy()));
    }

    #[test]
    fn heard_time_counts_only_once_a_reconnect_is_playing() {
        // M10 §6: the ring drains during backoff, and that audio is heard,
        // but no reconnect has started playing yet, so it is not stability.
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        outage.add_heard(Duration::from_secs(60));
        assert!(!outage.is_over(&policy()));
        outage.playing_from();
        assert!(!outage.is_over(&policy()), "the window starts from zero");
    }

    #[test]
    fn a_failure_closes_the_window_and_forgets_what_was_heard() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        outage.playing_from();
        outage.add_heard(Duration::from_secs(29));
        outage.failed(t0 + Duration::from_secs(30), &policy());
        outage.add_heard(Duration::from_secs(5));
        outage.playing_from();
        outage.add_heard(Duration::from_secs(1));
        assert!(!outage.is_over(&policy()));
    }
```

- [ ] **Step 2: Run them to see the failure**

Run: `cargo test --locked --lib playback::reconnect`
Expected: compile errors. `playing_from` takes a position, `add_heard` does not exist, and `is_over` takes two arguments.

- [ ] **Step 3: Replace the `Outage` data and methods**

In `src/playback/reconnect.rs`, replace the `Outage` struct and its `impl` with:

```rust
#[derive(Clone, Debug)]
pub struct Outage {
    started: Instant,
    failures: usize,
    next_attempt_at: Instant,
    /// Whether a reconnect is playing and its stability window has started.
    window_open: bool,
    /// Audio heard since the window opened (M10 §6). Seeks move the position
    /// but not this, so they neither end nor extend the window.
    heard: Duration,
}

impl Outage {
    pub fn begin(now: Instant) -> Self {
        Self {
            started: now,
            failures: 0,
            next_attempt_at: now,
            window_open: false,
            heard: Duration::ZERO,
        }
    }

    /// A playing connection or an attempt failed.
    pub fn failed(&mut self, now: Instant, policy: &ReconnectPolicy) -> Next {
        self.window_open = false;
        self.heard = Duration::ZERO;
        if now.duration_since(self.started) >= policy.budget {
            return Next::GiveUp;
        }
        let step = self.failures.min(policy.backoff.len() - 1);
        self.failures += 1;
        self.next_attempt_at = now + policy.backoff[step];
        Next::AttemptAt(self.next_attempt_at)
    }

    pub fn due(&self, now: Instant) -> bool {
        !self.window_open && now >= self.next_attempt_at
    }

    /// A reconnect started playing: the stability window opens at zero.
    pub fn playing_from(&mut self) {
        self.window_open = true;
        self.heard = Duration::ZERO;
    }

    /// Credit audio heard since the last reading. Ignored until a reconnect
    /// is playing, so ring drain during backoff never counts.
    pub fn add_heard(&mut self, heard: Duration) {
        if self.window_open {
            self.heard = self.heard.saturating_add(heard);
        }
    }

    pub fn is_over(&self, policy: &ReconnectPolicy) -> bool {
        self.window_open && self.heard >= policy.stable_after
    }
}
```

Also change `ReconnectPolicy::stable_after`'s doc comment to: `/// Heard audio after a reconnect that ends the outage. Played audio only: bytes, decoded frames and seeks do not count.`

- [ ] **Step 4: Run the unit tests**

Run: `cargo test --locked --lib playback::reconnect`
Expected: the library does not compile yet (`engine.rs` still calls the old API). Go on to Step 5.

- [ ] **Step 5: Feed heard time from the worker**

In `src/playback/engine.rs`:

(a) Add the field to `struct Worker`, after `outage`:

```rust
    /// Where heard-time accounting last read the current generation (M10
    /// §6): its number and how far past its anchor the position was.
    heard_mark: Option<(u16, Duration)>,
```

and initialise it in `Worker::new` after `outage: None,`:

```rust
            heard_mark: None,
```

(b) Add this method next to `capture_position`:

```rust
    /// M10 §6: credit the outage with what was heard since the last reading.
    /// A new generation (a seek, a reopen, a device rebuild) starts again from
    /// its own anchor, so a seek moves nothing here.
    fn settle_heard(&mut self) {
        let Some(anchor) = lock(&self.transport).as_ref().map(|core| core.anchor) else {
            return;
        };
        let generation = self.generation;
        let offset = self.position.saturating_sub(anchor);
        let since = match self.heard_mark {
            Some((marked, last)) if marked == generation => offset.saturating_sub(last),
            _ => offset,
        };
        self.heard_mark = Some((generation, offset));
        if let Some(outage) = self.outage.as_mut() {
            outage.add_heard(since);
        }
    }
```

(c) In `capture_position`, settle the generation's final delta in the `Ok` arm:

```rust
        match captured {
            Ok(position) => {
                self.position = position;
                self.settle_heard();
                true
            }
            Err(_) => false,
        }
```

(d) In `service_reconnect`, replace the `Playing` arm with:

```rust
            PlaybackState::Playing => {
                self.settle_heard();
                let policy = *lock(&self.reconnect_policy);
                if self
                    .outage
                    .as_ref()
                    .is_some_and(|outage| outage.is_over(&policy))
                {
                    self.outage = None;
                }
            }
```

and in the `Reconnecting` arm's `Ok(())` branch, replace the body with:

```rust
                    Ok(()) => {
                        if let Some(outage) = self.outage.as_mut() {
                            outage.playing_from();
                        }
                    }
```

- [ ] **Step 6: Run the unit tests and the live suites**

Run: `cargo test --locked --lib playback::reconnect && cargo test --locked --test m7_reconnect --test m7_live_recovery --test m7_live_playback --test m7_cancellation`
Expected: all PASS, with no edits to the m7 suites.

- [ ] **Step 7: Lint and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings
git add src/playback/reconnect.rs src/playback/engine.rs
git commit -m "feat(m10): count heard time, not position growth, toward outage stability"
```

---

### Task 2: A reopen keeps a seek the session already proved

`ensure_source_open` adopts the reopened source's capabilities. A fresh `HttpMediaSource` has an unproven demuxer, so a session proven `Native` falls back to `Unknown` on every reopen. That would make every recovery after the first ineligible, and it makes a stopped seek run a needless trial. This is decision 1 under Global Constraints.

**Files:**
- Create: `tests/support/wav.rs`
- Modify: `tests/support/mod.rs` (add `pub mod wav;` after `pub mod server;`)
- Create: `tests/m10_finite_reconnect.rs`
- Modify: `src/playback/engine.rs` (`ensure_source_open`, near line 3431)

**Interfaces:**
- Produces, for later tasks:
  - `support::wav::{RATE, frame_index_wav, frame_indices}`
  - in the m10 file: `FRAMES`, `RESUME`, `episode()`, `quick()`, `start(server, policy)`, `state(wanted)`, `stored(event)`, `stored_target(event)`

- [ ] **Step 1: Create the frame-index fixture**

Create `tests/support/wav.rs`:

```rust
//! Frame-index WAV fixtures (M10 §9): every frame encodes its own index, so a
//! render log says exactly which frames played, and in what order.

/// The harness device's rate, so nothing is resampled between the file and
/// the render log.
pub const RATE: u32 = 48_000;

/// Small on purpose: both channels stay far below the i16 range, so a frame
/// decodes back exactly whether Symphonia scales by 32767 or 32768.
const BASE: u32 = 1024;

/// A stereo, 16-bit PCM WAV of `frames` frames at [`RATE`]. Frame `i` holds
/// `i / BASE` on the left and `i % BASE + 1` on the right. The right channel
/// is never zero, so no frame equals silence and dropping silence from a
/// render log can never hide a frame.
pub fn frame_index_wav(frames: u32) -> Vec<u8> {
    let data_len = frames * 4;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // channels
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 4).to_le_bytes()); // byte rate
    wav.extend_from_slice(&4u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for index in 0..frames {
        let left = (index / BASE) as i16;
        let right = (index % BASE + 1) as i16;
        wav.extend_from_slice(&left.to_le_bytes());
        wav.extend_from_slice(&right.to_le_bytes());
    }
    wav
}

/// Decodes an interleaved stereo render log back to frame indices, dropping
/// silence (both channels zero), which no encoded frame ever is.
pub fn frame_indices(samples: &[f32]) -> Vec<u32> {
    samples
        .chunks_exact(2)
        .filter(|frame| frame[0] != 0.0 || frame[1] != 0.0)
        .map(|frame| {
            let left = (frame[0] * 32768.0).round() as u32;
            let right = (frame[1] * 32768.0).round() as u32;
            left * BASE + right - 1
        })
        .collect()
}
```

Add `pub mod wav;` to `tests/support/mod.rs` after `pub mod server;`.

- [ ] **Step 2: Create the m10 suite with its first test**

Create `tests/m10_finite_reconnect.rs`:

```rust
//! M10: a range-capable finite HTTP episode that drops mid-play recovers and
//! keeps playing from where the listener was (spec
//! `docs/superpowers/specs/2026-09-29-tenuto-m10-finite-reconnect-design.md`).

mod support;

use std::time::Duration;

use support::TestEngine;
use support::server::{Script, TestServer};
use support::wav::{RATE, frame_index_wav};
use tenuto::media::capabilities::SeekSupport;
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;

/// Four seconds: long enough to drop, recover and keep playing.
const FRAMES: u32 = 4 * RATE;

/// Where every session starts. Nonzero, so the load's own resume seek proves
/// the demuxer seekable and the episode is eligible (§3).
const RESUME: Duration = Duration::from_millis(500);

fn episode() -> Script {
    Script::serving(frame_index_wav(FRAMES))
}

fn quick() -> ReconnectPolicy {
    ReconnectPolicy {
        backoff: [Duration::from_millis(20); 5],
        budget: Duration::from_secs(2),
        stable_after: Duration::from_secs(10),
    }
}

fn state(wanted: PlaybackState) -> impl Fn(&PlaybackEvent) -> bool {
    move |event| matches!(event, PlaybackEvent::StateChanged { state, .. } if *state == wanted)
}

fn stored(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::SeekTargetStored { .. })
}

fn stored_target(event: PlaybackEvent) -> Duration {
    let PlaybackEvent::SeekTargetStored { target, .. } = event else {
        unreachable!("the caller matched SeekTargetStored")
    };
    target
}

/// Load at [`RESUME`] and play. Connection 1 is the probe; connection 2 is
/// the resume seek, and it is the connection that plays.
fn start(server: &TestServer, policy: ReconnectPolicy) -> TestEngine {
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(policy);
    engine.load_remote_with_resume(
        &server.url("/episode.wav"),
        ResumeIntent::StartAt(RESUME),
    );
    assert_eq!(
        engine.await_loaded().capabilities.seek,
        SeekSupport::Native,
        "the resume seek must prove the episode seekable"
    );
    // Consumed here, so a later wait can only match a transition the test
    // itself caused.
    engine.await_event(state(PlaybackState::Paused));
    engine.send(PlaybackCommand::Play);
    engine.await_event(state(PlaybackState::Playing));
    engine
}

#[test]
fn a_reopen_keeps_a_seek_the_session_already_proved() {
    let server = TestServer::start(episode());
    let mut engine = start(&server, quick());
    assert_eq!(server.requests().len(), 2, "a probe, then the resume seek");
    let capabilities = |event: &PlaybackEvent| {
        matches!(event, PlaybackEvent::CapabilitiesChanged { .. })
    };
    let announced = engine.count_events(capabilities);

    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    assert_eq!(
        stored_target(engine.await_event(stored)),
        Duration::from_secs(2)
    );

    assert_eq!(
        engine.count_events(capabilities),
        announced,
        "a reopen of a proven episode must not fall back to Unknown"
    );
    assert_eq!(
        server.requests().len(),
        3,
        "the stopped seek must reopen once and run no trial seek"
    );
    engine.finish();
    server.shutdown();
}
```

- [ ] **Step 3: Run it to see the failure**

Run: `cargo test --locked --test m10_finite_reconnect`
Expected: FAIL. Either the `CapabilitiesChanged` count rises (`Unknown`, then `Native` after the trial), or the request count is above 3.

- [ ] **Step 4: Carry the proof in `ensure_source_open`**

In `src/playback/engine.rs` `ensure_source_open`, change `let prepared = prepare(&location, &context)?;` to `let mut prepared = ...`. Then insert the following after the `is_retired()` cancellation check and before the `if self.capabilities != prepared.capabilities` comparison:

```rust
        // M10: a reopen of the same finite location reads the same container,
        // so a demuxer this session already proved seekable stays proven.
        // Without this every reopen falls back to `Unknown`: an episode would
        // recover from its first outage but never from a second, and a
        // stopped seek would spend a trial seek re-proving what it knew.
        if self.capabilities.seek == SeekSupport::Native
            && prepared.capabilities.continuity == Continuity::Finite
            && prepared.capabilities.seek == SeekSupport::Unknown
        {
            prepared.source.note_demuxer_proven();
            prepared.capabilities = prepared.source.capabilities();
        }
```

- [ ] **Step 5: Run the m10 suite and the remote suites**

Run: `cargo test --locked --test m10_finite_reconnect --test engine_remote --test http_playback`
Expected: PASS. `engine_remote`'s `a_capability_change_carries_the_current_session_rev` loads at zero and never proves the source, so it still sees the trial.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings
git add tests/support/wav.rs tests/support/mod.rs tests/m10_finite_reconnect.rs src/playback/engine.rs
git commit -m "fix(m10): a reopen keeps the seek proof the session already has"
```

---

### Task 3: `PendingResume`, read rather than taken

`requested_target: Option<Duration>` becomes `pending: Option<PendingResume>` (§4). `restore()` reads it instead of taking it, and clears it only after the transport is installed. Today a failed reseek inside `restore()` loses the stored target. `SeekBy` now bases on the stored intent.

**Files:**
- Modify: `src/playback/engine.rs`: the struct field and `Worker::new`, `dispatch`'s `SeekBy` arm (near line 2401), `load` (near line 2447), `play` (near line 2700), `restore` (near line 2729), `seek_to` (near lines 3050 and 3104), `restart` (near lines 3228 and 3236)
- Modify: `tests/m10_finite_reconnect.rs`

**Interfaces:**
- Produces:
  - `enum PendingResume { Seek(Duration), Restart }` with `fn target(self) -> Duration`
  - `Worker::complete_pending(&mut self, actual: Duration)`
  - `Worker::landing_provenance(&self, actual: Duration, provenance: PositionProvenance) -> PositionProvenance`
  - in the m10 file: `PATIENCE`, `server(playing, attempts)`, `failed(event)`

- [ ] **Step 1: Write the failing tests**

Append to `tests/m10_finite_reconnect.rs`:

```rust
const PATIENCE: Duration = Duration::from_secs(20);

/// Connection 1 probes and connection 2 plays (`playing`); `attempts` script
/// connections 3 onward. The last entry answers every later connection, so
/// end the list with `episode()`.
fn server(playing: Script, attempts: Vec<Script>) -> TestServer {
    let mut script = episode().then(playing);
    for next in attempts {
        script = script.then(next);
    }
    TestServer::start(script)
}

fn failed(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::Failed { .. })
}

#[test]
fn a_failed_resume_keeps_the_stored_target_for_the_next_play() {
    // 3: the stopped seek's reopen. 4: Space's range request, refused.
    // 5 + 6: the second Space reopens and lands.
    let server = server(
        episode(),
        vec![
            episode(),
            Script::serving(Vec::new()).status(503),
            episode(),
        ],
    );
    let mut engine = start(&server, quick());
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    let target = Duration::from_secs(2);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(failed);

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(
        landed.actual.abs_diff(target) < Duration::from_millis(1),
        "the second Space resumed at {:?}, not the stored {target:?}",
        landed.actual
    );
    engine.await_event(state(PlaybackState::Playing));
    engine.finish();
    server.shutdown();
}

#[test]
fn seek_by_while_stopped_accumulates_on_the_stored_target() {
    let server = TestServer::start(episode());
    let mut engine = start(&server, quick());
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    engine.await_event(stored);
    engine.send(PlaybackCommand::SeekBy(1));
    assert_eq!(
        stored_target(engine.await_event(stored)),
        Duration::from_secs(3),
        "SeekBy must base on the stored target, not the stopped position"
    );
    engine.finish();
    server.shutdown();
}
```

- [ ] **Step 2: Run them to see the failure**

Run: `cargo test --locked --test m10_finite_reconnect`
Expected:
- the first test FAILS: no `SeekCompleted` arrives (panic after 20 s), because the first Space took the target;
- the second FAILS with a stored target of about 1.5 s.

- [ ] **Step 3: Introduce `PendingResume`**

In `src/playback/engine.rs`, add near the other private types above `struct Worker`:

```rust
/// M10 §4: what a resume must establish, held until a landing is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingResume {
    Seek(Duration),
    #[expect(dead_code, reason = "constructed by the offline restart (M10 Task 5)")]
    Restart,
}

impl PendingResume {
    fn target(self) -> Duration {
        match self {
            Self::Seek(target) => target,
            Self::Restart => Duration::ZERO,
        }
    }
}
```

Rename the field `requested_target: Option<Duration>` to:

```rust
    /// A seek or restart stored but not yet established (M10 §4). Read, never
    /// taken, by every resume; cleared only once a landing is installed.
    pending: Option<PendingResume>,
```

and in `Worker::new` change `requested_target: None,` to `pending: None,`.

- [ ] **Step 4: Update every site**

Make these edits in `src/playback/engine.rs`:

- `dispatch`, `SeekBy` arm: `let base = self.position;` becomes:

  ```rust
                  // M10 §7: presses accumulate on a stored intent.
                  let base = self.pending.map_or(self.position, PendingResume::target);
  ```

- `load`: `self.requested_target = None;` becomes `self.pending = None;`.
- `play`, the resume guard: `&& self.requested_target.is_none()` becomes `&& self.pending.is_none()`.
- `restore`, indefinite branch: `if self.requested_target.take().is_some()` becomes `if self.pending.take().is_some()`.
- `seek_to`, stopped arm: `self.requested_target = Some(target);` becomes `self.pending = Some(PendingResume::Seek(target));`.
- `seek_to`, landing arm: `self.requested_target = None;` becomes `self.pending = None;`.
- `restart`, both branches: `self.requested_target = None;` becomes `self.pending = None;`.

- [ ] **Step 5: Read, don't take, in `restore`**

Replace `restore`'s tail, from the comment `// A target stored while stopped was never validated` to the end of the function, with:

```rust
        // M10 §4: read, not taken. A reseek or install that fails leaves the
        // intent for the next Space and for the checkpoint.
        let target = self.pending.map_or(self.position, PendingResume::target);
        let landed = match self.reseek(target) {
            Ok((actual, provenance)) => {
                self.position = adopt_preserved(target, actual);
                self.position_provenance = self.landing_provenance(actual, provenance);
                actual
            }
            // A stop or a shutdown arrived mid-refinement. The preserved
            // position still stands; the interrupt is handled by the loop.
            Err(error) if is_cancelled(&error) => return,
            Err(error) => {
                self.fail(format!("cannot resume at {target:?}: {error}"));
                return;
            }
        };
        match self.reinstall(true) {
            Ok(()) => {
                self.announce_playing();
                self.complete_pending(landed);
            }
            Err(error) if is_cancelled(&error) => {}
            Err(error) => self.fail(format!("cannot start the audio device: {error}")),
        }
    }

    /// M10 §4: a stored intent is cleared only once its landing is installed,
    /// and only then reported: `SeekCompleted` for a seek, `RestartEstablished`
    /// for a restart. Emitting at store time would claim a landing no decoder
    /// had confirmed.
    fn complete_pending(&mut self, actual: Duration) {
        let session_rev = self.session_rev;
        let provenance = self.position_provenance;
        match self.pending.take() {
            Some(PendingResume::Seek(requested)) => {
                // §11: requested/actual seek.
                tracing::debug!(?requested, ?actual, "stored seek target confirmed");
                self.emit(PlaybackEvent::SeekCompleted {
                    session_rev,
                    requested,
                    actual,
                    refinement_truncated: false,
                    provenance,
                });
            }
            Some(PendingResume::Restart) => {
                let position = self.position;
                self.emit(PlaybackEvent::RestartEstablished {
                    session_rev,
                    position,
                    provenance,
                });
            }
            None => {}
        }
    }

    /// A restart that lands at zero is exact, whatever provenance a reseek
    /// that had nothing to do carried forward (M10 §4).
    fn landing_provenance(
        &self,
        actual: Duration,
        provenance: PositionProvenance,
    ) -> PositionProvenance {
        if self.pending == Some(PendingResume::Restart) && actual == Duration::ZERO {
            PositionProvenance::Established
        } else {
            provenance
        }
    }
```

Keep everything in `restore` above that comment (the indefinite branch, the `Unsupported` gate, `ensure_source_open`, the `source.is_none()` warning and the `capture_position`) as it is.

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked --test m10_finite_reconnect --test engine_remote --test engine_contract --test session_policy`
Expected: PASS.

- [ ] **Step 7: Lint and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings
git add src/playback/engine.rs tests/m10_finite_reconnect.rs
git commit -m "fix(m10): a stored intent survives a failed resume; SeekBy bases on it"
```

---

### Task 4: Finite recovery

This task adds finite entry into `Reconnecting` (§3), the attempt (§5) and the retry classifier. It also makes seek frame conversion round, so that resuming at a captured position replays nothing (decision 2).

**Files:**
- Modify: `src/playback/reconnect.rs` (add `retryable` and its test)
- Modify: `src/playback/engine.rs`
- Modify: `src/playback/decode.rs` (`duration_to_frames`, near line 374)
- Modify: `tests/support/mod.rs` (`rendered`, `clear_rendered`)
- Modify: `tests/m10_finite_reconnect.rs`

**Interfaces:**
- Consumes: `PendingResume`, `complete_pending` and `landing_provenance` (Task 3); `Outage::playing_from()` (Task 1).
- Produces:
  - `pub fn retryable(failure: &RemoteFailure, continuity: Continuity) -> bool` in `reconnect.rs`
  - `Worker::recoverable(&self, &PlaybackError) -> Option<RemoteFailure>`
  - `Worker::reconnect_finite(&mut self) -> Result<(), PlaybackError>`
  - `Worker::prime_attempt(&mut self) -> Result<(), PlaybackError>`
  - `attempt_failure: Option<PlaybackError>`
  - `TestEngine::rendered(&self) -> Vec<f32>` and `TestEngine::clear_rendered(&mut self)`
  - in the m10 file: `BYTES_PER_SEC`, `CUT`, `frame_at`, `assert_consecutive`

- [ ] **Step 1: The classifier, test first**

Append to `src/playback/reconnect.rs`'s `mod tests`:

```rust
    #[test]
    fn retry_classification_follows_continuity() {
        use crate::http::error::{Operation, Phase};
        let truncated = RemoteFailure::TruncatedBody { missing: 1 };
        for continuity in [Continuity::Indefinite, Continuity::Finite] {
            let transport = RemoteFailure::Transport {
                operation: Operation::Read,
                detail: String::new(),
            };
            assert!(retryable(&transport, continuity));
            assert!(retryable(
                &RemoteFailure::Timeout { phase: Phase::Stall },
                continuity
            ));
            assert!(retryable(
                &RemoteFailure::Status {
                    status: 503,
                    operation: Operation::Open
                },
                continuity
            ));
            assert!(!retryable(&RemoteFailure::ResourceChanged, continuity));
            assert!(!retryable(
                &RemoteFailure::Status {
                    status: 404,
                    operation: Operation::Open
                },
                continuity
            ));
        }
        assert!(retryable(&RemoteFailure::LiveEnded, Continuity::Indefinite));
        assert!(!retryable(&truncated, Continuity::Indefinite));
        assert!(retryable(&truncated, Continuity::Finite));
    }
```

Run: `cargo test --locked --lib playback::reconnect`. Expected: FAIL to compile (`retryable` does not exist).

Then add to `src/playback/reconnect.rs`, after the `Next` enum:

```rust
use crate::http::error::RemoteFailure;
use crate::media::capabilities::Continuity;

/// M10 §3: whether trying the same location again can help, for media of
/// this continuity. A station's body has no declared length, so it is never
/// "truncated", and only a station can end live. A finite body that ended
/// early can be fetched again from where it broke.
pub fn retryable(failure: &RemoteFailure, continuity: Continuity) -> bool {
    match failure {
        RemoteFailure::LiveEnded => continuity == Continuity::Indefinite,
        RemoteFailure::TruncatedBody { .. } => continuity == Continuity::Finite,
        other => other.is_retryable(),
    }
}
```

Move the two `use` lines up to the existing `use std::time::...` line. Run the unit tests again. Expected: PASS.

- [ ] **Step 2: Harness access to the render log**

In `tests/support/mod.rs`, add next to `captured_is_audible`:

```rust
    /// Every sample the virtual device rendered since the last
    /// `clear_rendered`, interleaved. `TestOutput` already keeps the whole
    /// log; this only hands it out.
    pub fn rendered(&self) -> Vec<f32> {
        lock(&self.device).output.captured().to_vec()
    }

    pub fn clear_rendered(&mut self) {
        lock(&self.device).output.clear_captured();
    }
```

- [ ] **Step 3: Write the failing recovery tests**

In `tests/m10_finite_reconnect.rs`, add `frame_indices` to the `support::wav` import, then append:

```rust
/// Bytes per second of the fixture: stereo, 16-bit.
const BYTES_PER_SEC: usize = RATE as usize * 4;

/// The playing connection drops one second of audio past [`RESUME`].
const CUT: usize = BYTES_PER_SEC;

fn frame_at(position: Duration) -> u32 {
    (position.as_secs_f64() * f64::from(RATE)).round() as u32
}

fn assert_consecutive(indices: &[u32]) {
    assert!(!indices.is_empty(), "nothing was rendered");
    if let Some(at) = indices.windows(2).position(|pair| pair[1] != pair[0] + 1) {
        panic!(
            "frame {} followed frame {} (render index {at}): frames were replayed or skipped",
            indices[at + 1],
            indices[at]
        );
    }
}

#[test]
fn a_dropped_connection_resumes_with_no_frame_repeated_or_skipped() {
    // The backoff (20 ms) is shorter than the 300 ms ring, so the attempt
    // runs with audio still queued: the capture must account for exactly
    // what was heard and discard exactly the rest.
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(&server, quick());
    engine.clear_rendered();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    let landed = engine.position();
    engine.play_for(landed + Duration::from_millis(500));

    let indices = frame_indices(&engine.rendered());
    assert_eq!(indices.first().copied(), Some(frame_at(RESUME)));
    assert_consecutive(&indices);
    let cut = frame_at(RESUME) + RATE;
    assert!(
        indices.last().is_some_and(|last| *last > cut + RATE / 4),
        "playback did not carry on past the cut: last frame {:?}",
        indices.last()
    );
    assert_eq!(engine.count_events(failed), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_truncated_reopen_or_reseek_is_retried_inside_the_same_outage() {
    // 3: the first attempt's reopen ends 20 bytes in, inside the header.
    // 4 + 5: the second reopens, then its range response ends 10 bytes in.
    // 6 + 7: the third lands.
    let server = server(
        episode().truncate_body_after(CUT),
        vec![
            episode().truncate_body_after(20),
            episode(),
            episode().truncate_body_after(10),
            episode(),
        ],
    );
    let mut engine = start(&server, quick());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(server.requests().len(), 7);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_range_less_server_still_fails() {
    let server = TestServer::start(episode().without_ranges().truncate_body_after(CUT));
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(quick());
    engine.load_remote(&server.url("/episode.wav"));
    engine.send(PlaybackCommand::Play);
    engine.play_until_terminal(PATIENCE);
    assert_eq!(engine.state(), PlaybackState::Failed);
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn an_episode_never_proved_seekable_still_fails() {
    // Ranged, but loaded at zero: nothing ever demonstrated a seek, so the
    // resume capability is Undetermined (§3).
    let server = TestServer::start(episode().truncate_body_after(CUT));
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(quick());
    engine.load_remote(&server.url("/episode.wav"));
    engine.send(PlaybackCommand::Play);
    engine.play_until_terminal(PATIENCE);
    assert_eq!(engine.state(), PlaybackState::Failed);
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_failure_while_priming_is_the_attempts_not_the_sessions() {
    // 3 + 4: the first attempt reopens and reseeks, then its range response
    // ends 24 KiB in: past the reseek, inside the priming fill.
    // 5 + 6: the second attempt lands.
    let server = server(
        episode().truncate_body_after(CUT),
        vec![episode(), episode().truncate_body_after(24 * 1024), episode()],
    );
    let mut engine = start(&server, quick());
    engine.clear_rendered();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    let landed = engine.position();
    engine.play_for(landed + Duration::from_millis(300));

    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(
        engine.count_events(state(PlaybackState::Playing)),
        0,
        "a failed attempt announced Playing"
    );
    assert_eq!(server.requests().len(), 6);
    assert_consecutive(&frame_indices(&engine.rendered()));
    engine.finish();
    server.shutdown();
}

#[test]
fn past_the_budget_it_fails_and_space_tries_exactly_once() {
    let server = server(
        episode().truncate_body_after(CUT),
        vec![Script::serving(Vec::new()).status(503)],
    );
    let mut engine = start(
        &server,
        ReconnectPolicy {
            budget: Duration::from_millis(300),
            ..quick()
        },
    );
    engine.play_until_terminal(PATIENCE);
    assert_eq!(engine.state(), PlaybackState::Failed);
    let before = server.requests().len();
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Failed);
    assert_eq!(server.requests().len(), before + 1, "Space is one attempt");
    engine.finish();
    server.shutdown();
}

#[test]
fn a_device_that_will_not_open_fails_the_attempt_at_once() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(
        &server,
        ReconnectPolicy {
            backoff: [Duration::from_millis(300); 5],
            ..quick()
        },
    );
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.silence_the_device();
    engine.await_state(PlaybackState::Failed);
    let after = server.requests().len();
    assert_eq!(
        engine.count_events(state(PlaybackState::Reconnecting)),
        0,
        "a device failure was retried against the network budget"
    );
    assert_eq!(server.requests().len(), after);
    engine.finish();
    server.shutdown();
}
```

If a request count assertion is off by a connection, print `server.requests()` (`RecordedRequest` is `Debug`) and fix the scripted chain's comment and count. Never loosen the assertion into an inequality.

- [ ] **Step 4: Run them to see the failure**

Run: `cargo test --locked --test m10_finite_reconnect`
Expected:
- the drop, truncation, priming and budget tests FAIL: the episode lands in `Failed` with no `Reconnecting`;
- the two ineligible tests PASS already, as they pin today's behaviour;
- the device test FAILS (no `Reconnecting` to begin with).

- [ ] **Step 5: Widen `attempt_failure`**

In `src/playback/engine.rs`:

- Change the field to `attempt_failure: Option<PlaybackError>,` and its doc comment to: `/// Whether an attempt (a station's \`fresh_open\`, a finite \`prime_attempt\`) is priming a source it has not committed to yet (M7 §5.2, M10 §5). While set, a read failure is recorded in \`attempt_failure\` instead of transitioning the session.` Put that comment on `attempting`, which sits just above.
- In `source_ended`, change `self.attempt_failure = Some(failure);` to `self.attempt_failure = Some(failure.into());`, and `if failure.is_retryable()` to `if retryable(&failure, Continuity::Indefinite)`.
- In `fresh_open`, change the `if let Some(failure) = failure` block to:

  ```rust
          if let Some(failure) = failure {
              self.abandon_attempt();
              return Err(failure);
          }
  ```

- Change the import `use super::reconnect::{Next, Outage, ReconnectPolicy};` to `use super::reconnect::{Next, Outage, ReconnectPolicy, retryable};`, and add `ResumeCapability` to the `crate::media::capabilities` import.

- [ ] **Step 6: Finite entry from `pump_audio`**

In `pump_audio`'s `match decoded`, insert this arm after the `Err(error) if self.is_indefinite() => { … }` arm:

```rust
                // M10 §5 step 6: while an attempt primes a remote source it
                // has not committed to, the source's death is the attempt's
                // failure. A local file keeps M1's drain-what-it-has rule
                // below, even while `restore` primes it.
                Err(error) if self.attempting && self.source_is_remote() => {
                    self.attempt_failure = Some(error);
                    self.source = None;
                    return;
                }
```

Then turn the final `Err(error) => match (remote_cause(&error), self.source_is_remote()) { … },` arm into a block that tries recovery first:

```rust
                Err(error) => {
                    // M10 §3: an eligible finite episode recovers rather than
                    // failing. Everything else takes the path below unchanged.
                    if let Some(failure) = self.recoverable(&error) {
                        self.enter_reconnecting(failure);
                        return;
                    }
                    match (remote_cause(&error), self.source_is_remote()) {
                        // … the three existing arms, unchanged …
                    }
                }
```

Add the predicate next to `source_ended`:

```rust
    /// M10 §3: the failure a finite read error recovers from, or `None` when
    /// it takes today's path. Only a playing, remote session whose resume
    /// capability is `Supported` recovers, and only from a failure the retry
    /// table calls retryable for finite media.
    fn recoverable(&self, error: &PlaybackError) -> Option<RemoteFailure> {
        let eligible = self.state == PlaybackState::Playing
            && self.source_is_remote()
            && self.capabilities.resume_capability() == ResumeCapability::Supported;
        if !eligible {
            return None;
        }
        remote_cause(error).filter(|failure| retryable(failure, Continuity::Finite))
    }
```

In `enter_reconnecting`, replace the `tracing::info!` line with:

```rust
                if self.is_indefinite() {
                    tracing::info!(reason = %failure, "live source lost; reconnecting");
                } else {
                    let position = self.position;
                    tracing::info!(reason = %failure, ?position, "connection lost; reconnecting");
                }
```

Update its doc comment's first line to `/// M7 §7, M10 §3. A playing connection, or an attempt, failed retryably.`

- [ ] **Step 7: The finite attempt**

In `service_reconnect`'s `Reconnecting` arm, replace `match self.fresh_open() {` with:

```rust
                let attempt = if self.is_indefinite() {
                    self.fresh_open()
                } else {
                    self.reconnect_finite()
                };
                match attempt {
```

In the same match's generic `Err(error)` branch:
- change the comment `` // `fresh_open` may have torn the old transport down `` to `` // The attempt may have torn the old transport down ``;
- change `if failure.is_retryable() {` to `if retryable(&failure, self.capabilities.continuity) {`.

Add after `abandon_attempt`:

```rust
    /// M10 §5: one recovery attempt for a finite source.
    ///
    /// `Ok` means `Playing` was announced and the pending intent, if any, was
    /// reported. On `Err` nothing of the attempt remains: no transport, no
    /// decoder, `pending` untouched and the position as it was captured.
    fn reconnect_finite(&mut self) -> Result<(), PlaybackError> {
        // 1. Capture and tear down before any network I/O, so the ring cannot
        //    move once the position is read. A no-op after the first attempt,
        //    which already left no transport.
        self.capture_and_teardown();
        let previous = (self.position, self.position_provenance);
        // 2.
        let target = self.pending.map_or(self.position, PendingResume::target);
        // 3 + 4. `ensure_source_open` sets `expected = Finite`, so a reopen
        //    that answers as a station fails with `ResourceChanged`.
        let landing = self
            .ensure_source_open()
            .and_then(|_| self.reseek(target));
        let (actual, provenance) = match landing {
            Ok(landing) => landing,
            Err(error) => {
                self.abandon_attempt();
                return Err(error);
            }
        };
        // 5. The landing becomes the anchor: `open_transport` copies
        //    `self.position` into the new `TransportCore`, so a landing
        //    assigned afterwards would play from the target while progress
        //    and every later capture counted from the old position.
        self.position = adopt_preserved(target, actual);
        self.position_provenance = self.landing_provenance(actual, provenance);
        // 6 + 7.
        if let Err(error) = self.prime_attempt() {
            (self.position, self.position_provenance) = previous;
            return Err(error);
        }
        // 8.
        self.start_running();
        self.announce_playing();
        self.complete_pending(actual);
        Ok(())
    }

    /// M10 §5 steps 6–7, shared by `reconnect_finite` and `restore`: install
    /// and prime with a read failure reported here rather than through
    /// `fail_with`, then commit only if nothing failed and nothing cancelled.
    /// `Ok` leaves the transport primed and parked, for the caller to start;
    /// `Err` leaves no transport and no decoder.
    ///
    /// No `primed` check, unlike `fresh_open`: a finite target at the very
    /// end primes nothing and is still a landing, which end-of-track follows.
    fn prime_attempt(&mut self) -> Result<(), PlaybackError> {
        self.attempting = true;
        self.attempt_failure = None;
        let opened = self.reinstall(false);
        self.attempting = false;
        // First, as in `fresh_open`: a stop, shutdown, pause or seek retires
        // the priming read, and `pump_audio` answers that quietly. Remote
        // only: `do_stop` retires the shared interrupt and a local source
        // never begins a new generation, so a local Stop → Play would read
        // as cancelled forever.
        let retired = self.source_is_remote() && self.source_interrupt.is_retired();
        if retired || self.interrupted() {
            self.abandon_attempt();
            return Err(PlaybackError::Cancelled);
        }
        let failure = self.attempt_failure.take();
        if let Err(error) = opened {
            self.abandon_attempt();
            return Err(error);
        }
        if let Some(error) = failure {
            self.abandon_attempt();
            return Err(error);
        }
        Ok(())
    }
```

- [ ] **Step 8: Round seek frame conversion**

In `src/playback/decode.rs`, change `duration_to_frames` to:

```rust
    /// Nearest frame, not floor: positions reach here through
    /// `Duration::from_secs_f64`, which rounds to the nanosecond, and at
    /// 48 kHz one frame in three converts back one frame short under a
    /// floor, so a resume at a captured position would replay a frame.
    fn duration_to_frames(&self, value: Duration) -> u64 {
        (value.as_secs_f64() * f64::from(self.sample_rate)).round() as u64
    }
```

In `src/playback/engine.rs`, make `adopt_preserved` symmetric, because rounding can now land a sub-frame past the promise:

```rust
fn adopt_preserved(promised: Duration, actual: Duration) -> Duration {
    if actual.abs_diff(promised) <= RESUME_TOLERANCE {
        promised
    } else {
        actual
    }
}
```

Update its doc comment's first sentence to: `Keep the promised value when the difference is sub-frame quantization, in either direction, and adopt the decoder's answer when it is a real difference.`

- [ ] **Step 9: Run the tests**

Run: `cargo test --locked --test m10_finite_reconnect && cargo test --locked --lib playback`
Expected: PASS.

- [ ] **Step 10: The whole tree**

Run: `cargo test --locked --no-fail-fast`
Expected: PASS, including the unchanged m7 suites, `engine_contract`, `engine_remote` and `m4_diagnostics`. If a seek-exactness test fails on the rounding, read it before touching it. A test pinning `actual <= target` encodes the old floor; change it only if it asserts the floor itself rather than a listener-visible rule, and say so in the commit message.

- [ ] **Step 11: Lint and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings
git add src/playback/reconnect.rs src/playback/engine.rs src/playback/decode.rs tests/support/mod.rs tests/m10_finite_reconnect.rs
git commit -m "feat(m10): a dropped finite episode reconnects and resumes where it was"
```

---

### Task 5: Commands during recovery, and Space's commit check

A seek or restart during recovery is stored with no network I/O (§7). This also applies in a recovery-pause (decision 6). `restore()` commits through `prime_attempt`, so a priming failure on Space fails honestly and keeps the intent (§5, last paragraph).

**Files:**
- Modify: `src/playback/engine.rs`: struct and `new`, `pump_audio`'s recovery branch, `load`, `seek_to`, `restart`, `restore`, `PendingResume` (remove the `#[expect]`)
- Modify: `tests/m10_finite_reconnect.rs`

**Interfaces:**
- Consumes: `prime_attempt`, `complete_pending`, `landing_provenance`, `recoverable`.
- Produces:
  - field `recovery_duration: Option<Duration>`
  - `Worker::connection_lost(&mut self, failure: RemoteFailure)`
  - `Worker::holds_intent_offline(&self) -> bool`
  - `Worker::store_intent(&mut self, intent: PendingResume)`
  - `Worker::fail_attempt(&mut self, error: PlaybackError)`
  - in the m10 file: `parked()`, `split_at_gap`

- [ ] **Step 1: Write the failing tests**

Append to `tests/m10_finite_reconnect.rs`:

```rust
/// A backoff long enough that no attempt runs while a test acts.
fn parked() -> ReconnectPolicy {
    ReconnectPolicy {
        backoff: [Duration::from_secs(30); 5],
        budget: Duration::from_secs(60),
        stable_after: Duration::from_secs(10),
    }
}

/// Splits a render log after its first discontinuity: before and after.
fn split_at_gap(indices: &[u32]) -> (&[u32], &[u32]) {
    match indices.windows(2).position(|pair| pair[1] != pair[0] + 1) {
        Some(at) => indices.split_at(at + 1),
        None => (indices, &[]),
    }
}

fn seek_completed(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::SeekCompleted { .. })
}

#[test]
fn a_seek_during_recovery_is_stored_offline_and_the_landing_anchors_at_it() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(
        &server,
        ReconnectPolicy {
            backoff: [Duration::from_millis(400); 5],
            ..quick()
        },
    );
    engine.clear_rendered();
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let before = server.requests().len();
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    assert_eq!(stored_target(engine.await_event(stored)), target);
    assert_eq!(
        server.requests().len(),
        before,
        "a seek during recovery touched the network"
    );
    assert_eq!(engine.count_events(seek_completed), 0, "completed before any attempt");

    engine.play_until_event(state(PlaybackState::Playing));
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    assert!(server.requests().len() > before);
    // §5 step 5: progress counts from the landing, not from the capture.
    let position = engine.position();
    assert!(
        position >= target && position < target + Duration::from_millis(500),
        "progress counts from {position:?}, not from the landing {target:?}"
    );
    engine.play_for(target + Duration::from_millis(300));
    let indices = frame_indices(&engine.rendered());
    let (_, after) = split_at_gap(&indices);
    assert_eq!(after.first().copied(), Some(frame_at(target)));
    assert_consecutive(after);
    engine.finish();
    server.shutdown();
}

#[test]
fn seek_by_presses_during_recovery_accumulate() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let mut targets = Vec::new();
    for _ in 0..3 {
        engine.send(PlaybackCommand::SeekBy(1));
        targets.push(stored_target(engine.await_event(stored)));
    }
    assert_eq!(targets[1], targets[0] + Duration::from_secs(1));
    assert_eq!(targets[2], targets[0] + Duration::from_secs(2));
    engine.finish();
    server.shutdown();
}

#[test]
fn a_failed_attempt_keeps_the_stored_target_for_the_next() {
    // 3: the first attempt is refused. 4 + 5: the second lands.
    let server = server(
        episode().truncate_body_after(CUT),
        vec![Script::serving(Vec::new()).status(503), episode()],
    );
    let mut backoff = [Duration::from_millis(20); 5];
    backoff[0] = Duration::from_millis(400);
    let mut engine = start(&server, ReconnectPolicy { backoff, ..quick() });
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);
    engine.play_until_event(state(PlaybackState::Playing));
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    assert_eq!(server.requests().len(), 5);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_restart_during_recovery_lands_at_zero_as_a_restart() {
    let server = server(
        episode().truncate_body_after(CUT),
        vec![Script::serving(Vec::new()).status(503), episode()],
    );
    let mut backoff = [Duration::from_millis(20); 5];
    backoff[0] = Duration::from_millis(400);
    let mut engine = start(&server, ReconnectPolicy { backoff, ..quick() });
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Restart);
    assert_eq!(stored_target(engine.await_event(stored)), Duration::ZERO);
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(engine.await_restart_established(), Duration::ZERO);
    assert_eq!(engine.count_events(seek_completed), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_after_a_restart_supersedes_it() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(
        &server,
        ReconnectPolicy {
            backoff: [Duration::from_millis(400); 5],
            ..quick()
        },
    );
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Restart);
    engine.await_event(stored);
    let target = Duration::from_secs(1);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    assert_eq!(stored_target(engine.await_event(stored)), target);
    engine.play_until_event(state(PlaybackState::Playing));
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    assert_eq!(
        engine.count_events(|event| matches!(event, PlaybackEvent::RestartEstablished { .. })),
        0
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_restart_stored_during_recovery_survives_stop_and_space() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Restart);
    engine.await_event(stored);
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    assert_eq!(engine.await_restart_established(), Duration::ZERO);
    engine.await_event(state(PlaybackState::Playing));
    engine.finish();
    server.shutdown();
}

#[test]
fn a_priming_failure_on_space_fails_honestly_and_keeps_the_target() {
    // 3 + 4: Space reopens and reseeks, then the range response ends inside
    // the priming fill. 5 + 6: the second Space lands.
    let server = server(
        episode().truncate_body_after(CUT),
        vec![episode(), episode().truncate_body_after(24 * 1024), episode()],
    );
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(failed);
    assert_eq!(engine.count_events(state(PlaybackState::Playing)), 0);
    assert_eq!(engine.count_events(seek_completed), 0);

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_near_the_end_during_recovery_plays_out_and_ends() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(
        &server,
        ReconnectPolicy {
            backoff: [Duration::from_millis(400); 5],
            ..quick()
        },
    );
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(
        engine.handle().submit_seek(Duration::from_millis(3800)),
        Admission::Accepted
    );
    engine.await_event(stored);
    engine.play_until_terminal(PATIENCE);
    assert!(engine.saw_end_of_track(), "the episode did not end");
    assert_eq!(engine.count_events(failed), 0);
    engine.finish();
    server.shutdown();
}
```

- [ ] **Step 2: Run them to see the failure**

Run: `cargo test --locked --test m10_finite_reconnect`
Expected: the new tests FAIL. Without the offline store, a seek or restart in `Reconnecting` goes through the networked `seek_to`/`restart` paths. Without the restore commit check, the priming-failure test sees a `SeekCompleted`.

- [ ] **Step 3: Cache the duration on entry**

Add the field after `pending`:

```rust
    /// The established duration of a finite source in recovery, cached on
    /// entry because the decoder that knew it is gone (M10 §3). Clamps a seek
    /// stored offline. Cleared by `load`.
    recovery_duration: Option<Duration>,
```

and `recovery_duration: None,` in `Worker::new`. In `load`, add `self.recovery_duration = None;` next to `self.pending = None;`.

In `pump_audio`'s recovery branch (Task 4), change `self.enter_reconnecting(failure);` to `self.connection_lost(failure);`, and add next to `recoverable`:

```rust
    /// M10 §3: an eligible finite episode lost its connection while playing.
    /// The transport stays up so the ring plays out, as for a station.
    fn connection_lost(&mut self, failure: RemoteFailure) {
        // Before `enter_reconnecting` retires the decoder that knows it.
        // Established only: an estimate is not a ceiling (`clamp_target`).
        self.recovery_duration = self
            .source
            .as_ref()
            .and_then(|source| established_duration(source.metadata()));
        self.enter_reconnecting(failure);
    }
```

- [ ] **Step 4: Store intent offline**

Add near `seek_to`:

```rust
    /// M10 §7: a finite session holding its intent with nothing open: in
    /// recovery, or paused out of one (no decoder, no transport). Seek
    /// support is already proven for any session that got here, so seeks and
    /// restarts are stored without network I/O.
    fn holds_intent_offline(&self) -> bool {
        if self.is_indefinite() || !self.source_is_remote() {
            return false;
        }
        match self.state {
            PlaybackState::Reconnecting => true,
            PlaybackState::Paused => self.source.is_none() && lock(&self.transport).is_none(),
            _ => false,
        }
    }

    /// Store `intent` and announce it, so the display and the checkpoint
    /// follow. The outage, backoff and schedule are untouched.
    fn store_intent(&mut self, intent: PendingResume) {
        self.pending = Some(intent);
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::SeekTargetStored {
            session_rev,
            target: intent.target(),
        });
    }
```

In `seek_to`, insert right after the `Failed` rejection (before the `Unsupported` gate):

```rust
        if self.holds_intent_offline() {
            let target = self
                .recovery_duration
                .map_or(requested, |duration| requested.min(duration));
            self.store_intent(PendingResume::Seek(target));
            return;
        }
```

In `restart`, insert right after the `is_indefinite()` rejection:

```rust
        // M10 §7: stored, never collapsed into `Seek(ZERO)`. The landing
        // reports `RestartEstablished`.
        if self.holds_intent_offline() {
            self.store_intent(PendingResume::Restart);
            return;
        }
```

Remove the `#[expect(dead_code, …)]` line from `PendingResume::Restart`.

- [ ] **Step 5: `restore` commits through `prime_attempt`**

In `restore`, replace the tail written in Task 3 (from `let target = …` through the `reinstall(true)` match) with:

```rust
        // M10 §4: read, not taken. A reseek, install or priming failure
        // leaves the intent for the next Space and for the checkpoint.
        let previous = (self.position, self.position_provenance);
        let target = self.pending.map_or(self.position, PendingResume::target);
        let landed = match self.reseek(target) {
            Ok((actual, provenance)) => {
                self.position = adopt_preserved(target, actual);
                self.position_provenance = self.landing_provenance(actual, provenance);
                actual
            }
            // A stop or a shutdown arrived mid-refinement. The preserved
            // position still stands; the interrupt is handled by the loop.
            Err(error) if is_cancelled(&error) => return,
            Err(error) => {
                self.fail(format!("cannot resume at {target:?}: {error}"));
                return;
            }
        };
        // M10 §5: the same commit check as a recovery attempt, so a priming
        // failure fails honestly instead of announcing Playing over a
        // `Failed` that `pump_audio` already set.
        match self.prime_attempt() {
            Ok(()) => {
                self.start_running();
                self.announce_playing();
                self.complete_pending(landed);
            }
            Err(error) => {
                (self.position, self.position_provenance) = previous;
                if !is_cancelled(&error) {
                    self.fail_attempt(error);
                }
            }
        }
    }

    /// Space is one attempt: when it fails, say what failed. A remote cause
    /// travels typed; anything else is the device or the decoder.
    fn fail_attempt(&mut self, error: PlaybackError) {
        if matches!(error, PlaybackError::Remote(_)) || remote_cause(&error).is_some() {
            self.fail_from(error);
        } else {
            self.fail(format!("cannot start playback: {error}"));
        }
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked --test m10_finite_reconnect --test engine_remote --test engine_contract --test http_playback --test decode_fixtures`
Expected: PASS. `engine_contract` and `decode_fixtures` matter here: `restore` now primes through `prime_attempt` for local files too, and both of its remote-only guards (Task 4, Steps 6 and 7) are what keep local Stop → Play and the drain-what-it-has rule unchanged.

- [ ] **Step 7: Lint and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings
git add src/playback/engine.rs tests/m10_finite_reconnect.rs
git commit -m "feat(m10): seek and restart during recovery are stored offline; Space commits like an attempt"
```

---

### Task 6: Pause and stop during recovery

A pause is cancelled at submission: `submit_pause` retires the source instead of freezing it while a finite recovery runs. The dispatched pause tears down whatever is left. A failure that arrives with a pause already submitted lands paused (§3, §7).

**Files:**
- Modify: `src/playback/engine.rs`:
  - `SourceTraits` and `submit_pause` (near line 456)
  - `set_state`, `enter_reconnecting`, `connection_lost`
  - `pause` (near line 2831), with `pause_indefinite` renamed to `pause_closing`
- Modify: `tests/support/mod.rs` (`load_remote_with_resume_and_limits`)
- Modify: `tests/m10_finite_reconnect.rs`

**Interfaces:**
- Produces:
  - `SourceTraits::recovering: AtomicBool`
  - `Worker::pause_closing(&mut self)` (renamed from `pause_indefinite`)
  - `Worker::lost_source_while_playing(&self) -> bool`
  - `TestEngine::load_remote_with_resume_and_limits(&mut self, url: &str, resume: ResumeIntent, limits: Limits) -> LoadRequestId`
  - in the m10 file: `patient()`, `start_with(server, policy, limits)`

- [ ] **Step 1: Harness loader with both a resume and limits**

In `tests/support/mod.rs`, next to `load_remote_with_limits`:

```rust
    /// `load_remote_with_limits` under a caller-decided `ResumeIntent`: M10's
    /// cancellation tests need both a proven (resumed) episode and deadlines
    /// only a command can beat.
    pub fn load_remote_with_resume_and_limits(
        &mut self,
        url: &str,
        resume: ResumeIntent,
        limits: Limits,
    ) -> LoadRequestId {
        let request = self.next_request();
        self.load_remote_inner(request, url, resume, Some(limits), true);
        request
    }
```

- [ ] **Step 2: Write the failing tests**

In `tests/m10_finite_reconnect.rs`, add `use tenuto::http::limits::Limits;` to the imports, then replace `fn start` with:

```rust
fn start(server: &TestServer, policy: ReconnectPolicy) -> TestEngine {
    start_with(server, policy, None)
}

/// Load at [`RESUME`] and play. Connection 1 is the probe; connection 2 is
/// the resume seek, and it is the connection that plays.
fn start_with(server: &TestServer, policy: ReconnectPolicy, limits: Option<Limits>) -> TestEngine {
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(policy);
    let url = server.url("/episode.wav");
    match limits {
        Some(limits) => {
            engine.load_remote_with_resume_and_limits(&url, ResumeIntent::StartAt(RESUME), limits);
        }
        None => engine.load_remote_with_resume(&url, ResumeIntent::StartAt(RESUME)),
    }
    assert_eq!(
        engine.await_loaded().capabilities.seek,
        SeekSupport::Native,
        "the resume seek must prove the episode seekable"
    );
    // Consumed here, so a later wait can only match a transition the test
    // itself caused.
    engine.await_event(state(PlaybackState::Paused));
    engine.send(PlaybackCommand::Play);
    engine.await_event(state(PlaybackState::Playing));
    engine
}
```

Then append:

```rust
/// Deadlines far past the harness's 20 s patience: what these tests prove is
/// a command waking a blocked attempt, never a timeout expiring under it.
fn patient() -> Limits {
    Limits {
        headers: Duration::from_secs(60),
        stall: Duration::from_secs(60),
        open: Duration::from_secs(60),
        ..Limits::brisk()
    }
}

#[test]
fn pausing_during_recovery_keeps_the_target_and_space_resumes_at_it() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);
    let before = server.requests().len();

    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Paused));
    assert!(
        !engine.handle().source_interrupt().is_frozen(),
        "a recovery pause must leave no freeze level standing"
    );
    assert_eq!(server.requests().len(), before);

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    engine.await_event(state(PlaybackState::Playing));
    engine.finish();
    server.shutdown();
}

#[test]
fn stopping_during_recovery_keeps_the_target_and_space_resumes_at_it() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let target = Duration::from_millis(2500);
    assert_eq!(engine.handle().submit_seek(target), Admission::Accepted);
    engine.await_event(stored);
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    let landed = engine.await_seek_completed(PATIENCE);
    assert!(landed.actual.abs_diff(target) < Duration::from_millis(1));
    engine.finish();
    server.shutdown();
}

#[test]
fn a_restart_stored_during_recovery_survives_pause_and_space() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Restart);
    engine.await_event(stored);
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Paused));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    assert_eq!(engine.await_restart_established(), Duration::ZERO);
    engine.finish();
    server.shutdown();
}

/// Pauses while an attempt is blocked at the server. Under `patient()` limits
/// only the pause can end the wait inside the harness's patience.
fn pause_cancels_a_blocked_attempt(attempts: Vec<Script>) {
    let server = server(episode().truncate_body_after(CUT), attempts);
    let mut engine = start_with(&server, quick(), Some(patient()));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert!(
        server.wait_until_stalled(PATIENCE),
        "the attempt never reached its stall"
    );
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Paused));
    assert!(
        !engine.handle().source_interrupt().is_frozen(),
        "a recovery pause must leave no freeze level standing"
    );
    assert_eq!(engine.count_events(failed), 0);
    assert_eq!(
        engine.count_events(state(PlaybackState::Paused)),
        0,
        "Paused was announced twice"
    );
    server.release();
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Playing));
    engine.finish();
    server.shutdown();
}

#[test]
fn pause_cancels_a_blocked_reopen() {
    pause_cancels_a_blocked_attempt(vec![episode().stall_headers(), episode()]);
}

#[test]
fn pause_cancels_a_blocked_reseek() {
    pause_cancels_a_blocked_attempt(vec![episode(), episode().stall_headers(), episode()]);
}

#[test]
fn pause_cancels_a_blocked_priming_read() {
    // The reseek needs a few KiB; priming wants the 300 ms ring (about
    // 56 KiB), so the stall at 16 KiB lands inside priming.
    pause_cancels_a_blocked_attempt(vec![
        episode(),
        episode().stall_body_after(16 * 1024),
        episode(),
    ]);
}

#[test]
fn stop_cancels_a_blocked_priming_read_and_space_resumes() {
    let server = server(
        episode().truncate_body_after(CUT),
        vec![episode(), episode().stall_body_after(16 * 1024), episode()],
    );
    let mut engine = start_with(&server, quick(), Some(patient()));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert!(server.wait_until_stalled(PATIENCE));
    engine.handle().submit_stop();
    engine.await_event(state(PlaybackState::Stopped));
    assert_eq!(engine.count_events(failed), 0);
    server.release();
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Playing));
    engine.finish();
    server.shutdown();
}

#[test]
fn a_pause_raced_by_a_dropped_connection_lands_paused_not_reconnecting() {
    // The playing connection stalls a second of audio in, then drops one byte
    // after its release. The drop reaches a worker blocked in that read, with
    // the pause submitted but not yet dispatched (§3).
    let server = server(
        episode()
            .stall_body_after(BYTES_PER_SEC)
            .truncate_body_after(BYTES_PER_SEC + 1),
        vec![episode()],
    );
    let mut engine = start_with(&server, quick(), Some(patient()));
    // Drain what the stalled body supplied, then demand far more without a
    // round trip a blocked worker could not answer (m7_cancellation's
    // StalledBody arrangement).
    engine.play_for(RESUME + Duration::from_millis(200));
    engine.let_time_pass_while_unresponsive(Duration::from_millis(1500));
    assert!(server.wait_until_stalled(PATIENCE));
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    // The hook announces this from inside the blocked read.
    engine.await_event(state(PlaybackState::Paused));
    server.release();

    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    assert!(!engine.handle().source_interrupt().is_frozen());
    let before = server.requests().len();
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_event(state(PlaybackState::Playing));
    assert!(
        server.requests().len() > before,
        "Space must reopen: a recovery pause holds no connection"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_new_load_during_recovery_drops_the_outage_and_the_target() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(
        engine.handle().submit_seek(Duration::from_millis(2500)),
        Admission::Accepted
    );
    engine.await_event(stored);
    engine.load_remote_with_resume(&server.url("/episode.wav"), ResumeIntent::StartAt(RESUME));
    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(RESUME + Duration::from_millis(200));
    assert_eq!(engine.count_events(seek_completed), 0);
    assert!(engine.position() < Duration::from_millis(2500));
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn shutdown_during_a_blocked_attempt_joins_promptly() {
    let server = server(
        episode().truncate_body_after(CUT),
        vec![episode().stall_headers(), episode()],
    );
    let mut engine = start_with(&server, quick(), Some(patient()));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert!(server.wait_until_stalled(PATIENCE));
    engine.handle().submit_shutdown();
    assert!(
        engine.join_within(Duration::from_secs(5)),
        "shutdown waited on a blocked attempt"
    );
    server.release();
    server.shutdown();
}
```

- [ ] **Step 3: Run them to see the failure**

Run: `cargo test --locked --test m10_finite_reconnect`
Expected:
- the pause tests FAIL: the freeze is left standing, or no `Paused` arrives within the patience because a frozen attempt is never cancelled;
- the raced-pause test FAILS with a `Reconnecting`;
- the stop, load and shutdown tests may pass already, since they pin existing paths.

- [ ] **Step 4: Publish `recovering` and cancel at submission**

In `SourceTraits`, add:

```rust
    /// A finite recovery is in progress (M10 §7): `submit_pause` must retire
    /// the attempt's read, not freeze it, as for a station.
    pub recovering: AtomicBool,
```

In `submit_pause`, change the condition to:

```rust
            if self.traits.indefinite.load(Ordering::Acquire)
                || self.traits.recovering.load(Ordering::Acquire)
            {
                self.source_interrupt.retire_generation(generation);
            } else {
                self.source_interrupt.freeze();
            }
```

and extend its doc comment with: `A finite recovery is retired too (M10 §7): a frozen reopen or priming read would also never wake, and a parked half-built transport would announce \`Paused\` mid-attempt.`

In `enter_reconnecting`'s finite log branch (Task 4), add after the `tracing::info!`:

```rust
                    self.traits.recovering.store(true, Ordering::Release);
```

In `set_state`, add as its first statement, before the early return:

```rust
        // M10 §7: every exit from Reconnecting ends a finite recovery.
        if state != PlaybackState::Reconnecting {
            self.traits.recovering.store(false, Ordering::Release);
        }
```

- [ ] **Step 5: The recovery pause**

Rename `pause_indefinite` to `pause_closing`, and make its doc comment:

```rust
    /// M7 §6.2, M10 §7. A pause that keeps no connection: a station's, and a
    /// finite session's during recovery. Both pause routes end here, the
    /// dispatched `Pause` and the one where the hook parked first and
    /// already announced `Paused`. Position and `pending` survive; the next
    /// Play runs `restore()` (or `fresh_open` for a station).
```

In its body, add `self.traits.recovering.store(false, Ordering::Release);` after `self.session_rev += 1;`. The `announced` branch assigns `state` directly and never reaches `set_state`.

Replace the head of `pause` with:

```rust
    fn pause(&mut self) {
        // First, ahead of the `Playing`-only guard below: a station never
        // parks, and neither does a finite session in recovery or one whose
        // recovery-time pause already retired its source (M10 §7).
        if self.is_indefinite()
            || self.state == PlaybackState::Reconnecting
            || self.lost_source_while_playing()
        {
            self.pause_closing();
            return;
        }
        if self.state != PlaybackState::Playing {
            return;
        }
```

Keep the rest of `pause` unchanged. Add next to it:

```rust
    /// M10 §7's race: `submit_pause` read `recovering` just before the
    /// attempt committed, so its retirement landed on the generation that is
    /// now playing. Parking a transport with no source behind it would strand
    /// the resume; close instead, and let Space reopen.
    fn lost_source_while_playing(&self) -> bool {
        self.state == PlaybackState::Playing
            && self.source_is_remote()
            && (self.source.is_none() || self.source_interrupt.is_retired())
    }
```

- [ ] **Step 6: A failure with a pause already submitted**

In `connection_lost` (Task 5), insert after the `recovery_duration` assignment:

```rust
        // M10 §3: a pause submitted but not yet dispatched raised the freeze
        // level, and the hook may already have announced `Paused`. The
        // listener asked to pause, so land there rather than reconnecting.
        if self.source_interrupt.is_frozen() {
            self.pause_closing();
            return;
        }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --locked --test m10_finite_reconnect --test m7_cancellation --test m7_submission --test m7_reconnect`
Expected: PASS.

- [ ] **Step 8: The whole tree**

Run: `cargo test --locked --no-fail-fast`
Expected: PASS.

- [ ] **Step 9: Lint and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings
git add src/playback/engine.rs tests/support/mod.rs tests/m10_finite_reconnect.rs
git commit -m "feat(m10): a pause cancels a finite recovery at submission"
```

---

### Task 7: The heard-time window across seeks

Spec test 9 at the worker level. A budget of 1 ms makes any second failure inside the same outage give up at once, so `Failed` versus `Reconnecting` shows whether the outage carried over. Old position-based accounting fails both tests; Task 1's counter passes them.

**Files:**
- Modify: `tests/m10_finite_reconnect.rs`

**Interfaces:**
- Consumes: everything above; no new production code.

- [ ] **Step 1: Write the tests**

Append:

```rust
/// Any second failure in the same outage gives up at once; a fresh outage
/// reconnects. One heard second ends the window.
fn one_shot() -> ReconnectPolicy {
    ReconnectPolicy {
        budget: Duration::from_millis(1),
        stable_after: Duration::from_secs(1),
        ..quick()
    }
}

#[test]
fn a_forward_seek_does_not_end_the_outage() {
    // 3 + 4: the reconnect lands. 5: the seek's range response drops half a
    // second in, about 200 ms of heard playback after the landing (the
    // decoder runs a 300 ms ring ahead of what is heard).
    let server = server(
        episode().truncate_body_after(CUT),
        vec![
            episode(),
            episode(),
            episode().truncate_body_after(BYTES_PER_SEC / 2),
            episode(),
        ],
    );
    let mut engine = start(&server, one_shot());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    let landed = engine.position();
    // Further than stable_after: position growth would call this stable.
    assert_eq!(
        engine.handle().submit_seek(landed + Duration::from_millis(1200)),
        Admission::Accepted
    );
    engine.await_seek_completed(PATIENCE);
    engine.play_until_terminal(PATIENCE);
    assert_eq!(
        engine.state(),
        PlaybackState::Failed,
        "the seek ended the outage, so the drop after it started a fresh one"
    );
    assert_eq!(engine.count_events(state(PlaybackState::Reconnecting)), 0);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_backward_seek_does_not_stop_heard_playback_ending_the_outage() {
    // 5: the seek's range response drops 1.5 s in, about 1.2 s of heard
    // playback after the landing: past stable_after.
    let server = server(
        episode().truncate_body_after(CUT),
        vec![
            episode(),
            episode(),
            episode().truncate_body_after(BYTES_PER_SEC * 3 / 2),
            episode(),
        ],
    );
    let mut engine = start(&server, one_shot());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(engine.handle().submit_seek(RESUME), Admission::Accepted);
    engine.await_seek_completed(PATIENCE);
    // A fresh outage: the budget did not carry over.
    engine.play_until_event(state(PlaybackState::Reconnecting));
    assert_eq!(engine.count_events(failed), 0);
    engine.finish();
    server.shutdown();
}
```

- [ ] **Step 2: Run them**

Run: `cargo test --locked --test m10_finite_reconnect a_forward_seek a_backward_seek`
Expected: PASS. To prove the tests can fail, temporarily change `is_over` to `self.window_open` (window open alone) and confirm the forward-seek test fails. Revert before committing.

- [ ] **Step 3: Lint and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings
git add tests/m10_finite_reconnect.rs
git commit -m "test(m10): seeks neither end nor extend the outage's heard window"
```

---

### Task 8: `KeyRouter` holds a stored target

Arrow keys never send `SeekBy`. `KeyRouter` turns a burst into an absolute `SeekTo`, and today it releases its hold on `SeekTargetStored`. Progress then puts the old heard position back in the mirror, so a second burst replaces the stored target instead of advancing it (§7, "Frontend hold").

**Files:**
- Modify: `src/application/seek.rs` (`KeyRouter` and its tests)
- Modify: `tests/m10_finite_reconnect.rs`

**Interfaces:**
- Produces: `KeyRouter::stored: Option<Duration>` and the private `KeyRouter::drop_burst(&mut self)`.

- [ ] **Step 1: Write the failing unit tests**

Append to `src/application/seek.rs`'s `mod tests`:

```rust
    #[test]
    fn a_stored_target_holds_the_display_and_seeds_the_next_burst() {
        // M10 §7: a seek while stopped or recovering is stored, not run. The
        // progress that follows reports the heard position, which the next
        // burst must not start from.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None);
        assert_eq!(
            router.take_due(now + SEEK_COALESCE_WINDOW),
            Some(Duration::from_secs(110))
        );
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::from_secs(110),
        });
        assert!(router.is_seeking(), "a stored target must keep the display on it");
        let later = now + Duration::from_secs(5);
        assert_eq!(
            router.press(Duration::from_secs(100), SEEK_STEP_SECS, later, None),
            Duration::from_secs(120),
            "the burst started from the mirror instead of the stored target"
        );
    }

    #[test]
    fn the_stored_target_is_the_workers_clamped_one() {
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None);
        router.take_due(now + SEEK_COALESCE_WINDOW);
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::from_secs(105),
        });
        assert_eq!(
            router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None),
            Duration::from_secs(95)
        );
    }

    #[test]
    fn the_landing_releases_a_stored_target() {
        let mut router = KeyRouter::new();
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::from_secs(110),
        });
        router.observe(&PlaybackEvent::SeekCompleted {
            session_rev: 0,
            requested: Duration::from_secs(110),
            actual: Duration::from_secs(110),
            refinement_truncated: false,
            provenance: PositionProvenance::Established,
        });
        assert!(!router.is_seeking());
    }

    #[test]
    fn a_restart_landing_releases_a_stored_target() {
        let mut router = KeyRouter::new();
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::ZERO,
        });
        router.observe(&PlaybackEvent::RestartEstablished {
            session_rev: 0,
            position: Duration::ZERO,
            provenance: PositionProvenance::Established,
        });
        assert!(!router.is_seeking());
    }

    #[test]
    fn a_stop_drops_the_burst_but_keeps_the_stored_target() {
        // The worker keeps `pending` across a stop, so the next burst must
        // still accumulate on it.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::from_secs(110),
        });
        router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None);
        router.drop_burst();
        assert_eq!(router.take_due(now + SEEK_COALESCE_WINDOW), None);
        assert!(router.is_seeking());
        assert_eq!(
            router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None),
            Duration::from_secs(120)
        );
    }
```

- [ ] **Step 2: Run them to see the failure**

Run: `cargo test --locked --lib application::seek`
Expected: compile error (`drop_burst` does not exist); after a stub, assertion failures on `is_seeking` and the accumulation.

- [ ] **Step 3: Implement the stored hold**

In `src/application/seek.rs`:

(a) Add the field to `KeyRouter`:

```rust
    /// A target the worker stored rather than ran (a seek while stopped or
    /// recovering, M10 §7). Held until a landing resolves it, so a later
    /// burst accumulates from it, not from a mirror that progress has put
    /// back at the heard position.
    stored: Option<Duration>,
```

(b) Change `displayed_target` to:

```rust
    fn displayed_target(&self) -> Option<Duration> {
        self.burst.target().or(self.submitted).or(self.stored)
    }
```

(c) Replace `observe`'s body with:

```rust
        match event {
            // Stored, not run: hold the worker's own (clamped) target.
            PlaybackEvent::SeekTargetStored { target, .. } => {
                self.submitted = None;
                self.stored = Some(*target);
            }
            PlaybackEvent::SeekCompleted { .. }
            | PlaybackEvent::SeekRejected { .. }
            | PlaybackEvent::SeekCancelled { .. }
            | PlaybackEvent::RestartEstablished { .. }
            | PlaybackEvent::EndOfTrack { .. } => self.release(),
            PlaybackEvent::Loaded { .. }
            | PlaybackEvent::LoadCancelled { .. }
            | PlaybackEvent::Failed { .. } => self.cancel(),
            _ => {}
        }
```

and change its doc comment's first paragraph to: `Lets the router see each drained event, so it can tell when the seek it is waiting on has settled: landed, been refused, been overtaken, or been stored to land later.`

(d) Change `release` to clear both holds:

```rust
    /// Hands the display back to the worker's own position.
    fn release(&mut self) {
        self.submitted = None;
        self.stored = None;
    }
```

(e) Add, and use it in `route` for `Restart | Stop | Shutdown` instead of `self.cancel()`:

```rust
    /// Drops an unsubmitted burst and the wait for a submitted one, keeping a
    /// stored target: the worker keeps its `pending` across a stop, and a
    /// restart replaces it with a `SeekTargetStored` of its own.
    fn drop_burst(&mut self) {
        self.burst.cancel();
        self.submitted = None;
    }
```

(f) Change `is_seeking` to:

```rust
    pub fn is_seeking(&self) -> bool {
        self.burst.is_open() || self.submitted.is_some() || self.stored.is_some()
    }
```

- [ ] **Step 4: The integration test**

In `tests/m10_finite_reconnect.rs`, change `use std::time::Duration;` to `use std::time::{Duration, Instant};`, add `use tenuto::app::KeyRouter;`, and append:

```rust
#[test]
fn arrow_bursts_during_recovery_accumulate_across_progress() {
    let server = server(episode().truncate_body_after(CUT), vec![episode()]);
    let mut engine = start(&server, parked());
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let mut router = KeyRouter::new();
    // Past the router's 250 ms quiet window.
    let quiet = Duration::from_millis(300);
    let mut targets = Vec::new();
    for _ in 0..2 {
        let now = Instant::now();
        let mirror = engine.handle().progress().position;
        router.route(&engine.handle(), true, mirror, None, now, PlaybackCommand::SeekBy(1));
        router.flush(&engine.handle(), now + quiet);
        let event = engine.await_event(stored);
        router.observe(&event);
        targets.push(stored_target(event));
        // Progress ticks: the ring drains during backoff, so the mirror moves.
        engine.let_time_pass(Duration::from_millis(100));
    }
    assert_eq!(targets[1], targets[0] + Duration::from_secs(1));
    engine.finish();
    server.shutdown();
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --locked --lib application::seek && cargo test --locked --test m10_finite_reconnect arrow_bursts && cargo test --locked --test m5_tui_input --test m5_runtime`
Expected: PASS. The existing test `a_press_during_the_wait_for_a_landing_reopens_the_burst` still passes; `loaded_and_failed_events_cancel_unsubmitted_bursts` still passes because those events still `cancel()`.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings
git add src/application/seek.rs tests/m10_finite_reconnect.rs
git commit -m "fix(m10): arrow bursts accumulate on a stored target across progress"
```

---

### Task 9: Documentation, acceptance and the gate

**Files:**
- Modify: `docs/architecture.md` (§1, §7.1, new §7.6, §12)
- Modify: `docs/reference.md` (Playing over HTTP)
- Modify: `CHANGELOG.md` (`[Unreleased]`)
- Modify: `docs/superpowers/specs/2026-09-29-tenuto-m10-finite-reconnect-design.md`
- Create: `docs/m10-acceptance.md`

- [ ] **Step 1: `docs/architecture.md`**

- §1, line 32. Replace "Reconnect attempts for a live stream continue a current Play:" with "Reconnect attempts, for a live stream or for a finite episode that dropped mid-play (§7.6), continue a current Play:". Keep the rest of the sentence.
- §7.1, line 286. Replace "`Reconnecting` exists only for indefinite media (§7.5): entered from `Playing` on a disconnect, it returns to `Playing` on a successful fresh open," with "`Reconnecting` serves indefinite media (§7.5) and range-capable finite media (§7.6): entered from `Playing` on a disconnect, it returns to `Playing` on a successful attempt,". Keep the rest of the sentence.
- Add a new `### 7.6 Finite media recovery` after §7.5's table, before `## 8`:

```markdown
### 7.6 Finite media recovery

A remote finite source whose resume capability is `Supported` recovers from a dropped or stalled connection through the same `Reconnecting` state, `Outage` and `ReconnectPolicy` as a station. Range-less servers and sources whose seek support is still `Unknown` fail as before. A reopen of an already-proven location keeps its `Native` seek support.

**Entry** (`Playing`, remote, `Supported`, not a retirement): a read failure that `retryable(failure, Finite)` accepts. The table: `Transport`, `Timeout`, 429 and 5xx retry for both continuities; `LiveEnded` only for a station; `TruncatedBody` only for finite media; everything else fails. The transport stays up so the ring plays out. A failure that arrives with a pause already submitted (the freeze level up) lands in the recovery pause instead.

**One attempt** (`Worker::reconnect_finite`):
1. Capture the heard position and tear the transport down before any network I/O.
2. Target: the stored seek, zero for a stored restart, otherwise the captured position.
3. Reopen (`expected = Finite`) and reseek.
4. Install the landing as the anchor before the transport opens.
5. Open and prime inside `prime_attempt`, which records a read failure as the attempt's rather than the session's.
6. Commit only if nothing failed or cancelled.

A failed attempt restores the captured position and keeps the stored intent. `restore()` (Space) commits through the same check.

**Pending intent.** `PendingResume { Seek(t), Restart }` is read, never taken, and cleared only after a landing is installed, which reports `SeekCompleted` or `RestartEstablished`.

**Heard time.** The outage ends after `stable_after` of heard playback since the last successful attempt, counted per generation from `Timeline` played frames, so a seek neither ends nor extends it.

**Commands while recovering, or paused out of recovery:**
- `SeekTo`, `SeekBy` and `Restart` are stored with no network I/O, clamped to the duration cached on entry.
- `submit_pause` retires the attempt's read instead of freezing it, and the dispatched pause closes everything (`pause_closing`).
- Stop and Load behave as for a station.
```

- §12. Add after the "Live radio, next" bullet:

```markdown
- **Finite recovery does not revalidate across a reopen (M10).** A reopen probes fresh at byte zero without the previous response's validator, so an episode replaced on the server during an outage resumes at the same time offset in the new file. Stop → Play has the same limit.
```

- [ ] **Step 2: `docs/reference.md`**

After the live-stream reconnect bullet (line 27), add:

```markdown
- If a podcast episode drops mid-play and its server supports range requests (most do), Tenuto reconnects with the same backoff and carries on from where you were. The five-minute budget is judged only when something fails, so an attempt already under way can finish after it. Seeking, pausing and stopping work while it reconnects, and a seek takes effect when the connection comes back. Episodes on servers without range support still fail, and Space tries once more.
```

- [ ] **Step 3: `CHANGELOG.md`**

Under `## [Unreleased]`, add an `### Added` section above `### Changed`:

```markdown
### Added

- A podcast episode that loses its connection mid-play (a VPN switch, a Wi-Fi
  drop, a server hiccup) now reconnects with the same backoff as a live
  stream and carries on from where you were, instead of failing with "the
  server went quiet". Seeking, pausing and stopping work while it
  reconnects. Servers that cannot resume at a position still fail, and Space
  tries once more.

### Fixed

- A seek stored while stopped is no longer lost when the next Play fails to
  resume; the Play after that still lands on it.
- Arrow-key seeks made while stopped accumulate on the stored target instead
  of starting again from the stopped position.
```

- [ ] **Step 4: The spec**

In the spec, add a `## 10. Decisions made during planning` section that lists the seven items under "Decisions beyond the spec" in this plan, one line each. Change §9's harness bullets to match what was built: `Script::then` chains; `TestEngine::rendered`/`clear_rendered` over the existing `TestOutput` log; the base-1024 encoding.

- [ ] **Step 5: Run the gate**

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

Expected: all pass. Record the exact pass/fail/ignored counts for the acceptance record.

- [ ] **Step 6: `docs/m10-acceptance.md`**

Create it in the shape of `docs/m9.1-acceptance.md`:
- scope and branch;
- the gate table with the four commands and their recorded results;
- a "Spec §9 tests" list mapping each spec test (1–9) and each Review Focus item to its test name in `tests/m10_finite_reconnect.rs`;
- the unchanged-suite note: `git diff main -- tests/m7_*.rs tests/m4_diagnostics.rs` prints nothing;
- a "Manual check (pending, user)" item: build release, play a Radio-T episode in Ghostty, toggle the VPN mid-play, and confirm it shows Reconnecting and then resumes at the same moment.

- [ ] **Step 7: Commit**

```bash
git add docs/architecture.md docs/reference.md CHANGELOG.md docs/m10-acceptance.md docs/superpowers/specs/2026-09-29-tenuto-m10-finite-reconnect-design.md
git commit -m "docs(m10): finite recovery in architecture, reference, changelog; acceptance record"
```

---

## Spec coverage map

| Spec | Task |
|---|---|
| §3 classifier, eligibility, entry, log line | 4 (with the freeze branch in 6) |
| §3 `recovery_duration` | 5 |
| §4 `PendingResume`, read not taken, completion events | 3 (Restart constructed in 5) |
| §5 attempt steps 1–9, `prime_attempt` | 4 |
| §5 `restore()` same commit check | 5 |
| §6 heard-time accounting | 1 (integration in 7) |
| §7 SeekTo / SeekBy / Restart during recovery | 5 |
| §7 Pause (submission, dispatch, race), Stop, Load | 6 |
| §7 frontend hold | 8 |
| §8 documentation | 9 |
| §9 tests 1, 2, 3, 5b (recovery), 8 | 4 |
| §9 tests 4, 5, 5a, 5b (restore), 6 | 3, 5 |
| §9 test 7 (pause and stop, all phases) | 6 |
| §9 test 7a | 8 |
| §9 test 9 | 7 |
