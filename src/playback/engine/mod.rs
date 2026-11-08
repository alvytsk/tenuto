//! The decode worker: the state machine that owns every pipeline component.
//!
//! The invariant this module exists to enforce is that stopping or recreating
//! the audio pipeline never implicitly resets the logical playback position.
//! Position lives here, in `Worker::position`, and only four things establish a
//! new one: loading media, an explicit restart, a successful seek, and a resume
//! that adopts a target stored while stopped. Everything else — pausing,
//! stopping, a device disappearing, a handshake timing out — preserves it.

use std::time::Duration;

use super::handshake::Handshake;
use super::output::Nanos;
use super::spectrum::registry::{TapMapping, TapRegistry};
use super::timeline::{PositionQuality, Timeline};

const STOP: u8 = 1;
const SHUTDOWN: u8 = 2;
const TICK: Duration = Duration::from_millis(10);
/// `pub(crate)` so `wait.rs`'s `WaitService` — the hook's other caller — waits
/// on the exact same deadline as the worker's own handshake calls.
pub(crate) const DEADLINE: Duration = Duration::from_millis(250);
/// Idle wait inside a handshake. The device runs on its own thread, so the
/// worker only has to stop spinning while it waits for an acknowledgment.
/// `pub(crate)`, shared with `wait.rs`, so the hook's `park`/`release` calls
/// pump the wait exactly as the worker's own do.
pub(crate) const PUMP_NAP: Duration = Duration::from_micros(250);

const COMMAND_CAPACITY: usize = 1024;
const SPAN_CAPACITY: usize = 64;
/// How much audio the PCM ring holds. Large enough that one loop iteration
/// cannot drain it, small enough that discarding it on a seek is cheap.
const RING_MILLIS: u64 = 300;
/// Floor on the interval between aggregated diagnostic warnings.
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_millis(500);
/// Refinement budget for an *explicit* seek. Preserving seeks pass `None`.
const SEEK_BUDGET: Duration = Duration::from_secs(5);
/// A refined seek lands on a whole *source* frame, while the position it is
/// asked to preserve is a whole *output* frame, so a preserving seek can come
/// back a fraction of a frame short of what it promised. Within this tolerance
/// the promise is kept, so that repeated stop/resume cycles cannot walk the
/// position backwards a frame at a time. Pause and resume no longer need this -
/// they never re-seek - but stop, resume-at-a-stored-target and device recovery
/// still do.
///
/// One source frame at 44.1 kHz is 22.68 us, and the residues this actually
/// absorbs measure 4.5 us and 13.6 us. 100 us is four of those frames: wide
/// enough for the quantum, and an order of magnitude below the packet-sized
/// miss a genuinely failed seek leaves, so a real miss surfaces rather than
/// being swallowed.
const RESUME_TOLERANCE: Duration = Duration::from_micros(100);

mod handle;
mod worker;

pub use handle::{EngineHandle, EventStream};
use worker::Worker;
use worker::frames_to_duration;

/// The transport state a blocked source read may service, behind one lock.
///
/// `Handshake` and `Timeline` live here rather than on `Worker` because the
/// wait hook needs them and `Worker` is `&mut`-shaped: the hook runs from
/// inside `pump_audio -> source.next_planar()`, which already holds
/// `&mut self.source`, so it cannot also reach the rest of `Worker` and
/// `unsafe_code = "forbid"` rules out a lifetime-erased slot back onto it.
/// Contention is nil: the hook runs only while the worker is blocked inside a
/// decoder read, which is precisely when the worker is not touching any of
/// this (Ruling 1's lock order note lives in `wait.rs`, which is the module
/// that actually has two lock-takers to order).
///
/// `pcm`, `link` and `config` stay off this struct and live as plain `Worker`
/// fields instead: `pcm` is `rtrb::Producer<f32>`, which is `!Sync` and must
/// stay on the worker regardless — the hook must never push audio — and
/// `link`/`config` are not needed by the hook at all (`Handshake` already
/// holds its own clone of `link`), so putting them under this lock would only
/// widen the hook's reach for no benefit and force `pump_audio` to touch this
/// lock for information it does not need (Ruling 3).
pub struct TransportCore {
    handshake: Handshake,
    timeline: Timeline,
    /// Media position the current generation's frame counting starts from.
    anchor: Duration,
    sample_rate: u32,
    /// This transport's spectrum tap, when it has one: what every `Run`
    /// publication must map first (decision 18).
    tap: Option<TransportTap>,
}

/// What a transport needs to publish its tap mappings: the registry, its
/// never-reused instance ID, and the revision and channel count it plays.
pub(crate) struct TransportTap {
    registry: TapRegistry,
    instance: u64,
    session_rev: u64,
    channels: u16,
}

impl TransportCore {
    /// `pub`, unlike the rest of this impl block, so a test can build a real
    /// `TransportCore` and exercise `WaitService::service_as` against it with
    /// no `Worker` in the loop at all — the only way to prove the clock path
    /// `observed_position` depends on actually advances a published position,
    /// which nothing in this crate's `tests/` could otherwise reach.
    pub fn new(
        handshake: Handshake,
        timeline: Timeline,
        anchor: Duration,
        sample_rate: u32,
    ) -> Self {
        Self {
            handshake,
            timeline,
            anchor,
            sample_rate,
            tap: None,
        }
    }

    /// Attaches the spectrum tap's mapping state. Builder-style, so `new`
    /// keeps its public signature.
    pub(crate) fn with_tap(mut self, tap: TransportTap) -> Self {
        self.tap = Some(tap);
        self
    }

    /// Publishes the mapping for the `Run` about to be published for
    /// `generation`, under the epoch that publication will use. Must run
    /// immediately before `start_running` or `release` (decision 18).
    fn publish_tap_mapping(&self, generation: u16) {
        if let Some(tap) = &self.tap {
            tap.registry.publish(TapMapping {
                instance: tap.instance,
                generation,
                epoch: self.handshake.upcoming_epoch(),
                session_rev: tap.session_rev,
                sample_rate: self.sample_rate,
                channels: tap.channels,
            });
        }
    }

    /// Retires every mapping of this transport's tap, for teardown.
    fn retire_tap(&self) {
        if let Some(tap) = &self.tap {
            tap.registry.retire_instance(tap.instance);
        }
    }

    /// Publishes `Run` for `generation`, mapping it first.
    fn start_running(&mut self, generation: u16) {
        self.publish_tap_mapping(generation);
        let Self {
            handshake,
            timeline,
            ..
        } = self;
        handshake.start_running(generation, timeline);
    }

    /// Drain spans and return the position implied by what has actually
    /// played.
    pub(crate) fn observed_position(&mut self, now: Nanos) -> Duration {
        self.handshake.drain_spans(&mut self.timeline);
        let played = self.timeline.played_frames(now);
        self.anchor + frames_to_duration(played, self.sample_rate)
    }

    pub(crate) fn quality(&self) -> PositionQuality {
        self.timeline.quality()
    }

    /// Park the callback, for the hook's freeze arm. Returns whether it was
    /// acknowledged, exactly as `Handshake::park`'s `Result` does, just
    /// flattened to a `bool` since the caller only branches on which.
    pub(crate) fn park(&mut self, pump: &mut dyn FnMut(), deadline: Duration) -> bool {
        self.handshake
            .park(&mut self.timeline, pump, deadline)
            .is_ok()
    }

    /// Release a parked callback: the hook's thaw arm and `Worker::play`'s
    /// resume. The mapping goes out first (decision 18).
    pub(crate) fn release(&mut self) {
        self.publish_tap_mapping(self.handshake.generation());
        self.handshake.release();
    }
}
