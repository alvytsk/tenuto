//! M7.1 §10: what an add does, per probe outcome.
//!
//! Tests were written after `src/library.rs`'s station functions, not
//! before them — the implementer who wrote `add_station`, `remove_station`,
//! `reprobe_station` and `load_mutating_stations` was interrupted before
//! writing tests, and this file is that missing half, not a from-scratch
//! TDD pass. See `tests/m6_feed_management.rs` for the store+service
//! harness this borrows its shape from, and `tests/support/server.rs` for
//! the `Script` builders.

mod support;

use std::fs;
use std::path::Path;
use std::sync::Arc;

use support::browse::{answer, wait_for_result};
use support::server::{Script, TestServer};
use tenuto::application::browse::{BrowseRequest, BrowseResult, BrowseWorker};
use tenuto::application::feed_ops::FeedOp;
use tenuto::application::runtime::LibraryStores;
use tenuto::clock::SystemClock;
use tenuto::feed::cache::CacheStore;
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::library::{AddStationOutcome, StationRow, add_station, list_stations, reprobe_station};
use tenuto::station::store::StationStore;
use tenuto::subscription::store::SubscriptionStore;
use url::Url;

/// A fresh `StationStore` rooted at `root/stations.json`, built repeatedly
/// (never cached) so a test can read back exactly what an add wrote, the
/// same shape `tests/m6_feed_management.rs::stores` uses for subscriptions.
fn store(root: &Path) -> StationStore {
    StationStore::new(root.join("stations.json"), Arc::new(SystemClock))
}

/// A real `HttpService`, spawned once per test against a loopback server.
fn service() -> Arc<HttpService> {
    HttpService::spawn(Limits::brisk()).unwrap_or_else(|error| panic!("service: {error}"))
}

/// Runs `add_station` synchronously on the service's own runtime, mirroring
/// `tests/m4_document_fetch.rs`'s `service.handle().block_on(...)` pattern.
fn add(
    service: &Arc<HttpService>,
    store: &StationStore,
    url: &str,
) -> Result<AddStationOutcome, tenuto::feed::error::FeedError> {
    service.handle().block_on(add_station(service, store, url))
}

fn reprobe(
    service: &Arc<HttpService>,
    store: &StationStore,
    slug: &str,
) -> Result<AddStationOutcome, tenuto::feed::error::FeedError> {
    service
        .handle()
        .block_on(reprobe_station(service, store, slug))
}

// --- Task 5: browse worker wiring (M7.1 §6) -------------------------------

/// A `LibraryStores` rooted at `root`, the station store built by [`store`]
/// above so a worker-level test seeds and reads through the same helper the
/// library-level tests above it use.
fn stores(root: &Path) -> LibraryStores {
    LibraryStores {
        subscriptions: SubscriptionStore::new(
            root.join("subscriptions.json"),
            Arc::new(SystemClock),
        ),
        cache: CacheStore::new(root.join("feeds")),
        stations: store(root),
    }
}

/// Sends `Stations` and waits for its listing, over the shared poller
/// (`tests/support/browse.rs`) `answer` above also uses.
fn list(worker: &BrowseWorker) -> Result<Vec<StationRow>, String> {
    worker.request(BrowseRequest::Stations);
    match wait_for_result(worker) {
        BrowseResult::Stations(rows) => rows,
        other => panic!("expected a station listing, got {other:?}"),
    }
}

#[test]
fn a_live_station_is_saved_with_its_identity() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo("https://cdn.example.org/logo.svg".to_string()),
    );
    let http = service();
    let st = store(root.path());

    let outcome = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified { slug, identity } = outcome else {
        panic!("expected Verified, got {outcome:?}");
    };
    assert_eq!(slug, "test-radio");
    assert_eq!(identity.name.as_deref(), Some("Test Radio"));
    assert_eq!(identity.genre.as_deref(), Some("Lofi"));
    assert_eq!(identity.bitrate_kbps, Some(128));
    assert_eq!(
        identity.logo,
        Some(Url::parse("https://cdn.example.org/logo.svg").unwrap())
    );

    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].slug, "test-radio");
    assert_eq!(rows[0].identity, Some(identity));

    let snapshot = st.read_snapshot().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(snapshot.stations.len(), 1);
    assert!(
        snapshot.stations[0].probed_at.is_some(),
        "a verified station's probed_at must be set"
    );
    server.shutdown();
}

#[test]
fn a_retryable_failure_saves_an_unverified_candidate() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::serving(Vec::new()).status(503));
    let http = service();
    let st = store(root.path());

    let outcome = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Unverified { slug, reason } = outcome else {
        panic!("expected Unverified, got {outcome:?}");
    };
    assert!(!slug.is_empty());
    assert!(!reason.is_empty());

    let snapshot = st.read_snapshot().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(snapshot.stations.len(), 1, "exactly one stored station");
    assert_eq!(snapshot.stations[0].identity, None);
    assert_eq!(snapshot.stations[0].probed_at, None);
    server.shutdown();
}

