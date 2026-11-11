# M14 revision: visualizer rollback + settings overlay — design

Date: 2026-11-11
Status: approved (2026-11-11)
Supersedes (in part): docs/superpowers/specs/2026-11-09-tenuto-m14-winamp-visuals-design.md
Branch: m14-winamp-visuals (unmerged; nothing here has shipped, so removals are compat-free)

## Intent and why

The human tried the M14 visualizers on real audio and rejected them: scope
rendered as a thick, barely-changing bar, flow as colored mush. The agreed
resolution is to remove the non-default visualizer modes entirely (the bars
panel predates M14 and stays exactly as it was) and to spend the milestone's
"beauty" budget on what the human actually asked for next: a settings screen
with version information and theme switching.

The one UX decision the human made explicitly: themes are switched by moving
a cursor through the list with **instant live preview** — the interface
recolors on every move. (M14's original "themes apply at startup and never
switch live" decision is reversed by this spec, deliberately.)

## Goals

- Bars remains the one, always-on visual panel; scope/flow and their entire
  support chain are removed.
- A settings overlay, opened with `o`: four themes with live-preview cursor
  navigation, plus an app version line.
- Theme choice made in settings persists (same storage `--theme` uses).
- Docs and tests describe what actually ships.

## Non-goals

- No other settings (volume, mouse, artwork keep their existing controls).
- No mouse support in the settings overlay (every existing overlay except
  browser/input is key-driven; mouse can come later as its own change).
- No new themes, no skin files, no theme export.
- The spectrum worker keeps its pre-M14 shape (no waveform publishing).

## Anti-goals (failure modes)

- A settings screen that changes nothing until "apply" — the human chose
  live preview.
- Orphaned references: docs, help text, or keybindings advertising removed
  modes (`v`, `--visualizer`) or a waveform the worker no longer publishes.

## Constraints

- Schema stays 4. Removing the `visualizer_mode` field from the persisted
  model is safe: it never shipped in a release, and serde ignores unknown
  keys in existing files.
- Runtime code: no `unsafe`, no `unwrap`, no `expect`; every cargo command
  `--locked`; layering ranks unchanged (persistence 0, playback 2,
  session/application 3, tui 4, cli 5); gruvbox stays byte-identical and
  paints no background.

## Approaches considered

1. **Settings as an overlay in the existing `Overlay` enum (chosen).** The
   house pattern: Help, SortMenu, Input, JumpToFile all open with a key,
   float over the frame, close with Esc. SortMenu is the closest precedent
   (cursor list, ▶ marker, green-on-ink selection, centered bordered box).
2. **A full-screen settings view.** Rejected: tenuto has no view switching;
   every surface is a floating overlay. A new navigation concept for one
   screen is disproportionate.
3. **Inline settings panel carved from the layout.** Rejected: steals queue
   rows at every tier and has no place for a centered list.

Persistence timing — save on close vs save on every change: **save on every
change** (controller's call). Each move to a different theme dispatches
`AppCommand::SetTheme(Some(spelling))` immediately, mirroring `--theme`'s
write-through; quitting straight from settings cannot lose the choice, and
the persistence layer already debounces ordinary-urgency writes.

## Part 1 — visualizer rollback (removals)

Everything M14 added for non-bars modes goes:

- `src/tui/spectrum.rs`: `VisualizerMode`, `next_visual`, `draw_scope`,
  `draw_flow`, `FlowHistory`, `MAX_FLOW_DEPTH`, the waveform/flow state and
  mode/width/height parameters on `SpectrumDisplay` (back to
  `update(source, now)`; no decay bookkeeping beyond today's pre-M14
  behavior), `waveform`/`flow` fields and `fresh_waveform` on `DrawSource`.
- `src/tui/render.rs`: mode dispatch in `draw_spectrum` (bars body becomes
  `draw_spectrum` again), the minimal-tier `height <= 1` rule (it existed
  only for scope/flow), `Visuals.waveform`/`Visuals.flow`.
- `src/tui/mod.rs`: the `set_wants_waveform` call, the width/height
  threading into `update`, the `ui.visualizer`/`ui.spectrum_rows` seeds.
- `src/tui/state.rs`: `UiState.visualizer`, `UiState.spectrum_rows`,
  `with_appearance`'s visualizer half.
- `src/tui/input.rs`: the `v` arm and `next_visual`.
- `src/tui/theme.rs`: unchanged (all four themes stay).
- `src/playback/spectrum/worker.rs`: `WAVEFORM_COLUMNS`, `waveform_points`,
  `wants_waveform`, `set_wants_waveform`, `SpectrumFrame.waveform`,
  `AnalyzedWindow.waveform` — the worker returns to its pre-M14 published
  frame.
- `src/persistence/model.rs`: `theme`/`visualizer_mode` — the `theme` field
  stays (settings uses it), `visualizer_mode` is removed.
- `src/session.rs` / `src/application/runtime.rs`: `set_visualizer_mode` and
  `AppCommand::SetVisualizer` die; `set_theme`/`SetTheme` stay (settings
  dispatches them at runtime now).
- `src/cli.rs` / `src/app.rs`: `--visualizer` dies on both the bare command
  and `tui`; `--theme` stays on both.
- Tests: the visualizer/waveform tests leave `tests/m14_visuals.rs`,
  `tests/m5_spectrum_engine.rs`, `tests/m5_tui_render.rs`, `tests/cli.rs`
  (mechanical, plus the pinned help text loses the flag); theme tests and
  the background-fill tests in `tests/m14_themes.rs` stay untouched.
- The startup resolution keeps only the theme path: persisted raw string →
  `ThemeId::from_stored` → gruvbox default; `--theme` still writes through.

## Part 2 — settings overlay

State (`src/tui/state.rs`):
- New `Overlay::Settings` variant.
- `UiState.settings_cursor: usize` (the SortMenu precedent), default 0.
- `UiState.theme_id: ThemeId` beside `theme`, kept in sync: startup resolves
  the id anyway; settings moves update both together. `theme` stays the
  render-facing palette; the doc comment "never switch live" is rewritten.

Input (`src/tui/input.rs`):
- `o` in the main list opens: cursor starts at the index of the current
  `theme_id` in the theme list; `Overlay::Settings` set.
- `settings_overlay(key, ui)` handler, modeled on `sort_menu_overlay`:
  `Esc`/`o` close; `Up`/`k` and `Down`/`j` move the cursor (saturating /
  clamped); on a move to a different theme: set `ui.theme_id` and
  `ui.theme` to the new `ThemeId`'s row and dispatch
  `Effect::App(AppCommand::SetTheme(Some(spelling)))`. No Enter — selection
  is continuous. Other keys: no effect.

Render (`src/tui/render.rs`):
- `draw_settings_overlay(buffer, area, cursor, theme)`: centered bordered
  box (organize-menu geometry class), green border, title " settings ".
  One line per theme: the storage spelling (`gruvbox`, `winamp`, `nord`,
  `solarized-dark`) — the same names the CLI takes; cursor line carries the
  ▶ marker and green-on-ink like the organize menu. A muted footer line:
  `tenuto {version}` from `env!("CARGO_PKG_VERSION")` (the value
  `tenuto --version` prints).
- The overlay draws after the theme fill like the other floats; because the
  cursor move already re-themed `ui.theme`, the whole frame recolors behind
  the box on the same redraw — the live preview.
- Help overlay: gains the `o` row, loses the `v` row.

## Part 3 — docs, changelog, acceptance

- `docs/reference.md`: `tui`/bare rows lose `--visualizer`; options keep
  `--theme` (wording: "saved as the new default"); keys table gains `o`
  (settings), loses `v`; the visual-panel paragraph returns to describing
  the bars panel; a settings paragraph replaces the visualizer prose.
- `docs/architecture.md`: §5's `tenuto-spectrum` row reverts to the
  pre-M14 sentence; §12's decision bullet is rewritten: built-in themes,
  switched live from the settings overlay (replacing the
  "no live theme-switch key" wording), no skin files; the M14 visual-modes
  sentence is dropped.
- `CHANGELOG.md` `[Unreleased]`: the M14 Added bullet is rewritten to
  describe themes + the settings overlay (no visualizer modes, no
  waveform); hard-wrap kept.
- `docs/m14-acceptance.md`: rewritten for what ships — bars-only panel;
  `o`/arrows live re-coloring; choice survives restart; version line;
  `--theme` unchanged; `--visualizer`/`v` gone. Manual rows stay honestly
  pending.

## Tests (the shape; exact placement is the plan's business)

- Settings input: open (`o`) lands the cursor on the current theme; moving
  re-colors `ui.theme`/`ui.theme_id` and dispatches `SetTheme(Some(...))`
  with the exact spelling; wrapping-free clamped cursor; Esc/o close
  without dispatching again.
- Render: the overlay box lists four themes + version line; cursor marker
  on the right line; live preview visible in a full-frame render (themed
  background switches with the cursor).
- Persistence: a state saved with `visualizer_mode` still loads (key
  ignored); `theme` round-trips as before.
- Rollback pins: `--visualizer` errors as an unknown argument on both
  command forms; the worker's frame carries no waveform (pre-M14 shape);
  bars render at one-row height as before M14.
- Version line: equals the clap `--version` value.

## What's left to the builder

- Exact test-file organization (which suite hosts which case), inline vs
  integration placement, and the precise help-text pin wording.
- The centered-box dimensions (within the organize-menu geometry class).
