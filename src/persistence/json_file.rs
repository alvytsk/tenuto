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
    /// A record failed the store's own validation. The text is a fixed
    /// sentence chosen by the store, never taken from the file.
    Invalid(&'static str),
}

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

/// What [`JsonFile::read`] reports for a file it cannot hand back.
#[derive(Debug)]
pub enum Rejected {
    Unreadable(io::Error),
    UnsupportedVersion(u32),
    Malformed(Malformed),
}

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

    pub fn now(&self) -> OffsetDateTime {
        self.clock.sample().wall
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
            return fs::rename(&self.path, &candidate)
                .is_ok()
                .then_some(candidate);
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
        Malformed::Invalid(reason) => {
            tracing::warn!(path = ?path, reason, "the file is malformed");
        }
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
        assert!(
            !nested.exists(),
            "a read never creates the parent directory"
        );

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

        assert!(matches!(
            file.read(BAND, decode),
            Err(Rejected::Unreadable(_))
        ));

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
            Err(Rejected::Malformed(Malformed::Json {
                category: "syntax",
                ..
            }))
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
