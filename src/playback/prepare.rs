//! One path from a [`SourceLocation`] to an open decoder plus evidence-backed
//! capabilities, for local files and HTTP alike (R5).
//!
//! §6's continuity/seek table lives here, applied uniformly: `DecodedSource`
//! already folds transport evidence into `capabilities()` (Ruling 4), so this
//! module's job is only to build that evidence for each kind of location and
//! then translate the one capability fact §6 refuses — `Unresolved` — into
//! the error R3 calls for.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSource;
use url::Url;

use crate::http::channel::{SourceInterrupt, WaitHook};
use crate::http::error::{RemoteFailure, redact_url};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::http::source::{HttpMediaSource, remote_cause};
use crate::media::capabilities::{Continuity, MediaCapabilities};
use crate::media::id::AbsolutePath;
use crate::media::source::SourceLocation;

use super::decode::DecodedSource;
use super::error::PlaybackError;

pub struct Prepared {
    pub source: DecodedSource,
    pub capabilities: MediaCapabilities,
}

/// Everything `prepare` needs beyond the location itself.
pub struct PrepareContext {
    /// `None` for local-only sessions — `--probe-only` on a file, and every
    /// test that never touches the network.
    pub http: Option<Arc<HttpService>>,
    pub interrupt: Arc<SourceInterrupt>,
    pub hook: Arc<dyn WaitHook>,
    pub limits: Limits,
    /// The continuity an established session already has. A prepared source
    /// that disagrees is refused before anything adopts it (M7 §3.5).
    pub expected: Option<Continuity>,
}

/// Open `location` and classify it, refusing anything that is not provably
/// finite before it is handed back.
pub fn prepare(
    location: &SourceLocation,
    context: &PrepareContext,
) -> Result<Prepared, PlaybackError> {
    let source = match location {
        SourceLocation::LocalPath(path) => open_local(path)?,
        SourceLocation::Http(url) => open_http(url, context)?,
    };
    let capabilities = source.capabilities();
    if let Some(expected) = context.expected
        && expected != capabilities.continuity
    {
        return Err(RemoteFailure::ResourceChanged.into());
    }
    match capabilities.continuity {
        // An unresolved source has proven neither that it ends nor that it
        // does not; it is still refused. A live one now plays (M7).
        Continuity::Unresolved => Err(RemoteFailure::ContinuityUndetermined.into()),
        Continuity::Finite | Continuity::Indefinite => Ok(Prepared {
            source,
            capabilities,
        }),
    }
}

