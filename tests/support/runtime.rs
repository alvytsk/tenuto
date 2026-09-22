//! Shared rig for suites that drive a [`PlayerRuntime`] headlessly: a real
//! engine over the paced, deviceless `NullOutput`, a state writer into a
//! temporary directory, and pumping helpers with a generous deadline.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tenuto::application::enrich::TagProbe;
use tenuto::application::runtime::{
    AppCommand, EngineFactory, EnqueueItem, LibraryStores, PlayerRuntime, RuntimeParts,
};
use tenuto::application::view::PlayerView;
use tenuto::clock::{Clock, SystemClock};
use tenuto::http::limits::Limits;
use tenuto::lifecycle::hooks::TestHook;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::persistence::writer::WriterHandle;
use tenuto::playback::engine::EngineHandle;
use tenuto::playback::output::null_output::NullOutput;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::queue::QueueEntryId;
use tenuto::session::Session;

pub struct Rig {
    pub _dir: tempfile::TempDir,
    pub runtime: PlayerRuntime,
    pub state_path: std::path::PathBuf,
}

pub fn null_engine() -> EngineFactory {
    Box::new(|| EngineHandle::spawn(Box::new(NullOutput::new())))
}

pub fn rig_with(state: PersistedState) -> Rig {
    rig_with_parts(state, None, null_engine())
}

/// A rig whose engine reconnects on `policy` rather than on the production
/// one, so a whole outage fits inside a test's patience.
pub fn rig_with_reconnect_policy(state: PersistedState, policy: ReconnectPolicy) -> Rig {
    rig_with_parts(
        state,
        None,
        Box::new(move || {
            let engine = EngineHandle::spawn(Box::new(NullOutput::new()));
            engine.set_reconnect_policy(policy);
            engine
        }),
    )
}

pub fn rig_with_parts(
    state: PersistedState,
    library: Option<LibraryStores>,
    engine_factory: EngineFactory,
) -> Rig {
    build(state, library, engine_factory, None)
}

/// A rig whose metadata workers run `probe`; every other rig has
/// enrichment disabled.
pub fn rig_with_probe(state: PersistedState, probe: TagProbe) -> Rig {
    build(state, None, null_engine(), Some(probe))
}

fn build(
    state: PersistedState,
    library: Option<LibraryStores>,
    engine_factory: EngineFactory,
    metadata_probe: Option<TagProbe>,
) -> Rig {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let state_path = dir.path().join("state.json");
    let writer = WriterHandle::spawn(
        Box::new(StateStore::new(state_path.clone(), clock.clone())),
        clock.clone(),
    );
    let runtime = PlayerRuntime::new(RuntimeParts {
        metadata_probe,
        ..parts(state, writer, clock, library, engine_factory)
    });
    Rig {
        _dir: dir,
        runtime,
        state_path,
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

pub fn pump_until(runtime: &mut PlayerRuntime, what: &str, done: impl Fn(&PlayerView) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        runtime.pump();
        if done(&runtime.view()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "never reached: {what}; view {:?}",
            runtime.view()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn pump_for(runtime: &mut PlayerRuntime, span: Duration) {
    let until = Instant::now() + span;
    while Instant::now() < until {
        runtime.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
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
