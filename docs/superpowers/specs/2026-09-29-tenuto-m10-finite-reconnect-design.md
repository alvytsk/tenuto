# M10 — Finite HTTP recovery

Status: approved in conversation on 2026-09-29; amended on 2026-10-03 after review (anchor order, attempt-local priming outcome, pause at submission, frontend seek hold). Implemented on `feature/finite-reconnect`; §10 records the decisions made while planning and executing, and §9's harness and test 1 describe what was built.

Branch: `feature/finite-reconnect`, from `main` at `4fb5245`.

## 1. Decision

A finite HTTP episode that stalls or drops mid-play (a VPN profile switch, a Wi-Fi blip, a server hiccup) recovers on its own and keeps playing from where the listener was, instead of landing in `Failed` with "the server went quiet during Stall".

Recovery reuses the live-stream machinery: the worker state `Reconnecting`, `Outage` and `ReconnectPolicy` (`src/playback/reconnect.rs`), driven from the worker loop and cancelled at once by a pause, stop, replacing load or shutdown. It does not watch the OS for network changes: the 15 s stall timeout already detects every cause of a dead connection, and network events would cover only one of them.

When M10 engages: the HTTP layer already resumes a short ranged body in place (`http::service`, once more than 64 KiB has arrived), so a plain mid-body cut never reaches the engine. M10 engages when that in-place resume fails, when the body ends inside the first chunk, or on a stall or timeout.

In scope: remote finite sources whose resume capability is `Supported` (architecture §8: `Finite` with `Native` seek support — range-capable servers, which is most podcasts).

Out of scope, and unchanged:
- Range-less servers (`SeekSupport::Unsupported`) and sources with `Unknown` seek support still fail, as today. They cannot resume at a position.
- Local files.
- Live streams, apart from the heard-time outage accounting in §6, which is behaviour-neutral for them.
- Cross-reopen validation. A reopen probes fresh at byte zero and does not carry the previous response's validator (ETag), so an episode file replaced on the server during the outage resumes at the same time offset in the new file. Stop → Play has the same limit today.

## 2. What exists today

- `pump_audio` (`src/playback/engine.rs:2106`) sends a finite source's remote read failure straight to `fail_with`. Only indefinite media reach `source_ended` → `enter_reconnecting`.
- `service_reconnect` (`engine.rs:2172`) runs each attempt through `fresh_open`, which is live-only (`expected = Indefinite`).
- `restore()` (`engine.rs:2729`) is how Play after Stop and Play from `Failed` reopen a retired remote source. Its order is: `ensure_source_open` → `capture_position` → `requested_target.take()` → `reseek` → `reinstall`. A reseek or install failure after the `take()` loses a stored target. The ring keeps playing between the capture and `reinstall`'s discard, so reusing this order for recovery would replay what was heard during the reseek.
- `open_transport()` copies `self.position` into the new `TransportCore`'s anchor, then calls `prime_and_run`, whose outcome it does not return: a priming read that fails reaches `pump_audio`'s finite arm, which calls `fail_with`, and `open_transport` still returns `Ok`. `fresh_open` guards against this with `attempting` / `attempt_failure`, a cancellation check after the open and a `primed` check. That guard exists only on the live path (`source_ended`).
- `pause()` returns early unless the state is `Playing` (except for indefinite media).
- `EngineHandle::submit_pause()` (`engine.rs:456`) retires the observed generation for indefinite media and **freezes** everything else. A freeze suspends the read's stall budget, so a frozen priming read against a silent server never wakes. Reopen and reseek waits are bounded by their own deadlines even when frozen, but they are not cancelled at once. The freeze hook (`WaitService::service_as`) parks the transport and announces `Paused` on its own.
- `submit_seek()` retires the observed generation too, so a seek submitted while an attempt is in flight cancels the attempt. A cancellation does not call `Outage::failed`, so `next_attempt_at` stays in the past and the next pass attempts at once.
- `SeekBy` bases on `self.position` (`engine.rs:2401`). Arrow keys never send `SeekBy`: `KeyRouter` (`src/application/seek.rs`) accumulates a burst against its displayed target or the mirror, and submits an absolute `SeekTo`. It releases its hold on `SeekTargetStored`, after which progress overwrites the mirror with the heard position.
- `seek_to` in `Idle`/`Stopped` validates through `ensure_source_open` and possibly `verify_seek_support`, both network requests, then stores `requested_target` and emits `SeekTargetStored`. `Session` persists that as `outstanding_target` (`session.rs:915`) and resolves it on `SeekCompleted` or `RestartEstablished` (`session.rs:897`).
- `Outage::is_over(position)` measures position growth since the reconnect, which is listening time for a station but moves with every seek for finite media.
- `RemoteFailure::is_retryable` (`src/http/error.rs:123`) covers `Transport`, `Timeout`, `LiveEnded`, 429 and 5xx. `TruncatedBody` is not retryable.
- The PCM ring holds 300 ms (`RING_MILLIS`). The default first backoff is 1 s.

