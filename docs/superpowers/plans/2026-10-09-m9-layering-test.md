# M9 Layering Test Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One integration test, `tests/m9_layering.rs`, that fails CI on any import that climbs the layers of `docs/architecture.md` §4, with an allowlist of today's 18 exceptions that can only shrink.

**Architecture:** A pure checker, `check(lib, files, layers, rules, allowed) -> Vec<String>`, reads `src/lib.rs` as the module registry and scans `(path, text)` pairs for `crate::` and root-reaching `super::` references. Self-tests feed it synthetic sources; one tree test feeds it `src/`. No runtime code changes.

**Tech Stack:** Rust 1.98.1, edition 2024, std only (no regex; the crate has no such dev-dependency).

**Spec:** `docs/superpowers/specs/2026-10-08-tenuto-m9-layering-test-design.md`. The code below was compiled and run against `main` at `0e749c0` while writing this plan: 12 tests pass, clippy (`-D warnings`, `unwrap_used`/`expect_used` denied with `clippy.toml`) and rustfmt are clean, and deleting the `("src/resume.rs", "playback")` entry makes the tree test fail with `src/resume.rs:20: resume (rank 0) -> playback (rank 2)`.

## Global Constraints

- Every cargo command uses `--locked`; toolchain 1.98.1.
- Tests may use `unwrap`/`expect` inside `#[test]` functions (`clippy.toml`); a bare helper needs `#[allow(clippy::unwrap_used)]` with a reason, as in `tests/m9_4_submission_door.rs`.
- No runtime code changes in this PR. No CHANGELOG entry (nothing changes for users).
- Commits carry no `Co-authored-by` or tool attribution.
- Branch `refactor/m9-layering-test` already exists with the spec commits; work on it.

## Review Focus

1. **A contributor adds a top-level module** (`pub mod x;` in `lib.rs`). Expected: the test fails naming the fix, "module `x` has no rank in LAYERS". *Pinned in Task 1:* `the_crate_root_is_a_registry`.
2. **B or C removes an edge but leaves its allowlist line.** Expected: "stale allowlist entry (…): it excuses nothing, delete it". *Pinned in Task 1:* `an_allowlisted_violation_passes_and_an_unused_entry_is_stale`; *demonstrated on the real tree in Task 2, Step 5.*
3. **A rewrite to `super::` or a grouped import that would hide an edge.** Expected: counted or rejected, never silent. *Pinned in Task 1:* `a_super_path_counts_once_it_reaches_the_crate_root`, `unsupported_path_forms_fail`.
4. **Test modules that `use super::*` at depth 1** (`clock.rs`, `commands.rs`, `app.rs`, `tui/mod.rs`, `playlist/mod.rs` today). Expected: no error. *Pinned in Task 1:* `a_root_glob_after_cfg_test_is_the_test_module_s_own`; *on the real tree in Task 2.*
5. **The macOS CI leg.** Paths are normalized to `/` and the file list is sorted, so output is stable across platforms. No separate test; the tree test runs on both legs.

Accepted by the spec, not tested: a `//` inside a string literal hides the rest of its line; a production `super::*` placed after a mid-file `#[cfg(test)]` item is treated as test code. Both are named in the test's module doc.

---

### Task 1: The checker and its self-tests

**Files:**
- Create: `tests/m9_layering.rs`

**Interfaces:**
- Produces (used by Task 2):
  - `const NAMED_RULES: &[(&str, &str)]`
  - `fn check(lib: &str, files: &[(String, String)], layers: &[(&str, u8)], rules: &[(&str, &str)], allowed: &[(&str, &str)]) -> Vec<String>` — every error, in order: registry errors, then per file (unsupported forms, then violations), then stale allowlist entries. Empty means clean.
  - Error strings the self-tests and Task 2 rely on: `"{path}:{line}: {from} (rank {n}) -> {to} (rank {m})"`, `"{path}:{line}: {from} -> {to}: {from} never references {to}"`, `"{path}:{line}: unsupported path form: {form}"`, `"stale allowlist entry {entry:?}: it excuses nothing, delete it"`.

- [ ] **Step 1: Write the self-tests against a stub checker**

Create `tests/m9_layering.rs` with exactly this content:

