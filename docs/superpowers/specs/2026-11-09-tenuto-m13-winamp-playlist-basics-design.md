# Tenuto M13: Winamp playlist basics

Status: drafted on 2026-11-09 from a feature conversation. It is ready for an implementation plan. The code does not yet match it.

Context:
- The five features in this milestone: repeat modes, jump to time, the time-remaining toggle, jump to file, and playlist organize (sort, reverse, randomize, deduplicate, prune dead entries).
- They were chosen as the small-scope tier of a Winamp feature survey; visuals (M14), the equalizer (M15) and a Subsonic source (M16) follow separately.
- Every hook point below was verified against the code on 2026-11-09.

## 1. Decisions

One milestone, five independent features sharing one rule: they change policy and rendering only. No engine state machine changes, no schema version bump, no new threads.

### 1.1 Repeat modes

A global `RepeatMode { Off, One, All }`, default `Off`, toggled with `R` in the main list and shown beside the shuffle mark on the tab strip (`R:1`, `R:*`; nothing shown when off, as with shuffle's absence).

The advance decision moves one notch: `Session` already computes the successor (`session.rs` around line 1009: owner playlist, `neighbor(id, Direction::Down)`, `Advance::Next` or `Advance::EndOfQueue`, consumed by `PlayerRuntime` through `take_advance()`). Repeat is a policy the *Session* applies to that computation:

| Mode | `neighbor` returns | End-of-playback-order behavior |
|---|---|---|
| `Off` | as today | `EndOfQueue`; Space replays the last entry (existing rule) |
| `One` | irrelevant | `Advance::Next(same entry)`, with an explicit **restart intent**: repeat-one of a podcast episode plays from zero, not from its checkpoint. This is the one subtle decision: a plain reload of an episode would resume it (the fresh-load intent in `resume::resume_intent`), so `Advance` gains a variant or flag that maps to a zero-start load |
| `All` | as today | wrap: `neighbor` returning `None` at the bottom yields `Advance::Next(first entry in playback order)` instead of `EndOfQueue`. With shuffle on, the current shuffled order is kept and wrapped; it is *not* reshuffled at the seam. A single-entry playlist with `All` replays itself, which falls out of the rule |

`[` and `]` keep never wrapping (Winamp's prev/next do not either). Repeat all + shuffle therefore replays the same shuffled pass.

Persistence: an additive `repeat_mode: Option<String>` field with `#[serde(default)]` on the state model. The envelope carries no `deny_unknown_fields` (verified: `src/persistence/model.rs` uses `#[serde(default)]` throughout), so schema 4 stays schema 4 — older builds ignore the field, newer builds default it to `Off`. The standing rule "a version bump happens when a reshape would break an older reader" is recorded here as precedent.

MPRIS (M11): `RepeatMode` maps to `LoopStatus` (`Off`→`None`, `One`→`Track`, `All`→`Playlist`), and an MPRIS `LoopStatus` request maps to `AppCommand::SetRepeat`. This is nearly free and completes the desktop story.

### 1.2 Jump to time

`Ctrl-J` opens the typed-input overlay (`InputPurpose::JumpToTime`) in the main list. Accepted grammar: `SS`, `MM:SS`, `HH:MM:SS` with one or two digits per part, an optional fraction (one to three digits) on the last part. Enter submits, Esc cancels, exactly like `a`/`n`/`r` typing. *As implemented:* this relaxes the "each part two digits" wording above — this section's own example, `1:23:00`, has a one-digit hour, and the example wins. An empty submit closes the box silently, like the other typing boxes.

- Empty or malformed input keeps the overlay open and shows the parse error inline (one line, under the input).
- A value past the decoded duration is clamped at the landing, matching how seeks already clamp; a value past a merely *declared* duration (podcast feed claim) is accepted and clamped the same way.
- Guards reuse the existing seek notices verbatim: `Play a track before seeking` when nothing is loaded, `Still loading` while a load is in flight, and the ended-track notice after the last entry.
- Submission is the same `AppCommand::SeekTo` path as the progress-bar click, so coalescing, checkpointing and the display-snap behavior of `KeyRouter` are inherited, not reimplemented.

### 1.3 Time-remaining toggle

`t` in the main list cycles the clock label through `elapsed → −remaining → total`. Rendering sits in one place: the label builder beside `clock()` (`src/tui/render.rs`, around line 804).

- `−remaining` needs a decoder-confirmed duration; with only a declared duration or none, the label falls back to elapsed (the same `DisplayDuration`/`DurationSource` distinction the ratio already makes).
- A click on the time label cycles the mode, mirroring Winamp's click on its time display. The progress bar's click-to-seek behavior is untouched.
- The mode is transient `UiState` state, not persisted; the player always opens on elapsed. Persisting it is listed under out of scope.

### 1.4 Jump to file

`f` in the main list opens a filter overlay over the *viewed* playlist: type to filter, `Up`/`Down` (and `j`/`k`) move within matches, Enter selects and plays, Esc cancels.

- Matching is a case-insensitive substring over "artist — title" where display metadata exists, else the entry's source text (path or URL). Declared durations do not participate.
- Enter moves the viewed playlist's selection to the match and plays it (the `Enter`-on-a-row path, so it also makes that playlist the playing one, exactly as Enter on a row does today).
- The overlay is display-only: it shows the same row text as the list, plus a match count. No editor, no mutability.
- Fuzzy subsequence matching, and filtering across all playlists, are out of scope (recorded in §8 with the reason).

### 1.5 Playlist organize

`S` in the main list opens an organize menu overlay — a short vertical list of operations on the *viewed* playlist, navigated with `Up`/`Down`, run with Enter, closed with Esc:

| Item | Effect |
|---|---|
| Sort by title | ascending, case-insensitive, stable; entries with no title sort by source text |
| Sort by artist | same rule on artist |
| Sort by path | source text ascending |
| Sort by duration | decoded duration if all entries have one, else refused with a notice; missing sorts last |
| Reverse | exact reversal of list order |
| Randomize | Fisher–Yates over list order; no seed persisted |
| Remove duplicates | entries sharing one `MediaId` collapse to the first occurrence — or to the playing entry, so playback is never released (recorded as implemented; see the shared rules below) |
| Remove dead | local paths that no longer exist are removed; remote sources are *not* probed |

Rules shared by every item:

- Each operation is one new `AppCommand`, handled like the existing playlist mutations (the `PlaylistSet` owns the rule, `PersistedState` forwards, the writer snapshots).
- Sorting, reversing and randomizing rewrite list order and **clear shuffle** on that playlist (`Shuffle` is a seed over list order; leaving a stale seed would silently scramble the new order). The `·shfl` suffix disappears; turning shuffle on again reshuffles the new order. This is stated here so it is a decision, not a surprise.
- The playing playlist's cursor follows its entry: after any reordering, the cursor points at the same `QueueEntryId` in its new position, and a playing track keeps playing untouched. Positions are not part of identity; nothing about the position contract is affected.
- Remove dead performs its existence checks on the browse worker (`tenuto-browse`), never inline on the main thread — the layering rule that the TUI must not read directories inline applies to `stat` too. The worker returns the set of dead entry IDs; the removal itself is one command.
- Removing duplicates is computed from identity already in memory; no I/O. *As implemented:* the playing entry is always the occurrence kept when it participates in a duplicate group — an organize operation never releases playback — so when the owned occurrence is not the first, the first goes instead.

## 2. What exists today (verified)

- `Session` owns the end-of-track advance: `session.rs` builds `Advance::Next(id)` / `Advance::EndOfQueue` from `Playlist::neighbor` (no wrap, P6), and `application/runtime.rs` (`take_advance()`, around line 928) loads what it hands back.
- The typed-input overlay pattern (`InputPurpose::AddUrl | NewPlaylist | Rename`) and the confirm overlays live in `src/tui/input.rs` and `src/tui/state.rs`.
- The time label and progress ratio are built in `src/tui/render.rs` around `clock()` (line 804), driven by `PlayerView`'s position and `DisplayDuration`.
- `PlaylistSet` (M9.1) owns order, shuffle (`Shuffle { seed, first }`, splitmix64), caps and cursor semantics; `playlist/mod.rs` documents P6 (no wrap).
- `PlayerView` already exposes what MPRIS needs; the `LoopStatus` plumbing extends `application/mpris/mod.rs`'s snapshot and request mapping.
- Persistence: `RawState` in `src/persistence/model.rs`, additive `#[serde(default)]` fields, schema 4 envelope in `src/persistence/json_file.rs`.

## 3. Module layout

No new top-level modules. New files only where the repo's file-size discipline demands them:

- `RepeatMode` in `src/playlist/mod.rs` (it is playlist policy, the same rank as shuffle; `persistence` already builds on `playlist`).
- Time parsing (`parse_hms`) as a small pure module under `src/tui/` (it renders and validates the same grammar).
- Organize operations as `AppCommand` variants dispatched through the existing runtime → `Session` → `PlaylistSet` path.

The layering test (`tests/m9_layering.rs`) needs no new ranks; any new file declared in `src/lib.rs` keeps its module's existing rank.

## 4. Keys summary (additions to `docs/reference.md` §The terminal player)

| Key | Action |
|---|---|
| `R` | Cycle repeat: off → one → all → off (main list) |
| `t` | Cycle the time label: elapsed → −remaining → total |
| `f` | Open jump-to-file filter over the viewed playlist |
| `Ctrl-J` | Open jump-to-time input |
| `S` | Open the organize menu for the viewed playlist |
| click on time | Cycle the time label |

(`R`, `t`, `f`, `S` are unused in the main list today; `r` rename, `R` browser refresh-all are untouched. The implementer re-verifies against the live dispatch table before wiring.)

## 5. Tests

Suites follow the milestone prefix, reusing `tests/support/` and the virtual clock:

- `tests/m13_repeat.rs` — the Session policy table: off/one/all against a playlist's end; repeat-one of an episode restarts at zero (the M8 fresh-load rule must not leak in); wrap under shuffle keeps the shuffled order; single-entry repeat-all; `[`/`]` still refuse to wrap; MPRIS `LoopStatus` mapping both directions; repeat survives save/load.
- `tests/m13_jump_time.rs` — grammar acceptance table (each form, each rejection), clamping past a decoded duration, the three seek notices, checkpoint after a jump.
- `tests/m13_time_display.rs` — label rendering for the three modes against confirmed/declared/absent durations (pure render tests over `PlayerView` fixtures).
- `tests/m13_jump_to_file.rs` — filter matching rules, Enter plays the match through the same path as Enter on a row, Esc, empty matches.
- `tests/m13_organize.rs` — each operation's order result, cursor-follows-entry after reordering, shuffle cleared on reorder, dedup by `MediaId`, dead-entry pruning through the browse worker (fixture tree), refusal of sort-by-duration with missing durations, cap interplay untouched.
- TUI overlay visuals join the existing TUI suite (`m8_tui` pattern) rather than new snapshot machinery.

## 6. Documentation

- `docs/reference.md`: the keys table, the playlist section (repeat is playback-order policy, so it belongs beside shuffle), and the time-label rule.
- `docs/architecture.md`: §4 `session`'s responsibility line gains "and the repeat policy on advance"; §12 loses nothing (repeat was never out of scope).
- `CHANGELOG.md` `[Unreleased]` entries at implementation time, hard-wrapped.

## 7. Acceptance

Manual script, recorded in `docs/m13-acceptance.md`: a three-track playlist with repeat all wraps; repeat one loops one episode from zero (checkpoint untouched); `Ctrl-J 1:23:00` lands and checkpoints; `t` flips the label; `f` finds a track by fragment; organize sorts, dedups and prunes a playlist with a playing entry, whose playback never hiccups and whose cursor survives.

## 8. Out of scope

- Persisting the time-label mode (one more additive field for near-zero value; revisit only on request).
- Fuzzy/subsequence matching in jump-to-file, and multi-playlist search (Winamp's J is single-list; the media-library scale problem is M16's browser work, not a filter tweak).
- Per-playlist repeat modes (Winamp's repeat is global; per-playlist interacts with the playing/viewed split in ways not worth the state).
- An `undo` for organize operations (the removal commands are destructive; the confirmation policy of `c`/`D` is *not* applied to organize — sorts are reversible by re-sorting, dedup and prune are not, so they follow the `y`-confirmation pattern already used by clear/delete).

## 9. Risks

- The repeat-one restart intent touches `resume::resume_intent`'s seam — the M8 acceptance suites (`m8_resume`, `m8_session_playlists`) are the tripwire; the spec's rule (checkpoint preserved, position starts at zero, protection rules unchanged) is tested in `m13_repeat.rs` before any rendering work.
- `R` conflicts are a documentation trap, not a code one: the browser's `R` (refresh all) is a different surface with its own dispatch; reference.md must say "in the main list".
- Sort-by-duration across declared vs decoded durations invites silent inconsistency; the refusal rule in §1.5 exists for that reason and is tested.
