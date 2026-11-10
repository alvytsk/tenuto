//! The `tenuto-spectrum` analysis worker (spec §10, decisions 18 and 25).
//!
//! One thread per [`crate::playback::engine::EngineHandle`]. It owns the
//! reading side of every transport's output tap, looks each block's label up
//! in the [`TapRegistry`], runs the FFT, and publishes at most
//! [`MAX_FRAMES_PER_SECOND`] labelled frames into a latest-value slot, each
//! one only once the output clock says its audio is being heard.
//!
//! The decisions - when a window is audible, when a frame goes out, when one
//! has expired - live in [`FrameSchedule`], a pure type that takes the
//! monotonic `now` and the device clock as parameters, so they are tested
//! without sleeping. The thread around it only moves data.
//!
//! While analysis is disabled the thread does not poll: it blocks on its
//! control channel until [`SpectrumHandle::set_enabled`], a reader attach or
//! shutdown wakes it, and forgets every frame, window and block it held.
//!
//! **Panics are not contained.** This thread belongs to the engine, and the
//! contained-panic boundary is reserved for artwork decoding/encoding and
//! metadata probing (§9, §11); "panics outside these job boundaries must
//! still take the fatal path" (§12). A panic anywhere here - the FFT
//! included - unwinds and ends this thread, and in the terminal player the
//! §11 panic hook fails the application. The engine's own shutdown still
//! completes: `SpectrumThread::stop` only sets a flag, sends on a channel
//! whose failure is ignored, and joins a thread that has already exited,
//! logging the panic rather than propagating it. A dead thread's control
//! channel is disconnected, so later attaches and wakes are dropped, and its
//! taps' rings simply fill and drop whole blocks on the callback side.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use super::analyzer::SpectrumAnalyzer;
use super::registry::{TapMapping, TapRegistry};
use super::tap::{DESCRIPTOR_CAPACITY, TapDescriptor, TapReader, TapWriter, tap_pair};
use crate::playback::output::Nanos;

pub const MAX_FRAMES_PER_SECOND: u32 = 20;
/// How long a published frame, or a window whose audio has started playing,
/// stays drawable without newer PCM (decision 25).
pub const FRAME_MAX_AGE: Duration = Duration::from_millis(150);
/// `level = max(new, previous × SMOOTHING)` between successive frames of one
/// mapping.
pub const SMOOTHING: f32 = 0.85;
/// The reference output width of a frame's decimated waveform: one
/// `(min, peak)` pair per column (§1.1).
pub const WAVEFORM_COLUMNS: usize = 96;

/// `1 s / MAX_FRAMES_PER_SECOND`.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(1_000 / MAX_FRAMES_PER_SECOND as u64);
/// The cadence at which the latest frame's mapping is checked against the
/// registry, so a retired transport's spectrum disappears without new audio.
const RETIREMENT_CHECK: Duration = Duration::from_millis(50);
/// How long an enabled worker waits for a control message between passes.
/// Shorter than `PUBLISH_INTERVAL`, so a window is published close to the
/// moment its audio is heard.
const ENABLED_POLL: Duration = Duration::from_millis(10);
/// Windows waiting for their output instant. Coalesced to one per
/// `PUBLISH_INTERVAL` of device time, this covers well over the half second
/// of audio a tap ring holds.
const PENDING_CAPACITY: usize = 32;

/// One published spectrum: the revision it belongs to, the band edges in Hz,
/// one level per band, the output instant of its audio, and the monotonic
/// instant it was published.
#[derive(Clone, Debug, PartialEq)]
pub struct SpectrumFrame {
    pub session_rev: u64,
    pub bands: Vec<(f64, f64)>,
    pub levels: Vec<f32>,
    /// The block's decimated `(min, peak)` envelope from
    /// [`decimate_waveform`], present only while a listener has asked for
    /// scope mode since the last frame (`SpectrumHandle::set_wants_waveform`).
    /// When nobody wants it this is `None` and the worker's work per pass is
    /// unchanged.
    pub waveform: Option<Vec<(f32, f32)>>,
    pub at: Nanos,
    pub published_at: Instant,
}

