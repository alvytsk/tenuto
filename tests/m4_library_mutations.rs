//! `src/library.rs`'s two mutating functions (design doc §5.3, §6.6, §6.7):
//! `subscribe` and `unsubscribe`. `Rig` (`tests/support/feeds.rs`) is the
//! fixture every M4 test suite imports: a temp root, a fake clock, and the
//! subscription/cache/state stores that share it.

#[path = "support/feeds.rs"]
mod feeds;
mod support;

use std::time::Duration;

use support::server::{DocumentReply, Script, TestServer};
use tenuto::{
    feed::error::FeedError,
    http::{limits::Limits, service::HttpService},
    library::{self, FollowupStep, SubscribeOutcome},
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

/// Step 1, verbatim from the brief: subscribe, confirm the listing sees it
/// and no checkpoints exist, then unsubscribe and confirm both the listing
/// and the state file are empty/absent again.
#[test]
fn subscription_roundtrip_does_not_touch_checkpoints() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::serving(RADIO_T.to_vec()));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/feed"),
        None,
    ))?;
    assert_eq!(result.slug, "radio-t");
    assert!(result.followup.is_none());
    assert_eq!(library::list_feeds(&rig.subs, &rig.cache)?.len(), 1);
    server.shutdown();
    let outcome = library::unsubscribe(&rig.subs, &rig.cache, "radio-t")?;
    assert!(outcome.followup.is_none());
    assert!(library::list_feeds(&rig.subs, &rig.cache)?.is_empty());
    assert!(!rig.state.path().exists());
    Ok(())
}

// --- Step 5: preflight and identity ----------------------------------

/// A fragment is not part of `AlreadySubscribed`'s comparison (§6.7):
/// requesting the same URL with a `#fragment` appended must match the
/// existing subscription and never reach the network.
#[test]
fn already_subscribed_ignores_a_fragment_and_performs_no_fetch()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        RADIO_T,
    )]));
    rig.seed(RADIO_T, &server.url("/feed"))?;
    let service = HttpService::spawn(Limits::brisk())?;

    let with_fragment = format!("{}#section", server.url("/feed"));
    match service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &with_fragment,
        None,
    )) {
        Err(FeedError::AlreadySubscribed { slug }) => assert_eq!(slug, "radio-t"),
        other => return Err(format!("expected AlreadySubscribed, got {other:?}").into()),
    }
    assert!(
        server.requests().is_empty(),
        "a duplicate request must never be fetched"
    );
    server.shutdown();
    Ok(())
}

/// An explicit default port is the other normalization `NormalizedUrl`
/// performs (§6.7): `:80` spelled out on an `http://` URL must still match
/// a stored `fetch_url` that omits it. Neither side is ever dialed here —
/// `AlreadySubscribed` short-circuits before any connection is attempted —
/// so an unroutable host is safe to use.
#[test]
fn already_subscribed_normalizes_an_explicit_default_port() -> Result<(), Box<dyn std::error::Error>>
{
    let rig = feeds::Rig::new()?;
    rig.seed(RADIO_T, "http://example.org/feed")?;
    let service = HttpService::spawn(Limits::brisk())?;

    match service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        "http://example.org:80/feed",
        None,
    )) {
        Err(FeedError::AlreadySubscribed { slug }) => assert_eq!(slug, "radio-t"),
        other => return Err(format!("expected AlreadySubscribed, got {other:?}").into()),
    }
    Ok(())
}

/// A differing query string is a genuinely different resource (§6.7:
/// "query serialization preserved"), so it is fetched and subscribed as a
/// second, independent feed. The derived slug collides on the shared title
/// and takes the first free `-2` suffix, and the first subscription's own
/// slug is left exactly as it was.
#[test]
fn a_query_difference_is_a_distinct_subscription_with_a_derived_suffix()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        RADIO_T,
    )]));
    let service = HttpService::spawn(Limits::brisk())?;

    let first = service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/feed"),
        None,
    ))?;
    assert_eq!(first.slug, "radio-t");

    let with_query = format!("{}?x=1", server.url("/feed"));
    let second = service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &with_query,
        None,
    ))?;
    assert_ne!(second.feed_id, first.feed_id);
    assert_eq!(second.slug, "radio-t-2");

    let slugs: Vec<String> = library::list_feeds(&rig.subs, &rig.cache)?
        .into_iter()
        .map(|summary| summary.slug)
        .collect();
    assert_eq!(slugs, vec!["radio-t", "radio-t-2"]);
    server.shutdown();
    Ok(())
}

