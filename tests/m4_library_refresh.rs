//! `src/library.rs`'s two refresh functions (design doc §5.3, §5.4, §6.6):
//! `refresh` and `refresh_all`. `Rig` (`tests/support/feeds.rs`) is the
//! fixture every M4 test suite imports: a temp root, a fake clock, and the
//! subscription/cache/state stores that share it.

#[path = "support/feeds.rs"]
mod feeds;
mod support;

use std::time::Duration;

use support::server::{DocumentReply, Script, TestServer};
use tenuto::{
    feed::error::FeedError,
    http::{document::CacheValidators, error::RemoteFailure, limits::Limits, service::HttpService},
    library::{self, FollowupStep, RefreshOutcome},
    persistence::model::PersistedState,
};

/// One RSS document, one item, an ASCII title that derives to `radio-t`.
const RADIO_T: &[u8] =
    br#"<rss><channel><title>Radio T</title><item><guid>id</guid></item></channel></rss>"#;

const TRUNCATED_XML: &[u8] = include_bytes!("fixtures/feeds/truncated-xml.xml");

fn reply(path: &str, status: u16, headers: Vec<(&str, &str)>, body: &[u8]) -> DocumentReply {
    DocumentReply {
        path: path.to_string(),
        status,
        headers: headers
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.to_vec(),
        conditional: false,
        header_delay: Duration::ZERO,
    }
}

/// An RSS document with a caller-chosen title, otherwise identical to
/// [`RADIO_T`] — used to force `changed` in the title-vs-`fetch_url`
/// comparison without touching anything else.
fn feed_with_title(title: &str) -> Vec<u8> {
    format!(r#"<rss><channel><title>{title}</title><item><guid>id</guid></item></channel></rss>"#)
        .into_bytes()
}

/// Step 1, verbatim from the brief: a 304 preserves the cached
/// representation and `subscriptions.json`'s bytes exactly, while advancing
/// only `last_refreshed_at`.
#[test]
fn unchanged_preserves_representation_and_advances_check_time()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![DocumentReply {
        path: "/feed".into(),
        status: 200,
        headers: vec![("ETag".into(), "\"v1\"".into())],
        body:
            br#"<rss><channel><title>Radio T</title><item><guid>id</guid></item></channel></rss>"#
                .to_vec(),
        conditional: true,
        header_delay: Duration::ZERO,
    }]));
    let service = HttpService::spawn(Limits::brisk())?;
    service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/feed"),
        Some("radio-t"),
    ))?;
    let sub = rig.subs.read_snapshot()?.subscriptions.remove(0);
    let before = rig.cache.read(&sub)?;
    let subs_bytes = std::fs::read(rig.subs.path())?;
    let subs_mtime = std::fs::metadata(rig.subs.path())?.modified()?;
    rig.clock.advance(Duration::from_secs(60));
    let outcome = service
        .handle()
        .block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    assert!(matches!(
        outcome,
        RefreshOutcome::Unchanged { followup: None, .. }
    ));
    let after = rig.cache.read(&sub)?;
    assert_eq!(after.last_fetched_at, before.last_fetched_at);
    assert!(after.last_refreshed_at > before.last_refreshed_at);
    assert_eq!(
        serde_json::to_value(&after.episodes)?,
        serde_json::to_value(&before.episodes)?
    );
    assert_eq!(std::fs::read(rig.subs.path())?, subs_bytes);
    assert_eq!(std::fs::metadata(rig.subs.path())?.modified()?, subs_mtime);
    server.shutdown();
    Ok(())
}

// --- Step 6, group A: an unusable cache forces an unconditional fetch -----

/// Rewrites `path`'s cache entry as JSON with `parser_version` bumped so it
/// no longer matches [`tenuto::feed::cache::PARSER_VERSION`], corrupts it
/// outright, or removes it, per `mode`.
fn break_cache(path: &std::path::Path, mode: &str) -> Result<(), Box<dyn std::error::Error>> {
    match mode {
        "parser_mismatch" => {
            let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
            value["parser_version"] = serde_json::json!(9_999);
            std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
        }
        "corrupt" => std::fs::write(path, b"not json")?,
        "missing" => std::fs::remove_file(path)?,
        other => return Err(format!("unknown mode {other:?}").into()),
    }
    Ok(())
}