/// Whether `frame` may still be drawn at `now`. Revision or token equality
/// alone never makes a frame fresh (decision 25).
pub fn frame_is_fresh(frame: &SpectrumFrame, now: Instant) -> bool {
    now.checked_duration_since(frame.published_at)
        .is_some_and(|age| age < FRAME_MAX_AGE)
}

/// Decimates one block of interleaved PCM into exactly [`WAVEFORM_COLUMNS`]
/// `(min, peak)` pairs for the scope's envelope. Column `c` covers the
/// samples `[c * len / WAVEFORM_COLUMNS, (c+1) * len / WAVEFORM_COLUMNS)` in
/// plain `usize` math; a column that covers no sample (the input is shorter
/// than the columns before it) is `(0.0, 0.0)`, so an empty input decimates
/// to 96 midline pairs. Each bound clamps to `[-1.0, 1.0]`, the range the
/// widget renders, and a non-finite sample is silence — the same rule the
/// levels follow in `Analysis::accept`. Pure: the same samples always
/// decimate to the same pairs.
pub fn decimate_waveform(samples: &[f32]) -> Vec<(f32, f32)> {
    let len = samples.len();
    let mut columns = Vec::with_capacity(WAVEFORM_COLUMNS);
    for column in 0..WAVEFORM_COLUMNS {
        let start = column * len / WAVEFORM_COLUMNS;
        let end = (column + 1) * len / WAVEFORM_COLUMNS;
        let bounds = samples[start..end]
            .iter()
            .copied()
            .fold(None::<(f32, f32)>, |acc, sample| {
                let sample = if sample.is_finite() { sample } else { 0.0 };
                match acc {
                    Some((min, max)) => Some((min.min(sample), max.max(sample))),
                    None => Some((sample, sample)),
                }
            });
        // A column that covers no sample is the flat midline; every real
        // bound clamps into the unit interval the widget renders.
        let (min, max) = bounds.unwrap_or((0.0, 0.0));
        columns.push((min.clamp(-1.0, 1.0), max.clamp(-1.0, 1.0)));
    }
    columns
}

/// The label a tap block carries, and the key a mapping is published under.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MappingKey {
    pub instance: u64,
    pub generation: u16,
    pub epoch: u32,
}

impl MappingKey {
    fn of(descriptor: &TapDescriptor) -> Self {
        Self {
            instance: descriptor.instance,
            generation: descriptor.generation,
            epoch: descriptor.epoch,
        }
    }
}

/// One analyzed window, labelled through its mapping, not yet published.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzedWindow {
    pub key: MappingKey,
    pub session_rev: u64,
    pub bands: Vec<(f64, f64)>,
    pub levels: Vec<f32>,
    /// The block's decimated waveform, or `None` when no listener asked for
    /// it this pass.
    pub waveform: Option<Vec<(f32, f32)>>,
    /// The predicted output instant of the block that completed the window.
    pub predicted: Nanos,
}

/// What to do with the latest-frame slot after a decision.
#[derive(Clone, Debug, PartialEq)]
pub enum SlotUpdate {
    Keep,
    Publish(SpectrumFrame),
    Clear,
}

/// A window whose output instant has been reached, and the monotonic
/// instant that happened - tracked apart from its device timestamp. Only a
/// newer window replaces it, with its own instant; nothing ever moves this
/// one's later, so waiting on the rate limit cannot make old PCM fresh.
#[derive(Debug)]
struct Ready {
    window: AnalyzedWindow,
    ready_at: Instant,
}

#[derive(Debug)]
struct Published {
    key: MappingKey,
    published_at: Instant,
}

/// The worker's publish and expiry decisions, free of threads and clocks.
#[derive(Debug, Default)]
pub struct FrameSchedule {
    pending: VecDeque<AnalyzedWindow>,
    ready: Option<Ready>,
    latest: Option<Published>,
    last_publication: Option<Instant>,
    /// The levels last published, for smoothing the next frame of the same
    /// mapping.
    smoothed: Option<(MappingKey, Vec<f32>)>,
}

