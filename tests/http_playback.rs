//! §12's acceptance evidence for H1, H2, H5 and H13, plus the M4A opening
//! evidence §12's closing paragraph asks for (H17's first half — a
//! byte-seekable source whose demuxer cannot seek — is not discharged here;
//! see the task 13 report for why no fixture in this repository reaches it).
//! Every server is `127.0.0.1:<ephemeral>` and every engine runs over
//! `TestOutput` — no public network, no real device.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenuto::clock::{Clock, FakeClock};
use tenuto::http::limits::Limits;
use tenuto::media::capabilities::SeekSupport;
use tenuto::media::id::{MediaId, NormalizedUrl};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::playback::state::PlaybackState;
use tenuto::resume::resume_candidate;
use tenuto::session::{Action, LoadTarget, Session};

use support::TestEngine;
use support::server::{Script, TestServer};

fn media_for(url: &str) -> MediaId {
    match NormalizedUrl::parse(url) {
        Ok(normalized) => MediaId::RemoteUrl(normalized),
        Err(error) => panic!("test URL {url:?} must normalize: {error}"),
    }
}

/// H1/H5's shared minimal `Session` + `StateStore` rig for a remote session,
/// the same "drain events, then sample once" ordering `resume_contract.rs`
/// pins against `app::run` — this file needs its own copy because the two
/// suites never share a `mod support::rig`, matching how `resume_contract.rs`
/// keeps its own `Rig` local rather than exporting one.
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
    RemoteRig::store_in(dir).0.load().state
}

#[test]
fn mp3_flac_and_wav_play_before_the_body_completes() {
    // H1: the whole point of finite HTTP media is that playback starts
    // before the transfer finishes, for every format M1 promised. Paced
    // rather than hard-stalled: opening any of these formats over HTTP reads
    // its body more than once before `Paused` is even reached (an initial
    // open, a small probe near the end, and a reopen used for the ongoing
    // decode - see H13's occupancy test for the detail), so a fixed-byte
    // stall has no threshold that is both past what every format's opening
    // needs and short of every format's total size. Pacing the whole
    // transfer sidesteps that: slow enough that opening alone does not
    // finish it, `bytes_written` staying under the full body size a little
    // further into playback is then a direct, format-independent proof that
    // the transfer is not yet complete.
    for (fixture, path) in [
        ("sine-5s.mp3", "/audio.mp3"),
        ("sine-5s.flac", "/audio.flac"),
        ("sine-5s.wav", "/audio.wav"),
    ] {
        let body_len = std::fs::metadata(support::fixture_path(fixture))
            .unwrap_or_else(|error| panic!("{fixture}: must exist: {error}"))
            .len() as usize;
        // Paced by the body's own size, so every format's transfer takes at
        // least four seconds however small the file is. A fixed piece size
        // gave the 81 KB MP3 well under a second, and `bytes_written` counts
        // every connection's bytes, so a loaded CI runner that was slow to
        // reach `Playing` saw the total pass the body's length.
        let server = TestServer::start(
            Script::from_fixture(fixture).trickle(body_len / 400, Duration::from_millis(10)),
        );
        let mut engine = TestEngine::start_idle();
        // Opening reads the paced body too, so `brisk`'s 2 s open deadline is
        // about half the transfer: a runner whose sleeps overshoot (the macOS
        // leg) failed every load. What this test measures starts after
        // opening, so opening gets all the time it needs.
        engine.load_remote_with_resume_and_limits(
            &server.url(path),
            ResumeIntent::StartAt(Duration::ZERO),
            Limits {
                open: Duration::from_secs(30),
                ..Limits::brisk()
            },
        );
        assert_eq!(
            engine.handle().submit(PlaybackCommand::Play),
            Admission::Accepted
        );
        engine.await_state(PlaybackState::Playing);
        engine.play_for(Duration::from_millis(150));

        assert!(
            server.bytes_written() < body_len,
            "{fixture}: the whole body ({body_len} bytes) had already arrived; the \
             pacing was not slow enough to observe playback starting before it finished"
        );
        assert!(
            engine.captured_is_audible(),
            "{fixture}: nothing reached the test output before the body finished"
        );
        assert!(
            engine.progress().position >= Duration::from_millis(100),
            "{fixture}: the position did not advance while playing: {:?}",
            engine.progress().position
        );

        engine.finish();
        server.shutdown();
    }
}