/// §5.4: a cache that is missing, corrupt or `parser_version`-mismatched is
/// treated as nothing to revalidate against, so `refresh` sends no
/// conditional headers and accepts whatever the server answers — here, a
/// full 200 — recovering the feed rather than propagating the read failure.
fn unusable_cache_forces_unconditional_refetch(
    mode: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        RADIO_T,
    )]));
    let sub = rig.seed(RADIO_T, &server.url("/feed"))?;
    let path = rig.cache.path_for(&sub.feed_id)?;
    break_cache(&path, mode)?;

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = service
        .handle()
        .block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    assert!(
        matches!(outcome, RefreshOutcome::Updated { followup: None, .. }),
        "{outcome:?}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("if-none-match"), None);
    assert_eq!(requests[0].header("if-modified-since"), None);
    assert!(rig.cache.read(&sub).is_ok());
    server.shutdown();
    Ok(())
}

#[test]
fn a_parser_mismatched_cache_forces_an_unconditional_refetch()
-> Result<(), Box<dyn std::error::Error>> {
    unusable_cache_forces_unconditional_refetch("parser_mismatch")
}

#[test]
fn a_corrupt_cache_forces_an_unconditional_refetch() -> Result<(), Box<dyn std::error::Error>> {
    unusable_cache_forces_unconditional_refetch("corrupt")
}

#[test]
fn a_missing_cache_forces_an_unconditional_refetch() -> Result<(), Box<dyn std::error::Error>> {
    unusable_cache_forces_unconditional_refetch("missing")
}

// --- Step 6, group B: an unsolicited 304 against an unusable cache fails --