impl FrameSchedule {
    /// Queues a newly analyzed window. A window of another mapping first
    /// discards everything still waiting; one that would play less than a
    /// publication interval after the last queued window is skipped, since
    /// it could never be published anyway.
    pub fn offer(&mut self, window: AnalyzedWindow) {
        let other_mapping = self
            .pending
            .back()
            .map(|tail| tail.key)
            .or(self.ready.as_ref().map(|ready| ready.window.key))
            .is_some_and(|key| key != window.key);
        if other_mapping {
            self.discard_pending();
        } else if self.pending.back().is_some_and(|tail| {
            window.predicted.0 < tail.predicted.0.saturating_add(interval_nanos())
        }) {
            return;
        }
        if self.pending.len() == PENDING_CAPACITY {
            self.pending.pop_front();
        }
        self.pending.push_back(window);
    }

    /// Decides the latest slot at monotonic `now`, with the output device's
    /// clock at `clock`.
    pub fn tick(&mut self, now: Instant, clock: Nanos) -> SlotUpdate {
        while self
            .pending
            .front()
            .is_some_and(|window| window.predicted.0 <= clock.0)
        {
            let Some(window) = self.pending.pop_front() else {
                break;
            };
            // Heard `late` ago by the device's own account: a worker that
            // only now noticed must not hand out old audio as new.
            let late = Duration::from_nanos(clock.0 - window.predicted.0).min(FRAME_MAX_AGE);
            let ready_at = now.checked_sub(late).unwrap_or(now);
            self.ready = Some(Ready { window, ready_at });
        }
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| now.saturating_duration_since(ready.ready_at) >= FRAME_MAX_AGE)
        {
            self.ready = None;
        }
        let expired = self.latest.as_ref().is_some_and(|latest| {
            now.saturating_duration_since(latest.published_at) >= FRAME_MAX_AGE
        });
        if expired {
            self.latest = None;
        }
        let allowed = self
            .last_publication
            .is_none_or(|last| now.saturating_duration_since(last) >= PUBLISH_INTERVAL);
        if allowed && let Some(Ready { window, .. }) = self.ready.take() {
            return SlotUpdate::Publish(self.publish(window, now));
        }
        if expired {
            SlotUpdate::Clear
        } else {
            SlotUpdate::Keep
        }
    }

    /// Drops every window and frame whose mapping `is_live` no longer
    /// recognizes; clears the slot when that includes the latest frame.
    pub fn retire(&mut self, is_live: impl Fn(MappingKey) -> bool) -> SlotUpdate {
        self.pending.retain(|window| is_live(window.key));
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| !is_live(ready.window.key))
        {
            self.ready = None;
        }
        if self
            .smoothed
            .as_ref()
            .is_some_and(|(key, _)| !is_live(*key))
        {
            self.smoothed = None;
        }
        if self
            .latest
            .as_ref()
            .is_some_and(|latest| !is_live(latest.key))
        {
            self.latest = None;
            return SlotUpdate::Clear;
        }
        SlotUpdate::Keep
    }

    /// Drops the windows not yet published, keeping the latest frame.
    pub fn discard_pending(&mut self) {
        self.pending.clear();
        self.ready = None;
    }

    /// Forgets everything: nothing held before a reset is ever published or
    /// smoothed into a frame after it.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn publish(&mut self, window: AnalyzedWindow, now: Instant) -> SpectrumFrame {
        let AnalyzedWindow {
            key,
            session_rev,
            bands,
            mut levels,
            waveform,
            predicted,
        } = window;
        if let Some((previous_key, previous)) = &self.smoothed
            && *previous_key == key
            && previous.len() == levels.len()
        {
            for (level, previous) in levels.iter_mut().zip(previous) {
                *level = level.max(previous * SMOOTHING);
            }
        }
        self.smoothed = Some((key, levels.clone()));
        self.latest = Some(Published {
            key,
            published_at: now,
        });
        self.last_publication = Some(now);
        SpectrumFrame {
            session_rev,
            bands,
            levels,
            // The envelope is honest: copied verbatim, never smoothed.
            waveform,
            at: predicted,
            published_at: now,
        }
    }
}

