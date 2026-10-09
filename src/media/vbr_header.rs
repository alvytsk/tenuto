//! Duration provenance evidence for MP3 (§5.5): whether a Xing/Info or VBRI
//! header in the first frame declares this file's total frame count, versus
//! symphonia falling back to `estimate_num_mpeg_frames`'s ~16-frame
//! extrapolation.
//!
//! Symphonia parses exactly this information into `XingInfoTag`
//! (`symphonia-bundle-mp3-0.6.1/src/demuxer.rs:749-758`) — a struct marked
//! `#[allow(dead_code)]` whose fields are discarded at the crate boundary
//! except `num_frames` and `lame`. There is no API to ask "did a real
//! header establish this duration, or did symphonia extrapolate it?" — the
//! only trace is a `log::info!` line, not an API — so this module reads the
//! same bytes independently, before the source is handed to
//! `MediaSourceStream`.

use std::io::SeekFrom;

use symphonia::core::io::MediaSource;

use crate::media::probe::ProbeError;

/// Read at least this many bytes before giving up. Chosen against a real
/// case, not a round number: the Radio-T episode this task's bug report
/// traces to carries a 37,422-byte ID3v2.4 tag, putting its first frame at
/// byte 37,432 and its Xing tag a few bytes past that — comfortably inside
/// 64 KiB, nowhere near an 8 KiB probe.
const PROBE_LEN: usize = 64 * 1024;

const MPEG_HEADER_LEN: usize = 4;
const XING_TAG_ID: [u8; 4] = *b"Xing";
const INFO_TAG_ID: [u8; 4] = *b"Info";
const VBRI_TAG_ID: [u8; 4] = *b"VBRI";
/// The VBRI tag sits at a fixed offset past the frame header, independent of
/// side info length — `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:1025`.
const VBRI_TAG_OFFSET_FROM_HEADER: usize = 32;
/// The minimum buffer length symphonia requires past the VBRI offset before
/// even checking the tag id — `is_maybe_vbri_tag`'s `MIN_VBRI_TAG_LEN`,
/// `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:1024`. Not the bare 4-byte tag
/// id: the gate covers the whole minimal tag (id, version, delay, quality,
/// byte and frame counts).
const MIN_VBRI_TAG_LEN: usize = 26;

/// What established an MP3's frame count, and therefore its duration.
///
/// Symphonia populates `Track::num_frames` from one of three paths and
/// exposes no field saying which ran — the only trace is a `log::info!`
/// line, which is not an API. So we determine it from the container bytes
/// ourselves, before the reader is built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VbrHeader {
    /// A Xing or Info tag in the first frame. `Info` is what LAME writes for
    /// a constant-bitrate file; both carry a declared total frame count.
    XingInfo,
    /// A VBRI tag (Fraunhofer encoders), at a fixed offset in the first frame.
    Vbri,
    /// Neither. Symphonia will fall back to `estimate_num_mpeg_frames`, which
    /// samples ~16 frames and extrapolates.
    Absent,
}

/// Reads the head of a seekable source and reports which header it carries.
/// Restores the source's position before returning `Ok`, so the caller may
/// hand it to `MediaSourceStream` unchanged.
///
/// Returns `Ok(None)` for a source this cannot answer for — a non-MP3
/// container, a short read, or a source that is not seekable. `None` means
/// "no evidence gathered", and the caller must not read it as `Absent`.
pub fn probe_vbr_header(source: &mut dyn MediaSource) -> Result<Option<VbrHeader>, ProbeError> {
    if !source.is_seekable() {
        return Ok(None);
    }
    let original_pos = source.stream_position()?;
    source.seek(SeekFrom::Start(0))?;
    // On failure, deliberately skip the restoring seek below rather than
    // running it unconditionally: a remote source's read failure is latched
    // by `LatchingSource` (`playback::prepare`) so the caller can recover
    // *why* opening failed, and that latch treats any later successful
    // seek as proof the fault cleared, wiping the very failure this
    // function is about to propagate. A caller that gets `Err` here is
    // abandoning this source anyway (`DecodedSource::from_media_source`
    // never reaches `MediaSourceStream::new` on this path), so there is no
    // reader left for a restored position to matter to. Verified against
    // `tests/prepare.rs::opening_that_exceeds_the_probe_byte_cap_fails_cancellably`
    // and `::a_probe_that_outlives_its_deadline_is_refused`, both of which
    // this exact ordering mistake broke.
    let buf = gather_evidence(source)?;
    source.seek(SeekFrom::Start(original_pos))?;
    Ok(detect(&buf))
}

