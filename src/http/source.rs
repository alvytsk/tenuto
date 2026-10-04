//! The Symphonia seam: everything above `HttpMediaSource` is bytes, everything
//! below it is HTTP.
//!
//! Implements `std::io::Read + Seek` and `symphonia::core::io::MediaSource`
//! over the [`super::channel::ByteChannel`], re-requesting ranges on seek and
//! validating the tail before a track may be called complete (§9).

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;
use url::Url;

use super::channel::{ByteChannel, HeaderOutcome, ReadOutcome, SourceInterrupt, WaitHook};
use super::error::{Operation, Phase, RemoteFailure, redact_url};
use super::limits::Limits;
use super::response::{Accepted, Established};
use super::service::{FetchRequest, HttpService};
use crate::media::capabilities::{DemuxerSeek, SourceEvidence};

/// One absolute instant that every wait taken during opening is clamped
/// against. Threaded down rather than checked around the outside, because
/// opening is a sequence of individually-short waits and checking only
/// between them lets a slow trickle run indefinitely: each individual read
/// can sit comfortably inside `limits.stall` while a server trickles a byte
/// every few seconds, so no read ever times out on its own and opening runs
/// forever unless something is watching the whole of it.
#[derive(Clone, Copy, Debug)]
pub struct OpeningDeadline(pub Instant);

impl OpeningDeadline {
    /// `limits.open` from now on the interrupt's clock.
    pub fn starting_now(interrupt: &SourceInterrupt, limits: &Limits) -> Self {
        Self(interrupt.now() + limits.open)
    }

    /// `None` once elapsed.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.0.checked_duration_since(now)
    }
}

/// The two bounds that apply while opening and must be lifted afterwards.
///
/// Shared, because `prepare` (Task 7) boxes the source into a
/// `MediaSourceStream` and then has no way back to it — this is cloned before
/// boxing so the caps can still be cleared through the clone once opening (the
/// format probe included) is over.
#[derive(Debug)]
pub struct OpeningLimits {
    /// `u64::MAX` means uncapped. An `Option<u64>` behind a `Mutex` would work
    /// too, but a single atomic needs no lock on the hot read path.
    probe_cap: AtomicU64,
    /// `false` once opening is over. Read on every wait the source takes, so
    /// ordinary playback reads stop being clamped to the opening deadline the
    /// instant this flips.
    opening: AtomicBool,
}

impl OpeningLimits {
    fn new() -> Self {
        Self {
            probe_cap: AtomicU64::new(u64::MAX),
            opening: AtomicBool::new(true),
        }
    }

    pub fn set_probe_cap(&self, cap: Option<u64>) {
        self.probe_cap
            .store(cap.unwrap_or(u64::MAX), Ordering::Release);
    }

    fn probe_cap(&self) -> Option<u64> {
        let value = self.probe_cap.load(Ordering::Acquire);
        if value == u64::MAX { None } else { Some(value) }
    }

    fn is_opening(&self) -> bool {
        self.opening.load(Ordering::Acquire)
    }

    /// Lift both bounds. Ordinary playback reads are then bounded only by
    /// `limits.stall`, which is the rule that ordinary playback has no
    /// whole-response deadline.
    ///
    /// Both fields are cleared, not just `opening`: `Read::read`'s probe-cap
    /// check is unconditional on `is_opening()`, and `consumed` only ever
    /// grows, so a cap left in place after opening fails every ordinary
    /// track whose total exceeds it, partway through playback.
    pub fn finish_opening(&self) {
        self.probe_cap.store(u64::MAX, Ordering::Release);
        self.opening.store(false, Ordering::Release);
    }
}

/// Which of the two bounds a wait's budget was clipped to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Bound {
    /// `limits.stall`: the wait's own phase is the one to report.
    Stall,
    /// The remainder of the opening deadline, which was shorter than
    /// `limits.stall`: a wait that exhausts it hit the *opening* deadline,
    /// whatever phase the wait itself measures.
    Opening,
}