fn interval_nanos() -> u64 {
    u64::try_from(PUBLISH_INTERVAL.as_nanos()).unwrap_or(u64::MAX)
}

// ------------------------------------------------------------- analysis

/// Turns labelled blocks into windows for the schedule, resetting whenever
/// the mapping, the format or the discontinuity sequence changes.
#[derive(Default)]
struct Analysis {
    analyzer: Option<SpectrumAnalyzer>,
    bands: Vec<(f64, f64)>,
    format: Option<(u32, u16)>,
    key: Option<MappingKey>,
    discontinuity: u32,
}

impl Analysis {
    /// Drops the analyzer and everything it had buffered.
    fn clear(&mut self) {
        self.analyzer = None;
        self.bands.clear();
        self.format = None;
        self.key = None;
    }

    fn accept(
        &mut self,
        descriptor: &TapDescriptor,
        mapping: &TapMapping,
        samples: &[f32],
        wants_waveform: bool,
        schedule: &mut FrameSchedule,
    ) {
        let key = MappingKey::of(descriptor);
        let format = (descriptor.sample_rate, descriptor.channels);
        let remapped = self.key != Some(key);
        if remapped {
            schedule.discard_pending();
        }
        // A new format needs a new analyzer altogether; a new mapping or a
        // gap in the PCM restarts the window.
        if self.format != Some(format) {
            self.analyzer = None;
        } else if (remapped || self.discontinuity != descriptor.discontinuity)
            && let Some(analyzer) = self.analyzer.as_mut()
        {
            analyzer.reset();
        }
        self.key = Some(key);
        self.format = Some(format);
        self.discontinuity = descriptor.discontinuity;

        let bands = &mut self.bands;
        let analyzer = self.analyzer.get_or_insert_with(|| {
            let analyzer = SpectrumAnalyzer::new(format.0, format.1);
            *bands = analyzer
                .bands()
                .iter()
                .map(|band| (band.low_hz, band.high_hz))
                .collect();
            analyzer
        });
        let Some(mut levels) = analyzer.push_interleaved(samples) else {
            return;
        };
        for level in &mut levels {
            if !level.is_finite() {
                *level = 0.0;
            }
        }
        // Only the block that completes a window pays for the decimation,
        // and only when a listener actually wants the waveform.
        let waveform = wants_waveform.then(|| decimate_waveform(samples));
        schedule.offer(AnalyzedWindow {
            key,
            session_rev: mapping.session_rev,
            bands: self.bands.clone(),
            levels,
            waveform,
            predicted: descriptor.predicted,
        });
    }
}

// --------------------------------------------------------------- thread

enum Control {
    Attach(TapReader),
    Wake,
}

struct Shared {
    /// Also every tap writer's own flag: a disabled tap copies nothing.
    enabled: Arc<AtomicBool>,
    /// Bumped on every enable or disable. A frame is tagged with the value
    /// the worker last reset under, and only a frame tagged with the current
    /// value is ever handed out.
    switches: AtomicU64,
    /// Set while a listener wants each frame's decimated waveform (scope
    /// mode). A level, not an event: the runner re-reads it on every enabled
    /// pass (≤10 ms), so no wake travels on it.
    wants_waveform: AtomicBool,
    latest: Mutex<Option<(u64, SpectrumFrame)>>,
    shutdown: AtomicBool,
}

impl Shared {
    fn latest(&self) -> MutexGuard<'_, Option<(u64, SpectrumFrame)>> {
        crate::playback::lock(&self.latest)
    }
}

/// The application's side of the analysis worker.
#[derive(Clone)]
pub struct SpectrumHandle {
    shared: Arc<Shared>,
    registry: TapRegistry,
    control: Sender<Control>,
}

impl SpectrumHandle {
    /// Starts or suspends analysis. Disabling hides the latest frame at
    /// once; re-enabling never brings back anything from before.
    pub fn set_enabled(&self, enabled: bool) {
        if self.shared.enabled.swap(enabled, Ordering::SeqCst) == enabled {
            return;
        }
        self.shared.switches.fetch_add(1, Ordering::SeqCst);
        if !enabled {
            *self.shared.latest() = None;
        }
        let _ = self.control.send(Control::Wake);
    }