#[test]
fn a_non_retryable_failure_saves_nothing() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::serving(Vec::new()).status(404));
    let http = service();
    let st = store(root.path());

    let error = add(&http, &st, &server.url("/radio")).expect_err("a 404 must not be saved");
    // R1: a 404 is a statement about the URL, a 503 is not.
    let text = error.to_string();
    assert!(text.contains("404"), "{text}");
    assert!(!text.contains("not a live stream"), "{text}");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn a_finite_url_is_not_a_station_and_is_not_saved() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3"));
    let http = service();
    let st = store(root.path());

    let error = add(&http, &st, &server.url("/radio")).expect_err("a finite URL is not a station");
    assert!(error.to_string().contains("not a live stream"), "{error}");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn an_icy_metaint_response_is_not_a_station_and_is_not_saved() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").live());
    let http = service();
    let st = store(root.path());

    let error =
        add(&http, &st, &server.url("/radio")).expect_err("icy-metaint must refuse the probe");
    assert!(error.to_string().contains("not a live stream"), "{error}");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn a_malformed_url_makes_no_request() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    // A never-used server, purely to observe that it is never touched.
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let _error = add(&http, &st, "https://").expect_err("an empty host must not resolve");
    assert_eq!(server.requests().len(), 0, "a malformed URL made a request");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn a_url_with_embedded_credentials_is_refused_and_never_echoed() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let error = add(&http, &st, "https://user:secret@radio.example/stream")
        .expect_err("embedded credentials must be refused");
    assert_eq!(server.requests().len(), 0, "credentials made a request");
    let text = error.to_string();
    assert!(!text.contains("user"), "{text}");
    assert!(!text.contains("secret"), "{text}");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

/// §10's last row via a different door: a URL malformed enough that
/// `url::Url::parse` itself rejects it (an empty host) must be refused
/// without ever being treated as a filesystem path. `is_url_spelling`
/// (`application::source`) only recognizes a literal `http://`/`https://`
/// prefix, so a single-slash typo like this one would, under
/// `resolve_source`, fall through to `resolve_path`: canonicalizing the raw
/// string as a local path, failing, and building a `PlaybackError::Open`
/// whose `Display` is `cannot open media {path:?}` — the *entire* raw
/// input, verbatim, including the query token. `station_identity_of` must
/// never take that branch at all.
#[test]
fn a_malformed_secret_bearing_url_is_refused_without_echoing_the_token() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let input = "https:/?token=SECRETVALUE";
    let error = add(&http, &st, input).expect_err("an empty host must be refused");
    assert_eq!(server.requests().len(), 0, "a malformed URL made a request");
    let text = error.to_string();
    assert!(!text.contains("SECRETVALUE"), "{text}");
    assert!(!text.contains(input), "{text}");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn a_duplicate_url_resolves_to_the_existing_station() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let first = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified {
        slug: first_slug, ..
    } = first
    else {
        panic!("expected Verified, got {first:?}");
    };

    let second = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::AlreadySaved {
        slug,
        identity,
        reprobe_failure,
    } = second
    else {
        panic!("expected AlreadySaved, got {second:?}");
    };
    assert_eq!(slug, first_slug);
    assert!(identity.is_some(), "the re-probe on add succeeded");
    assert_eq!(
        reprobe_failure, None,
        "a successful re-probe must not report a failure"
    );

    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert_eq!(rows.len(), 1, "a duplicate must not be saved twice");
    assert_eq!(server.requests().len(), 2, "the second add still re-probed");
    server.shutdown();
}

/// §6/§10: a duplicate add still re-probes, and when that re-probe fails
/// the failure must reach the caller through `AlreadySaved`, not be
/// swallowed — the same "record kept, failure reported" contract
/// `reprobe_station` holds for an explicit re-probe (M7.1 §6: "leaves the
/// record alone and reports the failure").
#[test]
fn a_duplicate_add_whose_reprobe_fails_reports_the_reason_and_keeps_the_old_identity() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .then(Script::from_fixture("sine-noxing.mp3")),
    );
    let http = service();
    let st = store(root.path());

    let first = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified {
        slug: first_slug,
        identity: first_identity,
    } = first
    else {
        panic!("expected Verified, got {first:?}");
    };

    let second = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::AlreadySaved {
        slug,
        identity,
        reprobe_failure,
    } = second
    else {
        panic!("expected AlreadySaved, got {second:?}");
    };
    assert_eq!(slug, first_slug);
    assert_eq!(
        identity,
        Some(first_identity),
        "a failed re-probe must not disturb the stored identity"
    );
    let reason = reprobe_failure.expect("a now-finite URL must report a re-probe failure");
    assert!(reason.contains("live"), "{reason}");

    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert_eq!(rows.len(), 1, "a duplicate must not be saved twice");
    server.shutdown();
}