/// Two distinct requested URLs that both redirect to the same aggregator
/// route are two independent subscriptions, not a detected duplicate
/// (§6.7): redirect targets never participate in `AlreadySubscribed`.
#[test]
fn redirects_to_a_shared_route_are_two_independent_subscriptions()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![
        reply("/a", 302, vec![("Location", "/common")], b""),
        reply("/b", 302, vec![("Location", "/common")], b""),
        reply("/common", 200, vec![], RADIO_T),
    ]));
    let service = HttpService::spawn(Limits::brisk())?;

    let first = service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/a"),
        None,
    ))?;
    let second = service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/b"),
        None,
    ))?;

    assert_ne!(first.feed_id, second.feed_id);
    assert_eq!(library::list_feeds(&rig.subs, &rig.cache)?.len(), 2);
    server.shutdown();
    Ok(())
}

/// An explicit `--as` collision is rejected before any fetch — never
/// silently suffixed, unlike a derived slug (§6.7's preflight rule).
#[test]
fn an_explicit_slug_collision_fails_before_fetching() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    rig.seed(RADIO_T, "https://example.org/feed")?;
    // A server that would fail this test if ever contacted.
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        RADIO_T,
    )]));
    let service = HttpService::spawn(Limits::brisk())?;

    match service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/feed"),
        Some("radio-t"),
    )) {
        Err(FeedError::SlugTaken { slug }) => assert_eq!(slug, "radio-t"),
        other => return Err(format!("expected SlugTaken, got {other:?}").into()),
    }
    assert!(
        server.requests().is_empty(),
        "an explicit collision must never be fetched"
    );
    server.shutdown();
    Ok(())
}

/// Resubscribing the same URL after unsubscribing mints a fresh `FeedId`
/// (§1.6) — never the old one — while an unrelated, pre-existing checkpoint
/// is left byte-for-byte untouched: `unsubscribe` takes no `StateStore` and
/// touches no such file, so the old id's checkpoints are simply orphaned.
#[test]
fn resubscribing_mints_a_fresh_id_and_leaves_old_checkpoints_untouched()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        RADIO_T,
    )]));
    let service = HttpService::spawn(Limits::brisk())?;

    let first = service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/feed"),
        None,
    ))?;

    let key = format!("podcast:{}/guid:seed", first.feed_id.as_str());
    let state: PersistedState = serde_json::from_value(serde_json::json!({
        "schema_version": 4,
        "checkpoints": {
            (key): {"position": {"secs": 12, "nanos": 0}, "completed": false,
                    "touch_seq": 1, "updated_at": "2026-09-11T00:00:00Z"}
        }
    }))?;
    rig.state.write(&state)?;
    let before = std::fs::read(rig.state.path())?;

    let outcome = library::unsubscribe(&rig.subs, &rig.cache, &first.slug)?;
    assert!(outcome.followup.is_none());

    let second = service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/feed"),
        None,
    ))?;
    assert_ne!(second.feed_id, first.feed_id);

    let after = std::fs::read(rig.state.path())?;
    assert_eq!(
        before, after,
        "an unrelated checkpoint must never move on subscribe or unsubscribe"
    );
    server.shutdown();
    Ok(())
}

/// A feed that fails to parse commits nothing: neither the cache nor
/// `subscriptions.json` is written (§5.3's commit order only ever starts
/// once parsing has already succeeded).
#[test]
fn a_feed_parse_failure_writes_neither_store() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        TRUNCATED_XML,
    )]));
    let service = HttpService::spawn(Limits::brisk())?;

    match service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/feed"),
        None,
    )) {
        Err(FeedError::Malformed { .. }) => {}
        other => return Err(format!("expected Malformed, got {other:?}").into()),
    }
    assert!(!rig.subs.path().exists(), "no subscription was ever saved");
    assert!(
        std::fs::read_dir(rig.root.path().join("cache/tenuto/feeds")).is_err(),
        "no cache entry was ever created"
    );
    server.shutdown();
    Ok(())
}

