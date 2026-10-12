# M9 Versioned JSON File and Resume Intent Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One crate-private `persistence::json_file` module owns the read / load-for-writing / quarantine / save lifecycle that `state.json`, `subscriptions.json` and `stations.json` each copy today. `library`'s two load-for-writing copies become one. `resume.rs` owns the fresh-load resume intent, and its helpers become private.

**Architecture:** There are five code tasks and one documentation and verification task.
- Task 1 builds `JsonFile` with its lifecycle suite and moves `StateStore` onto it.
- Task 2 moves the subscription and station stores onto it.
- Task 3 merges `library`'s refusal helpers.
- Task 4 pins the decode diagnostics in the log.
- Task 5 moves resume intent into `resume.rs`.
- Task 6 updates the docs, runs the CLI parity check and writes the acceptance record.

Each item lands in `json_file` with the task that first uses it, so no task leaves dead code for clippy to reject.

**Tech Stack:** Rust 1.98.1, edition 2024, serde / serde_json, thiserror 2, time (`serde::rfc3339`), tracing, `tempfile` (dev).

**Spec:** `docs/superpowers/specs/2026-10-11-tenuto-m9-versioned-json-file-design.md`. Read it before starting. §2 (compatibility boundary) is the contract every task keeps.

## Global Constraints

- Every cargo command uses `--locked`; the toolchain is 1.98.1.
- Runtime code has no `unsafe`, `unwrap` or `expect` (the lints deny them). Tests may use them (`clippy.toml`). Helpers in `tests/support/` are not `#[test]` code: they `match` and `panic!` instead, like `support::media`.
- On-disk formats are unchanged: `state.json` reads `1..=4` and writes 4, while `subscriptions.json` and `stations.json` read and write exactly 1. Saves are pretty JSON through `atomic::replace_bytes`.
- Quarantine names are `<file name>.rejected-<stamp>`, then `-2` … `-100`. Recovery copies are `<file name>.queue-recovery-<stamp>[-n]`. `<stamp>` is `YYYYMMDDTHHMMSSZ`. Under `FakeClock` it is `19700101T000000Z`.
- `json_file` is `pub(crate)`. Public rustdoc must not link to it (`rustdoc::private_intra_doc_links` fails `cargo doc -D warnings`): name it in plain backticks.
- `persistence::store::LoadReason` and `persistence::store::MAX_QUARANTINE_CANDIDATES` keep working through `pub use` re-exports. `playback::command::ResumeIntent` keeps working through `pub use crate::resume::ResumeIntent`.
- Never add to `tests/m9_layering.rs`'s `ALLOWED` or `NAMED_RULES`. `resume` must not import `persistence`.
- No log line may carry file content or an unredacted URL.
- Moved items keep their doc comments; update only the paths and names inside them.
- Commits carry no `Co-authored-by` or tool attribution.
- Work on branch `refactor/m9-json-file`, which already holds the spec commits `dfe17e0` and `aacadc4`.
- Each task ends with this gate, in order: `cargo fmt --check`, then `cargo clippy --locked --all-targets --all-features -- -D warnings`, then `cargo test --locked --no-fail-fast`.

## Review Focus

1. **Envelope shapes that are not a version.** This covers an empty file, `"schema_version":"2"`, a value above `u32::MAX`, a negative value, a missing field, a top-level array, and valid JSON followed by trailing bytes. Each must be classified malformed and quarantined, never treated as an unsupported version and kept with writing off. *Pinned in Task 1:* `envelope_shapes_that_are_not_a_version_are_malformed`.
2. **Listing progress while `state.json` holds a queue the next load will repair.** A read must succeed and must not leave a `queue-recovery` copy behind. *Pinned in Task 1:* `a_read_never_makes_a_recovery_copy` in `tests/m5_state_recovery.rs`.
3. **Exact read-only error text.** This covers `tenuto feeds` and the Radio tab with an unreadable, newer-version, broken-JSON or invalid-record file. Each reason string must stay as it is today. *Pinned in Task 2:* `read_errors_keep_their_text` in both store suites, written against today's code first.
4. **An episode stored at exactly `0:00` versus one never stored.** `Some(ZERO)` must give `Candidate { position: ZERO }`, and `None` must give `StartAt(ZERO)`. An estimate-only entry must report `established: None`. *Pinned in Task 5:* the `resume_intent_*` tests in `tests/resume_decision.rs`.
5. **A finished local file or URL.** The fresh-load media policy beats completion, so it gives `StartAt(ZERO)` and not `Candidate { completed: true }`. A finished episode with a newer estimate stays `Candidate { completed: true }`. *Pinned in Task 5:* `resume_intent_never_resumes_a_local_file_or_url` and `resume_intent_completion_beats_an_estimate`.

---

### Task 1: `JsonFile`, and `StateStore` on top of it

**Files:**
- Create: `src/persistence/json_file.rs`
- Modify: `src/persistence/mod.rs` (declare the module)
- Modify: `src/persistence/store.rs` (whole file rewritten below)
- Modify: `src/lifecycle/stderr.rs:20` (the `stamp` import)
- Modify: `tests/persistence_store.rs` (delete moved tests)
- Modify: `tests/m5_state_recovery.rs` (one new test)

**Interfaces:**
- Consumes: `atomic::{replace_bytes, create_private_new, set_private, sync_parent_best_effort}`, `PersistenceError`, `crate::clock::Clock`.
- Produces, all in `crate::persistence::json_file`:
  - `pub struct JsonFile` with `new(PathBuf, Arc<dyn Clock>) -> Self` and `path(&self) -> &Path`.
  - `read<T>(&self, RangeInclusive<u32>, impl FnOnce(u32, &[u8]) -> Result<T, Malformed>) -> Result<Option<T>, Rejected>`.
  - `load<T>(&self, same args) -> Load<T>`.
  - `save(&self, &impl Serialize) -> Result<(), PersistenceError>`.
  - `keep_copy(&self, &[u8], &str) -> Option<PathBuf>`.
  - `pub enum LoadReason` (moved, unchanged).
  - `pub enum Malformed { Json { category: &'static str, line: usize, column: usize } }`.
  - `pub enum Rejected { Unreadable(io::Error), UnsupportedVersion(u32), Malformed(Malformed) }`.
  - `pub struct Load<T> { found: Option<Found<T>>, writable: bool, reason: LoadReason }` and `pub struct Found<T> { value: T, bytes: Vec<u8> }`.
  - `pub fn malformed(&serde_json::Error) -> Malformed`, `pub(crate) fn stamp(OffsetDateTime) -> String`, and `pub const MAX_QUARANTINE_CANDIDATES: u32 = 100`.
  - Task 2 adds `Malformed::Invalid`, `Malformed::reason`, `Rejected::reason` and `JsonFile::now`.

- [ ] **Step 1: Write the lifecycle suite, with no implementation yet**

Add to `src/persistence/mod.rs`, after `pub mod atomic;`:

```rust
pub(crate) mod json_file;
```

Create `src/persistence/json_file.rs` with only this content:

```rust
#[cfg(test)]
mod tests {
    use std::fs;
    use std::ops::RangeInclusive;
    use std::path::Path;
    use std::sync::Arc;

    use serde::{Deserialize, Serialize};

    use crate::clock::FakeClock;

    use super::{JsonFile, LoadReason, MAX_QUARANTINE_CANDIDATES, Malformed, Rejected, malformed};

    /// The stamp a `FakeClock` produces, which starts at the epoch.
    const EPOCH_STAMP: &str = "19700101T000000Z";

    /// A band with a version on each side of it, so "below" and "above" are
    /// both testable.
    const BAND: RangeInclusive<u32> = 2..=3;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Toy {
        schema_version: u32,
        items: Vec<u32>,
    }

    fn file_in(dir: &Path) -> JsonFile {
        JsonFile::new(dir.join("toy.json"), Arc::new(FakeClock::new()))
    }

    /// Hands back the version it was given, so a test can see it.
    fn decode(version: u32, bytes: &[u8]) -> Result<(u32, Vec<u32>), Malformed> {
        let toy: Toy = serde_json::from_slice(bytes).map_err(|source| malformed(&source))?;
        Ok((version, toy.items))
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_missing_file_reads_as_none_and_loads_writable_creating_nothing() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("tenuto");
        let file = JsonFile::new(nested.join("toy.json"), Arc::new(FakeClock::new()));

        assert!(matches!(file.read(BAND, decode), Ok(None)));
        assert!(!nested.exists(), "a read never creates the parent directory");

        let load = file.load(BAND, decode);
        assert!(load.found.is_none());
        assert!(load.writable);
        assert!(matches!(load.reason, LoadReason::Missing));
        assert!(!nested.exists(), "a missing load does not create it either");
    }

    #[test]
    fn a_directory_at_the_path_is_unreadable_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("toy.json")).unwrap();
        let file = file_in(dir.path());

        assert!(matches!(file.read(BAND, decode), Err(Rejected::Unreadable(_))));

        let load = file.load(BAND, decode);
        assert!(load.found.is_none());
        assert!(!load.writable);
        assert!(matches!(load.reason, LoadReason::Unreadable));
        assert!(dir.path().join("toy.json").is_dir());
        assert_eq!(listing(dir.path()), ["toy.json"]);
    }

    #[test]
    fn a_version_outside_the_band_is_unsupported_even_when_its_shape_is_alien() {
        let cases: [(u32, &[u8]); 2] = [
            (1, br#"{"schema_version":1,"items":"not a list"}"#),
            (4, br#"{"schema_version":4,"items":{"a":true}}"#),
        ];
        for (version, bytes) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("toy.json");
            fs::write(&path, bytes).unwrap();
            let file = file_in(dir.path());

            assert!(
                matches!(file.read(BAND, decode), Err(Rejected::UnsupportedVersion(found)) if found == version)
            );
            let load = file.load(BAND, decode);
            assert!(
                matches!(load.reason, LoadReason::UnsupportedVersion { found } if found == version)
            );
            assert!(!load.writable);
            assert!(load.found.is_none());
            assert_eq!(fs::read(&path).unwrap(), bytes, "preserved in place");
            assert_eq!(listing(dir.path()), ["toy.json"], "not quarantined");
        }
    }

    #[test]
    fn a_read_of_a_malformed_file_reports_it_and_never_moves_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toy.json");
        fs::write(&path, b"{ not json").unwrap();

        assert!(matches!(
            file_in(dir.path()).read(BAND, decode),
            Err(Rejected::Malformed(Malformed::Json { category: "syntax", .. }))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"{ not json");
        assert_eq!(listing(dir.path()), ["toy.json"]);
    }

    /// Review Focus 1: only a readable `u32` is a version. Anything else is
    /// garbage, quarantined, never "a newer build's file" kept with writing
    /// off.
    #[test]
    fn envelope_shapes_that_are_not_a_version_are_malformed() {
        let cases: [&[u8]; 7] = [
            b"",
            br#"{"schema_version":"2","items":[]}"#,
            br#"{"schema_version":4294967296,"items":[]}"#,
            br#"{"schema_version":-1,"items":[]}"#,
            br#"{"items":[]}"#,
            b"[]",
            br#"{"schema_version":2,"items":[]} trailing"#,
        ];
        for bytes in cases {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("toy.json"), bytes).unwrap();
            let file = file_in(dir.path());
            let shown = String::from_utf8_lossy(bytes);

            assert!(
                matches!(file.read(BAND, decode), Err(Rejected::Malformed(_))),
                "read of {shown:?}"
            );
            let load = file.load(BAND, decode);
            assert!(
                matches!(load.reason, LoadReason::Quarantined { .. }),
                "load of {shown:?}: {:?}",
                load.reason
            );
            assert!(load.writable);
        }
    }

    #[test]
    fn a_malformed_load_is_quarantined_under_its_file_name_and_writing_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toy.json");
        fs::write(&path, b"{ not json").unwrap();

        let load = file_in(dir.path()).load(BAND, decode);
        let LoadReason::Quarantined { moved_to } = &load.reason else {
            panic!("expected a quarantine, got {:?}", load.reason);
        };
        assert!(load.writable);
        assert!(load.found.is_none());
        assert!(!path.exists());
        assert_eq!(fs::read(moved_to).unwrap(), b"{ not json");
        assert_eq!(
            moved_to.file_name().unwrap().to_str().unwrap(),
            format!("toy.json.rejected-{EPOCH_STAMP}")
        );
    }

    #[test]
    fn a_quarantine_collision_is_suffixed_never_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("toy.json"), b"garbage").unwrap();
        let taken = dir.path().join(format!("toy.json.rejected-{EPOCH_STAMP}"));
        fs::write(&taken, b"an earlier rejection").unwrap();

        let load = file_in(dir.path()).load(BAND, decode);
        let LoadReason::Quarantined { moved_to } = &load.reason else {
            panic!("expected a quarantine, got {:?}", load.reason);
        };
        assert_eq!(
            moved_to.file_name().unwrap().to_str().unwrap(),
            format!("toy.json.rejected-{EPOCH_STAMP}-2")
        );
        assert_eq!(fs::read(&taken).unwrap(), b"an earlier rejection");
    }

    #[test]
    fn a_quarantine_with_every_candidate_taken_disables_writing_and_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("toy.json"), b"garbage").unwrap();
        for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
            let name = if suffix == 1 {
                format!("toy.json.rejected-{EPOCH_STAMP}")
            } else {
                format!("toy.json.rejected-{EPOCH_STAMP}-{suffix}")
            };
            fs::write(dir.path().join(name), b"taken").unwrap();
        }

        let load = file_in(dir.path()).load(BAND, decode);
        assert!(matches!(load.reason, LoadReason::QuarantineFailed));
        assert!(!load.writable);
        assert_eq!(fs::read(dir.path().join("toy.json")).unwrap(), b"garbage");
    }

    #[test]
    fn a_load_hands_decode_the_found_version_and_returns_the_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"{ \"schema_version\" : 3 ,\n  \"items\": [7, 8] }\n";
        fs::write(dir.path().join("toy.json"), bytes).unwrap();
        let file = file_in(dir.path());

        let load = file.load(BAND, decode);
        assert!(load.writable);
        assert!(matches!(load.reason, LoadReason::Loaded));
        let found = load.found.unwrap();
        assert_eq!(found.value, (3, vec![7, 8]));
        assert_eq!(found.bytes, bytes);

        assert!(matches!(file.read(BAND, decode), Ok(Some((3, _)))));
    }

    #[test]
    fn a_save_is_pretty_private_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = file_in(dir.path());
        let toy = Toy {
            schema_version: 2,
            items: vec![1, 2],
        };

        file.save(&toy).unwrap();

        let path = dir.path().join("toy.json");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            serde_json::to_string_pretty(&toy).unwrap()
        );
        assert!(matches!(file.read(BAND, decode), Ok(Some((2, items))) if items == [1, 2]));
        assert_eq!(listing(dir.path()), ["toy.json"]);
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn keep_copy_writes_the_exact_bytes_privately_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let taken = dir
            .path()
            .join(format!("toy.json.queue-recovery-{EPOCH_STAMP}"));
        fs::write(&taken, b"an earlier copy").unwrap();

        let copy = file_in(dir.path())
            .keep_copy(b"\x00original\xff", "queue-recovery")
            .unwrap();

        assert_eq!(
            copy.file_name().unwrap().to_str().unwrap(),
            format!("toy.json.queue-recovery-{EPOCH_STAMP}-2")
        );
        assert_eq!(fs::read(&copy).unwrap(), b"\x00original\xff");
        assert_eq!(fs::read(&taken).unwrap(), b"an earlier copy");
        #[cfg(unix)]
        assert_eq!(mode(&copy), 0o600);
    }

    #[test]
    fn malformed_keeps_the_position_and_drops_the_content() {
        let error = serde_json::from_slice::<Toy>(
            br#"{"schema_version":2,"items":["https://SECRETHOST/x"]}"#,
        )
        .unwrap_err();

        let sanitized = malformed(&error);

        assert!(matches!(
            sanitized,
            Malformed::Json {
                category: "data",
                line: 1,
                ..
            }
        ));
        assert!(!format!("{sanitized:?}").contains("SECRETHOST"));
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --locked --lib persistence::json_file`
Expected: compile errors: `JsonFile`, `LoadReason`, `Malformed`, `Rejected`, `malformed`, `MAX_QUARANTINE_CANDIDATES` not found in `super`.

