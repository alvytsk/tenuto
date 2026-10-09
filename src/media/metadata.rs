use std::sync::Arc;
use std::time::Duration;

use crate::media::provenance::PositionProvenance;
use crate::media::tags::CoverBytes;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<String>,
    pub duration: Option<Duration>,
    /// Whether `duration` came from a real index/container header (or is
    /// simply absent) versus `estimate_num_mpeg_frames`'s ~16-frame
    /// extrapolation (§5.5). The spike measured that estimate 40% short on a
    /// genuinely VBR file with no Xing/VBRI tag — exact for CBR, which is why
    /// nothing had noticed. Defaults to `Established`: nothing in this
    /// milestone's decode path yet distinguishes the two, so every existing
    /// caller keeps the meaning it always had.
    pub duration_provenance: PositionProvenance,
    /// The container's embedded front cover, still encoded, as the decoder
    /// saw it at load time. Shared rather than copied: this value is cloned
    /// into events and mirrors.
    pub front_cover: Option<Arc<CoverBytes>>,
}
