//! The per-session TUI log (design doc M5 §11): a uniquely named append log
//! under `<state dir>/logs/`, retention of the five most recent prior logs,
//! and — on Unix — redirecting the process's own fd 2 into it so a panic or
//! a library's stray `eprintln!` lands in the log instead of corrupting the
//! terminal `tui` is drawing to.
//!
//! `open_session_log` and `retain_recent_logs` are exercised directly by
//! `tests/m5_session_log.rs`. `redirect_stderr` is exercised only from a
//! subprocess (Task 29): redirecting fd 2 in-process would also capture the
//! test runner's own stderr.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::persistence::PersistenceError;
use crate::persistence::atomic::{prepare_directory, set_private};
use crate::persistence::json_file::stamp;

/// How many prior session logs `open_session_log` leaves in place besides
/// the one it is about to create.
pub const KEPT_PRIOR_LOGS: usize = 5;

/// Highest `-<pid>-N` suffix `open_session_log` tries before giving up. Only
/// reached if this many sessions somehow started within the same wall-clock
/// second under the same pid; matches the analogous `MAX_QUARANTINE_CANDIDATES`
/// in `persistence::store`.
const MAX_LOG_CANDIDATES: u32 = 100;

const LOG_PREFIX: &str = "tenuto-tui-";
const LOG_SUFFIX: &str = ".log";

/// `<state dir>/logs`, sibling to wherever `state_file` (i.e. `state.json`)
/// lives.
pub fn log_dir(state_file: &Path) -> PathBuf {
    state_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("logs")
}

/// Deletes the oldest `tenuto-tui-*.log` files in `dir` beyond the most
/// recent `keep`, returning the paths removed. Files that do not match the
/// `tenuto-tui-*.log` pattern (a stray `notes.txt`, say) are never touched.
///
/// Matching names sort ascending by their embedded stamp, which sorts
/// chronologically since the stamp is a fixed-width `YYYYMMDDTHHMMSSZ`.
/// Within the same second, a name with a `-<pid>-2.log` disambiguator sorts
/// *before* its un-suffixed `-<pid>.log` sibling — `-` (0x2D) is less than
/// `.` (0x2E) — so the retry candidate from a same-second collision can be
/// judged "older" than the file it lost the race to. This is an accepted
/// quirk of ascending name sort rather than a bug: the two logs are from the
/// same second either way, and retention still keeps exactly `keep` of them.
pub fn retain_recent_logs(dir: &Path, keep: usize) -> io::Result<Vec<PathBuf>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.starts_with(LOG_PREFIX) && name.ends_with(LOG_SUFFIX) {
            names.push(name.to_string());
        }
    }
    names.sort();

    let mut removed = Vec::new();
    if names.len() > keep {
        let cut = names.len() - keep;
        for name in &names[..cut] {
            let path = dir.join(name);
            std::fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// Creates `<state dir>/logs/` under the private-directory policy (0700,
/// created if missing, tightened if not), retains the five most recent
/// prior logs, then creates a new `tenuto-tui-<stamp>-<pid>.log` opened
/// with `append(true).create_new(true)` at mode 0600. A same-second,
/// same-pid collision (two sessions started within the same wall-clock
/// second) retries `-<pid>-2` through `-<pid>-100`.
pub fn open_session_log(state_file: &Path, wall: OffsetDateTime) -> io::Result<(File, PathBuf)> {
    let dir = log_dir(state_file);
    prepare_directory(&dir).map_err(persistence_io_error)?;
    retain_recent_logs(&dir, KEPT_PRIOR_LOGS)?;

    let stamp = stamp(wall);
    let pid = std::process::id();
    for suffix in 1..=MAX_LOG_CANDIDATES {
        let name = if suffix == 1 {
            format!("{LOG_PREFIX}{stamp}-{pid}{LOG_SUFFIX}")
        } else {
            format!("{LOG_PREFIX}{stamp}-{pid}-{suffix}{LOG_SUFFIX}")
        };
        let path = dir.join(name);
        match create_private_append(&path) {
            Ok(file) => {
                // D12 (persistence::atomic): reassert the mode explicitly
                // rather than trust the create-time mode, which the umask
                // can widen.
                set_private(&path)?;
                return Ok((file, path));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("exhausted session log name candidates for pid {pid} at {stamp}"),
    ))
}

/// `prepare_directory` only ever fails with `PersistenceError::Io` on the
/// paths this module exercises; the fallback keeps this total without
/// leaking `PersistenceError`'s redacted `Display` text as anything other
/// than an opaque message.
fn persistence_io_error(error: PersistenceError) -> io::Error {
    match error {
        PersistenceError::Io { source, .. } => source,
        other => io::Error::other(other.to_string()),
    }
}

#[cfg(unix)]
fn create_private_append(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .append(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().append(true).create_new(true).open(path)
}

/// A live redirection of the process's own fd 2 (stderr) to a file. Dropping
/// it restores fd 2 to whatever it pointed at before.
#[cfg(unix)]
pub struct StderrRedirect(
    /// Kept alive only so the redirect stays installed; dropping this is
    /// what restores fd 2. Never read.
    #[allow(dead_code)]
    gag::Redirect<File>,
);

/// Redirects the process's fd 2 to `file` for as long as the returned
/// [`StderrRedirect`] lives. Only meaningful for the whole process — anyone
/// still holding the original stderr handle now writes into `file` too —
/// which is why this is exercised only from a subprocess (Task 29) rather
/// than in-process, where it would swallow the test runner's own stderr.
#[cfg(unix)]
pub fn redirect_stderr(file: File) -> io::Result<StderrRedirect> {
    gag::Redirect::stderr(file)
        .map(StderrRedirect)
        .map_err(io::Error::from)
}
