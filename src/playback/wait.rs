//! What a blocked source read services on the worker's behalf (G5, §8, §9).
//!
//! `TransportCore` (in `engine.rs`) is the only mutable state a blocked read
//! can reach: `Handshake` and `Timeline` live there, behind one lock, because
//! `pump_audio -> source.next_planar()` already holds `&mut self.source` when
//! the read blocks, and `unsafe_code = "forbid"` rules out a lifetime-erased
//! slot back onto the rest of `Worker`. `WaitService` is everything else the
//! hook needs, and nothing more: it holds no decoder and no source, so §8's
//! "must not re-enter decoder reads or seeks" is a property of the type
//! rather than a rule in prose.
//!
//! **Lock order, stated once and never varied: `SessionFacts` before
//! `TransportCore`.** `SourceInterrupt::state` is a leaf and is never held
//! while either is taken — `ByteChannel::read` already drops it across the
//! hook call for exactly this reason. Nothing in this module takes
//! `TransportCore` before `SessionFacts`.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::http::channel::{SourceInterrupt, WaitHook};
use crate::media::id::MediaId;

use super::command::LoadRequestId;
use super::engine::{DEADLINE, PUMP_NAP, TransportCore};
use super::event::{PlaybackEvent, Progress};
use super::outbox::Outbox;
use super::output::Nanos;
use super::state::PlaybackState;
use super::timeline::PositionQuality;
use crate::media::provenance::PositionProvenance;

/// A poisoned lock means a thread already panicked while holding it; there is
/// nothing better to do than carry on with the state it left. Same pattern as
/// `http::channel::lock` and `playback::prepare::lock`.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Distinguishes why `service_as` is running: only a `BlockedRead` reports
/// `buffering` (Ruling 4) and acts on the freeze level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Servicing {
    /// `WaitHook::service`, called from inside a blocked decoder read.
    BlockedRead,
    /// `Worker::publish_progress`, called once per pass through the main loop.
    WorkerLoop,
}

/// The scalars `service()` needs that change with the session rather than
/// with the transport.
///
/// Deliberately does not carry `quality`: that is derived fresh on every
/// publish from `degraded`, `playing` and (while playing) the transport's own
/// `Timeline::quality`, never stored.
pub struct SessionFacts {
    pub session_rev: u64,
    pub media: Option<MediaId>,
    pub position: Duration,
    pub degraded: bool,
    pub playing: bool,
    /// Whether `position` is decoder-established or a byte-offset estimate
    /// (§3). Mirrored from the worker's own truth exactly like the fields
    /// above; `publish_progress` copies it straight into `Progress`, never
    /// derives it from `degraded` or `playing`.
    pub provenance: PositionProvenance,
    /// Set by the hook when it parks for a freeze, cleared when it releases.
    /// The worker reads it to know the transport is parked without having
    /// dispatched the `Pause` itself.
    pub frozen_by_hook: bool,
    /// Mirrored from `Worker::adopted_load` on every `publish_progress` pass
    /// (M5 §6, Decision 2); copied straight into `Progress::load` below,
    /// never derived from anything else here.
    pub load: Option<LoadRequestId>,
}

/// What a blocked source read services on the worker's behalf.
///
/// Four jobs, and no others: drain spans into the timeline, publish the
/// keep-latest progress snapshot, act on the freeze level — park the output
/// when a pause arrives, release it when a play does, and announce each —
/// and, while the worker is stuck in the read, deliver what the outbox
/// holds.
pub struct WaitService {
    transport: Arc<Mutex<Option<TransportCore>>>,
    progress: Arc<Mutex<Progress>>,
    facts: Arc<Mutex<SessionFacts>>,
    interrupt: Arc<SourceInterrupt>,
    /// The engine's one event queue, shared with the worker.
    outbox: Arc<Outbox>,
    /// `AudioOutput::now()` needs `&self.output`, which the hook cannot
    /// reach — it runs from inside `next_planar()`, deep under
    /// `&mut self.source`, structurally unable to borrow the rest of
    /// `Worker`. The caller supplies a `Send + Sync` stand-in instead. In
    /// production this reads `Worker.device_clock`, which
    /// `CallbackCore::fill` keeps live on the audio backend thread — the one
    /// context still running while the decode thread this module's own
    /// caller may be blocked on is stuck — so it keeps advancing for the
    /// full length of a blocked read, not just once per worker loop pass.
    clock: Arc<dyn Fn() -> Nanos + Send + Sync>,
}

