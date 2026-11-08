//! The glitch counters behind the worker's aggregated warning.
//!
//! Counters, not a queue: the warning reports running totals, and it goes
//! out only when an ordinary outbox slot is free - a receiver that stays
//! connected without draining therefore bounds diagnostics by the channel
//! itself, not by a rate limit alone. The [`Worker`](super::worker::Worker)
//! keeps the emission orchestration (outbox, session revision); what lives
//! here is the arithmetic those totals are built from.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use super::super::event::PlaybackEvent;
use super::super::link::{Diagnostics, OutputLink};
use super::super::lock;
use super::super::output::cpal_output::OutputFault;
use super::super::state::PlaybackState;
use super::super::wait::Servicing;
use super::worker::{Worker, frames_to_duration};
use super::{DEADLINE, PUMP_NAP, TransportCore};

/// Floor on the interval between aggregated diagnostic warnings.
pub(super) const DIAGNOSTIC_INTERVAL: Duration = Duration::from_millis(500);

/// Underrun, lost-span and report bookkeeping for one worker.
pub(super) struct GlitchCounters {
    pub(super) xruns: u64,
    pub(super) lost_spans: u64,
    /// The total already announced by a warning; a warning is due only when
    /// the running total has moved past it.
    pub(super) reported_total: u64,
    pub(super) last_report: Instant,
}

impl GlitchCounters {
    /// Starts silent: the first report is due as soon as there is something
    /// to report, so the clock is set one interval back.
    pub(super) fn start(now: Instant) -> Self {
        Self {
            xruns: 0,
            lost_spans: 0,
            reported_total: 0,
            last_report: now
                .checked_sub(DIAGNOSTIC_INTERVAL)
                .unwrap_or_else(Instant::now),
        }
    }

    /// Folds one link reading into the counters. Underruns after the decoder
    /// has run dry are the expected sound of a track ending, not a glitch
    /// worth reporting.
    pub(super) fn absorb(&mut self, reading: &Diagnostics, decoder_drained: bool) {
        self.lost_spans += u64::from(reading.spans_dropped);
        if !decoder_drained {
            self.xruns += u64::from(reading.xruns);
        }
    }

    /// The running total this warning would report.
    pub(super) fn total_with(&self, dropped_events: u64, device_warnings: u64) -> u64 {
        self.xruns + self.lost_spans + dropped_events + device_warnings
    }

    /// A warning is due only when the total moved and the floor interval
    /// between warnings has passed.
    pub(super) fn report_is_due(&self, total: u64) -> bool {
        total != self.reported_total && self.last_report.elapsed() >= DIAGNOSTIC_INTERVAL
    }

    /// The message the aggregated warning carries.
    pub(super) fn message(&self, dropped_events: u64, device_warnings: u64) -> String {
        format!(
            "audio glitches: {} underruns, {} lost spans, {} dropped events, {} device warnings",
            self.xruns, self.lost_spans, dropped_events, device_warnings
        )
    }

    /// Commits a warning as sent: the totals it reported are now the ones a
    /// future warning compares against.
    pub(super) fn mark_reported(&mut self, total: u64, now: Instant) {
        self.reported_total = total;
        self.last_report = now;
    }
}

// ---------------------------------------------------------------- reporting

impl Worker {
    pub(super) fn teardown(&mut self) -> Option<Arc<OutputLink>> {
        // Close first: dropping the stream joins the backend thread, which is
        // what makes the link's rescue slot safe to read afterwards.
        self.output.close();
        retire_faults(&mut self.deferred_fault, &self.faults);
        let link = self.link.take();
        // A load, stop, recovery or shutdown invalidates every mapping of the
        // old transport, so its spectrum disappears without new audio.
        if let Some(core) = lock(&self.transport).take() {
            core.retire_tap();
        }
        self.pcm = None;
        self.config = None;
        self.converter = None;
        self.staging.clear();
        self.pushed_total = 0;
        self.source_eof = false;
        self.converter_flushed = false;
        self.decoder_drained = false;
        link
    }

