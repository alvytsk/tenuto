//! §7's acceptance rules, as pure functions.
//!
//! Nothing here touches a socket. That is deliberate: §7 is fifteen separate
//! rules, and a rule that can only be reached through a live server is a rule
//! whose failure mode is a flaky test rather than an assertion.

use url::Url;

use super::error::{Operation, RangeRejection, RedirectRejection, RemoteFailure};
use super::limits::Limits;

/// A case-insensitive header view, constructible from a live `HeaderMap` or
/// from a literal slice, so every rule below is assertable without a request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), (*value).to_string()))
                .collect(),
        )
    }

    pub fn from_map(map: &http::HeaderMap) -> Self {
        Self(
            map.iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
                })
                .collect(),
        )
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.0
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    pub first: u64,
    pub last: u64,
    pub total: Option<u64>,
}

impl ByteRange {
    /// Inclusive interval, so the body is `last - first + 1` bytes.
    ///
    /// Checked, because `last` comes off the wire: `bytes 0-18446744073709551615/*`
    /// parses fine and overflows a bare `+ 1`, which in release mode wraps to
    /// zero and turns a hostile header into a silently empty interval.
    // Fires even for a non-`usize` return; there is no empty `ByteRange`, since
    // an inclusive interval always covers at least the byte it names.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> Option<u64> {
        self.last.checked_sub(self.first)?.checked_add(1)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Validator {
    /// Strong entity tag, quotes included. A `W/` tag is deliberately not
    /// stored here: §7 forbids sending a weak validator as `If-Range`, and
    /// keeping the two apart at parse time makes that unforgettable.
    pub strong_etag: Option<String>,
    /// A `W/`-prefixed tag. Never sent as `If-Range` — but §7 says range access
    /// without a strong validator is *best effort*, which means comparing the
    /// metadata that is available, not discarding it.
    pub weak_etag: Option<String>,
    pub last_modified: Option<String>,
}

/// What one accepted response established about *the resource at one URL*:
/// its total length, its validator, and the URL whose response supplied
/// both.
///
/// The URL is what scopes the validator. An `ETag` identifies a
/// representation of a single resource, so `If-Range` is sent only on a
/// request hop that lands on exactly this URL — never forwarded across a
/// redirect to a different one, which stands for a different resource the
/// validator says nothing about (the document transport scopes its
/// conditional headers the same way).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Established {
    pub url: Url,
    pub total: Option<u64>,
    pub validator: Validator,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Accepted {
    /// Usable as a stream from byte zero, and only from byte zero.
    Sequential {
        len: Option<u64>,
    },
    Ranged {
        range: ByteRange,
    },
    /// An ongoing stream: no length, no ranges, and no end that is not a
    /// disconnect (M7 §4).
    Live,
}

/// What a validated response established: how its bytes may be used, the
/// validator to compare against on any later request, its headers, how many
/// redirects were followed to reach it, and the URL of the hop that produced
/// it — the terminal hop, which is the URL a later `If-Range` is scoped to.
///
/// It lives here rather than beside the fetch that produces it because every
/// field is a response-acceptance value, and the byte channel must be able to
/// name the type without depending on the HTTP client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchAccepted {
    pub url: Url,
    pub accepted: Accepted,
    pub validator: Validator,
    pub headers: Headers,
    pub redirects: u8,
}

pub fn parse_content_range(value: &str) -> Result<ByteRange, RangeRejection> {
    let rest = value
        .trim()
        .strip_prefix("bytes ")
        .ok_or(RangeRejection::Malformed)?;
    let (interval, total) = rest.split_once('/').ok_or(RangeRejection::Malformed)?;
    let (first, last) = interval.split_once('-').ok_or(RangeRejection::Malformed)?;
    let first: u64 = first
        .trim()
        .parse()
        .map_err(|_| RangeRejection::Malformed)?;
    let last: u64 = last.trim().parse().map_err(|_| RangeRejection::Malformed)?;
    if last < first {
        return Err(RangeRejection::ReversedInterval);
    }
    let total = match total.trim() {
        "*" => None,
        digits => Some(digits.parse().map_err(|_| RangeRejection::Malformed)?),
    };
    let range = ByteRange { first, last, total };
    // An interval that cannot fit inside its own total is impossible, not
    // merely odd: `bytes 0-99/10` describes 100 bytes of a 10-byte object.
    if let Some(total) = total
        && last >= total
    {
        return Err(RangeRejection::IntervalPastTotal);
    }
    // And an interval whose length does not compute at all is malformed.
    if range.len().is_none() {
        return Err(RangeRejection::Malformed);
    }
    Ok(range)
}

