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
//!   is not real (write it with `crate::`), never miss one that is. A
//!   leading `self::` is transparent.
//!
//! Rejected as unsupported: `crate::{`; `tenuto::`; a `super::` inside a
//! `::{` group, whose real depth includes the group's prefix; and a
//! `super::*` or `super::{` at least as long as the file's depth, except in
//! test code (after the file's first `#[cfg(test)]`) at exactly that depth,
//! where it means the file's own module.
//!
//! Not detected: whitespace inside a path (`crate :: x`, which `cargo fmt`
//! removes), `use crate as c`, and a reference after a `//` inside a string
//! on the same line.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
    ("volume", 0),
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

/// `(from, to)`: `from` never references `to`, whatever their ranks.
const NAMED_RULES: &[(&str, &str)] = &[
    ("playback", "persistence"),
    ("playlist", "persistence"),
    ("resume", "persistence"),
];

/// `(file, target module)` pairs excused today. Delete an entry with the
/// edge it excuses.
const ALLOWED: &[(&str, &str)] = &[
    // B: foundation cleanup.
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

/// Whether the segment at `at` begins a path: not preceded by an identifier
/// character or a `:`, except through a leading `self::`.
fn starts_path(code: &str, at: usize) -> bool {
    let before = &code[..at];
    if let Some(prefix) = before.strip_suffix("self::") {
        return starts_path(code, prefix.len());
    }
    !before
        .chars()
        .next_back()
        .is_some_and(|c| is_ident_char(c) || c == ':')
}

/// Byte offsets where `word` starts a path.
fn path_starts(code: &str, word: &str) -> Vec<usize> {
    code.match_indices(word)
        .map(|(at, _)| at)
        .filter(|&at| starts_path(code, at))
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
    // One entry per open brace: whether it opened a `::{` group.
    let mut groups: Vec<bool> = Vec::new();
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
        let mut at = 0;
        while let Some(rest) = code.get(at..).filter(|rest| !rest.is_empty()) {
            if rest.starts_with("super::") && starts_path(code, at) {
                let mut tail = rest;
                let mut supers = 0;
                while let Some(after) = tail.strip_prefix("super::") {
                    supers += 1;
                    tail = after;
                }
                at = code.len() - tail.len();
                if groups.last() == Some(&true) {
                    unsupported.push((number, "super:: inside a `::{` group"));
                } else if supers >= depth {
                    // Only a test module's glob at exactly the file's depth
                    // means the file's own module; anything longer is the
                    // crate root or past it.
                    if tail.starts_with('*') || tail.starts_with('{') {
                        if !(in_tests && supers == depth) {
                            unsupported
                                .push((number, "super::* or super::{ reaching the crate root"));
                        }
                    } else if modules.contains_key(ident_prefix(tail)) {
                        references.push((number, ident_prefix(tail).to_owned()));
                    }
                }
                continue;
            }
            if rest.starts_with('{') {
                groups.push(code[..at].ends_with("::"));
            } else if rest.starts_with('}') {
                groups.pop();
            }
            at += rest.chars().next().map_or(1, char::len_utf8);
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

#[test]
fn a_glob_past_the_test_module_s_own_file_is_unsupported() {
    let errors = errors_for(&[(
        "src/resume.rs",
        "#[cfg(test)]\nmod tests {\n    use super::super::*;\n}",
    )]);
    assert_one_error(&errors, "src/resume.rs:3: unsupported path form: super::*");
    let errors = errors_for(&[(
        "src/resume.rs",
        "#[cfg(test)]\nmod tests {\n    use super::super::{persistence};\n}",
    )]);
    assert_one_error(&errors, "src/resume.rs:3: unsupported path form: super::*");
}

#[test]
fn a_super_path_inside_a_prefixed_group_is_unsupported() {
    let errors = errors_for(&[(
        "src/playlist/queue.rs",
        "use super::{super::persistence::model::State};",
    )]);
    assert_one_error(
        &errors,
        "src/playlist/queue.rs:1: unsupported path form: super:: inside a `::{` group",
    );
    let errors = errors_for(&[(
        "src/playlist/queue.rs",
        "use super::{\n    super::persistence::model::State,\n    local,\n};",
    )]);
    assert_one_error(
        &errors,
        "src/playlist/queue.rs:2: unsupported path form: super:: inside a `::{` group",
    );
    let errors = errors_for(&[(
        "src/playlist/queue.rs",
        "use super::{local, other::Thing};\nfn f() { super::local(); }",
    )]);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn self_before_super_is_transparent() {
    let errors = errors_for(&[("src/resume.rs", "use self::super::persistence::State;")]);
    assert_one_error(&errors, "src/resume.rs:1: resume -> persistence");
}
