//! Bounded, whole-body document GETs — the feed transport beside `service`'s
//! ranged media one (§3.1).
//!
//! A feed document is a different shape of request from finite media: no
//! `Range`, no byte-channel handoff, and a size cap enforced on the running
//! total rather than trusted to `Content-Length` (§3.4). This module reuses
//! the media transport's client and `Limits` but never its runtime, its
//! redirect loop, or its byte channel — `fetch_document` contains no
//! `block_on` and is driven entirely by whichever runtime the caller awaits
//! it on (§3.1).
//!
//! Redirects reuse [`accept_redirect`] unchanged (§3.2): the same hop cap,
//! loop detection, scheme check and downgrade refusal that guard finite
//! media guard a feed document too, rather than a second policy forked here.
//! Conditional GETs (§3.3) are scoped to one resource: a validator is sent
//! only on the hop whose URL matches the validator's own, and dropped the
//! moment a redirect moves the request to a different URL.

use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE,
    IF_NONE_MATCH, LAST_MODIFIED, LOCATION,
};
use serde::{Deserialize, Serialize};
use url::Url;

use super::error::{Operation, Phase, RemoteFailure};
use super::limits::Limits;
use super::response::accept_redirect;
use crate::telemetry::redact_url;

/// §3.5: an XML feed first, everything else as a low-priority fallback.
const ACCEPT_VALUE: &str =
    "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.1";

/// One document request: where to fetch from, and the validators (if any)
/// carried over from a prior fetch of that same URL.
///
/// `validators` is sent as `If-None-Match` / `If-Modified-Since` only on the
/// hop whose request URL equals `validators.url` (§3.3) — never forwarded
/// across a redirect to a different resource.
#[derive(Debug)]
pub struct DocumentRequest {
    pub origin: Url,
    pub validators: Option<CacheValidators>,
}

/// The validators a response supplied, tied to the URL that supplied them
/// (§3.3: a validator identifies a representation of *one* resource, so it
/// must never be forwarded across a redirect to something else).
///
/// URL fields stay `Url` values rather than redacted strings so a cache can
/// compare them exactly — but that also means their `Debug` carries a live
/// URL, so nothing in this crate may log a complete `CacheValidators`,
/// `DocumentRequest` or `DocumentOutcome` `Debug` representation.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct CacheValidators {
    pub url: Url,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// The result of one document fetch.
#[derive(Debug)]
pub enum DocumentOutcome {
    Unchanged {
        final_url: Url,
        permanent_url: Option<Url>,
        validators: CacheValidators,
    },
    Fetched {
        bytes: Vec<u8>,
        final_url: Url,
        permanent_url: Option<Url>,
        validators: CacheValidators,
        content_type: Option<String>,
    },
}

/// The whole operation, timed from the first connect: every redirect hop
/// (none yet, in this task), headers, and the whole body (§3.4).
pub(super) async fn fetch_document(
    client: reqwest::Client,
    limits: Limits,
    request: DocumentRequest,
) -> Result<DocumentOutcome, RemoteFailure> {
    let deadline = tokio::time::Instant::now() + limits.open;
    match tokio::time::timeout_at(deadline, fetch_inner(client, limits, request)).await {
        Err(_) => Err(RemoteFailure::Timeout { phase: Phase::Open }),
        // A hop's own timeout that fired with the whole deadline also spent
        // is the deadline expiring: which of two timers due together the
        // runtime polled first is a race, and `timeout` polls its inner
        // future first. `HttpMediaSource` reclassifies the same way.
        Ok(Err(RemoteFailure::Timeout { .. })) if tokio::time::Instant::now() >= deadline => {
            Err(RemoteFailure::Timeout { phase: Phase::Open })
        }
        Ok(result) => result,
    }
}

/// Reuses `resolve_source`'s policy (`src/app.rs`) as a reference, not a
/// dependency: HTTP(S) only, a host present, no embedded userinfo. Feed and
/// HTTP code must not depend on `app.rs`, so the same three checks are
/// re-expressed here directly against a parsed `Url`.
fn validate_source_policy(url: &Url) -> Result<(), RemoteFailure> {
    let invalid = |reason: &'static str| RemoteFailure::InvalidSource {
        input: redact_url(url.as_str()),
        reason,
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid("expected http(s) with a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("URLs with embedded credentials are not supported"));
    }
    Ok(())
}