/// The wait budget for one blocking operation, and the bound it came from.
#[derive(Clone, Copy, Debug)]
struct WaitBudget {
    budget: Duration,
    bound: Bound,
}

impl WaitBudget {
    /// The failure a wait that ran out of this budget actually reports.
    ///
    /// A wait only knows its own phase - `wait_for_headers` reports
    /// `Headers`, `ByteChannel::read` reports `Stall` - and reports it
    /// whenever the budget it was handed runs out. When that budget was the
    /// opening deadline's remainder, the bound that expired was the opening
    /// deadline, and `Open` is the phase that names it. Before this, which
    /// phase a trickling server's probe reported depended on where the
    /// deadline fell inside a byte gap: a read with more of the remainder
    /// left than its next slice check saw the next byte first, and the
    /// *following* read's `wait_budget` reported `Open`; one with less
    /// reported `Stall` from inside the clipped wait (the macOS leg, and
    /// Linux under load, 2026-09-17). Only the wait's own timeout phase is
    /// reclassified: a published `Connect` timeout, say, is a fact about
    /// the fetch, not about this budget.
    fn exhausted(self, failure: RemoteFailure, own: Phase) -> RemoteFailure {
        match (self.bound, failure) {
            (Bound::Opening, RemoteFailure::Timeout { phase }) if phase == own => {
                RemoteFailure::Timeout { phase: Phase::Open }
            }
            (_, failure) => failure,
        }
    }
}

/// A station identity header, trimmed, with an all-whitespace value treated
/// as absent — the same "decorative field, never costs a station its
/// classification" rule `icy-br`/`icy-logo` already follow (§5). Otherwise
/// a trailing space survives into the draw, e.g. `Lofi  · 128 kbps`.
fn trimmed_or_none(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(str::to_string)
}

/// The wait budget for one blocking operation: `limits.stall` once opening is
/// over, or the smaller of `limits.stall` and however much of the opening
/// deadline remains while it is not - tagged with which of the two it was.
///
/// Used identically by the header wait in `open`, every `Read::read` a probe
/// takes, and every seek's header wait — the three places Ruling 1 names.
fn wait_budget(
    opening_limits: &OpeningLimits,
    opening_deadline: OpeningDeadline,
    limits: &Limits,
    now: Instant,
) -> Result<WaitBudget, RemoteFailure> {
    if !opening_limits.is_opening() {
        return Ok(WaitBudget {
            budget: limits.stall,
            bound: Bound::Stall,
        });
    }
    match opening_deadline.remaining(now) {
        Some(remaining) if remaining < limits.stall => Ok(WaitBudget {
            budget: remaining,
            bound: Bound::Opening,
        }),
        Some(_) => Ok(WaitBudget {
            budget: limits.stall,
            bound: Bound::Stall,
        }),
        None => Err(RemoteFailure::Timeout { phase: Phase::Open }),
    }
}

/// The ICY identity of a live source (M7.1 §5). Every field is optional
/// because ICY guarantees none of them: `response::is_icy` accepts a
/// response carrying `icy-br` and no `icy-name`, and such a station is
/// legitimately verified-but-unnamed. Display text throughout — the caller
/// escapes it before drawing it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StationIdentity {
    pub name: Option<String>,
    pub genre: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub logo: Option<Url>,
}

/// Finite remote media over HTTP, read and seekable through the byte channel.
pub struct HttpMediaSource {
    service: Arc<HttpService>,
    origin: Url,
    interrupt: Arc<SourceInterrupt>,
    channel: ByteChannel,
    hook: Arc<dyn WaitHook>,
    limits: Limits,
    opening_deadline: OpeningDeadline,
    opening_limits: Arc<OpeningLimits>,
    /// The validator and total bytes established by whichever response is
    /// current — carried into every later request's `If-Range` and cross-
    /// checked against every later response (§7).
    established: Established,
    pos: u64,
    byte_len: Option<u64>,
    byte_seekable: bool,
    live: bool,
    /// Set by a seek that lands exactly on a known byte EOF, so the next read
    /// answers `Ok(0)` locally instead of asking the server for a range past
    /// its own advertised end (which it would answer 416, and a 416 is never
    /// completion — see `response::accept`).
    at_byte_eof: bool,
    /// Latched on the first `ReadOutcome::Retired`, so a source that keeps
    /// being asked after a retirement keeps answering the same way instead of
    /// re-entering the channel (§8).
    retired: bool,
    consumed: u64,
    identity: Option<StationIdentity>,
}

