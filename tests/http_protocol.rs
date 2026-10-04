//! §12's acceptance evidence for H6, H7, H8 and H11: the wire-level protocol
//! rules already unit-tested in `tests/http_response.rs`/`tests/http_fetch.rs`
//! against fabricated headers or a bare `fetch::open`, proven here end to end
//! through a real `TestEngine` — the position and state a listener actually
//! sees, not just the internal `RemoteFailure` a lower layer produced. Every
//! server is `127.0.0.1:<ephemeral>`; every engine runs over `TestOutput`.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::http::channel::SourceInterrupt;
use tenuto::http::error::{RedirectRejection, RemoteFailure};
use tenuto::http::limits::Limits;
use tenuto::media::id::{MediaId, NormalizedUrl};
use tenuto::media::source::SourceLocation;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::error::PlaybackError;
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::prepare::{PrepareContext, prepare};
use tenuto::playback::state::PlaybackState;
use tenuto::session::{Action, LoadTarget, Session};

use support::TestEngine;
use support::server::{Script, TestServer};

fn media_for(url: &str) -> MediaId {
    match NormalizedUrl::parse(url) {
        Ok(normalized) => MediaId::RemoteUrl(normalized),
        Err(error) => panic!("test URL {url:?} must normalize: {error}"),
    }
}

fn url(text: &str) -> url::Url {
    url::Url::parse(text).unwrap_or_else(|error| panic!("test URL {text:?} must parse: {error}"))
}

struct NoHook;
impl tenuto::http::channel::WaitHook for NoHook {
    fn service(&self) {}
}

/// A `PrepareContext` for the `prepare()`-level cases (H6's three failure
/// modes), which need no engine — `prepare.rs` uses the identical shape.
fn context() -> PrepareContext {
    let http = match tenuto::http::service::HttpService::spawn(Limits::default()) {
        Ok(service) => service,
        Err(error) => panic!("the HTTP service must start: {error}"),
    };
    PrepareContext {
        http: Some(http),
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: std::sync::Arc::new(NoHook),
        limits: Limits::default(),
        expected: None,
    }
}

/// `/audio`'s port, for a test that swaps the server behind the connection a
/// `TestEngine` already opened without changing the URL — the same
/// `start_on` pattern `engine_remote.rs` uses for its own cancellation tests.
fn port_of(server_url: &str) -> u16 {
    url(server_url)
        .port()
        .unwrap_or_else(|| panic!("test URL {server_url:?} must carry a port"))
}

