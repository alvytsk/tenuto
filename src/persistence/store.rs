//! The file on disk: read it, classify it, replace it atomically.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::clock::Clock;
use crate::media::id::MediaId;

use super::PersistenceError;
use super::atomic::replace_bytes;
use super::model::{PersistedCheckpoint, PersistedState, RawState, SCHEMA_VERSION};
use super::queue_codec::QueueReset;

/// How many `-2`, `-3`, … candidates a quarantine will try before giving up.
pub const MAX_QUARANTINE_CANDIDATES: u32 = 100;

/// The oldest schema version `load` still knows how to bring forward to
/// [`SCHEMA_VERSION`] (design doc §4.5). A version below this, like one
/// above [`SCHEMA_VERSION`], is genuinely unreadable rather than migratable:
/// `UnsupportedVersion` and a preserved file either way.
const OLDEST_MIGRATABLE_VERSION: u32 = 1;

/// Only the field every future version is obliged to keep. Read before the
/// model, so that a valid newer file is never misclassified as garbage (D3).
#[derive(Deserialize)]
struct VersionEnvelope {
    schema_version: u32,
}

/// The pure decode step shared by [`StateStore::load`] and
/// [`StateStore::read_snapshot`] (design doc §5.5): read the version envelope
/// first and reject an unsupported version before the full model is ever
/// deserialised, so a valid newer file is never misclassified as garbage
/// (D3). Every other outcome — an unreadable envelope, an unreadable model
/// shape — is reported through the same sanitized [`PersistenceError::Deserialize`],
/// since neither caller may repeat a `serde_json::Error`'s message verbatim: a
/// checkpoint map key is a media identity that can carry a URL.
///
/// Does not touch the filesystem at all: no read, no quarantine, no write.
/// What each caller does with a rejection — quarantine and keep writing,
/// disable writing, or simply return `Err` — is entirely theirs.
///
/// `accept_queue` gates whether the file's queue data — schema 4's
/// `playlists`/`playing`, or schema 3's `queue`/`active_entry` — is decoded
/// at all: [`StateStore::load`] passes `true`, and
/// [`StateStore::read_snapshot`] passes `false`, since a listing never
/// examines queue data (task brief recovery rules 1-2). The queue's own
/// recovery outcome — `Some(QueueReset)` when the queue or active entry had
/// to be reset — is returned alongside the state; [`StateStore::load`] uses
/// it to back up the original file before writing resumes (§6), and
/// [`StateStore::read_snapshot`] discards it, since a read never backs up or
/// writes anything.
fn decode_state(
    path: &Path,
    bytes: &[u8],
    accept_queue: bool,
) -> Result<(PersistedState, Option<QueueReset>), PersistenceError> {
    let envelope = serde_json::from_slice::<VersionEnvelope>(bytes)
        .map_err(|source| malformed(path, &source))?;

    // Not `> SCHEMA_VERSION` alone: a file from a build that renumbered
    // downward is just as unreadable as one from a build ahead of this one.
    // `OLDEST_MIGRATABLE_VERSION` widens the accepted band by exactly the
    // versions this build knows how to bring forward — today only v1 —
    // everything else, above or below, is rejected the same way it always
    // was.
    if envelope.schema_version > SCHEMA_VERSION
        || envelope.schema_version < OLDEST_MIGRATABLE_VERSION
    {
        return Err(PersistenceError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: envelope.schema_version,
            supported: SCHEMA_VERSION,
        });
    }

    let raw =
        serde_json::from_slice::<RawState>(bytes).map_err(|source| malformed(path, &source))?;
    let (mut state, queue_reset) = raw.into_state(accept_queue);

    // A v1 file's shape already deserialises cleanly into the current
    // `PersistedState` (§4.5): `position` lands in `Some`, and the absent
    // `estimated` defaults to `None`. What is missing is the label — without
    // this, the next write would serialise that v2-shaped data back out
    // under a v1 envelope, which the next v1 build would read and quietly
    // discard.
    if envelope.schema_version != SCHEMA_VERSION {
        tracing::info!(
            path = ?path,
            from = envelope.schema_version,
            to = SCHEMA_VERSION,
            "migrating the state file to the current schema"
        );
        state.migrate_to_current_schema();
    }
    Ok((state, queue_reset))
}

/// Sanitizes a `serde_json::Error` into a category plus line/column,
/// deliberately dropping its `Display` text (and any source), since that text
/// can quote the file's raw content — including a checkpoint map key, which is
/// a media identity that may carry a URL.
fn malformed(path: &Path, source: &serde_json::Error) -> PersistenceError {
    use serde_json::error::Category;

    let category = match source.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    tracing::warn!(
        path = ?path,
        category,
        line = source.line(),
        column = source.column(),
        "the state file could not be decoded"
    );
    PersistenceError::Deserialize {
        path: path.to_path_buf(),
        category,
        line: source.line(),
        column: source.column(),
    }
}

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
    path: PathBuf,
    clock: Arc<dyn Clock>,
}