- [ ] **Step 3: Implement the module**

Put this above the `#[cfg(test)]` block in `src/persistence/json_file.rs`. `LoadReason` and its variant docs move verbatim from `store.rs`.

```rust
//! One versioned JSON file: read it, classify it, move garbage aside, and
//! replace it atomically. `state.json`, `subscriptions.json` and
//! `stations.json` share this lifecycle; each store keeps only its record
//! shape and validation.
//!
//! A concrete struct with generic methods, not a trait
//! (`docs/architecture.md` §12): the stores differ in what they decode,
//! never in how the file is handled.
//!
//! - [`JsonFile::read`] is for read-only paths. It never quarantines,
//!   creates a directory, copies or writes; a missing file is `Ok(None)` and
//!   anything else unusable is an `Err` the caller shows.
//! - [`JsonFile::load`] is for the paths that will write. An unreadable
//!   file or an unsupported version stays in place with writing off; a
//!   malformed file is moved aside, and writing stays on only if that
//!   worked.

use std::fs;
use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::clock::Clock;

use super::PersistenceError;
use super::atomic::{create_private_new, replace_bytes, set_private, sync_parent_best_effort};

/// How many `-2`, `-3`, … candidates a quarantine or a kept copy will try
/// before giving up.
pub const MAX_QUARANTINE_CANDIDATES: u32 = 100;

#[derive(Debug)]
pub enum LoadReason {
    Loaded,
    Missing,
    /// The file was garbage and has been moved aside; writing continues.
    Quarantined {
        moved_to: PathBuf,
    },
    /// The file was garbage and could not be moved aside; writing is disabled
    /// so that the next checkpoint does not overwrite what §6 requires be kept.
    QuarantineFailed,
    /// A version this build does not support; preserved in place.
    UnsupportedVersion {
        found: u32,
    },
    /// Present but unreadable. Preserved in place for the same reason.
    Unreadable,
}

/// Why a file's bytes are not a usable file. Deliberately carries no text
/// from the file: a `serde_json::Error`'s `Display` can quote the input, and
/// these files hold URLs (design doc §7.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Malformed {
    Json {
        category: &'static str,
        line: usize,
        column: usize,
    },
}

/// What [`JsonFile::read`] reports for a file it cannot hand back.
#[derive(Debug)]
pub enum Rejected {
    Unreadable(io::Error),
    UnsupportedVersion(u32),
    Malformed(Malformed),
}

/// What [`JsonFile::load`] found, and whether the caller may write.
pub struct Load<T> {
    pub found: Option<Found<T>>,
    pub writable: bool,
    pub reason: LoadReason,
}

/// A decoded file and the exact bytes it was decoded from, for a caller that
/// must keep the original aside before writing (`state.json`'s queue
/// recovery).
pub struct Found<T> {
    pub value: T,
    pub bytes: Vec<u8>,
}

/// Only the field every future version is obliged to keep. Read before the
/// full shape, so that a valid newer file is never misclassified as garbage.
#[derive(Deserialize)]
struct VersionEnvelope {
    schema_version: u32,
}

/// The two ways a present, readable file is refused.
enum Refused {
    Version(u32),
    Malformed(Malformed),
}

impl From<Refused> for Rejected {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Version(found) => Self::UnsupportedVersion(found),
            Refused::Malformed(malformed) => Self::Malformed(malformed),
        }
    }
}

pub struct JsonFile {
    path: PathBuf,
    clock: Arc<dyn Clock>,
}

impl JsonFile {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self { path, clock }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read<T>(
        &self,
        versions: RangeInclusive<u32>,
        decode: impl FnOnce(u32, &[u8]) -> Result<T, Malformed>,
    ) -> Result<Option<T>, Rejected> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Rejected::Unreadable(error)),
        };
        decode_versioned(&bytes, &versions, decode)
            .map(Some)
            .map_err(Rejected::from)
    }

    pub fn load<T>(
        &self,
        versions: RangeInclusive<u32>,
        decode: impl FnOnce(u32, &[u8]) -> Result<T, Malformed>,
    ) -> Load<T> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Self::fresh(true, LoadReason::Missing);
            }
            Err(error) => {
                tracing::warn!(
                    path = ?self.path,
                    %error,
                    "cannot read the file; leaving it in place and not writing this session"
                );
                return Self::fresh(false, LoadReason::Unreadable);
            }
        };

        match decode_versioned(&bytes, &versions, decode) {
            Ok(value) => Load {
                found: Some(Found { value, bytes }),
                writable: true,
                reason: LoadReason::Loaded,
            },
            Err(Refused::Version(found)) => {
                tracing::warn!(
                    path = ?self.path,
                    found,
                    supported = *versions.end(),
                    "unsupported schema; preserving the file and not writing this session"
                );
                Self::fresh(false, LoadReason::UnsupportedVersion { found })
            }
            Err(Refused::Malformed(malformed)) => {
                log_malformed(&self.path, &malformed);
                self.reject_malformed()
            }
        }
    }

    /// Pretty JSON, replaced atomically: temp file, 0600, fsync, rename,
    /// best-effort parent fsync (`atomic::replace_bytes`). Validates
    /// nothing: each store validates before it calls this.
    pub fn save(&self, value: &impl Serialize) -> Result<(), PersistenceError> {
        let bytes =
            serde_json::to_vec_pretty(value).map_err(|source| PersistenceError::Serialize {
                path: self.path.clone(),
                source,
            })?;
        replace_bytes(&self.path, &bytes)
    }

    /// Copies `bytes` aside as `<file name>.<tag>-<stamp>[-n]` with
    /// create-new semantics, never renaming or rewriting the original. The
    /// copy can hold the user's whole listening history, so its mode is
    /// set explicitly rather than left to the umask (D12). `None` on any
    /// failure but the parent fsync, which stays best-effort.
    pub fn keep_copy(&self, bytes: &[u8], tag: &str) -> Option<PathBuf> {
        for candidate in self.candidates(tag) {
            let mut file = match create_private_new(&candidate) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            };
            if set_private(&candidate).is_err() {
                return None;
            }
            if file
                .write_all(bytes)
                .and_then(|()| file.sync_all())
                .is_err()
            {
                return None;
            }
            sync_parent_best_effort(self.parent());
            return Some(candidate);
        }
        None
    }

    fn fresh<T>(writable: bool, reason: LoadReason) -> Load<T> {
        Load {
            found: None,
            writable,
            reason,
        }
    }

    fn reject_malformed<T>(&self) -> Load<T> {
        match self.quarantine() {
            Some(moved_to) => {
                tracing::warn!(path = ?self.path, ?moved_to, "file quarantined");
                Self::fresh(true, LoadReason::Quarantined { moved_to })
            }
            None => {
                tracing::warn!(
                    path = ?self.path,
                    "cannot quarantine the file; not writing this session"
                );
                Self::fresh(false, LoadReason::QuarantineFailed)
            }
        }
    }

    /// Moves the file aside under a timestamped name, never over one that
    /// already exists. Single-user, single-process by design (§17), so the
    /// exists-then-rename window is not a hazard worth more machinery.
    fn quarantine(&self) -> Option<PathBuf> {
        for candidate in self.candidates("rejected") {
            if candidate.exists() {
                continue;
            }
            return fs::rename(&self.path, &candidate).is_ok().then_some(candidate);
        }
        None
    }

    fn candidates(&self, tag: &str) -> impl Iterator<Item = PathBuf> {
        let name = self.path.file_name().unwrap_or_default().to_string_lossy();
        let base = format!("{name}.{tag}-{}", stamp(self.clock.sample().wall));
        let dir = self.parent().to_path_buf();
        (1..=MAX_QUARANTINE_CANDIDATES).map(move |suffix| {
            if suffix == 1 {
                dir.join(&base)
            } else {
                dir.join(format!("{base}-{suffix}"))
            }
        })
    }

    fn parent(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// Version envelope first, then `decode`. Touches no filesystem.
fn decode_versioned<T>(
    bytes: &[u8],
    versions: &RangeInclusive<u32>,
    decode: impl FnOnce(u32, &[u8]) -> Result<T, Malformed>,
) -> Result<T, Refused> {
    let envelope = serde_json::from_slice::<VersionEnvelope>(bytes)
        .map_err(|source| Refused::Malformed(malformed(&source)))?;
    if !versions.contains(&envelope.schema_version) {
        return Err(Refused::Version(envelope.schema_version));
    }
    decode(envelope.schema_version, bytes).map_err(Refused::Malformed)
}

/// The sanitized detail a listener needs to repair the file, as fields,
/// with none of its content.
fn log_malformed(path: &Path, malformed: &Malformed) {
    match *malformed {
        Malformed::Json {
            category,
            line,
            column,
        } => tracing::warn!(path = ?path, category, line, column, "the file is malformed"),
    }
}

/// Sanitizes a `serde_json::Error` into a category plus line and column,
/// deliberately dropping its `Display` text, which can quote the file's raw
/// content — a checkpoint key or a feed URL.
pub fn malformed(source: &serde_json::Error) -> Malformed {
    use serde_json::error::Category;

    let category = match source.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    Malformed::Json {
        category,
        line: source.line(),
        column: source.column(),
    }
}

/// `20260908T143211Z` — filesystem-safe, no colons (§13).
///
/// `pub(crate)` so `lifecycle::stderr` uses the same format for per-session
/// TUI log filenames rather than duplicating it.
pub(crate) fn stamp(at: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    )
}
```

