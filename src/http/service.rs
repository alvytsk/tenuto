//! The HTTP service: the application's Tokio runtime, the reqwest client,
//! and the one fetch task per source generation.
//!
//! §4's binding rule is that the decode worker must never block on the Tokio
//! runtime. `HttpService::fetch` therefore never awaits anything on the
//! calling thread: it spawns the whole request-plus-body task onto its own
//! runtime and hands back a [`HeaderWait`] the caller blocks on with the same
//! condvar every body read uses. The redirect loop, response validation and
//! body streaming all happen inside that spawned task, never on the caller.

use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT_ENCODING, IF_RANGE, LOCATION, RANGE};
use url::Url;

use super::channel::{ByteChannel, HeaderOutcome, Outcome, SourceInterrupt, WaitHook};
use super::error::{Operation, Phase, RangeRejection, RemoteFailure, redact_url};
use super::limits::Limits;
use super::response::{
    Accepted, Established, FetchAccepted, Headers, accept, accept_redirect, if_range_value,
    validator_from,
};

/// How often the body loop re-tests the freeze flag and re-arms its stall
/// timer. Short enough that a pause is noticed promptly; long enough that
/// this is not a poll loop.
const TICK: Duration = Duration::from_millis(100);

/// One request: where to start, what was already established about the
/// resource (for `If-Range` and validator comparison), and which operation
/// this is for diagnostics.
pub struct FetchRequest {
    pub origin: Url,
    pub start: u64,
    pub established: Option<Established>,
    pub operation: Operation,
}

/// Owns the application's Tokio runtime and the reqwest client.
///
/// One worker thread is enough: there is one active fetch per source
/// generation, never more. The runtime belongs to the application, not the
/// decode worker, so `--probe-only` and the local-file path can run with no
/// runtime at all. Storing the `Runtime` as a plain field is what makes
/// `Drop` shut it down — no bespoke teardown is needed here.
pub struct HttpService {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    limits: Limits,
}

impl HttpService {
    pub fn spawn(limits: Limits) -> Result<Arc<Self>, RemoteFailure> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| RemoteFailure::Transport {
                operation: Operation::Open,
                detail: error.to_string(),
            })?;
        // `Policy::none()` is required, not a preference: §7's hop limit,
        // loop detection, scheme check and downgrade refusal are ours, and
        // reqwest's default redirect policy implements none of them. Letting
        // reqwest follow redirects on its own would silently bypass every
        // one of those rules.
        //
        // HTTP/2's receive window is the transport-level analogue of our own
        // buffer cap: `http2_adaptive_window` defaults to `false`, so without
        // setting these two explicitly a peer may buffer arbitrarily far
        // ahead of what `Limits` promises. reqwest 0.13 has no equivalent
        // knob for HTTP/1.1 (see docs/architecture.md).
        let stream_window = u32::try_from(limits.chunk_bytes).unwrap_or(u32::MAX);
        let connection_window = u32::try_from(limits.buffer_bytes).unwrap_or(u32::MAX);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(limits.connect)
            .http2_initial_stream_window_size(stream_window)
            .http2_initial_connection_window_size(connection_window)
            .build()
            .map_err(|error| RemoteFailure::Transport {
                operation: Operation::Open,
                detail: transport_detail(error),
            })?;
        Ok(Arc::new(Self {
            runtime,
            client,
            limits,
        }))
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The runtime handle `fetch_document`'s caller blocks on.
    ///
    /// `fetch_document` itself never blocks: the CLI bridge calls
    /// `service.handle().block_on(service.fetch_document(req))`, and M5
    /// awaits the same future on this handle's runtime without blocking its
    /// own event loop (§3.1).
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// One whole-body, unranged document GET — the feed transport beside
    /// `fetch`'s media one. Reuses this service's client and `Limits`, never
    /// its runtime: the returned future is driven by whatever runtime the
    /// caller awaits it on.
    pub async fn fetch_document(
        &self,
        request: super::document::DocumentRequest,
    ) -> Result<super::document::DocumentOutcome, RemoteFailure> {
        super::document::fetch_document(self.client.clone(), self.limits, request).await
    }

    /// One request. Follows redirects manually, validates the response, and
    /// spawns the task that streams the body into `channel`.
    ///
    /// Never awaits anything on the calling thread: the whole request is
    /// handed to the runtime via `spawn`, and this returns immediately with a
    /// [`HeaderWait`] the caller blocks on synchronously, exactly as it
    /// blocks on body bytes.
    pub fn fetch(
        &self,
        request: FetchRequest,
        channel: ByteChannel,
        generation: u64,
    ) -> HeaderWait {
        let interrupt = Arc::clone(channel.interrupt());
        let client = self.client.clone();
        let limits = self.limits;
        self.runtime
            .spawn(run_fetch(client, limits, request, channel, generation));
        HeaderWait {
            interrupt,
            generation,
        }
    }
}