## 3. Eligibility and retry classification

A single classifier, `retryable(failure, continuity)`, replaces the bare `is_retryable()` calls on the reconnect paths:

| Failure | Indefinite | Finite |
|---|---|---|
| `Transport`, `Timeout`, 429, 5xx | retry | retry |
| `LiveEnded` | retry | n/a |
| `TruncatedBody` | n/a | retry |
| everything else (`ResourceChanged`, 4xx, `InvalidRange`, …) | fail | fail |

It is used both when a playing connection fails (entry) and when an attempt fails (reopen, reseek or priming), so `TruncatedBody` retries on every path.

A finite source enters `Reconnecting` from `pump_audio` when all hold:
- the state is `Playing`;
- the source is remote (`source_is_remote()`) and its resume capability is `Supported`;
- the read error is not a retirement (the retirement arm stays first);
- `remote_cause(&error)` classifies as retryable under the table.

Anything else takes today's path unchanged.

An eligible failure that arrives while the freeze level is up (a pause already submitted, not yet dispatched) does not enter `Reconnecting`. It lands directly in the recovery-pause state (§7, `Pause`): no source, `pending` kept, `Paused`, and Space runs `restore()`. The listener asked for a pause, and the hook may already have announced one.

`enter_reconnecting` keeps its shape. For a finite source it sets `SourceTraits::recovering` (§7), and it caches the source's established duration (`established_duration(source.metadata())`, never an estimated one) into a new field `recovery_duration: Option<Duration>`, before `retire_remote_source` drops the decoder. Its log line becomes:
- indefinite: `live source lost; reconnecting` (as today);
- finite: `connection lost; reconnecting`, with a `position` field.

Both log only `reason = %failure`. A `RemoteFailure`'s `Display` is already redacted; no URL is logged.

## 4. The pending resume intent

`requested_target: Option<Duration>` becomes `pending: Option<PendingResume>`:

```rust
enum PendingResume {
    Seek(Duration),
    Restart,
}
```

- A seek stores `Seek(target)`, superseding any `Restart`.
- A restart stores `Restart`. It is never collapsed into `Seek(ZERO)`: the two need different completion events.
- It is read, not taken, by every attempt and by `restore()`, and cleared only after the transport is installed and running. On that success the worker emits `SeekCompleted` for `Seek` and `RestartEstablished` for `Restart`, and not before.
- A failed or cancelled reopen, reseek or device open leaves it in place for the next attempt, for Space, and for the checkpoint.
- `Load` clears it, as today.

Existing `Idle`/`Stopped` behaviour is otherwise unchanged: a stopped seek still validates and stores `Seek`, and a stopped `restart()` still executes at once.

## 5. One attempt

`service_reconnect` branches on `is_indefinite()`. Stations keep `fresh_open`. Finite sources run a new `reconnect_finite()`:

