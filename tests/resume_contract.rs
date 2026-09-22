//! Session 1 → persist → session 2. The rig runs `app::run`'s ordering —
//! drain events, then sample once — against the test engine and a real store
//! in a tempdir, writing synchronously so nothing here depends on a thread.
//!
//! The quit goes through `Session::reconcile_shutdown`, which is the same
//! handoff `app::run` performs: these tests would not be worth much if they
//! verified a copy of it that lives here.

use std::sync::Arc;
use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::{PersistedCheckpoint, PersistedState, SCHEMA_VERSION};
use tenuto::persistence::store::StateStore;
use tenuto::persistence::writer::Urgency;
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::playback::command::{PlaybackCommand, ResumeIntent};
use tenuto::playback::decode::DecodedSource;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::playback::volume::Volume;
use tenuto::resume::{RestartPreference, decide_resume, restart_preference, resume_candidate};
use tenuto::session::{Action, CAPTURE_INTERVAL, LoadTarget, Session};

mod support;

use support::{TestEngine, media};

const TRACK: &str = "sine-5s.flac";
/// The fixture's duration, which the probe would supply in `app::run`. The rig
/// checks the probe agrees with it, so it cannot drift from the fixture.
const TRACK_DURATION: Duration = Duration::from_secs(5);

/// How long `Rig::send` gives a command to be applied and its event to be
/// flushed. §3 spends one worker pass on each, and the engine's own pass is
/// paced by the harness device, so this is a budget rather than a count of
/// passes: eight periods' worth of wall time, which every command in these
/// tests has needed a small fraction of.
const SETTLE_PASSES: usize = 40;
const SETTLE_NAP: Duration = Duration::from_millis(5);

fn fixture_path() -> AbsolutePath {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(TRACK);
    let Ok(canonical) = path.canonicalize() else {
        panic!("the fixture must exist: {path:?}");
    };
    match AbsolutePath::new(canonical) {
        Ok(path) => path,
        Err(error) => panic!("the fixture path must be identifiable: {error}"),
    }
}

fn track_id() -> MediaId {
    MediaId::LocalFile(fixture_path())
}

/// A store on `dir/state.json` and the clock it stamps `updated_at` from, for
/// the tests that do not build a whole rig.
fn store_in(dir: &std::path::Path) -> (StateStore, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new());
    // `clock.clone()`, not `Arc::clone(&clock)`: the annotation constrains the
    // argument position, and `&Arc<FakeClock>` does not coerce to
    // `&Arc<dyn Clock>`.
    let injected: Arc<dyn Clock> = clock.clone();
    (StateStore::new(dir.join("state.json"), injected), clock)
}

struct Rig {
    engine: TestEngine,
    session: Session,
    store: StateStore,
    clock: Arc<FakeClock>,
}

impl Rig {
    /// Session 1: a fresh file, playing from the top.
    fn open(dir: &std::path::Path) -> Self {
        Self::open_at(dir, PersistedState::default(), Duration::ZERO)
    }

    fn open_at(dir: &std::path::Path, state: PersistedState, start_at: Duration) -> Self {
        // §11 validates a stored position against the duration the probe
        // reports, and these tests stand `TRACK_DURATION` in for it.
        let probed = match DecodedSource::open(&fixture_path()) {
            Ok(source) => source.metadata().duration,
            Err(error) => panic!("the fixture must be decodable: {error}"),
        };
        assert_eq!(
            probed,
            Some(TRACK_DURATION),
            "TRACK_DURATION must be what the probe reports for {TRACK}"
        );

        let (store, clock) = store_in(dir);
        let mut session = Session::new(state);
        // Registered before the engine ever sends the `Load`, so the
        // `Loaded` it produces carries a token `session` recognizes (M5 §6)
        // — `TestEngine::start_at`'s own auto-generated token would not.
        let request = session
            .register_load(LoadTarget::Legacy, &track_id())
            .unwrap_or_else(|error| panic!("registered: {error:?}"));
        let mut engine = TestEngine::start_idle();
        engine.load_with_resume_as(request, fixture_path(), ResumeIntent::StartAt(start_at));
        engine.send(PlaybackCommand::Play);
        engine.await_state(PlaybackState::Playing);
        let mut rig = Self {
            engine,
            session,
            store,
            clock,
        };
        rig.pump();
        rig
    }