- [ ] **Step 4: Run the suite to verify it passes**

Run: `cargo test --locked --lib persistence::json_file`
Expected: 12 passed. `cargo build` may warn that `JsonFile` is unused until Step 5; that is expected here.

- [ ] **Step 5: Move `StateStore` onto `JsonFile`**

Replace everything in `src/persistence/store.rs` from the top of the file through the end of `stamp()` with the following. Nothing in that file survives outside it.

```rust
//! The state file on disk: `state.json`'s decode, migration and queue
//! recovery, over the shared versioned-file lifecycle in `json_file`.

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::clock::Clock;
use crate::media::id::MediaId;

use super::PersistenceError;
use super::json_file::{self, Found, JsonFile, Load, Malformed, Rejected};
use super::model::{PersistedCheckpoint, PersistedState, RawState, SCHEMA_VERSION};
use super::queue_codec::QueueReset;

pub use super::json_file::{LoadReason, MAX_QUARANTINE_CANDIDATES};

/// The oldest schema version `load` still knows how to bring forward to
/// [`SCHEMA_VERSION`] (design doc §4.5). A version below this, like one
/// above [`SCHEMA_VERSION`], is genuinely unreadable rather than migratable:
/// `UnsupportedVersion` and a preserved file either way.
const OLDEST_MIGRATABLE_VERSION: u32 = 1;

/// Every version `load` accepts. A file from a build that renumbered
/// downward is as unreadable as one from a build ahead of this one; the band
/// is widened by exactly the versions this build can bring forward.
const VERSIONS: RangeInclusive<u32> = OLDEST_MIGRATABLE_VERSION..=SCHEMA_VERSION;

/// The decode step shared by [`StateStore::load`] and
/// [`StateStore::read_snapshot`], after the shared version check (design
/// doc §5.5). Touches no filesystem: what each caller does with a rejection
/// is theirs.
///
/// `accept_queue` gates whether the file's queue data — schema 4's
/// `playlists`/`playing`, or schema 3's `queue`/`active_entry` — is decoded
/// at all: [`StateStore::load`] passes `true`, and
/// [`StateStore::read_snapshot`] passes `false`, since a listing never
/// examines queue data. The queue's own recovery outcome — `Some(QueueReset)`
/// when the queue or active entry had to be reset — is returned alongside
/// the state; [`StateStore::load`] uses it to keep the original bytes aside
/// before writing resumes (§6), and [`StateStore::read_snapshot`] discards
/// it, since a read never copies or writes anything.
fn decode_state(
    path: &Path,
    version: u32,
    bytes: &[u8],
    accept_queue: bool,
) -> Result<(PersistedState, Option<QueueReset>), Malformed> {
    let raw = serde_json::from_slice::<RawState>(bytes)
        .map_err(|source| json_file::malformed(&source))?;
    let (mut state, queue_reset) = raw.into_state(accept_queue);

    // A v1 file's shape already deserialises cleanly into the current
    // `PersistedState` (§4.5): `position` lands in `Some`, and the absent
    // `estimated` defaults to `None`. What is missing is the label — without
    // this, the next write would serialise that v2-shaped data back out
    // under a v1 envelope, which the next v1 build would read and quietly
    // discard.
    if version != SCHEMA_VERSION {
        tracing::info!(
            path = ?path,
            from = version,
            to = SCHEMA_VERSION,
            "migrating the state file to the current schema"
        );
        state.migrate_to_current_schema();
    }
    Ok((state, queue_reset))
}

/// Whether the exact original bytes were copied aside before a repaired
/// queue's state could reach a writer (§6).
#[derive(Debug)]
pub enum QueueBackup {
    Saved(PathBuf),
    Failed,
}

/// Reported on [`LoadOutcome`] whenever `decode_state` had to reset queue
/// data: what was reset, and whether the pre-repair bytes were preserved.
#[derive(Debug)]
pub struct QueueRepair {
    pub reset: QueueReset,
    pub backup: QueueBackup,
}

pub struct LoadOutcome {
    pub state: PersistedState,
    pub writable: bool,
    pub reason: LoadReason,
    pub queue_repair: Option<QueueRepair>,
}

/// A read-only wrapper over a decoded [`PersistedState`], returned by
/// [`StateStore::read_snapshot`] (design doc §5.5). It exposes only the
/// lookups a listing needs and carries no mutator at all, so a read path
/// cannot record a checkpoint even by mistake.
pub struct StateSnapshot(PersistedState);

impl StateSnapshot {
    pub fn entry_for(&self, media: &MediaId) -> Option<&PersistedCheckpoint> {
        self.0.entry_for(media)
    }

    pub fn completed_for(&self, media: &MediaId) -> bool {
        self.0.completed_for(media)
    }
}

pub struct StateStore {
    file: JsonFile,
}

impl StateStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self {
            file: JsonFile::new(path, clock),
        }
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// The platform state path. Only `app` calls this, which is what keeps the
    /// tests off `$HOME` (§13).
    pub fn platform_path() -> Result<PathBuf, PersistenceError> {
        let dirs = directories::ProjectDirs::from("", "", "tenuto")
            .ok_or(PersistenceError::NoStateDirectory)?;
        // `state_dir` honors XDG_STATE_HOME on Linux and is None elsewhere.
        let base = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf();
        Ok(base.join("state.json"))
    }

    pub fn load(&self) -> LoadOutcome {
        let path = self.file.path();
        let Load {
            found,
            writable,
            reason,
        } = self
            .file
            .load(VERSIONS, |version, bytes| decode_state(path, version, bytes, true));

        let Some(Found {
            value: (state, queue_reset),
            bytes,
        }) = found
        else {
            return LoadOutcome {
                state: PersistedState::default(),
                writable,
                reason,
                queue_repair: None,
            };
        };
        let Some(reset) = queue_reset else {
            return LoadOutcome {
                state,
                writable,
                reason,
                queue_repair: None,
            };
        };

        // §6: the exact bytes go aside before any writer can replace them,
        // and a failed copy costs the session its writes.
        let backup = match self.file.keep_copy(&bytes, "queue-recovery") {
            Some(path) => QueueBackup::Saved(path),
            None => QueueBackup::Failed,
        };
        let writable = writable && matches!(backup, QueueBackup::Saved(_));
        tracing::warn!(
            fields = reset.fields_reset(),
            writable,
            "queue data in the state file was reset"
        );
        LoadOutcome {
            state,
            writable,
            reason,
            queue_repair: Some(QueueRepair { reset, backup }),
        }
    }

    /// The non-mutating counterpart to [`Self::load`] (design doc §5.5): a
    /// listing that only wants to join against checkpoints must never
    /// quarantine a malformed file or create the state directory, since
    /// doing either from a read would rewrite `state.json` while merely
    /// displaying progress. A missing file is not an error here — it is the
    /// normal "nothing has ever played" state — but anything present and
    /// unreadable is, so a listing can never mistake "state could not be
    /// read" for "nothing has been played yet".
    pub fn read_snapshot(&self) -> Result<StateSnapshot, PersistenceError> {
        let path = self.file.path();
        match self
            .file
            .read(VERSIONS, |version, bytes| decode_state(path, version, bytes, false))
        {
            Ok(Some((state, _))) => Ok(StateSnapshot(state)),
            Ok(None) => Ok(StateSnapshot(PersistedState::default())),
            Err(Rejected::Unreadable(source)) => Err(PersistenceError::Io {
                path: path.to_path_buf(),
                op: "read",
                source,
            }),
            Err(Rejected::UnsupportedVersion(found)) => Err(PersistenceError::UnsupportedVersion {
                path: path.to_path_buf(),
                found,
                supported: SCHEMA_VERSION,
            }),
            Err(Rejected::Malformed(Malformed::Json {
                category,
                line,
                column,
            })) => Err(PersistenceError::Deserialize {
                path: path.to_path_buf(),
                category,
                line,
                column,
            }),
        }
    }

    pub fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        // The version the file claims is this build's, asserted here rather than
        // taken from the snapshot on trust: a file stamped with a version this
        // build cannot read would be quarantined or preserved by its own next
        // load (D3). Only a defect inside this crate could get one here — the
        // field is private to the model and writing is already disabled for
        // every version but this one — so it is an assertion, not a repair, and
        // it stays out of the release path where D11 forbids a panic.
        debug_assert_eq!(
            state.schema_version(),
            SCHEMA_VERSION,
            "only this build's schema version is ever written"
        );
        self.file.save(state)
    }
}
```

In `src/lifecycle/stderr.rs:20`, replace `use crate::persistence::store::stamp;` with `use crate::persistence::json_file::stamp;`.

- [ ] **Step 6: Add the Review Focus 2 test**

Append to `tests/m5_state_recovery.rs`:

```rust
/// Review Focus 2: a listing reads progress from a `state.json` whose queue
/// the next `load` will repair. The read succeeds and leaves no recovery
/// copy behind: only `load` keeps the original aside.
#[test]
fn a_read_never_makes_a_recovery_copy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let original = std::fs::read(&path).expect("read");

    let snapshot = StateStore::new(path.clone(), Arc::new(FakeClock::new()))
        .read_snapshot()
        .expect("a repairable queue does not make the file unreadable");

    assert!(snapshot.completed_for(&media("a")));
    assert_eq!(std::fs::read(&path).expect("unchanged"), original);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .expect("listing")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from("state.json")]);
}
```

- [ ] **Step 7: Shrink `tests/persistence_store.rs` to what is `state.json`'s own**

Delete these tests; the lifecycle suite now owns them:
- `a_missing_file_yields_empty_state_and_writing_stays_on`
- `a_quarantine_name_collision_is_suffixed_rather_than_clobbered`
- `a_quarantine_that_cannot_be_performed_disables_writing_and_keeps_the_file`
- `an_unsupported_version_is_preserved_in_place_and_disables_writing`
- `a_newer_file_is_classified_by_version_even_when_its_shape_is_alien`