/// A server that always answers 304, regardless of what (if anything) was
/// sent, must not be trusted just because the local cache happens to be
/// unusable: `refresh` sends no conditional header in that state (group A),
/// so this 304 is unsolicited, and the application layer refuses to
/// fabricate a `CachedFeed` from nothing.
fn unsolicited_304_against_unusable_cache_fails(
    mode: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply("/feed", 304, vec![], b"")]));
    let sub = rig.seed(RADIO_T, &server.url("/feed"))?;
    let path = rig.cache.path_for(&sub.feed_id)?;
    break_cache(&path, mode)?;
    let before = if mode == "missing" {
        None
    } else {
        Some(std::fs::read(&path)?)
    };

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = service
        .handle()
        .block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    match outcome {
        RefreshOutcome::Failed {
            error: FeedError::Remote(RemoteFailure::UnsolicitedNotModified),
            ..
        } => {}
        other => {
            return Err(
                format!("expected an UnsolicitedNotModified failure, got {other:?}").into(),
            );
        }
    }
    match before {
        Some(bytes) => assert_eq!(
            std::fs::read(&path)?,
            bytes,
            "the cache must not be replaced"
        ),
        None => assert!(!path.exists(), "a missing cache must stay missing"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_server_that_always_answers_304_fails_against_a_parser_mismatched_cache()
-> Result<(), Box<dyn std::error::Error>> {
    unsolicited_304_against_unusable_cache_fails("parser_mismatch")
}

#[test]
fn a_server_that_always_answers_304_fails_against_a_corrupt_cache()
-> Result<(), Box<dyn std::error::Error>> {
    unsolicited_304_against_unusable_cache_fails("corrupt")
}

#[test]
fn a_server_that_always_answers_304_fails_against_a_missing_cache()
-> Result<(), Box<dyn std::error::Error>> {
    unsolicited_304_against_unusable_cache_fails("missing")
}

// --- Step 6, group C: failures below the commit seam touch nothing -------

/// Snapshots both durable files' exact bytes, runs `refresh` expecting
/// `Failed`, then asserts neither file moved at all — the failure never
/// reached the commit seam in [`library::refresh_work`] (private, but this
/// is exactly what its doc comment promises).
fn failure_touches_neither_store(
    rig: &feeds::Rig,
    service: &HttpService,
    cache_path: &std::path::Path,
) -> Result<RefreshOutcome, Box<dyn std::error::Error>> {
    let cache_before = std::fs::read(cache_path)?;
    let subs_before = std::fs::read(rig.subs.path())?;
    let outcome = service
        .handle()
        .block_on(library::refresh(service, &rig.subs, &rig.cache, "radio-t"))?;
    assert!(
        matches!(outcome, RefreshOutcome::Failed { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(cache_path)?,
        cache_before,
        "the cache must be untouched"
    );
    assert_eq!(
        std::fs::read(rig.subs.path())?,
        subs_before,
        "subscriptions.json must be untouched"
    );
    Ok(outcome)
}

/// A malformed cached validator (a raw CR/LF, which `HeaderValue` refuses)
/// fails while building the request itself — deterministic, and exactly the
/// same shape of transport failure `m4_document_protocol.rs`'s
/// `an_invalid_cached_header_value_fails_safely_rather_than_panicking` uses
/// — rather than relying on a real connection refusal's timing.
#[test]
fn a_transport_failure_leaves_the_cache_and_subscription_untouched()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        RADIO_T,
    )]));
    let sub = rig.seed(RADIO_T, &server.url("/feed"))?;
    let mut cached = rig.cache.read(&sub)?;
    cached.validators = CacheValidators {
        url: sub.fetch_url.clone(),
        etag: Some("bad\r\nvalue".to_string()),
        last_modified: None,
    };
    rig.cache.save(&sub, &cached)?;
    let path = rig.cache.path_for(&sub.feed_id)?;

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = failure_touches_neither_store(&rig, &service, &path)?;
    match outcome {
        RefreshOutcome::Failed {
            error: FeedError::Remote(RemoteFailure::Transport { .. }),
            ..
        } => {}
        other => return Err(format!("expected a Transport failure, got {other:?}").into()),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn an_http_failure_leaves_the_cache_and_subscription_untouched()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    // `rig.seed` never touches the network, so the server's only script can
    // already be the refresh attempt's failing response.
    let server = TestServer::start(Script::documents(vec![reply("/feed", 500, vec![], b"")]));
    let sub = rig.seed(RADIO_T, &server.url("/feed"))?;
    let path = rig.cache.path_for(&sub.feed_id)?;

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = failure_touches_neither_store(&rig, &service, &path)?;
    match outcome {
        RefreshOutcome::Failed {
            error: FeedError::Remote(RemoteFailure::Status { status: 500, .. }),
            ..
        } => {}
        other => return Err(format!("expected a Status(500) failure, got {other:?}").into()),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_parse_failure_leaves_the_cache_and_subscription_untouched()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        TRUNCATED_XML,
    )]));
    let sub = rig.seed(RADIO_T, &server.url("/feed"))?;
    let path = rig.cache.path_for(&sub.feed_id)?;

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = failure_touches_neither_store(&rig, &service, &path)?;
    match outcome {
        RefreshOutcome::Failed {
            error: FeedError::Malformed { .. },
            ..
        } => {}
        other => return Err(format!("expected Malformed, got {other:?}").into()),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_body_limit_failure_leaves_the_cache_and_subscription_untouched()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        &[b'x'; 64],
    )]));
    let sub = rig.seed(RADIO_T, &server.url("/feed"))?;
    let path = rig.cache.path_for(&sub.feed_id)?;

    let limits = Limits {
        document_bytes: 16,
        ..Limits::brisk()
    };
    let service = HttpService::spawn(limits)?;
    let outcome = failure_touches_neither_store(&rig, &service, &path)?;
    match outcome {
        RefreshOutcome::Failed {
            error: FeedError::Remote(RemoteFailure::DocumentTooLarge { limit: 16 }),
            ..
        } => {}
        other => return Err(format!("expected DocumentTooLarge, got {other:?}").into()),
    }
    server.shutdown();
    Ok(())
}

// --- Step 6, group D: 200/redirect semantics -------------------------------

