# Tenuto M13: Winamp playlist basics — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `R` repeats (off/one/all), `Ctrl-J` jumps to a typed time, `t` flips the clock to −remaining/total, `f` jumps to a filtered track, and `S` organizes the viewed playlist (sort/reverse/randomize/dedup/prune).

**Spec:** [`../specs/2026-11-09-tenuto-m13-winamp-playlist-basics-design.md`](../specs/2026-11-09-tenuto-m13-winamp-playlist-basics-design.md) — read it first; every rule here defers to it.

**Architecture:**
- Policy before pixels: each feature lands its model/rules first, then its key, then its docs.
- Repeat lives in `Session`'s advance computation; everything else is TUI + `AppCommand` plumbing.
- Persistence is additive only (`#[serde(default)]`, schema stays 4).

**Branch:** `feature/m13-winamp-basics`, from `main`.

---

## Task 1: Repeat modes

- [ ] Add `RepeatMode` to `src/playlist/mod.rs` (`Off | One | All`, `Default = Off`) and an additive `repeat_mode: Option<String>` field on the state model; roundtrip test in the persistence suite.
- [ ] Thread the mode into `Session` (a setter beside the other session-owned settings) and implement the policy table at the advance site (`session.rs` ~1009): `All` wraps to the first entry of playback order; `One` yields `Advance::Next(same id)` with a restart intent; `Off` unchanged.
- [ ] Map the restart intent through `resume::resume_intent` so repeat-one of an episode starts at zero while the checkpoint is untouched; add the regression test before rendering work.
- [ ] `AppCommand::SetRepeat`; wire `R` in `src/tui/input.rs`; render the `R:1`/`R:*` badge beside the shuffle mark; map `RepeatMode` ↔ MPRIS `LoopStatus` in `application/mpris/mod.rs` and accept a `LoopStatus` request.
- [ ] Write `tests/m13_repeat.rs` per the spec's §5 list. Run: `cargo test --locked --test m13_repeat --test m8_resume --test m8_session_playlists --test m7_submission`.

## Task 2: Jump to time

- [ ] Pure parser `parse_hms` under `src/tui/` with the spec's strict grammar; unit-test the acceptance table (each form, each rejection).
- [ ] `InputPurpose::JumpToTime`; `Ctrl-J` opens it (Ctrl chords never fire plain shortcuts — verify the guard exists on this path); inline parse-error line; submit → `AppCommand::SeekTo`; guards reuse the three existing seek notices verbatim.
- [ ] `tests/m13_jump_time.rs` (grammar, clamp past decoded duration, notices, checkpoint after jump).

## Task 3: Time-remaining toggle

- [ ] Clock mode enum on `UiState` (`Elapsed | Remaining | Total`); `t` cycles; click on the time label cycles (mouse routing beside the progress-bar click); label builder at `render.rs` `clock()` implements the fallback rules (remaining needs a decoded duration).
- [ ] `tests/m13_time_display.rs` over `PlayerView` fixtures (confirmed/declared/absent durations).

## Task 4: Jump to file

- [ ] Filter overlay (`f`): case-insensitive substring over "artist — title" else source text; Up/Down/j/k within matches; Enter selects and plays through the existing Enter-on-a-row path; Esc cancels; match count shown.
- [ ] `tests/m13_jump_to_file.rs`.

## Task 5: Organize menu

- [ ] `AppCommand` variants for the eight operations; implement the list-order rewrites in `PlaylistSet` (cursor-follows-entry; shuffle cleared on reorder — spec §1.5); dedup by `MediaId`; prune-dead computed on `tenuto-browse` (existence checks off the main thread); dedup and prune take the `y` confirmation, sorts run at once.
- [ ] `S` opens the menu overlay; refusals (sort-by-duration with missing durations) surface as notices.
- [ ] `tests/m13_organize.rs` per spec §5 (including the browse-worker prune with a fixture tree).

## Task 6: Docs and gate

- [ ] `docs/reference.md` (keys table §4 of the spec, playlist section, time-label rule); `docs/architecture.md` §4 session line; `CHANGELOG.md` `[Unreleased]`; acceptance script run and recorded in `docs/m13-acceptance.md`.
- [ ] Full gate: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast && RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`.