/// How much of the head to read unconditionally. Deliberately small: large
/// enough to hold an ID3v2 header and, for the overwhelming majority of real
/// files (no tag, or a small one), the frame header and tag-check region
/// too — so the common case never asks a source for more than this.
const INITIAL_LEN: usize = 4 * 1024;

/// Bytes needed past the frame header to fully check both possible tag
/// positions: Xing/Info's widest `side_info_len` (32, MPEG1 stereo) plus its
/// 4-byte tag id, versus VBRI's fixed 32-byte offset plus symphonia's own
/// 26-byte minimum tag length past that offset (`MIN_VBRI_TAG_LEN`) — the
/// larger of the two, since either may be what a given frame carries.
const TAG_REGION_LEN: usize = if 32 + 4 > VBRI_TAG_OFFSET_FROM_HEADER + MIN_VBRI_TAG_LEN {
    32 + 4
} else {
    VBRI_TAG_OFFSET_FROM_HEADER + MIN_VBRI_TAG_LEN
};

/// Reads only as much of the head as the evidence actually requires: a
/// small initial chunk, extended only when that chunk itself reveals an
/// ID3v2 tag long enough to push the first frame beyond it — never the full
/// `PROBE_LEN` cap for the ordinary case of no tag or a small one.
///
/// This matters beyond efficiency: a remote source can have more bytes to
/// deliver than it has sent yet (a slow or deliberately stalled body,
/// chiefly, as `tests/app_cli.rs`'s stall-recovery tests exercise on
/// `sine-5s.flac`). Forcing a full 64 KiB read for every open — including
/// files that plainly do not need it — would block this open on exactly
/// the backpressure those tests exist to prove the engine can get out of
/// band of. Extending only when the ID3 header itself says the frame sits
/// further out keeps that blocking read confined to files that actually
/// have that much of a tag — which is also the only case §5.5's evidence
/// gathering needs it for.
fn gather_evidence(source: &mut dyn MediaSource) -> Result<Vec<u8>, ProbeError> {
    let mut buf = read_up_to(source, INITIAL_LEN)?;
    // `first_frame_offset` never actually returns `None` (see its own
    // comment); the fallback is defensive, not a guess.
    let frame_offset = first_frame_offset(&buf).unwrap_or(0);
    let needed = frame_offset.saturating_add(MPEG_HEADER_LEN + TAG_REGION_LEN);
    if needed > buf.len() {
        let target = needed.min(PROBE_LEN);
        if target > buf.len() {
            let more = read_up_to(source, target - buf.len())?;
            buf.extend(more);
        }
    }
    Ok(buf)
}