    /// One iteration of `app::run`: drain the events, then sample once.
    fn pump(&mut self) {
        while let Some(event) = self.engine.try_event() {
            let action = self.session.observe(&event, self.clock.sample());
            Self::write(&self.store, action);
        }
        let progress = self.engine.progress();
        let action = self.session.tick(&progress, self.clock.sample());
        Self::write(&self.store, action);
    }

    /// An associated function, not a method: `pump` already holds a mutable
    /// borrow of `session` when it calls this.
    ///
    /// The writer thread's coalescing has its own tests; what matters here is
    /// which snapshot the policy produced, so it is written straight through.
    fn write(store: &StateStore, action: Action) {
        if let Action::Submit { state, .. } = action
            && let Err(error) = store.write(&state)
        {
            panic!("the tempdir must be writable: {error}");
        }
    }

    fn send(&mut self, command: PlaybackCommand) {
        self.engine.send(command);
        // The worker applies a command on one pass and flushes the event it
        // produced on the next (§3), so run the application loop for a while
        // rather than once. Deliberately not `TestEngine::position`, which
        // settles by sending a volume command of its own and consuming the
        // answer — it would eat the very `VolumeChanged` a test is watching
        // for. The harness clock is frozen, so no playback time passes here.
        for _ in 0..SETTLE_PASSES {
            self.pump();
            std::thread::sleep(SETTLE_NAP);
        }
    }

    fn engine_state(&mut self) -> PlaybackState {
        self.engine.state()
    }

    /// `q`: interrupt, join, then the policy's half of the handoff — replay
    /// what the loop never drained and take one forced snapshot.
    fn quit(mut self) {
        let Some(report) = self.engine.shutdown_report() else {
            panic!("the engine was already gone");
        };
        let final_state = self
            .session
            .reconcile_shutdown(&report, self.clock.sample());
        if let Err(error) = self.store.write(&final_state) {
            panic!("the tempdir must be writable: {error}");
        }
    }
}

fn reload(dir: &std::path::Path) -> PersistedState {
    store_in(dir).0.load().state
}

#[test]
fn a_stop_and_a_quit_resume_where_playback_reached() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.quit();

    let state = reload(dir.path());
    let entry = state
        .entry_for(&track_id())
        .expect("an entry for the track");
    assert!(
        entry.position.unwrap() >= Duration::from_secs(2)
            && entry.position.unwrap() < TRACK_DURATION,
        "session 2 must resume near where session 1 stopped: {:?}",
        entry.position
    );
    assert!(!entry.completed);

    let decision = decide_resume(
        state
            .entry_for(&track_id())
            .and_then(|entry| resume_candidate(entry.position, entry.completed)),
        Some(TRACK_DURATION.into()),
    );
    assert!(decision.start_at() >= Duration::from_secs(2));
}

#[test]
fn a_pause_and_a_quit_resume_where_playback_reached() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.send(PlaybackCommand::Pause);
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(entry.position.unwrap() >= Duration::from_secs(2));
}

#[test]
fn a_seek_is_persisted_from_the_canonical_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(1));
    rig.pump();
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(3)));
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position.unwrap() >= Duration::from_secs(3),
        "the seek's landing, taken from Progress rather than from the event: {:?}",
        entry.position
    );
}

/// The ordinary 5 s capture, which no other test here reaches: the rig's clock
/// is frozen, so every entry otherwise comes from a forced trigger or the
/// shutdown snapshot. Driving the interval needs nothing but the clock.
#[test]
fn an_ordinary_capture_lands_once_the_interval_has_passed() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    assert!(
        reload(dir.path()).entry_for(&track_id()).is_none(),
        "nothing is due: no clock time has passed since playback established"
    );

    rig.clock.advance(CAPTURE_INTERVAL);
    let progress = rig.engine.progress();
    let action = rig.session.tick(&progress, rig.clock.sample());
    let Action::Submit { state, urgency } = action else {
        panic!("the elapsed interval must produce a capture");
    };
    assert_eq!(
        urgency,
        Urgency::Ordinary,
        "an interval capture is not a forced one"
    );
    store_in(dir.path())
        .0
        .write(&state)
        .expect("the tempdir is writable");

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position.unwrap() >= Duration::from_secs(2),
        "the capture carries the tick's position: {:?}",
        entry.position
    );
    rig.quit();
}