/// A permanent redirect to a malformed document still commits nothing —
/// the intermediate `permanent_url` a successful subscribe would have
/// adopted as `fetch_url` must not leak a subscription into existence on
/// its own.
#[test]
fn a_permanently_redirected_malformed_response_creates_no_subscription()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![
        reply("/old", 301, vec![("Location", "/new")], b""),
        reply("/new", 200, vec![], TRUNCATED_XML),
    ]));
    let service = HttpService::spawn(Limits::brisk())?;

    match service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/old"),
        None,
    )) {
        Err(FeedError::Malformed { .. }) => {}
        other => return Err(format!("expected Malformed, got {other:?}").into()),
    }
    assert!(!rig.subs.path().exists(), "no subscription was ever saved");
    server.shutdown();
    Ok(())
}

// --- Step 7 support: every failed subscription-load reason aborts -----

/// A malformed `subscriptions.json` is quarantined by `load`, and
/// `load_mutating` still refuses to build on top of the resulting empty
/// snapshot: `subscribe` aborts visibly, without ever reaching the network.
#[test]
fn a_malformed_subscriptions_file_aborts_subscribe_after_quarantining()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    std::fs::create_dir_all(rig.subs.path().parent().ok_or("no parent directory")?)?;
    std::fs::write(rig.subs.path(), b"not json")?;
    let server = TestServer::start(Script::documents(vec![reply(
        "/feed",
        200,
        vec![],
        RADIO_T,
    )]));
    let service = HttpService::spawn(Limits::brisk())?;

    match service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        &server.url("/feed"),
        None,
    )) {
        Err(FeedError::SubscriptionsUnreadable { .. }) => {}
        other => return Err(format!("expected SubscriptionsUnreadable, got {other:?}").into()),
    }
    assert!(
        server.requests().is_empty(),
        "a quarantined subscriptions file must never be fetched against"
    );
    let quarantined = std::fs::read_dir(rig.subs.path().parent().ok_or("no parent directory")?)?
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("subscriptions.json.rejected-")
        });
    assert!(quarantined, "the malformed file must have been moved aside");
    server.shutdown();
    Ok(())
}

/// An unsupported schema version is preserved in place, not quarantined —
/// `load_mutating` still aborts `unsubscribe`, and the file is left exactly
/// as it was.
#[test]
fn an_unsupported_schema_version_aborts_unsubscribe() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    std::fs::create_dir_all(rig.subs.path().parent().ok_or("no parent directory")?)?;
    let bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": 2,
        "subscriptions": []
    }))?;
    std::fs::write(rig.subs.path(), &bytes)?;

    match library::unsubscribe(&rig.subs, &rig.cache, "radio-t") {
        Err(FeedError::SubscriptionsUnreadable { .. }) => {}
        other => return Err(format!("expected SubscriptionsUnreadable, got {other:?}").into()),
    }
    assert_eq!(
        std::fs::read(rig.subs.path())?,
        bytes,
        "an unsupported schema version must be preserved untouched"
    );
    Ok(())
}

// --- Step 6: deterministic commit-boundary failure injection ----------

/// Subscribe's commit boundary (§5.3): a nonempty directory sitting at the
/// still-missing subscription file path makes the cache commit succeed and
/// the subscription rename fail. The failure is injected deterministically
/// — `wait_until_stalled` proves the request already left the client (every
/// preflight check ran) before the obstruction is created and the response
/// released, so there is no window in which the write could race ahead of
/// the setup.
#[test]
fn a_subscription_file_obstruction_leaves_the_cache_committed_and_unreferenced()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(
        Script::documents(vec![reply("/feed", 200, vec![], RADIO_T)]).stall_headers(),
    );
    let service = HttpService::spawn(Limits::brisk())?;
    let url = server.url("/feed");

    let outcome = std::thread::scope(
        |scope| -> Result<SubscribeOutcome, Box<dyn std::error::Error>> {
            let handle = scope.spawn(|| {
                service.handle().block_on(library::subscribe(
                    &service, &rig.subs, &rig.cache, &url, None,
                ))
            });

            assert!(
                server.wait_until_stalled(Duration::from_secs(5)),
                "subscribe's fetch never reached the server"
            );

            // The subscriptions file does not exist yet. Putting a nonempty
            // directory in its place — while the response is still
            // withheld — is what makes the eventual rename fail, without
            // any dependence on timing: the response cannot possibly land,
            // and therefore the commit cannot possibly start, until
            // `release()` below runs.
            std::fs::create_dir_all(rig.subs.path())?;
            std::fs::write(rig.subs.path().join("sentinel"), b"keep")?;

            assert!(
                server.release(),
                "the stalled connection was not parked where expected"
            );

            let joined = handle.join().map_err(|_| "subscribe thread panicked")?;
            Ok(joined?)
        },
    )?;

    assert!(
        matches!(
            outcome.followup.as_ref().map(|f| &f.step),
            Some(FollowupStep::SaveSubscription)
        ),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(rig.subs.path().join("sentinel"))?,
        b"keep",
        "the obstruction must be left exactly as the test made it"
    );
    let cache_path = rig.cache.path_for(&outcome.feed_id)?;
    assert!(
        cache_path.exists(),
        "the cache commit must have landed before the subscription save failed"
    );

    server.shutdown();
    Ok(())
}

