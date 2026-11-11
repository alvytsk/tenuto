# M14 acceptance: themes, the settings overlay and the bars panel

Scope: what M14 ships after the revision
(`docs/superpowers/specs/2026-11-11-tenuto-m14-settings-and-visuals-rollback-design.md`,
whose Part 3 names this page) — the four built-in themes, the settings
overlay, and the spectrum panel back at bars-only after the scope and flow
modes were removed. Branch `m14-winamp-visuals`, from `main` at `5a7db09`.

## Automated evidence

All four gates ran green on this branch on 2026-11-11, Linux, Rust 1.98.1,
at this revision's final commit (the docs commit this page ships in, after
the rollback and the settings overlay landed):

- `cargo fmt --check` — exit 0.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` —
  exit 0, no warnings.
- `cargo test --locked --no-fail-fast` — exit 0: 1,707 passed, 0 failed,
  5 ignored (the `#[ignore]`d device smoke and the manual-measurement
  snapshots) across 123 test binaries and the doc-tests. No suite failed
  in this run.
- `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` — exit 0.

What the M14 suites pin, so the manual rows below carry only what a
buffer cannot see:

- `tests/m14_visuals.rs` (6 tests) — the theme's raw spelling through the
  session (`set_theme` writing it, `None` clearing the field),
  `SetTheme` landing in state through the runtime; the absent `--theme`
  parsing to `None`; `with_appearance` setting the startup palette and
  keeping the theme id in sync; the flag writing through and the startup
  resolution reading the saved spelling back, an unknown or absent one
  defaulting to gruvbox; the one-row panel rendering bars, as before M14.
- `tests/m14_themes.rs` (7 tests) — the theme's additive persistence
  roundtrip and an unknown stored spelling passing through raw;
  `--theme` naming the valid set on rejection, on the `tui` form and on
  the bare `tenuto` form alike; the background property as buffers: a
  winamp-themed frame fills every cell, the gruvbox default leaves the
  terminal background visible, and every tier fills completely.
- `tests/m14_settings.rs` (7 tests) — `o` opening the overlay with the
  cursor on the session's current theme and dispatching nothing; a cursor
  move re-theming the session at once and dispatching exactly one
  `SetTheme` carrying the persisted spelling (the live preview); the
  clamped edges moving and dispatching nothing; `Esc` and `o` closing
  without dispatching; re-opening after a move landing on the moved-to
  theme; the drawn box listing the four storage spellings in order,
  marking the cursor with `▶` and naming the app version on a muted
  footer; a move live-previewing the themed fill behind the open box at
  the normal and minimal tiers, no cell left unpainted.
- The rollback pins, outside the M14 suites: `tests/cli.rs` rejects
  `--visualizer` as an unknown argument on both command forms, and
  `tests/persistence_model.rs` loads a state file still carrying the old
  `visualizer_mode` key with the key ignored (`schema_version` stays 4).
- Neighbors held green through the revision: the M5 TUI render and input
  suites (whose snapshots pin gruvbox rendering unchanged),
  `m5_spectrum_*` (the worker publishing its pre-M14 frame shape, no
  waveform) and `m9_layering` (no rank changes).

## Manual checks (pending, human)

The writer of this page has no terminal, no audio device and no display.
**Every row below is marked pending**; none is marked passed, and nothing
here should be read as evidence that the themes, the settings overlay and
the bars panel look right on a real screen. A person performs each row and
replaces *pending* with *pass* or *fail*, noting the terminal, its color
support and, for a failure, what was seen.

Build with `cargo build --release --locked` and run `target/release/tenuto
tui …` from a shell. The bars row needs a real output device and a real
file with dynamics (music, not a sine); the theme, settings and color rows
can run with `TENUTO_AUDIO_OUTPUT=null` when no device is at hand. Media
to prepare: one local music file.

| Check | Procedure and pass condition | Result |
|---|---|---|
| Bars-only panel on a real file | Play the file: the panel draws the spectrum as before M14 — equal bars with peak caps over them, falling flat on silence, nothing moving on its own. No other mode is reachable and no key switches the panel | pending |
| Bars at the one-row tier | Shrink below 22 rows: the compact player's one-row panel still draws the bars row (pinned at the buffer level by `a_one_row_panel_renders_bars`; the eye on a real terminal is what is left) | pending |
| Settings: live preview, then restart | `o` opens settings with the cursor on the current theme. Down (or `j`): the whole interface recolors at once — with a dark theme, the background switches with the move; the cursor line carries the `▶` marker. Up (or `k`) moves back the same way; the list never wraps. Quit straight from settings with `Ctrl-C`, start plain `tenuto tui`: the moved-to theme is back | pending |
| Version line | In the open overlay, the muted line under the theme list reads `tenuto <version>`; it equals what `tenuto --version` prints | pending |
| `--theme` on both command forms | `tenuto --theme winamp` and `tenuto tui --theme winamp` both open on winamp, and either choice is saved (a later plain start keeps it). A typo — `--theme dracula` on either form — is refused before the player opens, with the valid set named | pending |
| `--visualizer` and `v` are gone | `tenuto tui --visualizer scope` and bare `tenuto --visualizer scope` both fail as unknown arguments; in the player, pressing `v` does nothing | pending |
| `--theme winamp` at 80×28 | `tenuto tui --theme winamp` in an 80×28 terminal: near-black background behind every pane, LED-green text and meters, amber selection, nothing left in the terminal's own color. Shrink below 28 rows (still ≥22): the compact tier, background still full. Shrink below 50 columns or 18 rows: the minimal tier, background still full. Every cell of every tier is painted | pending |
| gruvbox default unchanged | Plain `tenuto tui`, no flags and no saved theme: the rendering is exactly the pre-M14 look — no background painted, the terminal's own background showing through. The buffer-level claim is pinned by the M5 snapshots and `the_gruvbox_default_leaves_the_terminal_background_visible`; the eye against the old look is what is left | pending |
| 8/16-color terminal | In a terminal without truecolor (8- or 16-color palette), `tenuto tui --theme winamp`: the panel is still fully painted — every cell covered, no unpainted holes; the RGB palette arrives as the terminal's nearest approximations | pending |

A failure stays recorded here even after it is fixed, with the fixing
commit named.