impl WaitService {
    pub fn new(
        transport: Arc<Mutex<Option<TransportCore>>>,
        progress: Arc<Mutex<Progress>>,
        facts: Arc<Mutex<SessionFacts>>,
        interrupt: Arc<SourceInterrupt>,
        outbox: Arc<Outbox>,
        clock: Arc<dyn Fn() -> Nanos + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            transport,
            progress,
            facts,
            interrupt,
            outbox,
            clock,
        })
    }

    /// Park the callback for a freeze. Returns whether the park is considered
    /// acknowledged: trivially `true` with no transport open, mirroring
    /// `Worker::pause`'s own `None => Ok(())` — there is nothing to park, so
    /// there is nothing to fail.
    ///
    /// A park that times out leaves `frozen_by_hook` false (the caller checks
    /// this return value before setting it), so the worker's own `pause()`
    /// handles the recovery once the blocked read finally returns.
    fn park(&self) -> bool {
        let mut guard = lock(&self.transport);
        match guard.as_mut() {
            Some(core) => {
                let mut pump = || std::thread::sleep(PUMP_NAP);
                core.park(&mut pump, DEADLINE)
            }
            None => true,
        }
    }

    /// Release a parked callback. A no-op with no transport open — there is
    /// nothing to release, and nothing downstream treats that as a failure.
    ///
    /// Goes through `TransportCore::release`, which publishes the spectrum
    /// tap's mapping for the thaw's `Run` before releasing (decision 18).
    fn release(&self) {
        let mut guard = lock(&self.transport);
        if let Some(core) = guard.as_mut() {
            core.release();
        }
    }

    /// Queue one event behind everything the worker emitted before it. The
    /// slice's own flush, at the end of `service_as`, delivers it.
    fn announce(&self, event: PlaybackEvent) {
        self.outbox.emit(event);
    }

    /// Update the keep-latest `Progress` snapshot from `SessionFacts`,
    /// recomputing the live position (and its quality) from the transport
    /// while playing — the same guard `Worker::publish_progress` used to
    /// apply inline, before this recompute moved here so the hook could
    /// share it.
    ///
    /// Paused counts as well as Playing: parking silences the callback, but
    /// the frames it already handed to the device still play out, so the
    /// position goes on rising for one output latency after the park and
    /// only then settles. Freezing the number at the instant of the park
    /// would report a position slightly behind what the listener actually
    /// heard.
    fn publish_progress(&self, servicing: Servicing) {
        let snapshot = {
            let mut facts = lock(&self.facts);
            let mut quality = if facts.degraded {
                PositionQuality::Degraded
            } else if facts.playing {
                PositionQuality::Estimated
            } else {
                PositionQuality::Exact
            };
            // Gating the recompute — and so the span-queue drain inside
            // `observed_position` — on `facts.playing` narrows what used to
            // run whenever a transport existed at all. It is safe today
            // because `facts.playing` is `matches!(state, Playing | Paused)`,
            // and those are the only states where the callback publishes NEW
            // spans (`CallbackCore::fill`'s `Run` phase). The remaining case —
            // `Ended`, where a transport can still be `Some` but parked — has
            // nothing to lose: `check_end_of_track` calls
            // `TransportCore::park`, which drains every already-queued span
            // as its own last step before returning, and a parked callback
            // publishes none after that. If a transport is ever left running
            // unparked outside Playing/Paused, this stops being true and
            // needs revisiting.
            //
            // M7: `Reconnecting` keeps the transport running unparked so the
            // ring plays out, and is included in `facts.playing` for exactly
            // the reason this comment warns about — audio heard after a
            // disconnect must be counted. The spans it publishes are the
            // already-pushed frames draining; once the ring is dry the
            // callback emits silence, which publishes no span at all, so the
            // position stands still rather than running on.
            if facts.playing {
                let now = (self.clock)();
                let mut transport = lock(&self.transport);
                if let Some(core) = transport.as_mut() {
                    facts.position = core.observed_position(now);
                    if !facts.degraded {
                        quality = core.quality();
                    }
                }
            }
            Progress {
                session_rev: facts.session_rev,
                media: facts.media.clone(),
                position: facts.position,
                quality,
                // Read straight from facts, never derived: provenance is an
                // orthogonal axis to quality, and this recompute must not
                // touch it either way (§3.1's stickiness).
                provenance: facts.provenance,
                // Ruling 4: true exactly for the caller that exists *because*
                // a source read is blocked - never derived from `quality`,
                // which reports an unrelated fact (a timing base that jumped).
                buffering: servicing == Servicing::BlockedRead,
                load: facts.load,
            }
        };
        // Keep-latest: nothing but the assignment happens under the lock.
        match self.progress.lock() {
            Ok(mut slot) => *slot = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    /// One implementation, two callers (Ruling 4): `WaitHook::service`
    /// delegates here with `Servicing::BlockedRead`, and
    /// `Worker::publish_progress` calls this directly with
    /// `Servicing::WorkerLoop` — which is what makes "the hook does exactly
    /// what the loop does" a fact rather than a comment for the progress
    /// snapshot.
    ///
    /// The freeze arm is the exception: it runs for a blocked read only. The
    /// level exists to reach a read the worker is stuck in (G2); a worker that
    /// is not stuck dispatches the `Pause` or `Play` queued beside the level
    /// itself. Acting on the level from the loop as well raced that dispatch:
    /// `submit_play` sends `Play` before it lowers the level, so a `Play`
    /// dispatched in between had its transport parked again by the next pass,
    /// with a stray `Paused`.
    pub(crate) fn service_as(&self, servicing: Servicing) {
        tracing::trace!(?servicing, "servicing the transport");
        let blocked = servicing == Servicing::BlockedRead;
        let frozen = self.interrupt.is_frozen();
        let mut facts = lock(&self.facts);
        if blocked && frozen && !facts.frozen_by_hook {
            // Park the callback. Everything else — the ring, the decoder, the
            // pending read — is left exactly as it is, which is what makes
            // resuming a single release (§9).
            if self.park() {
                facts.frozen_by_hook = true;
                // `facts.playing` is deliberately left untouched: it tracks
                // "the transport is in a Playing-or-Paused generation",
                // exactly as `Worker::pause()` leaves `self.state` at
                // `Paused` rather than something the recompute below would
                // skip. Clearing it here would report a position frozen at
                // the instant of the park, which is precisely what
                // `publish_progress`'s own doc comment says the recompute
                // exists to avoid — and it would report `Exact` quality over
                // a device that is still draining buffered frames.
                self.announce(PlaybackEvent::StateChanged {
                    session_rev: facts.session_rev,
                    state: PlaybackState::Paused,
                    request: None,
                });
            }
        } else if blocked && !frozen && facts.frozen_by_hook {
            self.release();
            facts.frozen_by_hook = false;
            self.announce(PlaybackEvent::StateChanged {
                session_rev: facts.session_rev,
                state: PlaybackState::Playing,
                request: None,
            });
        }
        // Ruling 2: dropped before `publish_progress`, which takes this same
        // lock first thing. `std::sync::Mutex` is not reentrant, so holding
        // this across the call deadlocks on the very first pass.
        drop(facts);
        self.publish_progress(servicing);
        // Progress first, then the events: whatever goes out from here was
        // emitted no later than the facts that progress was built from
        // (outbox spec §4.3). A worker pass flushes at its own step 4.
        if blocked {
            self.outbox.flush();
        }
    }
}

impl WaitHook for WaitService {
    fn service(&self) {
        self.service_as(Servicing::BlockedRead);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An inert `WaitService`: no transport, nothing queued, and a receiver
    /// nobody holds. Enough to exercise `service_as` without a real decoder
    /// or a real network read anywhere behind it.
    fn inert_service() -> Arc<WaitService> {
        let (outbox, _events) = crate::playback::outbox::channel();
        WaitService::new(
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(Progress {
                session_rev: 1,
                media: None,
                position: Duration::ZERO,
                quality: PositionQuality::Exact,
                provenance: PositionProvenance::Established,
                buffering: false,
                load: None,
            })),
            Arc::new(Mutex::new(SessionFacts {
                session_rev: 1,
                media: None,
                position: Duration::ZERO,
                degraded: false,
                playing: false,
                provenance: PositionProvenance::Established,
                frozen_by_hook: false,
                load: None,
            })),
            SourceInterrupt::new(1024),
            outbox,
            Arc::new(|| Nanos(0)),
        )
    }

    fn published_buffering(service: &WaitService) -> bool {
        match service.progress.lock() {
            Ok(guard) => guard.buffering,
            Err(poisoned) => poisoned.into_inner().buffering,
        }
    }

    /// Ruling 4: `buffering` is a fact about which caller is running, not
    /// something derived from the transport or the timing base. The worker's
    /// own loop pass is never "buffering", even with nothing open to be
    /// buffering on.
    #[test]
    fn the_worker_loops_own_pass_never_reports_buffering() {
        let service = inert_service();
        service.service_as(Servicing::WorkerLoop);
        assert!(!published_buffering(&service));
    }

    /// The hook exists *because* a source read is blocked on the network -
    /// that is the one fact this field reports, and `WaitHook::service`
    /// always runs as `BlockedRead` (never `WorkerLoop`).
    #[test]
    fn a_blocked_read_is_reported_as_buffering() {
        let service = inert_service();
        service.service();
        assert!(published_buffering(&service));
    }
}
