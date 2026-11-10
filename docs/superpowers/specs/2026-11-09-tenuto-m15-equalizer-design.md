# Tenuto M15: The ten-band equalizer

Status: drafted on 2026-11-09 from a feature conversation. It is ready for an implementation plan. The code does not yet match it.

Context:
- Architecture §12 lists "equalizer" among the early out-of-scope decisions. This milestone revises that line deliberately: Winamp's most-requested feature, and tenuto's decoder-to-output path is short and fully owned, which is what makes an honest EQ tractable.
- It is real DSP work. The spec keeps it honest: a fixed cascade of biquads, no fft convolution, no per-source presets, one global EQ.

## 1. Decision

A 10-band parametric EQ on ISO octave centers — `31.25, 62.5, 125, 250, 500, 1k, 2k, 4k, 8k, 16k` Hz — each band a peaking biquad (RBJ cookbook, Q = 1.41 band default, gain −12…+12 dB in 1 dB steps), cascaded in order, applied to the decoded audio before the output ring.

### 1.1 Placement: decode worker, before the ring

The chain today: Symphonia decode → resample (rubato) → interleaved f32 frames → rtrb ring → CPAL callback (drain, `SpanRecord`, spectrum tap, volume gain via the link). The EQ sits **in the decode worker on the resampled interleaved buffer, immediately before the ring write**:

- The CPAL callback's contract ("lock, allocate, wait: never") is untouched — biquad coefficients are read once per buffer, not per sample, from an atomic snapshot; the callback thread does not change at all.
- The spectrum tap reads post-ring audio, so the visualizer hears the EQ, exactly as Winamp's did. (Volume gain commutes with LTI filters, so gain-at-the-link order does not matter.)
- The worker's real-time budget grows by 10 biquads × channels × frames — a few multiply-adds per sample; measured in the plan against the existing null-output pacing before merge.

Parameter transport: an `ArcSwap`-free, dependency-free atomic snapshot — a version counter plus a `[f32; 10]` gain array plus an `enabled` bit, read under `SeqCst` at each buffer start. A mid-buffer change takes effect on the *next* buffer (a few ms), so a swap can never click inside a buffer.

State: each biquad keeps per-channel `x1/x2/y1/y2` state. Disable resets state (not just bypasses) so a re-enable never lurches from stale history. Band gains that are 0 dB still run through their biquad (idempotent at unity, and state stays warm); a fully flat *and* disabled EQ is a pure bypass.

Denormals: flush-to-zero on subnormal accumulation (a branch on magnitude in the biquad update), so a quiet passage through ten cascaded filters cannot stall the worker. Sample rate: coefficients are computed per active sample rate; the worker recomputes on `CapabilitiesChanged` (a stream format change), resetting state.

### 1.2 Presets

A static table, compiled in, named as Winamp named them: `Flat, Rock, Pop, Jazz, Classical, Dance, Club, Bass, Treble, Laptop`. Each preset is ten gains in dB; coefficients derive from gains at startup — presets are data, never code paths. Selecting a preset sets the ten gains and keeps the EQ enabled (Flat included: it enables a flat EQ rather than disabling, so the display stays truthful).

### 1.3 UI

`e` in the main list opens the EQ overlay:

| Key | Action |
|---|---|
| Up, Down, `j`, `k` | select band |
| Left, Right, `-`, `+`, `=` | gain −1/+1 dB |
| `0` | all bands to 0 dB |
| `p` | cycle presets (the overlay shows which) |
| `x` | toggle EQ on/off |
| Esc, `e` | close |

Rendering: ten vertical sliders (block characters), band frequency labels, the dB scale, the on/off and preset state — a single overlay in the existing overlay layer, mouse untouched. The transport bar gains no EQ indicator; the overlay is the truth.

### 1.4 Persistence

`eq: Option<EqState>` with `#[serde(default)]`, where `EqState { enabled: bool, gains: [f32; 10] }` — the M13 additive rule, schema stays 4. Gains are stored as dB floats; the preset name is *not* persisted (gains are the truth; a preset is an input method, and a hand-tweaked "Rock +2 dB @ 1k" must not display as Rock).

Per-playlist EQ, asked for in the original survey, is cut to a recorded decision: one listening context exists per session (one output device, one stream), so a per-playlist EQ multiplies state without a use case Winamp itself ever had — its EQ was global. Revisit only with a concrete scenario (§8).

### 1.5 Scope of application

The EQ applies to every media kind: local files, finite HTTP, live streams, podcast episodes. There is no per-source opt-out; the state snapshot and checkpoints are unaffected (EQ is presentation, never position or progress — nothing in §7 of the architecture contract moves).

