mod support;

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};

use support::server::{Script, TestServer};
use symphonia::core::io::MediaSource;
use tenuto::http::channel::{SourceInterrupt, WaitHook};
use tenuto::http::error::RemoteFailure;
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::http::source::{HttpMediaSource, is_retired, remote_cause};
use url::Url;

struct NoHook;
impl WaitHook for NoHook {
    fn service(&self) {}
}

#[allow(clippy::unwrap_used)] // A URL built from a bound loopback port.
fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn service() -> Arc<HttpService> {
    match HttpService::spawn(Limits::default()) {
        Ok(service) => service,
        Err(error) => panic!("the HTTP service must start: {error}"),
    }
}

fn body() -> Vec<u8> {
    (0..8192u32).map(|i| (i % 251) as u8).collect()
}

/// Open with nothing to probe: opening is over as soon as this returns, so
/// every wait below is bounded by `limits.stall` alone. The opening deadline
/// is exercised by `prepare`'s own tests.
fn open(server: &TestServer, interrupt: Arc<SourceInterrupt>) -> HttpMediaSource {
    match HttpMediaSource::open_and_probe(
        service(),
        url(&server.url("/audio")),
        interrupt,
        Arc::new(NoHook),
        Limits::default(),
        |source| source,
    ) {
        Ok(source) => source,
        Err(error) => panic!("opening must succeed: {error}"),
    }
}

/// Default limits with a 4096-byte probe cap.
fn capped() -> Limits {
    Limits {
        probe_bytes: 4096,
        ..Limits::default()
    }
}

#[test]
fn a_range_capable_source_reports_its_length_and_is_seekable() {
    let server = TestServer::start(Script::serving(body()));
    let source = open(
        &server,
        SourceInterrupt::new(Limits::default().buffer_bytes),
    );
    assert_eq!(source.byte_len(), Some(8192));
    assert!(source.is_seekable());
    assert_eq!(
        source.evidence(),
        tenuto::media::capabilities::SourceEvidence {
            byte_len: Some(8192),
            byte_seekable: true,
            live: false,
            demuxer: tenuto::media::capabilities::DemuxerSeek::Unproven,
        }
    );
    server.shutdown();
}

#[test]
fn a_range_ignoring_source_is_not_seekable_but_still_has_a_length() {
    let server = TestServer::start(Script::serving(body()).without_ranges());
    let source = open(
        &server,
        SourceInterrupt::new(Limits::default().buffer_bytes),
    );
    assert!(!source.is_seekable());
    assert_eq!(source.byte_len(), Some(8192));
    server.shutdown();
}

#[test]
fn reading_to_the_end_yields_the_body_and_then_a_single_clean_eof() {
    let server = TestServer::start(Script::serving(body()));
    let mut source = open(
        &server,
        SourceInterrupt::new(Limits::default().buffer_bytes),
    );
    let mut all = Vec::new();
    match source.read_to_end(&mut all) {
        Ok(_) => {}
        Err(error) => panic!("reading must succeed: {error}"),
    }
    assert_eq!(all, body());
    match source.read(&mut [0u8; 8]) {
        Ok(0) => {}
        other => panic!("expected a clean EOF, got {other:?}"),
    }
    server.shutdown();
}

#[test]
fn seeking_forward_issues_a_range_request_at_that_byte() {
    // H2's byte half.
    let server = TestServer::start(Script::serving(body()));
    let mut source = open(
        &server,
        SourceInterrupt::new(Limits::default().buffer_bytes),
    );
    match source.seek(SeekFrom::Start(4096)) {
        Ok(position) => assert_eq!(position, 4096),
        Err(error) => panic!("seeking must succeed: {error}"),
    }
    let mut head = [0u8; 4];
    match source.read_exact(&mut head) {
        Ok(()) => {}
        Err(error) => panic!("reading after a seek must succeed: {error}"),
    }
    assert_eq!(
        head,
        [body()[4096], body()[4097], body()[4098], body()[4099]]
    );

    let ranges: Vec<_> = server.requests().iter().filter_map(|r| r.range()).collect();
    assert!(
        ranges.contains(&(4096, None)),
        "no range request at 4096: {ranges:?}"
    );
    server.shutdown();
}