/// §5.2: there is no `last_changed_at`. A parsed 200 that happens to carry
/// exactly the same bytes as before still reports `Updated` and still
/// advances both timestamps — M4 promises fetch status, not a content hash.
#[test]
fn an_identical_body_still_reports_updated_and_advances_both_timestamps()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        RADIO_T,
    )]));
    let sub = rig.seed(RADIO_T, &server.url("/feed"))?;
    let before = rig.cache.read(&sub)?;
    let subs_bytes = std::fs::read(rig.subs.path())?;
    let subs_mtime = std::fs::metadata(rig.subs.path())?.modified()?;

    rig.clock.advance(Duration::from_secs(60));
    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = service
        .handle()
        .block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    assert!(
        matches!(outcome, RefreshOutcome::Updated { followup: None, .. }),
        "{outcome:?}"
    );
    let after = rig.cache.read(&sub)?;
    assert!(after.last_fetched_at > before.last_fetched_at);
    assert!(after.last_refreshed_at > before.last_refreshed_at);
    assert_eq!(
        serde_json::to_value(&after.episodes)?,
        serde_json::to_value(&before.episodes)?
    );
    // §5.1: "An ordinary refresh, 200 or 304, touches exactly one file: the
    // cache." Title and fetch_url are both unchanged here, so `changed` is
    // false and `subscriptions.json` must never even be opened for writing —
    // proven by exact bytes *and* mtime, not just round-tripped content.
    assert_eq!(std::fs::read(rig.subs.path())?, subs_bytes);
    assert_eq!(std::fs::metadata(rig.subs.path())?.modified()?, subs_mtime);
    server.shutdown();
    Ok(())
}

/// A title change commits to both the cache and `subscriptions.json`
/// without ever touching the immutable `slug`.
#[test]
fn a_title_update_preserves_the_slug() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        &feed_with_title("Radio T Redux"),
    )]));
    let sub = rig.seed(RADIO_T, &server.url("/feed"))?;
    assert_eq!(sub.title.as_deref(), Some("Radio T"));

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = service
        .handle()
        .block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    assert!(
        matches!(
            outcome,
            RefreshOutcome::Updated {
                followup: None,
                url_moved: None,
                ..
            }
        ),
        "{outcome:?}"
    );
    let updated = library::list_feeds(&rig.subs, &rig.cache)?;
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].slug, "radio-t");
    assert_eq!(updated[0].title.as_deref(), Some("Radio T Redux"));
    server.shutdown();
    Ok(())
}

/// A permanent redirect moves `fetch_url` and reports `url_moved`, while the
/// `FeedId` and an unrelated checkpoint are left exactly as they were —
/// `refresh` takes no `StateStore` and touches no such file at all.
#[test]
fn a_permanent_redirect_moves_fetch_url_and_preserves_id_and_checkpoints()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![
        reply("/old", 301, vec![("Location", "/new")], b""),
        reply("/new", 200, vec![], RADIO_T),
    ]));
    let sub = rig.seed(RADIO_T, &server.url("/old"))?;

    let key = format!("podcast:{}/guid:id", sub.feed_id.as_str());
    let state: PersistedState = serde_json::from_value(serde_json::json!({
        "schema_version": 4,
        "checkpoints": {
            (key): {"position": {"secs": 12, "nanos": 0}, "completed": false,
                    "touch_seq": 1, "updated_at": "2026-09-11T00:00:00Z"}
        }
    }))?;
    rig.state.write(&state)?;
    let state_before = std::fs::read(rig.state.path())?;

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = service
        .handle()
        .block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    let (url_moved, followup) = match outcome {
        RefreshOutcome::Updated {
            url_moved,
            followup,
            ..
        } => (url_moved, followup),
        other => return Err(format!("expected Updated, got {other:?}").into()),
    };
    assert!(url_moved.is_some());
    assert!(followup.is_none());

    let after = rig.subs.read_snapshot()?.subscriptions.remove(0);
    assert_eq!(after.feed_id, sub.feed_id);
    assert_eq!(after.fetch_url, server.url("/new").parse()?);
    assert_eq!(std::fs::read(rig.state.path())?, state_before);
    server.shutdown();
    Ok(())
}