#[test]
fn seeking_installs_the_media_position_and_requests_that_byte() {
    // H2: a seek's landing is both a local fact (the engine's own position)
    // and a remote one (the byte range the server actually saw) — forward
    // and backward alike, so a backward seek is not secretly served from
    // whatever the forward one already buffered.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_secs(4))),
        Admission::Accepted
    );
    let forward_landed = engine.await_seek_completed(Duration::from_secs(10));
    assert!(
        forward_landed >= Duration::from_secs(3),
        "the forward seek landed short at {forward_landed:?}"
    );
    assert!(
        engine.progress().position >= Duration::from_secs(3),
        "the engine's own position was not installed at the forward seek's landing"
    );
    let forward_byte = server
        .requests()
        .into_iter()
        .rev()
        .find_map(|request| request.range())
        .map(|(first, _)| first)
        .unwrap_or_else(|| panic!("no ranged request for the forward seek reached the server"));
    assert!(
        forward_byte > 0,
        "the forward seek's request did not target a nonzero byte offset"
    );

    assert_eq!(
        engine
            .handle()
            .submit(PlaybackCommand::SeekTo(Duration::from_millis(500))),
        Admission::Accepted
    );
    let backward_landed = engine.await_seek_completed(Duration::from_secs(10));
    assert!(
        backward_landed < Duration::from_secs(2),
        "the backward seek did not actually land earlier: {backward_landed:?}"
    );
    assert!(
        engine.progress().position < Duration::from_secs(2),
        "the engine's own position was not installed at the backward seek's landing: {:?}",
        engine.progress().position
    );
    let backward_byte = server
        .requests()
        .into_iter()
        .rev()
        .find_map(|request| request.range())
        .map(|(first, _)| first)
        .unwrap_or_else(|| panic!("no ranged request for the backward seek reached the server"));
    assert!(
        backward_byte < forward_byte,
        "the backward seek's request byte {backward_byte} was not earlier than the \
         forward seek's {forward_byte}"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_range_less_server_plays_but_cannot_seek_or_resume() {
    // H5, first half: a server that ignores ranges is still finite media —
    // it plays — but every seek is refused, whether or not it also bothers
    // to advertise `Accept-Ranges` while lying about honouring it.
    for script in [
        Script::from_fixture("sine-5s.flac").without_ranges(),
        Script::from_fixture("sine-5s.flac")
            .without_ranges()
            .without_accept_ranges_header(),
    ] {
        let server = TestServer::start(script);
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&server.url("/audio.flac"));
        assert_eq!(
            engine.handle().submit(PlaybackCommand::Play),
            Admission::Accepted
        );
        engine.await_state(PlaybackState::Playing);
        engine.play_for(Duration::from_millis(100));

        assert_eq!(
            engine
                .handle()
                .submit(PlaybackCommand::SeekTo(Duration::from_secs(2))),
            Admission::Accepted
        );
        let rejection = engine.await_event(|e| matches!(e, PlaybackEvent::SeekRejected { .. }));
        assert!(
            matches!(rejection, PlaybackEvent::SeekRejected { .. }),
            "a range-less server's seek must be refused, not silently accepted"
        );
        assert_eq!(
            engine.state(),
            PlaybackState::Playing,
            "the refused seek must not disturb ongoing sequential playback"
        );

        engine.finish();
        server.shutdown();
    }

    // H5, second half: §10's fallback. A positive checkpoint a non-seekable
    // source cannot restore is protected — periodic, pause, stop and
    // shutdown captures must not overwrite it just because playback fell
    // back to zero.
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let url = {
        // A throwaway server only to learn a port `start_on` can reuse for
        // both the seekable session and the range-less one after it, so the
        // URL — and the `MediaId` derived from it — never changes.
        let probe = TestServer::start(Script::from_fixture("sine-5s.flac"));
        let url = probe.url("/audio.flac");
        probe.shutdown();
        url
    };
    let media = media_for(&url);
    let port = url::Url::parse(&url)
        .unwrap_or_else(|error| panic!("test URL must parse: {error}"))
        .port()
        .unwrap_or_else(|| panic!("test URL must carry a port"));

    // Session 1: range-capable, plays past zero, then quits — a positive,
    // incomplete checkpoint lands in the store.
    let (store, clock) = RemoteRig::store_in(dir.path());
    let server1 = TestServer::start_on(port, Script::from_fixture("sine-5s.flac"));
    let mut session1 = Session::new(PersistedState::default());
    let request1 = session1
        .register_load(LoadTarget::Legacy, &media)
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    let mut engine1 = TestEngine::start_idle();
    engine1.load_remote_as(request1, &url, ResumeIntent::StartAt(Duration::ZERO));
    assert_eq!(
        engine1.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
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

    let before = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("session 1 must have left a checkpoint"));
    assert!(before.position.unwrap() >= Duration::from_secs(1) && !before.completed);

    // Session 2: the same URL, now behind a range-less server. Restoration
    // is unavailable, so playback falls back to zero with the entry
    // protected — a fresh, if unrelated, span of progress must not replace
    // the position session 1 actually reached.
    let server2 = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").without_ranges());

    let (store2, clock2) = RemoteRig::store_in(dir.path());
    let mut engine2 = TestEngine::start_idle();
    engine2.load_remote_with_resume(
        &url,
        ResumeIntent::Candidate(
            resume_candidate(before.position, before.completed)
                .expect("session 1's checkpoint must carry an established position"),
        ),
    );
    let loaded = engine2.await_loaded();
    match loaded.disposition {
        StartDisposition::ResumeUnavailable { retained } => {
            assert_eq!(retained, before.position.unwrap());
        }
        other => panic!("expected ResumeUnavailable, got {other:?}"),
    }
    assert_eq!(loaded.position, Duration::ZERO);

    let mut rig2 = RemoteRig {
        engine: engine2,
        session: Session::new(reload(dir.path())),
        store: store2,
        clock: clock2,
    };
    rig2.pump();
    assert_eq!(
        rig2.engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    rig2.engine.await_state(PlaybackState::Playing);
    rig2.engine.play_for(Duration::from_millis(300));
    rig2.pump();
    rig2.quit();

    let after = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("the protected entry must still exist"));
    assert_eq!(
        after.position, before.position,
        "a protected entry was overwritten by progress from the zero-start fallback"
    );
    assert!(!after.completed);

    server2.shutdown();
}

