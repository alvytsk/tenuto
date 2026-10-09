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
