//! The engine's outgoing events: one ordered queue and one delivery policy,
//! shared by the decode worker and the wait hook (M9.4).
//!
//! The hook runs on the worker's own thread, inside a decoder read that
//! blocked on the network, so the two never emit at the same time. The lock
//! exists because the hook cannot borrow `Worker` from inside that read, not
//! because anything contends for it. It is a leaf: no method here calls out
//! while holding it and no guard leaves this module, so it is never held
//! across a decoder read or a hook call.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam_channel::{Receiver, Sender, TrySendError};

use super::event::PlaybackEvent;

const EVENT_CAPACITY: usize = 64;
/// Ordinary events may not occupy these; terminal and protected ones may.
const RESERVED_EVENT_SLOTS: usize = 9;
const PENDING_CAP: usize = 128;

// Reserve budget. Terminal or protected outcomes may occupy the reserved
// tail; ordinary events may not. The worst case is one loop iteration
// emitting, at most:
//
//   stop interrupt        1  StateChanged{Stopped}
//   a serviced fault      2  Failed + StateChanged, or DeviceRecovered + StateChanged
//   a dispatched command  5  Load is the widest: StateChanged{Loading},
//                            CapabilitiesChanged, Loaded, then either
//                            StateChanged{Paused} (`open_transport` succeeds)
//                            or Failed + StateChanged{Failed} (it does not) -
//                            the failure tail is one event wider than the
//                            success one, so 5 is this row's true worst case.
//   end of track          2  EndOfTrack + StateChanged{Ended}
//                        --
//                        10 (naive union)
//
// 10 looks like it breaks `RESERVED_EVENT_SLOTS == 9`, but the reserve only
// has to be as large as the TERMINAL-OR-PROTECTED share of that union: an
// ordinary event that cannot flush is merely held in the queue (bounded
// separately, by `PENDING_CAP`) until the reserve clears - never lost, and
// so never in need of reservation. Only a terminal or protected outcome,
// which the reserved tail exists to guarantee delivery for even under a full
// ordinary backlog, must actually fit (Decision 4, Decision 5). Row by row:
//
//   stop interrupt        1  StateChanged{Stopped} (terminal)
//   a fatal fault          2  Failed + StateChanged{Failed} (both terminal)
//   a dispatched load      3  Loaded (protected, M5 §6) + Failed +
//                             StateChanged{Failed} (both terminal)
//   end of track           2  EndOfTrack + StateChanged{Ended} (both terminal)
//                        --
//                         8 <= RESERVED_EVENT_SLOTS (9)
//
// A cancelled load - a stop or shutdown landing during the open - emits only
// `LoadCancelled` (1, protected), well under the dispatched-load row's own
// share, so it never raises the bound. These four rows are still not
// mutually exclusive in a single pass, so the union is the bound rather than
// the maximum of them; command admission closes while a backlog exists, and
// `service_faults` defers a fault whose events would not fit, so neither
// source can outrun the drain.
//
// `RestartEstablished` and `SeekCancelled` both belong to commands narrower
// than `Load` (their own event plus a `StateChanged`, at most 2, and neither
// terminal), so neither raises the bound. The resume-unavailable warning does
// not add a sixth to the dispatched-command row either: it rides on
// `Loaded.disposition` rather than an event of its own. The wait hook's
// `Paused`/`Playing` announcements are ordinary and need no reserve.

/// The engine's event channel: the receiver for the application, the outbox
/// for the worker and the wait hook.
pub fn channel() -> (Arc<Outbox>, Receiver<PlaybackEvent>) {
    let (sender, receiver) = crossbeam_channel::bounded(EVENT_CAPACITY);
    let outbox = Outbox {
        inner: Mutex::new(Inner {
            sender,
            pending: VecDeque::new(),
            dropped: 0,
            disconnected: false,
        }),
    };
    (Arc::new(outbox), receiver)
}

/// Every event the engine sends, and the rules for when it goes.
pub struct Outbox {
    inner: Mutex<Inner>,
}

struct Inner {
    sender: Sender<PlaybackEvent>,
    pending: VecDeque<PlaybackEvent>,
    dropped: u64,
    disconnected: bool,
}

impl Inner {
    fn free_slots(&self) -> usize {
        EVENT_CAPACITY.saturating_sub(self.sender.len())
    }
}

impl Outbox {
    /// A poisoned lock means a thread already panicked while holding it;
    /// carry on with the state it left, as `wait.rs`'s `lock` does.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Queue `event` behind everything pending. At `PENDING_CAP`: a
    /// protected event is queued anyway; a terminal one displaces the oldest
    /// event that is neither terminal nor protected, or is dropped when there
    /// is none; an ordinary one is dropped. Every drop or displacement counts
    /// in `dropped`. Never sends.
    pub fn emit(&self, event: PlaybackEvent) {
        let mut inner = self.lock();
        // Protected events — load outcomes — bypass the cap entirely
        // (Decision 4): correlation cannot be inferred from anything later.
        if inner.pending.len() >= PENDING_CAP && !event.is_protected() {
            if !event.is_terminal() {
                inner.dropped += 1;
                return;
            }
            // A terminal outcome displaces the oldest ordinary event rather
            // than being dropped: nothing that follows implies it.
            match displacement_victim(&inner.pending) {
                Some(index) => {
                    inner.pending.remove(index);
                    inner.dropped += 1;
                }
                None => {
                    inner.dropped += 1;
                    return;
                }
            }
        }
        inner.pending.push_back(event);
    }

