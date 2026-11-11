# M14 Revision: Visualizer Rollback + Settings Overlay — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the scope/flow visualizer modes entirely (bars stays, the engine returns to its pre-M14 shape) and add a settings overlay (`o`) with live-preview theme switching and a version line.

**Architecture:** Four sequential tasks: (1) the user-visible visualizer removal across engine/tui/cli, (2) the dead command-chain and persistence-field removal, (3) the settings overlay on the existing `Overlay` enum + SortMenu patterns, (4) docs. Each task ends with the full suite green.

**Tech Stack:** Rust 1.98.1 (pinned), ratatui, clap, serde; cargo `--locked` everywhere.

**Spec:** docs/superpowers/specs/2026-11-11-tenuto-m14-settings-and-visuals-rollback-design.md (the plan argues from the spec; executors read both).

## Global Constraints

- Schema stays 4: removing the `visualizer_mode` persistence field is safe (never shipped; serde ignores the key in existing files).
- Runtime code: no `unsafe`, no `unwrap`, no `expect` (tests exempt via `clippy.toml`). Every cargo command `--locked`.
- Gates before every commit: `cargo fmt` (verify `cargo fmt --check`), `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked --no-fail-fast`. `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` before the docs task's commit.
- Layering ranks unchanged (persistence 0, playback 2, session/application 3, tui 4, cli 5); no new top-level `src/` modules; never widen `tests/m9_layering.rs`'s `ALLOWED`.
- Gruvbox stays byte-identical, paints no background; the other three themes keep their palettes and backgrounds.
- The `theme` persistence field and `--theme` (both command forms, write-through) are untouched by Tasks 1–2.

## Review Focus

The five inputs most likely to bite a person, each pinned by a test:

1. A saved state written by an M14 dev build carries `"visualizer_mode":"flow"` — the app must load it, ignore the key, and run bars. → Task 2, Step 1 test in `tests/persistence_model.rs`.
2. `--visualizer` (either command form) after removal — clap must reject it as an unknown argument, exit 2, same as any typo'd flag. → Task 1, Step 1 tests in `tests/cli.rs`.
3. Theme saved under an unknown spelling (`"dracula"`) — startup resolves gruvbox and settings opens with the cursor on gruvbox. → Task 1 Step 4 keeps the existing startup-resolution test (unknown spelling → `Gruvbox`, the M14 Task-9 pin); Task 3's input test 1 pins the cursor landing on it.
4. Cursor saturated at the list edges (`Up` on gruvbox, `Down` on solarized-dark) — no move, no re-color, no `SetTheme` dispatch. → Task 3, input tests.
5. Settings overlay at every tier including minimal (one-row panels, tiny windows) — the centered box still draws, the frame still re-colors. → Task 3, buffer-level render test at two sizes.

---

### Task 1: The visualizer removal (engine + tui + cli)

**Files:**
- Modify: `src/playback/spectrum/worker.rs`, `src/tui/spectrum.rs`, `src/tui/render.rs`, `src/tui/mod.rs`, `src/tui/state.rs`, `src/tui/input.rs`, `src/cli.rs`, `src/app.rs`
- Modify (mechanical): `tests/m14_visuals.rs`, `tests/m14_themes.rs`, `tests/m5_spectrum_engine.rs`, `tests/m5_tui_render.rs`, `tests/cli.rs`, and any other call site the compiler names (`tests/m5_no_network.rs` historically constructs `Visuals`)

**Interfaces:**
- Consumes: the current code exactly as reviewed at `d67ae4c`.
- Produces (later tasks rely on this): `SpectrumDisplay::update(&mut self, source: &DrawSource<'_>, now: Instant)`; `draw_spectrum(buffer, area, levels: Option<&[f32]>, peaks: Option<&[f32]>, theme: &Theme)` rendering bars at every height; `Visuals` without `waveform`/`flow`; `UiState` without `visualizer`/`spectrum_rows`; `with_appearance(self, theme: Theme) -> Self` (Task 3 widens it again); `TuiOptions` without `visualizer`; the worker's frame without `waveform`.

- [ ] **Step 1: Write the removal-pin tests first**

In `tests/cli.rs`, replace the `--visualizer` parse assertions with rejection pins (both command forms):

```rust
#[test]
fn the_visualizer_flag_is_gone_from_both_command_forms() {
    let bare = Cli::try_parse_from(["tenuto", "--visualizer", "scope"]);
    let tui = Cli::try_parse_from(["tenuto", "tui", "--visualizer", "scope"]);
    assert!(bare.is_err());
    assert!(tui.is_err());
}
```

