# Tenuto M14: Winamp visuals — visualizer modes and themes

Status: drafted on 2026-11-09 from a feature conversation. It is ready for an implementation plan. The code does not yet match it.

Context:
- Architecture §12 lists "equalizer, themes, plugins" among early out-of-scope decisions. This milestone deliberately revises *themes* (and extends the spectrum — itself the M5 exception — into selectable modes). The equalizer is M15, and stays a separate revision of that line. The revision is one line in §12 at implementation time, recording that a terminal player chose to revisit it, not that the original decision was wrong for its milestone.
- No new audio plumbing: both features render from data that already exists or extends an existing published frame.

## 1. Visualizer modes

### 1.1 Decision

`v` in the main list cycles the visual panel through `bars → scope → flow → bars`, persisted across restarts as an additive field (§1.3). The panel keeps its current geometry rules (it takes the rows the track information leaves; the compact tiers shrink it as today).

| Mode | Data | Rendering |
|---|---|---|
| `bars` | as today | the existing spectrum bars, untouched as the default |
| `scope` | a new decimated waveform in the published frame | one to three rows of block-character oscilloscope: the peak envelope of the most recent FFT input block, drawn left to right, centered on a midline; falls to a flat midline on silence, exactly as bars fall |
| `flow` | bars data + the panel's own history | a color-mapped "lava" field: each column keeps a short energy history (a ring in the widget, no engine state), drawn as stacked colored blocks from the bottom whose hue shifts with energy; decays like the existing peak fall |

`scope` is the only change that reaches the engine: the spectrum worker (`tenuto-spectrum`, 2048-sample Hann FFT, ≤20 fps into a latest-value slot) already owns the PCM block before the FFT. The published frame gains an optional decimated waveform — one peak/min pair per output column of a reference width (pick 96 columns, downsample by peak magnitude in the worker), computed only when any listener asked for scope since its last frame (a cheap atomic flag the widget sets). When nobody wants it, the worker's work is unchanged, so the null-output and low-power paths see no difference.

Layering: the frame extension lives in the existing `playback::spectrum` seam (`TapWriter` → worker → `SpectrumHandle` slot). The widget in `src/tui/spectrum.rs` gains the two renderers. No new threads, no new channels, no change to the callback path.

### 1.2 Rules