fn open_local(path: &Path) -> Result<DecodedSource, PlaybackError> {
    let canonical = path.canonicalize().map_err(|source| PlaybackError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let absolute =
        AbsolutePath::new(canonical).map_err(|error| PlaybackError::UnsupportedInput {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    DecodedSource::open(&absolute)
}

fn open_http(url: &Url, context: &PrepareContext) -> Result<DecodedSource, PlaybackError> {
    let service = context
        .http
        .as_ref()
        .ok_or_else(|| RemoteFailure::InvalidSource {
            input: redact_url(url.as_str()),
            reason: "no HTTP service in this session",
        })?;

    let mut hint = Hint::new();
    if let Some(extension) = extension_from_url(url) {
        hint.with_extension(&extension);
    }
    let label = PathBuf::from(redact_url(url.as_str()));
    let probed = HttpMediaSource::open_and_probe(
        Arc::clone(service),
        url.clone(),
        Arc::clone(&context.interrupt),
        Arc::clone(&context.hook),
        context.limits,
        |source| {
            // Evidence and the station name are read off the source before
            // it is boxed and consumed by the probe — there is no way back
            // to it afterwards.
            let evidence = source.evidence();
            let station = source
                .station_identity()
                .and_then(|identity| identity.name.clone());
            // Symphonia's own format probe does not preserve read errors on
            // every path: `Probe::next` scans byte-by-byte for a marker with
            // `while let Ok(byte) = mss.read_byte()`, and a failing read there
            // simply ends the loop and is reported as a generic "no suitable
            // format reader found", with no wrapped cause at all — verified
            // against symphonia-core 0.6.1. Latching the failure at the read
            // itself, before Symphonia gets a chance to lose it, is the only
            // place this project can reliably recover *why* a remote probe
            // failed.
            let latch = Arc::new(FailureLatch::default());
            let latching = LatchingSource {
                inner: source,
                latch: Arc::clone(&latch),
            };
            DecodedSource::from_media_source(Box::new(latching), hint, label, evidence)
                .map(|decoded| (decoded, station))
                .map_err(|error| promote_latched(error, &latch))
        },
    )?;
    let (mut decoded, station) = probed?;
    if let Some(name) = station {
        decoded.set_fallback_title(name);
    }
    Ok(decoded)
}

/// A poisoned lock means a thread already panicked while holding it; there is
/// nothing better to do than carry on with the state it left.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The most recent [`RemoteFailure`] a read or seek produced, cleared the
/// moment a later one succeeds.
///
/// Kept separate from `LatchingSource` so this exact rule — last failure
/// wins, a success clears it — is unit-testable against fabricated
/// `io::Result`s, with no real socket or `TestServer` involved: this project
/// has no test server reachable from a `src/` unit test (`tests/support` is
/// only visible to integration tests), and reliably provoking Symphonia's own
/// *specific* recovery path (`probe_trailing`'s tolerated anchored-metadata
/// read, `probe.rs:475-544`) through a real fixture is impractical — it fires
/// only when trailing bytes happen to match a real tag format's marker. The
/// state machine below is what actually has to be correct; testing it
/// directly is more reliable than hoping to reproduce Symphonia's internals.
///
/// Last-failure-wins matters because `probe_trailing`'s own comment calls
/// tolerating a failed anchored-metadata read "reasonable" — a stall
/// Symphonia recovered from is not why it eventually gave up, and must not
/// still be sitting in the latch by the time something else decides the
/// outcome; an unrelated local failure downstream (an unsupported channel
/// count, say) must not be misreported as that stale network fault.
#[derive(Default)]
struct FailureLatch(Mutex<Option<RemoteFailure>>);

impl FailureLatch {
    /// Fold one `io::Result` into the latch: a success clears it, a failure
    /// with a recoverable remote cause replaces whatever was there.
    fn observe<T>(&self, result: &io::Result<T>) {
        match result {
            Ok(_) => *lock(&self.0) = None,
            Err(error) => {
                if let Some(failure) = remote_cause(error) {
                    *lock(&self.0) = Some(failure);
                }
            }
        }
    }

    fn get(&self) -> Option<RemoteFailure> {
        lock(&self.0).clone()
    }
}

/// Wraps `HttpMediaSource`, feeding every read and seek outcome into a
/// [`FailureLatch`] before handing the (possibly since-mangled) error onward
/// to Symphonia. See `open_http`'s comment for why this exists.
struct LatchingSource {
    inner: HttpMediaSource,
    latch: Arc<FailureLatch>,
}

impl io::Read for LatchingSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let result = self.inner.read(out);
        self.latch.observe(&result);
        result
    }
}

impl io::Seek for LatchingSource {
    fn seek(&mut self, from: io::SeekFrom) -> io::Result<u64> {
        let result = self.inner.seek(from);
        self.latch.observe(&result);
        result
    }
}

impl MediaSource for LatchingSource {
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

/// Override a decode failure with the latched remote cause, if one was
/// recorded. A failure that never touched the network (an unrecognised local
/// format, say), or one that did but was later superseded by a success,
/// leaves the latch empty and passes through unchanged; an already-typed
/// `PlaybackError::Remote` (a failure Symphonia happened not to mangle) is
/// left alone too, since the latch can only agree with it.
fn promote_latched(error: PlaybackError, latch: &FailureLatch) -> PlaybackError {
    match error {
        PlaybackError::Remote(failure) => PlaybackError::Remote(failure),
        other => match latch.get() {
            Some(failure) => PlaybackError::Remote(failure),
            None => other,
        },
    }
}

/// The last path segment's extension, if any, so the format probe gets the
/// same hint a local `open` would give it from a file extension.
fn extension_from_url(url: &Url) -> Option<String> {
    let last_segment = url.path_segments()?.next_back()?;
    let (_, extension) = last_segment.rsplit_once('.')?;
    Some(extension.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::error::Phase;
    use crate::http::source::RemoteIoError;

    fn timeout(phase: Phase) -> io::Error {
        io::Error::other(RemoteIoError(RemoteFailure::Timeout { phase }))
    }

    fn unsupported_input() -> PlaybackError {
        PlaybackError::UnsupportedInput {
            path: PathBuf::from("remote"),
            reason: "5 channels; M1 supports mono and stereo".into(),
        }
    }

    #[test]
    fn a_failure_is_latched_and_then_promoted_over_a_decode_error() {
        let latch = FailureLatch::default();
        latch.observe::<()>(&Err(timeout(Phase::Stall)));
        let promoted = promote_latched(unsupported_input(), &latch);
        assert!(
            matches!(
                promoted,
                PlaybackError::Remote(RemoteFailure::Timeout {
                    phase: Phase::Stall
                })
            ),
            "{promoted}"
        );
    }

    #[test]
    fn a_later_success_clears_an_earlier_latched_failure() {
        let latch = FailureLatch::default();
        latch.observe::<()>(&Err(timeout(Phase::Stall)));
        assert_eq!(
            latch.get(),
            Some(RemoteFailure::Timeout {
                phase: Phase::Stall
            })
        );
        latch.observe(&Ok(()));
        assert_eq!(latch.get(), None);
    }

    #[test]
    fn a_transient_failure_recovered_before_an_unrelated_local_error_is_not_reported() {
        // The scenario the review flagged: `probe_trailing` tolerates and
        // continues past a failed anchored-metadata read, so a stall from
        // that read must not outlive the success that follows it — and once
        // it is cleared, an unrelated local decode failure must be reported
        // as itself, not misclassified as the stale network fault.
        let latch = FailureLatch::default();
        latch.observe::<()>(&Err(timeout(Phase::Stall))); // transient …
        latch.observe(&Ok(())); // … recovered …
        let promoted = promote_latched(unsupported_input(), &latch); // … unrelated local failure.
        assert!(
            matches!(promoted, PlaybackError::UnsupportedInput { .. }),
            "a stale, already-recovered remote failure was reported instead of the real local one: {promoted}"
        );
    }

    #[test]
    fn a_later_failure_replaces_an_earlier_one_still_latched() {
        let latch = FailureLatch::default();
        latch.observe::<()>(&Err(timeout(Phase::Stall)));
        latch.observe::<()>(&Err(timeout(Phase::Open)));
        assert_eq!(
            latch.get(),
            Some(RemoteFailure::Timeout { phase: Phase::Open })
        );
    }
}