/// §4.3/§10 (Task 6 fix round 1): the `SeekSupport::Unsupported` fallback a
/// non-seekable source triggers for `Candidate` (this file's test just
/// above) must not be routed around by `EstimatedCandidate` — an estimated
/// target is exactly as unreachable on a range-less server as an
/// established one, and the load must still announce `ResumeUnavailable`
/// rather than silently starting at zero. Ablation: an `EstimatedCandidate`
/// branch that skips the `SeekSupport::Unsupported` check (the constraint
/// the coordinator's fix round named explicitly) makes this fail —
/// `loaded.disposition` would read `ResumedEstimated` and `loaded.position`
/// would land nonzero instead of at `Duration::ZERO`.
#[test]
fn an_estimated_resume_on_a_range_less_server_falls_back_to_resume_unavailable() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_resume(
        &server.url("/audio.flac"),
        ResumeIntent::EstimatedCandidate {
            target: Duration::from_secs(2),
            established: Some(Duration::from_secs(1)),
        },
    );
    let loaded = engine.await_loaded();
    match loaded.disposition {
        StartDisposition::ResumeUnavailable { retained } => {
            assert_eq!(
                retained,
                Duration::from_secs(2),
                "the estimate is the location that could not be honoured"
            );
        }
        other => panic!("expected ResumeUnavailable, got {other:?}"),
    }
    assert_eq!(loaded.position, Duration::ZERO);

    engine.finish();
    server.shutdown();
}

/// Build a WAV body of arbitrary size by repeating `sine-5s.wav`'s own PCM
/// payload, with the RIFF and `data` chunk sizes patched to match. H13 needs
/// a body well past the byte channel's real capacity (see the occupancy
/// test's own comment for why that capacity is a megabyte, not the brief's
/// smaller numbers), and synthesizing one this way avoids committing a
/// multi-megabyte fixture just to prove it. WAV rather than FLAC: PCM has no
/// frame sync to search for, so repeating its payload verbatim stays
/// trivially decodable throughout.
fn large_wav_body(repeats: usize) -> Vec<u8> {
    let source = std::fs::read(support::fixture_path("sine-5s.wav"))
        .unwrap_or_else(|error| panic!("the fixture must exist: {error}"));
    let marker = source
        .windows(4)
        .position(|w| w == b"data")
        .unwrap_or_else(|| panic!("sine-5s.wav has no data chunk"));
    let payload_start = marker + 8;
    let prefix = &source[..payload_start];
    let payload = &source[payload_start..];

    let mut body = Vec::with_capacity(prefix.len() + payload.len() * repeats);
    body.extend_from_slice(prefix);
    for _ in 0..repeats {
        body.extend_from_slice(payload);
    }

    let data_len = u32::try_from(payload.len() * repeats)
        .unwrap_or_else(|error| panic!("synthesized WAV body too large: {error}"));
    body[marker + 4..marker + 8].copy_from_slice(&data_len.to_le_bytes());
    let riff_len = u32::try_from(body.len() - 8)
        .unwrap_or_else(|error| panic!("synthesized WAV body too large: {error}"));
    body[4..8].copy_from_slice(&riff_len.to_le_bytes());
    body
}