    pub(super) fn capture_and_teardown(&mut self) {
        let rate = self
            .config
            .as_ref()
            .map(|c| c.sample_rate)
            .unwrap_or(48_000);
        let exact = self.capture_position();
        // Read before `teardown` clears the `TransportCore` this comes from.
        // Unused when `exact` is true (the common case) or when there was
        // never a transport to begin with - the rescue branch below is the
        // only place it matters, and it is reachable only when both a
        // transport existed and its capture timed out.
        let anchor = lock(&self.transport)
            .as_ref()
            .map(|core| core.anchor)
            .unwrap_or(self.position);
        let generation = self.generation;
        let link = self.teardown();
        if exact {
            return;
        }
        // The capture timed out. The callback's last unpublished span survives
        // in the link, so read it once the teardown has joined the backend
        // thread; if it is unusable, keep the last validated position rather
        // than fabricating one.
        self.degraded = true;
        if let Some(link) = link
            && let Some(record) = link.take_rescue_after_teardown()
            && record.generation == generation as u16
        {
            self.position = anchor + frames_to_duration(record.media_total_after, rate);
        }
    }

    /// Freeze the callback and read back the frames it really played.
    ///
    /// Returns `false` when the device did not answer, in which case the
    /// caller must fall back to the link's rescue record.
    pub(super) fn capture_position(&mut self) -> bool {
        let Some(rate) = self.config.as_ref().map(|c| c.sample_rate) else {
            return true;
        };
        let captured = {
            let Self {
                transport, output, ..
            } = self;
            let mut guard = lock(transport);
            let Some(core) = guard.as_mut() else {
                return true;
            };
            let anchor = core.anchor;
            let TransportCore {
                handshake,
                timeline,
                ..
            } = core;
            let mut clock = || output.now();
            let mut pump = || std::thread::sleep(PUMP_NAP);
            handshake
                .freeze_and_capture(timeline, &mut clock, &mut pump, DEADLINE)
                .map(|frames| anchor + frames_to_duration(frames, rate))
        };
        match captured {
            Ok(position) => {
                self.position = position;
                self.settle_heard();
                true
            }
            Err(_) => false,
        }
    }

    pub(super) fn service_faults(&mut self) {
        let mut worst: Option<OutputFault> = self.deferred_fault.take();
        let mut seen = 0u64;
        // Collapse the queue to its most severe entry: acting on each one in
        // turn would let a burst of faults generate a burst of events.
        while let Ok(fault) = self.faults.try_recv() {
            seen += 1;
            if worst.is_none_or(|current| severity(fault) > severity(current)) {
                worst = Some(fault);
            }
        }
        let Some(fault) = worst else {
            return;
        };
        // Command admission already stops the loop creating events faster than
        // they drain. Faults arrive asynchronously and answer to no such gate,
        // so apply the same rule here: hold the fault until its events fit,
        // rather than acting now and dropping the report at the cap.
        if !self.outbox.fits(Self::FAULT_EVENT_BUDGET) {
            self.deferred_fault = Some(fault);
            self.device_warnings += seen;
            return;
        }
        self.device_warnings += seen;
        match fault {
            // Rerouted or refused realtime: playback continues, but the timing
            // base underneath the timeline may have jumped.
            OutputFault::Recoverable(
                cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied,
            ) => self.degraded = true,
            // An underrun counts no frames: the timeline freezes at what was
            // really heard, which stays exact, so a benign `Xrun` must not
            // latch degraded quality onto the whole generation. The warning
            // still counts through `device_warnings`.
            OutputFault::Recoverable(cpal::ErrorKind::Xrun) => {}
            // Any future recoverable kind (`ErrorKind` is non-exhaustive)
            // degrades: without host-specific knowledge it may well have
            // disturbed the timing base, and position honesty wins.
            OutputFault::Recoverable(_) => self.degraded = true,
            OutputFault::Rebuild(kind) => {
                let resume = self.state == PlaybackState::Playing;
                // Nothing to report here: `rebuild` has already emitted
                // whatever the outcome warrants.
                let _ = self.rebuild(&format!("{kind:?}"), resume);
            }
            OutputFault::Fatal(kind) => self.fail(format!("audio device failed: {kind:?}")),
        }
    }