pub fn validator_from(headers: &Headers) -> Validator {
    let etag = headers.get("etag").map(str::trim);
    let strong_etag = etag
        .filter(|tag| !tag.starts_with("W/") && tag.starts_with('"') && tag.ends_with('"'))
        .map(str::to_string);
    let weak_etag = etag.filter(|tag| tag.starts_with("W/")).map(str::to_string);
    Validator {
        strong_etag,
        weak_etag,
        last_modified: headers.get("last-modified").map(str::to_string),
    }
}

/// The `If-Range` header value, or `None` when there is no strong validator.
pub fn if_range_value(validator: &Validator) -> Option<String> {
    validator.strong_etag.clone()
}

fn is_hls(headers: &Headers) -> bool {
    headers.get("content-type").is_some_and(|value| {
        value
            .trim()
            .eq_ignore_ascii_case("application/vnd.apple.mpegurl")
    })
}

/// Explicit ICY semantics. Deliberately narrow: continuity is never inferred
/// from a missing `Content-Length`, chunked framing, a MIME type or a suffix.
fn is_icy(headers: &Headers) -> bool {
    headers.get("icy-name").is_some() || headers.get("icy-br").is_some()
}

/// The live classification of a *successful* response, `None` when it is not
/// live. Called only from `accept`'s 200 and 206 arms, after status, encoding,
/// multipart and validator checks — an error status is never audio.
fn classify_live(headers: &Headers) -> Option<Result<(), RemoteFailure>> {
    if is_hls(headers) {
        return Some(Err(RemoteFailure::UnsupportedLiveMedia));
    }
    // Only a positive interval means the body is interleaved. StreamGuys
    // answers every client that did not ask for metadata with
    // `icy-metaint: 0`, so presence alone proves nothing (M7 §12 models the
    // value as `Option<NonZeroU32>` for the same reason).
    if headers
        .get("icy-metaint")
        .and_then(|value| value.trim().parse::<u32>().ok())
        .is_some_and(|interval| interval > 0)
    {
        return Some(Err(RemoteFailure::IcyFramingUnsupported));
    }
    is_icy(headers).then_some(Ok(()))
}