impl HttpMediaSource {
    /// Open at byte zero. Performs the opening range GET and classifies
    /// access. The accepted response *is* the initial stream (§6): no second
    /// request.
    ///
    /// Returns the [`OpeningLimits`] handle alongside the source rather than
    /// exposing `set_probe_cap` on the source itself, because `prepare` boxes
    /// the source into a `MediaSourceStream` and loses any other way back to
    /// it once probing starts.
    pub fn open(
        service: Arc<HttpService>,
        origin: Url,
        interrupt: Arc<SourceInterrupt>,
        hook: Arc<dyn WaitHook>,
        limits: Limits,
        opening: OpeningDeadline,
    ) -> Result<(Self, Arc<OpeningLimits>), RemoteFailure> {
        let channel = ByteChannel::new(Arc::clone(&interrupt));
        let opening_limits = Arc::new(OpeningLimits::new());
        let generation = interrupt.begin();
        let request = FetchRequest {
            origin: origin.clone(),
            start: 0,
            established: None,
            operation: Operation::Open,
        };
        let header_wait = service.fetch(request, channel.clone(), generation);

        let outcome = wait_budget(&opening_limits, opening, &limits, interrupt.now())
            .map_err(HeaderOutcome::Failed)
            .and_then(|budget| {
                header_wait
                    .wait(hook.as_ref(), budget.budget)
                    .map_err(|outcome| exhausted_headers(budget, outcome))
            });

        let accepted = match outcome {
            Ok(accepted) => accepted,
            Err(failure) => {
                // Every non-success path out of `open` retires the
                // generation it began (Ruling 3): `begin()`/`retire()` are
                // only ever paired on this thread today (see
                // `SourceInterrupt::begin`'s own note to the same effect), so
                // there is nothing concurrent here to clobber.
                interrupt.retire();
                return Err(match failure {
                    HeaderOutcome::Retired => RemoteFailure::Cancelled,
                    HeaderOutcome::Failed(failure) => failure,
                });
            }
        };

        let (byte_len, byte_seekable, live, identity) = match accepted.accepted {
            Accepted::Sequential { len } => (len, false, false, None),
            Accepted::Ranged { range } => (range.total, true, false, None),
            Accepted::Live => (
                None,
                false,
                true,
                Some(StationIdentity {
                    name: trimmed_or_none(accepted.headers.get("icy-name")),
                    genre: trimmed_or_none(accepted.headers.get("icy-genre")),
                    // A bitrate that is not a plain decimal, and a logo that
                    // is not an absolute URL, are simply absent: a decorative
                    // field never costs a station its classification (§5).
                    bitrate_kbps: accepted
                        .headers
                        .get("icy-br")
                        .and_then(|value| value.trim().parse::<u32>().ok()),
                    // Only an absolute http(s) URL with a host is kept: a
                    // `file:`/`data:`/`javascript:` value would never be
                    // fetched by `fetch_document` anyway, and admitting it
                    // here just persists a logo that can never load (§5).
                    logo: accepted.headers.get("icy-logo").and_then(|value| {
                        Url::parse(value.trim()).ok().filter(|u| {
                            matches!(u.scheme(), "http" | "https") && u.host_str().is_some()
                        })
                    }),
                }),
            ),
        };
        let established = Established {
            total: byte_len,
            validator: accepted.validator,
        };

        // §11: source opening and its redirect count, in one line - every URL
        // through `redact_url` first, since a signed query or userinfo must
        // never reach a log line.
        tracing::debug!(
            url = %redact_url(origin.as_str()),
            redirects = accepted.redirects,
            "opened remote source"
        );
        // §11: capability evidence, separately from the line above - this is
        // exactly `SourceEvidence`'s own fields, logged at the point they are
        // first established rather than only inferred later from behaviour.
        tracing::debug!(
            byte_len = ?byte_len,
            byte_seekable,
            live,
            "remote source capability evidence"
        );

        let source = Self {
            service,
            origin,
            interrupt,
            channel,
            hook,
            limits,
            opening_deadline: opening,
            opening_limits: Arc::clone(&opening_limits),
            established,
            pos: 0,
            byte_len,
            byte_seekable,
            live,
            at_byte_eof: false,
            retired: false,
            consumed: 0,
            identity,
        };
        Ok((source, opening_limits))
    }