#[test]
fn redirects_preserve_identity_and_query_and_the_bad_ones_fail() {
    // H6, success half: the query survives the hop, and the `MediaId` the
    // engine reports on `Loaded` is built from the URL as given, not
    // wherever the chain actually landed.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").redirect_chain(2));
    let original = server.url("/audio?token=abc&Expires=9");
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&original);
    let loaded = engine.await_event(|e| matches!(e, PlaybackEvent::Loaded { .. }));
    let PlaybackEvent::Loaded { media, .. } = loaded else {
        unreachable!("await_event's predicate already matched Loaded")
    };
    assert_eq!(
        media,
        media_for(&original),
        "the loaded media's identity followed the redirect instead of staying the original URL"
    );
    let requests = server.requests();
    assert_eq!(
        requests.first().and_then(|r| r.query.as_deref()),
        Some("token=abc&Expires=9"),
        "the original query did not reach the origin verbatim: {requests:?}"
    );
    // At least one full traversal of the two-hop chain: opening a remote
    // source makes more than one request on its own (H13's occupancy test
    // documents the extra probe and reopen), so this is a lower bound, not
    // an exact count.
    assert!(
        requests.len() >= 3,
        "expected at least a two-hop chain plus the final GET: {requests:?}"
    );
    engine.finish();
    server.shutdown();

    // H6, failure half: a loop, an over-long chain and a non-HTTP scheme
    // must all fail opening, not merely fail to seek or play. The fourth
    // case §12 names, an HTTPS-to-HTTP downgrade, is a distinct
    // `RedirectRejection` this loopback server cannot stage for real (it
    // never serves TLS) — it is unit-tested directly, real `Downgrade`
    // variant and all, by `tests/http_response.rs::redirects_are_bounded_
    // checked_and_never_downgraded`, and `tests/http_fetch.rs::an_https_
    // to_http_downgrade_is_refused` pins the *service* wiring the same way
    // this test's non-HTTP-scheme case does: a redirect to an unsupported
    // scheme, standing in for what the loopback origin cannot serve.
    let looping = TestServer::start(Script::serving(b"x".to_vec()).redirect_loop());
    let error = match prepare(
        &SourceLocation::Http(url(&looping.url("/audio"))),
        &context(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("a redirect loop must not open"),
    };
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::Redirect {
                reason: RedirectRejection::Loop
            })
        ),
        "{error}"
    );
    looping.shutdown();

    let long = TestServer::start(Script::serving(b"x".to_vec()).redirect_chain(9));
    let error = match prepare(&SourceLocation::Http(url(&long.url("/audio"))), &context()) {
        Err(error) => error,
        Ok(_) => panic!("an over-long redirect chain must not open"),
    };
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::Redirect {
                reason: RedirectRejection::TooMany
            })
        ),
        "{error}"
    );
    long.shutdown();

    let scheme =
        TestServer::start(Script::serving(b"x".to_vec()).redirect_to("ftp://example.com/a.mp3"));
    let error = match prepare(
        &SourceLocation::Http(url(&scheme.url("/audio"))),
        &context(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("a non-HTTP redirect target must not open"),
    };
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::Redirect {
                reason: RedirectRejection::UnsupportedScheme
            })
        ),
        "{error}"
    );
    scheme.shutdown();
}

#[test]
fn every_malformed_range_response_fails_without_committing_a_target() {
    // H7: each case plays a healthy source to a nonzero position, then
    // swaps in a server that answers every further range request with one
    // specific malformed shape and seeks. The seek's own attempt and its
    // best-effort restoration both hit the same malformed response, so the
    // worker lands in `Failed` — the point being proven either way is that
    // the malformed response's bogus offset is never adopted: the position
    // stays exactly what it was before the seek, never `SeekCompleted`.
    let real_fixture = std::fs::read(support::fixture_path("sine-5s.flac"))
        .unwrap_or_else(|error| panic!("the fixture must exist: {error}"));
    let mut different_length = real_fixture.clone();
    different_length.push(0);

    let cases: [(&str, Script); 7] = [
        (
            "wrong start",
            Script::from_fixture("sine-5s.flac")
                .content_range_override("bytes 9999990-9999999/99999999"),
        ),
        (
            "reversed interval",
            Script::from_fixture("sine-5s.flac").content_range_override("bytes 100-50/99999999"),
        ),
        ("conflicting total", Script::serving(different_length)),
        (
            "missing range",
            Script::from_fixture("sine-5s.flac").content_range_override(""),
        ),
        (
            "multipart",
            Script::from_fixture("sine-5s.flac").multipart_range(),
        ),
        (
            "unexpected 200",
            Script::from_fixture("sine-5s.flac").range_answered_with_200(),
        ),
        ("416", Script::from_fixture("sine-5s.flac").status(416)),
    ];

    for (name, malformed_script) in cases {
        let healthy = TestServer::start(Script::from_fixture("sine-5s.flac"));
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&healthy.url("/audio.flac"));
        assert_eq!(
            engine.handle().submit(PlaybackCommand::Play),
            Admission::Accepted
        );
        engine.await_state(PlaybackState::Playing);
        engine.play_for(Duration::from_millis(200));
        let before = engine.progress().position;
        let port = port_of(&healthy.url("/audio.flac"));
        healthy.shutdown();

        let malformed = TestServer::start_on(port, malformed_script);
        assert_eq!(
            engine
                .handle()
                .submit(PlaybackCommand::SeekTo(Duration::from_secs(3))),
            Admission::Accepted
        );
        // Either outcome commits nothing: `Failed` (the common case, since
        // the restoration attempt hits the same malformed response) or a
        // `SeekRejected` if some case instead recovers cleanly - never a
        // `SeekCompleted` landing on the malformed response's bogus offset.
        // The predicate itself is the proof of that (fix round 2, MINOR: a
        // `!matches!(_, SeekCompleted)` used to stand here too, vacuous
        // against a value this same predicate already restricted to
        // `Failed | SeekRejected` - the real evidence is `count_events` and
        // the position assertion below).
        engine.await_event(|e| {
            matches!(
                e,
                PlaybackEvent::Failed { .. } | PlaybackEvent::SeekRejected { .. }
            )
        });
        assert_eq!(
            engine.count_events(|e| matches!(e, PlaybackEvent::SeekCompleted { .. })),
            0,
            "{name}: a malformed range response must never complete a seek"
        );
        assert_eq!(
            engine.progress().position,
            before,
            "{name}: the malformed response's bogus offset was committed"
        );

        engine.finish();
        malformed.shutdown();
    }
}