Keep `a_malformed_file_is_quarantined_and_writing_continues` as the wiring test (it pins the `state.json.rejected-…` name). Above it, add:

```rust
/// Wiring: `StateStore` hands the shared lifecycle its own file name, so the
/// quarantine is `state.json.rejected-…`. The band is pinned by
/// `a_genuinely_unknown_version_is_still_preserved_unwritten` below.
```

Keep every other test. Then drop the `MAX_QUARANTINE_CANDIDATES` import if it is now unused.

- [ ] **Step 8: Run the gate**

Run, in order: `cargo fmt --check`, then `cargo clippy --locked --all-targets --all-features -- -D warnings`, then `cargo test --locked --no-fail-fast`.
Expected: all pass. `tests/m5_state_recovery.rs`, `tests/m5_state_schema.rs`, `tests/m8_state_playlists.rs` and `tests/m9_layering.rs` are the suites most likely to catch a slip.

- [ ] **Step 9: Commit**

```bash
git add src/persistence src/lifecycle/stderr.rs tests/persistence_store.rs tests/m5_state_recovery.rs
git commit -m "refactor(m9): one versioned JSON file lifecycle, and state.json on it"
```

---

### Task 2: The subscription and station stores on `JsonFile`

**Files:**
- Modify: `src/persistence/json_file.rs` (`Malformed::Invalid`, `Malformed::reason`, `Rejected::reason`, `JsonFile::now`, the `Invalid` log arm, two tests)
- Modify: `src/persistence/store.rs` (the `Invalid` fallback in `read_snapshot`)
- Modify: `src/subscription/store.rs`
- Modify: `src/station/store.rs`
- Modify: `tests/m4_subscription_store.rs`, `tests/m7_1_station_store.rs`

**Interfaces:**
- Consumes: Task 1's `JsonFile`, `Load`, `Found`, `Malformed`, `Rejected` and `malformed`.
- Produces, used by Task 3 and Task 4:
  - `Malformed::Invalid(&'static str)` and `Malformed::reason(&self, file: &str) -> String`.
  - `Rejected::reason(&self, file: &str, supported: u32) -> String`.
  - `JsonFile::now(&self) -> OffsetDateTime`.
- The store APIs are unchanged: `SubscriptionStore` and `StationStore` keep `new`, `path`, `now`, `read_snapshot`, `load` and `save`, plus `StationStore::logo_for` and `is_station`.

- [ ] **Step 1: Pin today's read-error text (Review Focus 3), on today's code**

Append to `tests/m4_subscription_store.rs`:

```rust
/// Review Focus 3: a listing shows exactly today's reason for each way the
/// file can be unusable.
#[test]
fn read_errors_keep_their_text() -> Result<(), Box<dyn std::error::Error>> {
    use tenuto::feed::error::FeedError;

    let reason = |path: &PathBuf| match store(path.clone()).read_snapshot() {
        Err(FeedError::SubscriptionsUnreadable { reason }) => reason,
        other => panic!("expected SubscriptionsUnreadable, got {other:?}"),
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");

    fs::create_dir(&path)?;
    assert!(
        reason(&path).starts_with("cannot read subscriptions file: "),
        "{}",
        reason(&path)
    );
    fs::remove_dir(&path)?;

    let mut newer = valid_file();
    newer["schema_version"] = json!(2);
    fs::write(&path, serde_json::to_vec(&newer)?)?;
    assert_eq!(
        reason(&path),
        "subscriptions file is schema version 2, and this build supports 1"
    );

    fs::write(&path, b"broken")?;
    assert_eq!(
        reason(&path),
        "subscriptions file is malformed (syntax error at line 1, column 1)"
    );

    let mut invalid = valid_file();
    invalid["subscriptions"][0]["slug"] = json!("Not A Slug");
    fs::write(&path, serde_json::to_vec(&invalid)?)?;
    assert_eq!(reason(&path), "a slug is not valid");
    Ok(())
}
```

Append to `tests/m7_1_station_store.rs`:

```rust
/// Review Focus 3, for stations.
#[test]
fn read_errors_keep_their_text() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let path = dir.path().join("stations.json");
    let reason = || match StationStore::new(path.clone(), clock()).read_snapshot() {
        Err(FeedError::StationsUnreadable { reason }) => reason,
        other => panic!("expected StationsUnreadable, got {other:?}"),
    };

    std::fs::create_dir(&path).unwrap_or_else(|error| panic!("{error}"));
    assert!(reason().starts_with("cannot read stations file: "), "{}", reason());
    std::fs::remove_dir(&path).unwrap_or_else(|error| panic!("{error}"));

    std::fs::write(&path, br#"{"schema_version":2,"stations":[]}"#)
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        reason(),
        "stations file is schema version 2, and this build supports 1"
    );

    std::fs::write(&path, b"broken").unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        reason(),
        "stations file is malformed (syntax error at line 1, column 1)"
    );

    let invalid = serde_json::json!({
        "schema_version": 1,
        "stations": [{
            "slug": "Not A Slug",
            "url": "https://radio.example/live",
            "media": "remote:https://radio.example/live",
            "added_at": "1970-01-01T00:00:00Z"
        }]
    });
    std::fs::write(&path, serde_json::to_vec(&invalid).unwrap_or_else(|error| panic!("{error}")))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(reason(), "a slug is not valid");
}
```

Run: `cargo test --locked --test m4_subscription_store --test m7_1_station_store read_errors_keep_their_text`
Expected: both PASS on today's code. They are characterization tests. `media` is `MediaId::to_string()`'s `remote:<url>` form (`src/media/id.rs:146`), so the only thing invalid in that record is the slug. If the station case reports a different sentence, print the file `StationStore::save` writes for the same URL and copy its `media` value.

- [ ] **Step 2: Write the failing serialize-failure tests**

Append to `tests/m4_subscription_store.rs`:

```rust
/// Spec §2: a public snapshot can carry a timestamp RFC 3339 cannot write.
/// The save fails as a persistence error and the file on disk is untouched.
#[test]
fn a_save_that_cannot_serialize_leaves_the_file_as_it_was()
-> Result<(), Box<dyn std::error::Error>> {
    use tenuto::feed::error::FeedError;
    use tenuto::persistence::PersistenceError;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    fs::write(&path, serde_json::to_vec(&valid_file())?)?;
    let before = fs::read(&path)?;
    let store = store(path.clone());
    let mut snapshot = store.read_snapshot()?;
    snapshot.subscriptions[0].added_at = time::Date::from_calendar_date(-1, time::Month::January, 1)?
        .midnight()
        .assume_utc();

    let result = store.save(&snapshot);

    assert!(
        matches!(
            result,
            Err(FeedError::Persistence(PersistenceError::Serialize { .. }))
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}
```

Append to `tests/m7_1_station_store.rs`:

```rust
/// Spec §2, for stations.
#[test]
fn a_save_that_cannot_serialize_leaves_the_file_as_it_was() {
    use tenuto::persistence::PersistenceError;

    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let path = dir.path().join("stations.json");
    let store = StationStore::new(path.clone(), clock());
    let mut snapshot = StationSnapshot {
        stations: vec![station("lofi", "https://radio.example/live", None)],
    };
    store
        .save(&snapshot)
        .unwrap_or_else(|error| panic!("{error}"));
    let before = std::fs::read(&path).unwrap_or_else(|error| panic!("{error}"));
    snapshot.stations[0].added_at = time::Date::from_calendar_date(-1, time::Month::January, 1)
        .unwrap_or_else(|error| panic!("{error}"))
        .midnight()
        .assume_utc();

    let result = store.save(&snapshot);

    assert!(
        matches!(
            result,
            Err(FeedError::Persistence(PersistenceError::Serialize { .. }))
        ),
        "{result:?}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap_or_else(|error| panic!("{error}")),
        before
    );
}
```

Run: `cargo test --locked --test m4_subscription_store --test m7_1_station_store a_save_that_cannot_serialize`
Expected: both FAIL. The error is `SubscriptionsUnreadable` / `StationsUnreadable { reason: "cannot serialize …" }`.

- [ ] **Step 3: Extend `json_file`**

In `src/persistence/json_file.rs`:

Add the variant to `Malformed`, after `Json { .. }`:

```rust
    /// A record failed the store's own validation. The text is a fixed
    /// sentence chosen by the store, never taken from the file.
    Invalid(&'static str),
```

Add after the `Malformed` enum:

```rust
impl Malformed {
    /// The sentence a store shows for this rejection: today's
    /// `"<file> is malformed (<category> error at line L, column C)"`, or
    /// the validation sentence as it stands.
    pub fn reason(&self, file: &str) -> String {
        match *self {
            Self::Json {
                category,
                line,
                column,
            } => format!("{file} is malformed ({category} error at line {line}, column {column})"),
            Self::Invalid(what) => what.to_string(),
        }
    }
}
```

Add after the `Rejected` enum:

```rust
impl Rejected {
    /// The reason a read-only listing shows, worded as the subscription and
    /// station stores always have.
    pub fn reason(&self, file: &str, supported: u32) -> String {
        match self {
            Self::Unreadable(error) => format!("cannot read {file}: {error}"),
            Self::UnsupportedVersion(found) => {
                format!("{file} is schema version {found}, and this build supports {supported}")
            }
            Self::Malformed(malformed) => malformed.reason(file),
        }
    }
}
```

Add to `impl JsonFile`, after `path()`:

```rust
    pub fn now(&self) -> OffsetDateTime {
        self.clock.sample().wall
    }
```

Add the arm to `log_malformed`'s `match`:

```rust
        Malformed::Invalid(reason) => {
            tracing::warn!(path = ?path, reason, "the file is malformed");
        }
```

Add these two tests to the `tests` module, and add a second decode function beside `decode`:

```rust
    /// Like `decode`, but a zero item fails validation.
    fn decode_nonzero(version: u32, bytes: &[u8]) -> Result<(u32, Vec<u32>), Malformed> {
        let (version, items) = decode(version, bytes)?;
        if items.contains(&0) {
            return Err(Malformed::Invalid("an item is zero"));
        }
        Ok((version, items))
    }

    #[test]
    fn a_record_that_fails_validation_is_malformed_like_a_syntax_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toy.json");
        fs::write(&path, br#"{"schema_version":2,"items":[1,0]}"#).unwrap();
        let file = file_in(dir.path());

        assert!(matches!(
            file.read(BAND, decode_nonzero),
            Err(Rejected::Malformed(Malformed::Invalid("an item is zero")))
        ));
        assert!(path.exists(), "a read never moves the file");

        let load = file.load(BAND, decode_nonzero);
        assert!(matches!(load.reason, LoadReason::Quarantined { .. }));
        assert!(load.writable);
    }

    #[test]
    fn a_rejection_reads_as_the_stores_have_always_worded_it() {
        let unsupported = Rejected::UnsupportedVersion(7);
        assert_eq!(
            unsupported.reason("toy file", 2),
            "toy file is schema version 7, and this build supports 2"
        );
        let json = Rejected::Malformed(Malformed::Json {
            category: "syntax",
            line: 3,
            column: 9,
        });
        assert_eq!(
            json.reason("toy file", 2),
            "toy file is malformed (syntax error at line 3, column 9)"
        );
        assert_eq!(
            Rejected::Malformed(Malformed::Invalid("an item is zero")).reason("toy file", 2),
            "an item is zero"
        );
    }
```

In `src/persistence/store.rs`'s `read_snapshot`, add the last arm after the `Malformed::Json` arm:

