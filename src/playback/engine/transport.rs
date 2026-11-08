//! The transport state a blocked source read may service, behind one lock.

use std::time::Duration;

use super::super::handshake::Handshake;
use super::super::output::Nanos;
use super::super::spectrum::registry::{TapMapping, TapRegistry};
use super::super::timeline::{PositionQuality, Timeline};
use super::worker::frames_to_duration;

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
    pub(super) handshake: Handshake,
    pub(super) timeline: Timeline,
    /// Media position the current generation's frame counting starts from.
    pub(super) anchor: Duration,
    pub(super) sample_rate: u32,
    /// This transport's spectrum tap, when it has one: what every `Run`
    /// publication must map first (decision 18).
    pub(super) tap: Option<TransportTap>,
}

/// What a transport needs to publish its tap mappings: the registry, its
/// never-reused instance ID, and the revision and channel count it plays.
pub(crate) struct TransportTap {
    pub(super) registry: TapRegistry,
    pub(super) instance: u64,
    pub(super) session_rev: u64,
    pub(super) channels: u16,
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
    pub(super) fn retire_tap(&self) {
        if let Some(tap) = &self.tap {
            tap.registry.retire_instance(tap.instance);
        }
    }

    /// Publishes `Run` for `generation`, mapping it first.
    pub(super) fn start_running(&mut self, generation: u16) {
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
