# M13 acceptance: Winamp playlist basics

Automated evidence (all green on this branch, run 2026-11-09):

- `tests/m13_repeat.rs` — 11 tests: the off/one/all advance table, wrap
  under shuffle (order kept across the seam), single-entry repeat all,
  repeat-one-of-a-completed-episode resolving to a start at zero (`D1`),
  MPRIS `LoopStatus` mapping both ways with the no-op case, the additive
  persistence roundtrip, an older file shape without the field, and an
  unknown spelling defaulting to off.
- `tests/m13_jump_time.rs` — 9 tests: the grammar table (one- and two-digit
  parts, fractions, every rejection), the overlay flow (open, seek, reject
  and correct, Esc, empty submit), Ctrl-J never firing inside another
  overlay, and the release-event guard.
- `tests/m13_time_display.rs` — 9 tests: the three rendered labels against
  a decoded duration, the declared/absent fallbacks, the unloaded entry's
  saved-history label in every mode, the estimate mark staying on elapsed,
  the click-on-time cycle, and the progress bar still seeking.
- `tests/m13_jump_to_file.rs` — 9 tests: matching over artist and title
  case-insensitively, narrowing, Enter playing the match through Enter's
  own path, no-match Enter doing nothing, Esc, arrows clamped to the
  matches, the selection surviving a still-matching filter, and the
  overlay's rendered rows and count.
- `tests/m13_organize.rs` — 13 tests: sorts by title/artist/path with the
  untagged source-text fallback, the duration refusal, the no-op sort
  keeping shuffle while a changing sort clears it, reverse twice, randomize
  determinism per seed and the single-entry no-op, the cursor following its
  entry through a sort with the advance from the new position, dedup by
  identity and the owned-entry exception, prune-dead through the real
  browse worker against a fixture tree, the no-local-entries refusal, and
  the menu's key flow with both confirmations.
- Neighbors held green through development: `m5_tui_input`,
  `m7_submission`, `m8_resume`, `m8_session_playlists`, `m8_roundtrip`,
  `persistence_model` (whose byte-exact serialization pin gained the
  additive `repeat_mode` field).

Manual checks for the terminal surfaces are pending, as in M5–M12: drive
`TENUTO_AUDIO_OUTPUT=null cargo run --locked -- tui` through the §7
acceptance script — repeat all wrapping a shuffled three-track playlist,
repeat one of an episode restarting from zero with its checkpoint intact,
`Ctrl-J 1:23:00` landing and checkpointing, `t` and the click cycling the
label, `f` finding a track by fragment, and an organize pass over a
playlist with a playing entry (no hiccup, cursor survives, shuffle badge
clears).

Known decisions recorded as implemented (in the spec): the jump-to-time
grammar takes one or two digits per part (the spec's own example has a
one-digit hour); dedup keeps the playing occurrence over an earlier one so
playback is never released by an organize operation.
