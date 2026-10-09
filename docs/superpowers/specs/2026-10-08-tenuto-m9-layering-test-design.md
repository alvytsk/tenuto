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

`src/main.rs` is the binary, not the library, and is not scanned.

## 3. Mechanism

`tests/m9_layering.rs`, in the style of `tests/m9_4_submission_door.rs`. The checker is a set of plain functions over `(path, text)` pairs, so the self-tests (§3.4) feed it synthetic sources and the tree test feeds it `src/`.

### 3.1 The crate root is the module registry

`src/lib.rs` is not a ranked module and is not scanned for references. It is the list of top-level modules, and the test validates it:
- Each line is blank, `pub mod <name>;` or `mod <name>;` (a declared module), or `pub use <module>::<name>;` (an alias with the rank of `<module>`; today only `queue` → `playlist`).
- Any other line fails as unsupported: an inline `mod x { … }`, a `use`, or any item. The crate root stays a registry, so a module can never hide inside it.
- Every declared module must be in the map, every map entry must be declared, and an alias's target must be a ranked module. An alias is not listed in the map itself; it takes its target's rank. A module missing from the map fails, so a new module is placed deliberately; a map entry with no declaration fails, so the map cannot rot.

A file under `src/` belongs to the first path component below `src/`, without `.rs`. A file whose component is not declared fails as unknown.

### 3.2 Reference forms

For each line of a scanned file, everything from the first `//` is dropped first. Doc links such as ``[`crate::persistence::store`]`` are not imports. Test modules (`#[cfg(test)]`) are scanned like the rest; today they add only downward references.

Handled:
- **`crate::<ident>`** is a reference to top-level `<ident>`.
- **`super::` chains that can reach the crate root.** A file's depth is its number of module path components: `src/resume.rs` and `src/media/mod.rs` are 1, `src/media/tags.rs` is 2, `src/tui/render/browser.rs` is 3. A chain of `k` `super::` segments with `k >= depth`, followed by an identifier that is a declared top-level module, is a reference to that module. So `use super::persistence::…` in `src/resume.rs` is caught. The scanner does not track inline modules: inside `mod tests` the real target is one level shallower, so it may report a reference that does not exist, never miss one that does. The fix for such a false positive is to write the path with `crate::`. A leading `self::` is transparent: `self::super::x` is read as `super::x`. Today every chain with `k >= depth` is `use super::*` or `use super::<local item>` inside a test module, and neither names a top-level module.

Rejected as unsupported (fails, naming the line):
- **`crate::{`**: a grouped import from the crate root. Write one `use crate::<module>::…` per module.
- **`super::{` or `super::*` with `k >= depth`**: what it imports from the crate root cannot be read off the line. The one exception is test code (after the file's first `#[cfg(test)]` line) with exactly `k == depth`: there it sits inside a test module and means the file's own module. A longer chain reaches the crate root or past it and is rejected wherever it appears. Today all five `k == depth` imports (`clock.rs`, `commands.rs`, `app.rs`, `tui/mod.rs`, `playlist/mod.rs`) follow the file's first `#[cfg(test)]`.
- **`super::` inside a `::{` group** (`use super::{super::persistence::…}`, on one line or as rustfmt splits it): its real depth includes the group's prefix, which the chain alone does not show. The scanner keeps a stack of open braces across lines and rejects any `super::` whose innermost open brace followed `::`. Write the path on its own `use` line. No source does this today.
- **`tenuto::`**: the crate naming itself, which only `extern crate self as tenuto` would allow.

`super::<ident>` that reaches the crate root and names something other than a declared module is ignored: the registry (§3.1) keeps the crate root free of anything but modules, so such a path can only be a local item seen from a test module.

Not detected, by construction: whitespace inside a path (`crate :: x`), which `cargo fmt --check` normalizes before CI's tests run; `use crate as c`, which no source uses and rustfmt does not rewrite; and a reference after a `//` inside a string literal on the same line. All three are named in the test's module doc so the gap is visible.

### 3.3 Checking and output

A reference violates when it points up a rank or breaks a named rule. It is excused when `(file, target module)` is on the allowlist.

The test fails when any list is non-empty:
- **Registry errors:** an unsupported `lib.rs` line, an unmapped module, a map entry with no declaration, a file under an undeclared module.
- **Unsupported forms:** one line each, `src/x.rs:LINE: <form>`.
- **Violations:** one line each, `src/x.rs:LINE: from (rank n) -> to (rank m)`, or the named rule it breaks.
- **Stale allowlist entries:** an entry that excused nothing. This is the ratchet: the PR that removes an edge must delete its entry.

### 3.4 Self-tests

Scanning today's tree only proves today's tree is clean. A handful of unit tests in the same file feed the checker synthetic sources and assert it catches each case:
- a downward reference passes; a same-rank reference passes;
- an upward `crate::` reference fails;
- each named rule fails (`playback`, `playlist`, `resume` → `persistence`);
- an escaping `super::persistence` in a depth-1 file fails; `super::` in a depth-2 file does not count;
- `crate::{`, an escaping `super::*` before any `#[cfg(test)]`, and `tenuto::` each fail as unsupported;
- a reference in a `//` comment is ignored;
- a `lib.rs` with an inline `mod x {`, an undeclared map entry, or a declared module missing from the map fails;
- a file under an undeclared module fails;
- an allowlisted violation passes, and an allowlist entry that excuses nothing fails as stale.
- added after the final review: a test module's `super::super::*` or `super::super::{` in a depth-1 file fails; a `super::` inside a `::{` group fails, on one line and split across lines; `self::super::persistence` counts as a reference.

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
- Edges below module granularity, such as which `playback` submodule `application` may reach. The `(file, target)` allowlist is coarse on purpose: an existing exception also excuses a second import along the same edge. It is a temporary cleanup list, not symbol-level tracking.
- Tracking inline modules or braces. The `super::` rule (§3.2) is depth-by-file with a `#[cfg(test)]` cut-off, which errs toward reporting.
