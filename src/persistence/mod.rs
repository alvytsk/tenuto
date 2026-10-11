//! Durable playback state: the model, the store that owns the file, and the
//! writer thread that owns the disk.

pub mod atomic;
pub(crate) mod json_file;
pub mod model;
pub mod queue_codec;
pub mod store;
pub mod writer;

use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// Deliberately without the words "state file": M4 broadened this
    /// variant past `state.json` to `subscriptions.json`, the feed cache and
    /// stdout, so naming one of the four would misreport the other three.
    /// `op` and `path` say which together — `cannot read "…/state.json"`,
    /// `cannot write command output to "<stdout>"`.
    #[error("cannot {op} {path:?}")]
    Io {
        path: PathBuf,
        op: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("cannot serialize state for {path:?}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// Deliberately without a `#[source]`: a `serde_json::Error`'s `Display`
    /// can quote the offending input verbatim, and a checkpoint map key is a
    /// media identity that may carry a URL. `category`, `line` and `column`
    /// give a caller enough to act on without repeating untrusted text.
    #[error("state file {path:?} is malformed ({category} error at line {line}, column {column})")]
    Deserialize {
        path: PathBuf,
        category: &'static str,
        line: usize,
        column: usize,
    },
    #[error("state file {path:?} is schema version {found}, and this build supports {supported}")]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("no platform state directory is available")]
    NoStateDirectory,
}