#[test]
fn seeking_to_the_known_byte_eof_answers_locally_without_a_request() {
    // §7: "Seeking to a known byte EOF may return EOF locally without
    // requesting it" — because the server would answer 416, and a 416 is not
    // completion.
    let server = TestServer::start(Script::serving(body()));
    let mut source = open(
        &server,
        SourceInterrupt::new(Limits::default().buffer_bytes),
    );
    let before = server.requests().len();
    match source.seek(SeekFrom::Start(8192)) {
        Ok(position) => assert_eq!(position, 8192),
        Err(error) => panic!("seeking to EOF must succeed: {error}"),
    }
    match source.read(&mut [0u8; 8]) {
        Ok(0) => {}
        other => panic!("expected EOF at the byte end, got {other:?}"),
    }
    assert_eq!(
        server.requests().len(),
        before,
        "a 416 was requested needlessly"
    );
    server.shutdown();
}

#[test]
fn a_retired_read_is_recovered_through_symphonias_own_error_type() {
    // The two unwraps that a `source()`-only walk misses. Symphonia implements
    // the deprecated `cause()`, and io::Error::source() yields the payload's
    // source rather than the payload. Both are exercised here by wrapping the
    // error exactly as the decoder does.
    let failure = RemoteFailure::TruncatedBody { missing: 9 };
    let io = std::io::Error::other(tenuto::http::source::RemoteIoError(failure.clone()));
    let wrapped = symphonia::core::errors::Error::IoError(io);
    assert_eq!(remote_cause(&wrapped), Some(failure));

    let cancelled = symphonia::core::errors::Error::IoError(std::io::Error::other(
        tenuto::http::source::RemoteIoError(RemoteFailure::Cancelled),
    ));
    assert!(
        is_retired(&cancelled),
        "a wrapped retirement was not recognised"
    );

    // A decode error that carries no remote cause must not be mistaken for one.
    let decode = symphonia::core::errors::Error::DecodeError("bad frame");
    assert_eq!(remote_cause(&decode), None);
    assert!(!is_retired(&decode));
}

// `ReadBytes::read_buf_exact` is symphonia's own inherent-looking trait
// method; clippy flags it only because a method of the same name might land
// in `std` one day, which is not this crate's concern.
#[allow(unstable_name_collisions)]
#[test]
fn a_retirement_does_not_use_interrupted_and_does_not_spin_a_retry_loop() {
    // symphonia-core-0.6.1 media_source_stream.rs:432 swallows Interrupted
    // inside `while !buf.is_empty()`, and std's read_exact does the same, so an
    // Interrupted that latches is an infinite loop rather than a terminating
    // one. Drive the real buffered reader to prove the error propagates.
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadBytes};

    let server = TestServer::start(Script::serving(body()).stall_body_after(16));
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let source = open(&server, Arc::clone(&interrupt));
    assert!(server.wait_until_stalled(Duration::from_secs(5)));
    interrupt.retire();

    let mut stream = MediaSourceStream::new(
        Box::new(source),
        MediaSourceStreamOptions {
            buffer_len: 64 * 1024,
        },
    );
    let started = Instant::now();
    let mut sink = [0u8; 8192];
    let error = match stream.read_buf_exact(&mut sink) {
        Err(error) => error,
        Ok(()) => panic!("a retired read filled the buffer"),
    };
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "read_buf_exact spun for {:?}",
        started.elapsed()
    );
    assert_ne!(
        error.kind(),
        std::io::ErrorKind::Interrupted,
        "Interrupted is retried by both read_exact and symphonia's reader"
    );
    assert!(is_retired(&error), "the retirement was lost: {error}");
    server.shutdown();
}

#[test]
fn a_retired_read_is_distinguishable_from_an_error_and_stays_retired() {
    // §8: errors and cancellation remain distinguishable even after Symphonia
    // wraps the I/O error. Latching is not what terminates a retry loop — the
    // non-retryable error kind is — but a retired source must still answer
    // identically however many times it is asked.
    let server = TestServer::start(Script::serving(body()).stall_body_after(16));
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let mut source = open(&server, Arc::clone(&interrupt));
    assert!(server.wait_until_stalled(Duration::from_secs(5)));
    interrupt.retire();

    let started = Instant::now();
    let first = source.read(&mut [0u8; 4096]);
    assert!(started.elapsed() < Duration::from_secs(1));
    match first {
        Err(error) => {
            assert!(
                is_retired(&error),
                "not recognised as a retirement: {error}"
            );
            assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
        }
        Ok(n) => panic!("expected a retirement, read {n} bytes"),
    }
    match source.read(&mut [0u8; 4096]) {
        Err(error) => assert!(is_retired(&error), "the retirement did not latch: {error}"),
        Ok(n) => panic!("expected a latched retirement, read {n} bytes"),
    }
    server.shutdown();
}

