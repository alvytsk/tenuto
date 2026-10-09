//! Design doc M6 §3 and §8: the browse worker's three mutation requests,
//! answered with the CLI's own wording, against a loopback feed server.
//! Browsing alone still makes no request.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use support::browse::answer;
use support::server::{DocumentReply, Script, TestServer};
use tenuto::application::browse::{BrowseRequest, BrowseResult, BrowseWorker};
use tenuto::application::feed_ops::FeedOp;
use tenuto::application::runtime::LibraryStores;
use tenuto::clock::SystemClock;
use tenuto::feed::cache::CacheStore;
use tenuto::library::list_feeds;
use tenuto::station::store::StationStore;
use tenuto::subscription::store::SubscriptionStore;

fn rss(title: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>{title}</title><item><title>e1</title><guid>e1</guid><enclosure url="https://cdn.example.org/1.mp3" type="audio/mpeg"/></item><item><title>e2</title><guid>e2</guid><enclosure url="https://cdn.example.org/2.mp3" type="audio/mpeg"/></item></channel></rss>"#
    )
    .into_bytes()
}

fn reply(path: &str, status: u16, body: Vec<u8>) -> DocumentReply {
    DocumentReply {
        path: path.to_string(),
        status,
        headers: Vec::new(),
        body,
        conditional: false,
        header_delay: Duration::ZERO,
    }
}

/// Stores rooted at `root`; built twice so the test can read what the
/// worker wrote.
fn stores(root: &Path) -> LibraryStores {
    LibraryStores {
        subscriptions: SubscriptionStore::new(
            root.join("data/tenuto/subscriptions.json"),
            Arc::new(SystemClock),
        ),
        cache: CacheStore::new(root.join("cache/tenuto/feeds")),
        stations: StationStore::new(
            root.join("data/tenuto/stations.json"),
            Arc::new(SystemClock),
        ),
    }
}

fn slugs(root: &Path) -> Vec<String> {
    let stores = stores(root);
    list_feeds(&stores.subscriptions, &stores.cache)
        .unwrap_or_else(|error| panic!("list: {error}"))
        .into_iter()
        .map(|feed| feed.slug)
        .collect()
}

#[test]
fn subscribe_refresh_and_unsubscribe_answer_with_the_cli_wording() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::documents(vec![
        reply("/a", 200, rss("Radio T")),
        reply("/bad", 500, Vec::new()),
    ]));
    let worker = BrowseWorker::spawn(Some(stores(root.path())));

    let request = BrowseRequest::Op(FeedOp::Subscribe {
        url: server.url("/a"),
        slug: None,
    });
    let (echoed, outcome) = answer(&worker, request.clone());
    assert_eq!(echoed, request);
    assert_eq!(
        outcome.as_deref(),
        Ok("radio-t: subscribed, 2 episodes retained, 0 skipped"),
        "{outcome:?}"
    );
    assert_eq!(slugs(root.path()), ["radio-t"]);

    let (_, again) = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Subscribe {
            url: server.url("/a"),
            slug: None,
        }),
    );
    assert_eq!(
        again,
        Err("already subscribed as radio-t".to_string()),
        "{again:?}"
    );

    let (_, invalid) = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Subscribe {
            url: "not a url".into(),
            slug: None,
        }),
    );
    assert!(invalid.is_err(), "{invalid:?}");
    let (_, failing) = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Subscribe {
            url: server.url("/bad"),
            slug: None,
        }),
    );
    assert!(failing.is_err(), "{failing:?}");
    assert_eq!(
        slugs(root.path()),
        ["radio-t"],
        "a failed subscribe changes nothing"
    );

    let (_, refreshed) = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Refresh {
            slug: Some("radio-t".into()),
        }),
    );
    assert_eq!(
        refreshed.as_deref(),
        Ok("radio-t: updated, 2 episodes retained, 0 skipped"),
        "{refreshed:?}"
    );

    let (_, removed) = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Unsubscribe {
            slug: "radio-t".into(),
        }),
    );
    assert_eq!(
        removed.as_deref(),
        Ok("radio-t: unsubscribed"),
        "{removed:?}"
    );
    assert!(slugs(root.path()).is_empty());
    let (_, missing) = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Unsubscribe {
            slug: "radio-t".into(),
        }),
    );
    assert_eq!(
        missing,
        Err("unknown feed: radio-t".to_string()),
        "{missing:?}"
    );
    server.shutdown();
}