```rust
//! M9: the crate's layering (`docs/architecture.md` §4), checked.
//!
//! Every top-level module has a rank in [`LAYERS`]. A module may reference
//! its own rank or lower, and three rules inside a layer are named in
//! [`NAMED_RULES`]. [`ALLOWED`] lists today's known exceptions; each names
//! the PR that removes it, and an entry that excuses nothing fails as stale.
//!
//! `src/lib.rs` is the module registry: only `mod`, `pub mod` and
//! `pub use <module>::<name>;` (an alias with that module's rank) may appear
//! there. `src/main.rs` is the binary and is not scanned.
//!
//! What counts as a reference, after dropping everything from the first `//`:
//! - `crate::<module>`;
//! - a `super::` chain at least as long as the file's module depth, followed
//!   by a declared module name. The depth comes from the file path, not from
//!   inline modules, so inside `mod tests` this can report a reference that
//!   is not real (write it with `crate::`), never miss one that is.
//!
//! Rejected as unsupported: `crate::{`, `tenuto::`, and a `super::*` or
//! `super::{` that can reach the crate root before the file's first
//! `#[cfg(test)]`.
//!
//! Not detected: whitespace inside a path (`crate :: x`, which `cargo fmt`
//! removes), `use crate as c`, and a reference after a `//` inside a string
//! on the same line.

use std::collections::{BTreeMap, BTreeSet};

/// `(from, to)`: `from` never references `to`, whatever their ranks.
const NAMED_RULES: &[(&str, &str)] = &[
    ("playback", "persistence"),
    ("playlist", "persistence"),
    ("resume", "persistence"),
];

/// Every layering error in `files` (`(path, text)`, paths like
/// `src/media/tags.rs`), given the crate root `lib`.
fn check(
    _lib: &str,
    _files: &[(String, String)],
    _layers: &[(&str, u8)],
    _rules: &[(&str, &str)],
    _allowed: &[(&str, &str)],
) -> Vec<String> {
    Vec::new()
}

// The checker itself, on synthetic sources.

const TEST_LIB: &str = "pub mod clock;
pub mod persistence;
pub mod playlist;
pub use playlist::queue;
pub mod resume;
pub mod playback;
pub mod application;
";

const TEST_LAYERS: &[(&str, u8)] = &[
    ("clock", 0),
    ("persistence", 0),
    ("playlist", 0),
    ("resume", 0),
    ("playback", 2),
    ("application", 3),
];

fn errors_with(files: &[(&str, &str)], allowed: &[(&str, &str)]) -> Vec<String> {
    let files: Vec<(String, String)> = files
        .iter()
        .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
        .collect();
    check(TEST_LIB, &files, TEST_LAYERS, NAMED_RULES, allowed)
}

fn errors_for(files: &[(&str, &str)]) -> Vec<String> {
    errors_with(files, &[])
}

fn assert_one_error(errors: &[String], needle: &str) {
    assert!(
        errors.len() == 1 && errors[0].contains(needle),
        "expected one error containing {needle:?}, got {errors:#?}"
    );
}

