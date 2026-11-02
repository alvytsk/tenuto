mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use support::server::{DocumentReply, Script, TestServer};
use tenuto::http::channel::{ByteChannel, HeaderOutcome, ReadOutcome, SourceInterrupt, WaitHook};
use tenuto::http::error::{Operation, Phase, RangeRejection, RedirectRejection, RemoteFailure};
use tenuto::http::limits::Limits;
use tenuto::http::response::{Accepted, FetchAccepted};
use tenuto::http::service::{FetchRequest, HttpService};
use url::Url;

struct NoHook;
impl WaitHook for NoHook {
    fn service(&self) {}
}

/// Proves a synchronous wait was actually *entered* (parked at least one
/// `wait_timeout` slice), not merely that some other thread reached the
/// server. `SourceInterrupt::wait_for_headers` calls `service.service()`
/// only after its first slice elapses, so blocking a test thread on this
/// flag is real proof of parking — unlike watching `server.requests()`,
/// which only proves the *fetch task* reached the server and says nothing
/// about whether the waiter thread was ever scheduled.
#[derive(Default)]
struct FlagHook(Arc<AtomicBool>);
impl WaitHook for FlagHook {
    fn service(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[allow(clippy::unwrap_used)] // A URL this test built from a bound loopback port.
fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn service(limits: Limits) -> Arc<HttpService> {
    match HttpService::spawn(limits) {
        Ok(service) => service,
        Err(error) => panic!("the HTTP service must start: {error}"),
    }
}

// `ByteChannel` is a `Clone` handle over `Arc<SourceInterrupt>`, not a type
// the caller wraps in its own `Arc` — Task 3 already made it cheap to clone.
// `interrupt.begin()` is what opens a live generation; `channel.generation()`
// only reads the current value back, which is not what a fetch needs.
fn open(
    service: &HttpService,
    origin: Url,
    start: u64,
) -> (
    ByteChannel,
    Arc<SourceInterrupt>,
    Result<FetchAccepted, HeaderOutcome>,
) {
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = interrupt.begin();
    let wait = service.fetch(
        FetchRequest {
            origin,
            start,
            established: None,
            operation: Operation::Open,
        },
        channel.clone(),
        generation,
    );
    let outcome = wait.wait(&NoHook, Duration::from_secs(10));
    (channel, interrupt, outcome)
}

fn drain(channel: &ByteChannel) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(10)) {
            ReadOutcome::Bytes(n) => all.extend_from_slice(&buffer[..n]),
            ReadOutcome::Eof => return all,
            other => panic!("unexpected read outcome: {other:?}"),
        }
    }
}

#[test]
fn an_opening_range_get_establishes_ranged_access_and_streams_the_body() {
    let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
    let server = TestServer::start(Script::serving(body.clone()));
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);

    match accepted {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Ranged { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }
    assert_eq!(drain(&channel), body);

    // §6: the opening probe is a real range GET, reusing its response as the
    // initial stream. There is no preliminary HEAD.
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].range(), Some((0, None)));
    server.shutdown();
}

#[test]
fn a_range_ignoring_server_is_accepted_as_sequential() {
    let body = vec![3u8; 2048];
    let server = TestServer::start(Script::serving(body.clone()).without_ranges());
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    match accepted {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Sequential { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }
    assert_eq!(drain(&channel), body);
    server.shutdown();
}

#[test]
fn ranges_work_even_when_accept_ranges_is_absent() {
    // §6: `Accept-Ranges` is a hint, not the evidence. A real range GET is.
    let server = TestServer::start(Script::serving(vec![1u8; 4096]).without_accept_ranges_header());
    let service = service(Limits::default());
    let (_channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    match accepted {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Ranged { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }
    server.shutdown();
}

#[test]
fn redirects_are_followed_up_to_the_limit_and_the_query_survives() {
    // H6. The redirect chain must not lose the signed query on the way.
    let server = TestServer::start(Script::serving(b"final".to_vec()).redirect_chain(3));
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) =
        open(&service, url(&server.url("/audio?token=abc&Expires=9")), 0);
    match accepted {
        Ok(accepted) => assert_eq!(accepted.redirects, 3),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }
    assert_eq!(drain(&channel), b"final");
    let requests = server.requests();
    assert_eq!(requests.len(), 4, "{requests:?}");
    assert_eq!(
        requests[0].query.as_deref(),
        Some("token=abc&Expires=9"),
        "the original query must reach the origin verbatim"
    );
    server.shutdown();
}

#[test]
fn a_redirect_loop_and_an_over_long_chain_both_fail() {
    let service = service(Limits::default());

    let looping = TestServer::start(Script::serving(b"x".to_vec()).redirect_loop());
    let (_c, _i, outcome) = open(&service, url(&looping.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Redirect {
                reason: RedirectRejection::Loop
            }))
        ),
        "{outcome:?}"
    );
    looping.shutdown();

    let long = TestServer::start(Script::serving(b"x".to_vec()).redirect_chain(9));
    let (_c, _i, outcome) = open(&service, url(&long.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Redirect {
                reason: RedirectRejection::TooMany
            }))
        ),
        "{outcome:?}"
    );
    long.shutdown();
}

