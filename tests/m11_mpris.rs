//! M11: desktop media controls drive the runtime through `AppCommand`.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

use std::time::Duration;

use runtime::{enqueue, pump_for, pump_until, rig_with, row_ids, step};
use support::server::{Script, TestServer};
use tenuto::application::runtime::{AppCommand, EnqueueItem};
use tenuto::application::transport::PlaybackPhase;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::state::PlaybackState;

const FIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");

#[test]
fn a_pause_after_the_engine_paused_does_not_resume() {
    let mut rig = rig_with(PersistedState::default());
    enqueue(&mut rig.runtime, vec![EnqueueItem::Path(FIVE.into())]);
    let row = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(row));
    pump_until(&mut rig, "playing", |v| v.phase == PlaybackPhase::Playing);
    rig.runtime.handle(AppCommand::PlayPause);
    // The view still says Playing: a toggle here would resume.
    rig.runtime.handle(AppCommand::Pause);
    pump_until(&mut rig, "paused", |v| v.phase == PlaybackPhase::Paused);
    pump_for(&mut rig, Duration::from_secs(1));
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Paused);
    let _ = rig.runtime.shutdown();
}

#[test]
fn pause_does_nothing_when_nothing_is_loaded() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Pause);
    step(&mut rig);
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Unloaded);
    let _ = rig.runtime.shutdown();
}

#[test]
fn pause_on_a_paused_station_leaves_it_paused() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let mut rig = rig_with(PersistedState::default());
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::from_input(&server.url("/radio"))],
    );
    let station = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(station));
    let paused = |v: &tenuto::application::view::PlayerView| {
        v.now_playing
            .as_ref()
            .is_some_and(|now| now.state == PlaybackState::Paused)
    };
    pump_until(&mut rig, "playing", |v| v.phase == PlaybackPhase::Playing);
    rig.runtime.handle(AppCommand::Pause);
    pump_until(&mut rig, "paused", paused);
    let phase = rig.runtime.view().phase;
    let settled = server.requests().len();

    rig.runtime.handle(AppCommand::Pause);
    pump_for(&mut rig, Duration::from_secs(2));
    assert_eq!(rig.runtime.view().phase, phase);
    assert!(!rig.runtime.view().reconnecting);
    assert_eq!(server.requests().len(), settled);
    let _ = rig.runtime.shutdown();
    server.shutdown();
}
