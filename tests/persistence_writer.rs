//! The writer thread: coalescing, retry, and the bounded shutdown.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tenuto::clock::{Clock, FakeClock};
use tenuto::persistence::PersistenceError;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::writer::{ShutdownOutcome, StateSink, Urgency, WriterHandle};
use tenuto::volume::Volume;

const PATIENCE: Duration = Duration::from_secs(5);

/// Records what it was asked to write, and fails the first `fail_first` attempts.
#[derive(Default)]
struct ScriptedSink {
    written: Mutex<Vec<f32>>,
    attempts: AtomicUsize,
    fail_first: usize,
}

impl ScriptedSink {
    fn new(fail_first: usize) -> Arc<Self> {
        Arc::new(Self {
            fail_first,
            ..Self::default()
        })
    }
    fn written(&self) -> Vec<f32> {
        self.written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
}

/// The sink the writer owns is a handle onto the `Arc`, so the test keeps a
/// handle to the same sink and can read back what landed. The wrapper is a
/// local type because the orphan rule forbids `impl StateSink for
/// Arc<ScriptedSink>`: `Arc` is not fundamental, so that would be a foreign
/// trait on a foreign type.
struct SharedSink(Arc<ScriptedSink>);

impl StateSink for SharedSink {
    fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        let attempt = self.0.attempts.fetch_add(1, Ordering::Relaxed);
        if attempt < self.0.fail_first {
            return Err(PersistenceError::NoStateDirectory);
        }
        self.0
            .written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(state.volume().as_gain());
        Ok(())
    }
}

impl SharedSink {
    fn of(sink: &Arc<ScriptedSink>) -> Box<Self> {
        Box::new(Self(Arc::clone(sink)))
    }
}

struct BlockingSink;

impl StateSink for BlockingSink {
    fn write(&self, _state: &PersistedState) -> Result<(), PersistenceError> {
        std::thread::sleep(Duration::from_secs(10));
        Ok(())
    }
}

/// Volume is the marker: it is one `f32` on the snapshot, so a test can name
/// each submission by a number and read back exactly which ones landed.
fn snapshot(marker: f32) -> PersistedState {
    let mut state = PersistedState::default();
    state.set_volume(Volume::new(marker));
    state
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_forced_submit_is_written_without_waiting_for_the_window() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    let writer = WriterHandle::spawn(SharedSink::of(&sink), clock);

    writer.submit(snapshot(0.5), Urgency::Forced);

    assert!(
        wait_until(|| sink.written() == vec![0.5]),
        "forced bypasses the coalescing window"
    );
}

#[test]
fn an_ordinary_submit_waits_out_the_window() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    // `Arc::clone(&clock)` here would be E0308: the annotation makes the
    // argument position expect `&Arc<dyn Clock>`, and `&Arc<FakeClock>` does
    // not coerce through a reference. A method call resolves on the concrete
    // type first, and it is the *result* that unsizes.
    let injected: Arc<dyn Clock> = clock.clone();
    let writer = WriterHandle::spawn(SharedSink::of(&sink), injected);

    writer.submit(snapshot(0.5), Urgency::Ordinary);
    clock.advance_monotonic(Duration::from_millis(1900));
    std::thread::sleep(Duration::from_millis(250));
    assert!(sink.written().is_empty(), "1.9 s is inside the 2 s window");

    clock.advance_monotonic(Duration::from_millis(200));
    assert!(
        wait_until(|| sink.written() == vec![0.5]),
        "the snapshot was never written after its deadline passed"
    );
}

#[test]
fn a_replacement_never_extends_the_deadline_and_the_newest_wins() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    // `Arc::clone(&clock)` here would be E0308: the annotation makes the
    // argument position expect `&Arc<dyn Clock>`, and `&Arc<FakeClock>` does
    // not coerce through a reference. A method call resolves on the concrete
    // type first, and it is the *result* that unsizes.
    let injected: Arc<dyn Clock> = clock.clone();
    let writer = WriterHandle::spawn(SharedSink::of(&sink), injected);

    writer.submit(snapshot(0.1), Urgency::Ordinary);
    clock.advance_monotonic(Duration::from_millis(1500));
    writer.submit(snapshot(0.2), Urgency::Ordinary);
    clock.advance_monotonic(Duration::from_millis(600));

    // 2.1 s after the first submission, so the deadline anchored on the first
    // has passed even though the second arrived 0.6 s ago.
    assert!(
        wait_until(|| sink.written() == vec![0.2]),
        "the newest snapshot wins, on the original deadline"
    );
}

#[test]
fn a_failed_write_is_retried_and_the_retry_carries_whatever_is_newest() {
    let sink = ScriptedSink::new(1);
    let clock = Arc::new(FakeClock::new());
    // `Arc::clone(&clock)` here would be E0308: the annotation makes the
    // argument position expect `&Arc<dyn Clock>`, and `&Arc<FakeClock>` does
    // not coerce through a reference. A method call resolves on the concrete
    // type first, and it is the *result* that unsizes.
    let injected: Arc<dyn Clock> = clock.clone();
    let writer = WriterHandle::spawn(SharedSink::of(&sink), injected);

    writer.submit(snapshot(0.1), Urgency::Forced);
    assert!(
        wait_until(|| sink.attempts() >= 1),
        "the writer never attempted the forced snapshot"
    );
    assert!(sink.written().is_empty(), "the first attempt fails");

    // A newer snapshot lands while the failed one waits to be retried; it must
    // supersede rather than be replaced by the failure.
    writer.submit(snapshot(0.9), Urgency::Forced);
    assert!(
        wait_until(|| sink.written() == vec![0.9]),
        "keep-latest survives a retry: got {:?}",
        sink.written()
    );
}

#[test]
fn shutdown_flushes_what_is_pending_and_confirms_it() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    let mut writer = WriterHandle::spawn(SharedSink::of(&sink), clock);

    // Ordinary, so nothing is due: shutdown must still flush it.
    writer.submit(snapshot(0.7), Urgency::Ordinary);
    assert!(matches!(writer.shutdown(), ShutdownOutcome::Written));
    assert_eq!(sink.written(), vec![0.7]);
}

#[test]
fn shutdown_reports_a_final_write_that_failed() {
    let sink = ScriptedSink::new(usize::MAX);
    let clock = Arc::new(FakeClock::new());
    let mut writer = WriterHandle::spawn(SharedSink::of(&sink), clock);

    writer.submit(snapshot(0.7), Urgency::Forced);
    assert!(matches!(writer.shutdown(), ShutdownOutcome::Failed(_)));
}

#[test]
fn a_shutdown_that_is_not_acknowledged_detaches_rather_than_hanging() {
    let clock = Arc::new(FakeClock::new());
    let mut writer = WriterHandle::spawn(Box::new(BlockingSink), clock);
    writer.submit(snapshot(0.7), Urgency::Forced);

    let started = Instant::now();
    assert!(matches!(writer.shutdown(), ShutdownOutcome::Unconfirmed));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(2_500),
        "the 2 s bound must not be defeated by an unconditional join: took {elapsed:?}"
    );
}

#[test]
fn submits_after_shutdown_are_rejected() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    let mut writer = WriterHandle::spawn(SharedSink::of(&sink), clock);

    assert!(matches!(writer.shutdown(), ShutdownOutcome::Written));
    writer.submit(snapshot(0.4), Urgency::Forced);
    std::thread::sleep(Duration::from_millis(100));
    assert!(sink.written().is_empty());
}