#[test]
fn an_https_to_http_downgrade_is_refused() {
    // Staged from a plain-HTTP loopback origin by asserting the rule directly:
    // the loopback server cannot serve TLS, so the downgrade rule is proven in
    // `tests/http_response.rs` and this test pins the *service* wiring by
    // sending a redirect to an unsupported scheme, which travels the same path.
    let server =
        TestServer::start(Script::serving(b"x".to_vec()).redirect_to("ftp://example.com/a.mp3"));
    let service = service(Limits::default());
    let (_c, _i, outcome) = open(&service, url(&server.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Redirect {
                reason: RedirectRejection::UnsupportedScheme
            }))
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

/// One path-routed reply for the `Script::documents` router, which serves any
/// client — media fetch included — by path, so a media redirect chain can be
/// staged hop by hop with a status of the test's choosing (the media-side
/// `redirect_chain`/`redirect_to` helpers hard-code 302).
fn route(path: &str, status: u16, headers: Vec<(&str, &str)>, body: &[u8]) -> DocumentReply {
    DocumentReply {
        path: path.to_string(),
        status,
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: body.to_vec(),
        conditional: false,
        header_delay: Duration::ZERO,
    }
}

#[test]
fn only_the_five_redirect_statuses_are_followed() {
    // The document transport's rule (§3.2), which media must match: only
    // 301/302/303/307/308 are redirects. 302 is already pinned by
    // `redirects_are_followed_up_to_the_limit_and_the_query_survives`; the
    // other four are checked here, followed by the two the old media loop
    // wrongly followed — 304 and 305 answer "not a redirect" even when a
    // `Location` header is present, so the fetch must answer for the status
    // itself instead of chasing the header.
    let service = service(Limits::default());
    for status in [301u16, 303, 307, 308] {
        let server = TestServer::start(Script::documents(vec![
            route("/a", status, vec![("Location", "/b")], b""),
            route("/b", 200, vec![], b"final"),
        ]));
        let (channel, _interrupt, accepted) = open(&service, url(&server.url("/a")), 0);
        match accepted {
            Ok(accepted) => assert_eq!(accepted.redirects, 1, "status {status}"),
            Err(outcome) => panic!("status {status} must be followed: {outcome:?}"),
        }
        assert_eq!(drain(&channel), b"final", "status {status}");
        assert_eq!(server.requests().len(), 2, "status {status}");
        server.shutdown();
    }

    for status in [304u16, 305] {
        let server = TestServer::start(Script::documents(vec![
            route("/a", status, vec![("Location", "/b")], b""),
            route("/b", 200, vec![], b"never reached"),
        ]));
        let (_channel, _interrupt, outcome) = open(&service, url(&server.url("/a")), 0);
        assert!(
            matches!(
                outcome,
                Err(HeaderOutcome::Failed(RemoteFailure::Status {
                    status: refused,
                    ..
                })) if refused == status
            ),
            "status {status} must not be followed: {outcome:?}"
        );
        assert_eq!(
            server.requests().len(),
            1,
            "status {status}: the Location was chased anyway"
        );
        server.shutdown();
    }
}

#[test]
fn a_redirect_status_without_a_location_is_a_status_failure() {
    // The document transport's own playbook: a redirect status whose headers
    // name no target is answered as the status it is, never guessed at.
    let server = TestServer::start(Script::documents(vec![route("/a", 302, vec![], b"")]));
    let service = service(Limits::default());
    let (_channel, _interrupt, outcome) = open(&service, url(&server.url("/a")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Status {
                status: 302,
                ..
            }))
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_redirect_target_with_embedded_credentials_is_refused() {
    // The document transport re-runs the source policy on every hop target;
    // the media transport must refuse the same URL it would refuse as input.
    // `accept_redirect` alone passes this Location (http, no downgrade, no
    // loop), so without the re-validation the credentials-bearing target
    // below would simply be fetched — and `/b` is a perfectly good media
    // route, so on the old behavior this fetch *succeeds*.
    let target = TestServer::start(Script::serving(b"final".to_vec()));
    let origin = TestServer::start(Script::documents(vec![route(
        "/a",
        302,
        vec![(
            "Location",
            &format!("http://alice:hunter2@127.0.0.1:{}/b", target.port()),
        )],
        b"",
    )]));
    let service = service(Limits::default());
    let (_channel, _interrupt, outcome) = open(&service, url(&origin.url("/a")), 0);
    match outcome {
        Err(HeaderOutcome::Failed(failure @ RemoteFailure::InvalidSource { .. })) => {
            // The target is attacker-chosen text; the rendered failure must
            // carry neither the credentials nor the query.
            let rendered = format!("{failure}");
            assert!(
                rendered.contains("credentials"),
                "the reason must name the credentials: {rendered}"
            );
            assert!(
                !rendered.contains("hunter2") && !rendered.contains("alice"),
                "the embedded credentials reached a diagnostic: {rendered}"
            );
        }
        Err(other) => panic!(
            "a credentials-bearing redirect target must fail as InvalidSource, got {other:?}"
        ),
        Ok(accepted) => panic!("the credentials-bearing target was fetched: {accepted:?}"),
    }
    assert!(
        target.requests().is_empty(),
        "the refused target was requested anyway: {:?}",
        target.requests()
    );
    origin.shutdown();
    target.shutdown();
}

#[test]
fn the_range_validator_is_not_forwarded_across_a_cross_origin_redirect() {
    // A validator identifies a representation of *one* resource, so — as the
    // document transport already refuses for its conditional headers — the
    // media transport must not hand B's `If-Range` validator to A, the
    // redirecting origin that never produced it. The hop that did produce it
    // still receives it: scoping is by URL, not a blanket drop, so the
    // in-place resume below keeps its changed-content protection where it is
    // sound.
    //
    // B serves a ranged body with a strong ETag and truncates its first
    // response, so the fetch resumes in place: A is asked again, redirects to
    // B again, and the resumed request is the one whose headers matter.
    const KIB: usize = 1024;
    let target = TestServer::start(
        Script::serving(vec![7u8; 512 * KIB])
            .truncate_body_after(200 * KIB)
            .truncate_only_first_response(),
    );
    let origin = TestServer::start(Script::documents(vec![route(
        "/audio",
        302,
        vec![(
            "Location",
            &format!("http://127.0.0.1:{}/audio", target.port()),
        )],
        b"",
    )]));
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&origin.url("/audio")), 0);
    assert!(accepted.is_ok(), "opening should succeed: {accepted:?}");
    assert_eq!(drain(&channel), vec![7u8; 512 * KIB]);

    let origin_requests = origin.requests();
    assert_eq!(origin_requests.len(), 2, "one opening hop, one resume hop");
    for (index, request) in origin_requests.iter().enumerate() {
        assert!(
            request.header("if-range").is_none(),
            "hop {index} forwarded the target's validator to the redirecting origin: {request:?}"
        );
    }
    let target_requests = target.requests();
    assert_eq!(
        target_requests.len(),
        2,
        "one opening body, one resumed body"
    );
    assert_eq!(
        target_requests[1].header("if-range"),
        Some("\"v1\""),
        "the hop that established the validator must still receive it: {target_requests:?}"
    );
    origin.shutdown();
    target.shutdown();
}

