//! Atomic whole-file replacement, shared by every durable store (state,
//! subscriptions, and the per-feed episode cache). Extracted from
//! `StateStore::write` (design doc §1.3 item 1): semantics are preserved
//! exactly, including parent-directory `fsync` as best-effort *after* the
//! rename, non-fatal and logged at debug.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::PersistenceError;

/// Shared across every destination so that concurrent writers - even to
/// different files - never choose the same temporary name.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Replace `path`'s contents with `bytes` atomically: write to a sibling
/// temporary file, fsync it, rename it over the destination, then make a
/// best-effort, non-fatal attempt to fsync the parent directory.
///
/// The parent directory is created (and, on Unix, tightened to 0700) if it
/// does not already exist or is more permissive than that.
pub fn replace_bytes(path: &Path, bytes: &[u8]) -> Result<(), PersistenceError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    prepare_directory(dir)?;
    sweep_orphans(dir);

    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}-{seq}", std::process::id()));
    let temp = dir.join(name);

    // The create-time mode already keeps the file private the instant it
    // exists; the explicit `set_permissions` right after is what makes the
    // 0600 an assertion the code makes rather than a side effect the create
    // call happened to have (D12).
    let write_temp = || -> Result<(), io::Error> {
        let mut file = create_private_new(&temp)?;
        set_private(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    if let Err(source) = write_temp() {
        let _ = fs::remove_file(&temp);
        return Err(PersistenceError::Io {
            path: temp,
            op: "write",
            source,
        });
    }

    if let Err(source) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(PersistenceError::Io {
            path: path.to_path_buf(),
            op: "replace",
            source,
        });
    }

    sync_parent_best_effort(dir);
    Ok(())
}

/// Best-effort, non-fatal fsync of a parent directory after a rename into it.
/// Not fatal: the replacement already happened, and a parent that cannot be
/// fsynced is a durability gap, not a lost checkpoint.
pub(crate) fn sync_parent_best_effort(dir: &Path) {
    if let Ok(handle) = File::open(dir)
        && let Err(error) = handle.sync_all()
    {
        tracing::debug!(path = ?dir, %error, "cannot fsync the parent directory");
    }
}

/// Removes `.tmp-<pid>-<seq>` temporaries whose writing process is gone: a
/// crash between the create and the rename of [`replace_bytes`] leaves one
/// behind in the destination's directory, and the next write there sweeps
/// it. A temporary whose pid is still alive — this process's own concurrent
/// writes, or another running Tenuto's — is left alone, so the sweep can
/// never pull a live writer's temporary out from under its rename. The pid
/// check reads `/proc`, so this is a no-op off Linux: a documented platform
/// gap (§17, D12), not a behavior difference in the writes themselves.
#[cfg(target_os = "linux")]
fn sweep_orphans(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let own = std::process::id();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(orphan_pid) else {
            continue;
        };
        if pid == own || Path::new("/proc").join(pid.to_string()).exists() {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => tracing::debug!(path = ?entry.path(), pid, "swept an orphaned temporary"),
            Err(error) => {
                tracing::debug!(path = ?entry.path(), %error, "cannot sweep an orphaned temporary")
            }
        }
    }
}

/// The pid embedded in a `<name>.tmp-<pid>-<seq>` temporary; `None` for
/// anything else — real store files, other dot files, directories.
#[cfg(target_os = "linux")]
fn orphan_pid(name: &str) -> Option<u32> {
    let (pid, _seq) = name.rsplit_once(".tmp-")?.1.split_once('-')?;
    pid.parse().ok()
}

#[cfg(unix)]
pub(crate) fn prepare_directory(dir: &Path) -> Result<(), PersistenceError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|source| PersistenceError::Io {
                path: dir.to_path_buf(),
                op: "create directory for",
                source,
            })?;
        // Deliberately falling through to the check below rather than
        // returning here: `mkdir`'s mode is masked by the process umask, so
        // a directory this build just created is the one case an early
        // return would leave unverified. D12's 0700 is asserted for every
        // path through this function.
    }

    // D12 says the mode is set explicitly, and a create-time mode reaches
    // exactly the case that never needs it. A directory left at 0755 by an
    // earlier build, a restore, or a hand-made `mkdir` is the one that does.
    let metadata = match fs::metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) => {
            // Not fatal (D11): whatever is wrong with the directory will
            // surface again, more informatively, at the write that follows.
            tracing::warn!(
                path = ?dir,
                %error,
                "cannot stat the destination directory to check its permissions"
            );
            return Ok(());
        }
    };
    if metadata.permissions().mode() & 0o777 != 0o700
        && let Err(error) = fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    {
        // Not fatal, and deliberately not a refusal to write: a directory
        // this process cannot chmod leaks a filename at worst. Losing the
        // checkpoint over it would be the larger harm.
        tracing::warn!(path = ?dir, %error, "cannot tighten the destination directory to 0700");
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn prepare_directory(dir: &Path) -> Result<(), PersistenceError> {
    // Platform defaults: a documented gap (§17, D12).
    fs::create_dir_all(dir).map_err(|source| PersistenceError::Io {
        path: dir.to_path_buf(),
        op: "create directory for",
        source,
    })
}

#[cfg(unix)]
pub(crate) fn create_private_new(path: &Path) -> Result<File, io::Error> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
pub(crate) fn create_private_new(path: &Path) -> Result<File, io::Error> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// D12 requires the destination's mode to be set explicitly rather than left
/// to whatever the create call happened to produce. On Unix, `set_permissions`
/// re-asserts the 0600 the create already applied, so there is no window
/// where the file is briefly more open than this.
#[cfg(unix)]
pub(crate) fn set_private(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
pub(crate) fn set_private(_path: &Path) -> Result<(), io::Error> {
    Ok(())
}