(Remove `assert_eq!(parsed.visualizer, None)`-style assertions and the `--visualizer` lines from both pinned help blocks; keep the `--theme` pins byte-exact.)

- [ ] **Step 2: Run them, watch the pins fail** (`cargo test --locked --test cli` — the flags still parse today, so the rejection asserts fail)

- [ ] **Step 3: Remove the machinery, leaf-first**

- `src/cli.rs`: both `--visualizer` args (`Cli` ~line 38, `Tui` ~line 85) and `validate_visualizer` (~line 145). Keep `validate_theme` and both `--theme` args untouched.
- `src/app.rs`: the two `TuiOptions` constructions stop forwarding `visualizer`.
- `src/tui/mod.rs`: `TuiOptions.visualizer` field gone; `resolve_visualizer` (~line 183) gone; the startup block keeps only the theme path — `SetTheme` write-through, then `ThemeId::from_stored(state.theme().unwrap_or("")) .unwrap_or_default().theme()` feeding `with_appearance(theme)`; `update_spectrum` (~line 468) drops the `ui.spectrum_rows` seed, the `set_wants_waveform` call, and passes only `(&DrawSource { .. }, Instant::now())` to `display.update`. `wants_analysis(row, view.phase)` stays (bars needs it).
- `src/tui/state.rs`: `UiState.visualizer` and `UiState.spectrum_rows` gone; `with_appearance(mut self, theme: Theme) -> Self` — rewrite its doc comment (the "no live-switch key" sentence goes; Task 3 replaces this comment again).
- `src/tui/input.rs`: the `KeyCode::Char('v')` arm (~line 490) gone, `next_visual` gone (it lives in `spectrum.rs`).
- `src/tui/spectrum.rs`: `VisualizerMode`, `next_visual`, `FlowHistory`, `MAX_FLOW_DEPTH`, `draw_scope`, `draw_flow`, `flow_color`, `lerp_color`, `DrawSource::fresh_waveform`, `SpectrumDisplay`'s `waveform`/`flow`/`last_mode` fields and `flow()`/`waveform()` accessors all gone; `update(&mut self, source, now)` keeps only the levels/peaks logic (fresh take + `DECAY_PER_DRAW` decay, unchanged from pre-M14). Rewrite the module doc and `SpectrumDisplay`'s doc without mode/flow/scope prose.
- `src/tui/render.rs`: `draw_spectrum(buffer, area, levels, peaks, theme)` — one body, the bars renderer, at every height (the `height <= 1` rule and the dispatch die); fold `draw_bars`' body in or keep the wrapper, one renderer either way; `Visuals` loses `waveform`/`flow` (~lines 47, 50) and `draw()`'s construction (~line 218) shrinks; the help keys list loses its `v` row.
- `src/playback/spectrum/worker.rs`: `WAVEFORM_COLUMNS`, `waveform_points` (~line 103), `Shared.wants_waveform` (~line 434), `set_wants_waveform` (~line 472), `SpectrumFrame.waveform`, `AnalyzedWindow.waveform`, `Analysis::accept`'s `wants_waveform` parameter (~line 363) and its `waveform_points` call (~line 405), the runner's per-pass flag read (~line 676) — the published frame returns to the pre-M14 shape. Update the module doc (the "optional decimated waveform" paragraph goes).

- [ ] **Step 4: Mechanically update the test suites**