    /// Send from the front while there is room: an ordinary event only while
    /// more than `RESERVED_EVENT_SLOTS` slots are free, a terminal or
    /// protected one into any free slot. Stops at the first event that does
    /// not fit, so order is kept. A disconnected receiver puts the event back
    /// at the front, so the shutdown report still carries it, and latches
    /// `disconnected`.
    pub fn flush(&self) {
        let mut inner = self.lock();
        loop {
            let Some(front) = inner.pending.front() else {
                return;
            };
            let free = inner.free_slots();
            let may_use_reserve = front.is_terminal() || front.is_protected();
            if free == 0 || (!may_use_reserve && free <= RESERVED_EVENT_SLOTS) {
                return;
            }
            let Some(event) = inner.pending.pop_front() else {
                return;
            };
            match inner.sender.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    inner.pending.push_front(event);
                    return;
                }
                Err(TrySendError::Disconnected(event)) => {
                    inner.pending.push_front(event);
                    inner.disconnected = true;
                    return;
                }
            }
        }
    }

    /// Send `event` only if nothing is pending and an ordinary slot is free;
    /// otherwise discard it and return `false`. For the aggregated warning,
    /// which is a snapshot of counters, not a queued event: one refused now
    /// is rebuilt from the same counters next time. A disconnected receiver
    /// latches `disconnected`.
    pub fn offer(&self, event: PlaybackEvent) -> bool {
        let mut inner = self.lock();
        if !inner.pending.is_empty() || inner.free_slots() <= RESERVED_EVENT_SLOTS {
            return false;
        }
        match inner.sender.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                inner.disconnected = true;
                false
            }
        }
    }

    /// Whether nothing is pending: command admission and `command_waiting`.
    pub fn is_empty(&self) -> bool {
        self.lock().pending.is_empty()
    }

    /// Whether `extra` more events fit under `PENDING_CAP`: the fault
    /// budget's check, which keeps the cap private to this module.
    pub fn fits(&self, extra: usize) -> bool {
        self.lock().pending.len() + extra <= PENDING_CAP
    }

    /// Events dropped or displaced at the cap, for the aggregated warning.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Whether a send has found the receiver gone.
    pub fn disconnected(&self) -> bool {
        self.lock().disconnected
    }

    /// Everything still pending, in order, leaving the queue empty: the
    /// shutdown report's tail.
    pub fn take_remaining(&self) -> Vec<PlaybackEvent> {
        self.lock().pending.drain(..).collect()
    }
}