/// play → stop → seek while stopped → quit. The sequence D7 and D17 exist for.
#[test]
fn a_stopped_seek_target_outlives_the_quit() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position.unwrap() < Duration::from_secs(2),
        "the stored target, not the pre-seek position the engine still reports: {:?}",
        entry.position
    );
}

/// The same sequence with a `q` that gives the event no time to be drained.
#[test]
fn a_stopped_seek_target_outlives_a_quit_that_races_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);

    // No pump between the seek and the quit: exactly what pressing `←` and then
    // `q` inside one poll window does. The wait is for the command to be taken,
    // not for its event — `q` does not wait either, but a command the worker
    // never read is not a lost event, it is a test asking the wrong question.
    rig.engine
        .send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.engine.await_commands_taken();
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position.unwrap() < Duration::from_secs(2),
        "the SeekTargetStored is not the application's to lose: {:?}",
        entry.position
    );
}

/// stop → seek to 1 s → Home → play → quit. `restart()` discards the target.
#[test]
fn a_restart_after_a_stopped_seek_persists_where_it_restarted() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.send(PlaybackCommand::Restart);
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position.unwrap() >= Duration::from_secs(2),
        "the restarted playback's position, not the target the restart threw away: {:?}",
        entry.position
    );
}

#[test]
fn a_finished_track_is_completed_and_reopens_at_zero_with_its_position_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_to_end();
    rig.pump();
    rig.quit();

    let state = reload(dir.path());
    let entry = state.entry_for(&track_id()).cloned().expect("an entry");
    assert!(entry.completed);
    assert!(
        entry.position.unwrap() > Duration::from_secs(4),
        "D1 retains it: {:?}",
        entry.position
    );

    let decision = decide_resume(
        state
            .entry_for(&track_id())
            .and_then(|entry| resume_candidate(entry.position, entry.completed)),
        Some(TRACK_DURATION.into()),
    );
    assert_eq!(
        decision.start_at(),
        Duration::ZERO,
        "and reopening starts at zero"
    );
}

#[test]
fn a_completed_entry_survives_a_launch_whose_device_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_to_end();
    rig.pump();
    rig.quit();
    let kept = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry");
    assert!(kept.completed);

    let after = relaunch_onto_a_refusing_device(dir.path());
    assert_eq!(
        after.position, kept.position,
        "D20: nothing established, so nothing overwrites the position D1 retains"
    );
    assert!(after.completed);
}

/// The other §11 row that opens at zero while keeping what it has: a stored
/// position past the end of the media, as a hand-edited or renamed-media file
/// would carry. D20 must retain it for the same reason it retains a completed
/// one — nothing established, so there is no validated position to write.
#[test]
fn a_position_past_the_end_survives_a_launch_whose_device_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = store_in(dir.path());
    let mut stale = PersistedState::default();
    stale.record(
        &PlaybackCheckpoint {
            media: track_id(),
            position: Duration::from_secs(600),
            updated_at: clock.sample().wall,
        },
        false,
    );
    store.write(&stale).expect("the tempdir is writable");
    let kept = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("the stale entry");
    assert_eq!(
        decide_resume(
            resume_candidate(kept.position, kept.completed),
            Some(TRACK_DURATION.into())
        )
        .start_at(),
        Duration::ZERO,
        "§11 opens a position past the end at zero"
    );

    let after = relaunch_onto_a_refusing_device(dir.path());
    assert_eq!(
        after.position, kept.position,
        "D20: nothing established, so nothing overwrites the retained position"
    );
    assert!(!after.completed);
}

/// Session 2 for the two §11 rows that open at zero. §11 starts such an entry
/// at zero, `load()` emits `Loaded` before it opens the device, and this device
/// offers six channels — which negotiation refuses, after the `Loaded` has
/// already gone out. The application therefore knows the media and reports a
/// position of zero, and playback never happened.
///
/// Deliberately not the rig: `TestEngine` always reaches `Playing`, which
/// establishes, so it cannot stage this at all.
fn relaunch_onto_a_refusing_device(dir: &std::path::Path) -> PersistedCheckpoint {
    let (store, clock) = store_in(dir);
    let mut session = Session::new(reload(dir));
    // `failed_device_session` is not `Session`-aware and always sends its
    // `Load` under token 1 (`tests/support/mod.rs`); registering here first
    // and doing nothing else with this fresh session hands out that same
    // token, so the `Loaded` it replays below is genuinely adopted, exactly
    // as it would be in production.
    let _ = session
        .register_load(LoadTarget::Legacy, &track_id())
        .unwrap_or_else(|error| panic!("registered: {error:?}"));

    let report = support::failed_device_session(TRACK, 6, Duration::ZERO);
    let final_state = session.reconcile_shutdown(&report, clock.sample());
    if let Err(error) = store.write(&final_state) {
        panic!("the tempdir must be writable: {error}");
    }

    match reload(dir).entry_for(&track_id()).cloned() {
        Some(entry) => entry,
        None => panic!("the entry the relaunch must not have dropped is gone"),
    }
}