/// Runs the manual redirect loop, sending a resource-scoped conditional
/// header on whichever hop's URL matches `request.validators` (§3.2/§3.3),
/// and returns the terminal response plus whether *that* hop's own request
/// carried a conditional header at all.
///
/// `accept_redirect` (`src/http/response.rs`) is reused unchanged: same hop
/// cap, loop detection, scheme check and downgrade refusal as finite media.
/// Redirect targets are additionally re-checked against
/// [`validate_source_policy`], which `accept_redirect` does not enforce
/// (host presence, no embedded userinfo).
async fn follow_redirects(
    client: &reqwest::Client,
    limits: &Limits,
    request: &DocumentRequest,
) -> Result<(reqwest::Response, bool, Option<Url>), RemoteFailure> {
    let mut current = request.origin.clone();
    let mut seen: Vec<Url> = Vec::new();
    let mut hops: u8 = 0;
    let mut permanent_prefix = true;
    let mut permanent_url: Option<Url> = None;

    loop {
        let matching = request.validators.as_ref().filter(|v| v.url == current);
        let mut builder = client
            .get(current.clone())
            .header(ACCEPT_ENCODING, "identity")
            .header(ACCEPT, ACCEPT_VALUE);
        let mut sent_conditional = false;
        if let Some(v) = matching {
            if let Some(etag) = &v.etag {
                builder = builder.header(IF_NONE_MATCH, etag);
                sent_conditional = true;
            }
            if let Some(modified) = &v.last_modified {
                builder = builder.header(IF_MODIFIED_SINCE, modified);
                sent_conditional = true;
            }
        }

        // A cached header value that reqwest cannot encode is caught here,
        // never a panic and never surfaced through the client's own `Debug`:
        // `build()` reports it as an ordinary transport error.
        let built = builder.build().map_err(|error| RemoteFailure::Transport {
            operation: Operation::Open,
            detail: super::service::transport_detail(error),
        })?;

        let response = tokio::time::timeout(limits.headers, client.execute(built))
            .await
            .map_err(|_| RemoteFailure::Timeout {
                phase: Phase::Headers,
            })?
            .map_err(|error| {
                if error.is_connect() && error.is_timeout() {
                    RemoteFailure::Timeout {
                        phase: Phase::Connect,
                    }
                } else {
                    RemoteFailure::Transport {
                        operation: Operation::Open,
                        detail: super::service::transport_detail(error),
                    }
                }
            })?;

        let status = response.status().as_u16();
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Ok((response, sent_conditional, permanent_url));
        }

        // The body of a redirect response is never read (§3.2): only the
        // `Location` header is consulted, and `response` is dropped here.
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let Some(location) = location else {
            return Err(RemoteFailure::Status {
                status,
                operation: Operation::Open,
            });
        };
        drop(response);

        let target = accept_redirect(&current, &location, hops + 1, &seen, limits)?;
        validate_source_policy(&target)?;
        // `permanent_url` advances only through the initial uninterrupted
        // run of 301/308 hops and freezes at the first non-permanent one
        // (§3.2) — this is that rule, unmodified from the design.
        if permanent_prefix && matches!(status, 301 | 308) {
            permanent_url = Some(target.clone());
        } else {
            permanent_prefix = false;
        }
        seen.push(current.clone());
        current = target;
        hops += 1;
    }
}

async fn fetch_inner(
    client: reqwest::Client,
    limits: Limits,
    request: DocumentRequest,
) -> Result<DocumentOutcome, RemoteFailure> {
    validate_source_policy(&request.origin)?;

    let (mut response, sent_conditional, permanent_url) =
        follow_redirects(&client, &limits, &request).await?;

    let status = response.status().as_u16();
    let final_url = response.url().clone();

    match status {
        200 => {
            if let Some(encoding) = response
                .headers()
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok())
                && !encoding.trim().eq_ignore_ascii_case("identity")
            {
                return Err(RemoteFailure::NonIdentityEncoding {
                    encoding: encoding.trim().to_string(),
                });
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let etag = response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let last_modified = response
                .headers()
                .get(LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);

            // A cheap early exit only: the header can be absent or false, so
            // the streaming check below is the authority (§3.4).
            if response
                .content_length()
                .is_some_and(|n| n > limits.document_bytes as u64)
            {
                return Err(RemoteFailure::DocumentTooLarge {
                    limit: limits.document_bytes,
                });
            }
            // No preallocation from that untrusted length: `bytes` starts
            // empty and only ever grows by what was actually received.
            let mut bytes = Vec::new();
            loop {
                let chunk = tokio::time::timeout(limits.stall, response.chunk())
                    .await
                    .map_err(|_| RemoteFailure::Timeout {
                        phase: Phase::Stall,
                    })?
                    .map_err(|error| RemoteFailure::Transport {
                        operation: Operation::Open,
                        detail: super::service::transport_detail(error),
                    })?;
                let Some(chunk) = chunk else { break };
                if chunk.len() > limits.document_bytes.saturating_sub(bytes.len()) {
                    return Err(RemoteFailure::DocumentTooLarge {
                        limit: limits.document_bytes,
                    });
                }
                bytes.extend_from_slice(&chunk);
            }

            Ok(DocumentOutcome::Fetched {
                bytes,
                final_url: final_url.clone(),
                permanent_url,
                validators: CacheValidators {
                    url: final_url,
                    etag,
                    last_modified,
                },
                content_type,
            })
        }
        304 => {
            // §3.3: unsolicited unless this exact hop sent a conditional
            // header *and* a matching cached validator record still exists.
            // Neither 200 body-length nor body-encoding checks apply here:
            // a 304 never carries a body.
            let existing = sent_conditional
                .then(|| request.validators.as_ref().filter(|v| v.url == final_url))
                .flatten();
            let Some(existing) = existing else {
                return Err(RemoteFailure::UnsolicitedNotModified);
            };
            let mut merged = existing.clone();
            if let Some(etag) = response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
            {
                merged.etag = Some(etag.to_string());
            }
            if let Some(last_modified) = response
                .headers()
                .get(LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
            {
                merged.last_modified = Some(last_modified.to_string());
            }
            Ok(DocumentOutcome::Unchanged {
                final_url,
                permanent_url,
                validators: merged,
            })
        }
        other => Err(RemoteFailure::Status {
            status: other,
            operation: Operation::Open,
        }),
    }
}