/// H8's shared `Session` + `StateStore` rig, matching `resume_contract.rs`'s
/// own "drain events, then sample once" ordering against `app::run` - kept
/// local rather than shared, the same as every other acceptance file here.
struct RemoteRig {
    engine: TestEngine,
    session: Session,
    store: StateStore,
    clock: Arc<FakeClock>,
}

impl RemoteRig {
    fn new(dir: &Path, engine: TestEngine) -> Self {
        let clock = Arc::new(FakeClock::new());
        let injected: Arc<dyn Clock> = clock.clone();
        Self {
            engine,
            session: Session::new(PersistedState::default()),
            store: StateStore::new(dir.join("state.json"), injected),
            clock,
        }
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
}

fn reload(dir: &Path) -> PersistedState {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
    StateStore::new(dir.join("state.json"), clock).load().state
}

#[test]
fn no_broken_transfer_can_become_a_completed_track() {
    // H8: none of these four ways a transfer can go wrong may end in
    // `Ended` - only `Failed`. And the checkpoint a periodic capture wrote
    // *before* the failure must survive it unharmed: a `Failed` transition
    // raises no force of its own (only `Paused`/`Stopped` do), so what
    // proves the checkpoint's safety is that the one this test forces in
    // while still `Playing` is still exactly there afterwards, not reset to
    // zero and not silently advanced by anything the failure reported.
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));

    let scenario = |name: &str, server: TestServer| {
        let mut rig = RemoteRig::new(dir.path(), TestEngine::start_without_recovery());
        let url = server.url("/audio.flac");
        let request = rig
            .session
            .register_load(LoadTarget::Legacy, &media_for(&url))
            .unwrap_or_else(|error| panic!("{name}: registered: {error:?}"));
        rig.engine
            .load_remote_as(request, &url, ResumeIntent::StartAt(Duration::ZERO));
        assert_eq!(
            rig.engine.handle().submit(PlaybackCommand::Play),
            Admission::Accepted
        );
        rig.engine.await_state(PlaybackState::Playing);
        rig.engine.play_for(Duration::from_millis(200));
        rig.pump();
        // Force a periodic capture while still genuinely `Playing`, before
        // the failure ever fires.
        rig.clock.advance(Duration::from_secs(6));
        rig.pump();
        let media = rig
            .engine
            .progress()
            .media
            .unwrap_or_else(|| panic!("{name}: the session must know its own media by now"));
        let captured = reload(dir.path())
            .entry_for(&media)
            .cloned()
            .unwrap_or_else(|| panic!("{name}: the periodic capture left no checkpoint"));
        assert!(
            captured.position.unwrap() >= Duration::from_millis(150),
            "{name}: the periodic capture did not carry real playback progress: {:?}",
            captured.position
        );
        assert!(!captured.completed);

        rig.engine.play_until_terminal(Duration::from_secs(10));
        assert_eq!(rig.engine.state(), PlaybackState::Failed, "{name}");
        assert!(!rig.engine.saw_end_of_track(), "{name}: reached EndOfTrack");
        rig.pump();

        let after = reload(dir.path())
            .entry_for(&media)
            .cloned()
            .unwrap_or_else(|| panic!("{name}: the checkpoint disappeared after the failure"));
        assert_eq!(
            after.position, captured.position,
            "{name}: the failure disturbed the pre-failure checkpoint"
        );
        assert!(
            !after.completed,
            "{name}: a failure must never be marked completed"
        );

        rig.engine.finish();
        server.shutdown();
    };

    // Short response: fewer bytes than the advertised length, then a clean
    // disconnect.
    scenario(
        "short response",
        TestServer::start(Script::from_fixture("sine-5s.flac").truncate_body_after(48 << 10)),
    );

    // Disconnect with no advertised length: chunked framing withholds
    // `Content-Length`, so the abrupt close is reported as a transport
    // failure rather than a length mismatch - a different `RemoteFailure`
    // category from the short-response case above, and still never a
    // completion.
    scenario(
        "disconnect",
        TestServer::start(
            Script::from_fixture("sine-5s.flac")
                .chunked()
                .truncate_body_after(48 << 10),
        ),
    );

    // Malformed audio over an otherwise perfect transfer: full
    // `Content-Length`, clean EOF, valid ETag - and the bytes past the
    // midpoint are garbage, so decoding itself must fail.
    let mut corrupt = std::fs::read(support::fixture_path("sine-5s.flac"))
        .unwrap_or_else(|error| panic!("the fixture must exist: {error}"));
    let middle = corrupt.len() / 2;
    for byte in &mut corrupt[middle..] {
        *byte = 0xFF;
    }
    scenario(
        "malformed audio",
        TestServer::start(Script::serving(corrupt)),
    );

    // Stalled body: the connection never closes and never sends another
    // byte, so only the stall deadline can end the attempt. Handled
    // separately from `scenario` because it needs to prove the stall was
    // actually entered before the deadline fires.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(48 << 10));
    let mut rig = RemoteRig::new(dir.path(), TestEngine::start_without_recovery());
    let stalled_url = server.url("/audio.flac");
    let stalled_request = rig
        .session
        .register_load(LoadTarget::Legacy, &media_for(&stalled_url))
        .unwrap_or_else(|error| panic!("stalled body: registered: {error:?}"));
    rig.engine.load_remote_as(
        stalled_request,
        &stalled_url,
        ResumeIntent::StartAt(Duration::ZERO),
    );
    assert_eq!(
        rig.engine.handle().submit(PlaybackCommand::Play),
        Admission::Accepted
    );
    rig.engine.await_state(PlaybackState::Playing);
    rig.engine.play_for(Duration::from_millis(200));
    rig.pump();
    rig.clock.advance(Duration::from_secs(6));
    rig.pump();
    let media = rig
        .engine
        .progress()
        .media
        .unwrap_or_else(|| panic!("stalled body: the session must know its own media by now"));
    let captured = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("stalled body: the periodic capture left no checkpoint"));
    assert!(captured.position.unwrap() >= Duration::from_millis(150));
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "stalled body: the read never blocked"
    );

    rig.engine.play_until_terminal(Duration::from_secs(10));
    assert_eq!(rig.engine.state(), PlaybackState::Failed, "stalled body");
    assert!(
        !rig.engine.saw_end_of_track(),
        "stalled body: reached EndOfTrack"
    );
    rig.pump();
    let after = reload(dir.path())
        .entry_for(&media)
        .cloned()
        .unwrap_or_else(|| panic!("stalled body: the checkpoint disappeared after the failure"));
    assert_eq!(
        after.position, captured.position,
        "stalled body: the failure disturbed the pre-failure checkpoint"
    );
    assert!(!after.completed);

    rig.engine.finish();
    assert!(server.release());
    server.shutdown();
}