/// A thin handle onto the header outcome a spawned fetch task will publish.
///
/// Holds nothing but the interrupt every other wait in this generation
/// already hangs off, plus the generation it belongs to. There is
/// deliberately no private `Mutex`/`Condvar` here: a wait that hung off its
/// own condvar would sleep through `retire()`, because `SourceInterrupt`'s
/// `wake_all` only reaches waits parked on its own three wake channels.
pub struct HeaderWait {
    interrupt: Arc<SourceInterrupt>,
    generation: u64,
}

impl HeaderWait {
    /// Cancellable by the same interrupt every read obeys.
    ///
    /// Returning `Err(HeaderOutcome::Failed(Timeout { .. }))` on this call's
    /// own `deadline` does **not** retire the generation — nothing here does.
    /// `HeaderWait` has no `Drop` either, so a caller that gives up on its
    /// own deadline still leaves the spawned fetch task holding the
    /// connection open, streaming into a buffer nobody will drain, until the
    /// caller retires the generation itself. The caller owns that: it is the
    /// one that knows whether it is about to retry, reopen at a different
    /// byte, or give up for good.
    pub fn wait(
        &self,
        service: &dyn WaitHook,
        deadline: Duration,
    ) -> Result<FetchAccepted, HeaderOutcome> {
        self.interrupt
            .wait_for_headers(self.generation, service, deadline)
    }
}

/// Renders a `reqwest::Error` without the URL its `Display` would otherwise
/// embed verbatim — which, for a signed media URL, is a query string a
/// diagnostic must never carry (§11). `redact_url` reattaches a safe form of
/// the same URL when the error had one at all.
pub(super) fn transport_detail(error: reqwest::Error) -> String {
    let redacted = error.url().map(|url| redact_url(url.as_str()));
    let message = error.without_url().to_string();
    match redacted {
        Some(redacted) => format!("{message} for url ({redacted})"),
        None => message,
    }
}

/// Everything one accepted response established, before its body is read.
struct Opened {
    response: reqwest::Response,
    accepted: Accepted,
    headers: Headers,
    redirects: u8,
}

/// What stays fixed across every request one fetch task makes: the opening
/// one and any resume after a body ended short.
struct Fetch {
    client: reqwest::Client,
    limits: Limits,
    interrupt: Arc<SourceInterrupt>,
    generation: u64,
    origin: Url,
    operation: Operation,
}

impl Fetch {
    /// One request from `origin`, redirects followed and the response validated
    /// (§7). `Ok(None)` means the generation was cancelled while waiting: the
    /// caller returns without publishing anything, as every other cancelled wait
    /// does.
    async fn request(
        &self,
        start: u64,
        established: Option<&Established>,
    ) -> Result<Option<Opened>, RemoteFailure> {
        let Self {
            client,
            limits,
            interrupt,
            generation,
            origin,
            operation,
        } = self;
        let (generation, operation) = (*generation, *operation);
        let mut current = origin.clone();
        let mut seen: Vec<Url> = Vec::new();
        let mut redirects: u8 = 0;

        // Manual redirect loop: reqwest's own policy is disabled (`spawn`'s
        // client is built with `Policy::none()`), so every hop is validated here
        // before it is followed, and the count is bounded by `limits.max_redirects`.
        let response = loop {
            let mut builder = client
                .get(current.clone())
                .header(RANGE, format!("bytes={start}-"))
                .header(ACCEPT_ENCODING, "identity");
            if let Some(established) = established
                && let Some(if_range) = if_range_value(&established.validator)
            {
                builder = builder.header(IF_RANGE, if_range);
            }
            let built = builder.build().map_err(|error| RemoteFailure::Transport {
                operation,
                detail: transport_detail(error),
            })?;

            // Dropping the request future on timeout is fine here — the request
            // is being abandoned outright — and it does not re-test the freeze,
            // which is also fine: a pause cannot arrive before the source it
            // would pause exists yet.
            let response = tokio::select! {
                biased;
                () = interrupt.cancelled(generation) => return Ok(None),
                result = client.execute(built) => result.map_err(|error| RemoteFailure::Transport {
                    operation,
                    detail: transport_detail(error),
                })?,
                () = interrupt.sleep(limits.headers) => {
                    return Err(RemoteFailure::Timeout { phase: Phase::Headers });
                }
            };

            let status = response.status().as_u16();
            let location = if (300..400).contains(&status) {
                response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            } else {
                None
            };

            let Some(location) = location else {
                break response;
            };

            let target = accept_redirect(&current, &location, redirects + 1, &seen, limits)?;
            // §11: redirect count, one line per hop rather than a single total
            // at the end - bounded by `limits.max_redirects`, never per-chunk, so
            // this cannot grow into the per-frame noise §11 rules out.
            tracing::debug!(
                hop = redirects + 1,
                url = %redact_url(target.as_str()),
                "following redirect"
            );
            seen.push(current.clone());
            current = target;
            redirects += 1;
        };

        let status = response.status().as_u16();
        let headers = Headers::from_map(response.headers());
        let accepted = accept(status, &headers, start, start == 0, established)?;
        Ok(Some(Opened {
            response,
            accepted,
            headers,
            redirects,
        }))
    }
}