## 2. What exists today (verified)

- The decode worker's resample → ring path (`src/playback/decode.rs`, `resample.rs`, `engine/worker.rs`) is the single production write point for decoded PCM; the test outputs (`output/null_output.rs`, `output/test_output.rs`) drive the same `CallbackCore`.
- Volume applies post-ring through the link (`link.set_gain`, `engine/worker.rs` around lines 728/1480) — untouched by this milestone.
- `CapabilitiesChanged` already announces format changes; the worker rebuilds resampler state there today, and the EQ coefficients join that rebuild.
- Additive persistence with `#[serde(default)]` (M13's rule).

## 3. Module layout

- `src/playback/eq.rs` — pure: `EqParams` (gains, sample rate) → coefficient sets; the biquad `process` step; the atomic `EqSnapshot` handle. Engine rank (2), no imports above it.
- The worker wires the snapshot into its resample→ring seam; the runtime forwards `AppCommand::SetEq`/`SetEqEnabled` to the engine as a new `PlaybackCommand::SetEq` (the command/event protocol of §7.1 gains exactly one command; the engine answers with an `EqChanged`-free design — the command is fire-and-forget like `SetVolume`, and the UI reads back from its own state).

Layering: `playback` never learns persistence; the snapshot fields serialize in `persistence/model.rs` and `session` forwards values, as volume does today.

## 4. Keys summary (additions to `docs/reference.md`)

| Key | Action |
|---|---|
| `e` | Open or close the equalizer overlay |

## 5. Tests

- Unit (in `src/playback/eq.rs`): coefficient math against reference responses — a sine at a band's center frequency is attenuated/boosted within tolerance; neighbor bands at ±1 octave stay within bounds; a −12 dB cascade sums near −12 dB at center; unity gains pass through within sub-0.1 dB; state reset on disable; denormal flush correctness.
- `tests/m15_eq.rs` — integration on the virtual clock: `SetEq` changes the audible path (the test output taps post-EQ samples — assert spectral dominance moves with a bass boost on a sine-sweep fixture); disabled EQ is bit-transparent on the same fixture; mid-buffer parameter swap lands on a buffer boundary; format change (resample) recomputes coefficients; persistence roundtrip; the overlay's keys and rendering through the TUI harness.
- Performance sanity: the null-output pacing path (`TENUTO_AUDIO_OUTPUT=null`) with EQ on vs off stays within the plan's stated budget on the sweep fixture.

## 6. Documentation

- `docs/reference.md`: the `e` key, an Equalizer section (bands, presets, persistence, applies-to rule).
- `docs/architecture.md`: §4 playback's responsibility line gains the EQ stage; §7 gains a short "7.7 Equalizer" (placement before the ring, atomic snapshot, state reset rules, why the callback is untouched); §5's decode-worker row mentions the biquad cascade; §12 drops "equalizer" from the out-of-scope line and records the revision decision (one sentence, dated).
- `CHANGELOG.md` at implementation time.

## 7. Acceptance

Manual script, recorded in `docs/m15-acceptance.md`: audible bass/treble change on a familiar track; presets land where the table says; EQ persists across restart; a live station equalizes without dropouts (the reconnect path reopens the transport — coefficients survive because the worker owns them, not the transport); spectrum reflects the EQ visually; CPU sanity with EQ on at 96 kHz/24-bit playback.

## 8. Out of scope

- Per-playlist EQ (§1.4 records the decision), per-track auto-gain (that is ReplayGain, a separate survey item), a preamp slider (the cascade's own headroom behavior is measured in tests; a preamp invites clipping debates without a use), user-defined presets, EQ in `play` (the plain front end takes no keys beyond its five; EQ is the player's), and fft-domain convolution.
- Compressor/limiter ahead of clipping: out of scope; +12 dB on ten bands can clip at the device, which is the user's volume knob to mind, stated in the docs.

## 9. Risks

- Coefficient churn on format changes: a mid-track device change rebuilds coefficients and resets state — an audible seam, inherent to transport rebuilds today (the same seam volume rides through); the acceptance script includes a device rate change for exactly this.
- Numeric drift in the cascade at extreme gains is bounded by the f64 coefficient / f32 state split (coefficients computed in f64, applied in f32); the unit tests pin the tolerance rather than hoping.
- The overlay is the fourth overlay surface; the input-routing table in `tui/input.rs` is the file most likely to conflict with parallel TUI work — the plan sequences the overlay last, after the audio path is proven.
