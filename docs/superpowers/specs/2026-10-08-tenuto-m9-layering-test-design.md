# Tenuto M9: layering test

Status: approved in conversation on 2026-10-08. It is ready for an implementation plan.

Branch: `refactor/m9-layering-test`, from `main` at `0e749c0`.

Context: the roadmap is [`2026-09-29-tenuto-m9-architecture-deepening.md`](2026-09-29-tenuto-m9-architecture-deepening.md). This is the first of five PRs that finish M9, in this order:

| PR | Work |
|---|---|
| A | This layering test, with an allowlist of today's exceptions |
| B | Foundation cleanup: `provenance`, `checkpoint`, `volume` out of `playback`; `media` stops calling `playback::decode` and returning `PlaybackError`; `AppError` to `app.rs`; `redact_url` below `media`; `ArtworkMode` and `MouseMode` out of `cli` |
| C | M9.3 feed operations below `commands`; `displayable` and the store constructors move down |
| D | M9.1 versioned JSON file module and resume intent in `resume.rs` |
| E | M9.4 transport owns park and event outbox, as a spike that writes a spec or drops the items with a reason |

Each later PR deletes its own allowlist lines. Nothing in this PR changes runtime code or user-visible behavior.

## 1. Goal

The layering rules in `architecture.md` §4 and `CLAUDE.md` are enforced only by review. A test makes an upward import fail CI, and makes the list of known exceptions a fact the code checks rather than a paragraph that drifts.

## 2. Layer map

Each top-level module of the library crate (`src/lib.rs`) has a rank. A module may reference its own rank or lower.

| Rank | Layer | Modules |
|---|---|---|
| 0 | Foundation | `clock`, `telemetry`, `error`, `lifecycle`, `media`, `persistence`, `playlist` (and its re-export `queue`), `resume` |
| 1 | Sources | `http`, `feed`, `subscription`, `station` |
| 2 | Engine | `playback` |
| 3 | Application | `application`, `session`, `library`, `artwork` |
| 4 | Front end | `tui` |
| 5 | Entry | `app`, `cli`, `commands` |

`playlist` and `resume` sit in Foundation, not Application as the §4 diagram drew them: `persistence` stores the `PlaylistSet`, and `playback` takes a `ResumeCandidate`. The diagram moves to match.

Rank cannot express three rules inside a layer, so they are named:
- `playback` never references `persistence`.
- `playlist` never references `persistence`.
- `resume` never references `persistence`.

A top-level module missing from the map fails the test, so a new module must be placed deliberately.

`src/main.rs` is the binary, not the library, and is not scanned.

## 3. Mechanism

`tests/m9_layering.rs`, in the style of `tests/m9_4_submission_door.rs`:

1. Walk `src/` recursively, skipping `main.rs`. A file's module is the first path component under `src/`, without `.rs`.
2. For each line, drop everything from the first `//`. Doc links such as ``[`crate::persistence::store`]`` are not imports.
3. Collect every `crate::<ident>` reference. No source uses a grouped `use crate::{…}`; if one appears, it fails as an unmapped module (`{` is not an identifier), which is the safe direction.
4. Test modules (`#[cfg(test)]`) are scanned like the rest. Today they add only downward references.
5. A reference violates when it points up a rank or breaks a named rule. It is excused when `(file, target module)` is on the allowlist.

The test fails when either list is non-empty:
- **Violations:** one line each, `src/x.rs:LINE: from (rank n) -> to (rank m)`, or the named rule it breaks.
- **Stale allowlist entries:** an entry that excused nothing. This is the ratchet: the PR that removes an edge must delete its entry.

## 4. Allowlist today

Each entry carries a comment naming the PR that removes it.

| File | Target | Removed by |
|---|---|---|
| `src/media/metadata.rs` | `playback` | B |
| `src/media/tags.rs` | `playback` | B |
| `src/media/vbr_header.rs` | `playback` | B |
| `src/persistence/model.rs` | `playback` | B |
| `src/persistence/queue_codec.rs` | `playback` | B |
| `src/playlist/queue.rs` | `playback` | B |
| `src/resume.rs` | `playback` | B |
| `src/error.rs` | `feed` | B |
| `src/error.rs` | `playback` | B |
| `src/media/display.rs` | `http` | B |
| `src/tui/mod.rs` | `cli` | B |
| `src/tui/images.rs` | `cli` | B |
| `src/application/browse.rs` | `commands` | C |
| `src/application/runtime.rs` | `commands` | C |
| `src/application/view.rs` | `commands` | C |
| `src/lifecycle/panic.rs` | `commands` | C |
| `src/tui/mod.rs` | `commands` | C |
| `src/tui/render/browser.rs` | `commands` | C |

## 5. Documentation

- `architecture.md` §4: move `playlist` and `resume` into Foundation in the diagram. The "Known layering exceptions" paragraph names `tests/m9_layering.rs` as the authority and keeps its prose summary. §12's M9 line mentions the test.
- `CLAUDE.md`, "Rules reviewers enforce": the layering bullet names the test.
- No CHANGELOG entry: nothing changes for users, as with the earlier M9 refactor PRs.

## 6. Out of scope

- Content rules such as "`library` never prints or calls `block_on`". They are a different check and can join this file later.
- Fixing any allowlisted edge. That is B's and C's work.
- Edges below module granularity, such as which `playback` submodule `application` may reach.