/// A temporary redirect never moves `fetch_url` — only `fetched_from` in the
/// cache reflects where the representation actually came from this time.
#[test]
fn a_temporary_redirect_preserves_fetch_url() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![
        reply("/old", 302, vec![("Location", "/new")], b""),
        reply("/new", 200, vec![], RADIO_T),
    ]));
    let sub = rig.seed(RADIO_T, &server.url("/old"))?;

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = service
        .handle()
        .block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    assert!(
        matches!(
            outcome,
            RefreshOutcome::Updated {
                url_moved: None,
                followup: None,
                ..
            }
        ),
        "{outcome:?}"
    );
    let after = rig.subs.read_snapshot()?.subscriptions.remove(0);
    assert_eq!(
        after.fetch_url, sub.fetch_url,
        "a temporary redirect must not move fetch_url"
    );
    let cached = rig.cache.read(&after)?;
    assert_eq!(cached.fetched_from, server.url("/new").parse()?);
    server.shutdown();
    Ok(())
}

/// A cache-file obstruction fails before the subscription is ever touched
/// (§5.3's commit seam): even though the response also carried a permanent
/// redirect, the subscription's `fetch_url` must not move when the cache
/// commit itself never landed.
#[test]
fn a_failed_cache_save_does_not_move_the_subscription_url() -> Result<(), Box<dyn std::error::Error>>
{
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![
        reply("/old", 301, vec![("Location", "/new")], b""),
        reply("/new", 200, vec![], RADIO_T),
    ]));
    let sub = rig.seed(RADIO_T, &server.url("/old"))?;

    let cache_path = rig.cache.path_for(&sub.feed_id)?;
    std::fs::remove_file(&cache_path)?;
    std::fs::create_dir(&cache_path)?;
    std::fs::write(cache_path.join("sentinel"), b"keep")?;
    let subs_before = std::fs::read(rig.subs.path())?;

    let service = HttpService::spawn(Limits::brisk())?;
    let outcome = service
        .handle()
        .block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    assert!(
        matches!(outcome, RefreshOutcome::Failed { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(rig.subs.path())?,
        subs_before,
        "subscriptions.json must be untouched when the cache commit itself failed"
    );
    let after = rig.subs.read_snapshot()?.subscriptions.remove(0);
    assert_eq!(
        after.fetch_url, sub.fetch_url,
        "the URL must not move on a failed cache save"
    );
    assert_eq!(std::fs::read(cache_path.join("sentinel"))?, b"keep");
    server.shutdown();
    Ok(())
}

// --- Step 7: a deterministic 304-plus-redirect-plus-failed-save case ------

/// Drives one `refresh` call against a subscription whose fetch reaches
/// `stalling` after (at most) an already-completed redirect hop elsewhere:
/// `stalling` parks right after its own request headers are read and before
/// any response is written, so `wait_until_stalled` proves that request
/// already left the client before `inject` runs, and `release` lets the
/// response land for real. Deterministic by construction — the same seam
/// Task 6/12 built — never a sleep, and never more than one park/release
/// cycle against any one `TestServer`, which is what keeps it race-free:
/// nothing here ever waits on a *second* park on a gate whose first release
/// has not yet been observably completed.
fn refresh_with_injection<F>(
    service: &HttpService,
    rig: &feeds::Rig,
    stalling: &TestServer,
    slug: &str,
    inject: F,
) -> Result<RefreshOutcome, Box<dyn std::error::Error>>
where
    F: FnOnce() -> Result<(), Box<dyn std::error::Error>>,
{
    std::thread::scope(
        |scope| -> Result<RefreshOutcome, Box<dyn std::error::Error>> {
            let handle = scope.spawn(|| {
                service
                    .handle()
                    .block_on(library::refresh(service, &rig.subs, &rig.cache, slug))
            });

            assert!(
                stalling.wait_until_stalled(Duration::from_secs(5)),
                "refresh never reached the stalling server"
            );
            inject()?;
            assert!(
                stalling.release(),
                "the stalled connection was not parked where expected"
            );

            let outcome = handle.join().map_err(|_| "refresh thread panicked")??;
            Ok(outcome)
        },
    )
}

/// Step 7, verbatim intent from the brief: a cached representation whose
/// `validators.url` is B while the stored `fetch_url` is still A, scripted
/// as A —301→ B with B answering 304. The redirect hop is served by a
/// separate, non-stalling server — only the hop that actually matters (B)
/// needs to be held open — so the whole exchange still needs only one
/// park/release cycle. The subscription file is obstructed while the
/// connection to B is stalled — after the snapshot has already been loaded
/// — so the eventual `subs.save` fails while the cache revalidation still
/// commits. Restoring the path and refreshing again self-heals: the URL
/// moves to B.
#[test]
fn a_304_permanent_redirect_with_a_failed_subscription_save_self_heals_on_retry()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server_b = TestServer::start(
        Script::documents(vec![DocumentReply {
            path: "/b".into(),
            status: 200,
            headers: vec![("ETag".into(), "\"v1\"".into())],
            body: RADIO_T.to_vec(),
            conditional: true,
            header_delay: Duration::ZERO,
        }])
        .stall_headers(),
    );
    let server_a = TestServer::start(Script::documents(vec![reply(
        "/a",
        301,
        vec![("Location", &server_b.url("/b"))],
        b"",
    )]));

    let sub = rig.seed(RADIO_T, &server_b.url("/b"))?;
    let mut cached = rig.cache.read(&sub)?;
    cached.validators.etag = Some("\"v1\"".to_string());
    rig.cache.save(&sub, &cached)?;
    let mut snapshot = rig.subs.read_snapshot()?;
    snapshot.subscriptions[0].fetch_url = server_a.url("/a").parse()?;
    rig.subs.save(&snapshot)?;

    let backup = rig.subs.path().with_extension("json.testbackup");
    let service = HttpService::spawn(Limits::brisk())?;

    rig.clock.advance(Duration::from_secs(60));
    let outcome = refresh_with_injection(&service, &rig, &server_b, "radio-t", || {
        std::fs::rename(rig.subs.path(), &backup)?;
        std::fs::create_dir_all(rig.subs.path())?;
        std::fs::write(rig.subs.path().join("sentinel"), b"keep")?;
        Ok(())
    })?;

    let (url_moved, followup) = match outcome {
        RefreshOutcome::Unchanged {
            url_moved,
            followup,
            ..
        } => (url_moved, followup),
        other => return Err(format!("expected Unchanged, got {other:?}").into()),
    };
    assert!(url_moved.is_some());
    assert!(
        matches!(
            followup.as_ref().map(|f| &f.step),
            Some(FollowupStep::SaveSubscription)
        ),
        "{followup:?}"
    );
    // The cache revalidation committed even though the subscription save
    // failed.
    let after_cache = rig.cache.read(&sub)?;
    assert!(after_cache.last_refreshed_at > cached.last_refreshed_at);

    let backed_up: serde_json::Value = serde_json::from_slice(&std::fs::read(&backup)?)?;
    assert_eq!(
        backed_up["subscriptions"][0]["fetch_url"],
        serde_json::json!(server_a.url("/a"))
    );

    // Restore the test path and refresh again: nothing obstructs the save
    // this time, and the URL moves to B.
    std::fs::remove_dir_all(rig.subs.path())?;
    std::fs::rename(&backup, rig.subs.path())?;

    let outcome = refresh_with_injection(&service, &rig, &server_b, "radio-t", || Ok(()))?;
    match outcome {
        RefreshOutcome::Unchanged {
            url_moved: Some(_),
            followup: None,
            ..
        } => {}
        other => return Err(format!("expected a clean Unchanged retry, got {other:?}").into()),
    }
    let after = rig.subs.read_snapshot()?.subscriptions.remove(0);
    assert_eq!(after.fetch_url, server_b.url("/b").parse()?);
    server_a.shutdown();
    server_b.shutdown();
    Ok(())
}

