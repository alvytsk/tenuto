# Tenuto M15: The ten-band equalizer — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `e` opens a ten-band EQ overlay (±12 dB, presets, on/off), the EQ shapes the decoded audio before the ring, and it persists — one global EQ for every media kind.

**Spec:** [`../specs/2026-11-09-tenuto-m15-equalizer-design.md`](../specs/2026-11-09-tenuto-m15-equalizer-design.md).

**Architecture:**
- Pure DSP first (`src/playback/eq.rs`), proven by unit tests against reference responses.
- Worker integration second: coefficients at buffer boundaries from an atomic snapshot; state reset on disable; recompute on `CapabilitiesChanged`.
- UI last (the overlay is the surface most likely to conflict with parallel TUI work).

**Branch:** `feature/m15-equalizer`, from `main`.

---

## Task 1: `src/playback/eq.rs` — the pure module

- [ ] `EqParams` → coefficient sets (RBJ peaking biquads, ISO centers 31.25 Hz … 16 kHz, f64 coefficients); per-channel biquad `process` step in f32 with denormal flush.
- [ ] The atomic `EqSnapshot` handle: version counter + `[f32; 10]` + enabled bit; readers take one consistent snapshot per buffer.
- [ ] Unit tests: center-band attenuation/boost within tolerance, neighbor-band bounds, cascade sums near the band gain, unity passes within 0.1 dB, disable resets state, denormal flush. This task merges on its own; nothing wires it yet.

## Task 2: Worker integration

- [ ] `PlaybackCommand::SetEq` (fire-and-forget like `SetVolume`); the decode worker applies the cascade on the resampled interleaved buffer immediately before the ring write; a mid-buffer parameter change waits for the next buffer.
- [ ] Disable = state reset, not just bypass; fully flat + disabled = pure bypass path. Recompute coefficients and reset state on `CapabilitiesChanged`.
- [ ] `tests/m15_eq.rs` part 1 (virtual clock): `SetEq` moves spectral dominance on a sine-sweep fixture through the test output tap; disabled is bit-transparent; format change survives; `TENUTO_AUDIO_OUTPUT=null` pacing with EQ on vs off within the stated budget.
- [ ] Additive persistence: `eq: Option<EqState { enabled, gains }>` with roundtrip test (gains are the truth; no preset name stored).

## Task 3: The overlay

- [ ] `e` opens/closes; the ten vertical sliders, dB scale, band labels, on/off and preset state per spec §1.3; keys: arrows/j/k select, left/right/`-`/`+`/`=` ±1 dB, `0` flat, `p` cycles the ten presets, `x` toggles.
- [ ] `tests/m15_eq.rs` part 2: overlay keys and rendering through the TUI harness.

## Task 4: Docs and gate

- [ ] `docs/reference.md` (the `e` key + Equalizer section); `docs/architecture.md` §4, §5, new §7.7, §12 revision; `CHANGELOG.md`; acceptance script (audible change, presets, persistence, live station without dropouts, spectrum reflects EQ, 96/24 CPU sanity) recorded in `docs/m15-acceptance.md`.
- [ ] Full gate: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast && RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`.