/// The value's half of §11's volume restore. The ordering half — `SetVolume`
/// before `Load`, so the restored level is in force from the first buffer — is
/// pinned on the sequence `app::run` issues, in `src/app.rs`: the harness
/// records the engine's events rather than the commands sent to it, so the
/// order of two commands is not observable from here.
#[test]
fn volume_survives_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    rig.quit();

    let state = reload(dir.path());
    assert_eq!(state.volume(), Volume::new(0.25));
    assert_eq!(state.current_media(), Some(&track_id()));
}

#[test]
fn a_position_past_the_end_is_refused_as_a_start() {
    // A stale file, as a hand-edited or renamed-media one would be.
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = store_in(dir.path());
    let mut state = PersistedState::default();
    state.record(
        &PlaybackCheckpoint {
            media: track_id(),
            position: Duration::from_secs(600),
            updated_at: clock.sample().wall,
        },
        false,
    );
    store.write(&state).unwrap();

    let reloaded = reload(dir.path());
    let decision = decide_resume(
        reloaded
            .entry_for(&track_id())
            .and_then(|entry| resume_candidate(entry.position, entry.completed)),
        Some(TRACK_DURATION.into()),
    );
    assert_eq!(decision.start_at(), Duration::ZERO);

    // And the engine can be started from that decision without complaint.
    let mut rig = Rig::open_at(dir.path(), reloaded, decision.start_at());
    assert_eq!(rig.engine_state(), PlaybackState::Playing);
    rig.quit();
}

// ------------------------------------------- restart preference (§4.3, R8)

/// Both of a checkpoint's locations survive a real write-and-reload round
/// trip and still compose the way `restart_preference` (Task 6) promises:
/// the estimate wins as the target, and the established position that was
/// also on record comes back as the fallback — neither field clobbers the
/// other on the way through the store, which is the concern this file's
/// other tests exist to catch and a pure unit test on `resume.rs` alone
/// could not.
#[test]
fn a_stored_estimate_and_its_established_fallback_both_survive_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = store_in(dir.path());
    let mut state = PersistedState::default();
    state.record(
        &PlaybackCheckpoint {
            media: track_id(),
            position: Duration::from_secs(40),
            updated_at: clock.sample().wall,
        },
        false,
    );
    state.record_estimated(
        track_id(),
        Duration::from_secs(97),
        clock.sample().wall,
        false,
    );
    store.write(&state).unwrap();

    let reloaded = reload(dir.path());
    let entry = match reloaded.entry_for(&track_id()) {
        Some(entry) => entry,
        None => panic!("the entry must survive the reload"),
    };
    assert_eq!(
        restart_preference(entry.position, entry.estimated),
        Some(RestartPreference {
            target: Duration::from_secs(97),
            established: Some(Duration::from_secs(40)),
        })
    );
}

/// R8, round-tripped: an entry that only ever carried an estimate must not
/// grow an established position merely by passing through the store — the
/// reload must still report `established: None`, not a fabricated zero.
#[test]
fn a_stored_estimate_with_no_established_position_reports_none_for_it_after_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = store_in(dir.path());
    let mut state = PersistedState::default();
    state.record_estimated(
        track_id(),
        Duration::from_secs(97),
        clock.sample().wall,
        false,
    );
    store.write(&state).unwrap();

    let reloaded = reload(dir.path());
    let entry = match reloaded.entry_for(&track_id()) {
        Some(entry) => entry,
        None => panic!("the entry must survive the reload"),
    };
    assert_eq!(
        entry.position, None,
        "no established position was ever recorded"
    );
    assert_eq!(
        restart_preference(entry.position, entry.estimated),
        Some(RestartPreference {
            target: Duration::from_secs(97),
            established: None,
        })
    );
}