/// Reads up to `len` further bytes from `source`'s current position,
/// stopping early at EOF. The returned `Vec` is exactly as long as what was
/// actually read.
fn read_up_to(source: &mut dyn MediaSource, len: usize) -> Result<Vec<u8>, ProbeError> {
    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    while filled < buf.len() {
        match source.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ProbeError::Io(error)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Where the first MPEG frame begins, per the leading ID3v2 tag if there is
/// one. Always `Some`: a buffer too short to hold an ID3v2 header is
/// treated the same as one with no tag at all (offset 0), since neither is
/// evidence of a tag actually being present. Kept as `Option` because
/// `detect`'s own `?` reads more naturally against it and a future refinement
/// may have a genuine "cannot tell" case.
fn first_frame_offset(buf: &[u8]) -> Option<usize> {
    if buf.len() < 10 || &buf[0..3] != b"ID3" {
        // No ID3v2 tag (or too little to tell): the stream, if this is MP3
        // at all, starts with the first frame directly.
        return Some(0);
    }
    let flags = buf[5];
    let size = (u32::from(buf[6] & 0x7f) << 21)
        | (u32::from(buf[7] & 0x7f) << 14)
        | (u32::from(buf[8] & 0x7f) << 7)
        | u32::from(buf[9] & 0x7f);
    let mut offset = 10usize + size as usize;
    // Footer flag (ID3v2.4 §3.1, bit 4 of the flags byte): an optional
    // 10-byte copy of the header repeated after the frames.
    if flags & 0x10 != 0 {
        offset += 10;
    }
    Some(offset)
}

/// The frame-header fields this detection needs beyond `MPEG_HEADER_LEN`
/// itself: whether the frame uses MPEG1 side-info sizing and whether it is
/// mono (both feed `side_info_len`), and whether a CRC follows the header
/// (feeds `header_size` — the side-info *zero-check*'s start, distinct from
/// the Xing/Info tag *offset*, which never counts the CRC; see `detect`).
struct FrameShape {
    is_mpeg1: bool,
    is_mono: bool,
    is_layer3: bool,
    has_crc: bool,
}

/// Validates a 4-byte MPEG frame header word and extracts what this module
/// needs from it. Mirrors the field checks in
/// `symphonia-bundle-mp3-0.6.1/src/header.rs::parse_frame_header` — sync,
/// version, layer, bitrate index, sample-rate index — without needing that
/// crate's private types, since only validity and two derived booleans are
/// needed here, never a full decode.
fn parse_frame_shape(word: [u8; 4]) -> Option<FrameShape> {
    if word[0] != 0xFF || word[1] & 0xE0 != 0xE0 {
        return None;
    }
    let header = u32::from_be_bytes(word);

    let version_bits = (header & 0x18_0000) >> 19;
    if version_bits == 0b01 {
        // Reserved version.
        return None;
    }
    let is_mpeg1 = version_bits == 0b11;

    let layer_bits = (header & 0x6_0000) >> 17;
    if layer_bits == 0b00 {
        // Reserved layer.
        return None;
    }
    let is_layer3 = layer_bits == 0b01;

    let bitrate_index = (header & 0xf000) >> 12;
    if bitrate_index == 0 || bitrate_index == 15 {
        return None;
    }

    let sample_rate_index = (header & 0xc00) >> 10;
    if sample_rate_index == 0b11 {
        return None;
    }

    let channel_mode_bits = (header & 0xc0) >> 6;
    let is_mono = channel_mode_bits == 0b11;

    // The protection bit: 0 means a CRC follows the header, 1 means none —
    // `symphonia-bundle-mp3-0.6.1/src/header.rs`'s `let has_crc = header &
    // 0x1_0000 == 0;`, reproduced exactly.
    let has_crc = header & 0x1_0000 == 0;

    Some(FrameShape {
        is_mpeg1,
        is_mono,
        is_layer3,
        has_crc,
    })
}

/// `symphonia-bundle-mp3-0.6.1/src/common.rs::FrameHeader::side_info_len`,
/// reproduced against the two fields this module tracks instead of that
/// crate's private `MpegVersion`/`ChannelMode` types.
fn side_info_len(shape: &FrameShape) -> usize {
    match (shape.is_mpeg1, shape.is_mono) {
        (true, true) => 17,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    }
}

fn detect(buf: &[u8]) -> Option<VbrHeader> {
    let frame_offset = first_frame_offset(buf)?;
    let header_end = frame_offset.checked_add(MPEG_HEADER_LEN)?;
    let word: [u8; 4] = match buf.get(frame_offset..header_end) {
        Some(bytes) => bytes.try_into().ok()?,
        None => {
            // `frame_offset` is nonzero only when `first_frame_offset` found
            // a leading ID3v2 tag; a short buffer at offset 0 means no tag,
            // and therefore no frame either — genuinely "not MP3" — so this
            // keeps returning `None`, which fails open to `Established` at
            // the call site. That fail-open is load-bearing for FLAC, WAV
            // and M4A, which reach here with no MP3 frame to find at all.
            // A short buffer *past* a real ID3v2 tag is a different case
            // (B3, §5.5): the file is MP3-shaped, but this probe gathered
            // no evidence about its Xing/Info/VBRI tag. Report `Absent`
            // (→ `Estimated`) rather than `None` (→ `Established`) — not
            // knowing must not license a destructive decision.
            return if frame_offset > 0 {
                Some(VbrHeader::Absent)
            } else {
                None
            };
        }
    };
    let shape = parse_frame_shape(word)?;

    let xing_pos = header_end + side_info_len(&shape);
    let vbri_pos = header_end + VBRI_TAG_OFFSET_FROM_HEADER;
    // Both candidate regions must be fully covered by what was read before
    // concluding anything about them. VBRI's minimum tag length is 26 bytes
    // past its offset, not the bare 4-byte tag id —
    // `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:1024,1033-1035`
    // (`MIN_VBRI_TAG_LEN`).
    let furthest_needed = (xing_pos + 4).max(vbri_pos + MIN_VBRI_TAG_LEN);
    if buf.len() < furthest_needed {
        // A genuine MP3 frame header was found above (unlike the `None`
        // case), but the tag region beyond it was not fully read. Same B3
        // reasoning as above: "we don't know" reports `Absent`, never the
        // `None` that a short read used to produce here.
        return Some(VbrHeader::Absent);
    }

    if shape.is_layer3 {
        // `header_size()` — where the side info actually starts — counts an
        // optional 2-byte CRC that the tag *offset* above deliberately does
        // not (symphonia's own comment: "The CRC is not included in this
        // offset calculation"). The two are different positions for the
        // same reason: the offset predicts where symphonia looks for the
        // tag id; this predicts where its side-info zero-check starts.
        let header_size = header_end + if shape.has_crc { 2 } else { 0 };

        // A real Xing/Info frame is a dummy frame whose side info is all
        // zero — `is_maybe_info_tag`, `demuxer.rs:966-967`. Without this,
        // four ASCII bytes that happen to spell "Xing"/"Info" inside
        // ordinary audio data at this exact offset would be accepted as a
        // real tag when symphonia would reject it and fall back to
        // `estimate_num_mpeg_frames` — reporting `Established` for a
        // duration that is actually an estimate, which is the one direction
        // §5.5 forbids.
        let xing_candidate = &buf[xing_pos..xing_pos + 4];
        let xing_side_info_is_zero = buf[header_size..xing_pos].iter().all(|&b| b == 0);
        if xing_side_info_is_zero
            && (xing_candidate == XING_TAG_ID || xing_candidate == INFO_TAG_ID)
        {
            return Some(VbrHeader::XingInfo);
        }

        // `is_maybe_vbri_tag`, `demuxer.rs:1023-1046`, mirrored in full: a
        // layer-3 frame (this `if`), a minimum buffer length (the
        // `furthest_needed` check above), the tag id, and — same reasoning
        // as the Xing/Info check just above — a zeroed side-info region
        // from `header_size` to the tag's own offset.
        let vbri_candidate = &buf[vbri_pos..vbri_pos + 4];
        let vbri_side_info_is_zero = buf[header_size..vbri_pos].iter().all(|&b| b == 0);
        if vbri_side_info_is_zero && vbri_candidate == VBRI_TAG_ID {
            return Some(VbrHeader::Vbri);
        }
    }

    Some(VbrHeader::Absent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek};

    /// A trivial in-memory `MediaSource`, so `probe_vbr_header` itself can be
    /// exercised directly against a fabricated buffer, without going through
    /// a real file or an HTTP fixture. Necessary for
    /// `probe_vbr_header_finds_a_xing_tag_beyond_an_8kib_id3_tag`: routing
    /// that case through `DecodedSource::open` (as the integration test
    /// does) cannot actually pin down that this function reads far enough,
    /// because `detect`'s own "offset beyond what was read means `None`,
    /// never `Absent`" rule (correctly) maps a too-small read to
    /// `Established` too — the same answer a correct 64 KiB read would give,
    /// for a different and wrong reason. Only asserting the `VbrHeader`
    /// value itself, not the `PositionProvenance` it eventually becomes,
    /// tells the two apart.
    struct MemSource(Cursor<Vec<u8>>);

    impl Read for MemSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Seek for MemSource {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }

    impl MediaSource for MemSource {
        fn is_seekable(&self) -> bool {
            true
        }

        fn byte_len(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
    }

    /// An ID3v2.4 tag of `padding` zero bytes of content, followed by a
    /// valid MPEG1 Layer III stereo frame header (`side_info_len` 32) with a
    /// `Xing` tag glued on immediately after its side info, and enough
    /// trailing bytes to satisfy `detect`'s VBRI-region length gate (26
    /// bytes past its offset) too — a real file always has more audio past
    /// its first frame; only this synthetic fixture would otherwise end
    /// exactly at the tag.
    fn id3_then_xing_frame(padding: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"ID3");
        buf.extend_from_slice(&[4, 0, 0]); // version, revision, flags
        let size = padding as u32;
        buf.extend_from_slice(&[
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);
        buf.extend(std::iter::repeat_n(0u8, padding));
        buf.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]); // MPEG1 L3 stereo
        buf.extend(std::iter::repeat_n(0u8, 32)); // side info, stereo
        buf.extend_from_slice(b"Xing");
        buf.extend(std::iter::repeat_n(0u8, 32)); // trailing "rest of the file"
        buf
    }

    #[test]
    fn probe_vbr_header_finds_a_xing_tag_beyond_an_8kib_id3_tag() {
        // The ID3 tag alone is comfortably past 8 KiB but still well inside
        // the 64 KiB this probe must read - the exact "8 KB trap" the
        // design doc calls out. Directly pins `PROBE_LEN`'s floor, which
        // the integration-level `Established` assertion cannot (see
        // `MemSource`'s own comment).
        let bytes = id3_then_xing_frame(20 * 1024);
        #[allow(clippy::unwrap_used)]
        let result = probe_vbr_header(&mut MemSource(Cursor::new(bytes))).unwrap();
        assert_eq!(result, Some(VbrHeader::XingInfo));
    }

    #[test]
    fn first_frame_offset_is_zero_with_no_id3_tag() {
        assert_eq!(first_frame_offset(b"\xff\xfb\x90\x00rest"), Some(0));
    }

    #[test]
    fn first_frame_offset_decodes_the_syncsafe_size() {
        // ID3 header + syncsafe size 0x22 (34) => frame at 10 + 34 = 44.
        let mut buf = vec![b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0x22];
        buf.resize(50, 0);
        assert_eq!(first_frame_offset(&buf), Some(44));
    }

    #[test]
    fn first_frame_offset_accounts_for_the_footer_flag() {
        // Same as above but with the footer flag (bit 4) set: 10 more bytes.
        let mut buf = vec![b'I', b'D', b'3', 4, 0, 0x10, 0, 0, 0, 0x22];
        buf.resize(60, 0);
        assert_eq!(first_frame_offset(&buf), Some(54));
    }

    #[test]
    fn parse_frame_shape_rejects_a_bad_sync() {
        assert!(parse_frame_shape([0xff, 0x1b, 0x90, 0x00]).is_none());
    }

    #[test]
    fn parse_frame_shape_rejects_a_reserved_version() {
        // Version bits 0b01 are reserved; every other combination
        // (0b00, 0b10, 0b11) is a real MPEG version.
        assert!(parse_frame_shape([0xff, 0xeb, 0x90, 0x00]).is_none());
    }

    #[test]
    fn parse_frame_shape_rejects_a_reserved_layer() {
        // Layer bits 0b00 are reserved; 0b01/0b10/0b11 are Layer III/II/I.
        assert!(parse_frame_shape([0xff, 0xf9, 0x90, 0x00]).is_none());
    }

    #[test]
    fn parse_frame_shape_rejects_an_invalid_bitrate_index() {
        // Bitrate index 0b1111 (15) is invalid.
        assert!(parse_frame_shape([0xff, 0xfb, 0xf0, 0x00]).is_none());
    }

    #[test]
    fn parse_frame_shape_rejects_an_invalid_sample_rate_index() {
        // Sample-rate index 0b11 (3) is reserved.
        assert!(parse_frame_shape([0xff, 0xfb, 0x0c, 0x00]).is_none());
    }

    #[test]
    fn detect_reports_absent_for_a_valid_header_with_no_tag() {
        let mut buf = vec![0xff, 0xfb, 0x90, 0x00];
        buf.resize(200, 0);
        assert_eq!(detect(&buf), Some(VbrHeader::Absent));
    }

    #[test]
    fn detect_rejects_xing_bytes_over_non_zero_side_info() {
        // symphonia's `is_maybe_info_tag` (demuxer.rs:966-967) additionally
        // requires the side-info region to be all zero before accepting a
        // Xing/Info tag as real - a genuine Xing/Info frame is a dummy frame
        // built entirely of zeroes plus the tag. Four ASCII bytes that
        // happen to spell "Xing" at the right offset inside otherwise
        // ordinary (non-zero) audio data is exactly the false positive that
        // check exists to catch: symphonia would reject it and fall back to
        // `estimate_num_mpeg_frames`, so the correct answer here is
        // `Absent`, matching what symphonia's own fallback path implies -
        // not `XingInfo`.
        let mut buf = vec![0xff, 0xfb, 0x90, 0x00]; // MPEG1 L3 stereo, no CRC
        buf.extend(std::iter::repeat_n(0xAAu8, 32)); // non-zero "side info"
        buf.extend_from_slice(b"Xing");
        buf.extend(std::iter::repeat_n(0u8, 22)); // reach the VBRI region's length gate too
        assert_eq!(detect(&buf), Some(VbrHeader::Absent));
    }

    #[test]
    fn detect_starts_the_zero_check_after_the_crc_not_the_tag_offset() {
        // The tag *offset* (`xing_pos`) deliberately never counts an
        // optional 2-byte CRC (symphonia's own comment: "The CRC is not
        // included in this offset calculation"), but `header_size` - where
        // the zero-check *starts* - does. Here the would-be CRC bytes are
        // non-zero (as a real CRC would be) while the rest of the side info
        // is zero: a correct implementation skips the CRC bytes and finds
        // the remainder all zero, accepting the tag; a version that used
        // the tag offset (or `has_crc = false`) for the zero-check's start
        // too would include those non-zero CRC bytes and wrongly reject it.
        let mut buf = vec![0xff, 0xfa, 0x90, 0x00]; // MPEG1 L3 stereo, CRC present
        buf.extend_from_slice(&[0xab, 0xcd]); // the 2 CRC bytes, non-zero
        buf.extend(std::iter::repeat_n(0u8, 30)); // the rest of side info, zeroed
        buf.extend_from_slice(b"Xing");
        buf.extend(std::iter::repeat_n(0u8, 22)); // reach the VBRI region's length gate too
        assert_eq!(detect(&buf), Some(VbrHeader::XingInfo));
    }

    #[test]
    fn detect_reports_vbri_for_a_valid_header_with_a_vbri_tag() {
        // A mono MPEG1 header: side_info_len is 17, distinct from VBRI's
        // fixed 32-byte offset, so a "VBRI" tag placed at the VBRI position
        // cannot be coincidentally matched by the Xing/Info check the way a
        // stereo header's matching offsets (32 either way) could hide a
        // broken VBRI branch behind a passing Xing/Info one.
        let mut buf = vec![0xff, 0xfb, 0x90, 0xc0]; // MPEG1 Layer III, mono
        buf.extend(std::iter::repeat_n(0u8, 32));
        buf.extend_from_slice(b"VBRI");
        // symphonia's `is_maybe_vbri_tag` requires 26 bytes past the VBRI
        // offset, not just the 4-byte tag id (`MIN_VBRI_TAG_LEN`) — pad past
        // that minimum, itself past symphonia's own floor.
        buf.extend(std::iter::repeat_n(0u8, 22));
        assert_eq!(detect(&buf), Some(VbrHeader::Vbri));
    }

    #[test]
    fn detect_reports_absent_for_vbri_bytes_short_of_symphonias_26_byte_minimum() {
        // The same bytes as `detect_reports_vbri_for_a_valid_header_with_a_vbri_tag`,
        // without the 22-byte pad past the tag id. symphonia's own
        // `is_maybe_vbri_tag` requires the buffer to reach 26 bytes past the
        // VBRI offset before it will even look at the tag id
        // (`MIN_VBRI_TAG_LEN`, demuxer.rs:1024) — the bare 4-byte id is not
        // enough. B4: a buffer this short must report "unknown" (`Absent`),
        // not accept the tag on 4 bytes symphonia itself would reject on
        // length alone.
        let mut buf = vec![0xff, 0xfb, 0x90, 0xc0]; // MPEG1 Layer III, mono
        buf.extend(std::iter::repeat_n(0u8, 32));
        buf.extend_from_slice(b"VBRI");
        assert_eq!(detect(&buf), Some(VbrHeader::Absent));
    }

    #[test]
    fn detect_rejects_vbri_bytes_over_non_zero_side_info() {
        // Mirrors `detect_rejects_xing_bytes_over_non_zero_side_info`, for
        // VBRI's own zero-side-info gate (`is_maybe_vbri_tag`,
        // demuxer.rs:1044-1045, B4). A mono header keeps the VBRI offset
        // (32) distinct from the Xing/Info offset (17), so this pins the
        // VBRI check specifically rather than riding on the Xing one.
        let mut buf = vec![0xff, 0xfb, 0x90, 0xc0]; // MPEG1 Layer III, mono
        buf.extend(std::iter::repeat_n(0xAAu8, 32)); // non-zero "side info"
        buf.extend_from_slice(b"VBRI");
        buf.extend(std::iter::repeat_n(0u8, 22)); // reach the 26-byte minimum
        assert_eq!(detect(&buf), Some(VbrHeader::Absent));
    }

    #[test]
    fn detect_ignores_vbri_bytes_in_a_non_layer3_frame() {
        // symphonia's `is_maybe_vbri_tag` refuses anything but a layer-3
        // frame before it looks at length, id or side info
        // (demuxer.rs:1027-1029, B4). Same header as
        // `detect_reports_vbri_for_a_valid_header_with_a_vbri_tag` with the
        // layer bits changed from Layer III (0b01) to Layer II (0b10);
        // everything else — the zeroed side info, the real "VBRI" id, the
        // padding past the 26-byte minimum — is left exactly as that test's
        // accepting case, so only the layer check can be what rejects it.
        let mut buf = vec![0xff, 0xfd, 0x90, 0xc0]; // MPEG1 Layer II, mono
        buf.extend(std::iter::repeat_n(0u8, 32));
        buf.extend_from_slice(b"VBRI");
        buf.extend(std::iter::repeat_n(0u8, 22));
        assert_eq!(detect(&buf), Some(VbrHeader::Absent));
    }

    #[test]
    fn detect_reports_absent_on_a_truncated_read_of_a_real_frame() {
        // A valid-looking header word but nothing after it to check either
        // tag region against: B3/§5.5 — this is an MP3 frame we could not
        // fully examine, not "not MP3", so it must report `Absent` (→
        // `Estimated`), never `None` (→ `Established`).
        let buf = vec![0xff, 0xfb, 0x90, 0x00];
        assert_eq!(detect(&buf), Some(VbrHeader::Absent));
    }

    #[test]
    fn detect_reports_none_when_the_frame_header_itself_is_unreachable_and_no_id3_tag_was_found() {
        // No ID3 tag (`first_frame_offset` returns `Some(0)`) and a buffer
        // too short even to hold the 4-byte frame header: genuinely "not
        // MP3, or too little to tell" — must stay `None`, load-bearing for
        // FLAC/WAV/M4A (B3).
        let buf = vec![0xff, 0xfb];
        assert_eq!(detect(&buf), None);
    }

    #[test]
    fn detect_reports_absent_when_an_id3_tag_pushes_the_frame_past_what_was_read() {
        // An ID3 tag was found (`frame_offset > 0`) but the buffer stops
        // before the frame header it points to — the exact shape a >64 KiB
        // ID3v2 tag produces against the real `PROBE_LEN` cap (B3). MP3-
        // shaped, no evidence gathered: `Absent`, never `None`.
        let mut buf = vec![b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0x22]; // frame at 10 + 34 = 44
        buf.resize(20, 0); // far short of the frame header at 44
        assert_eq!(detect(&buf), Some(VbrHeader::Absent));
    }

    #[test]
    fn detect_reports_none_for_bytes_that_are_not_an_mpeg_frame() {
        // FLAC's own signature, standing in for "not MP3 at all".
        let mut buf = b"fLaC".to_vec();
        buf.resize(200, 0);
        assert_eq!(detect(&buf), None);
    }
}
