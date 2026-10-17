//! Unit-level facts `WaitService` rests on: the hook and the main loop are one
//! implementation, and servicing with no transport preserves the retained
//! position instead of rewinding it to zero.
//!
//! The engine-level version of this fact - progress still rising while a real
//! read is blocked - needs a stalled server to stage, so it lives in Task 13
//! (H13). This is only the layer that fact rests on.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tenuto::http::channel::{SourceInterrupt, WaitHook};
use tenuto::media::provenance::PositionProvenance;
use tenuto::playback::engine::TransportCore;
use tenuto::playback::event::{PlaybackEvent, Progress};
use tenuto::playback::handshake::Handshake;
use tenuto::playback::link::OutputLink;
use tenuto::playback::outbox::{self, Outbox};
use tenuto::playback::output::{Nanos, SpanRecord};
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::{PositionQuality, Timeline};
use tenuto::playback::wait::{SessionFacts, WaitService};

/// The wiring only the freeze tests care about, in the inert configuration:
/// nothing frozen, nothing queued, and nobody reading. The receiver is
/// dropped, so a flush latches a disconnect and keeps its events queued,
/// which these tests never look at.
fn inert() -> (Arc<SourceInterrupt>, Arc<Outbox>) {
    let (outbox, _events) = outbox::channel();
    (SourceInterrupt::new(1024), outbox)
}

#[test]
fn servicing_publishes_the_position_the_timeline_reports() {
    let transport: Arc<Mutex<Option<TransportCore>>> = Arc::new(Mutex::new(None));
    let progress = Arc::new(Mutex::new(Progress {
        session_rev: 0,
        media: None,
        position: Duration::ZERO,
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: None,
    }));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 4,
        media: None,
        position: Duration::from_secs(9),
        degraded: false,
        playing: true,
        provenance: PositionProvenance::Established,
        frozen_by_hook: false,
        load: None,
    }));
    let (interrupt, outbox) = inert();
    let service = WaitService::new(
        Arc::clone(&transport),
        Arc::clone(&progress),
        Arc::clone(&facts),
        interrupt,
        outbox,
        Arc::new(|| Nanos(0)),
    );

    service.service();

    let published = match progress.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(published.session_rev, 4);
    // With no transport the retained position stands: a blocked read must not
    // rewind the position to zero just because there is nothing to sample.
    assert_eq!(published.position, Duration::from_secs(9));
}

/// §3: `quality` and `provenance` are independent axes, and this is the one
/// place that could accidentally couple them — `publish_progress` computes
/// `quality` fresh on every publish, and provenance must still be read
/// straight off `facts.provenance` rather than being inferred from the
/// `quality` it just computed. `facts.degraded = true` here forces `Degraded`
/// quality (with no transport, so nothing else can override it); provenance
/// is set to `Established` independently. If a future change derived
/// provenance from quality (e.g. "Degraded implies Estimated"), the
/// published `provenance` would flip to `Estimated` and this assertion would
/// fail.
#[test]
fn a_degraded_quality_does_not_imply_estimated_provenance() {
    let transport: Arc<Mutex<Option<TransportCore>>> = Arc::new(Mutex::new(None));
    let progress = Arc::new(Mutex::new(Progress {
        session_rev: 0,
        media: None,
        position: Duration::ZERO,
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: None,
    }));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 1,
        media: None,
        position: Duration::from_secs(4),
        degraded: true,
        playing: true,
        provenance: PositionProvenance::Established,
        frozen_by_hook: false,
        load: None,
    }));
    let (interrupt, outbox) = inert();
    let service = WaitService::new(
        Arc::clone(&transport),
        Arc::clone(&progress),
        Arc::clone(&facts),
        interrupt,
        outbox,
        Arc::new(|| Nanos(0)),
    );

    service.service();

    let published = match progress.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(published.quality, PositionQuality::Degraded);
    assert_eq!(
        published.provenance,
        PositionProvenance::Established,
        "provenance must be read from facts, never derived from quality"
    );
}