/// Headroom on top of `buffer_bytes + chunk_bytes` for what the kernel's own
/// TCP socket buffers can absorb on a fast loopback connection before
/// backpressure from a full `ByteChannel` ever reaches the server's writes —
/// this machine's own `tcp_rmem` autotunes up to 32 MiB, so this is sized
/// to that order of magnitude rather than guessed small. Named so the
/// occupancy bound below reads as "ours plus the kernel's", not one
/// unexplained number.
const KERNEL_SOCKET_SLACK: usize = 16 << 20;

#[test]
fn occupancy_stays_bounded_and_starvation_silence_does_not_advance_position() {
    // H13, first half: a fast server and a consumer that never drains must
    // not let the byte channel's producer keep pulling the whole recording
    // over the wire. Observed from the server's own write counter, since the
    // channel lives inside the worker and no test can reach it directly —
    // and a bound only visible there would be a claim about accounting, not
    // about the wire.
    //
    // The brief's own `Limits { buffer_bytes: 64 << 10, chunk_bytes: 8 << 10,
    // .. }` does not reach this path: `buffer_bytes` configures
    // `HttpService`'s HTTP/2 connection window (`http2_initial_connection_
    // window_size`, `src/http/service.rs`), and `TestServer` speaks plain
    // HTTP/1.1 only — reqwest has no equivalent window knob for HTTP/1.1
    // (the comment beside that call says so directly). `chunk_bytes` *is*
    // used regardless of transport, as the application-level chunk cap
    // `chunk_cap` in the same file's transfer loop. The channel's real ring
    // capacity (`SourceInterrupt`) is what `buffer_bytes` actually sizes —
    // just allocated once, for the worker's whole life, from
    // `Limits::default().buffer_bytes` in `EngineHandle::spawn_with`
    // (`src/playback/engine.rs`), regardless of whatever `Limits` a
    // later-attached `HttpService` carries. So the bound this suite can
    // observe is `Limits::default()`'s own numbers, not the smaller ones a
    // caller might pass to `HttpService::spawn`.
    //
    // `bytes_written` is the *server's* write count, not the channel's
    // occupancy directly — `ByteChannel::push` genuinely blocks once
    // `state.bytes.len()` reaches `buffer_bytes` (verified against
    // `src/http/channel.rs`), but the kernel's own TCP send/receive buffers
    // sit in front of that block and can absorb several megabytes more on a
    // fast loopback connection before backpressure ever reaches the
    // server's `write_all` (this machine's own `tcp_rmem` autotunes up to
    // 32 MiB). `KERNEL_SOCKET_SLACK` names that margin explicitly as the
    // kernel's, not this project's, and the body is sized well past it so a
    // regression that actually removed the channel's cap (unbounded growth
    // toward the whole body) would still be caught.
    let body = large_wav_body(40);
    let body_len = body.len();
    let server = TestServer::start(Script::serving(body));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.wav"));

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = server.bytes_written();
    let mut stable_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let now = server.bytes_written();
        if now != last {
            last = now;
            stable_since = Instant::now();
        }
        if stable_since.elapsed() >= Duration::from_millis(500) {
            break;
        }
        if Instant::now() >= deadline {
            panic!("bytes_written never settled; still at {now}");
        }
    }
    let settled = server.bytes_written();
    let bound =
        Limits::default().buffer_bytes + Limits::default().chunk_bytes + KERNEL_SOCKET_SLACK;
    assert!(
        settled <= bound,
        "occupancy grew past buffer_bytes + chunk_bytes + kernel slack: \
         {settled} > {bound} (of a {body_len}-byte body)"
    );

    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    // `settled` bytes of uncompressed PCM is itself several seconds of
    // audio (how many varies run to run with whatever the kernel's own
    // socket buffers happened to absorb before the producer's own read
    // stopped pulling), so nothing refills the channel merely by *being*
    // `Playing` — draining actually has to reach however far that head
    // start goes. Polled rather than a single fixed `play_for`, so the
    // assertion is "growth resumes" rather than a guess at how many
    // seconds of buffered audio one run happened to accumulate.
    let mut drained = Duration::ZERO;
    loop {
        drained += Duration::from_secs(2);
        engine.play_for(drained);
        if server.bytes_written() > settled {
            break;
        }
        assert!(
            drained < Duration::from_secs(60),
            "playing for {drained:?} did not resume the transfer: stuck at {}",
            server.bytes_written()
        );
    }

    engine.finish();
    server.shutdown();

    // H13, second half: a body that stops arriving must not let heard
    // position advance past what was actually decoded. The engine is left
    // Playing throughout, not Paused, so the worker is legitimately blocked
    // inside the stalled read once the ring runs dry - which is exactly why
    // this uses `let_time_pass_while_unresponsive` below rather than
    // `let_time_pass`: the latter settles behind a `SetVolume` round trip the
    // worker cannot answer until the stall itself resolves, and would wait
    // out that patience instead of observing anything about the position.
    // A generous stall deadline, not the cached brisk service every plain
    // `load_remote` shares: draining the ring below takes real wall time
    // while the read stays genuinely blocked, and that has to fit
    // comfortably inside the deadline or the stall timeout H8 covers
    // separately would fire here instead.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote_with_limits(&server.url("/audio.flac"), Limits::default());
    assert_eq!(
        engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the read never blocked, so starvation was never actually staged"
    );

    engine.drain_ring_without_advancing_clock();
    assert_eq!(
        engine.state(),
        PlaybackState::Playing,
        "starvation must read as silence, not as a stop or a failure"
    );
    // The ring being empty of *new* audio is not the same instant as every
    // already-published span having been walked through - the output
    // latency means a little more of the clock this drain already advanced
    // is still ahead of what the device has actually reached. Letting that
    // settle first is what makes `starved_at` the position silence actually
    // starts from.
    engine.let_time_pass_while_unresponsive(Duration::from_millis(300));
    let starved_at = engine.progress().position;
    engine.let_time_pass_while_unresponsive(Duration::from_millis(200));
    assert_eq!(
        engine.progress().position,
        starved_at,
        "position advanced during network starvation with nothing left to play"
    );
    assert_eq!(engine.state(), PlaybackState::Playing);

    assert!(server.release());
    engine.finish();
    server.shutdown();
}

