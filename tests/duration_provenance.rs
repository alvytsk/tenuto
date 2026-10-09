//! Task 9 (§5.5): duration provenance from the MP3 container's own bytes.
//!
//! `decide_resume` (`src/resume.rs`) already retains a checkpoint instead of
//! discarding it as `StalePastEnd` when a duration's provenance is
//! `Estimated`. Before this task, `DecodedSource::open` hardcoded
//! `PositionProvenance::Established` for every duration, so that branch was
//! unreachable in production: a legitimate checkpoint on a headerless VBR
//! file was declared stale and discarded. These tests prove the producer
//! side — `probe_vbr_header`, wired into `DecodedSource::open` — actually
//! sets `Estimated` when the container gives no evidence, and that the
//! rescue this unlocks in `decide_resume` really fires end to end.

use std::path::Path;
use std::time::Duration;

use tenuto::media::id::AbsolutePath;
use tenuto::media::provenance::PositionProvenance;
use tenuto::playback::decode::DecodedSource;
use tenuto::resume::{KnownDuration, ResumeCandidate, ResumeDecision, decide_resume};

#[allow(clippy::unwrap_used)]
fn fixture(name: &str) -> AbsolutePath {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    AbsolutePath::new(path.canonicalize().unwrap()).unwrap()
}

#[allow(clippy::unwrap_used)]
fn open(name: &str) -> DecodedSource {
    DecodedSource::open(&fixture(name)).unwrap()
}

#[test]
fn an_mp3_with_a_xing_header_reports_established_duration() {
    let source = open("sine.mp3");
    assert_eq!(
        source.metadata().duration_provenance,
        PositionProvenance::Established,
        "sine.mp3 carries a Xing header; its duration is a real index, not an estimate"
    );
}

#[test]
fn an_mp3_without_a_xing_header_reports_estimated_duration() {
    // The fixture symphonia's own README documents as "-write_xing 0" — no
    // Xing/Info/VBRI marker anywhere in the file. Before this task, every
    // duration this decoder reported was hardcoded `Established`, so this
    // is the test that fails today.
    let source = open("sine-noxing.mp3");
    assert_eq!(
        source.metadata().duration_provenance,
        PositionProvenance::Estimated
    );
}

#[test]
fn a_long_vbr_mp3_without_a_header_reports_estimated_duration() {
    // The fixture whose duration symphonia's estimate_num_mpeg_frames gets
    // 40% wrong (600s true, ~361s estimated) — the exact scenario the bug
    // report measured.
    let source = open("sine-long-vbr-noxing.mp3");
    assert_eq!(
        source.metadata().duration_provenance,
        PositionProvenance::Estimated
    );
}

/// Splits off a leading ID3v2 tag, the same syncsafe-size arithmetic
/// `probe_vbr_header` uses, so the synthetic-tag test below can build a file
/// with exactly one ID3v2 tag (the large synthetic one) ahead of real audio
/// frames rather than stacking a second tag behind it.
#[allow(clippy::unwrap_used)]
fn strip_id3(bytes: &[u8]) -> &[u8] {
    assert!(
        bytes.len() >= 10 && &bytes[0..3] == b"ID3",
        "fixture must start with an ID3v2 tag"
    );
    let flags = bytes[5];
    let size = (u32::from(bytes[6] & 0x7f) << 21)
        | (u32::from(bytes[7] & 0x7f) << 14)
        | (u32::from(bytes[8] & 0x7f) << 7)
        | u32::from(bytes[9] & 0x7f);
    let mut offset = 10usize + size as usize;
    if flags & 0x10 != 0 {
        offset += 10;
    }
    &bytes[offset..]
}

fn syncsafe(size: u32) -> [u8; 4] {
    [
        ((size >> 21) & 0x7f) as u8,
        ((size >> 14) & 0x7f) as u8,
        ((size >> 7) & 0x7f) as u8,
        (size & 0x7f) as u8,
    ]
}

/// A spec-valid ID3v2.4 tag consisting of nothing but zeroed padding (no
/// frames) at least `min_len` bytes long including its 10-byte header — a
/// minimal, self-contained way to push the first MPEG frame well past
/// whatever a too-small probe buffer would read.
fn synthetic_id3v24_tag(min_len: usize) -> Vec<u8> {
    let padding = min_len.saturating_sub(10);
    let mut tag = Vec::with_capacity(10 + padding);
    tag.extend_from_slice(b"ID3");
    tag.push(4); // major version
    tag.push(0); // revision
    tag.push(0); // flags: no unsync, no extended header, no experimental, no footer
    tag.extend_from_slice(&syncsafe(padding as u32));
    tag.extend(std::iter::repeat_n(0u8, padding));
    tag
}