```rust
            // State decode only ever builds `Malformed::Json`; this arm keeps
            // the match exhaustive with static text, not a new variant.
            Err(Rejected::Malformed(Malformed::Invalid(what))) => {
                Err(PersistenceError::Deserialize {
                    path: path.to_path_buf(),
                    category: what,
                    line: 0,
                    column: 0,
                })
            }
```

- [ ] **Step 4: Move `SubscriptionStore` onto `JsonFile`**

In `src/subscription/store.rs`:

1. Replace the imports (lines 18–34) with:

```rust
use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::clock::Clock;
use crate::feed::error::FeedError;
use crate::media::id::NormalizedUrl;
use crate::persistence::json_file::{self, JsonFile, Load, Malformed};
use crate::persistence::store::LoadReason;

use super::model::{Subscription, validate_feed_id, validate_slug};
```

2. After `const SCHEMA_VERSION: u32 = 1;`, add:

```rust
const VERSIONS: RangeInclusive<u32> = SCHEMA_VERSION..=SCHEMA_VERSION;

/// How every reason this store reports names its file.
const FILE: &str = "subscriptions file";
```

3. Delete `struct VersionEnvelope`, `enum DecodeError` (with its doc comment), `fn decode_subscriptions` and `fn malformed`, then add:

```rust
/// The full shape, then §5.6's semantic validation, once the shared
/// lifecycle has checked the version. Touches no filesystem.
fn decode(_version: u32, bytes: &[u8]) -> Result<SubscriptionSnapshot, Malformed> {
    let file = serde_json::from_slice::<SubscriptionFile>(bytes)
        .map_err(|source| json_file::malformed(&source))?;
    validate_records(file.subscriptions).map(|subscriptions| SubscriptionSnapshot { subscriptions })
}

fn empty() -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        subscriptions: Vec::new(),
    }
}
```

4. In `validate_records`, change the return type to `Result<Vec<Subscription>, Malformed>`. Then replace each `DecodeError::Malformed("<sentence>".to_string())` with `Malformed::Invalid("<sentence>")`, keeping every sentence exactly as it is. There are five: `a feed id is not valid`, `a feed id is used by more than one subscription`, `a slug is not valid`, `a slug is used by more than one subscription`, and `a fetch_url is not http(s)`.

5. Replace `struct SubscriptionStore` and its whole `impl`, keeping each method's doc comment from today, with:

```rust
pub struct SubscriptionStore {
    file: JsonFile,
}

impl SubscriptionStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self {
            file: JsonFile::new(path, clock),
        }
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn now(&self) -> OffsetDateTime {
        self.file.now()
    }

    pub fn read_snapshot(&self) -> Result<SubscriptionSnapshot, FeedError> {
        match self.file.read(VERSIONS, decode) {
            Ok(snapshot) => Ok(snapshot.unwrap_or_else(empty)),
            Err(rejected) => Err(FeedError::SubscriptionsUnreadable {
                reason: rejected.reason(FILE, SCHEMA_VERSION),
            }),
        }
    }

    pub fn load(&self) -> SubscriptionLoad {
        let Load {
            found,
            writable,
            reason,
        } = self.file.load(VERSIONS, decode);
        SubscriptionLoad {
            snapshot: found.map_or_else(empty, |found| found.value),
            writable,
            reason,
        }
    }

    pub fn save(&self, snapshot: &SubscriptionSnapshot) -> Result<(), FeedError> {
        let records: Vec<SubscriptionRecord> =
            snapshot.subscriptions.iter().map(to_record).collect();

        // Re-runs exactly the load-side validation over the records about to
        // be written, so a `SubscriptionSnapshot` built by hand (rather than
        // decoded from disk) cannot write invalid data just because it
        // skipped `read_snapshot`/`load`.
        validate_records(records.clone()).map_err(|malformed| {
            FeedError::SubscriptionsUnreadable {
                reason: malformed.reason(FILE),
            }
        })?;

        self.file.save(&SubscriptionFile {
            schema_version: SCHEMA_VERSION,
            subscriptions: records,
        })?;
        Ok(())
    }
}
```

6. Delete `parent`, `fresh`, `reject_malformed`, `quarantine` and the file's `fn stamp`. In the module doc comment, change "It applies `StateStore::load`'s policy" to "It applies the shared versioned-file policy".

- [ ] **Step 5: Move `StationStore` onto `JsonFile`**

In `src/station/store.rs`, apply the same change as Step 4, with these names:
- **Imports:** same as Step 4, except keep `use crate::http::source::StationIdentity;`, use `use crate::media::id::{MediaId, NormalizedUrl};`, and use `use super::model::{Station, validate_station_slug};`.
- **`FILE`:** `"stations file"`.
- **`decode`:** reads `StationFile` and builds `StationSnapshot { stations }`. `empty()` returns `StationSnapshot { stations: Vec::new() }`.
- **`validate_records`:** returns `Result<Vec<Station>, Malformed>`. Its sentences stay exactly as they are: `a slug is not valid`, `a slug is used by more than one station`, `a url is not http(s)`, `a media identifier is not valid` and `a media identifier does not match its url`.
- **Store struct:** `StationStore { file: JsonFile }`. `read_snapshot` maps to `FeedError::StationsUnreadable`, `load` returns `StationLoad`, and `save` validates (mapping to `StationsUnreadable`), then calls `self.file.save(&StationFile { schema_version: SCHEMA_VERSION, stations: records })?`.
- **Unchanged:** `logo_for` and `is_station` call `self.read_snapshot()` as today.
- **Delete:** `VersionEnvelope`, `DecodeError`, `decode_stations`, `malformed`, `parent`, `fresh`, `reject_malformed`, `quarantine` and `stamp`.

- [ ] **Step 6: Run the new and pinned tests**

Run: `cargo test --locked --test m4_subscription_store --test m7_1_station_store`, then `cargo test --locked --lib persistence::json_file`
Expected: all pass, including `read_errors_keep_their_text` (unchanged text) and `a_save_that_cannot_serialize_leaves_the_file_as_it_was` (now green).

- [ ] **Step 7: Shrink the two store suites to record validation plus wiring**

In `tests/m4_subscription_store.rs`, delete these tests, which the lifecycle suite now owns:
- `a_missing_read_creates_no_parent_directory`
- `unsupported_schema_version_is_preserved_not_quarantined`
- `a_quarantine_name_collision_is_suffixed_rather_than_clobbered`
- `a_quarantine_with_every_candidate_taken_disables_writing`
- `a_directory_at_the_path_is_unreadable`

Replace `only_mutating_load_quarantines` with this wiring test:

```rust
/// Wiring: the store hands the shared lifecycle its own file name and its
/// one version. The lifecycle itself is tested in `persistence::json_file`.
#[test]
fn only_mutating_load_quarantines() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let store = store(path.clone());

    for version in [0, 2] {
        let mut file = valid_file();
        file["schema_version"] = json!(version);
        fs::write(&path, serde_json::to_vec(&file)?)?;
        let result = store.load();
        assert!(
            matches!(result.reason, LoadReason::UnsupportedVersion { found } if found == version),
            "version {version}: {:?}",
            result.reason
        );
    }

    fs::write(&path, b"broken")?;
    assert!(store.read_snapshot().is_err());
    assert_eq!(fs::read(&path)?, b"broken");
    let result = store.load();
    let LoadReason::Quarantined { moved_to } = result.reason else {
        panic!("expected quarantine")
    };
    assert_eq!(
        moved_to.file_name().and_then(|name| name.to_str()),
        Some(format!("subscriptions.json.rejected-{EPOCH_STAMP}").as_str())
    );
    assert_eq!(fs::read(moved_to)?, b"broken");
    Ok(())
}
```

Keep `assert_malformed` and the §5.6 rejection matrix: they are record validation. Remove the `MAX_QUARANTINE_CANDIDATES` and `BTreeSet` imports if they are now unused.

In `tests/m7_1_station_store.rs`, delete `read_snapshot_reports_a_malformed_file_and_never_quarantines_it` and `load_quarantines_a_malformed_file_and_starts_empty`, and add:

```rust
/// Wiring: the store hands the shared lifecycle its own file name and its
/// one version. The lifecycle itself is tested in `persistence::json_file`.
#[test]
fn the_store_wires_its_file_name_and_version() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let path = dir.path().join("stations.json");
    let store = StationStore::new(path.clone(), clock());

    for version in [0u32, 2] {
        std::fs::write(&path, format!(r#"{{"schema_version":{version},"stations":[]}}"#))
            .unwrap_or_else(|error| panic!("{error}"));
        let load = store.load();
        assert!(
            matches!(load.reason, LoadReason::UnsupportedVersion { found } if found == version),
            "version {version}: {:?}",
            load.reason
        );
    }

    std::fs::write(&path, b"{ not json").unwrap_or_else(|error| panic!("{error}"));
    let load = store.load();
    let LoadReason::Quarantined { moved_to } = load.reason else {
        panic!("expected a quarantine, got {:?}", load.reason);
    };
    assert_eq!(
        moved_to.file_name().and_then(|name| name.to_str()),
        Some("stations.json.rejected-19700101T000000Z")
    );
    assert!(load.snapshot.stations.is_empty());
    assert!(load.writable);
}
```

Fix the doc comment on `a_media_that_disagrees_with_its_url_is_malformed`, which mentions the deleted `load_quarantines_a_malformed_file_and_starts_empty`. It now refers to "any other malformed file".

- [ ] **Step 8: Run the gate**

Run the three gate commands. Expected: all pass. `tests/m4_library_mutations.rs`, `tests/m6_feed_management.rs`, `tests/m7_1_station_probe.rs` and `tests/m4_diagnostics.rs` exercise these stores through `library`.

- [ ] **Step 9: Commit**

```bash
git add src/persistence src/subscription/store.rs src/station/store.rs tests/m4_subscription_store.rs tests/m7_1_station_store.rs
git commit -m "refactor(m9): subscriptions.json and stations.json share the file lifecycle"
```

---

### Task 3: One refusal for a store that cannot be written

**Files:**
- Modify: `src/library.rs:380-480` (`load_mutating`, `load_mutating_stations`), the module doc at `:16-20`, and a new unit-test module at the end of the file

**Interfaces:**
- Consumes: `LoadReason`, `SubscriptionLoad`, `StationLoad`.
- Produces: the private `fn writable<T>(snapshot: T, writable: bool, reason: LoadReason, file: &str, consequence: &str) -> Result<T, String>`. `load_mutating` and `load_mutating_stations` keep their names and signatures, and the seven call sites stay as they are.

- [ ] **Step 1: Write the failing table test**

Append to `src/library.rs` (if a `#[cfg(test)] mod tests` already exists at the end, add to it instead):

```rust
#[cfg(test)]
mod refusal_tests {
    use std::path::PathBuf;

    use super::{LoadReason, writable};

    const FILE: &str = "toy file";
    const CONSEQUENCE: &str = "nothing was changed";

    fn refusal(writable_flag: bool, reason: LoadReason) -> String {
        writable((), writable_flag, reason, FILE, CONSEQUENCE).unwrap_err()
    }

    #[test]
    fn only_a_writable_loaded_or_missing_file_lets_a_mutation_proceed() {
        assert!(writable(7, true, LoadReason::Loaded, FILE, CONSEQUENCE) == Ok(7));
        assert!(writable(7, true, LoadReason::Missing, FILE, CONSEQUENCE) == Ok(7));
        // A quarantine that worked leaves the session writable, and still
        // refuses this mutation.
        let moved_to = PathBuf::from("/x/toy.json.rejected-19700101T000000Z");
        assert_eq!(
            refusal(true, LoadReason::Quarantined { moved_to }),
            "toy file was malformed and has been moved aside to \
             /x/toy.json.rejected-19700101T000000Z; nothing was changed"
        );
    }

    #[test]
    fn every_other_outcome_reads_from_one_template() {
        assert_eq!(
            refusal(false, LoadReason::QuarantineFailed),
            "toy file is malformed and could not be moved aside; nothing was changed"
        );
        assert_eq!(
            refusal(false, LoadReason::Unreadable),
            "toy file could not be read; nothing was changed"
        );
        assert_eq!(
            refusal(false, LoadReason::UnsupportedVersion { found: 9 }),
            "toy file is schema version 9, which this build does not support; nothing was changed"
        );
        assert_eq!(
            refusal(false, LoadReason::Loaded),
            "toy file is not writable; nothing was changed"
        );
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --locked --lib library::refusal_tests`
Expected: compile error, because `writable` is not found in `super`.