    /// The ICY identity of a live source (M7.1 §5); `None` for finite media.
    /// Display text: the caller escapes it.
    pub fn station_identity(&self) -> Option<&StationIdentity> {
        self.identity.as_ref()
    }

    pub fn evidence(&self) -> SourceEvidence {
        SourceEvidence {
            byte_len: self.byte_len,
            byte_seekable: self.byte_seekable,
            live: self.live,
            // A remote source has demonstrated nothing about its container —
            // byte access says nothing about the demuxer's ability to seek in
            // media time (§4).
            demuxer: DemuxerSeek::Unproven,
        }
    }

    /// The bounded tail read §9 requires before a track may be called
    /// complete. Always requires the fetch's own terminal outcome, even when
    /// `pos == byte_len`: a chunked 206 can deliver its whole advertised
    /// interval and then die before its terminal framing, and "every byte
    /// arrived" is not "the transfer ended cleanly".
    pub fn confirm_complete(&mut self) -> Result<(), RemoteFailure> {
        if self.retired {
            // `retired` can have been latched by a prior `Read::read` without
            // that call retiring the interrupt itself (a plain read owns no
            // generation to clean up — see the note on its `Retired` arm), so
            // this path still has to do it, per every non-success exit here
            // retiring.
            self.interrupt.retire();
            return Err(RemoteFailure::Cancelled);
        }
        // A fixed scratch buffer: this is a discard read, not a delivery one,
        // so nothing about its size needs to track the caller's buffers.
        let mut scratch = [0u8; 64 * 1024];
        loop {
            match self
                .channel
                .read(&mut scratch, self.hook.as_ref(), self.limits.stall)
            {
                ReadOutcome::Bytes(_) => continue,
                ReadOutcome::Eof => {
                    // §11: source completion. This is the one place a finite
                    // remote body is actually confirmed to have ended cleanly
                    // - see this method's own doc comment for why "every byte
                    // arrived" alone is not enough to log this early.
                    tracing::debug!(
                        url = %redact_url(self.origin.as_str()),
                        "remote source confirmed complete"
                    );
                    return Ok(());
                }
                ReadOutcome::Retired => {
                    self.retired = true;
                    self.interrupt.retire();
                    return Err(RemoteFailure::Cancelled);
                }
                ReadOutcome::Failed(failure) => {
                    // Latched the same way `Retired` is: this call is leaving
                    // the interrupt retired either way, so a second call
                    // should discover that immediately at the top of this
                    // function rather than re-entering the channel to find
                    // out.
                    self.retired = true;
                    self.interrupt.retire();
                    return Err(failure);
                }
            }
        }
    }

    /// Bytes consumed since opening, for the §8 probe cap.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    fn wait_budget(&self) -> Result<WaitBudget, RemoteFailure> {
        wait_budget(
            &self.opening_limits,
            self.opening_deadline,
            &self.limits,
            self.interrupt.now(),
        )
    }
}

