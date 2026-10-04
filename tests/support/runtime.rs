//! Shared rig for suites that drive a [`PlayerRuntime`] headlessly, on one
//! virtual clock (M9.5): the engine plays into a `VirtualDevice`, and the
//! runtime, its state writer and the engine's network budgets all read one
//! `FakeClock`. Time passes only inside `pump_until` and `pump_for`, which
//! step the device and the clock together a period at a time, so a quiet
//! window, a coalesce or a backoff is measured in the same time as the audio
//! that played across it, however loaded the machine.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use crate::support::{PERIOD, VirtualDevice};
use tenuto::application::enrich::TagProbe;
use tenuto::application::runtime::{
    AppCommand, EngineFactory, EnqueueItem, LibraryStores, PlayerRuntime, RuntimeParts,
};
use tenuto::application::view::PlayerView;
use tenuto::clock::{Clock, FakeClock};
use tenuto::http::limits::Limits;
use tenuto::lifecycle::hooks::TestHook;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::persistence::writer::{StateSink, WriterHandle};
use tenuto::playback::engine::EngineHandle;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::queue::QueueEntryId;
use tenuto::session::Session;

/// A deadlock guard in real time; reaching it is a failure, never an exit.
const PATIENCE: Duration = Duration::from_secs(20);

pub struct Rig {
    pub _dir: tempfile::TempDir,
    pub runtime: PlayerRuntime,
    pub state_path: std::path::PathBuf,
    clock: Arc<FakeClock>,
    device: VirtualDevice,
}

pub fn rig_with(state: PersistedState) -> Rig {
    build(state, None, None, None, None)
}

/// A rig whose engine reconnects on `policy` rather than on the production
/// one, so a whole outage fits inside a test's patience.
pub fn rig_with_reconnect_policy(state: PersistedState, policy: ReconnectPolicy) -> Rig {
    build(state, None, Some(policy), None, None)
}

pub fn rig_with_library(state: PersistedState, library: LibraryStores) -> Rig {
    build(state, Some(library), None, None, None)
}

/// A rig whose metadata workers run `probe`; every other rig has
/// enrichment disabled.
pub fn rig_with_probe(state: PersistedState, probe: TagProbe) -> Rig {
    build(state, None, None, Some(probe), None)
}

/// A rig whose writer writes into `sink` rather than a state file.
pub fn rig_with_sink(state: PersistedState, sink: Box<dyn StateSink>) -> Rig {
    build(state, None, None, None, Some(sink))
}

fn build(
    state: PersistedState,
    library: Option<LibraryStores>,
    policy: Option<ReconnectPolicy>,
    metadata_probe: Option<TagProbe>,
    sink: Option<Box<dyn StateSink>>,
) -> Rig {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let fake = Arc::new(FakeClock::new());
    fake.set_wall(OffsetDateTime::now_utc());
    let clock: Arc<dyn Clock> = fake.clone();
    let state_path = dir.path().join("state.json");
    let sink = sink.unwrap_or_else(|| Box::new(StateStore::new(state_path.clone(), clock.clone())));
    let writer = WriterHandle::spawn(sink, clock.clone());
    let device = VirtualDevice::new();
    let engine_factory: EngineFactory = {
        let device = device.clone();
        let clock = clock.clone();
        Box::new(move || {
            let engine = EngineHandle::spawn_on_clock(
                device.output(),
                crossbeam_channel::never(),
                clock.clone(),
            );
            if let Some(policy) = policy {
                engine.set_reconnect_policy(policy);
            }
            engine
        })
    };
    let runtime = PlayerRuntime::new(RuntimeParts {
        metadata_probe,
        ..parts(state, writer, clock, library, engine_factory)
    });
    Rig {
        _dir: dir,
        runtime,
        state_path,
        clock: fake,
        device,
    }
}

/// The one `RuntimeParts` literal every rig builds from.
pub fn parts(
    state: PersistedState,
    writer: WriterHandle,
    clock: Arc<dyn Clock>,
    library: Option<LibraryStores>,
    engine_factory: EngineFactory,
) -> RuntimeParts {
    RuntimeParts {
        session: Session::new(state),
        writer,
        persisting: true,
        clock,
        engine_factory,
        library,
        http_limits: Limits::default(),
        metadata_probe: None,
        hook: TestHook::None,
    }
}

/// Pump, then step one period at a time until `done` holds.
pub fn pump_until(rig: &mut Rig, what: &str, done: impl Fn(&PlayerView) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    rig.runtime.pump();
    while !done(&rig.runtime.view()) {
        assert!(
            Instant::now() < deadline,
            "never reached: {what}; view {:?}",
            rig.runtime.view()
        );
        step(rig);
    }
}

/// Let `span` of virtual time pass, pumping after every period.
pub fn pump_for(rig: &mut Rig, span: Duration) {
    let mut passed = Duration::ZERO;
    while passed < span {
        step(rig);
        passed += PERIOD;
    }
}

/// One period of virtual time on the device and the clock, then a pump: the
/// unit `pump_until` and `pump_for` are made of, for a test's own loop. The
/// nap is a period of real time, so the virtual clock never outruns real
/// time: the engine's worker and the threads off the clock (metadata,
/// artwork, the test server) get at least the span a test lets pass.
pub fn step(rig: &mut Rig) {
    rig.device.advance(PERIOD);
    rig.clock.advance(PERIOD);
    std::thread::sleep(PERIOD);
    rig.runtime.pump();
}

/// Adds into whichever playlist the runtime is viewing — what a pre-M8
/// `AppCommand::Enqueue(items)` meant, now that the command names its
/// destination.
pub fn enqueue(runtime: &mut PlayerRuntime, items: Vec<EnqueueItem>) {
    let dest = runtime.viewed();
    runtime.handle(AppCommand::Enqueue { dest, items });
}

pub fn row_ids(runtime: &PlayerRuntime) -> Vec<QueueEntryId> {
    runtime.view().rows.iter().map(|row| row.id).collect()
}