Delete every test named for scope/flow/waveform/mode from `tests/m14_visuals.rs` (keep the levels-gate, bars-render, and `DrawSource` refusal tests, retargeted to the new `update` signature — and explicitly KEEP the theme-resolution test: unknown spelling → `Gruvbox`, the M14 Task-9 pin that Review Focus 3 relies on); `tests/m5_spectrum_engine.rs` loses its waveform tests and fixture fields; `tests/m5_tui_render.rs`/`tests/m5_no_network.rs` lose `Visuals`'s removed fields and the frame fixture's `waveform:`; `tests/m14_themes.rs`'s CLI cases lose `--visualizer` variants (theme cases untouched) and its buffer renders' `with_appearance` calls drop the second argument.

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
git add -A && git commit -m "refactor: remove the scope and flow visualizer modes (m14)"
```

Expected: all green; `grep -rn "VisualizerMode\|waveform\|FlowHistory" src/` returns nothing.

### Task 2: The dead command chain and persistence field

**Files:**
- Modify: `src/session.rs`, `src/application/runtime.rs`, `src/persistence/model.rs`, `tests/persistence_model.rs`, `tests/m14_visuals.rs` (its Session-level `set_visualizer_mode` tests)

**Interfaces:**
- Consumes: nothing from Task 1 (the chain is already unreferenced).
- Produces: `AppCommand` without `SetVisualizer`; `Session` without `set_visualizer_mode`; `PersistedState` without `visualizer_mode()`/`set_visualizer_mode`/the raw field. `Session::set_theme` and `AppCommand::SetTheme` stay (Task 3 dispatches them).

- [ ] **Step 1: Write the compat test first** — in `tests/persistence_model.rs`, a state saved WITH the key still loads (the byte-exact pin at ~line 272 keeps `"visualizer_mode":null` — edit it to keep the key in the *input* JSON while the model round-trip no longer carries it; assert the deserialized state has no visualizer accessor left by construction — the pin is that deserialization of the old JSON shape succeeds):

```rust
#[test]
fn a_saved_visualizer_mode_key_is_ignored() {
    let raw = r#"{"schema_version":4,"theme":"winamp","visualizer_mode":"flow"}"#;
    let state: PersistedState = serde_json::from_str(raw).expect("old shape loads");
    assert_eq!(state.theme(), Some("winamp"));
}
```

(Follow the file's existing deserialization helpers if its pattern differs; the assertion that matters: the old-shape JSON deserializes.)

- [ ] **Step 2: Run it, watch it fail** (the field still exists today, so this may already pass — if it passes, note that honestly in the report as a regression pin, mirroring the Task-10 gruvbox-pin precedent)

- [ ] **Step 3: Remove the chain** — `Session::set_visualizer_mode` (~session.rs:769), `AppCommand::SetVisualizer` (~runtime.rs:218) and its arm (~runtime.rs:815), the persistence field at all six sites in `src/persistence/model.rs` (RawState ~87, PersistedState ~119, `into_state` ~187, `Serialize` Out ~222/235, `Default`-adjacent ~251, accessors ~293–298). Leave the `#[allow(clippy::large_enum_variant)]` on `Action` alone (removing a variant may re-trip or re-clear the threshold — run clippy and keep whatever is honest: if the allow becomes unnecessary, delete it in this task).

- [ ] **Step 4: Update the byte-exact pin** in `tests/persistence_model.rs` — the serialized output no longer carries the key; delete the Session-level `set_visualizer_mode` tests from `tests/m14_visuals.rs`.

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
git add -A && git commit -m "refactor: drop the dead visualizer command chain (m14)"
```

### Task 3: The settings overlay

**Files:**
- Modify: `src/tui/theme.rs` (one constant), `src/tui/state.rs`, `src/tui/input.rs`, `src/tui/render.rs`, `src/tui/mod.rs`
- Create: `tests/m14_settings.rs`

**Interfaces:**
- Consumes: `AppCommand::SetTheme(Option<String>)` (runtime arm exists), `ThemeId::{Gruvbox, Winamp, Nord, SolarizedDark}` with `as_stored() -> Option<&'static str>` (total over the four), `ThemeId::from_stored`, the `Overlay` dispatch arms in `input.rs` (~line 65) and `render.rs` (~line 250), `draw_sort_menu_overlay` + `centered_box`/`inset` as the geometry/style precedent.
- Produces: `Overlay::Settings`; `UiState.settings_cursor: usize`, `UiState.theme_id: ThemeId`; `with_appearance(self, theme: Theme, theme_id: ThemeId) -> Self`; `THEME_ORDER: [ThemeId; 4]` in `theme.rs`; input arm `KeyCode::Char('o')` opening settings; `draw_settings_overlay(buffer, area, cursor: usize, theme: &Theme)`.

- [ ] **Step 1: Write the failing tests** — new `tests/m14_settings.rs` (follow `tests/m5_tui_input.rs` for key-dispatch fixtures and `tests/m14_themes.rs` for themed buffer renders):

Input (through the real key handler):
1. `o` opens: cursor lands on the current theme's index (`with_appearance(ThemeId::Winamp.theme(), ThemeId::Winamp)` → cursor 1, overlay `Settings`).
2. `Down`/`j` from gruvbox: `ui.theme` becomes winamp's palette, `ui.theme_id` Winamp, effects exactly `vec![Effect::App(AppCommand::SetTheme(Some("winamp".into())))]`.
3. Edge saturation: `Up` on gruvbox and `Down` on solarized-dark move nothing and return no effects.
4. `Esc` and `o` close; closing dispatches nothing.
5. Moving to the theme already current cannot happen via a move (order has no duplicates) — instead pin: re-opening after a move shows the moved-to cursor.

Render (buffer-level, themed):
6. The box lists the four storage spellings in `THEME_ORDER` order; the cursor line carries the `▶` marker; a footer line starts `tenuto ` and matches `env!("CARGO_PKG_VERSION")`.
7. Live preview: a full-frame `draw()` with overlay open at cursor winamp paints the winamp background (the Task-10 no-`Color::Reset` property on the winamp theme), at two sizes — the normal tier's reference size and a minimal-tier size — proving the overlay draws at every tier.