    /// Requests (or cancels) each published frame's decimated waveform for
    /// the scope. A Relaxed store is enough — the runner re-reads the flag
    /// on every enabled pass, and a frame or two of lag is invisible at
    /// [`MAX_FRAMES_PER_SECOND`] — so no wake is sent: when nothing else
    /// stirs, there is no frame to carry a waveform anyway.
    pub fn set_wants_waveform(&self, wanted: bool) {
        self.shared.wants_waveform.store(wanted, Ordering::Relaxed);
    }

    /// The latest published frame, if analysis is enabled and one exists.
    /// Freshness is the caller's to judge with [`frame_is_fresh`].
    pub fn latest(&self) -> Option<SpectrumFrame> {
        if !self.shared.enabled.load(Ordering::SeqCst) {
            return None;
        }
        let switches = self.shared.switches.load(Ordering::SeqCst);
        self.shared
            .latest()
            .as_ref()
            .filter(|(tag, _)| *tag == switches)
            .map(|(_, frame)| frame.clone())
    }

    pub fn registry(&self) -> &TapRegistry {
        &self.registry
    }
}

/// The playback worker's side: builds each transport's tap and hands its
/// reader to the analysis thread.
pub(crate) struct SpectrumPort {
    registry: TapRegistry,
    enabled: Arc<AtomicBool>,
    control: Sender<Control>,
    next_instance: u64,
}

impl SpectrumPort {
    /// A tap for a new transport under a never-reused instance ID.
    pub(crate) fn open_tap(&mut self, channels: u16, sample_rate: u32) -> (u64, TapWriter) {
        let instance = self.next_instance;
        self.next_instance = self.next_instance.wrapping_add(1);
        let (writer, reader) = tap_pair(instance, channels, sample_rate, Arc::clone(&self.enabled));
        let _ = self.control.send(Control::Attach(reader));
        (instance, writer)
    }

    pub(crate) fn registry(&self) -> &TapRegistry {
        &self.registry
    }
}

/// Ownership of the analysis thread, for the engine to stop and join.
pub(crate) struct SpectrumThread {
    shared: Arc<Shared>,
    control: Sender<Control>,
    join: Option<JoinHandle<()>>,
}

impl SpectrumThread {
    /// Asks the thread to exit and joins it. Idempotent.
    pub(crate) fn stop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        let _ = self.control.send(Control::Wake);
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            tracing::warn!("the spectrum worker panicked");
        }
    }
}

/// Starts the analysis thread, reading output time from `device_clock`.
pub(crate) fn spawn(
    device_clock: Arc<AtomicU64>,
) -> (SpectrumHandle, SpectrumPort, SpectrumThread) {
    let shared = Arc::new(Shared {
        enabled: Arc::new(AtomicBool::new(false)),
        switches: AtomicU64::new(0),
        wants_waveform: AtomicBool::new(false),
        latest: Mutex::new(None),
        shutdown: AtomicBool::new(false),
    });
    let registry = TapRegistry::default();
    let (control_tx, control_rx) = crossbeam_channel::unbounded();
    let runner = Runner {
        shared: Arc::clone(&shared),
        registry: registry.clone(),
        control: control_rx,
        device_clock,
        readers: Vec::new(),
        analysis: Analysis::default(),
        schedule: FrameSchedule::default(),
        seen_switches: 0,
        next_retirement_check: Instant::now(),
        scratch: Vec::new(),
    };
    let join = std::thread::Builder::new()
        .name("tenuto-spectrum".into())
        .spawn(move || runner.run())
        .ok();
    if join.is_none() {
        tracing::warn!("could not start the spectrum worker; the spectrum stays flat");
    }
    (
        SpectrumHandle {
            shared: Arc::clone(&shared),
            registry: registry.clone(),
            control: control_tx.clone(),
        },
        SpectrumPort {
            registry,
            enabled: Arc::clone(&shared.enabled),
            control: control_tx.clone(),
            next_instance: 1,
        },
        SpectrumThread {
            shared,
            control: control_tx,
            join,
        },
    )
}

