//! `FeedError`: the one error type every M4 feed and subscription operation
//! constructs from (§7.2).
//!
//! **Redaction must hold under `Debug`, not only `Display`.** `main.rs`
//! prints `{error}` and logs `?error`, and derived `Debug` prints field
//! values verbatim, so a redaction that applied only to `Display` would leak
//! through the log line. Every URL-bearing field here therefore holds an
//! already-redacted `String`, never a live [`url::Url`] — the same
//! discipline [`crate::http::error::RemoteFailure`] follows.

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("feed encoding is invalid")]
    Encoding,
    #[error("unsupported feed encoding: {label}")]
    UnsupportedEncoding { label: String },
    #[error("unsupported feed format")]
    UnsupportedFormat,
    #[error("malformed feed: {detail}")]
    Malformed { detail: String },
    #[error("episode {index} {title:?} has no audio enclosure; nothing to play")]
    NotPlayable {
        slug: String,
        index: usize,
        title: String,
    },
    #[error("unknown feed: {slug}")]
    UnknownSlug { slug: String },
    #[error("episode index {index} is outside 1..={retained} for {slug}")]
    IndexOutOfRange {
        slug: String,
        index: usize,
        retained: usize,
    },
    #[error("no cached episodes for {slug}; run tenuto refresh {slug}")]
    CacheMissing { slug: String },
    #[error("corrupt cache for {slug}: {detail}; run tenuto refresh {slug}")]
    CacheCorrupt { slug: String, detail: String },
    #[error("cache parser {found} differs from {expected} for {slug}; run tenuto refresh {slug}")]
    CacheParserMismatch {
        slug: String,
        found: u32,
        expected: u32,
    },
    #[error("cannot use subscriptions: {reason}")]
    SubscriptionsUnreadable { reason: String },
    #[error("cannot use stations: {reason}")]
    StationsUnreadable { reason: String },
    /// A station probe refused an add or re-probe. The reason carries its own
    /// context — a `redact_url`-redacted URL plus the transport's
    /// classification — so `Display` passes it through untouched; the
    /// stations *file* is fine, which is what
    /// [`FeedError::StationsUnreadable`] must never be read as claiming.
    #[error("{reason}")]
    StationProbeFailed { reason: String },
    #[error("Another subscription update is in progress")]
    SubscriptionsBusy,
    #[error("Another station update is in progress")]
    StationsBusy,
    #[error("invalid slug {slug:?}; expected 1-32 ASCII lowercase letters, digits or hyphens")]
    InvalidSlug { slug: String },
    #[error("slug already taken: {slug}")]
    SlugTaken { slug: String },
    #[error("already subscribed as {slug}")]
    AlreadySubscribed { slug: String },
    #[error("{failed} of {total} feeds did not complete successfully")]
    BatchIncomplete { failed: usize, total: usize },
    #[error(transparent)]
    Remote(#[from] crate::http::error::RemoteFailure),
    #[error(transparent)]
    Persistence(#[from] crate::persistence::PersistenceError),
}
