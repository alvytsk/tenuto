//! The reconnect sub-machine: how a remote failure becomes a bounded or
//! open-ended recovery, and how the worker services that recovery while
//! staying responsive to commands.
//!
//! These three methods are the whole of it. Everything they reach for - the
//! transport teardown, the landing, the announcements - stays on
//! [`Worker`](super::worker::Worker) and its module; what lives here is only
//! the outage-driven decision loop that wraps those primitives (M7, M10).

use std::sync::atomic::Ordering;

use super::super::error::PlaybackError;
use super::super::lock;
use super::super::reconnect::{Next, Outage, retryable};
use super::super::state::PlaybackState;
use super::worker::{Approach, Landed, PendingResume, Worker, is_cancelled};
use crate::http::error::RemoteFailure;
use crate::http::source::remote_cause;

impl Worker {
    /// M7 §7, M10 §3. A playing connection, or an attempt, failed retryably.
    ///
    /// The outage is the thing that spans attempts: it is begun by the first
    /// such failure and outlives every attempt until sustained playback ends
    /// it, the budget gives up, or the listener ends the request.
    pub(super) fn enter_reconnecting(&mut self, failure: RemoteFailure) {
        let now = self.source_interrupt.now();
        let policy = *lock(&self.reconnect_policy);
        let outage = self.outage.get_or_insert_with(|| Outage::begin(now));
        match outage.failed(now, &policy) {
            Next::GiveUp => self.fail_with(format!("{failure}"), Some(failure)),
            Next::AttemptAt(_) => {
                if self.is_indefinite() {
                    tracing::info!(reason = %failure, "live source lost; reconnecting");
                } else {
                    let position = self.position;
                    tracing::info!(reason = %failure, ?position, "connection lost; reconnecting");
                    self.traits.recovering.store(true, Ordering::Release);
                }
                // The output transport stays up so the ring plays out; only
                // the source goes.
                self.source_interrupt.retire();
                self.retire_remote_source();
                self.set_state(PlaybackState::Reconnecting);
            }
        }
    }

    /// Run from the loop, never from a sleep: commands stay serviced through
    /// every backoff, so a pause, a stop, a replacing load or a shutdown ends
    /// the attempts at once rather than after the current one.
    pub(super) fn service_reconnect(&mut self) {
        match self.state {
            PlaybackState::Playing => {
                self.settle_heard();
                let policy = *lock(&self.reconnect_policy);
                if self
                    .outage
                    .as_ref()
                    .is_some_and(|outage| outage.is_over(&policy))
                {
                    self.outage = None;
                }
            }
            PlaybackState::Reconnecting => {
                // M7 §6: a cancellation already in hand ends recovery *now*.
                // An attempt begun here blocks this whole pass inside an open
                // that may last `limits.headers`, or inside a priming read a
                // silent station never answers; the stop, pause or replacing
                // load that cancels it is only looked at when the loop comes
                // back round, so recovery would outlive the explicit Play it
                // belongs to by a whole attempt. The interrupt word covers a
                // stop or a shutdown raised since step 1 consumed the flags
                // (the loop's `select!` is free to answer the wake that
                // carried one and leave the flag standing for the next pass);
                // a queued command covers a pause or a replacing load, which
                // travel on the command channel and cannot be peeked at. Any
                // queued command is reason enough to come back for it: the
                // loop takes one per pass, so an attempt is postponed by a
                // pass, never dropped. Commands are only taken while nothing
                // is backed up, so a blocked outbox must not be allowed to
                // stall recovery indefinitely - hence the backlog test.
                let command_waiting = self.outbox.is_empty() && !self.commands.is_empty();
                if self.interrupted() || command_waiting {
                    return;
                }
                if !self
                    .outage
                    .as_ref()
                    .is_some_and(|outage| outage.due(self.source_interrupt.now()))
                {
                    return;
                }
                let attempt = if self.is_indefinite() {
                    self.fresh_open()
                } else {
                    self.reconnect_finite()
                };
                match attempt {
                    Ok(()) => {
                        if let Some(outage) = self.outage.as_mut() {
                            outage.playing_from();
                        }
                    }
                    // The command that cancelled it decides what happens next.
                    Err(error) if is_cancelled(&error) => {}
                    Err(error) => {
                        let failure = match error {
                            PlaybackError::Remote(failure) => failure,
                            other => match remote_cause(&other) {
                                Some(failure) => failure,
                                // Not the server's fault: a device that will
                                // not open, or any other error with no remote
                                // cause, is never retried against a remote
                                // budget.
                                None => {
                                    self.fail_from(other);
                                    return;
                                }
                            },
                        };
                        // The attempt may have torn the old transport down
                        // and announced nothing; stay in Reconnecting —
                        // quietly, which for this no-change re-assert is
                        // exactly what `adopt_state` does.
                        self.adopt_state(PlaybackState::Reconnecting);
                        if retryable(&failure, self.capabilities.continuity) {
                            self.enter_reconnecting(failure);
                        } else {
                            self.fail_with(format!("{failure}"), Some(failure));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// M10 §5: one recovery attempt for a finite source.
    ///
    /// `Ok` means `Playing` was announced and the pending intent, if any, was
    /// reported. On `Err` nothing of the attempt remains: no transport, no
    /// decoder, `pending` untouched and the position as it was captured.
    pub(super) fn reconnect_finite(&mut self) -> Result<(), PlaybackError> {
        // 1. Capture and tear down before any network I/O, so the ring cannot
        //    move once the position is read. A no-op after the first attempt,
        //    which already left no transport.
        self.capture_and_teardown();
        let previous = (self.position, self.position_provenance);
        // 2.
        let target = self.pending.map_or(self.position, PendingResume::target);
        // 3 + 4. `ensure_source_open` sets `expected = Finite`, so a reopen
        //    that answers as a station fails with `ResourceChanged`.
        // 5. `land` adopts the landing as the anchor before the transport
        //    exists: `open_transport` copies `self.position` into the new
        //    `TransportCore`, so a landing assigned afterwards would play from
        //    the target while progress and every later capture counted from
        //    the old position.
        let landed = match self.ensure_source_open() {
            Ok(_) => self.land(target, Approach::Preserve),
            Err(error) => Landed::Failed(error),
        };
        let landing = match landed {
            Landed::At(landing) => landing,
            Landed::Cancelled => {
                self.abandon_attempt();
                return Err(PlaybackError::Cancelled);
            }
            Landed::Failed(error) => {
                self.abandon_attempt();
                return Err(error);
            }
        };
        // 6 + 7.
        if let Err(error) = self.prime_attempt() {
            (self.position, self.position_provenance) = previous;
            return Err(error);
        }
        // 8.
        self.start_running();
        self.announce_playing();
        self.complete_pending(landing);
        Ok(())
    }
}