#[test]
fn a_status_failure_is_reported_with_its_status() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).status(503));
    let service = service(Limits::default());
    let (_c, _i, outcome) = open(&service, url(&server.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Status {
                status: 503,
                ..
            }))
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_header_wait_that_elapses_reports_a_headers_timeout() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let service = service(Limits::brisk());
    let (_c, _i, outcome) = open(&service, url(&server.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Timeout {
                phase: Phase::Headers
            }))
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_truncated_body_fails_rather_than_reporting_eof() {
    // H8. This is the single most important rule in §7: a short body that read
    // back as EOF becomes a completed track and a destroyed checkpoint.
    let server = TestServer::start(Script::serving(vec![5u8; 8192]).truncate_body_after(1024));
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    assert!(
        accepted.is_ok(),
        "opening should succeed; the body fails later"
    );

    let mut buffer = [0u8; 4096];
    let mut seen = 0usize;
    let outcome = loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(5)) {
            ReadOutcome::Bytes(n) => seen += n,
            other => break other,
        }
    };
    assert!(seen >= 1024, "the delivered prefix was lost: {seen}");
    assert!(
        matches!(
            outcome,
            ReadOutcome::Failed(RemoteFailure::TruncatedBody { .. })
        ),
        "a truncated body must not read back as EOF: {outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_retirement_during_a_header_wait_wakes_it_without_releasing_the_server() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let service = service(Limits::default());
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = interrupt.begin();
    let wait = service.fetch(
        FetchRequest {
            origin: url(&server.url("/audio")),
            start: 0,
            established: None,
            operation: Operation::Open,
        },
        channel.clone(),
        generation,
    );
    // A non-empty `server.requests()` would only prove the *fetch task*
    // reached the server — the waiter thread below could still never have
    // been scheduled, and every assertion here would pass just the same even
    // if `retire()` landed before `wait_for_headers` ever parked. Real proof
    // that the wait was entered is that `wait_for_headers` calls
    // `service.service()`, which only happens *after* a `wait_timeout` slice
    // — so block on that flag instead of on the server having seen anything.
    let hook = FlagHook::default();
    let entered = Arc::clone(&hook.0);
    let waiter = std::thread::spawn(move || wait.wait(&hook, Duration::from_secs(30)));
    let started = std::time::Instant::now();
    while !entered.load(Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait was never entered"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.retire();
    let interrupted_at = std::time::Instant::now();
    match waiter.join() {
        Ok(outcome) => {
            assert!(
                matches!(outcome, Err(HeaderOutcome::Retired)),
                "{outcome:?}"
            );
            assert!(interrupted_at.elapsed() < Duration::from_secs(1));
        }
        Err(_) => panic!("the waiter thread panicked"),
    }
    server.shutdown();
}

#[test]
fn a_body_stall_exceeding_the_limit_reports_a_stall_timeout() {
    let server = TestServer::start(Script::serving(vec![2u8; 4096]).stall_body_after(512));
    let service = service(Limits::brisk());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    assert!(
        accepted.is_ok(),
        "opening should succeed; the stall is in the body"
    );

    let mut buffer = [0u8; 4096];
    let mut seen = 0usize;
    let outcome = loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(5)) {
            ReadOutcome::Bytes(n) => seen += n,
            other => break other,
        }
    };
    assert!(seen >= 512, "the delivered prefix was lost: {seen}");
    assert!(
        matches!(
            outcome,
            ReadOutcome::Failed(RemoteFailure::Timeout {
                phase: Phase::Stall
            })
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

#[test]
fn an_error_with_no_advertised_length_reports_transport_not_truncation() {
    // The other side of the merged `Ok(None)`/`Err` classification: a
    // `Transfer-Encoding: chunked` 200 never sends `Content-Length`, so
    // there is no total to compare `delivered` against — a missing
    // terminator must report `Transport`, never `TruncatedBody`.
    let server = TestServer::start(
        Script::serving(vec![6u8; 4096])
            .without_ranges()
            .chunked()
            .truncate_body_after(1024),
    );
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    match &accepted {
        Ok(accepted) => assert!(
            matches!(accepted.accepted, Accepted::Sequential { len: None }),
            "{accepted:?}"
        ),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }

    let mut buffer = [0u8; 4096];
    let mut seen = 0usize;
    let outcome = loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(5)) {
            ReadOutcome::Bytes(n) => seen += n,
            other => break other,
        }
    };
    assert!(seen >= 1024, "the delivered prefix was lost: {seen}");
    assert!(
        matches!(
            outcome,
            ReadOutcome::Failed(RemoteFailure::Transport { .. })
        ),
        "an error with nothing advertised must report Transport, not TruncatedBody: {outcome:?}"
    );
    server.shutdown();
}