    /// Read the link's counters before anything else can swap them away, and
    /// forward the span losses to the timeline that has to account for them.
    ///
    /// Runs before `publish_progress`: `Handshake::drain_spans` (invoked
    /// inside `TransportCore::observed_position`, which `publish_progress`
    /// reaches through `service_as`) reads the same link and forwards
    /// `spans_dropped` into the timeline itself, so reading it here first is
    /// what makes that second read come back zero and leaves the timeline as
    /// the one place span losses actually get counted.
    pub(super) fn collect_diagnostics(&mut self) {
        let Some(link) = self.link.as_ref() else {
            return;
        };
        let diagnostics = link.take_diagnostics();
        {
            let mut guard = lock(&self.transport);
            if let Some(core) = guard.as_mut() {
                core.timeline.note_dropped(diagnostics.spans_dropped);
            }
        }
        self.glitches.absorb(&diagnostics, self.decoder_drained);
    }

    /// Mirror the worker's own truth into `SessionFacts`, then run the same
    /// `service` the hook does (Ruling 4) — which is what actually recomputes
    /// the live position from the transport while playing, publishes the
    /// `Progress` snapshot, and services the freeze level. One implementation,
    /// two callers.
    ///
    /// Two writers keep `device_clock` current, for two different reasons.
    /// `CallbackCore::fill` writes it on every callback invocation, including
    /// while parked or frozen, which is what keeps the recompute below seeing
    /// the clock advance while the worker itself is stuck inside a blocked
    /// decoder read and this method is not running at all — Cause A this
    /// exists to fix. This method *also* writes it, immediately before
    /// `service_as`, because it is the only writer that can be exactly as
    /// fresh as `self.output.now()` at the instant this recompute actually
    /// runs: `CallbackCore::fill` only fires once per output buffer period
    /// (2 ms with this crate's own `TestOutput` harness, and it is what
    /// `capture_position`'s protocol round-trip is compared against in
    /// `tests/engine_contract.rs`), so relying on it alone during an ordinary
    /// loop pass would leave the published position up to one buffer period
    /// behind a freeze-captured one taken moments later — which is exactly
    /// the discrepancy that broke `stop_preserves_the_logical_position` and
    /// its siblings the first time this was tried. Reading `self.output.now()`
    /// unconditionally is safe even while idle: it can return `Nanos(0)`
    /// (`CpalOutput::now()`'s `None` arm) when no stream is open, but nothing
    /// ever reads `device_clock` unless `facts.playing` is true, and that is
    /// only ever true when a transport - and so a stream - exists.
    ///
    /// `fetch_max`, not `store`, for the same reason `CallbackCore::fill`
    /// uses it: this write and the callback's race on two different threads
    /// with no ordering between them, so a plain `store` could let a
    /// just-sampled-but-not-yet-stored older instant from one thread
    /// overwrite a newer value the other already published, and the reader
    /// would see position step backward. `fetch_max` only ever moves this
    /// forward, which is correct for both writers *within one clock
    /// domain* — the domain reset that must still win with a plain `store`
    /// lives in `open_transport`, not here.
    pub(super) fn publish_progress(&mut self) {
        self.device_clock
            .fetch_max(self.output.now().0, Ordering::Relaxed);
        self.sync_facts();
        self.service.service_as(Servicing::WorkerLoop);
        // Read back whatever the shared recompute settled on, so
        // `self.position` - the field every command (`SeekBy` among them)
        // reads as "the current position" - stays live while playing, exactly
        // as this method used to keep it before the recompute moved into
        // `WaitService` so the hook could share it.
        self.position = lock(&self.facts).position;
    }