#[test]
fn servicing_a_freeze_parks_the_output_and_announces_paused() {
    // Section 9: pause must work during a stalled read. The worker is inside
    // pump_audio and will not reach its command loop until the read returns,
    // so if the hook does not do this, nothing does - the output keeps
    // draining and Paused is never emitted.
    let (outbox, events_rx) = outbox::channel();
    let interrupt = SourceInterrupt::new(1024);
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 3,
        media: None,
        position: Duration::from_secs(7),
        degraded: false,
        playing: true,
        provenance: PositionProvenance::Established,
        frozen_by_hook: false,
        load: None,
    }));
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(Progress {
            session_rev: 3,
            media: None,
            position: Duration::from_secs(7),
            quality: PositionQuality::Exact,
            provenance: PositionProvenance::Established,
            buffering: false,
            load: None,
        })),
        Arc::clone(&facts),
        Arc::clone(&interrupt),
        Arc::clone(&outbox),
        Arc::new(|| Nanos(0)),
    );

    interrupt.freeze();
    service.service();
    match events_rx.try_recv() {
        Ok(PlaybackEvent::StateChanged {
            session_rev: 3,
            state: PlaybackState::Paused,
            request: None,
        }) => {}
        other => panic!("expected StateChanged{{Paused}}, got {other:?}"),
    }
    // Idempotent: a second slice must not re-announce.
    service.service();
    assert!(
        events_rx.try_recv().is_err(),
        "the freeze was announced twice"
    );

    interrupt.thaw();
    service.service();
    match events_rx.try_recv() {
        Ok(PlaybackEvent::StateChanged {
            session_rev: 3,
            state: PlaybackState::Playing,
            request: None,
        }) => {}
        other => panic!("expected StateChanged{{Playing}}, got {other:?}"),
    }
}

fn quiet_progress() -> Arc<Mutex<Progress>> {
    Arc::new(Mutex::new(Progress {
        session_rev: 1,
        media: None,
        position: Duration::ZERO,
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: None,
    }))
}

fn playing_facts() -> Arc<Mutex<SessionFacts>> {
    Arc::new(Mutex::new(SessionFacts {
        session_rev: 1,
        media: None,
        position: Duration::ZERO,
        degraded: false,
        playing: true,
        provenance: PositionProvenance::Established,
        frozen_by_hook: false,
        load: None,
    }))
}

fn warning(message: &str) -> PlaybackEvent {
    PlaybackEvent::Warning {
        session_rev: 1,
        message: message.into(),
    }
}

#[test]
fn a_hook_announcement_queues_behind_the_workers_backlog_and_a_blocked_read_delivers_both_in_order()
{
    // One queue: what the worker emitted before its read blocked goes out
    // first, and the hook's Paused after it, from the same slice's flush.
    let (outbox, events_rx) = outbox::channel();
    let interrupt = SourceInterrupt::new(1024);
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        quiet_progress(),
        playing_facts(),
        Arc::clone(&interrupt),
        Arc::clone(&outbox),
        Arc::new(|| Nanos(0)),
    );
    outbox.emit(warning("first"));
    outbox.emit(warning("second"));

    interrupt.freeze();
    service.service();

    let received: Vec<PlaybackEvent> = events_rx.try_iter().collect();
    assert!(
        matches!(
            received.as_slice(),
            [
                PlaybackEvent::Warning { message: first, .. },
                PlaybackEvent::Warning { message: second, .. },
                PlaybackEvent::StateChanged {
                    state: PlaybackState::Paused,
                    ..
                },
            ] if first == "first" && second == "second"
        ),
        "{received:?}"
    );
}

#[test]
fn a_blocked_read_flushes_a_backlog_the_worker_left() {
    // The worker queued more than the channel's ordinary slots hold (64,
    // less the 9 reserved for terminal and protected outcomes: 55), then
    // blocked in a read. As the application drains, each slice of that read
    // sends what now fits, instead of waiting for the read to return.
    let (outbox, events_rx) = outbox::channel();
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        quiet_progress(),
        playing_facts(),
        SourceInterrupt::new(1024),
        Arc::clone(&outbox),
        Arc::new(|| Nanos(0)),
    );
    for n in 0..56 {
        outbox.emit(warning(&n.to_string()));
    }
    outbox.flush();
    assert_eq!(events_rx.len(), 55);
    assert!(!outbox.is_empty());

    let _ = events_rx.try_recv();
    service.service();

    assert_eq!(events_rx.len(), 55);
    assert!(outbox.is_empty(), "the held event was not sent");
}