/// The request-plus-body task `HttpService::fetch` spawns: one per source
/// generation, running entirely on the service's runtime.
///
/// A ranged body that ends short is resumed in place: one more range
/// request from the byte the bytes stopped at, carrying `If-Range`, feeding
/// the same channel, so the reader never notices. This is how a stream a CDN
/// dropped while the player sat paused (nginx's `send_timeout`) comes back.
/// It is not a retry loop: a response must have delivered at least one
/// transfer chunk before its end is worth resuming from, so a server that
/// keeps dying early fails the attempt on the first response that proves it.
async fn run_fetch(
    client: reqwest::Client,
    limits: Limits,
    request: FetchRequest,
    channel: ByteChannel,
    generation: u64,
) {
    let FetchRequest {
        origin,
        mut start,
        established,
        operation,
    } = request;
    let fetch = Fetch {
        client,
        limits,
        interrupt: Arc::clone(channel.interrupt()),
        generation,
        origin,
        operation,
    };
    let Fetch {
        limits, interrupt, ..
    } = &fetch;

    let opened = fetch.request(start, established.as_ref()).await;
    let Opened {
        mut response,
        accepted,
        headers,
        redirects,
    } = match opened {
        Ok(Some(opened)) => opened,
        Ok(None) => return,
        Err(failure) => {
            interrupt.publish_headers(generation, Err(failure));
            return;
        }
    };

    // The interval this response promises, so the body loop can tell a
    // short body (§7: `TruncatedBody`) from a body that simply ended a
    // smaller-than-total interval exactly where it said it would (`Eof`,
    // not media EOF).
    let (mut advertised, total, resumable) = match accepted {
        Accepted::Sequential { len } => (len, len, false),
        Accepted::Ranged { range } => (range.len(), range.total, true),
        Accepted::Live => (None, None, false),
    };
    let live = matches!(accepted, Accepted::Live);

    let validator = validator_from(&headers);
    // What every resumed request compares its response against: the
    // caller's own established facts when it had them, else this response's.
    let established = established.unwrap_or(Established {
        total,
        validator: validator.clone(),
    });
    interrupt.publish_headers(
        generation,
        Ok(FetchAccepted {
            accepted,
            validator,
            headers,
            redirects,
        }),
    );

    let mut delivered: u64 = 0;
    // Never zero: `bytes.chunks(0)` panics, and `Limits` is injectable with
    // no validating constructor, so a caller-supplied zero must not reach it.
    let chunk_cap = limits.chunk_bytes.max(1);
    loop {
        // Pinned once per chunk, polled across many timer slices. `&mut
        // chunk` in the select leaves the future in place when another
        // branch wins, so nothing is dropped mid-poll and no delivered bytes
        // are lost — `Response::chunk` is not documented cancel-safe, and
        // rebuilding it on every slice could lose buffered data. Scoped so
        // the borrow of `response` ends before a resume may replace it.
        let next = {
            let mut chunk = pin!(response.chunk());
            let mut demanded = Duration::ZERO;
            let mut last = interrupt.now();
            loop {
                // Deliberately no `wait_while_frozen` here (fix round 1):
                // delivery must not be gated on the freeze level. `ByteChannel::
                // push` already blocks on capacity once the buffer is full, and
                // that backpressure — not a producer-side freeze gate — is what
                // bounds a paused fetch. Gating delivery here instead deadlocked
                // a seek or a reopen taken while paused: both need bytes from a
                // fetch task that would otherwise sit here until a `Play` that
                // may never come (§9). Only the stall-timer's own charging
                // (below) still checks the level.
                let slice = TICK.min(limits.stall - demanded);
                tokio::select! {
                    biased;
                    () = interrupt.cancelled(generation) => return,
                    result = &mut chunk => break result,
                    () = tokio::time::sleep(slice) => {
                        // Only unfrozen time is charged. A freeze that lands
                        // inside the sleep is caught on the next slice.
                        let now = interrupt.now();
                        if !interrupt.is_frozen() {
                            demanded += now.duration_since(last);
                        }
                        last = now;
                        if demanded >= limits.stall {
                            channel.finish(generation, Outcome::Failed(
                                RemoteFailure::Timeout { phase: Phase::Stall },
                            ));
                            return;
                        }
                    }
                }
            }
        };

        let ended = match next {
            Ok(Some(bytes)) => {
                let mut offset = 0usize;
                while offset < bytes.len() {
                    let (piece, exceeded) =
                        clamp_to_advertised(chunk_cap, advertised, delivered, &bytes[offset..]);
                    let took = piece.len();
                    // Bytes past the advertised interval belong to different
                    // media and must never reach a reader — clamped and
                    // refused *before* the push, not pushed and checked
                    // after, which would have already handed a decoder up to
                    // `chunk_bytes` of the wrong recording.
                    if !piece.is_empty() && !channel.push(generation, piece).await {
                        return;
                    }
                    delivered += took as u64;
                    offset += took;
                    if exceeded {
                        channel.finish(
                            generation,
                            Outcome::Failed(RemoteFailure::InvalidRange {
                                reason: RangeRejection::LengthMismatch,
                            }),
                        );
                        return;
                    }
                }
                drop(bytes);
                continue;
            }
            Ok(None) => None,
            Err(error) => Some(transport_detail(error)),
        };

        let short = advertised.is_some_and(|total| delivered < total);
        if short && resumable && delivered >= chunk_cap as u64 {
            start += delivered;
            // §11: reconnect, with the transport's own reason — which the
            // `TruncatedBody` below would otherwise discard.
            tracing::info!(
                url = %redact_url(fetch.origin.as_str()),
                start_byte = start,
                reason = ended.as_deref().unwrap_or("body ended early"),
                "resuming interrupted body"
            );
            match fetch.request(start, Some(&established)).await {
                Ok(Some(opened)) => {
                    advertised = match opened.accepted {
                        Accepted::Ranged { range } => range.len(),
                        // `accept` refuses a 200 past byte zero, so a
                        // resumed request can only ever be ranged.
                        Accepted::Sequential { len } => len,
                        Accepted::Live => None,
                    };
                    response = opened.response;
                    delivered = 0;
                    continue;
                }
                Ok(None) => return,
                Err(failure) => {
                    channel.finish(generation, Outcome::Failed(failure));
                    return;
                }
            }
        }
        if let Some(detail) = &ended {
            tracing::debug!(delivered, detail, "body ended with a transport error");
        }
        let outcome = if live {
            // M7 §4: every end of a live body is a disconnect, a clean one
            // included. `Eof` here would drain to `EndOfTrack`.
            Outcome::Failed(RemoteFailure::LiveEnded)
        } else {
            classify_body_end(advertised, delivered, operation, ended)
        };
        channel.finish(generation, outcome);
        return;
    }
}

