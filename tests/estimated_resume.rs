//! Task 7's acceptance evidence for §6's cross-process rows: the estimated
//! restart carried through a real `StateStore` file on disk, across
//! independent `Session`/`TestEngine` pairs that share no Rust state with
//! each other — the same boundary a second launch of the application would
//! cross (the same technique `tests/resume_contract.rs`'s `Rig` and
//! `tests/http_resume.rs`'s `RemoteRig` already use for the established
//! side). Every server is `127.0.0.1:<ephemeral>`; every persistence test
//! here uses a `tempfile::TempDir`, never the platform state path.
//!
//! `tests/estimated_seek.rs` and `tests/session_policy.rs` already prove the
//! mechanism this file exercises end to end: `restart_preference` (Task 6)
//! and `ResumeIntent::EstimatedCandidate` (`src/playback/engine.rs`) at
//! their own boundaries. What none of them can show is that the whole
//! chain survives an actual write-to-disk and reload — a real second
//! process, not a value passed straight from one function to the next in
//! the same test. Since Task 8, `resume_intent_for` no longer drives this
//! for this file's plain-URL fixture — a fresh load of a local file or
//! plain URL starts from zero — so each "relaunch" below hands
//! `restart_preference`'s own output to the engine directly, the way a
//! real relaunch of a podcast episode still would.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenuto::clock::{Clock, FakeClock};
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::media::id::{MediaId, NormalizedUrl};
use tenuto::media::source::SourceLocation;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::resume::{restart_preference, resume_candidate};
use tenuto::session::{Action, LoadTarget, Session};
use url::Url;

use support::server::{Script, TestServer};
use support::{Loaded, TestEngine};

const NOXING: &str = "sine-long-noxing.mp3"; // 600s, CBR, no Xing/Info/VBRI.
const VBR_NOXING: &str = "sine-long-vbr-noxing.mp3"; // 600s true, ~361s estimated.

fn media_for(url: &str) -> MediaId {
    match NormalizedUrl::parse(url) {
        Ok(normalized) => MediaId::RemoteUrl(normalized),
        Err(error) => panic!("test URL {url:?} must normalize: {error}"),
    }
}

/// The same minimal `Session` + `StateStore` rig `http_resume.rs` and
/// `resume_contract.rs` each keep their own copy of — local rather than
/// shared, matching how those files keep their own copies too.
struct Rig {
    engine: TestEngine,
    session: Session,
    store: StateStore,
    clock: Arc<FakeClock>,
}

impl Rig {
    fn store_in(dir: &Path) -> (StateStore, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::new());
        let injected: Arc<dyn Clock> = clock.clone();
        (StateStore::new(dir.join("state.json"), injected), clock)
    }

    fn write(store: &StateStore, action: Action) {
        if let Action::Submit { state, .. } = action
            && let Err(error) = store.write(&state)
        {
            panic!("the tempdir must be writable: {error}");
        }
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

    /// Waits for the `Loaded` event a fresh `load_remote_with_resume` just
    /// requested, and feeds *this* `session` every event on the way there —
    /// `Loaded` included.
    ///
    /// Deliberately not `TestEngine::await_loaded`: that method removes the
    /// matching event from the engine's own inbox once it finds it, so a
    /// `Session` that only starts observing afterward never learns
    /// `current_media` from it (`on_loaded` is the only place that sets it).
    /// Every write this session could ever make is gated on `current_media`
    /// being `Some` (`Session::tick`'s first guard), so a rig built that way
    /// would silently never write anything again — an `after` checkpoint
    /// that matches `before` would look like proof of persistence when it is
    /// really proof that persistence never ran. This is the fix: observe
    /// `Loaded` here, in the same rig that goes on to `pump` and `quit`.
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

    /// Interrupt, join, and the policy's half of the handoff — the same
    /// sequence `app::run`'s `q` performs.
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
    Rig::store_in(dir).0.load().state
}