struct NoOpHook;
impl tenuto::http::channel::WaitHook for NoOpHook {
    fn service(&self) {}
}

fn open_source(server: &TestServer) -> tenuto::http::source::HttpMediaSource {
    let service = match tenuto::http::service::HttpService::spawn(Limits::default()) {
        Ok(service) => service,
        Err(error) => panic!("the HTTP service must start: {error}"),
    };
    match tenuto::http::source::HttpMediaSource::open_and_probe(
        service,
        url(&server.url("/audio")),
        SourceInterrupt::new(Limits::default().buffer_bytes),
        Arc::new(NoOpHook),
        Limits::default(),
        |source| source,
    ) {
        Ok(source) => source,
        Err(error) => panic!("opening must succeed: {error}"),
    }
}

#[test]
fn validators_follow_the_documented_policy() {
    // H11: a strong validator is conclusive, so a change fails the seek; a
    // weak or absent one is best-effort, so the seek still lands - and in
    // neither case does an `If-Range` header ever carry a weak (`W/`) value,
    // which §7 forbids sending. Checked directly against what the server
    // actually received, not just against the client's own bookkeeping.
    use std::io::{Seek, SeekFrom};

    // Strong validator, changed after the opening request: the seek's
    // request must carry `If-Range` with the *original* strong value (the
    // one `established` when opening), and must fail once the origin
    // answers with the new one.
    let server = TestServer::start(Script::serving(body_bytes()).changing_etag_after(1));
    let mut source = open_source(&server);
    let outcome = source.seek(SeekFrom::Start(4096));
    assert!(
        matches!(
            outcome
                .err()
                .and_then(|e| tenuto::http::source::remote_cause(&e)),
            Some(RemoteFailure::ResourceChanged)
        ),
        "a changed strong validator must fail the seek as ResourceChanged"
    );
    let requests = server.requests();
    let seek_request = requests
        .get(1)
        .unwrap_or_else(|| panic!("the seek must have made its own request: {requests:?}"));
    let if_range = seek_request
        .header("if-range")
        .unwrap_or_else(|| panic!("a strong validator must be sent as If-Range: {requests:?}"));
    assert!(
        !if_range.starts_with("W/"),
        "an If-Range header carried a weak value: {if_range}"
    );
    assert_eq!(if_range, "\"v1\"");
    server.shutdown();

    // Weak validator only: best-effort still lets the seek land, and the
    // seek's request carries no If-Range at all - §7 forbids sending a weak
    // validator that way.
    let server = TestServer::start(Script::serving(body_bytes()).weak_etag());
    let mut source = open_source(&server);
    let outcome = source.seek(SeekFrom::Start(4096));
    assert!(
        outcome.is_ok(),
        "a weak validator alone must not block a seek: {outcome:?}"
    );
    let requests = server.requests();
    let seek_request = requests
        .get(1)
        .unwrap_or_else(|| panic!("the seek must have made its own request: {requests:?}"));
    assert!(
        seek_request.header("if-range").is_none(),
        "a weak validator must never be sent as If-Range: {requests:?}"
    );
    server.shutdown();

    // No validator at all: still best-effort, still lands, still nothing to
    // send.
    let server = TestServer::start(Script::serving(body_bytes()).no_validator());
    let mut source = open_source(&server);
    let outcome = source.seek(SeekFrom::Start(4096));
    assert!(
        outcome.is_ok(),
        "no validator must not block a seek: {outcome:?}"
    );
    let requests = server.requests();
    let seek_request = requests
        .get(1)
        .unwrap_or_else(|| panic!("the seek must have made its own request: {requests:?}"));
    assert!(
        seek_request.header("if-range").is_none(),
        "an absent validator must never produce an If-Range: {requests:?}"
    );
    server.shutdown();
}

fn body_bytes() -> Vec<u8> {
    (0..8192u32).map(|i| (i % 251) as u8).collect()
}