/// Clamp one raw piece of body to at most `chunk_cap` bytes and, when a
/// total is advertised, to no more than what remains of it.
///
/// Pure and pinned by a direct unit test rather than only by what a real
/// transport happens to be willing to hand a decoder: a `Content-Length`
/// body can never legitimately overshoot what its own header promised — a
/// standards-observing client enforces that as a hard cap itself, which two
/// throwaway probes against hyper 1.11.1 confirmed (a body that fully
/// satisfies its declared length never yields another chunk or an error, and
/// a server that lies with a *shorter* `Content-Length` than it writes never
/// hands the excess to `chunk()` at all). So this boundary — the second half
/// of it, specifically, "flag and stop before pushing the excess" — cannot be
/// driven by any conformant origin, and a unit test is the only sound way to
/// pin it.
///
/// Returns the piece to push and whether the advertised total was reached or
/// crossed by it, in which case nothing past `piece` may be processed.
fn clamp_to_advertised(
    chunk_cap: usize,
    advertised: Option<u64>,
    delivered: u64,
    raw: &[u8],
) -> (&[u8], bool) {
    let want = chunk_cap.min(raw.len());
    match advertised {
        Some(total) => {
            let remaining = total.saturating_sub(delivered);
            if remaining == 0 {
                return (&raw[..0], true);
            }
            // `remaining` is capped against `want`, itself a `usize`, before
            // the cast back — it never truncates.
            let allowed = remaining.min(want as u64) as usize;
            (&raw[..allowed], allowed < want)
        }
        None => (&raw[..want], false),
    }
}

