//! §12's acceptance evidence for H4 and H16's cross-process half: resume and
//! checkpoint protection carried through a real `StateStore` file on disk,
//! across independent `Session`/`TestEngine` pairs that share no Rust state
//! with each other - the same boundary a second launch of the application
//! would cross. Every server is `127.0.0.1:<ephemeral>`; every persistence
//! test here uses a `tempfile::TempDir`, never the platform state path.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::id::{MediaId, NormalizedUrl};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::command::{Admission, ResumeIntent};
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::playback::state::PlaybackState;
use tenuto::resume::resume_candidate;
use tenuto::session::{Action, LoadTarget, Session};

use support::server::{Script, TestServer};
use support::{Loaded, TestEngine};

fn media_for(url: &str) -> MediaId {
    match NormalizedUrl::parse(url) {
        Ok(normalized) => MediaId::RemoteUrl(normalized),
        Err(error) => panic!("test URL {url:?} must normalize: {error}"),
    }
}

/// The same minimal `Session` + `StateStore` rig `http_playback.rs` keeps its
/// own copy of, staged like `resume_contract.rs`'s `Rig` - local rather than
/// shared, matching how that file keeps its own `Rig` private too.
struct RemoteRig {
    engine: TestEngine,
    session: Session,
    store: StateStore,
    clock: Arc<FakeClock>,
}