#[test]
fn downward_same_rank_and_alias_references_pass() {
    let errors = errors_for(&[
        (
            "src/application/mod.rs",
            "use crate::playback::Engine;\nuse crate::clock::Clock;",
        ),
        (
            "src/persistence/model.rs",
            "use crate::playlist::Playlist;\nuse crate::queue::Queue;",
        ),
    ]);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn an_upward_reference_fails() {
    let errors = errors_for(&[("src/playback/engine.rs", "use crate::application::Runtime;")]);
    assert_one_error(
        &errors,
        "src/playback/engine.rs:1: playback (rank 2) -> application (rank 3)",
    );
}

#[test]
fn each_named_rule_fails() {
    for path in [
        "src/playback/mod.rs",
        "src/playlist/mod.rs",
        "src/resume.rs",
    ] {
        let errors = errors_for(&[(path, "use crate::persistence::model::State;")]);
        assert_one_error(&errors, "never references persistence");
    }
}

#[test]
fn a_super_path_counts_once_it_reaches_the_crate_root() {
    let errors = errors_for(&[("src/resume.rs", "use super::persistence::model::State;")]);
    assert_one_error(&errors, "src/resume.rs:1: resume -> persistence");
    let errors = errors_for(&[(
        "src/playlist/queue.rs",
        "use super::super::persistence::model::State;",
    )]);
    assert_one_error(&errors, "src/playlist/queue.rs:1: playlist -> persistence");
    let errors = errors_for(&[("src/playlist/queue.rs", "use super::persistence::Local;")]);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn unsupported_path_forms_fail() {
    let errors = errors_for(&[("src/application/mod.rs", "use crate::{clock, playback};")]);
    assert_one_error(&errors, "unsupported path form: crate::{");
    let errors = errors_for(&[("src/application/mod.rs", "use tenuto::clock::Clock;")]);
    assert_one_error(&errors, "unsupported path form: tenuto::");
    let errors = errors_for(&[("src/resume.rs", "use super::*;")]);
    assert_one_error(&errors, "unsupported path form: super::*");
    let errors = errors_for(&[("src/resume.rs", "use super::{persistence, clock};")]);
    assert_one_error(&errors, "unsupported path form: super::*");
}

#[test]
fn a_root_glob_after_cfg_test_is_the_test_module_s_own() {
    let errors = errors_for(&[(
        "src/resume.rs",
        "fn decide() {}\n#[cfg(test)]\nmod tests {\n    use super::*;\n    use super::decide;\n}",
    )]);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn references_in_comments_are_ignored() {
    let errors = errors_for(&[(
        "src/playback/mod.rs",
        "/// See [`crate::application::Runtime`].\nlet a = 1; // crate::application",
    )]);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn an_undeclared_reference_target_fails() {
    let errors = errors_for(&[("src/application/mod.rs", "use crate::nowhere::Thing;")]);
    assert_one_error(&errors, "`crate::nowhere` is not a declared module");
}

#[test]
fn the_crate_root_is_a_registry() {
    let errors = check(
        "pub mod clock;\nmod inner {}\n",
        &[],
        &[("clock", 0)],
        &[],
        &[],
    );
    assert_one_error(&errors, "src/lib.rs:2: the crate root holds only");
    let errors = check(
        "pub mod clock;\n",
        &[],
        &[("clock", 0), ("http", 1)],
        &[],
        &[],
    );
    assert_one_error(&errors, "LAYERS: `http` is not declared");
    let errors = check(
        "pub mod clock;\npub mod extra;\n",
        &[],
        &[("clock", 0)],
        &[],
        &[],
    );
    assert_one_error(&errors, "module `extra` has no rank");
    let errors = check(
        "pub mod clock;\npub use nowhere::thing;\n",
        &[],
        &[("clock", 0)],
        &[],
        &[],
    );
    assert_one_error(&errors, "alias `thing` points at `nowhere`");
}

#[test]
fn a_file_under_an_undeclared_module_fails() {
    let errors = errors_for(&[("src/stray/mod.rs", "")]);
    assert_one_error(&errors, "src/stray/mod.rs: belongs to `stray`");
}

#[test]
fn an_allowlisted_violation_passes_and_an_unused_entry_is_stale() {
    let upward = [("src/playback/engine.rs", "use crate::application::Runtime;")];
    let entry = [("src/playback/engine.rs", "application")];
    assert!(errors_with(&upward, &entry).is_empty());
    assert_one_error(&errors_with(&[], &entry), "stale allowlist entry");
}
```

- [ ] **Step 2: Run the self-tests and watch them fail**

Run: `cargo test --locked --test m9_layering`
Expected: compiles (an unused-import warning for `BTreeMap`/`BTreeSet` is fine at this step). FAIL: `an_upward_reference_fails`, `each_named_rule_fails`, `a_super_path_counts_once_it_reaches_the_crate_root`, `unsupported_path_forms_fail`, `an_undeclared_reference_target_fails`, `the_crate_root_is_a_registry`, `a_file_under_an_undeclared_module_fails`, `an_allowlisted_violation_passes_and_an_unused_entry_is_stale` (each with "expected one error containing …, got []"). PASS: the three that expect no errors.

- [ ] **Step 3: Implement the checker**

Replace the stub `check` (the whole `/// Every layering error …` doc comment and function) with the following. The helpers go between `NAMED_RULES` and `check`.

```rust
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_ident(text: &str) -> bool {
    !text.is_empty() && text.chars().all(is_ident_char)
}

fn ident_prefix(text: &str) -> &str {
    let end = text.find(|c| !is_ident_char(c)).unwrap_or(text.len());
    &text[..end]
}

fn code_of(line: &str) -> &str {
    line.split("//").next().unwrap_or_default()
}

/// Byte offsets where `word` starts a path: not preceded by an identifier
/// character or a `:`.
fn path_starts(code: &str, word: &str) -> Vec<usize> {
    code.match_indices(word)
        .map(|(at, _)| at)
        .filter(|&at| {
            !code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| is_ident_char(c) || c == ':')
        })
        .collect()
}

/// Declared modules and aliases, each mapped to its canonical module.
fn read_registry(
    lib: &str,
    layers: &[(&str, u8)],
    errors: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let mut declared = BTreeSet::new();
    let mut aliases = Vec::new();
    for (number, line) in lib.lines().enumerate() {
        let code = code_of(line).trim();
        if code.is_empty() {
            continue;
        }
        let module = code
            .strip_prefix("pub mod ")
            .or_else(|| code.strip_prefix("mod "))
            .and_then(|rest| rest.strip_suffix(';'))
            .filter(|name| is_ident(name));
        let alias = code
            .strip_prefix("pub use ")
            .and_then(|rest| rest.strip_suffix(';'))
            .and_then(|path| path.split_once("::"))
            .filter(|(target, name)| is_ident(target) && is_ident(name));
        if let Some(name) = module {
            declared.insert(name.to_owned());
        } else if let Some((target, name)) = alias {
            aliases.push((number + 1, name.to_owned(), target.to_owned()));
        } else {
            errors.push(format!(
                "src/lib.rs:{}: the crate root holds only module declarations and aliases: {code}",
                number + 1
            ));
        }
    }
    let mapped: BTreeSet<&str> = layers.iter().map(|(name, _)| *name).collect();
    for name in &declared {
        if !mapped.contains(name.as_str()) {
            errors.push(format!("src/lib.rs: module `{name}` has no rank in LAYERS"));
        }
    }
    for name in &mapped {
        if !declared.contains(*name) {
            errors.push(format!("LAYERS: `{name}` is not declared in src/lib.rs"));
        }
    }
    let mut canonical: BTreeMap<String, String> = declared
        .iter()
        .filter(|name| mapped.contains(name.as_str()))
        .map(|name| (name.clone(), name.clone()))
        .collect();
    for (number, name, target) in aliases {
        if canonical.contains_key(&target) {
            canonical.insert(name, target);
        } else {
            errors.push(format!(
                "src/lib.rs:{number}: alias `{name}` points at `{target}`, which is not a ranked module"
            ));
        }
    }
    canonical
}

/// The top-level module a file belongs to, and its module depth.
fn module_of(path: &str) -> (&str, usize) {
    let parts: Vec<&str> = path
        .strip_prefix("src/")
        .unwrap_or(path)
        .split('/')
        .collect();
    let top = parts[0].strip_suffix(".rs").unwrap_or(parts[0]);
    let depth = if parts.last() == Some(&"mod.rs") {
        parts.len() - 1
    } else {
        parts.len()
    };
    (top, depth)
}

/// One file's references and unsupported path forms, by line number.
struct Scan {
    references: Vec<(usize, String)>,
    unsupported: Vec<(usize, &'static str)>,
}

fn scan(text: &str, depth: usize, modules: &BTreeMap<String, String>) -> Scan {
    let mut references = Vec::new();
    let mut unsupported = Vec::new();
    let mut in_tests = false;
    for (number, line) in text.lines().enumerate() {
        let number = number + 1;
        let code = code_of(line);
        if code.contains("#[cfg(test)]") {
            in_tests = true;
        }
        for at in path_starts(code, "crate::") {
            let rest = &code[at + "crate::".len()..];
            if rest.starts_with('{') {
                unsupported.push((number, "crate::{"));
            } else {
                references.push((number, ident_prefix(rest).to_owned()));
            }
        }
        if !path_starts(code, "tenuto::").is_empty() {
            unsupported.push((number, "tenuto::"));
        }
        for at in path_starts(code, "super::") {
            let mut rest = &code[at..];
            let mut supers = 0;
            while let Some(after) = rest.strip_prefix("super::") {
                supers += 1;
                rest = after;
            }
            if supers < depth {
                continue;
            }
            if rest.starts_with('*') || rest.starts_with('{') {
                if !in_tests {
                    unsupported.push((number, "super::* or super::{ reaching the crate root"));
                }
            } else {
                let name = ident_prefix(rest);
                if modules.contains_key(name) {
                    references.push((number, name.to_owned()));
                }
            }
        }
    }
    Scan {
        references,
        unsupported,
    }
}

/// Every layering error in `files` (`(path, text)`, paths like
/// `src/media/tags.rs`), given the crate root `lib`.
fn check(
    lib: &str,
    files: &[(String, String)],
    layers: &[(&str, u8)],
    rules: &[(&str, &str)],
    allowed: &[(&str, &str)],
) -> Vec<String> {
    let mut errors = Vec::new();
    let modules = read_registry(lib, layers, &mut errors);
    let rank: BTreeMap<&str, u8> = layers.iter().copied().collect();
    let mut used = BTreeSet::new();
    for (path, text) in files {
        let (top, depth) = module_of(path);
        let Some(from) = modules.get(top) else {
            errors.push(format!(
                "{path}: belongs to `{top}`, which src/lib.rs does not declare"
            ));
            continue;
        };
        let Scan {
            references,
            unsupported,
        } = scan(text, depth, &modules);
        for (number, form) in unsupported {
            errors.push(format!("{path}:{number}: unsupported path form: {form}"));
        }
        for (number, target) in references {
            let Some(to) = modules.get(&target) else {
                errors.push(format!(
                    "{path}:{number}: `crate::{target}` is not a declared module"
                ));
                continue;
            };
            if to == from {
                continue;
            }
            let (from_rank, to_rank) = (rank[from.as_str()], rank[to.as_str()]);
            let reason = if to_rank > from_rank {
                format!("{from} (rank {from_rank}) -> {to} (rank {to_rank})")
            } else if rules.contains(&(from.as_str(), to.as_str())) {
                format!("{from} -> {to}: {from} never references {to}")
            } else {
                continue;
            };
            if let Some(entry) = allowed
                .iter()
                .find(|(file, target)| file == path && target == to)
            {
                used.insert(*entry);
            } else {
                errors.push(format!("{path}:{number}: {reason}"));
            }
        }
    }
    for entry in allowed {
        if !used.contains(entry) {
            errors.push(format!(
                "stale allowlist entry {entry:?}: it excuses nothing, delete it"
            ));
        }
    }
    errors
}
```

How the pieces map to the spec: `read_registry` is §3.1 (an alias is not in `LAYERS`; it takes its target's rank through `canonical`). `module_of` gives the file depth of §3.2 (`src/resume.rs` and `src/media/mod.rs` are 1, `src/media/tags.rs` is 2). `scan` is §3.2's handled and rejected forms; `in_tests` flips at the first `#[cfg(test)]` and stays on. `check` is §3.3: an upward rank wins over a named rule in the message; the allowlist is matched on the canonical target.

- [ ] **Step 4: Run the self-tests and watch them pass**

Run: `cargo test --locked --test m9_layering`
Expected: 11 passed, 0 failed, no warnings.

- [ ] **Step 5: Lint and format**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`
Expected: both clean. (If `cargo fmt --check` reports a diff, run `cargo fmt` and re-check; the code above is already rustfmt output.)

- [ ] **Step 6: Commit**

```bash
git add tests/m9_layering.rs
git commit -m "test(m9): a layering checker over the crate's import paths"
```

---

### Task 2: Check the real tree, and point the docs at it

**Files:**
- Modify: `tests/m9_layering.rs` (add `LAYERS`, `ALLOWED`, `rust_files`, the tree test)
- Modify: `docs/architecture.md` (§4 diagram line 124 and the "Known layering exceptions" paragraph at line 205; §12 "Architecture deepening (M9)" at line 635)
- Modify: `CLAUDE.md` (the "Layering:" bullet, line 28)

**Interfaces:**
- Consumes from Task 1: `check`, `NAMED_RULES`, the error string formats.
- Produces: `const LAYERS: &[(&str, u8)]`, `const ALLOWED: &[(&str, &str)]` — PRs B and C delete lines from `ALLOWED`.

- [ ] **Step 1: Add the layer map, an empty allowlist and the tree test**

In `tests/m9_layering.rs`, change the imports to:

```rust
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
```

Insert above `NAMED_RULES`:

```rust
/// Rank per top-level module: 0 foundation, 1 sources, 2 engine,
/// 3 application, 4 front end, 5 entry.
const LAYERS: &[(&str, u8)] = &[
    ("clock", 0),
    ("telemetry", 0),
    ("error", 0),
    ("lifecycle", 0),
    ("media", 0),
    ("persistence", 0),
    ("playlist", 0),
    ("resume", 0),
    ("http", 1),
    ("feed", 1),
    ("subscription", 1),
    ("station", 1),
    ("playback", 2),
    ("application", 3),
    ("session", 3),
    ("library", 3),
    ("artwork", 3),
    ("tui", 4),
    ("app", 5),
    ("cli", 5),
    ("commands", 5),
];
```

Insert below `NAMED_RULES` (empty for now, to see the real violations first):

```rust
/// `(file, target module)` pairs excused today. Delete an entry with the
/// edge it excuses.
const ALLOWED: &[(&str, &str)] = &[];
```

Insert after `check`, before the `// The checker itself, on synthetic sources.` comment:

```rust
#[allow(clippy::unwrap_used)] // A bare helper: an unreadable source tree is a test failure.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_source_tree_respects_the_layers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let mut paths = Vec::new();
    rust_files(&root.join("src"), &mut paths);
    let mut files: Vec<(String, String)> = paths
        .iter()
        .map(|path| {
            let relative = path.strip_prefix(root).unwrap();
            let name = relative.to_string_lossy().replace('\\', "/");
            (name, std::fs::read_to_string(path).unwrap())
        })
        .filter(|(name, _)| name != "src/lib.rs" && name != "src/main.rs")
        .collect();
    files.sort();
    let errors = check(&lib, &files, LAYERS, NAMED_RULES, ALLOWED);
    assert!(
        errors.is_empty(),
        "layering (docs/architecture.md §4, tests/m9_layering.rs):\n{}",
        errors.join("\n")
    );
}
```

- [ ] **Step 2: Run the tree test and read the real violations**

Run: `cargo test --locked --test m9_layering the_source_tree_respects_the_layers`
Expected: FAIL, listing violations (one line per referencing line, so possibly more than 18 lines) from exactly these 16 files (18 `(file, target)` pairs; `src/tui/mod.rs` appears for both `cli` and `commands`, `src/error.rs` for both `feed` and `playback`) and nothing else: no registry error, no unsupported form, no undeclared module:

```
src/application/browse.rs  -> commands
src/application/runtime.rs -> commands
src/application/view.rs    -> commands
src/error.rs               -> feed, playback
src/lifecycle/panic.rs     -> commands
src/media/display.rs       -> http
src/media/metadata.rs      -> playback
src/media/tags.rs          -> playback
src/media/vbr_header.rs    -> playback
src/persistence/model.rs   -> playback
src/persistence/queue_codec.rs -> playback
src/playlist/queue.rs      -> playback
src/resume.rs              -> playback
src/tui/images.rs          -> cli
src/tui/mod.rs             -> cli, commands
src/tui/render/browser.rs  -> commands
```

If anything else appears, stop and report it: the spec's allowlist (§4) was checked against `0e749c0`, and a difference means the tree moved.

- [ ] **Step 3: Fill the allowlist**

Replace the empty `ALLOWED` with:

```rust
/// `(file, target module)` pairs excused today. Delete an entry with the
/// edge it excuses.
const ALLOWED: &[(&str, &str)] = &[
    // B: foundation cleanup.
    ("src/media/metadata.rs", "playback"),
    ("src/media/tags.rs", "playback"),
    ("src/media/vbr_header.rs", "playback"),
    ("src/persistence/model.rs", "playback"),
    ("src/persistence/queue_codec.rs", "playback"),
    ("src/playlist/queue.rs", "playback"),
    ("src/resume.rs", "playback"),
    ("src/error.rs", "feed"),
    ("src/error.rs", "playback"),
    ("src/media/display.rs", "http"),
    ("src/tui/mod.rs", "cli"),
    ("src/tui/images.rs", "cli"),
    // C: feed operations below `commands`.
    ("src/application/browse.rs", "commands"),
    ("src/application/runtime.rs", "commands"),
    ("src/application/view.rs", "commands"),
    ("src/lifecycle/panic.rs", "commands"),
    ("src/tui/mod.rs", "commands"),
    ("src/tui/render/browser.rs", "commands"),
];
```

- [ ] **Step 4: Run the whole file and watch it pass**

Run: `cargo test --locked --test m9_layering`
Expected: 12 passed, 0 failed.

- [ ] **Step 5: Prove the ratchet on the real tree, then undo**

Temporarily delete the line `("src/resume.rs", "playback"),` from `ALLOWED` and run:
`cargo test --locked --test m9_layering the_source_tree_respects_the_layers`
Expected: FAIL with exactly `src/resume.rs:20: resume (rank 0) -> playback (rank 2)`.

Restore the line, then temporarily add `("src/clock.rs", "tui"),` and run the same command.
Expected: FAIL with exactly `stale allowlist entry ("src/clock.rs", "tui"): it excuses nothing, delete it`.

Remove that line again. `git diff tests/m9_layering.rs` must show only Steps 1 and 3.

- [ ] **Step 6: Move `playlist` and `resume` into Foundation in the §4 diagram**

In `docs/architecture.md`, delete this line from the `appl` subgraph (line 124):

```
        queue["playlist/ (PlaylistSet, Playlist, Queue), resume.rs<br/>the playlist rules, resume decision"]
```

and add it to the `base` subgraph, directly after the `persist[...]` line:

```
        persist["persistence/<br/>model, store, atomic, writer"]
        queue["playlist/ (PlaylistSet, Playlist, Queue), resume.rs<br/>the playlist rules, resume decision"]
```

In the edge list, after `persist --> media`, add:

```
    persist --> queue
```

- [ ] **Step 7: Make the test the authority on layering exceptions**

In `docs/architecture.md`, replace the paragraph and its first two bullets, from `**Known layering exceptions.**` through the bullet ending `` `provenance`, `checkpoint` and `volume`. `` (keep the third bullet, about both front ends loading `StateStore`, unchanged), with:

```markdown
**Layering is checked.** `tests/m9_layering.rs` gives each top-level module a rank: Foundation 0 (`playlist` and `resume` included, since `persistence` and `playback` build on them), Sources 1, Engine 2, Application 3, Front end 4, Entry 5. A module may import its own rank or lower, and `playback`, `playlist` and `resume` never import `persistence`. `src/lib.rs` is the module registry: a module declared there without a rank in the test's `LAYERS` fails it, so every new module is placed deliberately.

**Known layering exceptions.** The test's `ALLOWED` list is the authority; an entry that no longer excuses anything fails, so the list only shrinks. Each is debt, not design, and the M9 roadmap (§12) names the fix:

- The entry layer is imported from below. `commands::displayable` is used by `application` (runtime, view, browse), `tui/render/browser.rs` and `lifecycle::panic`. `tui/mod.rs` takes its stores from `commands::platform_*_store`, and `application::browse` calls `commands::wait_http` (M9.3 feed operations).
- Foundation reaches up. `media::tags` calls `playback::decode`'s probing functions, and `media::tags` and `media::vbr_header` return `playback::error::PlaybackError`. `media`, `playlist`, `resume` and `persistence` import value types from `playback`: `provenance`, `checkpoint` and `volume`. `error.rs` holds `AppError`, which wraps `feed` and `playback` errors. `media::display` uses `http`'s `redact_url`. `tui` takes `ArtworkMode` and `MouseMode` from `cli` (M9 foundation cleanup).
```

Then check the lines read in order: the new two paragraphs, the two new bullets, then the unchanged `- Both front ends load `StateStore` …` bullet.

- [ ] **Step 8: Mention the test in §12 and in CLAUDE.md**

In `docs/architecture.md` §12, in the `**Architecture deepening (M9).**` bullet, insert before its last sentence (`None of it revisits the decisions above.`):

```
`tests/m9_layering.rs` checks the layering of §4 and holds the exceptions still to remove.
```

In `CLAUDE.md`, replace the line:

```
- Layering: `playback` never imports persistence or blocks on Tokio; `library` never prints or `block_on`s; only `session` builds `PersistedState` snapshots; `tui` mutates state only through `Session`.
```

with:

```
- Layering: `playback` never imports persistence or blocks on Tokio; `library` never prints or `block_on`s; only `session` builds `PersistedState` snapshots; `tui` mutates state only through `Session`. `tests/m9_layering.rs` checks import ranks (`docs/architecture.md` §4): fix the import, never add to `ALLOWED`.
```

- [ ] **Step 9: Run the full gate**

Run:
```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```
Expected: all clean; the full suite passes with the same ignored count as `main` plus 12 new passing tests in `m9_layering`. Record the totals for the PR description.

- [ ] **Step 10: Commit**

```bash
git add tests/m9_layering.rs docs/architecture.md CLAUDE.md
git commit -m "test(m9): the source tree is checked against the layers, with today's exceptions listed"
```