#[test]
fn a_reprobe_of_a_now_finite_url_keeps_the_record_and_reports_the_failure() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .then(Script::from_fixture("sine-noxing.mp3")),
    );
    let http = service();
    let st = store(root.path());

    let added = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified { slug, identity } = added else {
        panic!("expected Verified, got {added:?}");
    };

    let before = st.read_snapshot().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(before.stations.len(), 1);

    let outcome = reprobe(&http, &st, &slug).expect_err("a now-finite URL must fail the reprobe");
    assert!(
        outcome.to_string().contains("could not be reprobed"),
        "{outcome}"
    );

    let after = st.read_snapshot().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(after.stations.len(), 1, "R1 governs entry, not eviction");
    assert_eq!(after.stations[0].slug, slug);
    assert_eq!(
        after.stations[0].identity,
        Some(identity),
        "the stale identity is kept, not cleared"
    );
    server.shutdown();
}

/// §4: an `add` against a file `load_mutating_stations` deliberately
/// preserves must return `Err` and must never touch the file's bytes.
/// Without the write guard, each of these silently replaces a preserved
/// file with a fresh one holding only the station this add just tried to
/// make.
#[test]
fn a_protected_store_unsupported_version_is_never_overwritten() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let path = root.path().join("stations.json");
    let original = br#"{"schema_version": 999, "stations": []}"#.to_vec();
    fs::write(&path, &original).unwrap_or_else(|error| panic!("seed: {error}"));

    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let _error =
        add(&http, &st, &server.url("/radio")).expect_err("an unsupported version must refuse");
    let after = fs::read(&path).unwrap_or_else(|error| panic!("reread: {error}"));
    assert_eq!(
        after, original,
        "an unsupported-version file must be untouched"
    );
    server.shutdown();
}

#[test]
fn a_protected_store_unreadable_is_never_overwritten() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let path = root.path().join("stations.json");
        let original = br#"{"schema_version": 1, "stations": []}"#.to_vec();
        fs::write(&path, &original).unwrap_or_else(|error| panic!("seed: {error}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
            .unwrap_or_else(|error| panic!("chmod: {error}"));

        // Root, and some filesystems/CI containers, ignore a 0o000 mode. A
        // quiet `return` here would let this test print a plain `ok` having
        // executed zero assertions — `cargo test` shows captured output only
        // for a *failing* test, so a silent skip is invisible on a normal
        // run. Fail loudly instead: an environment that cannot exercise this
        // path is worth surfacing, not swallowing.
        assert!(
            fs::read(&path).is_err(),
            "this environment does not enforce file mode 0o000 (running as \
             root, or a permissive filesystem); \
             a_protected_store_unreadable_is_never_overwritten cannot \
             exercise the path it targets here"
        );

        let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
        let http = service();
        let st = store(root.path());

        let _error = add(&http, &st, &server.url("/radio")).expect_err("unreadable must refuse");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|error| panic!("chmod back: {error}"));
        let after = fs::read(&path).unwrap_or_else(|error| panic!("reread: {error}"));
        assert_eq!(after, original, "an unreadable file must be untouched");
        server.shutdown();
    }
}

#[test]
fn a_protected_store_quarantine_failure_is_never_overwritten() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let path = root.path().join("stations.json");
        let original = b"not json at all".to_vec();
        fs::write(&path, &original).unwrap_or_else(|error| panic!("seed: {error}"));
        // A read-only directory: `StationStore::quarantine`'s `fs::rename`
        // needs write permission on the parent, not the file, so this is
        // what turns quarantine into `QuarantineFailed` instead of moving
        // the file aside.
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o555))
            .unwrap_or_else(|error| panic!("chmod dir: {error}"));

        let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
        let http = service();
        let st = store(root.path());

        let outcome = add(&http, &st, &server.url("/radio"));

        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("chmod dir back: {error}"));

        // Root, and some filesystems, ignore a read-only directory. As
        // above: fail loudly rather than returning quietly, so a run in
        // such an environment cannot pass having exercised nothing.
        assert!(
            outcome.is_err(),
            "this environment does not enforce a read-only directory \
             (running as root, or a permissive filesystem); \
             a_protected_store_quarantine_failure_is_never_overwritten \
             cannot exercise the path it targets here"
        );

        let after = fs::read(&path).unwrap_or_else(|error| panic!("reread: {error}"));
        assert_eq!(
            after, original,
            "a file that could not be quarantined must be untouched"
        );
        server.shutdown();
    }
}