/// Decide whether a response may be installed at `requested_start`.
///
/// `at_origin` is true only while opening at byte zero — the single case in
/// which a 200 is usable (§7).
pub fn accept(
    status: u16,
    headers: &Headers,
    requested_start: u64,
    at_origin: bool,
    established: Option<&Established>,
) -> Result<Accepted, RemoteFailure> {
    if let Some(encoding) = headers.get("content-encoding")
        && !encoding.trim().eq_ignore_ascii_case("identity")
    {
        return Err(RemoteFailure::NonIdentityEncoding {
            encoding: encoding.trim().to_string(),
        });
    }
    if headers
        .get("content-type")
        .is_some_and(|value| value.trim().to_ascii_lowercase().starts_with("multipart/"))
    {
        return Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::Multipart,
        });
    }
    check_validator(headers, established)?;

    let declared_len = headers
        .get("content-length")
        .and_then(|v| v.trim().parse().ok());
    match status {
        200 => {
            if !at_origin {
                return Err(RemoteFailure::InvalidRange {
                    reason: RangeRejection::RangeIgnored,
                });
            }
            if let Some(live) = classify_live(headers) {
                return live.map(|()| Accepted::Live);
            }
            // §7's best-effort rule cuts both ways: without a strong validator
            // we may not claim to detect a same-length replacement, but a
            // length that contradicts the total already established is
            // evidence we do have, and ignoring it is how a reopened
            // range-less source silently starts decoding different media.
            if let (Some(declared), Some(known)) = (declared_len, established.and_then(|e| e.total))
                && declared != known
            {
                return Err(RemoteFailure::ResourceChanged);
            }
            Ok(Accepted::Sequential { len: declared_len })
        }
        206 => {
            let value = headers
                .get("content-range")
                .ok_or(RemoteFailure::InvalidRange {
                    reason: RangeRejection::Malformed,
                })?;
            let range = parse_content_range(value)
                .map_err(|reason| RemoteFailure::InvalidRange { reason })?;
            if range.first != requested_start {
                return Err(RemoteFailure::InvalidRange {
                    reason: RangeRejection::WrongStart,
                });
            }
            if let Some(live) = classify_live(headers) {
                live?;
                if requested_start != 0 {
                    return Err(RemoteFailure::InvalidRange {
                        reason: RangeRejection::WrongStart,
                    });
                }
                return Ok(Accepted::Live);
            }
            if let Some(declared) = declared_len
                && Some(declared) != range.len()
            {
                return Err(RemoteFailure::InvalidRange {
                    reason: RangeRejection::LengthMismatch,
                });
            }
            if let (Some(total), Some(known)) = (range.total, established.and_then(|e| e.total))
                && total != known
            {
                return Err(RemoteFailure::InvalidRange {
                    reason: RangeRejection::ConflictingTotal,
                });
            }
            Ok(Accepted::Ranged { range })
        }
        // Never completion. The one benign case — seeking to a known byte EOF —
        // is answered locally in `HttpMediaSource` without a request, so any
        // 416 that actually reaches here is a failure.
        416 => Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::Unsatisfiable,
        }),
        other => Err(RemoteFailure::Status {
            status: other,
            operation: if at_origin {
                Operation::Open
            } else {
                Operation::Seek
            },
        }),
    }
}

/// A validator that changed means the bytes are no longer the same recording.
///
/// §7 is a *hierarchy*, not a single rule. A strong ETag is conclusive. Without
/// one, range access is best effort — which obliges us to compare the metadata
/// that is available rather than to ignore it. What best effort forbids is the
/// opposite claim: that an unchanged weak validator *proves* the content is the
/// same. So a changed weak ETag or a changed `Last-Modified` fails, and an
/// absent or unchanged one proves nothing and is allowed through.
fn check_validator(
    headers: &Headers,
    established: Option<&Established>,
) -> Result<(), RemoteFailure> {
    let Some(established) = established else {
        return Ok(());
    };
    let known = &established.validator;
    let fresh = validator_from(headers);
    let changed = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(known), Some(got)) => known != got,
        // An absent validator on either side is no evidence either way.
        _ => false,
    };
    if changed(&known.strong_etag, &fresh.strong_etag)
        || changed(&known.weak_etag, &fresh.weak_etag)
        || changed(&known.last_modified, &fresh.last_modified)
    {
        return Err(RemoteFailure::ResourceChanged);
    }
    Ok(())
}

pub fn accept_redirect(
    from: &Url,
    location: &str,
    hops: u8,
    seen: &[Url],
    limits: &Limits,
) -> Result<Url, RemoteFailure> {
    if hops > limits.max_redirects {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::TooMany,
        });
    }
    // A location starting with ":" indicates a malformed scheme per RFC 3986.
    if location.starts_with(':') {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::InvalidLocation,
        });
    }
    let target = from.join(location).map_err(|_| RemoteFailure::Redirect {
        reason: RedirectRejection::InvalidLocation,
    })?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::UnsupportedScheme,
        });
    }
    if from.scheme() == "https" && target.scheme() == "http" {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::Downgrade,
        });
    }
    if seen.contains(&target) {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::Loop,
        });
    }
    Ok(target)
}
