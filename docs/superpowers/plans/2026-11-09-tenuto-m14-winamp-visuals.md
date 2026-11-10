# Tenuto M14: Winamp visuals — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `v` cycles the visual panel (bars → scope → flow), and `--theme` selects among four built-in themes, `winamp` included, with an optional painted background.

**Spec:** [`../specs/2026-11-09-tenuto-m14-winamp-visuals-design.md`](../specs/2026-11-09-tenuto-m14-winamp-visuals-design.md).

**Architecture:**
- Themes land first (pure style work), the background fill second, visualizer modes last (the only engine-touching task).
- The spectrum worker's frame gains an optional decimated waveform; nothing else in `playback` moves.

**Branch:** `feature/m14-winamp-visuals`, from `main` (after M13; no code dependency on it).

---

## Task 1: Theme table

- [ ] `ThemeId` + `THEMES` in `src/tui/theme.rs`; specify all nine fields for `gruvbox` (byte-identical to today), `winamp`, `nord`, `solarized-dark` — no placeholder values; gruvbox stays the `Default`.
- [ ] `--theme <name>` on `tui` (clap validation naming the valid set on error); additive `theme: Option<String>` persistence with roundtrip test.
- [ ] `tests/m14_themes.rs` part 1: table-driven completeness, default selection, flag validation, persistence.

## Task 2: The background fill

- [ ] `bg: Option<Color>` on `Theme` (`None` for gruvbox); when `Some`, one full-buffer fill pass first in `render()`, before layout.
- [ ] Buffer-level render test: gruvbox output byte-identical to a pre-change snapshot; winamp filled. Walk every layout tier (80×28, compact, minimal) at both widths — a tier that relied on the terminal background cannot merge unnoticed (spec §9).
- [ ] Rewrite the module doc's "paints no background" sentence to the per-theme property.

## Task 3: Visualizer modes

- [ ] Extend the published spectrum frame with an optional decimated waveform (96 columns, peak/min per column), computed by the `tenuto-spectrum` worker only under an atomic request flag; existing spectrum suites stay green unmodified.
- [ ] Visualizer mode enum on `UiState` + additive persistence + `--visualizer bars|scope|flow`; `v` cycles; minimal tiers restrict to bars/scope.
- [ ] `scope` renderer (block-character envelope on a midline, silence falls flat) and `flow` renderer (per-column energy history ring in the widget, stacked colored blocks, peak-fall decay); no idle animation on silence in any mode.
- [ ] `tests/m14_visuals.rs` per spec §5 (request-gated waveform, decimation bounds, mode cycling, persistence, minimal-tier restriction, deterministic fixture frames through the injected slot).

## Task 4: Docs and gate

- [ ] `docs/reference.md` (`tui` flags, `v`, the visual-panel paragraph, theme list); `docs/architecture.md` §5 spectrum row + §12 revision sentence; `CHANGELOG.md`; acceptance script (all modes on a file and a live station; gruvbox renders byte-identical to today; `winamp` at all tiers) recorded in `docs/m14-acceptance.md`.
- [ ] Full gate: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast && RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`.