#[test]
fn an_over_length_body_is_refused_and_the_excess_never_reaches_a_reader() {
    // A ranged response with `Transfer-Encoding: chunked` skips `accept()`'s
    // `Content-Length` cross-check, so an overridden `Content-Range` that
    // undercounts what the server actually writes reaches the body loop
    // uncaught by the header phase — exactly the shape Important 1's clamp
    // exists for. `chunked()` on a 206 and `content_range_override` are both
    // real `TestServer` capabilities; the mismatch between what they
    // advertise and what the server actually sends is deliberate.
    let server = TestServer::start(
        Script::serving(vec![9u8; 64])
            .chunked()
            .content_range_override("bytes 0-9/*"),
    );
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    match accepted {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Ranged { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }

    let mut buffer = [0u8; 4096];
    let mut seen = 0usize;
    let outcome = loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(5)) {
            ReadOutcome::Bytes(n) => seen += n,
            other => break other,
        }
    };
    assert!(
        seen <= 10,
        "bytes past the advertised total reached a reader: {seen}"
    );
    assert!(
        matches!(
            outcome,
            ReadOutcome::Failed(RemoteFailure::InvalidRange {
                reason: RangeRejection::LengthMismatch
            })
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

#[test]
fn an_error_after_the_advertised_total_is_fully_delivered_reports_transport_not_truncation() {
    // The other side of `classify_body_end`'s boundary: a chunked 206 whose
    // overridden `Content-Range` exactly matches what the server writes, but
    // whose terminating chunk never arrives. Every advertised byte reaches
    // the reader, so this must not read back as a shortfall.
    let server = TestServer::start(
        Script::serving(vec![9u8; 64])
            .chunked()
            .content_range_override("bytes 0-63/*")
            .truncate_body_after(64),
    );
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    match accepted {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Ranged { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }

    let mut buffer = [0u8; 128];
    let mut seen = 0usize;
    let outcome = loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(5)) {
            ReadOutcome::Bytes(n) => seen += n,
            other => break other,
        }
    };
    assert_eq!(seen, 64, "every advertised byte should have been delivered");
    assert!(
        matches!(
            outcome,
            ReadOutcome::Failed(RemoteFailure::Transport { .. })
        ),
        "a shortfall-free error must report Transport, not TruncatedBody: {outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_retirement_mid_body_closes_the_request_after_the_wait_was_entered() {
    // Trickled in small pieces so the proof below survives a TCP quirk: a
    // single large `write_all` right after the peer drops its response can
    // still succeed, because the kernel accepts it into the send buffer
    // before a RST comes back — confirmed against a hand-rolled origin.
    // Detecting the close reliably needs a *second* write after the first,
    // once the RST has had a moment to arrive.
    let server = TestServer::start(
        Script::serving(vec![4u8; 4096])
            .stall_body_after(1024)
            .trickle(256, Duration::from_millis(30)),
    );
    let service = service(Limits::default());
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = interrupt.begin();
    let wait = service.fetch(
        FetchRequest {
            origin: url(&server.url("/audio")),
            start: 0,
            established: None,
            operation: Operation::Open,
        },
        channel.clone(),
        generation,
    );
    match wait.wait(&NoHook, Duration::from_secs(10)) {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Ranged { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }

    // Proof the server side genuinely stalled mid-body rather than having
    // already finished.
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the server never reached its stall point"
    );

    // Proof the *reader* has drained everything sent so far and is now
    // genuinely parked waiting for more — the same `service()`-after-a-slice
    // proof Important 4 uses for the header wait, applied to the body.
    let hook = FlagHook::default();
    let entered = Arc::clone(&hook.0);
    let reader = {
        let channel = channel.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match channel.read(&mut buffer, &hook, Duration::from_secs(30)) {
                    ReadOutcome::Bytes(_) => continue,
                    other => return other,
                }
            }
        })
    };
    let started = std::time::Instant::now();
    while !entered.load(Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the reader never parked"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let bytes_before_retire = server.bytes_written();
    interrupt.retire();
    let interrupted_at = std::time::Instant::now();
    match reader.join() {
        Ok(outcome) => {
            assert!(matches!(outcome, ReadOutcome::Retired), "{outcome:?}");
            assert!(interrupted_at.elapsed() < Duration::from_secs(1));
        }
        Err(_) => panic!("the reader thread panicked"),
    }

    // Real proof retiring closed the request rather than merely stopping the
    // read side: releasing the stalled connection thread lets it attempt to
    // keep writing, trickled in small pieces. The first piece or two may
    // still land (kernel buffering ahead of the RST), but the remaining
    // 3072 bytes cannot all arrive — a connection retire() left open would
    // deliver the rest in full within this window.
    //
    // `reader.join()` only synchronizes with the *reader* thread's condvar
    // wake, which is immediate — it says nothing about whether the fetch
    // task's own async cancellation (a Tokio `Notify` the runtime still has
    // to schedule and poll) has actually run yet. This margin is what keeps
    // that race from being able to flip the assertion below.
    std::thread::sleep(Duration::from_millis(150));
    server.release();
    std::thread::sleep(Duration::from_millis(500));
    let delivered_after_release = server.bytes_written() - bytes_before_retire;
    assert!(
        delivered_after_release < 1024,
        "far more of the remaining body landed ({delivered_after_release} of 3072 bytes) than a \
         connection retire() should have closed would allow through"
    );
    server.shutdown();
}