- [ ] **Step 3: Replace the two copies with one helper**

In `src/library.rs`, replace `load_mutating` and `load_mutating_stations` and their doc comments (`:380-480`) with:

```rust
/// Narrows a store's `load` to the one outcome a mutation may proceed on: a
/// writable `Loaded` or `Missing`. Every other `LoadReason` becomes a visible
/// refusal, so a mutation never overwrites a file the store chose to
/// preserve. A quarantine that worked leaves the session writable and still
/// refuses this mutation: the user's list just moved aside, and saving an
/// empty one over it now would look like data loss.
fn writable<T>(
    snapshot: T,
    writable: bool,
    reason: LoadReason,
    file: &str,
    consequence: &str,
) -> Result<T, String> {
    if writable && matches!(reason, LoadReason::Loaded | LoadReason::Missing) {
        return Ok(snapshot);
    }
    Err(match reason {
        LoadReason::Quarantined { moved_to } => format!(
            "{file} was malformed and has been moved aside to {}; {consequence}",
            moved_to.display()
        ),
        LoadReason::QuarantineFailed => {
            format!("{file} is malformed and could not be moved aside; {consequence}")
        }
        LoadReason::Unreadable => format!("{file} could not be read; {consequence}"),
        LoadReason::UnsupportedVersion { found } => format!(
            "{file} is schema version {found}, which this build does not support; {consequence}"
        ),
        // Unreachable given the guard above: kept so this match stays
        // exhaustive over every `LoadReason` rather than relying on a
        // wildcard arm to paper over a future variant.
        LoadReason::Loaded | LoadReason::Missing => format!("{file} is not writable; {consequence}"),
    })
}

/// [`writable`] for `subscriptions.json`.
fn load_mutating(subs: &SubscriptionStore) -> Result<SubscriptionSnapshot, FeedError> {
    let SubscriptionLoad {
        snapshot,
        writable: ok,
        reason,
    } = subs.load();
    writable(
        snapshot,
        ok,
        reason,
        "subscriptions file",
        "subscribe, unsubscribe and refresh are unavailable",
    )
    .map_err(|reason| FeedError::SubscriptionsUnreadable { reason })
}

/// [`writable`] for `stations.json` (M7.1 §4).
fn load_mutating_stations(stations: &StationStore) -> Result<StationSnapshot, FeedError> {
    let StationLoad {
        snapshot,
        writable: ok,
        reason,
    } = stations.load();
    writable(snapshot, ok, reason, "stations file", "no station was changed")
        .map_err(|reason| FeedError::StationsUnreadable { reason })
}
```

Update the module doc at `:16-20` if it describes `load_mutating` by its old per-store wording. Keep the comments at `:969` and `:1085`; they still name the right functions.

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test --locked --lib library::refusal_tests`
Expected: 2 passed.

- [ ] **Step 5: Run the gate**

Run the three gate commands. Expected: all pass. No integration test pins the old text (verified with `grep -rn "unavailable until\|no station was changed" tests` before this plan was written). If a test fails on the new text, update the string it expects and mention it in the commit message.

- [ ] **Step 6: Commit**

```bash
git add src/library.rs
git commit -m "refactor(m9): one refusal for a subscriptions or stations file that cannot be written"
```

---

### Task 4: Decode diagnostics reach the log

A regression guard for spec §2's logging line. It passes once Tasks 1–2 are right, and fails if a later change drops a field or leaks content.

**Files:**
- Modify: `tests/m4_diagnostics.rs` (one test at the end)

**Interfaces:**
- Consumes: `process::command_in`, `support::fixture_path`, and the `tracing` fields from Task 1 and Task 2 (`category`, `line`, `column`, `reason`).

- [ ] **Step 1: Write the test**

Append to `tests/m4_diagnostics.rs`:

```rust
/// Spec D §2: a malformed file's log line still says why — the sanitized
/// category, line and column for a JSON error, the validation sentence for a
/// bad record — under the default `tenuto=info` filter, and never the file's
/// content.
#[test]
fn a_malformed_file_logs_why_without_its_content() -> Fallible {
    let root = tempfile::tempdir()?;
    let data = root.path().join("data").join("tenuto");
    let state = root.path().join("state").join("tenuto");
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&state)?;
    let subscriptions = data.join("subscriptions.json");
    let run = |args: &[&str]| -> std::io::Result<String> {
        let output = process::command_in(root.path())
            .args(args)
            .env("TENUTO_AUDIO_OUTPUT", "null")
            .output()?;
        Ok(String::from_utf8_lossy(&output.stderr).into_owned())
    };

    std::fs::write(
        &subscriptions,
        br#"{"schema_version":1,"subscriptions":[{"fetch_url":"https://SECRETHOST/feed""#,
    )?;
    let syntax = run(&["unsubscribe", "radio-t"])?;

    std::fs::write(
        &subscriptions,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "subscriptions": [{
                "feed_id": "9f3c1a7e42b58d0c6f19ab3e5d72c840",
                "slug": "Not A Slug",
                "title": null,
                "fetch_url": "https://SECRETHOST/feed",
                "added_at": "2026-09-11T09:14:22Z"
            }]
        }))?,
    )?;
    let invalid = run(&["unsubscribe", "radio-t"])?;

    std::fs::write(
        state.join("state.json"),
        br#"{"schema_version":4,"checkpoints":{"url:https://SECRETHOST/a":"#,
    )?;
    let fixture = support::fixture_path("sine.wav");
    let play = run(&["play", &fixture.display().to_string()])?;

    for (name, log) in [("syntax", &syntax), ("state", &play)] {
        assert!(
            log.contains("the file is malformed")
                && log.contains("category=")
                && log.contains("line=")
                && log.contains("column="),
            "{name}: the position fields are missing: {log}"
        );
    }
    assert!(
        invalid.contains("the file is malformed") && invalid.contains("reason=\"a slug is not valid\""),
        "the validation reason is missing: {invalid}"
    );
    for log in [&syntax, &invalid, &play] {
        assert!(!log.contains("SECRETHOST"), "file content reached the log: {log}");
    }
    Ok(())
}
```

- [ ] **Step 2: Run it**

Run: `cargo test --locked --test m4_diagnostics a_malformed_file_logs_why_without_its_content`
Expected: PASS. If `reason=` renders without quotes in this `tracing-subscriber` version, match the rendering on stderr (`reason=a slug is not valid`) and keep the assertion specific to the sentence. If it fails for any other reason, the bug is in Task 1's or Task 2's `log_malformed`. Fix it there, not here.

- [ ] **Step 3: Run the gate and commit**

Run the three gate commands. Expected: all pass.

```bash
git add tests/m4_diagnostics.rs
git commit -m "test(m9): a malformed file's log says why, without its content"
```

---

### Task 5: Resume intent lives in `resume.rs`

**Files:**
- Modify: `src/resume.rs`
- Modify: `src/playback/command.rs:10-35` (the enum becomes a re-export)
- Modify: `src/playback/engine.rs:2850-2856` (comments only)
- Modify: `src/session.rs:31,48-57,1533-1598`
- Modify: `tests/support/mod.rs` (two helpers)
- Modify: `tests/resume_decision.rs`, `tests/resume_contract.rs`, `tests/estimated_resume.rs`, `tests/http_resume.rs`, `tests/http_playback.rs`, `tests/session_policy.rs`

**Interfaces:**
- Consumes: nothing from Tasks 1–4.
- Produces:
  - `tenuto::resume::ResumeIntent` (moved; also at `tenuto::playback::command::ResumeIntent`).
  - `tenuto::resume::StoredProgress { position: Option<Duration>, estimated: Option<Duration>, completed: bool }` (`Clone, Copy, Debug, Eq, PartialEq`).
  - `tenuto::resume::resume_intent(media: &MediaId, stored: Option<StoredProgress>) -> ResumeIntent`.
  - Test support: `support::podcast_media() -> MediaId` and `support::established_candidate(&PersistedCheckpoint) -> ResumeCandidate`.
  - Removed from the public surface: `session::resume_intent_for`, `resume::resume_candidate`, `resume::restart_preference` and `resume::RestartPreference`.

- [ ] **Step 1: Write the failing intent tests**

In `tests/resume_decision.rs`, replace the `use tenuto::resume::{…}` import with:

```rust
use tenuto::media::id::{EpisodeKey, FeedId, MediaId};
use tenuto::resume::{
    KnownDuration, ResumeCandidate, ResumeDecision, ResumeIntent, StoredProgress, decide_resume,
    resume_intent,
};
```

Add these helpers after `stored`:

```rust
/// The one kind of media a fresh load resumes (M8 §7), so these tests see
/// the stored-progress rules rather than the media policy.
fn episode() -> MediaId {
    let enclosure = url::Url::parse("https://cdn.example/a.mp3").unwrap();
    MediaId::PodcastEpisode {
        feed: FeedId::new("subscription-1".into()).unwrap(),
        episode: EpisodeKey::resolve(Some("guid"), Some(&enclosure), None).unwrap(),
    }
}

fn progress(position: Option<u64>, estimated: Option<u64>, completed: bool) -> StoredProgress {
    StoredProgress {
        position: position.map(Duration::from_secs),
        estimated: estimated.map(Duration::from_secs),
        completed,
    }
}

fn intent(stored: StoredProgress) -> ResumeIntent {
    resume_intent(&episode(), Some(stored))
}
```

Delete the seven tests in the `resume_candidate_*` and `restart_preference_*` group (from `resume_candidate_with_no_established_position_and_not_completed_is_no_candidate` through `restart_preference_with_neither_location_is_none`). Replace them with these, keeping each old test's doc comment on its counterpart where one is given:

```rust
/// §4.2: an entry carrying only an estimate has no established candidate;
/// with no estimate either, nothing is stored to resume.
#[test]
fn resume_intent_with_nothing_established_and_no_estimate_starts_at_zero() {
    assert_eq!(intent(progress(None, None, false)), ResumeIntent::StartAt(Duration::ZERO));
    assert_eq!(resume_intent(&episode(), None), ResumeIntent::StartAt(Duration::ZERO));
}

/// Review Focus 4: established at the start is a candidate, distinct from
/// never established — `decide_resume` reads it as `AtStart`.
#[test]
fn resume_intent_keeps_a_position_established_at_zero_as_a_candidate() {
    assert_eq!(
        intent(progress(Some(0), None, false)),
        ResumeIntent::Candidate(stored(0, false))
    );
}

/// §4.2: an estimated timeline can complete a track without ever
/// establishing its anchor, so completion is still reported with no
/// established position.
#[test]
fn resume_intent_reports_completion_even_with_no_established_position() {
    assert_eq!(
        intent(progress(None, None, true)),
        ResumeIntent::Candidate(stored(0, true))
    );
    assert_eq!(
        decide_resume(Some(stored(0, true)), secs(300)),
        ResumeDecision::Completed
    );
}

#[test]
fn resume_intent_with_an_established_position_carries_it_through() {
    assert_eq!(
        intent(progress(Some(93), None, false)),
        ResumeIntent::Candidate(stored(93, false))
    );
}

/// §4.3: `estimated` is the listener's most recent expressed intent and
/// wins over an older established point, which is kept as the fallback.
#[test]
fn resume_intent_prefers_the_estimate_and_keeps_the_established_fallback() {
    assert_eq!(
        intent(progress(Some(40), Some(97), false)),
        ResumeIntent::EstimatedCandidate {
            target: Duration::from_secs(97),
            established: Some(Duration::from_secs(40)),
        }
    );
}