impl RemoteRig {
    fn store_in(dir: &Path) -> (StateStore, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::new());
        let injected: Arc<dyn Clock> = clock.clone();
        (StateStore::new(dir.join("state.json"), injected), clock)
    }

    fn pump(&mut self) {
        while let Some(event) = self.engine.try_event() {
            let action = self.session.observe(&event, self.clock.sample());
            Self::write(&self.store, action);
        }
        let progress = self.engine.progress();
        let action = self.session.tick(&progress, self.clock.sample());
        Self::write(&self.store, action);
    }

    fn write(store: &StateStore, action: Action) {
        if let Action::Submit { state, .. } = action
            && let Err(error) = store.write(&state)
        {
            panic!("the tempdir must be writable: {error}");
        }
    }

    /// Waits for the `Loaded` event a fresh `load_remote_with_resume` just
    /// requested, and feeds *this* `session` every event on the way there —
    /// `Loaded` included.
    ///
    /// Deliberately not `TestEngine::await_loaded`: that method removes the
    /// matching event from the engine's own inbox once it finds it, so a
    /// `Session` that only starts observing afterward never learns
    /// `current_media` from it (`on_loaded`, `src/session.rs`, is the only
    /// place that sets it). Every write this session could ever make is
    /// gated on `current_media` being `Some` (`Session::tick`'s first
    /// guard), so a rig built the other way would silently never write
    /// anything again — an `after` checkpoint that matches `before` would
    /// look like proof of persistence (or of protection) when it is really
    /// proof that persistence never ran at all. This was exactly the defect
    /// found in this file's two tests before this fix: `engine2/3
    /// .await_loaded()` was called before the corresponding `RemoteRig`
    /// existed, so `rig2`/`rig3`'s `pump`/`quit` calls could never have
    /// written anything regardless of what checkpoint protection did.
    fn pump_until_loaded(&mut self) -> Loaded {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            while let Some(event) = self.engine.try_event() {
                let loaded = match &event {
                    PlaybackEvent::Loaded {
                        position,
                        disposition,
                        capabilities,
                        ..
                    } => Some(Loaded {
                        position: *position,
                        disposition: *disposition,
                        capabilities: *capabilities,
                    }),
                    _ => None,
                };
                let action = self.session.observe(&event, self.clock.sample());
                Self::write(&self.store, action);
                if let Some(loaded) = loaded {
                    return loaded;
                }
            }
            if Instant::now() >= deadline {
                panic!("no Loaded event arrived within 20s");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Interrupt, join, and the policy's half of the handoff - the same
    /// sequence `app::run`'s `q` performs, and every session below crosses
    /// exactly once, so the next one starts from disk alone.
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

fn reload(dir: &Path) -> PersistedState {
    RemoteRig::store_in(dir).0.load().state
}

#[test]
fn a_second_session_resumes_from_the_flushed_checkpoint() {
    // H4: this session hands the engine the flushed checkpoint's own
    // `ResumeIntent::Candidate` directly, and a redirect encountered on the
    // way does not change the identity the checkpoint is filed under.
    // Since Task 8, a real second process opening this same plain URL
    // would start at zero instead — `resume_intent_for` no longer resumes
    // `MediaId::RemoteUrl` — so this pins the checkpoint/identity plumbing
    // itself, not what an actual relaunch decides today.
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let url = {
        let probe = TestServer::start(Script::from_fixture("sine-5s.flac"));
        let url = probe.url("/audio");
        probe.shutdown();
        url
    };
    let media = media_for(&url);
    let port = url::Url::parse(&url)
        .unwrap_or_else(|error| panic!("test URL must parse: {error}"))
        .port()
        .unwrap_or_else(|| panic!("test URL must carry a port"));

    // Session 1: plays past zero, then quits. A positive, incomplete
    // checkpoint lands in the store.
    let (store, clock) = RemoteRig::store_in(dir.path());
    let server1 = TestServer::start_on(port, Script::from_fixture("sine-5s.flac"));
    let mut session1 = Session::new(PersistedState::default());
    let request1 = session1
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine1 = TestEngine::start_idle();
    engine1.load_remote_as(request1, &url, ResumeIntent::StartAt(Duration::ZERO));
    assert_eq!(engine1.handle().submit_play(), Admission::Accepted);
    engine1.await_state(PlaybackState::Playing);
    engine1.play_for(Duration::from_secs(2));
    let mut rig1 = RemoteRig {
        engine: engine1,
        session: session1,
        store,
        clock,
    };
    rig1.pump();
    rig1.quit();
    server1.shutdown();

    let flushed = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("session 1 must have left a checkpoint"));
    assert!(flushed.position.unwrap() >= Duration::from_secs(1) && !flushed.completed);

    // Session 2: a fresh `Session`/`TestEngine` pair that only knows the
    // reloaded file - no Rust object here was ever touched by session 1.
    // The same original URL now redirects once before the body: the
    // `MediaId` the checkpoint is resolved under must still be the one
    // built from what was typed, not the hop actually served from.
    let server2 =
        TestServer::start_on(port, Script::from_fixture("sine-5s.flac").redirect_chain(1));
    let (store2, clock2) = RemoteRig::store_in(dir.path());
    let mut session2 = Session::new(reload(dir.path()));
    let request2 = session2
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine2 = TestEngine::start_idle();
    engine2.load_remote_as(
        request2,
        &url,
        ResumeIntent::Candidate(
            resume_candidate(flushed.position, flushed.completed)
                .expect("session 1's checkpoint must carry an established position"),
        ),
    );
    let mut rig2 = RemoteRig {
        engine: engine2,
        session: session2,
        store: store2,
        clock: clock2,
    };
    // `pump_until_loaded`, not `TestEngine::await_loaded`: the latter would
    // consume the `Loaded` event before `rig2.session` ever saw it, leaving
    // `current_media` unset and every write below silently inert regardless
    // of whether resume actually worked.
    let loaded = rig2.pump_until_loaded();
    match loaded.disposition {
        StartDisposition::Resumed => {}
        other => panic!("expected Resumed, got {other:?}"),
    }
    assert!(
        loaded.position >= Duration::from_secs(1),
        "session 2 did not resume near where session 1 stopped: {:?}",
        loaded.position
    );

    let requests = server2.requests();
    assert!(
        requests.iter().any(|r| r.path == "/audio"),
        "the redirect's first hop never reached the server: {requests:?}"
    );

    // Establish and play on a little, then quit — proving session 2's own
    // continued playback actually reaches the file through this rig, not
    // only that it read the resumed position back correctly. Without this,
    // the trailing assertion below would hold even if `rig2` never wrote
    // anything at all (exactly the defect `pump_until_loaded`'s doc comment
    // describes), since it would just be restating `flushed.completed`.
    assert_eq!(rig2.engine.handle().submit_play(), Admission::Accepted);
    rig2.engine.await_state(PlaybackState::Playing);
    // `play_for` waits for the *absolute* position to reach its argument,
    // not for a span of further playback — session 2 resumed past 300ms
    // already, so the target has to be past where it resumed, not a bare
    // 300ms, or this returns immediately without advancing anything.
    rig2.engine
        .play_for(loaded.position + Duration::from_millis(300));
    rig2.pump();
    rig2.quit();

    let after = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("the entry must still be filed under the original identity"));
    assert!(!after.completed);
    assert!(
        after.position.unwrap_or(Duration::ZERO)
            >= flushed.position.unwrap() + Duration::from_millis(100),
        "session 2's own continued playback never reached the file: flushed at {:?}, after at {:?}",
        flushed.position,
        after.position
    );

    server2.shutdown();
}