#[test]
#[allow(clippy::unwrap_used)]
fn a_xing_header_behind_a_large_id3_tag_is_still_found() {
    // NOTE: this does *not* pin the 8 KiB read floor — a probe shrunk to
    // 8 KiB still passes this test (`detect`'s short-read case fails open
    // to `None` -> `Established`, the same value asserted below, for the
    // wrong reason). That floor is pinned directly by the private unit test
    // `probe_vbr_header_finds_a_xing_tag_beyond_an_8kib_id3_tag`
    // (`src/media/vbr_header.rs`), which asserts the `VbrHeader` value
    // itself rather than the `PositionProvenance` it becomes. What this
    // test does prove: a real Xing/Info tag sitting behind a tag well past
    // the unconditional 4 KiB initial read (so `gather_evidence` must
    // extend its read) but still comfortably inside the 64 KiB cap is
    // found through the full `DecodedSource::open` pipeline, not just by
    // `detect` in isolation.
    let sine_bytes = std::fs::read(fixture("sine.mp3").as_path()).unwrap();
    let audio = strip_id3(&sine_bytes);
    let mut combined = synthetic_id3v24_tag(20 * 1024);
    assert!(
        combined.len() > 16 * 1024,
        "tag must be well past the unconditional 4 KiB initial read"
    );
    combined.extend_from_slice(audio);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large-id3.mp3");
    std::fs::write(&path, &combined).unwrap();
    let abs = AbsolutePath::new(path.canonicalize().unwrap()).unwrap();
    let source = DecodedSource::open(&abs).unwrap();

    assert_eq!(
        source.metadata().duration_provenance,
        PositionProvenance::Established,
        "a real Xing/Info tag well behind the initial 4 KiB read, but inside the 64 KiB probe, \
         must still be found end to end"
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn an_mp3_with_a_tag_past_probe_len_and_no_xing_header_reports_estimated_duration() {
    // B3/§5.5: an ID3v2 tag over 64 KiB (`PROBE_LEN`) pushes the first MPEG
    // frame past what `probe_vbr_header` reads at all. Before the fix,
    // `detect` returned `None` here — the same "gathered no evidence" value
    // it returns for a non-MP3 container — and `None` maps to `Established`
    // at the call site, silently claiming a real index for a duration
    // symphonia is actually estimating. The fixture behind the tag carries
    // no Xing/Info/VBRI tag either, so there is no header to find even once
    // the frame is reached.
    let sine_bytes = std::fs::read(fixture("sine-noxing.mp3").as_path()).unwrap();
    let audio = strip_id3(&sine_bytes);
    let mut combined = synthetic_id3v24_tag(80 * 1024);
    assert!(
        combined.len() > 64 * 1024,
        "tag must exceed PROBE_LEN so the first frame is never reached"
    );
    combined.extend_from_slice(audio);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge-id3-no-xing.mp3");
    std::fs::write(&path, &combined).unwrap();
    let abs = AbsolutePath::new(path.canonicalize().unwrap()).unwrap();
    let source = DecodedSource::open(&abs).unwrap();

    assert_eq!(
        source.metadata().duration_provenance,
        PositionProvenance::Estimated,
        "an MP3 frame this probe could not reach is unknown, not established"
    );
}

#[test]
fn a_flac_source_reports_established_duration() {
    let source = open("sine.flac");
    assert_eq!(
        source.metadata().duration_provenance,
        PositionProvenance::Established,
        "FLAC's STREAMINFO is a real duration; the MP3 probe must not touch non-MP3 containers"
    );
}

#[test]
fn an_estimated_duration_does_not_declare_a_checkpoint_stale() {
    // The end-to-end proof this task exists for: a real checkpoint sitting
    // past a wrong estimate must survive as `Unvalidated`, not be thrown
    // away as `StalePastEnd`.
    let source = open("sine-long-vbr-noxing.mp3");
    assert_eq!(
        source.metadata().duration_provenance,
        PositionProvenance::Estimated
    );
    let duration = source
        .metadata()
        .duration
        .unwrap_or_else(|| panic!("sine-long-vbr-noxing.mp3 must report some duration"));
    assert!(
        duration < Duration::from_secs(600),
        "expected the known under-estimate (~361s), got {duration:?} - \
         if this is no longer an under-estimate the checkpoint below is not actually testing \
         the rescue this task exists for"
    );

    let checkpoint = Duration::from_secs(400);
    let candidate = ResumeCandidate {
        position: checkpoint,
        completed: false,
    };
    let known_duration = KnownDuration {
        value: duration,
        provenance: source.metadata().duration_provenance,
    };
    let decision = decide_resume(Some(candidate), Some(known_duration));
    assert_eq!(decision, ResumeDecision::Unvalidated(checkpoint));
    assert_eq!(
        decision.start_at(),
        checkpoint,
        "the checkpoint must still read 400s, not be reset to zero"
    );
}

#[test]
fn an_established_duration_still_lets_a_genuinely_stale_checkpoint_through() {
    // Companion to the test above: proves this harness can still reach
    // `StalePastEnd` at all. Without this, the previous test could pass
    // against a `decide_resume` that simply never declares anything stale.
    let source = open("sine.mp3");
    assert_eq!(
        source.metadata().duration_provenance,
        PositionProvenance::Established
    );
    let duration = source
        .metadata()
        .duration
        .unwrap_or_else(|| panic!("sine.mp3 must report a Xing-established duration"));

    let checkpoint = duration + Duration::from_secs(5);
    let candidate = ResumeCandidate {
        position: checkpoint,
        completed: false,
    };
    let known_duration = KnownDuration {
        value: duration,
        provenance: PositionProvenance::Established,
    };
    let decision = decide_resume(Some(candidate), Some(known_duration));
    assert_eq!(decision, ResumeDecision::StalePastEnd);
}