/// The oldest event `emit` may displace to make room for a terminal arrival
/// at `PENDING_CAP`: neither terminal (already guaranteed delivery some other
/// way) nor protected (never displaced — Decision 4).
fn displacement_victim(pending: &VecDeque<PlaybackEvent>) -> Option<usize> {
    pending
        .iter()
        .position(|event| !event.is_terminal() && !event.is_protected())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::media::provenance::PositionProvenance;
    use crate::playback::command::LoadRequestId;
    use crate::playback::state::PlaybackState;

    /// The channel's ordinary limit: an ordinary event needs more than the
    /// reserve free.
    const ORDINARY: usize = EVENT_CAPACITY - RESERVED_EVENT_SLOTS;

    fn ordinary(tag: usize) -> PlaybackEvent {
        PlaybackEvent::Warning {
            session_rev: 1,
            message: tag.to_string(),
        }
    }

    fn terminal() -> PlaybackEvent {
        PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::ZERO,
            provenance: PositionProvenance::Established,
        }
    }

    fn protected() -> PlaybackEvent {
        PlaybackEvent::LoadCancelled {
            session_rev: 1,
            request: LoadRequestId::from_raw(1),
        }
    }

    fn tag(event: &PlaybackEvent) -> Option<usize> {
        match event {
            PlaybackEvent::Warning { message, .. } => message.parse().ok(),
            _ => None,
        }
    }

    #[test]
    fn an_ordinary_event_at_the_cap_is_dropped_and_counted() {
        let (outbox, _events) = channel();
        for n in 0..PENDING_CAP {
            outbox.emit(ordinary(n));
        }
        outbox.emit(ordinary(PENDING_CAP));
        assert_eq!(outbox.dropped(), 1);
        let remaining = outbox.take_remaining();
        assert_eq!(remaining.len(), PENDING_CAP);
        assert_eq!(remaining.last().and_then(tag), Some(PENDING_CAP - 1));
    }

    #[test]
    fn a_terminal_event_at_the_cap_displaces_the_oldest_ordinary_one() {
        let (outbox, _events) = channel();
        outbox.emit(protected());
        for n in 0..PENDING_CAP - 1 {
            outbox.emit(ordinary(n));
        }
        outbox.emit(terminal());
        assert_eq!(outbox.dropped(), 1);
        let remaining = outbox.take_remaining();
        assert_eq!(remaining.len(), PENDING_CAP);
        assert!(remaining[0].is_protected(), "{:?}", remaining[0]);
        assert_eq!(tag(&remaining[1]), Some(1), "ordinary 0 was the victim");
        assert!(matches!(
            remaining.last(),
            Some(PlaybackEvent::EndOfTrack { .. })
        ));
    }

    /// A terminal, non-protected event may be displaced to make room for
    /// another terminal event at `PENDING_CAP`; a protected one — a load
    /// outcome — never may, since correlation cannot be inferred from
    /// anything later (Decision 4).
    #[test]
    fn a_terminal_event_never_displaces_a_protected_one() {
        let mut pending = VecDeque::new();
        pending.push_back(protected());
        pending.push_back(terminal());
        pending.push_back(ordinary(0));
        assert_eq!(displacement_victim(&pending), Some(2));
        pending.pop_back();
        assert_eq!(displacement_victim(&pending), None);
    }

    #[test]
    fn a_protected_event_is_queued_past_the_cap() {
        let (outbox, _events) = channel();
        for n in 0..PENDING_CAP {
            outbox.emit(ordinary(n));
        }
        outbox.emit(protected());
        assert_eq!(outbox.dropped(), 0);
        assert_eq!(outbox.take_remaining().len(), PENDING_CAP + 1);
    }

    #[test]
    fn flush_keeps_ordinary_events_out_of_the_reserve_and_keeps_order() {
        let (outbox, events) = channel();
        for n in 0..EVENT_CAPACITY {
            outbox.emit(ordinary(n));
        }
        outbox.emit(terminal());
        outbox.flush();
        let sent: Vec<Option<usize>> = events.try_iter().map(|e| tag(&e)).collect();
        assert_eq!(sent, (0..ORDINARY).map(Some).collect::<Vec<_>>());
        // The terminal event waits behind the ordinary ones it followed.
        let rest = outbox.take_remaining();
        assert_eq!(rest.len(), EVENT_CAPACITY - ORDINARY + 1);
        assert!(matches!(
            rest.last(),
            Some(PlaybackEvent::EndOfTrack { .. })
        ));
    }

    #[test]
    fn a_terminal_or_protected_event_may_use_the_reserve() {
        let (outbox, events) = channel();
        for n in 0..ORDINARY {
            outbox.emit(ordinary(n));
        }
        outbox.flush();
        assert_eq!(events.len(), ORDINARY);
        outbox.emit(ordinary(ORDINARY));
        outbox.flush();
        assert_eq!(
            events.len(),
            ORDINARY,
            "an ordinary event took a reserved slot"
        );
        assert!(!outbox.is_empty());
        let _ = outbox.take_remaining();
        outbox.emit(terminal());
        outbox.emit(protected());
        outbox.emit(PlaybackEvent::StateChanged {
            session_rev: 1,
            state: PlaybackState::Stopped,
            request: None,
        });
        outbox.flush();
        assert_eq!(events.len(), ORDINARY + 3);
        assert!(outbox.is_empty());
    }

    #[test]
    fn offer_sends_only_into_a_free_ordinary_slot_with_nothing_pending() {
        let (outbox, events) = channel();
        assert!(outbox.offer(ordinary(0)));
        assert_eq!(events.len(), 1);

        outbox.emit(ordinary(1));
        assert!(!outbox.offer(ordinary(2)), "jumped a pending event");
        assert_eq!(events.len(), 1);

        let (full, full_events) = channel();
        for n in 0..ORDINARY {
            full.emit(ordinary(n));
        }
        full.flush();
        assert!(!full.offer(ordinary(ORDINARY)), "took a reserved slot");
        assert_eq!(full_events.len(), ORDINARY);
    }

    #[test]
    fn a_disconnect_latches_and_keeps_the_unsent_event() {
        let (outbox, events) = channel();
        drop(events);
        outbox.emit(ordinary(7));
        outbox.flush();
        assert!(outbox.disconnected());
        let remaining = outbox.take_remaining();
        assert_eq!(remaining.iter().map(tag).collect::<Vec<_>>(), vec![Some(7)]);
    }

    #[test]
    fn offer_latches_a_disconnect() {
        let (outbox, events) = channel();
        drop(events);
        assert!(!outbox.offer(ordinary(0)));
        assert!(outbox.disconnected());
    }

    #[test]
    fn take_remaining_returns_events_in_order_and_empties() {
        let (outbox, _events) = channel();
        for n in 0..3 {
            outbox.emit(ordinary(n));
        }
        let remaining = outbox.take_remaining();
        assert_eq!(
            remaining.iter().map(tag).collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2)]
        );
        assert!(outbox.is_empty());
    }

    #[test]
    fn fits_counts_room_under_the_cap() {
        let (outbox, _events) = channel();
        for n in 0..PENDING_CAP - 8 {
            outbox.emit(ordinary(n));
        }
        assert!(outbox.fits(8));
        assert!(!outbox.fits(9));
    }
}