/// M7.1 §6: the worker answers `AddStation` with a `Mutation` echoing the
/// request, the added station then shows up in `Stations`, `RemoveStation`
/// answers likewise, and the station is gone from a following `Stations`.
#[test]
fn the_worker_answers_every_station_request() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let worker = BrowseWorker::spawn(Some(stores(root.path())));

    let add_request = BrowseRequest::Op(FeedOp::AddStation {
        url: server.url("/radio"),
    });
    let (echoed, outcome) = answer(&worker, add_request.clone());
    assert_eq!(echoed, add_request);
    let text = outcome.unwrap_or_else(|error| panic!("add failed: {error}"));
    assert!(text.contains("added, verified"), "{text}");

    let rows = list(&worker).unwrap_or_else(|error| panic!("list: {error}"));
    assert_eq!(rows.len(), 1, "{rows:?}");
    let slug = rows[0].slug.clone();

    let remove_request = BrowseRequest::Op(FeedOp::RemoveStation { slug: slug.clone() });
    let (echoed, outcome) = answer(&worker, remove_request.clone());
    assert_eq!(echoed, remove_request);
    assert_eq!(outcome, Ok(format!("{slug}: removed")), "{outcome:?}");

    let rows = list(&worker).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty(), "{rows:?}");
    server.shutdown();
}

/// R3 at the worker level (no `draw()` here — see
/// `tests/m5_no_network.rs::a_restored_station_is_listed_and_drawn_without_a_request`
/// for the drawn counterpart): a station already saved is listed, and then
/// removed, with the server's request count unchanged, extending the
/// invariant to the two station requests that must never touch the network
/// (M7.1 §6).
#[test]
fn listing_and_removing_stations_open_no_connection() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());

    // Seeded directly through `add_station`, off the worker under test, so
    // the request that populates the store is never counted against it.
    let seed = store(root.path());
    let http = service();
    let outcome =
        add(&http, &seed, &server.url("/radio")).unwrap_or_else(|error| panic!("seed: {error}"));
    let AddStationOutcome::Verified { slug, .. } = outcome else {
        panic!("expected Verified, got {outcome:?}");
    };
    let seeded_requests = server.requests().len();
    assert!(seeded_requests > 0, "the seed add must have made a request");

    let worker = BrowseWorker::spawn(Some(stores(root.path())));
    let rows = list(&worker).unwrap_or_else(|error| panic!("list: {error}"));
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].slug, slug);

    let (_, removed) = answer(
        &worker,
        BrowseRequest::Op(FeedOp::RemoveStation { slug: slug.clone() }),
    );
    assert_eq!(removed, Ok(format!("{slug}: removed")), "{removed:?}");

    assert_eq!(
        server.requests().len(),
        seeded_requests,
        "listing and removing a station must open no connection"
    );
    server.shutdown();
}

/// M6 §5's correlation rule, extended to the three new station mutations:
/// each of `AddStation`, `ReprobeStation` and `RemoveStation`'s `Mutation`
/// answer echoes back the exact request it answers, round-trip by
/// round-trip. (This test is strictly serial — one request outstanding at a
/// time — so it proves per-round-trip echo correctness across the three
/// variants, not the concurrent case the rule exists for; nothing here
/// exercises a late answer arriving after its request was superseded.)
#[test]
fn every_station_mutation_echoes_the_request_it_answers() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .then(Script::from_fixture("sine-noxing.mp3").icy_station()),
    );
    let worker = BrowseWorker::spawn(Some(stores(root.path())));

    let add_request = BrowseRequest::Op(FeedOp::AddStation {
        url: server.url("/radio"),
    });
    let (echoed, outcome) = answer(&worker, add_request.clone());
    assert_eq!(echoed, add_request);
    outcome.unwrap_or_else(|error| panic!("add failed: {error}"));

    let rows = list(&worker).unwrap_or_else(|error| panic!("list: {error}"));
    let slug = rows[0].slug.clone();

    let reprobe_request = BrowseRequest::Op(FeedOp::ReprobeStation { slug: slug.clone() });
    let (echoed, outcome) = answer(&worker, reprobe_request.clone());
    assert_eq!(echoed, reprobe_request);
    outcome.unwrap_or_else(|error| panic!("reprobe failed: {error}"));

    let remove_request = BrowseRequest::Op(FeedOp::RemoveStation { slug: slug.clone() });
    let (echoed, outcome) = answer(&worker, remove_request.clone());
    assert_eq!(echoed, remove_request);
    outcome.unwrap_or_else(|error| panic!("remove failed: {error}"));

    server.shutdown();
}
