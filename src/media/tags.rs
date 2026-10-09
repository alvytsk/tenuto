//! What a local file says about itself before anything decodes it (design
//! doc M5 §8–§9): title, artist and album tags, the container's duration and
//! the embedded front cover. Only symphonia's container probe runs — no
//! decoder is built, no packet is read and no audio device is opened — so
//! enriching a queue row costs a header read, not playback.

use std::time::Duration;

use symphonia::core::formats::TrackType;
use symphonia::core::meta::{Metadata, MetadataRevision, StandardVisualKey};

use crate::media::id::AbsolutePath;
use crate::media::provenance::PositionProvenance;
use crate::playback::decode::{
    ProbedContainer, StandardNames, open_local_file, probe_container, standard_names,
    track_duration,
};
use crate::playback::error::PlaybackError;

/// The largest embedded cover kept, encoded (§9: 10 MiB).
pub const MAX_EMBEDDED_COVER_BYTES: usize = 10 * 1024 * 1024;

/// An embedded picture exactly as the file stores it, still encoded.
#[derive(Clone, Eq, PartialEq)]
pub struct CoverBytes {
    pub data: Vec<u8>,
    /// The MIME type the tag declares; a hint, not a verified format.
    pub media_type: Option<String>,
}

/// The length, not the bytes: this rides on `MediaMetadata`, whose `Debug`
/// reaches logs and test failures.
impl std::fmt::Debug for CoverBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverBytes")
            .field("len", &self.data.len())
            .field("media_type", &self.media_type)
            .finish()
    }
}

/// The revision's front cover (decision 13: only a visual whose usage is
/// `FrontCover` counts), and whether one exists but exceeds
/// [`MAX_EMBEDDED_COVER_BYTES`] — in which case it is dropped, not
/// truncated. Shared by the local tag probe and the playback decoder, so a
/// remote stream's ID3 or FLAC picture is read by the same rule.
pub(crate) fn front_cover_of(revision: Option<&MetadataRevision>) -> (Option<CoverBytes>, bool) {
    let cover = revision.and_then(|revision| {
        revision
            .media
            .visuals
            .iter()
            .find(|visual| visual.usage == Some(StandardVisualKey::FrontCover))
    });
    match cover {
        Some(visual) if visual.data.len() > MAX_EMBEDDED_COVER_BYTES => (None, true),
        Some(visual) => (
            Some(CoverBytes {
                data: visual.data.to_vec(),
                media_type: visual.media_type.clone(),
            }),
            false,
        ),
        None => (None, false),
    }
}

/// The names and front cover across every revision the probe queued, a newer
/// revision's value winning. `Metadata::current` is only the *oldest*: for an
/// MP3 that is the ID3v1 trailer — 30-byte names, no picture — queued ahead
/// of the ID3v2 tag that holds the real ones. Drains the queue to its latest
/// revision.
pub(crate) fn probed_metadata(
    mut metadata: Metadata<'_>,
) -> (StandardNames, Option<CoverBytes>, bool) {
    let mut names = StandardNames::default();
    let (mut front_cover, mut cover_oversized) = (None, false);
    loop {
        let revision = metadata.current();
        let newer = standard_names(revision);
        names.title = newer.title.or(names.title);
        names.artist = newer.artist.or(names.artist);
        names.album = newer.album.or(names.album);
        names.year = newer.year.or(names.year);
        let (cover, oversized) = front_cover_of(revision);
        if cover.is_some() || oversized {
            (front_cover, cover_oversized) = (cover, oversized);
        }
        if metadata.pop().is_none() {
            return (names, front_cover, cover_oversized);
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<String>,
    pub duration: Option<Duration>,
    /// Computed exactly as playback computes it, so a row enriched here
    /// never claims more certainty than the loaded track later would.
    pub duration_provenance: PositionProvenance,
    pub front_cover: Option<CoverBytes>,
    /// A front cover exists but is larger than [`MAX_EMBEDDED_COVER_BYTES`];
    /// `front_cover` is then `None`.
    pub cover_oversized: bool,
}

/// Probes `path`'s container and reads its current metadata revision. The
/// tag strings are returned as the file spells them; whoever displays them
/// escapes them.
///
/// Only a visual whose usage is `FrontCover` counts (decision 13): the spec
/// asks for front-cover artwork, and an untyped or back-cover picture must
/// not stand in for it.
pub fn probe_local_tags(path: &AbsolutePath) -> Result<LocalTags, PlaybackError> {
    let local = open_local_file(path)?;
    let ProbedContainer {
        mut reader,
        vbr_header,
    } = probe_container(Box::new(local.file), &local.hint)?;
    let (duration, duration_provenance) =
        track_duration(reader.default_track(TrackType::Audio), vbr_header);

    let (names, front_cover, cover_oversized) = probed_metadata(reader.metadata());

    Ok(LocalTags {
        title: names.title,
        artist: names.artist,
        album: names.album,
        year: names.year,
        duration,
        duration_provenance,
        front_cover,
        cover_oversized,
    })
}