#[test]
fn a_truncated_body_surfaces_as_a_typed_remote_failure_not_as_eof() {
    let server = TestServer::start(Script::serving(body()).truncate_body_after(1024));
    let mut source = open(
        &server,
        SourceInterrupt::new(Limits::default().buffer_bytes),
    );
    let mut sink = Vec::new();
    let error = match source.read_to_end(&mut sink) {
        Err(error) => error,
        Ok(n) => panic!("a truncated body read back as {n} clean bytes"),
    };
    assert!(
        matches!(
            remote_cause(&error),
            Some(RemoteFailure::TruncatedBody { .. })
        ),
        "the typed cause was lost: {error}"
    );
    server.shutdown();
}

#[test]
fn a_changed_strong_validator_on_a_seek_fails_as_resource_changed() {
    // H11. Same length, different ETag: only the validator catches it.
    let server = TestServer::start(Script::serving(body()).changing_etag_after(1));
    let mut source = open(
        &server,
        SourceInterrupt::new(Limits::default().buffer_bytes),
    );
    let outcome = source.seek(SeekFrom::Start(4096));
    let error = match outcome {
        Err(error) => error,
        Ok(_) => panic!("a replaced recording must not seek successfully"),
    };
    assert!(
        matches!(remote_cause(&error), Some(RemoteFailure::ResourceChanged)),
        "{error}"
    );
    server.shutdown();
}

#[test]
fn a_live_source_is_flagged_as_live_evidence() {
    // `.icy_station()`, not `.live()`, because `.live()` also sends
    // `icy-metaint`, which `accept` refuses as unsupported framing rather
    // than accepting as live. `icy_station()` turns ranges off itself (it
    // models a real Icecast mount, which answers `Range: bytes=0-` — always
    // sent, even on the opening request — with a plain 200), so this needs
    // no `.without_ranges()` of its own.
    let server = TestServer::start(Script::serving(body()).icy_station());
    let source = open(
        &server,
        SourceInterrupt::new(Limits::default().buffer_bytes),
    );
    assert!(source.evidence().live);
    assert_eq!(source.evidence().byte_len, None);
    server.shutdown();
}

#[test]
fn the_probe_cap_stops_a_runaway_scan() {
    // Ruling 1: the cap holds for every read the probe takes.
    let server = TestServer::start(Script::serving(vec![0u8; 1 << 20]));
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let scanned = HttpMediaSource::open_and_probe(
        service(),
        url(&server.url("/audio")),
        interrupt,
        Arc::new(NoHook),
        capped(),
        |mut source| source.read_to_end(&mut Vec::new()),
    );
    let error = match scanned {
        Ok(Err(error)) => error,
        Ok(Ok(n)) => panic!("the cap was ignored; read {n} bytes"),
        Err(error) => panic!("opening must succeed: {error}"),
    };
    assert!(
        matches!(
            remote_cause(&error),
            Some(RemoteFailure::ProbeLimitExceeded { .. })
        ),
        "{error}"
    );
    server.shutdown();
}

#[test]
fn finishing_opening_lifts_the_probe_cap_for_ordinary_playback() {
    // Fix round 1, IMPORTANT 1: finishing opening must clear the cap itself,
    // not just the `opening` flag — `Read::read`'s probe-cap check is
    // unconditional on `is_opening()`, and `consumed` only ever grows, so a
    // cap left in place after opening fails every ordinary track whose total
    // exceeds it, partway through playback. A body bigger than the cap read
    // to completion, successfully, once `open_and_probe` has returned, is the
    // regression test for that.
    let body = vec![0u8; 1 << 20];
    let server = TestServer::start(Script::serving(body.clone()));
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let mut source = match HttpMediaSource::open_and_probe(
        service(),
        url(&server.url("/audio")),
        interrupt,
        Arc::new(NoHook),
        capped(),
        |source| source,
    ) {
        Ok(source) => source,
        Err(error) => panic!("opening must succeed: {error}"),
    };

    let mut sink = Vec::new();
    match source.read_to_end(&mut sink) {
        Ok(_) => {}
        Err(error) => panic!("a cap left in place after opening blocked playback: {error}"),
    }
    assert_eq!(sink, body, "well past the 4096-byte cap, read in full");
    server.shutdown();
}