/// The same failure seam as above, for a 200 that also changes the title:
/// both the URL move and the title reconciliation are dropped by the failed
/// save, then both land cleanly on retry.
#[test]
fn a_200_permanent_redirect_with_a_failed_subscription_save_self_heals_on_retry()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server_b = TestServer::start(
        Script::documents(vec![reply(
            "/b",
            200,
            vec![],
            &feed_with_title("Radio T Redux"),
        )])
        .stall_headers(),
    );
    let server_a = TestServer::start(Script::documents(vec![reply(
        "/a",
        301,
        vec![("Location", &server_b.url("/b"))],
        b"",
    )]));

    let sub = rig.seed(RADIO_T, &server_a.url("/a"))?;
    assert_eq!(sub.title.as_deref(), Some("Radio T"));

    let backup = rig.subs.path().with_extension("json.testbackup");
    let service = HttpService::spawn(Limits::brisk())?;

    let outcome = refresh_with_injection(&service, &rig, &server_b, "radio-t", || {
        std::fs::rename(rig.subs.path(), &backup)?;
        std::fs::create_dir_all(rig.subs.path())?;
        std::fs::write(rig.subs.path().join("sentinel"), b"keep")?;
        Ok(())
    })?;

    let (retained, url_moved, followup) = match outcome {
        RefreshOutcome::Updated {
            retained,
            url_moved,
            followup,
            ..
        } => (retained, url_moved, followup),
        other => return Err(format!("expected Updated, got {other:?}").into()),
    };
    assert_eq!(retained, 1);
    assert!(url_moved.is_some());
    assert!(
        matches!(
            followup.as_ref().map(|f| &f.step),
            Some(FollowupStep::SaveSubscription)
        ),
        "{followup:?}"
    );

    // The cache committed the new title even though the subscription save
    // failed.
    let after_cache = rig.cache.read(&sub)?;
    assert_eq!(after_cache.title.as_deref(), Some("Radio T Redux"));

    std::fs::remove_dir_all(rig.subs.path())?;
    std::fs::rename(&backup, rig.subs.path())?;

    let outcome = refresh_with_injection(&service, &rig, &server_b, "radio-t", || Ok(()))?;
    match outcome {
        RefreshOutcome::Updated {
            url_moved: Some(_),
            followup: None,
            ..
        } => {}
        other => return Err(format!("expected a clean Updated retry, got {other:?}").into()),
    }
    let after = rig.subs.read_snapshot()?.subscriptions.remove(0);
    assert_eq!(after.fetch_url, server_b.url("/b").parse()?);
    assert_eq!(after.title.as_deref(), Some("Radio T Redux"));
    server_a.shutdown();
    server_b.shutdown();
    Ok(())
}

