/// Which request an outcome belongs to, so a failure says what was being done
/// rather than only what went wrong.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Open,
    Read,
    Seek,
    Reopen,
    /// The bounded tail read that confirms a finite body actually ended (§9).
    Complete,
}

/// Which deadline elapsed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Connect,
    Headers,
    /// No data while actively demanding it.
    Stall,
    /// The whole of opening and probing.
    Open,
    /// One seek's own operation deadline (M3.1 Task 4), checked regardless
    /// of the freeze level. Distinct from `Stall`, which is active-demand
    /// time and is suspended while paused.
    Seek,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedirectRejection {
    TooMany,
    Loop,
    UnsupportedScheme,
    InvalidLocation,
    /// HTTPS to HTTP. Never followed, whatever the hop count.
    Downgrade,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeRejection {
    /// A 206 whose `Content-Range` start is not the byte that was requested.
    WrongStart,
    /// `last < first`.
    ReversedInterval,
    /// A total that contradicts one already established for this session.
    ConflictingTotal,
    /// A 206 with no `Content-Range`, or one that does not parse.
    Malformed,
    /// `multipart/byteranges`. M3 never requests it and never accepts it.
    Multipart,
    /// A range was required and the server answered 200 instead.
    RangeIgnored,
    /// A body shorter or longer than the interval the header advertised.
    LengthMismatch,
    /// `last >= total`: the interval does not fit inside the object it claims
    /// to be part of.
    IntervalPastTotal,
    /// A 416 for a range that is not the known byte EOF.
    Unsatisfiable,
}

/// Typed remote faults, one variant per §11 category.
///
/// `Display` is the third-party-safe rendering: every URL here has already
/// passed through [`crate::telemetry::redact_url`], so no signed query or userinfo can reach a
/// status line, a log line or a `PlaybackEvent::Failed` message.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RemoteFailure {
    #[error("{input} is not a usable media URL: {reason}")]
    InvalidSource { input: String, reason: &'static str },
    #[error("the server answered HTTP {status} while {operation:?}")]
    Status { status: u16, operation: Operation },
    #[error("redirect refused: {reason:?}")]
    Redirect { reason: RedirectRejection },
    #[error("the server went quiet during {phase:?}")]
    Timeout { phase: Phase },
    #[error("the server's range response is unusable: {reason:?}")]
    InvalidRange { reason: RangeRejection },
    #[error("this recording changed on the server while it was open")]
    ResourceChanged,
    #[error("the response body ended {missing} bytes early")]
    TruncatedBody { missing: u64 },
    #[error("opening needed more than {limit} bytes of input")]
    ProbeLimitExceeded { limit: u64 },
    #[error("cannot establish whether this source ever ends")]
    ContinuityUndetermined,
    #[error("not a direct live audio stream; HLS playlists are not supported")]
    UnsupportedLiveMedia,
    #[error("this stream interleaves metadata, which is not supported yet")]
    IcyFramingUnsupported,
    /// A live body ended. Never completion: a station has no end (M7 §3.3).
    #[error("the live stream ended")]
    LiveEnded,
    #[error("this server cannot seek or resume this recording")]
    SeekUnavailable,
    #[error("the response used {encoding} content encoding, not identity")]
    NonIdentityEncoding { encoding: String },
    #[error("network error while {operation:?}: {detail}")]
    Transport {
        operation: Operation,
        detail: String,
    },
    /// Not a fault: a stop, seek or shutdown retired the read that was in
    /// flight. §8 requires this to stay distinguishable from every failure
    /// above even after Symphonia wraps the `io::Error`.
    #[error("the read was cancelled")]
    Cancelled,
    /// §3.4: the streaming body would exceed `Limits::document_bytes`. A
    /// declared `Content-Length` already above the cap is a cheap early exit,
    /// but the running total while streaming is the authority.
    #[error("feed document exceeds {limit} bytes")]
    DocumentTooLarge { limit: usize },
    /// §3.3: a 304 arrived when no conditional header was ever sent or when
    /// the cache backing the conditional request was missing or corrupt.
    #[error("received HTTP 304 without a usable conditional request")]
    UnsolicitedNotModified,
}

impl RemoteFailure {
    /// Whether trying the same location again can help (M7 §4). A failure
    /// that says the location itself is unusable is never retried.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { .. } | Self::Timeout { .. } | Self::LiveEnded => true,
            Self::Status { status, .. } => *status == 429 || (500..600).contains(status),
            _ => false,
        }
    }
}