/// §6: "restart selects `estimated` when present and reports what it kept
/// ... **both** exits in §4.4 clear it ... leaves the established checkpoint
/// intact." The cross-process half of the persistence row: an estimate and
/// its established fallback, each written by a different real process, both
/// surviving on disk through a third process that resumes from the
/// estimate.
///
/// Ablation: an `open_persistence`-equivalent that read `entry.position`
/// instead of `restart_preference`'s output would make session 3 land at the
/// established position instead of the estimate — the `loaded3.position`
/// assertion would fail. An `engine.rs::load` that let a positive
/// `EstimatedCandidate` write straight through `record_current` (ignoring
/// §4.2's routing) would clear or overwrite `after_session_3`'s established
/// position — the final assertion would fail instead.
#[test]
fn a_relaunch_selects_the_estimate_and_leaves_the_established_checkpoint_on_disk() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture(NOXING));
    let url = server.url("/audio.mp3");
    let media = media_for(&url);

    // Session 1: an ordinary, decoder-confirmed play past zero, then quits.
    // A positive, established checkpoint lands in the store — no estimate
    // anywhere yet.
    let (store1, clock1) = Rig::store_in(dir.path());
    let mut session1 = Session::new(PersistedState::default());
    let request1 = session1
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine1 = TestEngine::start_idle();
    engine1.load_remote_as(request1, &url, ResumeIntent::StartAt(Duration::ZERO));
    assert_eq!(engine1.handle().submit_play(), Admission::Accepted);
    engine1.await_state(PlaybackState::Playing);
    engine1.play_for(Duration::from_secs(2));
    let mut rig1 = Rig {
        engine: engine1,
        session: session1,
        store: store1,
        clock: clock1,
    };
    rig1.pump();
    rig1.quit();

    let established = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("session 1 must have left an established checkpoint"));
    assert!(
        established.position.unwrap_or(Duration::ZERO) > Duration::ZERO,
        "sanity: session 1 must have played forward before quitting"
    );
    assert!(!established.completed);
    assert_eq!(
        established.estimated, None,
        "sanity: no estimate exists yet"
    );

    // Session 2: resumes at the established position, then seeks forward —
    // a `Coarse` landing on this no-index MP3 lands `Estimated`
    // (`tests/estimated_seek.rs` proves the mechanism this borrows). Quits
    // without ever re-establishing, so the estimate is the session's last
    // word.
    let (store2, clock2) = Rig::store_in(dir.path());
    let mut session2 = Session::new(reload(dir.path()));
    let request2 = session2
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine2 = TestEngine::start_idle();
    engine2.load_remote_as(
        request2,
        &url,
        ResumeIntent::Candidate(
            resume_candidate(established.position, established.completed)
                .unwrap_or_else(|| panic!("session 1's checkpoint must carry a position")),
        ),
    );
    let mut rig2 = Rig {
        engine: engine2,
        session: session2,
        store: store2,
        clock: clock2,
    };
    let loaded2 = rig2.pump_until_loaded();
    match loaded2.disposition {
        StartDisposition::Resumed => {}
        other => panic!("expected Resumed, got {other:?}"),
    }
    assert_eq!(rig2.engine.handle().submit_play(), Admission::Accepted);
    rig2.engine.await_state(PlaybackState::Playing);
    rig2.engine.play_for(Duration::from_millis(100));
    let seek_target = loaded2.position + Duration::from_secs(120);
    assert_eq!(
        rig2.engine.handle().submit_seek(seek_target),
        Admission::Accepted
    );
    let landed = rig2.engine.await_seek_completed(Duration::from_secs(10));
    assert_eq!(
        landed.provenance,
        PositionProvenance::Estimated,
        "sanity: the seek driving this test must actually land estimated"
    );
    rig2.pump();
    rig2.quit();

    let after_session_2 = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("the entry must survive session 2"));
    assert_eq!(
        after_session_2.position, established.position,
        "an estimated landing must never overwrite the established position"
    );
    assert!(
        after_session_2.estimated.is_some(),
        "the estimated landing is still worth recording"
    );
    assert!(!after_session_2.completed);

    // Session 3: hands the engine `restart_preference`'s own output
    // directly. §4.3 prefers the estimate over the established fallback;
    // since Task 8 a real relaunch of this file's plain-URL fixture would
    // start at zero instead (`resume_intent_for` no longer resumes
    // `MediaId::RemoteUrl`), so this pins the preference and its on-disk
    // survival directly rather than through a real relaunch.
    let preference = restart_preference(after_session_2.position, after_session_2.estimated)
        .unwrap_or_else(|| panic!("an entry carrying both fields must produce a preference"));
    assert_eq!(preference.established, established.position);

    let (store3, clock3) = Rig::store_in(dir.path());
    let mut session3 = Session::new(reload(dir.path()));
    let request3 = session3
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine3 = TestEngine::start_idle();
    engine3.load_remote_as(
        request3,
        &url,
        ResumeIntent::EstimatedCandidate {
            target: preference.target,
            established: preference.established,
        },
    );
    let mut rig3 = Rig {
        engine: engine3,
        session: session3,
        store: store3,
        clock: clock3,
    };
    let loaded3 = rig3.pump_until_loaded();
    assert_eq!(
        Some(loaded3.position),
        after_session_2.estimated,
        "a relaunch must select the estimated location, not the established fallback"
    );
    match loaded3.disposition {
        StartDisposition::ResumedEstimated { established: kept } => {
            assert_eq!(
                kept, established.position,
                "the resume must report exactly what established fallback it kept"
            );
        }
        other => panic!("expected ResumedEstimated, got {other:?}"),
    }

    // Session 3 quits without ever re-establishing either — the checkpoint
    // it leaves behind must still carry the untouched established position.
    rig3.pump();
    rig3.quit();
    server.shutdown();

    let after_session_3 = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("the entry must survive session 3"));
    assert_eq!(
        after_session_3.position, established.position,
        "the established checkpoint must stay intact on disk through the whole cross-process cycle"
    );
}

