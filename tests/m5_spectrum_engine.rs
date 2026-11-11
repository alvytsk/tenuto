//! Task 28: transport mappings, the analysis worker's publish/expire
//! decisions, and the engine's spectrum wiring (spec §10, decisions 18/25).

mod support;

use std::time::{Duration, Instant};

use support::TestEngine;
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::output::Nanos;
use tenuto::playback::spectrum::registry::{TapMapping, TapRegistry};
use tenuto::playback::spectrum::worker::{
    AnalyzedWindow, FRAME_MAX_AGE, FrameSchedule, MappingKey, SlotUpdate, SpectrumFrame,
    frame_is_fresh,
};

fn mapping(instance: u64, generation: u16, epoch: u32, rev: u64) -> TapMapping {
    TapMapping {
        instance,
        generation,
        epoch,
        session_rev: rev,
        sample_rate: 48_000,
        channels: 2,
    }
}

#[test]
fn a_newer_generation_retires_the_older_one_and_a_reused_generation_never_matches_another_instance()
{
    let registry = TapRegistry::default();
    registry.publish(mapping(1, 5, 10, 3));
    assert!(registry.lookup(1, 5, 10).is_some());
    registry.publish(mapping(1, 6, 11, 3));
    assert!(
        registry.lookup(1, 5, 10).is_none(),
        "seek retired generation 5"
    );
    registry.publish(mapping(2, 6, 11, 4));
    assert_eq!(registry.lookup(2, 6, 11).map(|m| m.session_rev), Some(4));
    assert_eq!(
        registry.lookup(1, 6, 11).map(|m| m.session_rev),
        Some(3),
        "instances are distinct"
    );
    registry.retire_instance(1);
    assert!(registry.lookup(1, 6, 11).is_none());
    assert!(registry.lookup(2, 6, 12).is_none(), "unknown epoch");
}