/// Classify how the body loop's outstanding read ended, once it is known to
/// have ended: how many bytes this response advertised (if any), how many
/// were actually delivered, and — if an error ended it — its already-redacted
/// detail text.
///
/// Pure, and unit-tested directly for the same reason as
/// [`clamp_to_advertised`]: a real `Content-Length` body can only ever end in
/// `Err` when short (confirmed empirically — see that function's doc), never
/// in a clean `Ok(None)`, which makes the `delivered == total` side of this
/// match — an error with nothing left owed — impossible to drive through a
/// real socket. Merging the `Ok(None)` and `Err` signals here, rather than
/// mapping every `Err` straight to `Transport`, is what makes a short body
/// report `TruncatedBody` at all: a real HTTP/1.1 origin signals a shortfall
/// as an `Err`, never as a clean `Ok(None)`.
fn classify_body_end(
    advertised: Option<u64>,
    delivered: u64,
    operation: Operation,
    error: Option<String>,
) -> Outcome {
    match advertised {
        Some(total) if delivered < total => Outcome::Failed(RemoteFailure::TruncatedBody {
            missing: total - delivered,
        }),
        _ => match error {
            None => Outcome::Eof,
            Some(detail) => Outcome::Failed(RemoteFailure::Transport { operation, detail }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_piece_within_the_advertised_total_is_pushed_whole_and_not_flagged() {
        let (piece, exceeded) = clamp_to_advertised(64, Some(10), 4, b"abcdef");
        assert_eq!(piece, b"abcdef");
        assert!(!exceeded);
    }

    #[test]
    fn a_piece_that_would_cross_the_advertised_total_is_clamped_and_flagged() {
        let (piece, exceeded) = clamp_to_advertised(64, Some(10), 8, b"abcdef");
        assert_eq!(piece, b"ab", "only the two bytes still owed may be pushed");
        assert!(exceeded);
    }

    #[test]
    fn nothing_is_pushed_once_the_advertised_total_is_already_reached() {
        let (piece, exceeded) = clamp_to_advertised(64, Some(10), 10, b"abcdef");
        assert!(
            piece.is_empty(),
            "not one byte past the total may reach a reader"
        );
        assert!(exceeded);
    }

    #[test]
    fn without_an_advertised_total_a_piece_is_bounded_only_by_the_chunk_cap() {
        let (piece, exceeded) = clamp_to_advertised(4, None, 1_000_000, b"abcdefgh");
        assert_eq!(piece, b"abcd");
        assert!(!exceeded);
    }

    #[test]
    fn a_shortfall_is_truncated_whether_or_not_an_error_carried_it() {
        assert_eq!(
            classify_body_end(Some(100), 40, Operation::Open, Some("reset".to_string())),
            Outcome::Failed(RemoteFailure::TruncatedBody { missing: 60 })
        );
        // A real Content-Length body never signals a shortfall this way —
        // the classifier must not depend on that to stay correct.
        assert_eq!(
            classify_body_end(Some(100), 40, Operation::Open, None),
            Outcome::Failed(RemoteFailure::TruncatedBody { missing: 60 })
        );
    }

    #[test]
    fn an_error_once_the_advertised_total_is_fully_delivered_is_transport_not_truncation() {
        assert_eq!(
            classify_body_end(Some(100), 100, Operation::Open, Some("reset".to_string())),
            Outcome::Failed(RemoteFailure::Transport {
                operation: Operation::Open,
                detail: "reset".to_string(),
            })
        );
    }

    #[test]
    fn an_error_with_nothing_advertised_is_transport() {
        assert_eq!(
            classify_body_end(None, 40, Operation::Open, Some("reset".to_string())),
            Outcome::Failed(RemoteFailure::Transport {
                operation: Operation::Open,
                detail: "reset".to_string(),
            })
        );
    }

    #[test]
    fn a_clean_end_at_or_without_an_advertised_total_is_eof() {
        assert_eq!(
            classify_body_end(Some(100), 100, Operation::Open, None),
            Outcome::Eof
        );
        assert_eq!(
            classify_body_end(None, 40, Operation::Open, None),
            Outcome::Eof
        );
    }
}