/// §6/R8: "reporting `established: None` for an estimate-only entry." A
/// media that never had an established position at all — only ever an
/// estimate — persists that estimate through a real quit. Session 2 hands
/// that estimate to the engine directly and resumes from it while
/// reporting no fallback exists, on both the wire (`Loaded.disposition`)
/// and the file underneath it — since Task 8, a real relaunch of this
/// file's plain-URL fixture would start at zero instead.
///
/// Ablation: a `checkpoint_from_progress` that bootstrapped `position` from
/// an estimated sample whenever none existed yet (deleting §4.2's second
/// write rule) would make `never_established.position` read `Some(..)`
/// instead of `None` below, and the disposition would carry
/// `established: Some(..)` instead of `None`.
#[test]
fn a_relaunch_resumes_an_estimate_only_entry_and_reports_no_established_fallback() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    // A fixed port, so the second server below serves the same identity a
    // real relaunch would find at the same address — the same technique
    // `tests/http_resume.rs` uses to cross a process boundary without
    // changing what the URL resolves to.
    let port = {
        let probe = TestServer::start(Script::from_fixture(NOXING));
        let url = probe.url("/audio.mp3");
        probe.shutdown();
        match Url::parse(&url) {
            Ok(parsed) => match parsed.port() {
                Some(port) => port,
                None => panic!("test URL must carry a port"),
            },
            Err(error) => panic!("test URL {url:?} must parse: {error}"),
        }
    };
    let server = TestServer::start_on(port, Script::from_fixture(NOXING));
    let url = server.url("/audio.mp3");
    let media = media_for(&url);

    // Session 1: loads fresh, plays for a moment and seeks forward
    // immediately — landing `Estimated` before the ordinary 5 s interval
    // (frozen here; the rig's `FakeClock` never advances) ever has a chance
    // to fire a plain established capture. `established` becomes true (the
    // seek's own `SeekCompleted` sets it), but every write that follows is
    // routed by `position_provenance`, which the seek also set to
    // `Estimated` — so `position` never gets a value at all.
    let (store1, clock1) = Rig::store_in(dir.path());
    let mut session1 = Session::new(PersistedState::default());
    let request1 = session1
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine1 = TestEngine::start_idle();
    engine1.load_remote_as(request1, &url, ResumeIntent::StartAt(Duration::ZERO));
    assert_eq!(engine1.handle().submit_play(), Admission::Accepted);
    engine1.await_state(PlaybackState::Playing);
    engine1.play_for(Duration::from_millis(100));
    assert_eq!(
        engine1.handle().submit_seek(Duration::from_secs(150)),
        Admission::Accepted
    );
    let landed = engine1.await_seek_completed(Duration::from_secs(10));
    assert_eq!(
        landed.provenance,
        PositionProvenance::Estimated,
        "sanity: the seek driving this test must actually land estimated"
    );
    let mut rig1 = Rig {
        engine: engine1,
        session: session1,
        store: store1,
        clock: clock1,
    };
    rig1.pump();
    rig1.quit();
    server.shutdown();

    let never_established = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("session 1 must have left an estimate-only entry"));
    assert_eq!(
        never_established.position, None,
        "no established position was ever recorded"
    );
    assert!(never_established.estimated.is_some());
    assert!(!never_established.completed);

    // Session 2: hands the engine `restart_preference`'s own output for an
    // entry with no established fallback at all — since Task 8, a real
    // relaunch of this plain-URL fixture would start at zero rather than
    // compute this preference at all.
    let preference = restart_preference(never_established.position, never_established.estimated)
        .unwrap_or_else(|| panic!("an entry carrying only an estimate must still produce one"));
    assert_eq!(preference.established, None);

    let server2 = TestServer::start_on(port, Script::from_fixture(NOXING));
    let url2 = server2.url("/audio.mp3");
    assert_eq!(
        media_for(&url2),
        media,
        "sanity: the second server must serve the same identity"
    );
    let (store2, clock2) = Rig::store_in(dir.path());
    let mut session2 = Session::new(reload(dir.path()));
    let request2 = session2
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine2 = TestEngine::start_idle();
    engine2.load_remote_as(
        request2,
        &url2,
        ResumeIntent::EstimatedCandidate {
            target: preference.target,
            established: preference.established,
        },
    );
    let mut rig2 = Rig {
        engine: engine2,
        session: session2,
        store: store2,
        clock: clock2,
    };
    let loaded2 = rig2.pump_until_loaded();
    assert_eq!(
        Some(loaded2.position),
        never_established.estimated,
        "a relaunch must select the estimated location, not the established fallback"
    );
    match loaded2.disposition {
        StartDisposition::ResumedEstimated { established: None } => {}
        other => panic!("expected ResumedEstimated{{established: None}}, got {other:?}"),
    }
    rig2.quit();
    server2.shutdown();
}