#[test]
fn playback_publishes_frames_labelled_with_the_current_revision_only() {
    let mut engine = TestEngine::start("sine-5s.flac");
    let spectrum = engine.handle().spectrum();
    spectrum.set_enabled(true);
    engine.play_for(Duration::from_millis(600));
    let deadline = Instant::now() + Duration::from_secs(10);
    let frame = loop {
        let at = engine.position();
        engine.play_for(at + Duration::from_millis(50));
        if let Some(frame) = spectrum.latest() {
            break frame;
        }
        assert!(Instant::now() < deadline, "no spectrum frame");
    };
    assert_eq!(frame.session_rev, engine.progress().session_rev);
    assert!(!frame.levels.is_empty() && frame.levels.len() <= 24);
    assert_eq!(frame.levels.len(), frame.bands.len());

    engine.interrupt_stop();
    engine.await_state(tenuto::playback::state::PlaybackState::Stopped);
    let deadline = Instant::now() + Duration::from_secs(5);
    while spectrum.latest().is_some() {
        assert!(
            Instant::now() < deadline,
            "a retired transport's frame must be cleared"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    engine.finish();
}

/// Recovery is awaited through `DeviceRecovered`, as `engine_contract`'s own
/// recreation test does: `await_recovery_capture` polls for a `Freeze` that a
/// live test device acknowledges within one driver nap, so it can miss it.
#[test]
fn device_recreation_gets_a_new_instance_and_drops_old_mappings() {
    let mut engine = TestEngine::start("sine-5s.flac");
    let spectrum = engine.handle().spectrum();
    spectrum.set_enabled(true);
    engine.play_for(Duration::from_millis(300));
    let before = engine.progress().session_rev;
    engine.force_device_loss();
    engine.await_event(|e| matches!(e, PlaybackEvent::DeviceRecovered { .. }));
    // `position` settles behind a worker round trip, so the progress read
    // after it already carries the recovered revision.
    let at = engine.position();
    let recovered = engine.progress().session_rev;
    assert!(recovered > before);
    engine.play_for(at + Duration::from_millis(600));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(frame) = spectrum.latest() {
            assert_ne!(
                frame.session_rev, before,
                "an old transport's frame survived"
            );
            if frame.session_rev == recovered {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no frame for the recovered revision"
        );
        let at = engine.position();
        engine.play_for(at + Duration::from_millis(50));
    }
    engine.finish();
}

#[test]
fn disabling_clears_the_latest_frame_and_re_enabling_never_revives_it() {
    let mut engine = TestEngine::start("sine-5s.flac");
    let spectrum = engine.handle().spectrum();
    spectrum.set_enabled(true);
    engine.play_for(Duration::from_millis(400));
    let deadline = Instant::now() + Duration::from_secs(10);
    while spectrum.latest().is_none() {
        assert!(Instant::now() < deadline, "no spectrum frame");
        let at = engine.position();
        engine.play_for(at + Duration::from_millis(50));
    }
    spectrum.set_enabled(false);
    assert!(
        spectrum.latest().is_none(),
        "disabling hides the frame at once"
    );
    spectrum.set_enabled(true);
    assert!(spectrum.latest().is_none(), "re-enabling revives nothing");
    // The test device's clock is frozen between `play_for` calls, so no new
    // audio reaches the tap: whatever appears now could only be old state.
    let quiet = Instant::now() + Duration::from_millis(200);
    while Instant::now() < quiet {
        assert!(spectrum.latest().is_none(), "an old frame came back");
        std::thread::sleep(Duration::from_millis(5));
    }
    engine.finish();
}

#[test]
fn an_unchanged_frame_expires_even_when_its_revision_still_matches() {
    let now = Instant::now();
    let frame = SpectrumFrame {
        session_rev: 1,
        bands: vec![(40.0, 85.0)],
        levels: vec![1.0],
        at: Nanos(0),
        published_at: now,
    };
    assert!(frame_is_fresh(&frame, now));
    assert!(!frame_is_fresh(&frame, now + FRAME_MAX_AGE));
    assert!(!frame_is_fresh(&frame, now + Duration::from_secs(10)));
}

// ------------------------------------------------ the worker's decisions

const KEY: MappingKey = MappingKey {
    instance: 1,
    generation: 1,
    epoch: 2,
};

fn window(key: MappingKey, predicted_ms: u64, level: f32) -> AnalyzedWindow {
    AnalyzedWindow {
        key,
        session_rev: 7,
        bands: vec![(40.0, 85.0), (85.0, 170.0)],
        levels: vec![level, level],
        predicted: Nanos(predicted_ms * 1_000_000),
    }
}

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn clock(value: u64) -> Nanos {
    Nanos(value * 1_000_000)
}

fn published(update: SlotUpdate) -> SpectrumFrame {
    match update {
        SlotUpdate::Publish(frame) => frame,
        other => panic!("expected a publication, got {other:?}"),
    }
}

#[test]
fn a_window_waits_for_its_output_instant_then_publishes_with_a_fresh_stamp() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    schedule.offer(window(KEY, 100, 0.5));
    assert_eq!(
        schedule.tick(t0, clock(99)),
        SlotUpdate::Keep,
        "not audible yet"
    );
    let frame = published(schedule.tick(t0 + ms(5), clock(100)));
    assert_eq!(frame.session_rev, 7);
    assert_eq!(frame.at, clock(100));
    assert_eq!(frame.published_at, t0 + ms(5));
    assert_eq!(frame.levels, vec![0.5, 0.5]);
}

#[test]
fn publication_is_limited_to_twenty_frames_per_second_and_smoothed() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    schedule.offer(window(KEY, 0, 1.0));
    published(schedule.tick(t0, clock(0)));
    schedule.offer(window(KEY, 60, 0.0));
    assert_eq!(
        schedule.tick(t0 + ms(20), clock(60)),
        SlotUpdate::Keep,
        "50 ms have not passed since the last publication"
    );
    let frame = published(schedule.tick(t0 + ms(50), clock(70)));
    assert!(
        frame.levels.iter().all(|level| (level - 0.85).abs() < 1e-6),
        "level = max(new, previous x 0.85): {:?}",
        frame.levels
    );
}