1. **Capture and tear down before any network I/O** (`capture_and_teardown`). This reads the heard position at the moment the attempt runs, settles the old generation's final heard delta (§6), and discards the ring, so the ring cannot move once the position is read. After the first attempt there is no transport; later attempts use `self.position` as it stands.
2. **Target**: `Seek(t)` → `t`; `Restart` → `ZERO`; none → the captured position.
3. `ensure_source_open()`. It already sets `expected = Finite` for an established finite session, so a reopen that answers as a station fails with `ResourceChanged`.
4. `reseek(target)` → `(actual, provenance)`.
5. **Install the landing as the anchor.** Keep `previous = (position, provenance)`, then set `position = adopt_preserved(target, actual)` and its provenance *before* the transport opens. `open_transport()` copies `self.position` into the new `TransportCore`'s anchor, so a landing assigned afterwards would play audio from 200 s while progress and every later capture count from 100 s. `restore()` already uses this order.
6. **Open and prime, attempt-local.** `attempting = true; attempt_failure = None; open_transport(playing = false); attempting = false`. While `attempting`, `pump_audio`'s finite failure arms record the failure in `attempt_failure` and drop the source, as `source_ended` does for stations, instead of calling `fail_with`. This covers a remote cause and a remote decode error; `attempt_failure` widens to carry either. Nothing has started running yet, so a failed attempt plays no audio.
7. **Commit check**, in `fresh_open`'s order, before anything is announced:
   - retired or interrupted → cancelled;
   - `open_transport` returned `Err` → that error;
   - `attempt_failure` is set → that failure.

   On any of these: `abandon_attempt()` (teardown, retire, drop the source), restore `previous`, and leave `pending` in place. A priming that reached EOF without a frame (a target at the very end) is not a failure; it commits, and end-of-track follows as usual.
8. **Success**: `start_running()`; clear `pending` and emit its completion event (§4); clear `recovering`; `outage.playing_from()` starts the stability window (§6); announce `StateChanged(Playing)`.
9. **Failure**: cancelled → nothing; the command that cancelled it decides what happens next. Otherwise, the failure is classified under §3: retryable → `enter_reconnecting` on the same outage; anything else → `Failed`. A device that will not open is not the server's fault and fails at once, as on the live path.

`restore()` gets the same `pending` rule: read before `reseek`, cleared only after `reinstall` succeeds. It commits through the same attempt-local check (steps 6–7), so a priming failure during Space leaves `pending` in place and fails honestly, instead of announcing `Playing` over a `Failed` that `pump_audio` already set. That is the path Space takes after a pause or stop during an outage, when the network may still be down.

`recovering` is cleared on every exit from finite `Reconnecting`: success, `Failed`, pause, stop and load.

The budget is the existing one: backoff 1, 2, 4, 8, then 15 s repeating; a 5-minute budget measured from the outage's first failure and **evaluated only when something fails**, so an attempt already in flight can finish after it; the outage ends after 30 s of heard playback (§6).

## 6. Heard-time outage accounting

`Outage::is_over(position)` is replaced by a heard-time counter, so that seeks neither end nor extend the stability window:

- `Outage` gains `heard: Duration`, reset to zero by `playing_from()` and by `failed()`. `playing_from()` no longer takes a position. `is_over` becomes `heard >= policy.stable_after`, and is only true after a successful attempt has started the window.
- Only playback after a successful reconnect counts. Audio drained from the old ring during backoff does not: nothing is counted while the state is `Reconnecting`, and the window has not started.
- The worker feeds it deltas of the current generation's `Timeline::played_frames`, converted with **that generation's** sample rate. It keeps a per-generation baseline: a new generation (a seek, a reopen, a device rebuild) starts a new baseline at zero, without subtracting anything.
- A generation's final delta is settled before it ends. Every `capture_position` call — which already freezes and reads the played frames before every discard or teardown — adds the delta since the last baseline.
- The delta is also settled each pass in `service_reconnect`'s `Playing` arm, before `is_over` is asked.

For a station, heard time equals listening-time growth, so live behaviour is unchanged. The m7 suites pin that.

## 7. Commands during finite `Reconnecting`