    /// Copy the fields `SessionFacts` mirror from the worker. Runs on every
    /// `publish_progress` and before every `emit` (outbox spec §4.3): a read
    /// that blocks later in the same command flushes the event from the
    /// hook, and the progress the hook publishes first must not be older
    /// than that event. `frozen_by_hook` is the hook's own and stays as it
    /// is.
    pub(super) fn sync_facts(&self) {
        let mut facts = lock(&self.facts);
        facts.session_rev = self.session_rev;
        facts.media = self.media.clone();
        facts.position = self.position;
        facts.degraded = self.degraded;
        // Mirrored, never derived from `degraded` or `playing` (§3.1's
        // stickiness): playing on from an estimated landing must keep
        // reporting it estimated for as long as the landing stands.
        facts.provenance = self.position_provenance;
        // Paused counts as well as Playing: parking silences the
        // callback, but the frames it already handed to the device still
        // play out, so the position goes on rising for one output
        // latency after the park and only then settles. Freezing the
        // number at the instant of the park would report a position
        // slightly behind what the listener actually heard.
        // `Reconnecting` too (M7 §7): the transport is left running so
        // the ring plays out after a disconnect, and that audio is heard,
        // so it must be counted.
        facts.playing = matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Reconnecting
        );
        facts.load = self.adopted_load;
    }

    pub(super) fn warn(&mut self, message: String) {
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::Warning {
            session_rev,
            message,
        });
    }

    /// Counters, not a queue: an aggregated warning goes out only when an
    /// ordinary slot is free and nothing is pending (`Outbox::offer`), and
    /// it reports the running totals. A receiver that stays connected
    /// without draining therefore bounds diagnostics by the channel itself,
    /// not by a rate limit alone. A refused offer leaves the totals
    /// unreported, so the next interval offers them again.
    pub(super) fn emit_aggregated_warning_if_slot_free(&mut self) {
        let dropped = self.outbox.dropped();
        let total = self.glitches.total_with(dropped, self.device_warnings);
        if !self.glitches.report_is_due(total) {
            return;
        }
        let message = self.glitches.message(dropped, self.device_warnings);
        let event = PlaybackEvent::Warning {
            session_rev: self.session_rev,
            message,
        };
        if self.outbox.offer(event) {
            self.glitches.mark_reported(total, Instant::now());
        }
        self.receivers_gone |= self.outbox.disconnected();
    }

    pub(super) fn flush_events(&mut self) {
        self.outbox.flush();
        self.receivers_gone |= self.outbox.disconnected();
    }

    pub(super) fn emit(&mut self, event: PlaybackEvent) {
        // Facts first (outbox spec §4.3): the hook may flush this event from
        // a read that blocks before the next loop pass.
        self.sync_facts();
        self.outbox.emit(event);
    }
}

fn severity(fault: OutputFault) -> u8 {
    match fault {
        // An underrun counts no frames and disturbs no timing base; every
        // other recoverable kind may have. When both are queued in one pass,
        // the timing-affecting fault must survive the collapse - an earlier
        // `Xrun` must not mask a `DeviceChanged` whose degradation the
        // position honesty depends on.
        OutputFault::Recoverable(cpal::ErrorKind::Xrun) => 0,
        OutputFault::Recoverable(_) => 1,
        OutputFault::Rebuild(_) => 2,
        OutputFault::Fatal(_) => 3,
    }
}

/// Retire every fault belonging to a transport being torn down.
///
/// A fault describes the transport that produced it, so carrying one past a
/// teardown lets an obsolete fatal fault fire later and turn a completed `Stop`
/// into `Failed`. Both sources have to go: the one held back for want of event
/// room, and anything still queued in the channel — an interrupt arriving
/// before the queue was drained leaves the fault there rather than deferred.
pub(super) fn retire_faults(deferred: &mut Option<OutputFault>, faults: &Receiver<OutputFault>) {
    *deferred = None;
    while faults.try_recv().is_ok() {}
}
