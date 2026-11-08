pub mod capabilities;
pub mod display;
pub mod id;
pub mod metadata;
pub mod probe;
pub mod provenance;
pub mod source;
pub mod tags;
pub mod vbr_header;

use std::time::Duration;

use time::OffsetDateTime;
use url::Url;

/// The scheme-and-host half of the public-source bar: `http` or `https` with
/// a host present. Exposed for error-message selection at sites that need to
/// distinguish "not http(s) with a host" from "embedded credentials"; most
/// callers want [`is_public_source_url`], which is this plus the credential
/// ban. This is the one implementation of both halves - the hand-copies it
/// replaced had already drifted, admitting scheme-only enclosure URLs from
/// untrusted feeds.
pub fn is_http_url_with_host(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
}

/// The public-source bar every remote URL tenuto acts on is held to (§5):
/// [`is_http_url_with_host`] and no embedded `user:password@` credentials.
/// A URL that fails this can never survive the document and media
/// transports' own source checks, so admitting it upstream only defers the
/// refusal to a later, less explainable failure.
pub fn is_public_source_url(url: &Url) -> bool {
    is_http_url_with_host(url) && url.username().is_empty() && url.password().is_none()
}

/// A podcast episode bound to a real identity (§2.2). It carries **no**
/// `key` field of its own — [`id::MediaId::feed`] and
/// [`id::MediaId::episode_key`] reach into `id` instead, so the two never
/// have a chance to disagree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Episode {
    pub id: id::MediaId,
    /// `None` when the item had identity but no usable enclosure (§2.4).
    pub source: Option<source::SourceLocation>,
    pub title: Option<String>,
    pub published: Option<OffsetDateTime>,
    /// From `itunes:duration`; advisory only, never fed to seek, resume or
    /// completion logic (§2.2).
    pub declared_duration: Option<Duration>,
}