- Mode switches are instant and lossless: the latest-value slot means a slow reader shows one stale frame at most, as today.
- `flow`'s history is *widget* state: closing or shrinking the panel drops it, and a mode switch starts a fresh history. The engine never learns about flow.
- The minimal tiers (below 50 columns or 18 rows) keep today's behavior: a one-row spectrum, and `v` cycles only bars/scope (flow needs more than one row to mean anything).
- Silence looks silent in every mode: no idle animation. (Winamp's visualizers dance on silence; tenuto's spectrum already does not, and honest signal is the repo's style.)

### 1.3 Persistence

`visualizer_mode: Option<String>` with `#[serde(default)]` — the M13 additive-field rule (no schema bump; unknown to older builds, defaulted by newer ones). `--visualizer bars|scope|flow` on the `tui` command line overrides the saved value for that run, mirroring `--artwork`.

## 2. Themes

### 2.1 Decision

`src/tui/theme.rs` already holds a nine-field `Theme` (gruvbox palette) consumed by the renderers. The milestone makes it a set:

- `ThemeId`: `gruvbox` (the default, today's palette byte-for-byte), `winamp` (the classic: near-black panels, LED-green text and meters `#33ff33`-family, amber selection, the burn-in fear be damned), `nord`, `solarized-dark`. Four built-ins, compiled in — no user-supplied files in this milestone (§8).
- A `THEMES` table maps `ThemeId` → `Theme`; every color field is specified in the plan for all four (no placeholders at merge time).
- Selection: `--theme <name>` on `tui` (validated at parse time, error names the valid set), persisted as `theme: Option<String>` with `#[serde(default)]` — again the additive rule. The default stays gruvbox, so nothing changes for anyone who never asked.
- No runtime switching key: the theme applies at startup. A live-switch key is listed under out of scope with the reason (every widget style is resolved from `Theme` at render; a live switch is one extra event-hop and a redraw, cheap — but the snapshot TUI tests and the startup-order seam both key on theme-at-startup, and Winamp itself needed a skin reload).

### 2.2 The background question

The player paints no background of its own today (the module doc in `theme.rs` states it as a property). The `winamp` theme wants near-black panels, and gruvbox's warmth depends on the terminal's own background.

Decision: `Theme` gains `bg: Option<Color>` (gruvbox: `None`, unchanged property). When `Some(c)`, the frame's first render step fills the full buffer area with `c` before any widget draws — one `Block`/fill pass in `render()`, before the layout is carved. Widgets that already set explicit per-element styles draw over it unchanged; the verification task in the plan walks every layout tier at desktop and mobile widths and compares against the no-background default to catch anywhere a style silently relies on the terminal background.

The module doc's "paints no background" sentence is rewritten to state the property per theme.

### 2.3 Artwork and scope mode interplay

Cover art, image protocol placement and the placeholder are theme-blind (they are pixels, not styles) — no change. The half-block fallback artwork *is* colored; it keeps its own palette (derived from the image), not the theme's, as today.

## 3. What exists today (verified)

- `src/tui/theme.rs`: `Theme { ink, panel, line, muted, text, cream, green, amber, cyan }`, `Default` = gruvbox, doc states no-background.
- `src/tui/spectrum.rs`: the bars widget with peak fall (`PEAK_FALL_PER_SECOND`); `playback/spectrum/` owns tap and worker (2048 Hann, 20 fps cap, latest-value slot via `SpectrumHandle`).
- `tui` CLI flags parsed in the clap command (`--mouse`, `--artwork`): the pattern for `--theme` and `--visualizer`.
- Additive persistence: `RawState` + `#[serde(default)]` (M13's rule applies verbatim).

## 4. Module layout

- No new top-level modules. `ThemeId`/`THEMES` live in `src/tui/theme.rs`; the spectrum frame extension in `playback/spectrum/`; the two renderers in `src/tui/spectrum.rs`.
- `tests/m9_layering.rs`: no rank changes; `tui` (4) already sits above `playback` (2) and consumes the spectrum handle through the runtime as today.

## 5. Tests

- `tests/m14_visuals.rs` — frame extension: the waveform appears only when requested, decimation bounds (peak/min per column), worker work unchanged when not requested (existing spectrum suites stay green unmodified); mode cycling and persistence roundtrip; minimal-tier restriction.
- `tests/m14_themes.rs` — the four themes' field completeness (a table-driven test that fails if a theme ships with a placeholder), default selection, `--theme` validation and override, persistence roundtrip, the background fill pass (a buffer-level render test: gruvbox buffer untouched; winamp buffer filled).
- TUI snapshots for scope/flow renderings join the existing TUI suite pattern (deterministic fixture frames through the fake spectrum slot — the tests/support harness already fakes engine time; the spectrum slot accepts an injected frame).

## 6. Documentation

- `docs/reference.md`: `tui` flags, the `v` key, the visualizer section (the spectrum paragraph generalizes to "the visual panel"), the theme list and the background property per theme.
- `docs/architecture.md`: §5 `tenuto-spectrum` row gains the optional waveform; §12's out-of-scope line drops "themes" and the spectrum exception sentence gains "and the M14 visual modes"; the theme decision (built-ins, no skin files) joins §12.
- `CHANGELOG.md` at implementation time.

## 7. Acceptance

Manual script, recorded in `docs/m14-acceptance.md`: all three modes on a real file and a live station (scope on live media must fall to the midline between bursts, never freeze); `--theme winamp` renders the charcoal/green look at 80×28, compact and minimal tiers; gruvbox by default renders byte-identical to today (a snapshot diff is the evidence); theme and visualizer survive restart; `--visualizer flow --theme winamp` together.

## 8. Out of scope

- User-supplied theme files (a skin directory, hot reload, per-element overrides): Winamp's skin ecosystem was third-party files; the terminal analog is a themes directory with TOML palettes. Deferred because the built-in table fixes the render seam first — a file format without a stable seam is a compatibility promise the repo cannot make yet.
- A live theme-switch key (§2.1 records why).
- A MilkDrop-style feedback/plasma renderer: honest scope is the history-based `flow` mode; anything framebuffer-shaped (per-cell color ramps with feedback) wants a different render path and a GPU-free perf budget this repo has not measured.
- Theming the `play` status line (it is plain text by design).

## 9. Risks

- The background fill touches every tier's first pass — the plan orders it after the theme table lands and requires the tier-by-tier visual comparison, so a tier that silently relied on the terminal background cannot merge unnoticed.
- The waveform decimation must stay on the spectrum worker's budget (it runs at ≤20 fps already); the plan sizes it (96 columns × min/max over the block = a few thousand comparisons per frame) and tests the unchanged-when-unrequested behavior.
- LED-green on some terminal palettes can be unreadable with non-default background schemes; the `winamp` theme painting its own `bg` (§2.2) is what makes it robust, which is why the background decision precedes the theme table in the plan order.