impl std::io::Read for HttpMediaSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // Latched: a retired source answers identically however many times
        // it is asked, rather than racing the channel on every retry.
        if self.retired {
            return Err(io::Error::other(RemoteIoError(RemoteFailure::Cancelled)));
        }
        if out.is_empty() {
            return Ok(0);
        }
        if self.at_byte_eof {
            return Ok(0);
        }
        if let Some(cap) = self.opening_limits.probe_cap() {
            let would_consume = self.consumed.saturating_add(out.len() as u64);
            if would_consume > cap {
                return Err(io::Error::other(RemoteIoError(
                    RemoteFailure::ProbeLimitExceeded { limit: cap },
                )));
            }
        }
        let budget = self
            .wait_budget()
            .map_err(|failure| io::Error::other(RemoteIoError(failure)))?;
        match self.channel.read(out, self.hook.as_ref(), budget.budget) {
            ReadOutcome::Bytes(n) => {
                self.consumed = self.consumed.saturating_add(n as u64);
                self.pos = self.pos.saturating_add(n as u64);
                Ok(n)
            }
            // The only `Ok(0)` there is.
            ReadOutcome::Eof => Ok(0),
            ReadOutcome::Retired => {
                // Unlike `open`/`seek`/`confirm_complete`, a plain read never
                // began a generation of its own to clean up: `ReadOutcome::
                // Retired` only comes back once the interrupt is already
                // retired (or superseded), so there is nothing here for this
                // call to retire.
                self.retired = true;
                Err(io::Error::other(RemoteIoError(RemoteFailure::Cancelled)))
            }
            ReadOutcome::Failed(failure) => Err(io::Error::other(RemoteIoError(
                budget.exhausted(failure, Phase::Stall),
            ))),
        }
    }
}

/// `WaitBudget::exhausted` for a header wait's outcome: only a `Failed`
/// timeout of the wait's own `Headers` phase is reclassified; `Retired` and
/// every other failure pass through untouched.
fn exhausted_headers(budget: WaitBudget, outcome: HeaderOutcome) -> HeaderOutcome {
    match outcome {
        HeaderOutcome::Failed(failure) => {
            HeaderOutcome::Failed(budget.exhausted(failure, Phase::Headers))
        }
        other => other,
    }
}

/// Resolve one `SeekFrom` against the current position and, for `End`, the
/// known byte length.
fn resolve_seek(pos: u64, byte_len: Option<u64>, from: io::SeekFrom) -> io::Result<u64> {
    let target = match from {
        io::SeekFrom::Start(offset) => Some(offset),
        io::SeekFrom::Current(offset) => offset_from(pos, offset),
        io::SeekFrom::End(offset) => byte_len.and_then(|len| offset_from(len, offset)),
    };
    target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek target out of range"))
}

fn offset_from(base: u64, offset: i64) -> Option<u64> {
    if offset >= 0 {
        base.checked_add(offset.unsigned_abs())
    } else {
        base.checked_sub(offset.unsigned_abs())
    }
}

