use std::path::Path;
use std::time::Duration;

use tenuto::media::capabilities::{Continuity, SeekSupport};
use tenuto::media::id::AbsolutePath;
use tenuto::playback::decode::DecodedSource;

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

/// Recovers a frame count from a `Duration`, rounding to the nearest frame.
///
/// `position()` reaches this Duration via `frames as f64 / sample_rate`,
/// which `Duration::from_secs_f64` then rounds to the nearest nanosecond;
/// inverting that with a *truncating* cast can land a whole frame short
/// purely from that nanosecond rounding (e.g. frame 16384 round-trips to
/// 16383.9999834). Rounding, not truncating, is the correct inverse.
/// For a target `Duration` supplied directly (not derived from `position()`),
/// this still matches `DecodedSource`'s internal (truncating)
/// `duration_to_frames` for every value used in these tests, since 250 ms and
/// 400 ms both convert to an exact frame count with no fractional part.
fn frames_from_duration(value: Duration, sample_rate: u32) -> u64 {
    (value.as_secs_f64() * f64::from(sample_rate)).round() as u64
}

#[test]
fn every_promised_format_decodes_to_planar_f32() {
    for name in ["sine.wav", "sine.flac", "sine.mp3"] {
        let mut source = open(name);
        assert_eq!(source.sample_rate(), 44_100, "{name}");
        assert_eq!(source.channels(), 2, "{name}");
        let mut frames = 0usize;
        while let Some(planes) = source.next_planar().unwrap() {
            assert_eq!(planes.len(), 2, "{name}");
            frames += planes[0].len();
        }
        // 0.5 s at 44100 Hz, allowing for codec delay and padding.
        assert!(
            (20_000..30_000).contains(&frames),
            "{name} decoded {frames} frames"
        );
    }
}

#[test]
fn metadata_and_capabilities_come_from_the_file() {
    let source = open("sine.flac");
    let duration = source.metadata().duration.unwrap();
    assert!(duration >= Duration::from_millis(450) && duration <= Duration::from_millis(550));
    let capabilities = source.capabilities();
    assert_eq!(capabilities.continuity, Continuity::Finite);
    assert_eq!(capabilities.seek, SeekSupport::Native);
}

#[test]
fn refined_seek_lands_at_the_requested_frame_not_the_packet_boundary() {
    // Symphonia's accurate seek lands at or before the target, so the decoder
    // must decode and discard forward to the exact frame.
    let mut source = open("sine.flac");
    let outcome = source
        .seek_refined(Duration::from_millis(250), None, &mut || false)
        .unwrap();
    assert!(!outcome.refinement_truncated);
    let delta = outcome.actual.as_millis().abs_diff(250);
    assert!(delta <= 2, "landed at {:?}, wanted 250 ms", outcome.actual);
}

#[test]
fn a_seek_on_a_container_whose_time_base_is_not_the_sample_rate_lands_on_target() {
    // Matroska timestamps blocks in the segment's own scale — milliseconds
    // by spec default — so this FLAC track's `time_base` is 1/1000, not
    // 1/44100. The seek used to seed its cursor with the demuxer's
    // timestamp verbatim, mixing millisecond ticks into frame arithmetic:
    // a 3.5 s target read the landing as 3500 *frames* (~79 ms), the
    // refinement loop then ran out of file long before its frame target,
    // and the seek reported landing near 1.6 s. The timestamp must be
    // converted through the track's time base so the landing is exact
    // exactly like the FLAC fixture above.
    let mut source = open("sine-5s.mka");
    let target = Duration::from_millis(3_500);
    let outcome = source.seek_refined(target, None, &mut || false).unwrap();
    assert!(!outcome.refinement_truncated);
    let delta = outcome.actual.as_millis().abs_diff(3_500);
    assert!(delta <= 2, "landed at {:?}, wanted 3.5 s", outcome.actual);

    // The landing must also be honest about the audio: draining the rest
    // yields exactly the frames past the target, no loss, no duplication.
    let mut whole = open("sine-5s.mka");
    let mut total = 0u64;
    while let Some(planes) = whole.next_planar().unwrap() {
        total += planes[0].len() as u64;
    }
    let target_frames = frames_from_duration(target, source.sample_rate());
    let mut remaining = 0u64;
    while let Some(planes) = source.next_planar().unwrap() {
        remaining += planes[0].len() as u64;
    }
    assert_eq!(
        remaining,
        total - target_frames,
        "expected total - target frames remaining after the seek and drain"
    );
}