impl StateStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self { path, clock }
    }

    pub fn path(&self) -> &Path {
        &self.path
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
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Self::fresh(true, LoadReason::Missing);
            }
            Err(error) => {
                tracing::warn!(
                    path = ?self.path,
                    %error,
                    "cannot read the state file; leaving it in place and not writing this session"
                );
                return Self::fresh(false, LoadReason::Unreadable);
            }
        };

        match decode_state(&self.path, &bytes, true) {
            Ok((state, None)) => LoadOutcome {
                state,
                writable: true,
                reason: LoadReason::Loaded,
                queue_repair: None,
            },
            Ok((state, Some(reset))) => {
                let backup = self.back_up_original(&bytes);
                let writable = matches!(backup, QueueBackup::Saved(_));
                tracing::warn!(
                    fields = reset.fields_reset(),
                    writable,
                    "queue data in the state file was reset"
                );
                LoadOutcome {
                    state,
                    writable,
                    reason: LoadReason::Loaded,
                    queue_repair: Some(QueueRepair { reset, backup }),
                }
            }
            Err(PersistenceError::UnsupportedVersion { found, .. }) => {
                tracing::warn!(
                    path = ?self.path,
                    found,
                    supported = SCHEMA_VERSION,
                    "unsupported state schema; preserving the file and not writing this session"
                );
                Self::fresh(false, LoadReason::UnsupportedVersion { found })
            }
            Err(_malformed) => self.reject_malformed(),
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
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(StateSnapshot(PersistedState::default()));
            }
            Err(source) => {
                return Err(PersistenceError::Io {
                    path: self.path.clone(),
                    op: "read",
                    source,
                });
            }
        };

        decode_state(&self.path, &bytes, false).map(|(state, _)| StateSnapshot(state))
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

        let bytes =
            serde_json::to_vec_pretty(state).map_err(|source| PersistenceError::Serialize {
                path: self.path.clone(),
                source,
            })?;

        replace_bytes(&self.path, &bytes)
    }

    fn parent(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    fn fresh(writable: bool, reason: LoadReason) -> LoadOutcome {
        LoadOutcome {
            state: PersistedState::default(),
            writable,
            reason,
            queue_repair: None,
        }
    }

    fn reject_malformed(&self) -> LoadOutcome {
        match self.quarantine() {
            Some(moved_to) => {
                tracing::warn!(path = ?self.path, ?moved_to, "state file quarantined");
                Self::fresh(true, LoadReason::Quarantined { moved_to })
            }
            None => {
                tracing::warn!(
                    path = ?self.path,
                    "cannot quarantine the state file; not writing this session"
                );
                Self::fresh(false, LoadReason::QuarantineFailed)
            }
        }
    }

    /// Move the file aside under a timestamped name, never over one that
    /// already exists. Single-user, single-process by design (§17), so the
    /// exists-then-rename window is not a hazard worth more machinery.
    fn quarantine(&self) -> Option<PathBuf> {
        let stamp = stamp(self.clock.sample().wall);
        let dir = self.parent();
        for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
            let name = if suffix == 1 {
                format!("state.json.rejected-{stamp}")
            } else {
                format!("state.json.rejected-{stamp}-{suffix}")
            };
            let candidate = dir.join(name);
            if candidate.exists() {
                continue;
            }
            if fs::rename(&self.path, &candidate).is_ok() {
                return Some(candidate);
            }
            return None;
        }
        None
    }

    /// §6: copy the exact bytes aside with create-new semantics before any
    /// writer can replace them. Never renames or rewrites the original.
    fn back_up_original(&self, bytes: &[u8]) -> QueueBackup {
        use std::io::Write;
        let stamp = stamp(self.clock.sample().wall);
        let dir = self.parent();
        for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
            let name = if suffix == 1 {
                format!("state.json.queue-recovery-{stamp}")
            } else {
                format!("state.json.queue-recovery-{stamp}-{suffix}")
            };
            let candidate = dir.join(name);
            let mut file = match super::atomic::create_private_new(&candidate) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return QueueBackup::Failed,
            };
            // D12 (atomic.rs): the backup holds the user's full listening
            // history, so its mode is asserted explicitly rather than left
            // to the create-time mode, which the umask can alter.
            if super::atomic::set_private(&candidate).is_err() {
                return QueueBackup::Failed;
            }
            if file
                .write_all(bytes)
                .and_then(|()| file.sync_all())
                .is_err()
            {
                return QueueBackup::Failed;
            }
            super::atomic::sync_parent_best_effort(dir);
            return QueueBackup::Saved(candidate);
        }
        QueueBackup::Failed
    }
}

/// `20260908T143211Z` — filesystem-safe, no colons (§13).
///
/// `pub(crate)` so `lifecycle::stderr` can reuse the exact same format for
/// per-session TUI log filenames rather than duplicating it.
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
