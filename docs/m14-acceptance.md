# M14 acceptance: Winamp visuals

Scope: visualizer modes and built-in themes
(`docs/superpowers/specs/2026-11-09-tenuto-m14-winamp-visuals-design.md` §7).
Branch `m14-winamp-visuals`, from `main` at `5a7db09`.

## Automated evidence

All four gates ran green on this branch on 2026-11-11, Linux, Rust 1.98.1,
code as of `2234a09` — the commit that records this page changes
documentation only:

- `cargo fmt --check` — exit 0.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` —
  exit 0, no warnings.
- `cargo test --locked --no-fail-fast` — exit 0: 1,731 passed, 0 failed,
  5 ignored (the `#[ignore]`d device smoke and the manual-measurement
  snapshots) across 123 test binaries, doc-tests included. The first full
  run that day saw `http_cancellation` fail under parallel load; the test
  passes in isolation and the whole suite passed on the rerun — the
  known load-flake class the repo's build notes describe, in a suite no
  M14 commit touches.
- `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` — exit 0.

What the M14 suites pin, so the manual rows below carry only what a
buffer cannot see:

- `tests/m14_visuals.rs` (39 tests) — the mode's additive persistence
  roundtrip and an unknown stored spelling passing through raw;
  `SetVisualizer` writing and clearing the stored spelling, `SetTheme`
  landing in state through the runtime; `--visualizer` and `--theme`
  parsing their stored spellings and rejecting others with the valid set
  named, absent flags parsing to none; waveform decimation bounds
  (constant block, spike placement, short and empty blocks zero-filled,
  non-finite samples as silence, out-of-range clamped) and the publish
  gate (a frame carries its waveform only when a listener asked);
  the widget's scope gate (a fresh matching waveform taken, a stale one
  decaying both drawn entries, bars mode never taking the waveform, a
  foreign revision or token refused); scope rendering (a spike as green
  blocks about the midline, silence as the muted midline alone and
  nothing at one row); flow history (fresh frames pushing newest last,
  a width change rebuilding, a mode switch resetting, stale frames
  cooling instead of pushing, the ring capped, a push without levels
  pushing nothing) and flow rendering (the rising ramp stacking green at
  the bottom and cream at the top, sixteen entries resampling into three
  rows, an all-zero history drawing nothing, an empty history falling
  back to the live levels); the `v` key cycling and dispatching each
  spelling, a one-row panel skipping flow, startup appearance from
  `with_appearance`, a restored flow mode rendering flow through the
  draw path and rendering bars at one row; the flag beating the saved
  mode and the saved beating the default, an unknown saved spelling
  losing to the default, the theme flag writing through and startup
  resolution reading it back.
- `tests/m14_themes.rs` (6 tests) — the theme's additive persistence
  roundtrip, an unknown spelling passing through raw, `--theme` naming
  the valid set on rejection; the background property as buffers: a
  winamp-themed frame fills every cell, the gruvbox default leaves the
  terminal background visible, and every tier fills completely.
- Neighbors held green through the milestone: the M5 TUI render and
  input suites (whose snapshots pin gruvbox rendering unchanged),
  `m5_spectrum_*` (whose worker and tap contracts the waveform extension
  had to keep), `m9_layering` (no rank changes) and the whole suite above.

## Manual checks (pending, human)

The writer of this page has no terminal, no audio device and no display.
**Every row below is marked pending**; none is marked passed, and nothing
here should be read as evidence that the modes and themes look right on a
real screen. A person performs each row and replaces *pending* with
*pass* or *fail*, noting the terminal, its color support and, for a
failure, what was seen.

Build with `cargo build --release --locked` and run `target/release/tenuto
tui …` from a shell. The audio rows need a real output device and a real
file with dynamics (music, not a sine); the theme, tier, persistence and
color rows can run with `TENUTO_AUDIO_OUTPUT=null` when no device is at
hand. Media to prepare: one local music file and one live radio station
in the playlists.

| Check | Procedure and pass condition | Result |
|---|---|---|
| All three modes on a real file | Play the file. `v` once: `scope` draws a connected block-character trace about the midline, moving with the music. `v` again: `flow` stacks colored blocks from the bottom, green through amber to cream as a column heats. `v` once more returns to `bars`, unchanged from before M14. Pause: the flow drains away and the scope falls flat — silence looks silent in every mode, no idle animation | pending |
| All three modes on a live station | Play the station, `v` to `scope`. Between musical bursts the scope falls to the flat midline and picks back up; it never freezes on a stale frame. Then `bars` and `flow` on the same station | pending |
| `--theme winamp` at 80×28 | `tenuto tui --theme winamp` in an 80×28 terminal: near-black background behind every pane, LED-green text and meters, amber selection, nothing left in the terminal's own color. Shrink below 28 rows (still ≥22): the compact tier, smaller cover and three- or four-row panel, background still full. Shrink below 50 columns or 18 rows: the minimal tier, no cover and no panel, background still full. Every cell of every tier is painted | pending |
| gruvbox default unchanged | Plain `tenuto tui`, no flags and no saved theme: the rendering is exactly the pre-M14 look — no background painted, the terminal's own background showing through. The buffer-level claim is pinned by the M5 snapshots and `the_gruvbox_default_leaves_the_terminal_background_visible`; the eye against the old look is what is left | pending |
| Theme and visualizer survive restart | `--theme winamp`, `v` to `flow`, quit with `q`, start plain `tenuto tui`: winamp and flow are both back. Start once more with `--visualizer scope`, quit, start plain: the saved flow mode returns — the flag held for its run only | pending |
| `--visualizer flow --theme winamp` together | One run with both flags: flow's green-amber-cream stacks on the winamp near-black background, panel fully painted | pending |
| 8/16-color terminal | In a terminal without truecolor (8- or 16-color palette), `tenuto tui --theme winamp`: the panel is still fully painted — every cell covered, no unpainted holes; the RGB palette arrives as the terminal's nearest approximations | pending |

A failure stays recorded here even after it is fixed, with the fixing
commit named.