/// `refresh_all`'s batch consistency (§6.6): one valid feed, one hard HTTP
/// failure, and one feed whose own subscription save is obstructed — all
/// three outcomes travel in the returned vector, in subscription order, and
/// the obstructed feed's failure does not touch the other two. Each feed is
/// served by its own `TestServer`, so only the third needs `stall_headers`
/// and only ever sees one park/release cycle.
#[test]
fn refresh_all_reports_all_three_outcomes_in_subscription_order()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server1 = TestServer::start(Script::documents(vec![reply(
        "/f1",
        200,
        vec![],
        &feed_with_title("Feed One"),
    )]));
    let server2 = TestServer::start(Script::documents(vec![reply(
        "/f2",
        200,
        vec![],
        &feed_with_title("Feed Two"),
    )]));
    let server3 = TestServer::start(Script::documents(vec![reply(
        "/f3",
        200,
        vec![],
        &feed_with_title("Feed Three"),
    )]));

    let service = HttpService::spawn(Limits::brisk())?;
    for (server, path, slug) in [
        (&server1, "/f1", "feed-one"),
        (&server2, "/f2", "feed-two"),
        (&server3, "/f3", "feed-three"),
    ] {
        service.handle().block_on(library::subscribe(
            &service,
            &rig.subs,
            &rig.cache,
            &server.url(path),
            Some(slug),
        ))?;
    }

    // Restart each server on its own port with the refresh-time script:
    // feed-one is an unchanged 200, feed-two a hard HTTP failure, and
    // feed-three a title change whose own subscription save will be
    // obstructed.
    let port1 = server1.port();
    server1.shutdown();
    let server1 = TestServer::start_on(
        port1,
        Script::documents(vec![reply(
            "/f1",
            200,
            vec![],
            &feed_with_title("Feed One"),
        )]),
    );
    let port2 = server2.port();
    server2.shutdown();
    let server2 = TestServer::start_on(
        port2,
        Script::documents(vec![reply("/f2", 500, vec![], b"")]),
    );
    let port3 = server3.port();
    server3.shutdown();
    let server3 = TestServer::start_on(
        port3,
        Script::documents(vec![reply(
            "/f3",
            200,
            vec![],
            &feed_with_title("Feed Three Updated"),
        )])
        .stall_headers(),
    );

    let outcomes = std::thread::scope(
        |scope| -> Result<Vec<RefreshOutcome>, Box<dyn std::error::Error>> {
            let handle = scope.spawn(|| {
                service
                    .handle()
                    .block_on(library::refresh_all(&service, &rig.subs, &rig.cache))
            });

            // feed-three is the only stalling server, and this batch's only
            // park/release cycle: obstruct subscriptions.json while it is
            // held open, so only this feed's own save fails.
            assert!(
                server3.wait_until_stalled(Duration::from_secs(5)),
                "feed-three never reached the server"
            );
            std::fs::remove_file(rig.subs.path())?;
            std::fs::create_dir_all(rig.subs.path())?;
            std::fs::write(rig.subs.path().join("sentinel"), b"keep")?;
            assert!(server3.release(), "feed-three was not parked as expected");

            Ok(handle.join().map_err(|_| "refresh_all thread panicked")??)
        },
    )?;

    assert_eq!(outcomes.len(), 3);
    match &outcomes[0] {
        RefreshOutcome::Updated {
            slug,
            followup: None,
            ..
        } => assert_eq!(slug, "feed-one"),
        other => return Err(format!("expected feed-one Updated, got {other:?}").into()),
    }
    match &outcomes[1] {
        RefreshOutcome::Failed { slug, .. } => assert_eq!(slug, "feed-two"),
        other => return Err(format!("expected feed-two Failed, got {other:?}").into()),
    }
    match &outcomes[2] {
        RefreshOutcome::Updated {
            slug,
            followup: Some(f),
            ..
        } => {
            assert_eq!(slug, "feed-three");
            assert!(matches!(f.step, FollowupStep::SaveSubscription));
        }
        other => {
            return Err(
                format!("expected feed-three Updated with a followup, got {other:?}").into(),
            );
        }
    }
    server1.shutdown();
    server2.shutdown();
    server3.shutdown();
    Ok(())
}