// -------------------------------------------------------------- upgrade (R3)

fn loaded_fresh_for(
    session: &mut Session,
    session_rev: u64,
    media: &MediaId,
    position: Duration,
) -> PlaybackEvent {
    let request = session
        .register_load(LoadTarget::Legacy, media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    PlaybackEvent::Loaded {
        session_rev,
        request,
        media: media.clone(),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position,
        disposition: StartDisposition::Fresh,
    }
}

fn state_changed_to(session_rev: u64, state: PlaybackState) -> PlaybackEvent {
    PlaybackEvent::StateChanged {
        session_rev,
        state,
        request: None,
    }
}

fn established_progress(
    session: &Session,
    session_rev: u64,
    media: &MediaId,
    secs: u64,
) -> Progress {
    Progress {
        session_rev,
        media: Some(media.clone()),
        position: Duration::from_secs(secs),
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: session.adopted().map(|adopted| adopted.request),
    }
}

/// R3: `tests/persistence_store.rs`'s `a_v1_file_is_accepted_and_normalised_to_v2`
/// and `the_upgrade_cycle_writes_v2_and_survives_a_reload` prove the migration
/// at the `StateStore` level directly. What they cannot show is the one hop
/// `open_persistence` (`engine.rs`) actually takes at every real launch: a
/// `Session` built straight from the migrated `LoadOutcome`, writing back
/// through the same store. This is that hop, proven end to end — a v1 file,
/// read and written through a real `Session`, still ends up v2 on disk with
/// both the old entry and a brand new one intact, and the store never
/// stopped reporting itself writable.
///
/// Ablation: an `open_persistence` that gated writing on the file's
/// *original* version rather than `LoadOutcome::writable` (the value the
/// migration itself already resolved) would make every assertion below fail
/// together — nothing but the untouched `a` entry would ever reach disk,
/// since production would have picked `DisabledSink` for a session that
/// started against a v1 file.
#[test]
fn a_session_opened_on_a_v1_file_keeps_persisting_as_v2_with_every_entry_intact() {
    let dir = tempfile::tempdir().unwrap();
    let v1 = br#"{
        "schema_version": 1,
        "current_media": "local:/music/a.flac",
        "volume": 0.6,
        "checkpoints": {
            "local:/music/a.flac": {
                "position": { "secs": 42, "nanos": 0 },
                "completed": false,
                "touch_seq": 7,
                "updated_at": "1970-01-01T00:00:00Z"
            }
        }
    }"#;
    std::fs::write(dir.path().join("state.json"), v1).unwrap();

    let (store, clock) = store_in(dir.path());
    let outcome = store.load();
    assert!(
        outcome.writable,
        "a v1 file that loads unwritable is the regression this row exists to catch"
    );
    let a = media("a");
    let b = media("b");
    let mut session = Session::new(outcome.state);

    // Drive a second, unrelated media through the real `Session` — the
    // production pairing `open_persistence` builds, not a direct
    // `store.write` the way `persistence_store.rs` proves the migration.
    let event = loaded_fresh_for(&mut session, 1, &b, Duration::ZERO);
    let _ = session.observe(&event, clock.sample());
    let _ = session.observe(&state_changed_to(1, PlaybackState::Playing), clock.sample());
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    match session.tick(&established_progress(&session, 1, &b, 15), clock.sample()) {
        Action::Submit { state, .. } => store.write(&state).unwrap(),
        Action::None => panic!("the interval capture must have produced a write"),
    }

    let bytes = std::fs::read(dir.path().join("state.json")).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        raw["schema_version"], 4,
        "the file on disk must claim the current schema once a Session has written through it: {raw}"
    );

    let reloaded = reload(dir.path());
    assert_eq!(reloaded.schema_version(), SCHEMA_VERSION);
    assert_eq!(
        reloaded.entry_for(&a).and_then(|entry| entry.position),
        Some(Duration::from_secs(42)),
        "the v1 entry must survive the upgrade untouched"
    );
    assert_eq!(
        reloaded.entry_for(&b).and_then(|entry| entry.position),
        Some(Duration::from_secs(15)),
        "the newly recorded entry must survive the round trip"
    );
    assert_eq!(reloaded.len(), 2, "no entry was lost across the upgrade");

    // One further reload, through the store alone: the file is genuinely v2
    // now, not merely accepted once and forgotten.
    assert!(store.load().writable);
}