| Command | Behaviour |
|---|---|
| `SeekTo(t)` | No network. Seek support is already `Native` for any source that got here. Clamp `t` against `recovery_duration` (unclamped when `None`, as a stopped seek with no established duration is). Store `Seek(t)`, emit `SeekTargetStored { target }`. During backoff the outage, backoff and schedule are untouched, and no extra attempt is triggered. If an attempt is in flight, `submit_seek`'s retirement cancels it (§2), and the next pass attempts at once toward the new target, with no backoff spent. |
| `SeekBy(n)` | Base is `pending`'s target if one is stored (`Restart` counts as `ZERO`), otherwise `self.position`. Then as `SeekTo`. This covers `SeekBy` sent by the CLI or tests. Arrow keys go through `KeyRouter`; see below. |
| `Restart` | Store `Restart`, emit `SeekTargetStored { target: ZERO }` so the display and checkpoint follow. The landing emits `RestartEstablished` (§4). |
| `Pause`, `TogglePause` | **At submission:** `SourceTraits` gains `recovering: AtomicBool` (§3, §5). `submit_pause` retires the observed generation instead of freezing when `indefinite \|\| recovering`, so a reopen, reseek or priming read wakes at once with a retirement, and the attempt's commit check abandons it as cancelled. No freeze level is raised: the hook never parks a half-built transport or announces `Paused` mid-attempt, and the stall budget stays live. **At dispatch:** a new finite arm in `pause()`, taken in `Reconnecting`: clear the outage and `recovering`, take `frozen_by_hook` and `thaw()` (as `pause_indefinite` does, which reconciles a freeze raised before entry), `capture_and_teardown` if a transport exists, retire the source interrupt and the remote source, keep `pending`, bump `session_rev`, announce `Paused` unless the hook already did. Space then runs `restore()`. **Race:** if the attempt committed between `submit_pause` reading `recovering` and retiring, the retirement lands on the generation that is now playing. A dispatched `Pause` that finds a finite remote session in `Playing` with its generation retired or its source dropped takes the same arm, instead of parking a transport that has no source. |
| `Stop` | Unchanged. `do_stop` already clears the outage and keeps `pending`. |
| `Play` | No-op, as for a station. |
| `Load` | Unchanged. Replaces the media and clears the outage and `pending`. |

The `SeekBy` base rule applies in every state where `pending` can be set (`Stopped`, `Paused` after a recovery pause, `Reconnecting`).