/// Unsubscribe's commit boundary (§5.3), verbatim from the brief: the cache
/// file is replaced by a directory containing a sentinel, and `unsubscribe`
/// must still remove the subscription, report the cache-removal failure,
/// and leave the sentinel untouched.
#[test]
fn a_cache_file_obstruction_leaves_the_subscription_removed_and_the_sentinel_untouched()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RADIO_T, "https://example.org/feed")?;

    let path = rig.cache.path_for(&sub.feed_id)?;
    std::fs::remove_file(&path)?;
    std::fs::create_dir(&path)?;
    std::fs::write(path.join("sentinel"), b"keep")?;

    let result = tenuto::library::unsubscribe(&rig.subs, &rig.cache, &sub.slug)?;
    assert!(matches!(
        result.followup.as_ref().map(|f| &f.step),
        Some(tenuto::library::FollowupStep::RemoveCache)
    ));
    assert!(rig.subs.read_snapshot()?.subscriptions.is_empty());
    assert_eq!(std::fs::read(path.join("sentinel"))?, b"keep");
    Ok(())
}

/// A nonempty directory at the subscription file path fails `unsubscribe`
/// before cache deletion is even attempted (§5.3: subscription first, then
/// cache) — the cache file must still exist afterward.
#[test]
fn a_subscription_file_obstruction_fails_unsubscribe_before_touching_the_cache()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RADIO_T, "https://example.org/feed")?;
    let cache_path = rig.cache.path_for(&sub.feed_id)?;
    assert!(cache_path.exists());

    std::fs::remove_file(rig.subs.path())?;
    std::fs::create_dir(rig.subs.path())?;
    std::fs::write(rig.subs.path().join("sentinel"), b"keep")?;

    match library::unsubscribe(&rig.subs, &rig.cache, &sub.slug) {
        Err(FeedError::SubscriptionsUnreadable { .. }) => {}
        other => return Err(format!("expected SubscriptionsUnreadable, got {other:?}").into()),
    }
    assert!(
        cache_path.exists(),
        "the cache must never be touched once the subscription load already failed"
    );
    Ok(())
}

// --- Subscription lock -------------------------------------------------

/// Every mutation holds `subscriptions.lock` (beside `subscriptions.json`)
/// for its whole read-modify-write, so a second writer refuses instead of
/// saving a stale snapshot over the first one's commit.
#[test]
fn a_mutation_refuses_while_another_holds_the_subscription_lock()
-> Result<(), Box<dyn std::error::Error>> {
    use tenuto::lifecycle::lock::ProfileLock;
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RADIO_T, "https://example.org/feed")?;
    let held = ProfileLock::acquire_file(&rig.subs.path().with_file_name("subscriptions.lock"))?;

    match library::unsubscribe(&rig.subs, &rig.cache, &sub.slug) {
        Err(error @ FeedError::SubscriptionsBusy) => {
            assert_eq!(
                error.to_string(),
                "Another subscription update is in progress"
            );
        }
        other => return Err(format!("expected SubscriptionsBusy, got {other:?}").into()),
    }
    assert_eq!(library::list_feeds(&rig.subs, &rig.cache)?.len(), 1);

    drop(held);
    library::unsubscribe(&rig.subs, &rig.cache, &sub.slug)?;
    assert!(library::list_feeds(&rig.subs, &rig.cache)?.is_empty());
    Ok(())
}