impl std::io::Seek for HttpMediaSource {
    fn seek(&mut self, from: io::SeekFrom) -> io::Result<u64> {
        let target = resolve_seek(self.pos, self.byte_len, from)?;

        if target == self.pos {
            return Ok(self.pos);
        }

        if let Some(len) = self.byte_len
            && target == len
        {
            // The server would answer a range past its own advertised end
            // with 416, and `response::accept` treats every 416 that reaches
            // it as a failure, never as completion. A known byte EOF is
            // answered locally instead, with no request at all.
            self.pos = target;
            self.at_byte_eof = true;
            return Ok(self.pos);
        }

        if !self.byte_seekable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                RemoteIoError(RemoteFailure::SeekUnavailable),
            ));
        }

        // `begin` both retires the old generation and opens the new one, so a
        // superseded response's bytes cannot enter the channel (H9).
        let generation = self.interrupt.begin();
        // §11: the range operation itself - the byte offset re-requested, not
        // the media-time seek that asked for it (`engine.rs`'s `seek_to` logs
        // that half, in media time, once the refinement it drives lands).
        tracing::debug!(
            url = %redact_url(self.origin.as_str()),
            start_byte = target,
            "issuing range request"
        );
        let request = FetchRequest {
            origin: self.origin.clone(),
            start: target,
            established: Some(self.established.clone()),
            operation: Operation::Seek,
        };
        let header_wait = self
            .service
            .fetch(request, self.channel.clone(), generation);

        let outcome = self
            .wait_budget()
            .map_err(HeaderOutcome::Failed)
            .and_then(|budget| {
                header_wait
                    .wait(self.hook.as_ref(), budget.budget)
                    .map_err(|outcome| exhausted_headers(budget, outcome))
            });

        if let Err(failure) = outcome {
            // Every non-success path out of `seek` retires the generation it
            // began (Ruling 3), for the same single-thread-pairs-begin-and-
            // retire reason as `open`.
            self.interrupt.retire();
            let failure = match failure {
                HeaderOutcome::Retired => RemoteFailure::Cancelled,
                HeaderOutcome::Failed(failure) => failure,
            };
            return Err(io::Error::other(RemoteIoError(failure)));
        }

        // A stop that landed *during* the header wait must win: the seek
        // notices it lost the race and abandons, rather than committing a
        // position against a source the application has already stopped.
        // Checked before any state below is touched.
        if !self.interrupt.is_current(generation) {
            self.interrupt.retire();
            return Err(io::Error::other(RemoteIoError(RemoteFailure::Cancelled)));
        }

        self.pos = target;
        self.at_byte_eof = false;
        Ok(self.pos)
    }
}

impl MediaSource for HttpMediaSource {
    fn is_seekable(&self) -> bool {
        self.byte_seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }
}

impl Drop for HttpMediaSource {
    fn drop(&mut self) {
        // A source dropped mid-fetch must not leave the fetch task pushing
        // into a channel nobody will ever drain again.
        self.interrupt.retire();
    }
}

/// The `io::Error` payload carrying a typed remote failure through Symphonia.
/// Public so `prepare` and the worker can recover it.
#[derive(Debug)]
pub struct RemoteIoError(pub RemoteFailure);

impl std::fmt::Display for RemoteIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// No `source()` override: this is the leaf `remote_cause` recurses down to,
// and a leaf with no source is what stops the recursion.
impl std::error::Error for RemoteIoError {}

/// The typed failure inside an error, wherever it is hiding.
///
/// Neither unwrap here is optional. `symphonia_core::errors::Error` implements
/// the deprecated `cause()` rather than `source()`, so a `source()`-only walk
/// stops at it and finds nothing; and `io::Error::source()` yields the
/// payload's source rather than the payload, which `get_ref()` is what
/// returns. Task 10's whole control flow — retired versus failed versus EOF —
/// rests on this function, so a silent `None` here is a stop that reads as a
/// truncated track.
pub fn remote_cause(error: &(dyn std::error::Error + 'static)) -> Option<RemoteFailure> {
    // Symphonia's own error, by value.
    if let Some(symphonia) = error.downcast_ref::<symphonia::core::errors::Error>() {
        if let symphonia::core::errors::Error::IoError(io) = symphonia {
            return remote_cause(io);
        }
        return None;
    }
    // An io::Error's custom payload.
    if let Some(io) = error.downcast_ref::<std::io::Error>()
        && let Some(inner) = io.get_ref()
    {
        if let Some(marker) = inner.downcast_ref::<RemoteIoError>() {
            return Some(marker.0.clone());
        }
        return remote_cause(inner);
    }
    if let Some(marker) = error.downcast_ref::<RemoteIoError>() {
        return Some(marker.0.clone());
    }
    // Only then fall back to the standard chain.
    error.source().and_then(remote_cause)
}

/// True when this error is a retirement rather than a fault.
pub fn is_retired(error: &(dyn std::error::Error + 'static)) -> bool {
    matches!(remote_cause(error), Some(RemoteFailure::Cancelled))
}