#[test]
fn an_m4a_recording_opens_over_ranges() {
    // §12's closing paragraph: an ISO-BMFF file whose `moov` atom sits at the
    // tail needs byte seeking to open at all (verified empirically when this
    // fixture was generated - see tests/fixtures/README.md), which is
    // exactly what a range-capable HTTP source provides and a sequential one
    // does not.
    let server = TestServer::start(Script::from_fixture("sine-5s.m4a"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.m4a"));
    assert_eq!(engine.state(), PlaybackState::Paused);
    let loaded = engine.await_event(|e| matches!(e, PlaybackEvent::Loaded { .. }));
    let PlaybackEvent::Loaded { capabilities, .. } = loaded else {
        unreachable!("await_event's predicate already matched Loaded")
    };
    // Unknown, like every remote source before a trial seek - the point of
    // this test is that a tail-`moov` layout *opens* over ranges at all, not
    // that this particular container also proves seekable.
    assert_eq!(
        capabilities.seek,
        SeekSupport::Unknown,
        "a byte-seekable source was advertised as media-seekable before anything demonstrated it"
    );

    engine.finish();
    server.shutdown();

    // The negative half: the same file, sequential access only, must not
    // open at all - it is the range support above that makes the difference,
    // not something incidental to this one fixture.
    let sequential = TestServer::start(Script::from_fixture("sine-5s.m4a").without_ranges());
    let mut sequential_engine = TestEngine::start_idle();
    sequential_engine.load_remote_expecting_failure(&sequential.url("/audio.m4a"));
    let failed = sequential_engine.await_event(|e| matches!(e, PlaybackEvent::Failed { .. }));
    assert!(
        matches!(failed, PlaybackEvent::Failed { .. }),
        "a sequential-only source opened a tail-moov file it has no way to seek into"
    );

    sequential_engine.finish();
    sequential.shutdown();
}
