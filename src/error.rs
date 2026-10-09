use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("cannot identify path {path:?}: {reason}")]
    InvalidPath { path: PathBuf, reason: &'static str },
    #[error("cannot normalize identity URL {input:?}: {source}")]
    InvalidUrl {
        input: String,
        #[source]
        source: url::ParseError,
    },
    #[error("cannot normalize identity URL {input:?}: expected HTTP(S) with a host")]
    UnsupportedUrl { input: String },
    #[error("cannot construct feed identity: empty identifier")]
    EmptyFeedId,
    #[error("cannot resolve episode identity: no GUID, enclosure URL, or item link")]
    MissingEpisodeIdentity,
    #[error("cannot parse media identity {input:?}: {reason}")]
    InvalidMediaId { input: String, reason: &'static str },
}

/// Failures around a `play` or `tui` invocation's shared lifecycle: the
/// profile lock, signal installation, the session log, terminal setup, and a
/// background worker's uncontained panic.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Lock(#[from] crate::lifecycle::lock::LockError),
    #[error("cannot install signal handling")]
    Signals(#[source] std::io::Error),
    #[error("cannot open the session log")]
    Log(#[source] std::io::Error),
    #[error("cannot redirect stderr to the session log")]
    Redirect(#[source] std::io::Error),
    #[error("cannot set up the terminal")]
    Terminal(#[source] std::io::Error),
    #[error("a background worker panicked")]
    WorkerPanicked,
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("invalid tracing filter")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    #[error("RUST_LOG is not valid Unicode")]
    Environment(#[source] std::env::VarError),
    #[error("cannot initialize tracing")]
    Install(#[from] tracing::subscriber::SetGlobalDefaultError),
}