#[test]
fn starvation_without_new_pcm_expires_the_latest_frame() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    schedule.offer(window(KEY, 0, 0.9));
    let frame = published(schedule.tick(t0, clock(0)));
    // The mapping stays valid and the output clock stops: no new PCM.
    assert_eq!(schedule.tick(t0 + ms(149), clock(10)), SlotUpdate::Keep);
    assert!(frame_is_fresh(&frame, t0 + ms(149)));
    assert_eq!(schedule.tick(t0 + ms(150), clock(10)), SlotUpdate::Clear);
    assert!(!frame_is_fresh(&frame, t0 + ms(150)));
    assert_eq!(
        schedule.tick(t0 + ms(400), clock(10)),
        SlotUpdate::Keep,
        "nothing left to clear or publish"
    );
    // A new window resumes the display.
    schedule.offer(window(KEY, 20, 0.4));
    let resumed = published(schedule.tick(t0 + ms(410), clock(20)));
    assert!(frame_is_fresh(&resumed, t0 + ms(410)));
}

#[test]
fn a_ready_window_expires_150_ms_after_its_output_instant_and_is_never_refreshed() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    schedule.offer(window(KEY, 0, 0.9));
    published(schedule.tick(t0, clock(0)));
    // Audible 60 ms of device time later, but the rate limit holds it back
    // and then the worker stalls: its PCM is old by the time it could go out.
    schedule.offer(window(KEY, 60, 0.9));
    assert_eq!(schedule.tick(t0 + ms(10), clock(60)), SlotUpdate::Keep);
    for step in 1..20 {
        let update = schedule.tick(t0 + ms(10 + step), clock(60));
        assert!(!matches!(update, SlotUpdate::Publish(_)), "{update:?}");
    }
    assert_eq!(
        schedule.tick(t0 + ms(160), clock(60)),
        SlotUpdate::Clear,
        "the published frame aged out"
    );
    assert_eq!(
        schedule.tick(t0 + ms(170), clock(60)),
        SlotUpdate::Keep,
        "the held-back window expired 150 ms after it became audible"
    );
}

#[test]
fn a_window_that_became_audible_long_ago_is_not_published_as_fresh() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    // The worker saw the device clock only after 500 ms of playback past it.
    schedule.offer(window(KEY, 0, 0.9));
    assert_eq!(schedule.tick(t0, clock(500)), SlotUpdate::Keep);
}

#[test]
fn reset_forgets_pending_and_latest_so_re_enabling_revives_nothing() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    schedule.offer(window(KEY, 0, 0.9));
    published(schedule.tick(t0, clock(0)));
    schedule.offer(window(KEY, 60, 0.9));
    schedule.reset();
    assert_eq!(schedule.tick(t0 + ms(60), clock(60)), SlotUpdate::Keep);
    assert_eq!(
        schedule.tick(t0 + ms(400), clock(400)),
        SlotUpdate::Keep,
        "neither the pending window nor the old latest frame came back"
    );
    // Smoothing restarts too: a quiet window is not lifted by the old one.
    schedule.offer(window(KEY, 500, 0.1));
    let frame = published(schedule.tick(t0 + ms(500), clock(500)));
    assert_eq!(frame.levels, vec![0.1, 0.1]);
}

#[test]
fn retired_mappings_clear_the_latest_frame_and_drop_pending_windows() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    schedule.offer(window(KEY, 0, 0.9));
    published(schedule.tick(t0, clock(0)));
    schedule.offer(window(KEY, 60, 0.9));
    assert_eq!(schedule.retire(|_| true), SlotUpdate::Keep);
    assert_eq!(schedule.retire(|key| key != KEY), SlotUpdate::Clear);
    assert_eq!(
        schedule.tick(t0 + ms(60), clock(60)),
        SlotUpdate::Keep,
        "the retired transport's pending window is gone too"
    );
}