const KIB: usize = 1024;

/// Reads until the channel ends, returning every byte and the terminal outcome.
fn drain_until_end(channel: &ByteChannel) -> (usize, ReadOutcome) {
    let mut buffer = [0u8; 4096];
    let mut seen = 0usize;
    loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(10)) {
            ReadOutcome::Bytes(n) => seen += n,
            other => return (seen, other),
        }
    }
}

#[test]
fn a_ranged_body_that_dies_after_progress_resumes_in_place() {
    // A CDN drops a stream nobody has read from for a while (nginx's
    // `send_timeout`, 60 s by default); when the paused reader comes back
    // it drains the buffer and then hits the dead stream. The fetch must
    // pick up where the bytes stopped, with `If-Range`, and the reader must
    // see one continuous body.
    let server = TestServer::start(
        Script::serving(vec![7u8; 512 * KIB])
            .truncate_body_after(200 * KIB)
            .truncate_only_first_response(),
    );
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    assert!(accepted.is_ok(), "opening should succeed: {accepted:?}");

    let (seen, outcome) = drain_until_end(&channel);
    assert!(matches!(outcome, ReadOutcome::Eof), "{outcome:?}");
    assert_eq!(seen, 512 * KIB, "every byte must arrive exactly once");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one reconnect, not a retry loop");
    assert_eq!(requests[1].range(), Some((200 * KIB as u64, None)));
    assert!(
        requests[1].header("if-range").is_some(),
        "the resumed request must carry the validator"
    );
    server.shutdown();
}

#[test]
fn a_reconnect_that_makes_no_progress_is_not_retried() {
    // Every response dies after 1 KiB: less than one transfer chunk, so the
    // response never proved the server is actually serving. That is the
    // spec's "no background retry loop": one truncated body, one failure.
    let server = TestServer::start(Script::serving(vec![7u8; 512 * KIB]).truncate_body_after(KIB));
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    assert!(accepted.is_ok(), "opening should succeed: {accepted:?}");

    let (_seen, outcome) = drain_until_end(&channel);
    assert!(
        matches!(
            outcome,
            ReadOutcome::Failed(RemoteFailure::TruncatedBody { .. })
        ),
        "{outcome:?}"
    );
    assert_eq!(server.requests().len(), 1, "no reconnect without progress");
    server.shutdown();
}