/// R8 / Review Focus 4: an entry that only ever carried an estimate reports
/// `established: None`, never a fabricated zero.
#[test]
fn resume_intent_with_no_established_fallback_reports_none_for_it() {
    assert_eq!(
        intent(progress(None, Some(97), false)),
        ResumeIntent::EstimatedCandidate {
            target: Duration::from_secs(97),
            established: None,
        }
    );
}

/// Review Focus 5: D1 replays a completed entry from zero, so a stray
/// estimate stored before completion never redirects it.
#[test]
fn resume_intent_completion_beats_an_estimate() {
    assert_eq!(
        intent(progress(Some(280), Some(97), true)),
        ResumeIntent::Candidate(stored(280, true))
    );
}

/// Review Focus 5, M8 §7: a local file or a plain URL never resumes on a
/// fresh load, whatever is stored — finished, part-way or estimated.
#[test]
fn resume_intent_never_resumes_a_local_file_or_url() {
    let local = MediaId::LocalFile(
        tenuto::media::id::AbsolutePath::new("/music/a.flac".into()).unwrap(),
    );
    let url = MediaId::RemoteUrl(
        tenuto::media::id::NormalizedUrl::parse("https://radio.example/a.mp3").unwrap(),
    );
    for media in [local, url] {
        for stored in [
            progress(Some(93), None, false),
            progress(Some(280), None, true),
            progress(Some(40), Some(97), false),
        ] {
            assert_eq!(
                resume_intent(&media, Some(stored)),
                ResumeIntent::StartAt(Duration::ZERO),
                "{media:?} {stored:?}"
            );
        }
    }
}
```

Run: `cargo test --locked --test resume_decision`
Expected: compile errors, because `ResumeIntent`, `StoredProgress` and `resume_intent` are not found in `tenuto::resume`.

- [ ] **Step 2: Move the type and the function into `resume.rs`**

1. **Move the enum.** Cut `ResumeIntent`, with its doc comment and derive, from `src/playback/command.rs` and paste it into `src/resume.rs` after `ResumeCandidate`. In its `EstimatedCandidate` doc, replace "won the preference over `position` (`resume::restart_preference`)" with "won the preference over `position` ([`resume_intent`])".
2. **Re-export it.** In `src/playback/command.rs`, put `pub use crate::resume::ResumeIntent;` where the enum was. Delete `use crate::resume::ResumeCandidate;` if nothing else in that file uses it.
3. **Add the view and the function.** Append to `src/resume.rs`, after `restart_preference`:

```rust
/// The three stored fields a fresh load's resume reads, without the
/// persistence type that holds them: `resume` never imports `persistence`
/// (G3, `tests/m9_layering.rs`'s `NAMED_RULES`). `session` builds it from a
/// `PersistedCheckpoint`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoredProgress {
    pub position: Option<Duration>,
    pub estimated: Option<Duration>,
    pub completed: bool,
}

/// What a fresh load of `media` asks the worker to do with what is stored
/// for it: §4.3's estimated preference folded in beside the established
/// path, which is unchanged.
///
/// M8 §7 ("Start from zero"): a local file or a plain URL never resumes on a
/// fresh load, whatever is stored — the checkpoint is still written (the
/// queue's "played" marker depends on it), only what a fresh load does with
/// it changes. A fresh-load policy only: pause and Stop → Play are engine
/// commands that never reach this function, so the position contract
/// (`docs/architecture.md` §1) holds unweakened for them. A podcast episode
/// resumes.
///
/// A completed entry never consults the estimate: D1 replays it from zero,
/// and a stray estimate stored before completion must not redirect that.
/// `decide_resume` is not consulted for the estimate either: §4.3 is a
/// preference between two already-known locations, not a duration-validated
/// choice. Nothing stored, or nothing established and no estimate, is a
/// start at zero.
pub fn resume_intent(media: &MediaId, stored: Option<StoredProgress>) -> ResumeIntent {
    let start = ResumeIntent::StartAt(Duration::ZERO);
    if matches!(media, MediaId::LocalFile(_) | MediaId::RemoteUrl(_)) {
        return start;
    }
    let Some(stored) = stored else {
        return start;
    };
    if stored.completed {
        return resume_candidate(stored.position, true).map_or(start, ResumeIntent::Candidate);
    }
    match restart_preference(stored.position, stored.estimated) {
        Some(preference) => ResumeIntent::EstimatedCandidate {
            target: preference.target,
            established: preference.established,
        },
        None => resume_candidate(stored.position, false).map_or(start, ResumeIntent::Candidate),
    }
}
```

4. **Make the helpers private.** Drop `pub` from `fn resume_candidate`, `fn restart_preference` and `struct RestartPreference` (its fields too). Then fix their doc links:
   - Change "``src/app.rs``'s `resume_intent_for` is the production caller" in `restart_preference`'s doc to "[`resume_intent`] is its one caller".
   - Change `ResumeCandidate`'s doc at `:63` so it no longer links the private `resume_candidate` from a public item. Use "built by `resume_intent` from stored progress" in plain backticks.
   - Change the module doc's sentence about `crate::session` to: "`crate::session` hands it a `StoredProgress` built from a stored entry and re-exports `decide_resume` and `ResumeDecision` so the tests that predate this split keep their import paths (Ruling 4)".
5. **Fix the engine comments.** In `src/playback/engine.rs:2850-2856`, change the two comment mentions of `resume::restart_preference` / `restart_preference` to `resume::resume_intent`. These are comments only.

- [ ] **Step 3: Make `Session` a one-line conversion**

In `src/session.rs`:
- **Imports:** delete `:57` `use crate::resume::{restart_preference, resume_candidate};` and add `use crate::resume::{StoredProgress, resume_intent};`. Keep `:56`'s `pub use crate::resume::{ResumeDecision, decide_resume};`.
- **Comment at `:48-51`:** if it names `resume::resume_candidate`, reword it to `resume::resume_intent`.
- **`Session::resume_intent` (`:1533-1539`):** replace it and its doc with:

```rust
    /// What a fresh load of `media` does with this state's stored entry
    /// (`resume::resume_intent`): the conversion from `PersistedCheckpoint`
    /// stops here, so `resume` never sees a persistence type.
    pub fn resume_intent(&self, media: &MediaId) -> ResumeIntent {
        resume_intent(
            media,
            self.state.entry_for(media).map(|entry| StoredProgress {
                position: entry.position,
                estimated: entry.estimated,
                completed: entry.completed,
            }),
        )
    }
```

- **Old free function:** delete `pub fn resume_intent_for` and its doc comment (`:1549-1598`).

Run: `cargo test --locked --test resume_decision --test m5_session_queue --test m8_resume`
Expected: PASS. If `m8_resume` or `m5_session_queue` fails, the new function differs from the old one. Compare branch by branch against the deleted body; don't touch the tests.

- [ ] **Step 4: Add the two test-support helpers**

Append to `tests/support/mod.rs`:

```rust
/// A podcast episode, the one kind of media a fresh load resumes (M8 §7).
pub fn podcast_media() -> MediaId {
    let enclosure = match Url::parse("https://cdn.example/episode.mp3") {
        Ok(url) => url,
        Err(error) => panic!("a literal URL must parse: {error}"),
    };
    let feed = match tenuto::media::id::FeedId::new("subscription-1".into()) {
        Ok(feed) => feed,
        Err(error) => panic!("a literal feed id must parse: {error:?}"),
    };
    match tenuto::media::id::EpisodeKey::resolve(Some("guid"), Some(&enclosure), None) {
        Ok(episode) => MediaId::PodcastEpisode { feed, episode },
        Err(error) => panic!("a literal episode key must resolve: {error:?}"),
    }
}

/// The candidate a stored, established entry hands the engine, built field
/// by field. For tests of the engine's own resume decision: they bypass the
/// fresh-load media policy (`resume::resume_intent`) on purpose, since their
/// fixtures are local files and plain URLs that policy never resumes.
pub fn established_candidate(
    entry: &tenuto::persistence::model::PersistedCheckpoint,
) -> tenuto::resume::ResumeCandidate {
    match entry.position {
        Some(position) => tenuto::resume::ResumeCandidate {
            position,
            completed: entry.completed,
        },
        None => panic!("the stored entry must carry an established position"),
    }
}
```

If `FeedId::new` or `EpisodeKey::resolve` returns an error type without `Debug`, use `{error}` instead.

- [ ] **Step 5: Migrate the engine-decision call sites**

These tests drive the engine or `decide_resume` directly, on local or URL fixtures. They get the literal candidate and keep their media. Add this comment once above the first changed line in each file:

```rust
// The fixture is a local file or a plain URL, which a fresh load never
// resumes (`resume::resume_intent`); this hands the engine the stored
// candidate directly, to test the engine's own resume decision.
```

Make these replacements:
- **`tests/http_resume.rs` (4 sites), `tests/http_playback.rs` (1) and `tests/estimated_resume.rs:231,556`:** replace `resume_candidate(X.position, X.completed).expect(…)` or `.unwrap_or_else(…)` with `support::established_candidate(&X)`.
- **`tests/resume_contract.rs:205,385,525`:** replace `.and_then(|entry| resume_candidate(entry.position, entry.completed))` with `.map(support::established_candidate)`. Use `.map(|entry| support::established_candidate(entry))` if the iterator yields `&&PersistedCheckpoint`.
- **`tests/resume_contract.rs:442`:** replace `resume_candidate(kept.position, kept.completed)` with `Some(support::established_candidate(&kept))`.
- **`tests/session_policy.rs:703`:** replace `resume_candidate(entry.position, entry.completed)` with `Some(support::established_candidate(entry))`.
- **`tests/estimated_resume.rs:288` and `:437`:** these feed the engine an `EstimatedCandidate` on a URL fixture.
  - Replace `let preference = restart_preference(X.position, X.estimated).unwrap_or_else(…);` with:

    ```rust
    let target = X
        .estimated
        .unwrap_or_else(|| panic!("session 1 must have left an estimate"));
    ```
  - Change `assert_eq!(preference.established, …)` to `assert_eq!(X.position, …)`.
  - In the `ResumeIntent::EstimatedCandidate { target: preference.target, established: preference.established }` they build, use `target` and `established: X.position`.
  - Update the comment above each to say it hands the engine the stored estimate directly. Drop the phrase "`resume_intent_for` no longer resumes".

Remove the now-unused `resume_candidate` / `restart_preference` imports from each file. Every file listed already declares `mod support;`.

- [ ] **Step 6: Migrate the stored-progress round trips**

`tests/resume_contract.rs:575` and `:609` check that a stored estimate survives a reload and still yields the preference. That is a stored-progress selection, so they go through `resume_intent` on a podcast episode with the reloaded fields:

```rust
    assert_eq!(
        tenuto::resume::resume_intent(
            &support::podcast_media(),
            Some(tenuto::resume::StoredProgress {
                position: entry.position,
                estimated: entry.estimated,
                completed: entry.completed,
            }),
        ),
        ResumeIntent::EstimatedCandidate {
            target: Duration::from_secs(97),
            established: Some(Duration::from_secs(40)),
        }
    );