**Frontend hold.** The worker rule alone does not make arrow keys accumulate. `KeyRouter` turns a burst into an absolute `SeekTo`, releases its hold on `SeekTargetStored`, and the next progress tick puts the old heard position back in the mirror. A second burst then starts from there and replaces the stored target instead of advancing it. `KeyRouter` therefore gains a third hold, `stored: Option<Duration>`:
- `SeekTargetStored { target }` sets `stored = Some(target)` (the worker's clamped value) and clears `submitted`, instead of releasing everything.
- `displayed_target()` becomes `burst.target().or(submitted).or(stored)`, so a new burst bases on the stored target and the display keeps showing it through progress ticks.
- `stored` is cleared by the events that resolve it: `SeekCompleted`, `RestartEstablished`, `SeekRejected`, `SeekCancelled`, `EndOfTrack`, and the load outcomes that already `cancel()`.
- `Stop` and `Restart` still drop an unsubmitted burst, but not `stored`: the worker keeps `pending` across a stop, and a `Restart` replaces it with its own `SeekTargetStored { target: ZERO }`.

This also fixes the same collapse for bursts in `Stopped` today.

## 8. Documentation

- `docs/architecture.md`:
  - §1: broaden "Reconnect attempts for a live stream continue a current Play" to cover finite recovery. The rules stay: cancellable at once, bounded, never started or resumed after a process restart.
  - §7.1: `Reconnecting` is no longer indefinite-only.
  - New §7.6 "Finite media recovery": eligibility and the retry table (§3), the attempt order (§5), `PendingResume` (§4), heard-time accounting (§6), and commands during recovery (§7).
  - §12: the cross-reopen validator limit (§1 of this spec).
- `docs/reference.md`: a bullet next to the live-stream one. A range-capable episode that drops reconnects with the same backoff; the five-minute budget is judged only when something fails, so an attempt in flight can finish later; seeking, pausing and stopping work during recovery; range-less servers still fail, and Space tries once more.
- `CHANGELOG.md` `[Unreleased]`, under Added.
- `docs/m10-acceptance.md` at the end.

## 9. Testing

All engine tests run on the virtual clock (`play_for`, `let_time_pass`, `play_until_event`), never a sleep.

### Harness additions (`tests/support/`)

- Faults are scripted per connection with the existing `Script::then` chain (`stall_body_after`, `truncate_body_after`, `stall_headers`); there is no `stall_only_first_response`. A helper `server(&[(ordinal, Script)])` keys each fault by absolute 1-based connection ordinal and serves every other ordinal normally. An open costs 4 requests (probe, open, Symphonia's tail read, rewind) and a seek costs 1. Every drop faults both the cut and the transport's in-place re-request.
- `TestEngine::rendered()` and `clear_rendered()` expose the virtual device's existing render log (`TestOutput::captured()` already holds every rendered sample since the last clear). Nothing new is opt-in. Render-log tests keep the clock stopped at the capture (`frozen_attempts()`).
- A WAV builder for a **frame-index fixture**: stereo 16-bit PCM at the virtual device's sample rate, so nothing is resampled. Each frame encodes its own index in base 1024: left = `index / 1024`, right = `index % 1024 + 1`. The right channel is never zero, so no encoded frame equals the silence marker and filtering silence cannot hide a skipped frame. The values stay small enough to decode exactly whichever i16 to f32 scale Symphonia uses.

### `tests/m10_finite_reconnect.rs`

1. **Audio continuity across a buffered stall.** Frame-index fixture, served ranged, stalling only the first response, with audio still in the ring. The policy's backoff (20 ms) is shorter than the 300 ms ring. Assert that the rendered log, silence removed, decodes to consecutive frame indices on both sides of exactly one seam, which replays exactly `UNHEARD_FRAMES` (one output latency, 4800 frames at 48 kHz): the heard model counts only played frames, so the latency in flight at the capture is rendered again once, as on every capture-and-reinstall path (Stop then Play too). Nothing else is repeated or skipped. Also assert the position after recovery equals the captured position within one callback period.
2. **`TruncatedBody` on every path.** A first-read truncation reconnects. A truncation during the attempt's reopen or reseek is retried on the same outage.
3. **Not eligible.** A range-less server (`without_ranges`) fails as today, with no `Reconnecting`.
4. **Seek while reconnecting.** `SeekTargetStored` is emitted; the server's request count does not change until the next scheduled attempt; that attempt lands on the target; `SeekCompleted` arrives only after install. Three `SeekBy(+n)` presses store `3n` past the base.
5. **Pending intent survives failure.** A failed attempt keeps `Seek(t)` for the next attempt. The same for a failed reseek in `restore()`.
5a. **Landing is the anchor.** Recovery with a stored `Seek(200 s)` from a capture at 100 s: the first progress after the landing, and a capture taken shortly after, read about 200 s plus the time played, never about 100 s. On the frame-index fixture, the first rendered index matches the landing.
5b. **Priming failure is the attempt's.** The reopen and reseek succeed, but the first read after the reseek fails retryably (truncated or stalled). The engine stays in `Reconnecting`, with no `Playing`, `SeekCompleted` or `RestartEstablished`. `pending` and the pre-attempt position survive, and the next attempt lands. A non-retryable priming failure ends in `Failed` without a `Playing` before it. The same check for `restore()`: a priming failure during Space emits no `Playing` and keeps `pending`.
6. **Restart intent.** `Restart` during recovery → the landing emits `RestartEstablished`, not `SeekCompleted`. It survives a failed attempt, and Pause → Space and Stop → Space through `restore()`. A later `SeekTo` supersedes it (the landing emits `SeekCompleted`).
7. **Pause and stop while reconnecting.** The outage is cleared, `pending` survives, Space resumes at it, and `Session` checkpoints the stored target. Pause goes through `submit_pause` (not a raw `Pause`) in each phase: during backoff, during a blocked reopen, during a blocked reseek, and during a priming read against a stalled server. Each cancels at once on the virtual clock, with no stall timeout spent, lands `Paused` exactly once, and leaves no freeze level standing (`is_frozen()` false). A pause submitted just before an eligible failure lands in the recovery-pause state, not `Reconnecting`.
7a. **Arrow bursts accumulate across progress.** Driven through `KeyRouter`, not raw `SeekBy`: a burst during recovery stores its target; progress ticks arrive; a second burst lands at the first target plus its own steps, not at the heard position plus its steps. The landing releases the hold. A `KeyRouter` unit test pins the same thing for `Stopped`.
8. **Budget.** Retryable failures past the budget end in `Failed`; Space then tries exactly once.
9. **Heard-time window (worker integration).** After a successful reconnect: a forward seek past `stable_after` does not end the outage; a backward seek does not stop it ending after `stable_after` of heard playback; ring drain during backoff does not count. Checked by making a later failure land inside or outside the same outage (whether the budget carried over).

Unit tests in `src/playback/reconnect.rs` cover the counter's arithmetic (reset on `playing_from` and `failed`, threshold). They do not replace test 9.

The m7 live suites (`m7_reconnect`, `m7_live_recovery`, `m7_live_playback`, `m7_cancellation`) must pass unchanged, and `m4_diagnostics` must still pass with the new log line.

## 10. Decisions

### Decisions made during planning

1. **The seek proof carries across a reopen.** A reopened remote source comes back `SeekSupport::Unknown`, which would make every recovery after the first ineligible. `ensure_source_open` keeps `Native` when the session had already proven it for that location.
2. **Seek frame conversion rounds.** `DecodedSource::duration_to_frames` truncated, while positions round to the nanosecond, so one frame in three converted back one short at 48 kHz and a resume replayed a frame. It now rounds, and `adopt_preserved` accepts a sub-tolerance difference in either direction.
3. **The render log already exists.** `TestOutput::captured()` holds every rendered sample since the last clear; the harness only exposes it.
4. **Per-connection faults reuse `Script::then`.** There is no `stall_only_first_response`.
5. **The frame-index fixture encodes `index / 1024` and `index % 1024 + 1`,** not base 32767.
6. **A recovery pause stores seeks and restarts offline,** exactly as `Reconnecting` does (§7's closing line made concrete).
7. **Test coverage substitutions.** The Session checkpoint of a stored target is pinned by `tests/session_policy.rs`; the non-retryable priming case is covered through Space's one-attempt path; the ring-drain case of test 9 is covered by the unit tests in `reconnect.rs`.

### Decisions made during execution

1. **Heard time from position minus anchor.** Counted per pass from the position's advance over the anchor, not literally from `Timeline::played_frames`. Equivalent, less code; at worst it drifts by sub-frame rounding.
2. **Faults keyed by connection ordinal.** The planned `server(playing, attempts)` chain helper assumed one request per open. `server(&[(ordinal, Script)])` replaces it, with ordinals named from measured costs (open 4, seek 1) and every count assertion exact. A change in Symphonia's or `HttpMediaSource`'s request shape breaks these tests loudly.
3. **Every cut also faults the in-place re-request.** The HTTP layer resumes a short ranged body itself, so a plain cut never reached the engine; the tests fault the re-request too, and the docs say when M10 engages (§1).
4. **Render continuity is one replayed latency.** One seam replaying exactly `UNHEARD_FRAMES`, consecutive on both sides. Test 1's "nothing repeated" is amended (§9). A listener may hear about 100 ms twice at a recovery, as after Stop then Play.
5. **The priming attempt keeps the source for `abandon_attempt`.** Dropping it retires the shared interrupt, so every priming failure read as a cancellation and retried outside backoff and budget. Pinned by `a_priming_failure_counts_against_the_budget`.
6. **Commands that prime while `Playing` yield to a drop.** If `restart` or `pause`'s rebuild primes, drops, and the state became `Reconnecting` or `Failed`, nothing is announced. An interrupted restart stores `PendingResume::Restart`, so the landing still reports `RestartEstablished`.
7. **The pause race predicate is broad.** `lost_source_while_playing` also closes a pause queued before a seek on a healthy remote episode; the seek is stored and Space reopens (one extra reopen). Narrowing it needs the seek bit visible at Pause dispatch.
8. **Space's device-start failure says "cannot start playback"** (restart keeps "cannot start the audio device").