struct Runner {
    shared: Arc<Shared>,
    registry: TapRegistry,
    control: Receiver<Control>,
    device_clock: Arc<AtomicU64>,
    readers: Vec<TapReader>,
    analysis: Analysis,
    schedule: FrameSchedule,
    seen_switches: u64,
    next_retirement_check: Instant,
    scratch: Vec<f32>,
}

impl Runner {
    fn run(mut self) {
        loop {
            if self.shared.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let switches = self.shared.switches.load(Ordering::SeqCst);
            if switches != self.seen_switches {
                self.forget();
                self.seen_switches = switches;
            }
            if !self.shared.enabled.load(Ordering::SeqCst) {
                self.forget();
                // No polling while disabled: only an enable, an attach or
                // shutdown gets this thread going again.
                match self.control.recv() {
                    Ok(control) => self.apply(control),
                    Err(_) => return,
                }
                continue;
            }
            while let Ok(control) = self.control.try_recv() {
                self.apply(control);
            }
            self.analyze_blocks();
            let now = Instant::now();
            if now >= self.next_retirement_check {
                let registry = &self.registry;
                let update = self.schedule.retire(|key| {
                    registry
                        .lookup(key.instance, key.generation, key.epoch)
                        .is_some()
                });
                self.store(update);
                self.next_retirement_check = now + RETIREMENT_CHECK;
            }
            let clock = Nanos(self.device_clock.load(Ordering::Relaxed));
            let update = self.schedule.tick(now, clock);
            self.store(update);
            match self.control.recv_timeout(ENABLED_POLL) {
                Ok(control) => self.apply(control),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn apply(&mut self, control: Control) {
        match control {
            Control::Attach(reader) => self.readers.push(reader),
            Control::Wake => {}
        }
    }

    /// Everything held from before a disable or re-enable: frames, windows,
    /// the analyzer, and every block already sitting in a tap ring.
    fn forget(&mut self) {
        self.schedule.reset();
        self.analysis.clear();
        for reader in &mut self.readers {
            while reader.next_block(&mut self.scratch).is_some() {
                self.scratch.clear();
            }
        }
        self.scratch.clear();
        self.readers.retain(|reader| !reader.is_finished());
        *self.shared.latest() = None;
    }

    fn analyze_blocks(&mut self) {
        // Read once per pass: every block of this pass sees the same
        // waveform request, whatever the front end does mid-pass.
        let wants_waveform = self.shared.wants_waveform.load(Ordering::Relaxed);
        let Self {
            readers,
            registry,
            analysis,
            schedule,
            scratch,
            ..
        } = self;
        for reader in readers.iter_mut() {
            // Bounded per pass: a ring holds at most this many blocks.
            for _ in 0..DESCRIPTOR_CAPACITY {
                scratch.clear();
                let Some(descriptor) = reader.next_block(scratch) else {
                    break;
                };
                let mapping =
                    registry.lookup(descriptor.instance, descriptor.generation, descriptor.epoch);
                match mapping {
                    Some(mapping)
                        if mapping.sample_rate == descriptor.sample_rate
                            && mapping.channels == descriptor.channels =>
                    {
                        analysis.accept(&descriptor, &mapping, scratch, wants_waveform, schedule);
                    }
                    // Unknown, retired or mislabelled: discarded.
                    _ => {}
                }
            }
        }
        scratch.clear();
        readers.retain(|reader| !reader.is_finished());
    }

    fn store(&self, update: SlotUpdate) {
        match update {
            SlotUpdate::Keep => {}
            SlotUpdate::Publish(frame) => {
                *self.shared.latest() = Some((self.seen_switches, frame));
            }
            SlotUpdate::Clear => *self.shared.latest() = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(discontinuity: u32) -> TapDescriptor {
        TapDescriptor {
            instance: 1,
            generation: 1,
            epoch: 2,
            channels: 1,
            sample_rate: 48_000,
            discontinuity,
            predicted: Nanos(0),
            samples: 0,
        }
    }

    const MAPPING: TapMapping = TapMapping {
        instance: 1,
        generation: 1,
        epoch: 2,
        session_rev: 3,
        sample_rate: 48_000,
        channels: 1,
    };

    #[test]
    fn a_window_is_labelled_through_its_mapping_with_finite_levels() {
        let mut analysis = Analysis::default();
        let mut schedule = FrameSchedule::default();
        let mut samples = vec![0.25_f32; super::super::bands::WINDOW];
        samples[7] = f32::NAN;
        analysis.accept(&descriptor(0), &MAPPING, &samples, false, &mut schedule);
        let SlotUpdate::Publish(frame) = schedule.tick(Instant::now(), Nanos(0)) else {
            panic!("a full window was not published");
        };
        assert_eq!(frame.session_rev, 3);
        assert_eq!(frame.levels.len(), frame.bands.len());
        assert!(frame.levels.iter().all(|level| level.is_finite()));
    }

    /// §12: an analysis panic takes the fatal path, so the thread is simply
    /// gone by the time the engine shuts down. Stopping it must neither hang
    /// nor re-panic, however many times it is asked.
    #[test]
    fn stopping_a_spectrum_thread_that_already_panicked_completes() {
        let shared = Arc::new(Shared {
            enabled: Arc::new(AtomicBool::new(true)),
            switches: AtomicU64::new(0),
            wants_waveform: AtomicBool::new(false),
            latest: Mutex::new(None),
            shutdown: AtomicBool::new(false),
        });
        let (control, receiver) = crossbeam_channel::unbounded::<Control>();
        let join = std::thread::Builder::new()
            .name("tenuto-spectrum".into())
            .spawn(move || {
                let _receiver = receiver;
                panic!("analysis bug");
            })
            .ok();
        let mut thread = SpectrumThread {
            shared,
            control: control.clone(),
            join,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while thread.join.as_ref().is_some_and(|join| !join.is_finished()) {
            assert!(
                Instant::now() < deadline,
                "the panicking thread never ended"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            control.send(Control::Wake).is_err(),
            "a dead thread's channel is disconnected"
        );
        thread.stop();
        thread.stop();
        assert!(thread.join.is_none());
    }

    #[test]
    fn a_disabled_handle_hides_frames_and_re_enabling_needs_a_new_one() {
        let (handle, _port, mut thread) = spawn(Arc::new(AtomicU64::new(0)));
        handle.set_enabled(true);
        let frame = SpectrumFrame {
            session_rev: 1,
            bands: vec![(40.0, 85.0)],
            levels: vec![1.0],
            waveform: None,
            at: Nanos(0),
            published_at: Instant::now(),
        };
        let switches = handle.shared.switches.load(Ordering::SeqCst);
        *handle.shared.latest() = Some((switches, frame.clone()));
        assert_eq!(handle.latest(), Some(frame.clone()));
        handle.set_enabled(false);
        assert_eq!(handle.latest(), None);
        // Even a frame left behind under the old tag stays hidden.
        *handle.shared.latest() = Some((switches, frame));
        handle.set_enabled(true);
        assert_eq!(handle.latest(), None);
        thread.stop();
        thread.stop();
    }

    /// §1.1: the waveform request is a cheap level, not an event: a Relaxed
    /// store on `Shared`, and no wake — the runner re-reads it on every
    /// enabled pass (≤10 ms).
    #[test]
    fn set_wants_waveform_sets_and_clears_the_shared_flag() {
        let (handle, _port, mut thread) = spawn(Arc::new(AtomicU64::new(0)));
        assert!(
            !handle.shared.wants_waveform.load(Ordering::Relaxed),
            "nobody has asked yet"
        );
        handle.set_wants_waveform(true);
        assert!(handle.shared.wants_waveform.load(Ordering::Relaxed));
        handle.set_wants_waveform(false);
        assert!(!handle.shared.wants_waveform.load(Ordering::Relaxed));
        thread.stop();
    }
}
