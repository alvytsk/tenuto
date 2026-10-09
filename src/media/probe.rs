//! The container probe playback and the local tag reader share (design doc
//! M5 §8): open a regular file, read its MP3 frame-count evidence, and let
//! symphonia recognise the container. No decoder is built and no packet is
//! read.

use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTag};
use symphonia::core::units::Timestamp;

use crate::media::id::AbsolutePath;
use crate::media::provenance::PositionProvenance;
use crate::media::vbr_header::{VbrHeader, probe_vbr_header};

/// Why a probe failed. Each variant stands for the same-named
/// `PlaybackError` variant, wording included, and converts into it with
/// its source moved across unchanged.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("cannot open media {path:?}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot play {path:?}: {reason}")]
    UnsupportedInput { path: PathBuf, reason: String },
    #[error("cannot decode media")]
    Decode(#[source] symphonia::core::errors::Error),
    #[error("terminal I/O error")]
    Io(#[from] std::io::Error),
}

/// A regular local file opened for reading, with the extension hint the
/// container probe starts from.
pub(crate) struct LocalFile {
    pub file: File,
    pub hint: Hint,
    pub byte_len: u64,
}

/// Opens `path` for probing, refusing anything but a regular file.
pub(crate) fn open_local_file(path: &AbsolutePath) -> Result<LocalFile, ProbeError> {
    let owned = path.as_path();
    let metadata_fs = std::fs::metadata(owned).map_err(|source| ProbeError::Open {
        path: owned.to_path_buf(),
        source,
    })?;
    // Reject pipes and device files so this local-file slice cannot acquire
    // an unbounded read.
    if !metadata_fs.is_file() {
        return Err(ProbeError::UnsupportedInput {
            path: owned.to_path_buf(),
            reason: "not a regular file".into(),
        });
    }
    let file = File::open(owned).map_err(|source| ProbeError::Open {
        path: owned.to_path_buf(),
        source,
    })?;
    let mut hint = Hint::new();
    if let Some(extension) = owned.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    Ok(LocalFile {
        file,
        hint,
        byte_len: metadata_fs.len(),
    })
}

/// A container symphonia's probe recognised, before any decoder exists.
pub(crate) struct ProbedContainer {
    pub reader: Box<dyn FormatReader + 'static>,
    pub vbr_header: Option<VbrHeader>,
}

/// Runs the container probe — no decoder is built and no packet is read —
/// shared by playback and the local tag probe so both see the same reader,
/// tags and duration evidence.
pub(crate) fn probe_container(
    mut source: Box<dyn MediaSource>,
    hint: &Hint,
) -> Result<ProbedContainer, ProbeError> {
    // Read the container's own evidence for the MP3 frame-count header
    // (Xing/Info/VBRI) before `MediaSourceStream` takes the source. This
    // must run first: symphonia's `Track` never says whether its
    // `num_frames` came from this header or from
    // `estimate_num_mpeg_frames`'s ~16-frame extrapolation, and by the
    // time the reader is built that distinction is unrecoverable.
    let vbr_header = probe_vbr_header(source.as_mut())?;
    let mss = MediaSourceStream::new(
        source,
        MediaSourceStreamOptions {
            buffer_len: 64 * 1024,
        },
    );
    let reader = symphonia::default::get_probe()
        .probe(
            hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(ProbeError::Decode)?;
    Ok(ProbedContainer { reader, vbr_header })
}

/// A track's duration and how far it can be trusted.
///
/// `Some(XingInfo | Vbri)` is a real index: `Established`. `Some(Absent)` is
/// symphonia's `estimate_num_mpeg_frames` fallback: `Estimated` (§5.5).
/// `None` means the probe gathered no evidence at all (a non-MP3 container,
/// a short read, or an unseekable source) and must not silently downgrade a
/// duration that may be perfectly good — every non-MP3 format's real
/// index/container header keeps the meaning it always had.
pub(crate) fn track_duration(
    track: Option<&Track>,
    vbr_header: Option<VbrHeader>,
) -> (Option<Duration>, PositionProvenance) {
    let duration = track.and_then(|track| {
        track
            .num_frames
            .zip(track.time_base)
            .and_then(|(frames, base)| {
                base.calc_time(Timestamp::new(frames as i64))
                    .map(|time| Duration::from_secs_f64(time.as_secs_f64()))
            })
    });
    let provenance = match vbr_header {
        Some(VbrHeader::XingInfo | VbrHeader::Vbri) | None => PositionProvenance::Established,
        Some(VbrHeader::Absent) => PositionProvenance::Estimated,
    };
    (duration, provenance)
}

/// The first title, artist and album a metadata revision carries.
#[derive(Default)]
pub(crate) struct StandardNames {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// The four-digit year of the first date tag (ID3 TDRC/TYER, Vorbis
    /// DATE), when it starts with one.
    pub year: Option<String>,
}

pub(crate) fn standard_names(revision: Option<&MetadataRevision>) -> StandardNames {
    let mut names = StandardNames::default();
    let Some(revision) = revision else {
        return names;
    };
    // `Tag::std`, when present, is a `StandardTag` that carries its value
    // inline (e.g. `StandardTag::TrackTitle(Arc<String>)`) rather than a
    // separate key enum. The first of each wins.
    for tag in &revision.media.tags {
        let (slot, value) = match &tag.std {
            Some(StandardTag::TrackTitle(value)) => (&mut names.title, value),
            Some(StandardTag::Artist(value)) => (&mut names.artist, value),
            Some(StandardTag::Album(value)) => (&mut names.album, value),
            Some(
                StandardTag::RecordingDate(value)
                | StandardTag::ReleaseDate(value)
                | StandardTag::OriginalReleaseDate(value),
            ) => {
                if names.year.is_none() {
                    names.year = year_of(value);
                }
                continue;
            }
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(value.to_string());
        }
    }
    names
}

/// The leading four digits of a date string, when it starts with four.
fn year_of(date: &str) -> Option<String> {
    let year = date.get(..4)?;
    year.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| year.to_owned())
}

#[cfg(test)]
mod tests {
    use super::year_of;

    #[test]
    fn a_year_is_the_leading_four_digits_of_a_date() {
        assert_eq!(year_of("1998-04-20"), Some("1998".to_owned()));
        assert_eq!(year_of("1998"), Some("1998".to_owned()));
        assert_eq!(year_of("199"), None);
        assert_eq!(year_of("unknown"), None);
    }
}
