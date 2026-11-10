# Tenuto M14: Winamp visuals — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The visual panel gains three modes (`bars`, `scope`, `flow`, cycled with `v`, persisted) and the player gains four built-in themes (`gruvbox`, `winamp`, `nord`, `solarized-dark`, chosen with `--theme`, persisted), with the three dark themes painting their own background.

**Architecture:** No new audio plumbing except one seam: the spectrum worker's published frame gains an optional decimated waveform, computed only when a listener asked for scope mode. Everything else is policy and rendering: a `VisualizerMode`/`ThemeId` vocabulary in `tui`, raw-string persistence fields following M13's additive rule, and two renderers beside the existing bars one. Theme resolution happens once at startup; there is no live-switch key.

**Tech Stack:** Rust 1.98.1 (pinned), ratatui, clap, serde. Every cargo command with `--locked`.

**Spec:** `docs/superpowers/specs/2026-11-09-tenuto-m14-winamp-visuals-design.md` — the plan argues from the spec; executors read both.

## Global Constraints

- No `unsafe`, no `unwrap`, no `expect` in runtime code (lints deny; tests exempt via `clippy.toml`).
- Every cargo command uses `--locked`; toolchain pinned by `rust-toolchain.toml`.
- Schema stays 4: both new fields are `Option<String>` with `#[serde(default)]` (M13's additive rule; `RawState` has no `deny_unknown_fields`).
- Layering (`tests/m9_layering.rs`): no new top-level modules, no rank changes, no `ALLOWED` additions. `VisualizerMode`/`ThemeId` live in `tui` (rank 4); persistence (rank 0) and `application` (rank 3) hold and move raw `String`s only. Fix the import, never add to `ALLOWED`.
- `--visualizer` is a run-scoped override (mirrors `--artwork`); `--theme` writes through to the saved state (spec §2.1 + §7 "theme … survive restart").
- Gruvbox stays byte-for-byte today's palette and paints no background; nothing changes for anyone who never asked.
- Silence looks silent in every mode: no idle animation (spec §1.2).
- The `winamp` theme's LED green (#33ff33-family) is robust *because* it paints its own bg — the background decision precedes the theme table (spec §9).

## Review Focus

1. A hand-edited state file with unknown strings (`"theme": "dracula"`, `"visualizer_mode": "plasma"`) must fall back to gruvbox/bars silently — never an error, never a crash. → Task 3's roundtrip tests assert unknown → `None` from the accessor, Task 9's resolution test asserts the default wins.
2. A one-row spectrum panel (compact below 22 rows) must render `flow` as the ordinary bars row, and `v` must skip flow — a one-row lava field means nothing (spec §1.2). → Task 8's render test, Task 9's skip test.
3. Scope on a live station must fall to a flat midline between bursts — a stale frame decays, never freezes the last burst (spec §7). → Task 7's decay test; acceptance script confirms on live media.
4. Flow's history must drop when the panel changes width or the mode switches — a resized panel must never read another width's columns (smear or panic). → Task 8's width-change test.
5. On a terminal that approximates Rgb (8/16 colors), every themed cell still carries the theme's bg — no `Color::Reset` cells left mid-panel. → Task 10's buffer test asserts every cell themed under `winamp`.

---

### Task 1: `VisualizerMode`

**Files:**
- Modify: `src/tui/spectrum.rs`

**Interfaces:**
- Produces: `#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub enum VisualizerMode { #[default] Bars, Scope, Flow }` with `pub fn next(self) -> Self` (Bars→Scope→Flow→Bars), `pub fn from_stored(raw: &str) -> Option<Self>` (`"bars"`→Bars, `"scope"`→Scope, `"flow"`→Flow, else None), `pub fn as_stored(self) -> Option<&'static str>` (Bars→None — the default serializes as absent, like `RepeatMode::Off`; Scope→`"scope"`; Flow→`"flow"`).

- [ ] **Step 1: Write the failing tests** (inline `mod tests` at the bottom of `src/tui/spectrum.rs`, the `src/playlist/repeat.rs` pattern)

```rust
#[cfg(test)]
mod visualizer_mode_tests {
    use super::VisualizerMode;

    #[test]
    fn the_cycle_returns_to_bars() {
        assert_eq!(VisualizerMode::Bars.next(), VisualizerMode::Scope);
        assert_eq!(VisualizerMode::Scope.next(), VisualizerMode::Flow);
        assert_eq!(VisualizerMode::Flow.next(), VisualizerMode::Bars);
    }

    #[test]
    fn storage_round_trips_and_defaults_unknown_values() {
        assert_eq!(VisualizerMode::Bars.as_stored(), None);
        for mode in [VisualizerMode::Scope, VisualizerMode::Flow] {
            let stored = mode.as_stored().expect("a spelling for a non-default mode");
            assert_eq!(VisualizerMode::from_stored(stored), Some(mode));
        }
        assert_eq!(VisualizerMode::from_stored("bars"), Some(VisualizerMode::Bars));
        assert_eq!(VisualizerMode::from_stored("Scope"), None);
        assert_eq!(VisualizerMode::from_stored(""), None);
        assert_eq!(VisualizerMode::from_stored("plasma"), None);
    }
}
```

Note `from_stored("bars")` is `Some(Bars)` — unlike `RepeatMode`, `bars` is a legal `--visualizer` spelling, so parsing is total over the vocabulary; only *storage* folds the default to absent.

- [ ] **Step 2: Run to verify failure** — `cargo test --locked --lib tui::spectrum` → FAIL (type not defined)
- [ ] **Step 3: Implement** the enum and the three methods in `src/tui/spectrum.rs`, with a module-doc paragraph: the `v` cycle, storage rule (default absent), and that the vocabulary is the CLI's too.
- [ ] **Step 4: Run to verify pass** — same command → PASS
- [ ] **Step 5: Commit** — `git commit -m "feat(tui): VisualizerMode bars, scope, flow (m14)"`

### Task 2: `ThemeId`, `Theme::bg`, and the four-theme table

**Files:**
- Modify: `src/tui/theme.rs`

**Interfaces:**
- Produces: `Theme` gains `pub bg: Option<Color>` (gruvbox: `None` — today's no-background property, now stated per theme in the module doc). `#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub enum ThemeId { #[default] Gruvbox, Winamp, Nord, SolarizedDark }` with `from_stored`/`as_stored` over the exact spellings `"gruvbox" | "winamp" | "nord" | "solarized-dark"` (all four parse; `as_stored` is total — an explicit `--theme gruvbox` is stored, unlike the visualizer default) and `pub fn theme(self) -> Theme` looking up `pub const THEMES: [(ThemeId, Theme); 4]`.

Exact palettes (spec §2.1: "every color field is specified … no placeholders"; gruvbox byte-for-byte today's `Default`; the other three are the plan's aesthetic call, tunable at acceptance, pinned here so the test can fail on a placeholder):

| field | gruvbox | winamp | nord | solarized-dark |
|---|---|---|---|---|
| `ink` | `#282828` | `#000000` | `#2e3440` | `#002b36` |
| `panel` | `#3c3836` | `#2a2a2a` | `#3b4252` | `#073642` |
| `line` | `#504945` | `#4a4a4a` | `#4c566a` | `#586e75` |
| `muted` | `#928374` | `#7d7d7d` | `#616e88` | `#657b83` |
| `text` | `#ebdbb2` | `#33ff33` | `#d8dee9` | `#839496` |
| `cream` | `#fbf1c7` | `#99ff99` | `#eceff4` | `#eee8d5` |
| `green` | `#b8bb26` | `#33ff33` | `#a3be8c` | `#859900` |
| `amber` | `#fabd2f` | `#ffaa33` | `#ebcb8b` | `#b58900` |
| `cyan` | `#8ec07c` | `#3399cc` | `#88c0d0` | `#2aa198` |
| `bg` | `None` | `Some(#0a0a0a)` | `Some(#2e3440)` | `Some(#002b36)` |

- [ ] **Step 1: Write the failing tests** (inline `mod tests` in `src/tui/theme.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::{ThemeId, THEMES};

    #[test]
    fn the_table_covers_every_theme_exactly_once() {
        assert_eq!(THEMES.len(), 4);
        for (id, theme) in THEMES {
            assert_eq!(*id, id.theme_id_of(theme), "table entry mismatch");
        }
        // Uniqueness: four distinct ThemeIds.
        let mut ids: Vec<_> = THEMES.iter().map(|(id, _)| *id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn the_three_dark_themes_paint_a_background_and_gruvbox_does_not() {
        assert_eq!(ThemeId::Gruvbox.theme().bg, None);
        for id in [ThemeId::Winamp, ThemeId::Nord, ThemeId::SolarizedDark] {
            assert!(id.theme().bg.is_some(), "{id:?} must paint its own bg");
        }
    }

    #[test]
    fn no_theme_ships_a_placeholder() {
        for (_, theme) in THEMES {
            for color in [theme.ink, theme.panel, theme.line, theme.muted, theme.text, theme.cream, theme.green, theme.amber, theme.cyan] {
                assert_ne!(color, ratatui::style::Color::Reset);
            }
        }
    }

    #[test]
    fn gruvbox_stays_bytes_identical_to_todays_palette() {
        let theme = ThemeId::Gruvbox.theme();
        assert_eq!(theme.ink, ratatui::style::Color::Rgb(0x28, 0x28, 0x28));
        assert_eq!(theme.green, ratatui::style::Color::Rgb(0xb8, 0xbb, 0x26));
        assert_eq!(theme.text, ratatui::style::Color::Rgb(0xeb, 0xdb, 0xb2));
        // …the remaining six fields likewise, copied from the current `Default`.
    }

    #[test]
    fn storage_round_trips_all_four_spellings() {
        for (raw, id) in [("gruvbox", ThemeId::Gruvbox), ("winamp", ThemeId::Winamp), ("nord", ThemeId::Nord), ("solarized-dark", ThemeId::SolarizedDark)] {
            assert_eq!(ThemeId::from_stored(raw), Some(id));
            assert_eq!(id.as_stored(), Some(raw));
        }
        assert_eq!(ThemeId::from_stored("dracula"), None);
        assert_eq!(ThemeId::from_stored("Nord"), None);
    }
}
```

(`theme_id_of` need not exist — write the first test as a direct equality per entry instead; the point is: every `ThemeId` has exactly one table row and `theme()` returns that row.)

- [ ] **Step 2: Run to verify failure** — `cargo test --locked --lib tui::theme` → FAIL
- [ ] **Step 3: Implement**: add `bg` to `Theme` (gruvbox `Default` gains `bg: None`); add `ThemeId`, `THEMES`, `from_stored`, `as_stored`, `theme()` per the table above; rewrite the module doc's "paints no background of its own" sentence to state the property per theme (spec §2.2).
- [ ] **Step 4: Run to verify pass** — same command → PASS
- [ ] **Step 5: Commit** — `git commit -m "feat(tui): ThemeId and the four built-in themes (m14)"`

### Task 3: Persistence — `theme` and `visualizer_mode` as additive fields

**Files:**
- Modify: `src/persistence/model.rs`
- Create: `tests/m14_themes.rs` (theme roundtrip), `tests/m14_visuals.rs` (visualizer roundtrip; later tasks append to both)

**Interfaces:**
- Produces: `PersistedState` gains private `theme: Option<String>`, `visualizer_mode: Option<String>` with `pub fn theme(&self) -> Option<&str>`, `pub fn set_theme(&mut self, theme: Option<String>)`, `pub fn visualizer_mode(&self) -> Option<&str>`, `pub fn set_visualizer_mode(&mut self, mode: Option<String>)`. `RawState` gains `#[serde(default)] theme: Option<String>` and `#[serde(default)] visualizer_mode: Option<String>`; `into_state` forwards them; the `Serialize` impl's `Out` struct gains `theme: &'a Option<String>` and `visualizer_mode: &'a Option<String>` (`None` serializes as `null`). Raw passthrough: persistence never parses the vocabulary.

- [ ] **Step 1: Write the failing tests.** In `tests/m14_themes.rs`, following `tests/persistence_model.rs`'s build-a-state-and-roundtrip-through-JSON pattern:

```rust
// theme roundtrip: a state with theme set to Some("winamp".into()), serialized,
// deserialized — asserts loaded.theme() == Some("winamp"); and the default
// PersistedState::default().theme() == None.
// Unknown spelling: deserialize a JSON object carrying "theme": "dracula" —
// loaded.theme() == Some("dracula") (raw passthrough; the *reader* defaults).
```

In `tests/m14_visuals.rs`, the same two tests for `visualizer_mode` (`Some("flow")` roundtrips; `"plasma"` passes through raw; default `None`).

- [ ] **Step 2: Run to verify failure** — `cargo test --locked --test m14_themes --test m14_visuals` → FAIL (no such methods)
- [ ] **Step 3: Implement** the fields, accessors, `RawState` entries, `into_state` forwarding and `Out` entries, each with an M13-style doc comment citing spec §1.3 / §2.1.
- [ ] **Step 4: Run to verify pass** — same command → PASS; also `cargo test --locked --test m5_state_schema --test m4_state_snapshot` stay green unmodified (schema 4 untouched).
- [ ] **Step 5: Commit** — `git commit -m "feat(persistence): additive theme and visualizer_mode fields (m14)"`

### Task 4: Session and `AppCommand` plumbing

**Files:**
- Modify: `src/session.rs` (beside `set_repeat`, line ~747), `src/application/runtime.rs` (`AppCommand` at line ~182, arms beside `SetRepeat` at ~line 801)

**Interfaces:**
- Consumes: Task 3's `set_theme`/`set_visualizer_mode`.
- Produces: `Session::set_theme(&mut self, theme: Option<String>) -> Action` and `Session::set_visualizer_mode(&mut self, mode: Option<String>) -> Action`, both `self.state.edit().…; self.submit(Urgency::Ordinary)`; `AppCommand::SetTheme(Option<String>)` and `AppCommand::SetVisualizer(Option<String>)` with runtime arms that forward to the Session methods and `submit` the action (the `SetRepeat` shape, spec §1.3's precedent). `None` is a real value: cycling back to bars sends `SetVisualizer(None)` and clears the field.

- [ ] **Step 1: Write the failing tests** in `tests/m14_visuals.rs` (Session-level, the `tests/m13_repeat.rs` shape): open a Session on a default state, `set_visualizer_mode(Some("flow".into()))`, assert `session.state().visualizer_mode() == Some("flow")`; set to `None`, assert it cleared; same pair for `set_theme`. Plus: `AppCommand::SetTheme(Some("winamp".into()))` dispatched through the runtime's command handling lands in state (the `tests/support/runtime.rs` harness pattern used by `m13_repeat` for `SetRepeat`).
- [ ] **Step 2: Run to verify failure** — `cargo test --locked --test m14_visuals` → FAIL
- [ ] **Step 3: Implement** the two Session methods and the two `AppCommand` variants + arms.
- [ ] **Step 4: Run to verify pass** — same command → PASS
- [ ] **Step 5: Commit** — `git commit -m "feat(application): SetTheme and SetVisualizer commands (m14)"`

### Task 5: CLI flags `--theme` and `--visualizer`

**Files:**
- Modify: `src/cli.rs` (`Tui` variant, line ~56), `src/tui/mod.rs` (`TuiOptions`, line ~95), `src/app.rs` (the `CliCommand::Tui` match constructing `TuiOptions`)

**Interfaces:**
- Consumes: Task 1/2's `from_stored`.
- Produces: `Tui` gains `#[arg(long, value_parser = validate_theme)] theme: Option<String>` and `#[arg(long, value_parser = validate_visualizer)] visualizer: Option<String>`; `TuiOptions` gains `pub theme: Option<String>`, `pub visualizer: Option<String>` (app.rs forwards). Validators in `cli.rs`:

```rust
fn validate_theme(value: &str) -> Result<String, String> {
    ThemeId::from_stored(value).map(|_| value.to_owned()).ok_or_else(|| {
        format!("unknown theme \"{value}\" (valid: gruvbox, winamp, nord, solarized-dark)")
    })
}
fn validate_visualizer(value: &str) -> Result<String, String> {
    VisualizerMode::from_stored(value).map(|_| value.to_owned()).ok_or_else(|| {
        format!("unknown visualizer \"{value}\" (valid: bars, scope, flow)")
    })
}
```

Both errors name the valid set, at parse time (spec §2.1).

- [ ] **Step 1: Write the failing tests** in `tests/m14_themes.rs` (CLI, the `tests/cli.rs` `Cli::try_parse_from` pattern): `["tenuto", "tui", "--theme", "winamp"]` parses to `Some("winamp")`; `--theme dracula` is an `Err` whose message contains `valid: gruvbox, winamp, nord, solarized-dark`; in `tests/m14_visuals.rs`: `--visualizer flow` parses; `--visualizer plasma` errs naming `valid: bars, scope, flow`; both flags absent → `None`.
- [ ] **Step 2: Run to verify failure** — `cargo test --locked --test m14_themes --test m14_visuals` → FAIL
- [ ] **Step 3: Implement** the args, validators and `TuiOptions` fields.
- [ ] **Step 4: Run to verify pass** — same command → PASS
- [ ] **Step 5: Commit** — `git commit -m "feat(cli): --theme and --visualizer on tui (m14)"`

### Task 6: The waveform — worker-side frame extension

**Files:**
- Modify: `src/playback/spectrum/worker.rs`, `tests/m5_spectrum_engine.rs` (struct literals), `tests/m5_tui_render.rs` (`frame()` literal)

**Interfaces:**
- Produces:
  - `pub const WAVEFORM_COLUMNS: usize = 96;`
  - `pub fn decimate_waveform(samples: &[f32]) -> Vec<(f32, f32)>` — always exactly `WAVEFORM_COLUMNS` pairs; column `c` covers samples `[c * len / 96, (c+1) * len / 96)` and carries `(min, max)` of that slice, each clamped to `[-1.0, 1.0]`; a column with no samples (input shorter than the columns it precedes) is `(0.0, 0.0)`; an empty input is 96 `(0.0, 0.0)` pairs (silence renders as the flat midline).
  - `SpectrumFrame` gains `pub waveform: Option<Vec<(f32, f32)>>`; `AnalyzedWindow` gains the same field.
  - `Shared` gains `wants_waveform: AtomicBool`; `SpectrumHandle::set_wants_waveform(&self, wanted: bool)` (a Relaxed store; the runner re-reads it every ≤10 ms pass — no wake needed).
  - `Analysis::accept` gains a `wants_waveform: bool` parameter computing `waveform: wants_waveform.then(|| decimate_waveform(samples))`; `FrameSchedule::publish` copies the window's field into the frame untouched (no smoothing on the waveform — honest envelope).
  - `Runner::analyze_blocks` reads `self.shared.wants_waveform.load(Ordering::Relaxed)` once per pass and passes it to `accept`.

- [ ] **Step 1: Write the failing tests**
  - In `tests/m14_visuals.rs` (pure decimation): 2048 samples of `0.5` → every pair `(0.5, 0.5)`; a spike of `-1.0` at sample 1024 of 2048 zeros → column 51 carries `(-1.0, 0.0)`; 10 samples → length 96, columns ≥ 10 are `(0.0, 0.0)`; `NaN` input is treated as `0.0` (the levels rule); values beyond ±1 clamp.
  - In `tests/m14_visuals.rs` (schedule pass-through, the `m5_spectrum_engine.rs` `window()`/`published()` helpers): `offer` a window with `waveform: Some(...)` → `tick` publishes a frame carrying it; `waveform: None` → frame `None`.
  - Inline in `worker.rs`'s `mod tests` (the existing `spawn` pattern): `set_wants_waveform(true)` → the flag is set on `Shared`; toggling off clears it.
- [ ] **Step 2: Run to verify failure** — `cargo test --locked --test m14_visuals --lib playback::spectrum` → FAIL
- [ ] **Step 3: Implement** the fields, function and plumbing. Update the existing struct literals mechanically: `worker.rs` tests (~line 743), `tests/m5_spectrum_engine.rs` (`SpectrumFrame` ~line 154 and `AnalyzedWindow` in `window()` ~line 160), `tests/m5_tui_render.rs` (`frame()` ~line 630) all gain `waveform: None`.
- [ ] **Step 4: Run to verify pass** — `cargo test --locked --test m14_visuals --lib playback::spectrum --test m5_spectrum_engine --test m5_spectrum_bands --test m5_spectrum_tap` → PASS (the M5 suites green unmodified is the "worker work unchanged when not requested" evidence; the flag's runner read is a three-line change covered by compile + Task 7's fixture path + the acceptance script).
- [ ] **Step 5: Commit** — `git commit -m "feat(playback): optional decimated waveform on the spectrum frame (m14)"`

### Task 7: Scope mode — view state and renderer

**Files:**
- Modify: `src/tui/spectrum.rs`, `src/tui/render.rs` (dispatch in `draw_spectrum`, line ~579; its in-file unit tests ~line 1186), `src/tui/mod.rs` (`Visuals`, line ~39; `update_spectrum`, line ~431)

**Interfaces:**
- Consumes: Task 1's `VisualizerMode`, Task 6's frame field.
- Produces:
  - `DrawSource::fresh_waveform(&self, now: Instant) -> Option<&'a [(f32, f32)]>` — the exact gate chain of `fresh_levels` (phase, revision, token, freshness).
  - `SpectrumDisplay` gains `waveform: Option<Vec<(f32, f32)>>`, decayed toward zero per draw by `DECAY_PER_DRAW` on both tuple entries when no fresh waveform, and `pub fn waveform(&self) -> Option<&[(f32, f32)]>`.
  - `SpectrumDisplay::update` gains a third parameter `mode: VisualizerMode`; it takes `fresh_waveform` only in `Scope` mode.
  - `pub fn draw_scope(buffer: &mut Buffer, area: Rect, waveform: Option<&[(f32, f32)]>, theme: &Theme)` in `spectrum.rs`: resample 96 columns to `area.width` by nearest index `x * 96 / width`; `midline = area.height / 2`, `half = area.height / 2`; per column, block cells (`'█'`, fg `theme.green`) from `midline - round(max * half)` down to `midline + round(min.abs() * half)` (both clamped into the area); a silent column (`min == max == 0`) draws the midline cell in `theme.muted` when `area.height >= 2`, nothing when height is 1. Stale/absent waveform: the display's decayed state, then the flat midline — exactly as bars fall.
  - `Visuals` gains `pub waveform: Option<&'a [(f32, f32)]>`; `update_spectrum` gains `ui: &mut UiState` (Task 9 seeds `spectrum_rows` there) and passes `mode` into `display.update`; the call site in `run_loop` (line ~401) and the `Visuals` construction (line ~404) gain the new fields. Mode dispatch itself lands in Task 9; until then `update_spectrum` passes `VisualizerMode::Bars` and `draw_spectrum` keeps its current rendering.

- [ ] **Step 1: Write the failing tests** in `tests/m14_visuals.rs`, the `tests/m5_tui_render.rs` `spectrum_display` module pattern (fixture `SpectrumFrame` with `waveform: Some(...)`, `DrawSource`, `SpectrumDisplay`): a fresh waveform is taken; a stale one decays both entries ×0.85; in `Bars` mode the waveform state stays `None`; `fresh_waveform` refuses a foreign revision/token. Buffer-level: `draw_scope` with a spike renders green blocks above and below the midline at the spike's column; all-zero input renders only muted midline cells (height ≥ 2) and nothing at height 1.
- [ ] **Step 2: Run to verify failure** — `cargo test --locked --test m14_visuals` → FAIL
- [ ] **Step 3: Implement** per the signatures above. Existing `SpectrumDisplay::update` call sites (`src/tui/mod.rs`, `tests/m5_tui_render.rs`) gain `VisualizerMode::Bars` mechanically.
- [ ] **Step 4: Run to verify pass** — `cargo test --locked --test m14_visuals --test m5_tui_render` → PASS
- [ ] **Step 5: Commit** — `git commit -m "feat(tui): scope renderer over the decimated waveform (m14)"`

### Task 8: Flow mode — history and renderer

**Files:**
- Modify: `src/tui/spectrum.rs`, `src/tui/render.rs` (dispatch and `Visuals`)

**Interfaces:**
- Consumes: Task 1's mode, Task 7's `Visuals`/`update_spectrum` shape.
- Produces:
  - `pub struct FlowHistory` in `spectrum.rs`: per-column energy ring, depth capped at `const MAX_FLOW_DEPTH: usize = 16`; `pub fn push(&mut self, width: u16, levels: Option<&[f32]>)` — a width change rebuilds from scratch (drops the old columns; the resample of `levels` to `width` is the bars value rule already in `draw_spectrum`); `pub fn decay(&mut self)` (× `DECAY_PER_DRAW` per entry — frames stop, the lava cools); `pub fn column(&self, x: usize) -> &[f32]` (newest last); `pub fn is_empty(&self) -> bool`.
  - `SpectrumDisplay` gains `flow: FlowHistory` with `pub fn flow(&self) -> &FlowHistory`; `update(…, mode)` pushes in `Flow` mode, decays the history when no fresh levels, and resets it when `mode` differs from the previous update's mode (a mode switch starts a fresh history — spec §1.2).
  - `pub fn draw_flow(buffer: &mut Buffer, area: Rect, history: &FlowHistory, levels: Option<&[f32]>, theme: &Theme)`: per column, resample its history to `area.height` rows (nearest), draw stacked `'█'` from the bottom; color by value through `fn flow_color(value: f32, theme: &Theme) -> Color` — `value < 0.5` linear-RGB-lerp `theme.green`→`theme.amber`, else `theme.amber`→`theme.cream`; zero-value cells draw nothing (silence stays silent).
  - `Visuals` gains `pub flow: Option<&'a FlowHistory>`; `draw_spectrum(buffer, area, levels, peaks, waveform, flow, mode, theme)` dispatches: `Bars` → today's body (extracted as `draw_bars`, byte-identical), `Scope` → `draw_scope`, `Flow` → `draw_flow`; **the minimal-tier rule: when `area.height <= 1`, every mode renders `draw_bars`** (spec §1.2). The three existing in-file `draw_spectrum` unit tests (~line 1186) gain the new arguments with `VisualizerMode::Bars`.

- [ ] **Step 1: Write the failing tests** in `tests/m14_visuals.rs`: `push` twice at width 4 then once at width 5 → the history is empty-then-rebuilt (no column count carry-over); `decay` halves nothing but multiplies by 0.85; the update-level rule: a mode switch Bars→Flow resets; stale levels decay the history. Buffer-level: `draw_flow` over a rising ramp renders bottom rows in greenish and top rows in cream-ish tones (assert via `flow_color`'s endpoints, not pixel hues); an all-zero history draws nothing; **height 1 renders bars, not flow**.
- [ ] **Step 2: Run to verify failure** — `cargo test --locked --test m14_visuals` → FAIL
- [ ] **Step 3: Implement** per the signatures above; extract `draw_bars` and add the dispatch.
- [ ] **Step 4: Run to verify pass** — same command, plus `--test m5_tui_render --test m8_tui` green unmodified.
- [ ] **Step 5: Commit** — `git commit -m "feat(tui): flow renderer with per-column energy history (m14)"`

### Task 9: The `v` key, minimal-tier skip, and startup wiring

**Files:**
- Modify: `src/tui/state.rs` (`UiState`), `src/tui/input.rs` (`no_overlay`, line ~423), `src/tui/mod.rs` (`start_and_loop` after line ~197, `run_loop`'s `update_spectrum` call, `Visuals` construction), `src/tui/render.rs` (`draw`, line ~166 — reads the mode)

**Interfaces:**
- Consumes: Tasks 1–8.
- Produces:
  - `UiState` gains `pub theme: Theme` (default `Theme::default()`), `pub visualizer: VisualizerMode` (default `Bars`), `pub spectrum_rows: u16` (default 0 — no panel seen yet), plus `pub fn with_appearance(mut self, theme: Theme, visualizer: VisualizerMode) -> Self`. Existing `UiState::new` call sites (dozens in tests) compile unchanged.
  - `v` in `no_overlay`: `ui.visualizer = next_visual(ui.visualizer, ui.spectrum_rows); vec![Effect::App(AppCommand::SetVisualizer(ui.visualizer.as_stored().map(str::to_owned)))]` where `fn next_visual(mode, rows) -> VisualizerMode` skips `Flow` when `rows <= 1` (a one-row panel cycles bars → scope → bars; spec §1.2). `u16`→`usize` conversions stay infallible (`usize::from`).
  - `update_spectrum(runtime, terminal, view, ui, display)`: sets `ui.spectrum_rows = regions.spectrum.map_or(0, |r| r.height)`, calls `handle.set_wants_waveform(ui.visualizer == VisualizerMode::Scope)`, and passes `ui.visualizer` into `display.update`.
  - Startup in `start_and_loop`, after the runtime is inserted and before `UiState::new`:
    - `if let Some(theme) = &options.theme { runtime.handle(AppCommand::SetTheme(Some(theme.clone()))); }` — the flag writes through (spec §2.1, §7).
    - Resolve once: `let theme = ThemeId::from_stored(runtime.session().state().theme().unwrap_or("")).unwrap_or_default().theme();` and the visualizer: the `--visualizer` flag first (run-scoped, never dispatched), then the saved raw string, then `unwrap_or_default()`.
    - `let ui = UiState::new(…).with_appearance(theme, visualizer);`
  - `draw()` passes `ui.visualizer` into `draw_spectrum` (the dispatch Task 8 installed).

- [ ] **Step 1: Write the failing tests**
  - `tests/m14_visuals.rs` (input dispatch, the `tests/m5_tui_input.rs` pattern): `v` on a default `UiState` (with `spectrum_rows = 8`) emits `SetVisualizer(Some("scope"))` and the state now reads `Scope`; pressing through scope → flow → bars emits `Some("flow")` then `None`. With `spectrum_rows = 1`: bars → scope → bars (flow skipped). A persisted-mode startup: `with_appearance(theme, VisualizerMode::from_stored("flow").unwrap())` renders flow through `render::draw` on a fixture (Task 8's buffer assertions), and at one row renders bars.
  - `tests/m14_visuals.rs` (resolution precedence, a small pure helper if cleaner): flag beats saved beats default; a saved unknown string (`"plasma"`) loses to the default; a `--theme` flag lands in state (Task 4's harness).
- [ ] **Step 2: Run to verify failure** — `cargo test --locked --test m14_visuals` → FAIL
- [ ] **Step 3: Implement** per the signatures above.
- [ ] **Step 4: Run to verify pass** — same command, plus `--test m5_tui_input --test m5_tui_render --test m8_tui --test m5_runtime` green.
- [ ] **Step 5: Commit** — `git commit -m "feat(tui): the v key, mode persistence and startup resolution (m14)"`

### Task 10: Theme application and the background fill

**Files:**
- Modify: `src/tui/render.rs` (`draw`, line ~166: `let theme = Theme::default();` becomes `let theme = ui.theme;`)

**Interfaces:**
- Consumes: Task 9's `UiState::theme`.
- Produces: the §2.2 fill — the frame's first render step, before the layout is carved:

```rust
let mut base = Style::new().fg(theme.text);
if let Some(bg) = theme.bg {
    base = base.bg(bg);
}
buffer.set_style(area, base);
```

(`set_style` patches styles, so every widget that sets explicit styles draws over it unchanged; artwork is pixels, not styles, and stays theme-blind — spec §2.3.)

- [ ] **Step 1: Write the failing tests** in `tests/m14_themes.rs` (buffer-level, the `tests/m5_tui_render.rs` render helper): `draw` with a `winamp`-themed `UiState` at 100×30 → **every** cell in the area carries `bg == Some` the winamp background (Review Focus 5); with the gruvbox default → every cell `bg == Color::Reset` (today's property, buffer-identical to the pre-M14 render the existing suites pin); a themed render at each of the four tiers' reference sizes (80×28, 79×27, 60×21, 49×17) likewise fills completely — the tier-by-tier walk spec §2.2 requires.
- [ ] **Step 2: Run to verify failure** — `cargo test --locked --test m14_themes` → FAIL
- [ ] **Step 3: Implement** the two-line change above.
- [ ] **Step 4: Run to verify pass** — same command, plus the full `cargo test --locked --no-fail-fast` (gruvbox renders byte-identical: every existing snapshot green unmodified is the evidence spec §7 asks for).
- [ ] **Step 5: Commit** — `git commit -m "feat(tui): themes apply at startup; dark themes fill their background (m14)"`

### Task 11: Documentation and acceptance

**Files:**
- Modify: `docs/reference.md`, `docs/architecture.md`, `CHANGELOG.md`
- Create: `docs/m14-acceptance.md`

- [ ] **Step 1: `docs/reference.md`** — the command-line table's `tui` row gains `[--theme gruvbox|winamp|nord|solarized-dark] [--visualizer bars|scope|flow]`; the options bullets gain `--theme` (validated, persisted, applies at startup) and `--visualizer` (overrides the saved mode for this run); the keys table gains `v` ("Cycle the visual panel: bars → scope → flow; a one-row panel cycles bars → scope"); the spectrum paragraph generalizes to "the visual panel" naming the three modes and the silence rule; a themes paragraph lists the four palettes and states the background property per theme (gruvbox paints none).
- [ ] **Step 2: `docs/architecture.md`** — §5's `tenuto-spectrum` row gains "and a decimated waveform when the view asked for scope mode"; §12's out-of-scope line drops "themes" (keeps "equalizer, plugins") and its spectrum sentence gains "and the M14 visual modes"; §12 gains one decision bullet: built-in themes, no skin files, no live theme-switch key, with spec §2.1/§8's reasons.
- [ ] **Step 3: `CHANGELOG.md`** — `[Unreleased]` → `Added`, hard-wrapped:

```markdown
- Visualizer modes (`v`): the spectrum panel cycles bars → scope → flow,
  persisted across restarts; `--visualizer` overrides the saved mode for one
  run. Scope draws a waveform the spectrum worker now publishes alongside its
  bands; flow keeps the panel's own per-column energy history and falls back
  to bars in a one-row panel.
- Built-in themes (`--theme`): gruvbox (the default, unchanged), winamp,
  nord and solarized-dark. The three dark themes paint their own background;
  the choice persists across restarts.
```

- [ ] **Step 4: `docs/m14-acceptance.md`** — the manual script from spec §7: all three modes on a real file and a live station (scope falls to the midline between bursts, never freezes); `--theme winamp` at 80×28, compact and minimal tiers; gruvbox default renders unchanged; theme and visualizer survive restart; `--visualizer flow --theme winamp` together; an 8/16-color terminal still shows a fully painted winamp panel. Record the run's date and results.
- [ ] **Step 5: Verification** — `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast` and `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` all clean.
- [ ] **Step 6: Commit** — `git commit -m "docs(m14): visuals reference, architecture decision, changelog, acceptance"`

## Self-review notes

- **Spec coverage:** §1.1 modes → Tasks 1, 6, 7, 8, 9; §1.2 rules → Tasks 8, 9 (+ Review Focus 2/3/4); §1.3 persistence → Tasks 1, 3, 4, 9; §2.1 themes → Tasks 2, 5, 9; §2.2 background → Tasks 2, 10; §2.3 artwork → no change, Task 10's buffer test keeps the placeholder path green; §3 verified seams → verified again this session (all line references above); §4 module layout → held (no new top-level modules, no rank changes); §5 tests → distributed per task; §6 docs → Task 11; §7 acceptance → Task 11; §8 out of scope → nothing implements skin files, a live-switch key, MilkDrop, or `play` theming.
- **The one interpretive call:** `--theme` writes through to saved state (spec §2.1 "persisted" + §7 "survive restart"); `--visualizer` does not (§1.3 says "for that run"). Flagged here so a reviewer can reject it cheaply.
- **Type consistency:** `VisualizerMode`/`ThemeId` only in `tui`; persistence and `AppCommand` carry `Option<String>`; `SpectrumFrame.waveform` is `Option<Vec<(f32, f32)>>` end to end; `update(…, mode)` is the single mode input to `SpectrumDisplay`.