// --- §6.6/§8.4: enumeration itself, as distinct from any per-feed result --

/// §8.4: "`refresh_all` over an unreadable `subscriptions.json` returns the
/// outer `Err`, not a fabricated per-feed `Failed`." A directory sitting at
/// the subscriptions path (the same deterministic unreadable case the
/// storage suites already use) makes `load_mutating` fail before
/// `refresh_all` ever has a slug to attach a `Failed` to, so the enumeration
/// failure must surface as the function's own `Err`, never as `Ok(vec![...])`
/// containing an invented outcome.
#[test]
fn refresh_all_over_an_unreadable_subscriptions_file_returns_the_outer_error()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    std::fs::create_dir_all(rig.subs.path())?;
    let service = HttpService::spawn(Limits::brisk())?;
    match service
        .handle()
        .block_on(library::refresh_all(&service, &rig.subs, &rig.cache))
    {
        Err(FeedError::SubscriptionsUnreadable { .. }) => {}
        other => return Err(format!("expected an outer Err, got {other:?}").into()),
    }
    Ok(())
}

/// Step 5's other named case: an empty but valid snapshot (no
/// subscriptions at all) is not an enumeration failure — `refresh_all`
/// returns an empty, successful batch.
#[test]
fn refresh_all_over_an_empty_snapshot_returns_an_empty_successful_batch()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let service = HttpService::spawn(Limits::brisk())?;
    let outcomes = service
        .handle()
        .block_on(library::refresh_all(&service, &rig.subs, &rig.cache))?;
    assert!(outcomes.is_empty());
    Ok(())
}