#[test]
fn the_protection_survives_a_process_boundary() {
    // H16's cross-process half: session_policy.rs's six protection tests
    // drive `Session` directly against fabricated events and a
    // `PersistedState` held in memory. What those cannot show is that the
    // same protection survives being written to and reloaded from an actual
    // file - a real `StateStore` in a `tempfile::TempDir`, crossed by two
    // `Session`/`TestEngine` pairs that share no Rust state with each other,
    // is what stands in for the process boundary a real restart crosses.
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let url = {
        let probe = TestServer::start(Script::from_fixture("sine-5s.flac"));
        let url = probe.url("/audio");
        probe.shutdown();
        url
    };
    let media = media_for(&url);
    let port = url::Url::parse(&url)
        .unwrap_or_else(|error| panic!("test URL must parse: {error}"))
        .port()
        .unwrap_or_else(|| panic!("test URL must carry a port"));

    // Session 1: range-capable, plays past zero, then quits.
    let (store, clock) = RemoteRig::store_in(dir.path());
    let server1 = TestServer::start_on(port, Script::from_fixture("sine-5s.flac"));
    let mut session1 = Session::new(PersistedState::default());
    let request1 = session1
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine1 = TestEngine::start_idle();
    engine1.load_remote_as(request1, &url, ResumeIntent::StartAt(Duration::ZERO));
    assert_eq!(engine1.handle().submit_play(), Admission::Accepted);
    engine1.await_state(PlaybackState::Playing);
    engine1.play_for(Duration::from_secs(2));
    let mut rig1 = RemoteRig {
        engine: engine1,
        session: session1,
        store,
        clock,
    };
    rig1.pump();
    rig1.quit();
    server1.shutdown();

    let original = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("session 1 must have left a checkpoint"));

    // Session 2: a fresh pair, on a range-less server behind the same URL.
    // Restoration is unavailable, so the entry is protected against
    // whatever zero-start progress this session records for itself.
    let server2 = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").without_ranges());
    let (store2, clock2) = RemoteRig::store_in(dir.path());
    let mut session2 = Session::new(reload(dir.path()));
    let request2 = session2
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine2 = TestEngine::start_idle();
    engine2.load_remote_as(
        request2,
        &url,
        ResumeIntent::Candidate(
            resume_candidate(original.position, original.completed)
                .expect("session 1's checkpoint must carry an established position"),
        ),
    );
    let mut rig2 = RemoteRig {
        engine: engine2,
        session: session2,
        store: store2,
        clock: clock2,
    };
    // `pump_until_loaded`, not `TestEngine::await_loaded`: see its doc
    // comment — this is the fix for the exact defect that let this test
    // pass with checkpoint protection entirely removed.
    let loaded = rig2.pump_until_loaded();
    match loaded.disposition {
        StartDisposition::ResumeUnavailable { retained } => {
            assert_eq!(retained, original.position.unwrap());
        }
        other => panic!("expected ResumeUnavailable, got {other:?}"),
    }
    assert_eq!(rig2.engine.handle().submit_play(), Admission::Accepted);
    rig2.engine.await_state(PlaybackState::Playing);
    rig2.engine.play_for(Duration::from_millis(300));
    rig2.pump();
    rig2.quit();
    server2.shutdown();

    let after_session_2 = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("the protected entry must still exist"));
    assert_eq!(
        after_session_2.position, original.position,
        "session 2 overwrote the protected entry with its own zero-start progress"
    );
    assert!(!after_session_2.completed);

    // Session 3: a third, independent pair - the protection just proven
    // survived one boundary must still hold crossing a second one, rather
    // than having been a one-shot fact only session 2's own in-memory
    // `Session` remembered. Still range-less, so restoration is still
    // unavailable and the fallback still applies.
    let server3 = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").without_ranges());
    let (store3, clock3) = RemoteRig::store_in(dir.path());
    let mut session3 = Session::new(reload(dir.path()));
    let request3 = session3
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine3 = TestEngine::start_idle();
    engine3.load_remote_as(
        request3,
        &url,
        ResumeIntent::Candidate(
            resume_candidate(after_session_2.position, after_session_2.completed)
                .expect("session 2's checkpoint must carry an established position"),
        ),
    );
    let mut rig3 = RemoteRig {
        engine: engine3,
        session: session3,
        store: store3,
        clock: clock3,
    };
    // `pump_until_loaded`, not `TestEngine::await_loaded` — same fix as
    // session 2, applied here too.
    let loaded3 = rig3.pump_until_loaded();
    match loaded3.disposition {
        StartDisposition::ResumeUnavailable { retained } => {
            assert_eq!(retained, original.position.unwrap());
        }
        other => panic!("expected ResumeUnavailable, got {other:?}"),
    }
    // Establish and actually play, the same as session 2 — without this,
    // session 3's `Session` never establishes, and D20's rule ("nothing
    // established, so nothing overwrites the position") is what preserves
    // the checkpoint below, not the protection this test exists to prove
    // survives a second boundary.
    assert_eq!(rig3.engine.handle().submit_play(), Admission::Accepted);
    rig3.engine.await_state(PlaybackState::Playing);
    rig3.engine.play_for(Duration::from_millis(300));
    rig3.pump();
    rig3.quit();
    server3.shutdown();

    let after_session_3 = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("the protected entry must still exist after a second boundary"));
    assert_eq!(
        after_session_3.position, original.position,
        "the protected entry did not survive a second process boundary"
    );
    assert!(!after_session_3.completed);
}
