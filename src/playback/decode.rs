use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::well_known::FORMAT_ID_MP3;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTag};
use symphonia::core::units::{TimeBase, Timestamp};

use crate::media::capabilities::{
    Continuity, DemuxerSeek, MediaCapabilities, SeekSupport, SourceEvidence,
};
use crate::media::id::AbsolutePath;
use crate::media::metadata::MediaMetadata;
use crate::media::vbr_header::{VbrHeader, probe_vbr_header};

use super::error::PlaybackError;
use crate::media::provenance::PositionProvenance;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeekOutcome {
    pub actual: Duration,
    pub refinement_truncated: bool,
    /// Whether this landing is decoder-established or a byte-offset
    /// estimate (§3) — follows the demuxer that actually ran `Coarse`, not
    /// the `SeekMode` this call requests uniformly. See `seek_refined`'s own
    /// comment for why.
    pub provenance: PositionProvenance,
}

pub struct DecodedSource {
    path: PathBuf,
    reader: Box<dyn FormatReader + 'static>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    sample_rate: u32,
    channels: u16,
    metadata: MediaMetadata,
    planes: Vec<Vec<f32>>,
    /// Media timestamp of the next frame `next_planar` will return.
    cursor: u64,
    /// `true` when `planes` already holds decoded audio (the tail of a packet
    /// trimmed during seek refinement) that `next_planar` should hand out
    /// before pulling a new packet from the reader.
    pending: bool,
    /// What the caller established about this source independent of the
    /// decoder — byte length, byte seekability, liveness, and whether the
    /// demuxer's own seek has been demonstrated. `capabilities()` folds this
    /// with what the decoder alone can tell.
    evidence: SourceEvidence,
}

// `FormatReader` and `AudioDecoder` are trait objects that do not implement
// `Debug`, so this is hand-written rather than derived. It exists so
// `Result<DecodedSource, _>::unwrap_err()` is usable in tests.
impl std::fmt::Debug for DecodedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedSource")
            .field("path", &self.path)
            .field("track_id", &self.track_id)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

impl DecodedSource {
    pub fn open(path: &AbsolutePath) -> Result<Self, PlaybackError> {
        let local = open_local_file(path)?;
        let evidence = SourceEvidence {
            byte_len: Some(local.byte_len),
            byte_seekable: true,
            live: false,
            // Not an assumption: M1 already ships this guarantee for the four
            // formats M1 supports, and `tests/decode_fixtures.rs` re-proves it
            // on every run.
            demuxer: DemuxerSeek::Proven,
        };
        Self::from_media_source(
            Box::new(local.file),
            local.hint,
            path.as_path().to_path_buf(),
            evidence,
        )
    }

    /// Open over any `MediaSource`, with the supplied evidence folded into the
    /// capabilities the decoder alone cannot establish.
    ///
    /// `label` is the path (local) or redacted origin (remote) carried purely
    /// for diagnostics — `UnsupportedInput`'s `path` field and this source's
    /// own `path()` accessor.
    pub fn from_media_source(
        source: Box<dyn MediaSource>,
        hint: Hint,
        label: PathBuf,
        evidence: SourceEvidence,
    ) -> Result<Self, PlaybackError> {
        let ProbedContainer {
            mut reader,
            vbr_header,
        } = probe_container(source, &hint)?;

        let track = reader.default_track(TrackType::Audio).ok_or_else(|| {
            PlaybackError::UnsupportedInput {
                path: label.clone(),
                reason: "no audio track".into(),
            }
        })?;
        let track_id = track.id;
        let time_base = track.time_base;
        let (duration, duration_provenance) = track_duration(Some(track), vbr_header);
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| PlaybackError::UnsupportedInput {
                path: label.clone(),
                reason: "no audio codec parameters".into(),
            })?;
        let sample_rate = params
            .sample_rate
            .ok_or_else(|| PlaybackError::UnsupportedInput {
                path: label.clone(),
                reason: "unknown sample rate".into(),
            })?;
        let channels = params
            .channels
            .as_ref()
            .map(|channels| channels.count() as u16)
            .unwrap_or(0);
        if channels == 0 || channels > 2 {
            return Err(PlaybackError::UnsupportedInput {
                path: label,
                reason: format!("{channels} channels; M1 supports mono and stereo"),
            });
        }
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(PlaybackError::Decode)?;
        let (names, front_cover, _) = crate::media::tags::probed_metadata(reader.metadata());
        let front_cover = front_cover.map(std::sync::Arc::new);