#[test]
fn a_mid_packet_seek_hands_out_the_retained_tail_exactly_once() {
    // 250 ms lands inside a FLAC block (blocks are ~92.9 ms at this sample
    // rate), so `seek_refined` must trim that block and retain its tail for
    // the next `next_planar` call rather than decoding it again or dropping it.
    let mut source = open("sine.flac");
    let sample_rate = source.sample_rate();
    let target = Duration::from_millis(250);
    let outcome = source.seek_refined(target, None, &mut || false).unwrap();
    assert!(!outcome.refinement_truncated);
    let pos_after_seek = source.position();

    let first: Vec<Vec<f32>> = source
        .next_planar()
        .unwrap()
        .expect("audio remains after a mid-file seek")
        .to_vec();
    let tail_len = first[0].len();
    assert!(
        tail_len > 0,
        "expected a non-empty retained tail after a mid-packet seek"
    );

    // `position()` is derived from the same cursor `next_planar` advances, so
    // it must move forward by exactly the number of frames just handed out —
    // no more (duplication), no less (loss).
    let pos_after_first = source.position();
    let advanced = frames_from_duration(pos_after_first, sample_rate)
        - frames_from_duration(pos_after_seek, sample_rate);
    assert_eq!(
        advanced, tail_len as u64,
        "position advanced by {advanced} frames but {tail_len} were returned"
    );

    // The retained tail must be handed out exactly once: the next call has to
    // be a fresh packet, not the same tail again.
    let second: Vec<Vec<f32>> = source
        .next_planar()
        .unwrap()
        .expect("more audio remains after the retained tail")
        .to_vec();
    assert_ne!(
        second, first,
        "the retained tail was handed out a second time instead of a fresh packet"
    );
    let advanced_2 = frames_from_duration(source.position(), sample_rate)
        - frames_from_duration(pos_after_first, sample_rate);
    assert_eq!(
        advanced_2,
        second[0].len() as u64,
        "position must also track the fresh packet that follows the retained tail"
    );
}

#[test]
fn seeking_mid_file_then_draining_yields_exactly_the_remaining_frames() {
    // Decode the whole fixture once, unseeked, to learn its true total frame
    // count.
    let mut whole = open("sine.flac");
    let mut total = 0u64;
    while let Some(planes) = whole.next_planar().unwrap() {
        total += planes[0].len() as u64;
    }

    let mut source = open("sine.flac");
    let target = Duration::from_millis(250);
    let outcome = source.seek_refined(target, None, &mut || false).unwrap();
    assert!(!outcome.refinement_truncated);
    let target_frames = frames_from_duration(target, source.sample_rate());

    let mut remaining = 0u64;
    while let Some(planes) = source.next_planar().unwrap() {
        remaining += planes[0].len() as u64;
    }

    // This single assertion catches both frame loss (remaining too small,
    // e.g. the retained tail silently dropped) and duplication (remaining
    // too large, e.g. the retained tail handed out twice).
    assert_eq!(
        remaining,
        total - target_frames,
        "expected total - target frames remaining after a mid-file seek and drain"
    );
}

#[test]
fn plane_lengths_stay_equal_across_a_mid_packet_seek_trim() {
    let mut source = open("sine.flac");
    let outcome = source
        .seek_refined(Duration::from_millis(250), None, &mut || false)
        .unwrap();
    assert!(!outcome.refinement_truncated);

    let planes = source
        .next_planar()
        .unwrap()
        .expect("retained tail expected after a mid-packet seek");
    let first_len = planes[0].len();
    for (channel, plane) in planes.iter().enumerate() {
        assert_eq!(
            plane.len(),
            first_len,
            "channel {channel} plane length diverged from channel 0 after a seek trim"
        );
    }
}

#[test]
fn an_exhausted_refinement_budget_reports_where_it_actually_arrived() {
    let mut source = open("sine.flac");
    let outcome = source
        .seek_refined(
            Duration::from_millis(400),
            Some(Duration::ZERO),
            &mut || false,
        )
        .unwrap();
    assert!(outcome.refinement_truncated);
    assert!(outcome.actual <= Duration::from_millis(400));
}

#[test]
fn cancellation_is_observed_between_decode_steps() {
    let mut source = open("sine.flac");
    let mut calls = 0;
    let result = source.seek_refined(Duration::from_millis(400), None, &mut || {
        calls += 1;
        true
    });
    assert!(matches!(
        result,
        Err(tenuto::playback::error::PlaybackError::Cancelled)
    ));
}

#[test]
fn a_missing_file_reports_a_contextual_error() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/absent.wav");
    let error = DecodedSource::open(&AbsolutePath::new(path).unwrap()).unwrap_err();
    assert!(error.to_string().contains("absent.wav"), "got: {error}");
}

#[test]
fn seeking_to_zero_lands_at_zero_on_every_format() {
    // Regression: MP3 readers report a negative `actual_ts` when a seek lands in
    // the encoder's delay region. Casting that straight to u64 wrapped the
    // cursor to ~1.8e19, and the next `cursor += frames` panicked with
    // "attempt to add with overflow" on the decode thread — reachable from the
    // UI by pressing Home (restart) on an MP3.
    for name in ["sine.mp3", "sine.flac", "sine.wav"] {
        let mut source = open(name);
        source.next_planar().unwrap();

        let outcome = source
            .seek_refined(Duration::ZERO, None, &mut || false)
            .unwrap();
        assert_eq!(
            outcome.actual,
            Duration::ZERO,
            "{name} did not land at zero"
        );
        assert_eq!(
            source.position(),
            Duration::ZERO,
            "{name} cursor is not zero"
        );

        // The overflow only fired on the decode AFTER the seek, so drain a few
        // packets: a wrapped cursor panics here rather than at the seek itself.
        for _ in 0..4 {
            if source.next_planar().unwrap().is_none() {
                break;
            }
        }
        assert!(
            source.position() < Duration::from_secs(1),
            "{name} position ran away after restart: {:?}",
            source.position()
        );
    }
}