#[test]
fn servicing_with_no_transport_is_harmless_and_repeatable() {
    let transport: Arc<Mutex<Option<TransportCore>>> = Arc::new(Mutex::new(None));
    let progress = Arc::new(Mutex::new(Progress {
        session_rev: 0,
        media: None,
        position: Duration::from_secs(3),
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: None,
    }));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 1,
        media: None,
        position: Duration::from_secs(3),
        degraded: false,
        playing: false,
        provenance: PositionProvenance::Established,
        frozen_by_hook: false,
        load: None,
    }));
    let (interrupt, outbox) = inert();
    let service = WaitService::new(
        transport,
        Arc::clone(&progress),
        facts,
        interrupt,
        outbox,
        Arc::new(|| Nanos(0)),
    );
    for _ in 0..100 {
        service.service();
    }
    let published = match progress.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(published.position, Duration::from_secs(3));
}

/// Fix-round evidence for the clock path: a real `TransportCore`, one
/// already-published span to interpolate within, and a clock the test
/// advances itself — with no `Worker` and no worker loop anywhere in this
/// test. This is what proves `service_as(Servicing::BlockedRead)` can move
/// the published position on its own, which is the entire point of the hook
/// existing (G5/§8): a blocked decoder read reaches only this path.
#[test]
fn servicing_a_blocked_read_advances_the_position_as_the_clock_advances() {
    const RATE: u32 = 48_000;

    let link = Arc::new(OutputLink::new());
    // Capacity 4 is plenty: this test publishes exactly one span, directly,
    // bypassing the callback protocol entirely — `drain_spans` only ever
    // pops from this ring, so nothing here needs `Phase::Run` to be running.
    let (mut span_tx, span_rx) = rtrb::RingBuffer::<SpanRecord>::new(4);
    let handshake = Handshake::new(Arc::clone(&link), span_rx);
    let timeline = Timeline::new(RATE);
    let core = TransportCore::new(handshake, timeline, Duration::ZERO, RATE);
    let transport = Arc::new(Mutex::new(Some(core)));

    // One second of media at 48 kHz, starting at t0 = 0.
    span_tx
        .push(SpanRecord {
            generation: 0,
            media_total_after: u64::from(RATE),
            t0: Nanos(0),
            frames: RATE,
        })
        .expect("a fresh 4-slot ring accepts one record");

    let progress = Arc::new(Mutex::new(Progress {
        session_rev: 5,
        media: None,
        position: Duration::ZERO,
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: None,
    }));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 5,
        media: None,
        position: Duration::ZERO,
        degraded: false,
        playing: true,
        provenance: PositionProvenance::Established,
        frozen_by_hook: false,
        load: None,
    }));
    let (interrupt, outbox) = inert();
    let now = Arc::new(AtomicU64::new(100_000_000)); // 100ms into the span
    let clock = {
        let now = Arc::clone(&now);
        Arc::new(move || Nanos(now.load(Ordering::Relaxed)))
    };
    let service = WaitService::new(
        Arc::clone(&transport),
        Arc::clone(&progress),
        facts,
        interrupt,
        outbox,
        clock,
    );

    // First slice of a blocked read: the worker loop is not running, only
    // the hook is - exactly `WaitHook::service`'s call shape.
    service.service();
    let first = match progress.lock() {
        Ok(guard) => guard.position,
        Err(poisoned) => poisoned.into_inner().position,
    };

    // The read is still blocked; the clock advances (as `CallbackCore::fill`
    // would, on the audio backend thread) and the hook is serviced again -
    // still with no worker loop pass in between.
    now.store(300_000_000, Ordering::Relaxed); // 300ms into the span
    service.service();
    let second = match progress.lock() {
        Ok(guard) => guard.position,
        Err(poisoned) => poisoned.into_inner().position,
    };

    assert!(
        second > first,
        "position must advance across two blocked-read services with no \
         worker loop pass between them: {first:?} -> {second:?}"
    );
    assert_eq!(second, Duration::from_millis(300));
}
