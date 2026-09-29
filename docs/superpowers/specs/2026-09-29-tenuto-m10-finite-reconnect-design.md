# M10 — Finite HTTP recovery

Status: approved in conversation on 2026-09-29. It is ready for an implementation plan. The design below is the agreed contract; the code does not yet match it.

Branch: `feature/finite-reconnect`, from `main` at `4fb5245`.

## 1. Decision

A finite HTTP episode that stalls or drops mid-play (a VPN profile switch, a Wi-Fi blip, a server hiccup) recovers on its own and keeps playing from where the listener was, instead of landing in `Failed` with "the server went quiet during Stall".

Recovery reuses the live-stream machinery: the worker state `Reconnecting`, `Outage` and `ReconnectPolicy` (`src/playback/reconnect.rs`), driven from the worker loop and cancelled at once by a pause, stop, replacing load or shutdown. It does not watch the OS for network changes: the 15 s stall timeout already detects every cause of a dead connection, and network events would cover only one of them.

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
- `pause()` returns early unless the state is `Playing` (except for indefinite media).
- `SeekBy` bases on `self.position` (`engine.rs:2401`).
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

`enter_reconnecting` keeps its shape. On entry for a finite source it also caches the source's established duration (`established_duration(source.metadata())`, never an estimated one) into a new field `recovery_duration: Option<Duration>`, before `retire_remote_source` drops the decoder. Its log line becomes:
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
4. `reseek(target)`.
5. `open_transport(playing = true)`.
6. **Success**: `position = adopt_preserved(target, actual)` and its provenance; clear `pending` and emit its completion event (§4); `outage.playing_from(…)` starts the stability window (§6); announce `StateChanged(Playing)`.
7. **Failure**: cancelled → nothing; the command that cancelled it decides what happens next. Otherwise, the failure is classified under §3: retryable → `enter_reconnecting` on the same outage; anything else → `Failed`. A device that will not open is not the server's fault and fails at once, as on the live path.

`restore()` gets the same `pending` rule: read before `reseek`, cleared only after `reinstall` succeeds. That is the path Space takes after a pause or stop during an outage, when the network may still be down.

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
| `SeekTo(t)` | No network. Seek support is already `Native` for any source that got here. Clamp `t` against `recovery_duration` (unclamped when `None`, as a stopped seek with no established duration is). Store `Seek(t)`, emit `SeekTargetStored { target }`. The outage, backoff and schedule are untouched; no extra attempt is triggered. |
| `SeekBy(n)` | Base is `pending`'s target if one is stored (`Restart` counts as `ZERO`), otherwise `self.position`. Three quick Right presses advance three steps. Then as `SeekTo`. |
| `Restart` | Store `Restart`, emit `SeekTargetStored { target: ZERO }` so the display and checkpoint follow. The landing emits `RestartEstablished` (§4). |
| `Pause`, `TogglePause` | New finite arm in `pause()`: clear the outage, `capture_and_teardown` if a transport exists, retire the source interrupt and the remote source, keep `pending`, bump `session_rev`, announce `Paused`. Space then runs `restore()`. |
| `Stop` | Unchanged. `do_stop` already clears the outage and keeps `pending`. |
| `Play` | No-op, as for a station. |
| `Load` | Unchanged. Replaces the media and clears the outage and `pending`. |

The `SeekBy` base rule applies in every state where `pending` can be set (`Stopped`, `Paused` after a recovery pause, `Reconnecting`), so arrow presses always accumulate on the stored intent.

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

- `Script::stall_only_first_response()`, next to `truncate_only_first_response()`.
- An **opt-in** full render log on the virtual device (for example `TestEngine::record_rendered()` and `rendered()`). Off by default; `captured()` keeps its last-buffer meaning.
- A WAV builder for a **frame-index fixture**: stereo 16-bit PCM at the virtual device's sample rate, so nothing is resampled. Each frame encodes its own index: left = `index / 32767`, right = `(index % 32767) + 1`. The right channel is never zero, so no encoded frame equals the silence marker and filtering silence cannot hide a skipped frame.

### `tests/m10_finite_reconnect.rs`

1. **Audio continuity across a buffered stall.** Frame-index fixture, served ranged, stalling only the first response, with audio still in the ring. The policy's backoff (20 ms) is shorter than the 300 ms ring. Assert that the rendered log, silence removed, decodes to strictly consecutive frame indices: nothing repeated, nothing skipped. Also assert the position after recovery equals the captured position within one callback period.
2. **`TruncatedBody` on every path.** A first-read truncation reconnects. A truncation during the attempt's reopen or reseek is retried on the same outage.
3. **Not eligible.** A range-less server (`without_ranges`) fails as today, with no `Reconnecting`.
4. **Seek while reconnecting.** `SeekTargetStored` is emitted; the server's request count does not change until the next scheduled attempt; that attempt lands on the target; `SeekCompleted` arrives only after install. Three `SeekBy(+n)` presses store `3n` past the base.
5. **Pending intent survives failure.** A failed attempt keeps `Seek(t)` for the next attempt. The same for a failed reseek in `restore()`.
6. **Restart intent.** `Restart` during recovery → the landing emits `RestartEstablished`, not `SeekCompleted`. It survives a failed attempt, and Pause → Space and Stop → Space through `restore()`. A later `SeekTo` supersedes it (the landing emits `SeekCompleted`).
7. **Pause and stop while reconnecting.** The outage is cleared, `pending` survives, Space resumes at it, and `Session` checkpoints the stored target.
8. **Budget.** Retryable failures past the budget end in `Failed`; Space then tries exactly once.
9. **Heard-time window (worker integration).** After a successful reconnect: a forward seek past `stable_after` does not end the outage; a backward seek does not stop it ending after `stable_after` of heard playback; ring drain during backoff does not count. Checked by making a later failure land inside or outside the same outage (whether the budget carried over).

Unit tests in `src/playback/reconnect.rs` cover the counter's arithmetic (reset on `playing_from` and `failed`, threshold). They do not replace test 9.

The m7 live suites (`m7_reconnect`, `m7_live_recovery`, `m7_live_playback`, `m7_cancellation`) must pass unchanged, and `m4_diagnostics` must still pass with the new log line.