/// §8: a refresh-all whose first feed succeeds and second fails carries the
/// first feed's success line, the second's failure and the batch line.
#[test]
fn refresh_all_reports_every_feed_then_the_batch_error() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let alive = TestServer::start(Script::documents(vec![reply("/a", 200, rss("Radio T"))]));
    let doomed = TestServer::start(Script::documents(vec![reply("/b", 200, rss("Other Show"))]));
    let worker = BrowseWorker::spawn(Some(stores(root.path())));
    let _ = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Subscribe {
            url: alive.url("/a"),
            slug: None,
        }),
    );
    let _ = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Subscribe {
            url: doomed.url("/b"),
            slug: None,
        }),
    );
    assert_eq!(slugs(root.path()), ["radio-t", "other-show"]);
    doomed.shutdown();

    let (_, outcome) = answer(&worker, BrowseRequest::Op(FeedOp::Refresh { slug: None }));
    let text = outcome.expect_err("one feed failed");
    assert!(
        text.contains("radio-t: updated, 2 episodes retained, 0 skipped"),
        "{text}"
    );
    assert!(text.contains("other-show: failed:"), "{text}");
    assert!(
        text.ends_with("1 of 2 feeds did not complete successfully"),
        "{text}"
    );
    alive.shutdown();
}

/// §8: removing a subscription is a local edit; the worker never opens a
/// connection for it.
#[test]
fn unsubscribe_touches_no_server() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::documents(vec![reply("/a", 200, rss("Radio T"))]));
    // Seed through one worker, then remove through a fresh one that has
    // never made a request.
    let seeding = BrowseWorker::spawn(Some(stores(root.path())));
    let _ = answer(
        &seeding,
        BrowseRequest::Op(FeedOp::Subscribe {
            url: server.url("/a"),
            slug: None,
        }),
    );
    let seen = server.requests().len();
    drop(seeding);

    let worker = BrowseWorker::spawn(Some(stores(root.path())));
    let (_, removed) = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Unsubscribe {
            slug: "radio-t".into(),
        }),
    );
    assert_eq!(
        removed.as_deref(),
        Ok("radio-t: unsubscribed"),
        "{removed:?}"
    );
    assert!(slugs(root.path()).is_empty());
    assert_eq!(server.requests().len(), seen, "unsubscribe made a request");
    server.shutdown();
}

/// Listing requests still touch nothing but the disk.
#[test]
fn browsing_makes_no_request() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::documents(vec![reply("/a", 200, rss("Radio T"))]));
    let worker = BrowseWorker::spawn(Some(stores(root.path())));
    let _ = answer(
        &worker,
        BrowseRequest::Op(FeedOp::Subscribe {
            url: server.url("/a"),
            slug: None,
        }),
    );
    let before = server.requests().len();

    worker.request(BrowseRequest::Feeds);
    worker.request(BrowseRequest::Episodes {
        slug: "radio-t".into(),
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = 0;
    while seen < 2 {
        assert!(Instant::now() < deadline, "listings never answered");
        match worker.try_result() {
            Some(BrowseResult::Feeds(Ok(feeds))) => {
                assert_eq!(feeds.len(), 1);
                seen += 1;
            }
            Some(BrowseResult::Episodes {
                episodes: Ok(episodes),
                ..
            }) => {
                assert_eq!(episodes.len(), 2);
                seen += 1;
            }
            Some(other) => panic!("unexpected {other:?}"),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    assert_eq!(server.requests().len(), before, "browsing made a request");
    server.shutdown();
}
