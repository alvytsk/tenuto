//! M7 §10: the runtime keeps a reconnecting station pausable and never
//! submits a seek for it. L18 (runtime half), L20.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

use std::time::Duration;

use runtime::{enqueue, pump_for, pump_until, rig_with_reconnect_policy, row_ids};
use support::server::{Script, TestServer};
use tenuto::application::runtime::{AppCommand, EnqueueItem};
use tenuto::persistence::model::PersistedState;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;

/// About two seconds of the 64 kbps fixture before the connection dies. The
/// rig's engine runs on the real-time-paced `NullOutput`, so this is two
/// seconds of the test's own patience, not of a virtual clock.
const CUT: usize = 16 * 1024;

#[test]
fn space_during_a_reconnect_pauses_and_arrow_keys_never_reach_the_engine() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .truncate_body_after(CUT)
            // Every later connection fails, so the outage lasts as long as
            // the test needs it to.
            .then(Script::serving(Vec::new()).status(503)),
    );
    let mut rig = rig_with_reconnect_policy(
        PersistedState::default(),
        ReconnectPolicy {
            backoff: [Duration::from_millis(20); 5],
            budget: Duration::from_secs(60),
            stable_after: Duration::from_secs(30),
        },
    );
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::from_input(&server.url("/radio"))],
    );
    let station = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(station));
    pump_until(&mut rig.runtime, "the station reconnects", |view| {
        view.reconnecting
    });
    assert!(rig.runtime.view().live, "{:?}", rig.runtime.view());

    rig.runtime.handle(AppCommand::SeekBy(10));
    // Past `KeyRouter`'s 250ms quiet window: a burst that had opened would
    // have flushed by now.
    pump_for(&mut rig.runtime, Duration::from_millis(350));
    let status = rig.runtime.view().status;
    assert!(
        status.as_deref().is_some_and(|s| s.contains("live stream")),
        "{status:?}"
    );

    rig.runtime.handle(AppCommand::PlayPause);
    pump_until(&mut rig.runtime, "the station pauses", |view| {
        view.now_playing
            .as_ref()
            .is_some_and(|now| now.state == PlaybackState::Paused)
    });
    // A paused station holds no connection, so the reconnect attempts stop
    // with it.
    let settled = server.requests().len();
    pump_for(&mut rig.runtime, Duration::from_millis(300));
    assert_eq!(server.requests().len(), settled);

    let _ = rig.runtime.shutdown();
    server.shutdown();
}
