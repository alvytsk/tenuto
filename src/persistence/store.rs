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
        } = self.file.load(VERSIONS, |version, bytes| {
            decode_state(path, version, bytes, true)
        });

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
        match self.file.read(VERSIONS, |version, bytes| {
            decode_state(path, version, bytes, false)
        }) {
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