/// §6/§5.5: "Refused resume preserves the checkpoint, through real
/// persistence." `tests/estimated_seek.rs::
/// a_launch_resume_past_an_estimated_ceiling_preserves_the_checkpoint`
/// proves this at the `Worker::position` level — the right boundary for
/// Task 4's own scope — but never touches a file. This is the same
/// retained limitation (§5.5) carried one boundary further: a checkpoint at
/// 400 s (real audio; the true file is 600 s), stored on disk, refused by
/// `MpaReader::seek`'s own `max_ts` bound (derived from symphonia's ~361 s
/// estimate, checked at `demuxer.rs:268-272` — *before* the `SeekMode`
/// dispatch at `:292-296`, so no seek mode this project could choose would
/// avoid it), across an actual relaunch. Not a bug to fix: this pins that a
/// later change cannot quietly turn the refusal into a silent reset, which
/// would be data loss wearing the costume of a cleanup.
///
/// Ablation: an on-disk write following the `Failed` state (there is none
/// today) would make the final `entry_for` assertion read something other
/// than 400 s.
#[test]
fn a_relaunch_refused_past_an_estimated_ceiling_leaves_the_stored_checkpoint_untouched() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture(VBR_NOXING));
    let url = server.url("/audio.mp3");
    let media = media_for(&url);
    let target = Duration::from_secs(400);

    // Seed the file directly with an established checkpoint at 400s — real
    // audio, on a 600s file symphonia will estimate at ~361s once opened.
    let (store, clock) = Rig::store_in(dir.path());
    let mut seed = PersistedState::default();
    seed.record(
        &PlaybackCheckpoint {
            media: media.clone(),
            position: target,
            updated_at: clock.sample().wall,
        },
        false,
    );
    if let Err(error) = store.write(&seed) {
        panic!("the tempdir must be writable: {error}");
    }

    let before = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("the seeded checkpoint must be on disk"));
    assert_eq!(before.position, Some(target));

    // Driven through the same `Rig` (real `Session`, real `StateStore`)
    // every other test in this file uses — not a bare engine with no
    // persistence wired up at all — so whatever the failure path actually
    // emits gets a genuine chance to reach the file. Since Task 8, a real
    // relaunch of this file's plain-URL fixture would start at zero rather
    // than attempt this resume at all; this pins the engine's own refusal
    // and persistence path directly instead.
    let (store2, clock2) = Rig::store_in(dir.path());
    let mut session = Session::new(reload(dir.path()));
    let request = session
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine = TestEngine::start_idle();
    let service = match HttpService::spawn(Limits::brisk()) {
        Ok(service) => service,
        Err(error) => panic!("the test HttpService must start: {error}"),
    };
    engine.handle().set_http(Some(service));
    let parsed = match Url::parse(&url) {
        Ok(parsed) => parsed,
        Err(error) => panic!("test URL {url:?} must parse: {error}"),
    };
    engine.send(PlaybackCommand::Load {
        request,
        media: media.clone(),
        source: SourceLocation::Http(parsed),
        resume: ResumeIntent::Candidate(
            resume_candidate(before.position, before.completed)
                .unwrap_or_else(|| panic!("the seeded checkpoint must carry a position")),
        ),
    });
    engine.await_state(PlaybackState::Failed);
    assert_eq!(
        engine.progress().position,
        target,
        "a refused resume must not discard the stored checkpoint from the engine's own memory"
    );

    let mut rig = Rig {
        engine,
        session,
        store: store2,
        clock: clock2,
    };
    rig.pump();
    rig.quit();
    server.shutdown();

    let after = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("the entry must still be on disk"));
    assert_eq!(
        after.position,
        Some(target),
        "the on-disk checkpoint must survive a refused resume unchanged, through real persistence"
    );
}