- [ ] **Step 2: Run them, watch them fail** (`cargo test --locked --test m14_settings` — compile errors: missing variant/fields/functions)

- [ ] **Step 3: Implement**

- `src/tui/theme.rs`: `pub const THEME_ORDER: [ThemeId; 4] = [ThemeId::Gruvbox, ThemeId::Winamp, ThemeId::Nord, ThemeId::SolarizedDark];` (same order as `THEMES`; doc: the settings cursor indexes this).
- `src/tui/state.rs`: `Overlay::Settings`; `UiState.settings_cursor: usize` (default 0) and `pub theme_id: ThemeId` (doc: kept in sync with `theme`; settings moves set both); `with_appearance(mut self, theme: Theme, theme_id: ThemeId) -> Self` sets both — rewrite the doc comment: the theme resolves at startup and the settings overlay switches it live.
- `src/tui/input.rs`: in the main list, `KeyCode::Char('o')` → `open_settings(ui)`: cursor = `THEME_ORDER` position of `ui.theme_id`, `ui.overlay = Overlay::Settings`, no effects. New `fn settings_overlay(key, ui) -> Vec<Effect>` registered beside `sort_menu_overlay`'s dispatch (~line 65): `Esc`/`o` close (no effects); `Up`/`k` and `Down`/`j` move `settings_cursor` clamped to `0..=THEME_ORDER.len()-1`; when the theme at the cursor differs from `ui.theme_id`: set `ui.theme_id` and `ui.theme` to that `ThemeId`'s row and return `vec![Effect::App(AppCommand::SetTheme(id.as_stored().map(str::to_owned)))]`; equal → no effects.
- `src/tui/render.rs`: `draw_settings_overlay(buffer, area, cursor, theme)` modeled line-for-line on `draw_sort_menu_overlay`: `centered_box(area, area.width.min(40), THEME_ORDER.len() + 2)`, `Clear`, green-bordered block titled " settings ", one line per `THEME_ORDER` entry showing `as_stored()`'s spelling, `▶` + green-on-ink on the cursor line, a muted (`theme.muted`) footer line `tenuto <version>` from `env!("CARGO_PKG_VERSION")`. Dispatch arm beside SortMenu's (~line 250). Help overlay: add the `o` row (`Open settings`), keep `? / Esc` semantics. Existing `with_appearance` call sites (startup, the `m14_themes.rs` renders) gain the `theme_id` argument mechanically — the compiler names them.


- [ ] **Step 4: Gates + commit**

```bash
cargo fmt && cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
git add -A && git commit -m "feat(tui): the settings overlay with themes and version (m14)"
```

### Task 4: Docs, changelog, acceptance

**Files:**
- Modify: `docs/reference.md`, `docs/architecture.md`, `CHANGELOG.md`, `docs/m14-acceptance.md`

**Interfaces:**
- Consumes: the shipped behavior at Tasks 1–3's head.

- [ ] **Step 1: `docs/reference.md`** — bare/`tui` rows keep only `--theme`; the options bullets keep the write-through wording, drop `--visualizer`; keys table gains `o` ("settings — themes, version"), loses `v`; the visual-panel paragraph describes the bars panel only (pre-M14 wording restored); a short settings paragraph: `o` opens, arrows re-color instantly and save, Esc closes, the version line shows the running release.

- [ ] **Step 2: `docs/architecture.md`** — §5's `tenuto-spectrum` row reverts to the pre-M14 sentence (no waveform); §12's decision bullet rewritten: built-in themes, switched live from the settings overlay, no skin files (the "no live theme-switch key" wording dies); drop the M14 visual-modes sentence; the intro's milestone range gains nothing (M14's shipped content is themes + settings).

- [ ] **Step 3: `CHANGELOG.md` `[Unreleased]`** — replace the M14 Added bullets with what ships: themes (four palettes, `--theme`, per-theme backgrounds, gruvbox unchanged) and the settings overlay (`o`, live-preview switching, version line). Keep a Changelog style, hard-wrapped.

- [ ] **Step 4: `docs/m14-acceptance.md`** — rewrite for what ships; manual rows stay `pending`: bars-only panel; `o` → arrows re-color live → restart keeps the theme; version line matches `--version`; `--theme` on both command forms; `--visualizer`/`v` rejected/gone; the automated-gates preamble cites the exact four commands with the final suite numbers from this task's run.

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo test --locked --no-fail-fast
git add -A && git commit -m "docs(m14): settings and bars-only visuals in reference, architecture, changelog, acceptance"
```