```

At `:609`, use `established: None`. Then change the file's import to `use tenuto::resume::decide_resume;`, dropping `RestartPreference`, `restart_preference` and `resume_candidate`.

- [ ] **Step 7: Verify nothing still names the removed items**

Run: `grep -rn "resume_intent_for\|restart_preference\|resume_candidate\|RestartPreference" src tests`
Expected: matches only inside `src/resume.rs` (the private helpers and their docs). Rewrite any others.

- [ ] **Step 8: Run the gate, plus docs**

Run the three gate commands, then `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`.
Expected: all pass, and no `private_intra_doc_links` warning. `tests/m9_layering.rs` must pass with `resume` importing only `media`.

- [ ] **Step 9: Commit**

```bash
git add src/resume.rs src/playback/command.rs src/playback/engine.rs src/session.rs tests
git commit -m "refactor(m9): resume intent for a fresh load lives in resume.rs"
```

---

### Task 6: Documentation, parity run, acceptance record

**Files:**
- Modify: `docs/architecture.md` (§4 rows)
- Modify: `docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` (§3 Status lines)
- Modify: `CHANGELOG.md` (`[Unreleased]`)
- Create: `docs/m9-d-acceptance.md`

- [ ] **Step 1: Update `architecture.md` §4**

- **`persistence` row:** change its responsibility from "Read, classify and atomically replace `state.json`. Coalesce writes on one thread." to: "One versioned JSON file lifecycle — read, load for writing, quarantine, save — shared by `state.json`, `subscriptions.json` and `stations.json`. Coalesce `state.json` writes on one thread."
- **`station` row:** change "with the subscription store's read, recovery and quarantine policy" to "with the shared file lifecycle".
- **`playlist` (…), `resume` row:** add a sentence: "`resume` owns the fresh-load resume intent (`resume_intent`), from the stored progress `session` hands it."

- [ ] **Step 2: Mark the roadmap items shipped**

In the deepening spec §3, add a Status line under "One versioned JSON file module." and another under "Resume intent lives in `resume.rs`.", in the same form as the M9.2 Status lines there:

```markdown
- Status: shipped through M9 PR D (`docs/m9-d-acceptance.md`). `persistence::json_file` holds the lifecycle; the three stores keep their record shape and validation.
```

```markdown
- Status: shipped through M9 PR D. `ResumeIntent` and `resume_intent` live in `resume.rs`, which takes a persistence-free `StoredProgress`; `resume_candidate` and `restart_preference` are private.
```

- [ ] **Step 3: CHANGELOG**

Under `## [Unreleased]`, append to `### Changed` (hard-wrapped at 78 columns like the entries around it):

```markdown
- When `subscriptions.json` or `stations.json` cannot be written because it
  is malformed, unreadable or from a newer build, the message now reads the
  same way for both files.
```

Append to `### Internal`:

```markdown
- `state.json`, `subscriptions.json` and `stations.json` share one read,
  recovery and save path. `ResumeIntent` moves to `resume` (still exported
  from `playback::command`); `session::resume_intent_for`,
  `resume::resume_candidate`, `resume::restart_preference` and
  `RestartPreference` leave the library's public surface.
```

- [ ] **Step 4: Scoped duplicate scan (spec §7)**

Run:

```bash
grep -rn "fn stamp\|struct VersionEnvelope\|const MAX_QUARANTINE_CANDIDATES\|rejected-" \
  src/persistence src/subscription/store.rs src/station/store.rs
```

Expected:
- One `fn stamp`, one `struct VersionEnvelope` and one `const MAX_QUARANTINE_CANDIDATES`, all in `src/persistence/json_file.rs`.
- `rejected-` appears only in `json_file.rs` doc comments and tests, plus the `"rejected"` tag passed to `candidates`. The format string is built once, in `candidates`.

`src/feed/cache.rs`'s own `VersionEnvelope` is out of scope (spec §8) and outside the scanned paths.

- [ ] **Step 5: CLI parity against `main`**

Write this script outside the repository (for example in `$(mktemp -d)`). Do not commit it.

```bash
#!/usr/bin/env bash
# M9 PR D parity: main vs branch over identical seeded profiles.
set -euo pipefail
REPO=$(git -C "${1:?repo}" rev-parse --show-toplevel)
WORK=$(mktemp -d)
git -C "$REPO" worktree add -q "$WORK/main" main
(cd "$WORK/main" && cargo build --locked --release -q)
(cd "$REPO" && cargo build --locked --release -q)
BIN_main="$WORK/main/target/release/tenuto"
BIN_branch="$REPO/target/release/tenuto"
WAV="$REPO/tests/fixtures/sine.wav"

python3 -m http.server 0 --bind 127.0.0.1 --directory "$REPO/tests/fixtures/feeds" \
  >"$WORK/http.log" 2>&1 &
HTTP=$!; trap 'kill $HTTP; git -C "$REPO" worktree remove --force "$WORK/main"' EXIT
sleep 1; PORT=$(grep -o 'port [0-9]*' "$WORK/http.log" | head -1 | cut -d' ' -f2)
FEED="http://127.0.0.1:$PORT/rss2-minimal.xml"

seed() { # $1 = profile root, $2 = case
  mkdir -p "$1/data/tenuto" "$1/state/tenuto" "$1/cache" "$1/config"
  local subs="$1/data/tenuto/subscriptions.json" state="$1/state/tenuto/state.json"
  case "$2" in
    subs-valid)       ;;
    subs-malformed)   printf 'broken' >"$subs" ;;
    subs-unsupported) printf '{"schema_version":2,"subscriptions":[]}' >"$subs" ;;
    subs-unreadable)  mkdir "$subs" ;;
    state-valid)      ;;
    state-malformed)  printf '{ not json' >"$state" ;;
    state-unsupported) printf '{"schema_version":5,"checkpoints":{}}' >"$state" ;;
    state-unreadable) mkdir "$state" ;;
    state-repair)     python3 - "$state" <<'PY'
import json, sys
q = [{"id": i, "media": f"local:/music/t{i}.flac",
      "source": {"kind": "local", "path": f"/music/t{i}.flac"}} for i in range(1, 5001)]
json.dump({"schema_version": 3, "current_media": None, "volume": 0.3, "checkpoints": {},
           "queue": q, "active_entry": 1}, open(sys.argv[1], "w"), indent=2)
PY
    ;;
  esac
}

commands() { # $1 = case
  case "$1" in
    subs-*)  echo "feeds"; echo "subscribe $FEED --as alpha"; echo "episodes alpha"
             echo "unsubscribe alpha" ;;
    state-*) echo "play $WAV" ;;
  esac
}

for side in main branch; do
  bin_var="BIN_$side"; bin=${!bin_var}
  for case in subs-valid subs-malformed subs-unsupported subs-unreadable \
              state-valid state-malformed state-unsupported state-unreadable state-repair; do
    root="$WORK/$side/$case"; out="$WORK/out/$side/$case"; mkdir -p "$out"
    seed "$root" "$case"; cp -a "$root" "$out/seeded"
    n=0
    while read -r cmd; do
      n=$((n+1))
      # shellcheck disable=SC2086
      env XDG_STATE_HOME="$root/state" XDG_DATA_HOME="$root/data" \
          XDG_CACHE_HOME="$root/cache" XDG_CONFIG_HOME="$root/config" \
          TENUTO_AUDIO_OUTPUT=null "$bin" $cmd >"$out/$n.stdout" 2>"$out/$n.stderr" \
        && echo 0 >"$out/$n.status" || echo $? >"$out/$n.status"
    done < <(commands "$case")
    cp -a "$root" "$out/after"
  done
done

python3 - "$WORK/out" <<'PY'
import json, pathlib, re, sys
out = pathlib.Path(sys.argv[1])
ids = {}
def norm(text, root):
    text = text.replace(str(root), "<root>")
    text = re.sub(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z", "<ts>", text)
    text = re.sub(r"\d{8}T\d{6}Z", "<stamp>", text)
    return re.sub(r"\b[0-9a-f]{32}\b", lambda m: ids.setdefault(m.group(0), f"<feed-{len(ids)+1}>"), text)
def tree(root):
    files = {}
    for p in sorted(root.rglob("*")):
        rel = norm(str(p.relative_to(root)), root)
        if p.is_dir():
            files[rel] = "<dir>"; continue
        raw = p.read_bytes()
        try:
            files[rel] = json.loads(norm(raw.decode(), root))
        except (UnicodeDecodeError, ValueError):
            files[rel] = raw
    return files
failed = False
for case in sorted((out / "main").iterdir()):
    other = out / "branch" / case.name
    for f in sorted(case.glob("*.st*")):
        ids.clear(); a = norm(f.read_text(), out / "main" / case.name)
        ids.clear(); b = norm((other / f.name).read_text(), out / "branch" / case.name)
        if a != b:
            failed = True
            print(f"--- {case.name}/{f.name}\nmain:   {a!r}\nbranch: {b!r}")
    ids.clear(); ta = tree(case / "after")
    ids.clear(); tb = tree(other / "after")
    if ta != tb:
        failed = True
        print(f"--- {case.name}: trees differ\nmain:   {sorted(ta)}\nbranch: {sorted(tb)}")
    # Preserved originals, raw bytes: a quarantine or recovery copy, or a
    # file left in place, must equal the seeded file exactly.
    for side in (case, other):
        for kept in sorted((side / "after").rglob("*")):
            name = kept.name
            for marker in (".rejected-", ".queue-recovery-"):
                if marker in name:
                    seed = side / "seeded" / kept.relative_to(side / "after").parent / name.split(marker)[0]
                    same = kept.read_bytes() == seed.read_bytes()
                    print(f"{side.parent.name}/{case.name}: {name} byte-identical to the seed: {same}")
                    failed |= not same
        if case.name.endswith(("-unsupported", "-unreadable")):
            for seed in (side / "seeded").rglob("*.json"):
                kept = side / "after" / seed.relative_to(side / "seeded")
                same = seed.is_file() and kept.read_bytes() == seed.read_bytes()
                print(f"{side.parent.name}/{case.name}: {seed.name} left in place: {same or seed.is_dir()}")
                failed |= not (same or (seed.is_dir() and kept.is_dir()))
sys.exit(1 if failed else 0)
PY
```

Run it: `bash <script> "$PWD"`.

Expected: the only differences printed are the ones spec §7 allows.
- **Refusal text:** the `*.stderr` lines carrying the §4.3 refusal (cases `subs-malformed`, `subs-unsupported`, `subs-unreadable`, on `subscribe` and `unsubscribe`).
- **Warn wording:** the lifecycle `warn!` lines, which are now "cannot read the file …", "unsupported schema …", "the file is malformed", "file quarantined" and "cannot quarantine the file …". These must keep their `path`, `found`, `moved_to`, `category`/`line`/`column` or `reason` fields.
- **Byte-identical originals:** every "byte-identical to the seed" line says `True`.
- **No other difference:** exit statuses and `*.stdout` must match exactly. Any other difference is a defect: fix it in the owning task's code, then rerun.

- [ ] **Step 6: Run the full gate and write the acceptance record**

Run all four gate commands from `CLAUDE.md`:
- `cargo fmt --check`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `cargo test --locked --no-fail-fast`
- `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`

Count the results. Then create `docs/m9-d-acceptance.md` in the shape of `docs/m9-c-acceptance.md`:
- Header: scope (the two M9.1 items of the deepening spec §3), spec and plan paths, branch and base `1db0cea`.
- `## Decisions`: the choices made with the user on 2026-10-11.
  - One template for the refusal text.
  - Privatize the helpers.
  - `writable(snapshot, writable, reason, file, consequence)`.
  - No `SaveError`.
  - Parent sync stays best-effort.
  - The selection/engine split in the resume tests.
  - Log fields retained.
- `## Gate`: a table with the four commands and their results. Include passed/failed/ignored counts against `main`'s 1472/0/3, and name the tests added and deleted.
- `## Layering`: `tests/m9_layering.rs` passes with `ALLOWED` empty.
- `## CLI parity`: how it ran (Step 5), the case table, and every allowed difference quoted once.
- `## Scan`: Step 4's output.

- [ ] **Step 7: Commit**

```bash
git add docs/architecture.md docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md CHANGELOG.md docs/m9-d-acceptance.md
git commit -m "docs(m9): PR D — architecture, roadmap status, changelog, acceptance"
```