        Ok(Self {
            path: label,
            reader,
            decoder,
            track_id,
            time_base,
            sample_rate,
            channels,
            metadata: MediaMetadata {
                title: names.title,
                artist: names.artist,
                album: names.album,
                year: names.year,
                duration,
                duration_provenance,
                front_cover,
            },
            planes: vec![Vec::new(); usize::from(channels)],
            cursor: 0,
            pending: false,
            evidence,
        })
    }

    pub fn metadata(&self) -> &MediaMetadata {
        &self.metadata
    }

    /// A transport-supplied title, used only when the container gave none.
    pub fn set_fallback_title(&mut self, title: String) {
        if self.metadata.title.is_none() {
            self.metadata.title = Some(title);
        }
    }

    /// Capabilities the decoder can establish *on its own*. The engine combines
    /// these with transport evidence: HTTP range support alone does not prove
    /// that a particular container can seek in media time (§4).
    pub fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            // Live evidence is checked *first*. A shoutcast server that also
            // sends a Content-Length would otherwise come back Finite and be
            // played as a recording — and `prepare` would never see the
            // `Indefinite` it refuses live media on, so the refusal path would
            // be unreachable.
            continuity: if self.evidence.live {
                Continuity::Indefinite
            } else if self.evidence.byte_len.is_some() || self.metadata.duration.is_some() {
                Continuity::Finite
            } else {
                Continuity::Unresolved
            },
            seek: match (self.evidence.byte_seekable, self.evidence.demuxer) {
                (true, DemuxerSeek::Proven) => SeekSupport::Native,
                (true, DemuxerSeek::Unproven) => SeekSupport::Unknown,
                (false, _) => SeekSupport::Unsupported,
            },
        }
    }

    /// Called once a trial seek (Task 10) demonstrates the demuxer can seek in
    /// media time, promoting `SeekSupport::Unknown` to `Native`.
    pub fn note_demuxer_proven(&mut self) {
        self.evidence.demuxer = DemuxerSeek::Proven;
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn position(&self) -> Duration {
        self.frames_to_duration(self.cursor)
    }

    /// Decode the next packet into planar `f32`. `Ok(None)` means end of media.
    ///
    /// If seek refinement trimmed a packet to land on the exact frame, the
    /// trimmed remainder is handed out first, before any new packet is pulled
    /// from the reader.
    pub fn next_planar(&mut self) -> Result<Option<&[Vec<f32>]>, PlaybackError> {
        if self.pending {
            self.pending = false;
            self.cursor += self.planes[0].len() as u64;
            return Ok(Some(&self.planes));
        }
        loop {
            let packet = match self.reader.next_packet().map_err(PlaybackError::Decode)? {
                Some(packet) => packet,
                None => return Ok(None),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = self
                .decoder
                .decode(&packet)
                .map_err(PlaybackError::Decode)?;
            let frames = decoded.frames() as u64;
            if frames == 0 {
                continue;
            }
            copy_planar(&decoded, &mut self.planes);
            self.cursor += frames;
            return Ok(Some(&self.planes));
        }
    }

    /// Seek, then decode-and-discard forward to the exact frame.
    ///
    /// `SeekMode::Coarse`, not `Accurate` (M3.1 Task 4): `Accurate`'s
    /// `preseek_accurate` rewinds to the first packet and rescans forward
    /// whenever a seek looks backward relative to the demuxer's own
    /// read-ahead position, inside one uncancellable, unbounded
    /// `FormatReader::seek()` call — that is the wedge M3's manual
    /// acceptance found. `Coarse` computes a byte offset directly from the
    /// track's own duration arithmetic instead, at a measured cost of a few
    /// KB rather than the whole prefix.
    ///
    /// Whether the landing this produces is an estimate or a decoder-
    /// confirmed position depends on which demuxer actually ran — see
    /// `SeekOutcome::provenance` and this method's own computation of it,
    /// just below the `seek()` call. It is *not* unconditionally
    /// `Estimated`: `Coarse` only changes behaviour for MP3 (fix round 1).
    ///
    /// The reader can only seek to a packet boundary, so refinement is
    /// required for an exact landing regardless of mode; it is also what
    /// primes the bit reservoir a `Coarse` landing needs before its output
    /// can be trusted (§5.2) — decoding and discarding forward to the target
    /// already does this, with no mode-specific bookkeeping. `budget` bounds
    /// refinement for an *explicit* seek; pass `None` for stop-resume and
    /// device recovery, which promise preservation.
    pub fn seek_refined(
        &mut self,
        target: Duration,
        budget: Option<Duration>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<SeekOutcome, PlaybackError> {
        let seeked = self
            .reader
            .seek(
                SeekMode::Coarse,
                SeekTo::Time {
                    time: duration_to_time(target),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|source| PlaybackError::SeekFailed { target, source })?;
        // Provenance follows the demuxer that actually ran `Coarse`, not the
        // `SeekMode` this call requests uniformly (M3.1 Task 4, fix round 1).
        // Across this crate's whole dependency tree, MP3's `MpaReader` is the
        // *only* `FormatReader::seek` that reads its `mode` argument at all —
        // FLAC's own seek (`symphonia-bundle-flac`) binary-searches on real
        // per-frame sample numbers carried in the frame headers themselves,
        // and every other format this crate supports (WAV, ISO-BMFF/AAC)
        // ignores `mode` and always does the equivalent of `Accurate`. So a
        // landing on any non-MP3 format is exactly as decoder-confirmed as
        // it always was; only MP3's `preseek_coarse` estimates a byte offset
        // from uniform-bitrate arithmetic, and only that estimate can be
        // wrong by the amounts §5.2 measured (235 s on a 600 s file, with no
        // way to tell from outside). Never conditioned on a Xing/Info tag or
        // anything else sampled from the file - symphonia ignores the Xing
        // TOC and does the same arithmetic regardless of whether one is
        // present, so a tagged MP3 is `Estimated` exactly like an untagged
        // one. Read from the reader's own `format_info()`, not from the
        // file extension, the URL, or the transport, so this is exactly the
        // demuxer that ran, not a guess about it.
        let provenance = if self.reader.format_info().format == FORMAT_ID_MP3 {
            PositionProvenance::Estimated
        } else {
            PositionProvenance::Established
        };
        self.decoder.reset();
        // `actual_ts` is signed and MP3 readers report a NEGATIVE timestamp when
        // seeking into an encoder's delay region, so a bare `as u64` wraps to
        // ~1.8e19 and the next `cursor += frames` overflows. A frame before the
        // first playable one is position zero.
        self.cursor = seeked.actual_ts.get().max(0) as u64;
        self.pending = false;

        let target_frames = self.duration_to_frames(target);
        let budget_frames = budget.map(|b| self.duration_to_frames(b));
        let start = self.cursor;
        let mut truncated = false;
        while self.cursor < target_frames {
            if cancelled() {
                return Err(PlaybackError::Cancelled);
            }
            if let Some(limit) = budget_frames
                && self.cursor.saturating_sub(start) >= limit
            {
                truncated = true;
                break;
            }
            match self.next_planar()? {
                Some(_) => {
                    // The reader can only seek to a packet boundary, so a
                    // decoded packet may run past the target frame. Trim its
                    // leading frames so the cursor lands exactly on target and
                    // the remainder is preserved as pending audio, rather than
                    // being decoded again or silently skipped.
                    if self.cursor > target_frames {
                        let overshoot = (self.cursor - target_frames) as usize;
                        let discard = self.planes[0].len() - overshoot;
                        for plane in &mut self.planes {
                            plane.drain(0..discard);
                        }
                        self.cursor = target_frames;
                        self.pending = !self.planes[0].is_empty();
                    }
                }
                None => break,
            }
        }
        Ok(SeekOutcome {
            actual: self.position(),
            refinement_truncated: truncated,
            provenance,
        })
    }

    /// Nearest frame, not floor: positions reach here through
    /// `Duration::from_secs_f64`, which rounds to the nanosecond, and at
    /// 48 kHz one frame in three converts back one frame short under a
    /// floor, so a resume at a captured position would replay a frame.
    fn duration_to_frames(&self, value: Duration) -> u64 {
        nearest_frame(value, self.sample_rate)
    }

    fn frames_to_duration(&self, frames: u64) -> Duration {
        Duration::from_secs_f64(frames as f64 / f64::from(self.sample_rate))
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn time_base(&self) -> Option<TimeBase> {
        self.time_base
    }
}

/// A regular local file opened for reading, with the extension hint the
/// container probe starts from.
pub(crate) struct LocalFile {
    pub file: File,
    pub hint: Hint,
    pub byte_len: u64,
}

/// Opens `path` for probing, refusing anything but a regular file.
pub(crate) fn open_local_file(path: &AbsolutePath) -> Result<LocalFile, PlaybackError> {
    let owned = path.as_path();
    let metadata_fs = std::fs::metadata(owned).map_err(|source| PlaybackError::Open {
        path: owned.to_path_buf(),
        source,
    })?;
    // Reject pipes and device files so this local-file slice cannot acquire
    // an unbounded read.
    if !metadata_fs.is_file() {
        return Err(PlaybackError::UnsupportedInput {
            path: owned.to_path_buf(),
            reason: "not a regular file".into(),
        });
    }
    let file = File::open(owned).map_err(|source| PlaybackError::Open {
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
) -> Result<ProbedContainer, PlaybackError> {
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
        .map_err(PlaybackError::Decode)?;
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

fn copy_planar(decoded: &GenericAudioBufferRef<'_>, planes: &mut Vec<Vec<f32>>) {
    decoded.copy_to_vecs_planar::<f32>(planes);
}

fn duration_to_time(value: Duration) -> symphonia::core::units::Time {
    // `subsec_nanos()` is always < 1_000_000_000, so `try_new` never actually
    // returns `None`; the fallback exists only to avoid `unwrap`.
    symphonia::core::units::Time::try_new(value.as_secs() as i64, value.subsec_nanos())
        .unwrap_or(symphonia::core::units::Time::ZERO)
}

#[cfg(test)]
mod year_tests {
    use super::year_of;

    #[test]
    fn a_year_is_the_leading_four_digits_of_a_date() {
        assert_eq!(year_of("1998-04-20"), Some("1998".to_owned()));
        assert_eq!(year_of("1998"), Some("1998".to_owned()));
        assert_eq!(year_of("199"), None);
        assert_eq!(year_of("unknown"), None);
    }
}

fn nearest_frame(value: Duration, sample_rate: u32) -> u64 {
    (value.as_secs_f64() * f64::from(sample_rate)).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_converted_from_frames_converts_back_to_the_same_frame() {
        for frames in 0..4 * 48_000u64 {
            let position = Duration::from_secs_f64(frames as f64 / 48_000.0);
            assert_eq!(nearest_frame(position, 48_000), frames);
        }
    }
}
