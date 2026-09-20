//! Design doc M5 §8: enqueueing, restoring or browsing a URL/podcast entry
//! makes no network request — only an explicit play prepares a remote
//! source. A loopback server counts every request it sees.

mod support;

#[path = "support/feeds.rs"]
mod feeds;

#[path = "support/runtime.rs"]
mod runtime;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::{Terminal, backend::TestBackend};
use runtime::{pump_for, rig_with, rig_with_probe, row_ids};
use serde_json::json;
use support::browse::wait_for_result;
use support::server::{Script, TestServer};
use tenuto::application::browse::{BrowseRequest, BrowseResult, BrowseWorker};
use tenuto::application::enrich::TagProbe;
use tenuto::application::runtime::{AppCommand, EnqueueItem, LibraryStores};
use tenuto::clock::{Clock, SystemClock};
use tenuto::feed::cache::CacheStore;
use tenuto::library::EpisodeCandidate;
use tenuto::media::id::{EpisodeKey, FeedId, MediaId, NormalizedUrl};
use tenuto::media::tags::probe_local_tags;
use tenuto::persistence::model::PersistedState;
use tenuto::queue::{NewQueueEntry, QueueSource};
use tenuto::session::Session;
use tenuto::station::model::Station;
use tenuto::station::store::{StationSnapshot, StationStore};
use tenuto::subscription::store::SubscriptionStore;
use tenuto::tui::render::{Visuals, draw};
use tenuto::tui::state::UiState;

const FEED_URL: &str = "https://feeds.example/radio-t.xml";
const LOCAL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");

fn remote_entry(url: &str) -> NewQueueEntry {
    let normalized = NormalizedUrl::parse(url).unwrap_or_else(|error| panic!("url: {error}"));
    NewQueueEntry::new(
        MediaId::RemoteUrl(normalized.clone()),
        QueueSource::RemoteUrl(normalized),
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("remote entry: {error}"))
}

fn podcast_entry(fallback: &str) -> NewQueueEntry {
    NewQueueEntry::new(
        MediaId::PodcastEpisode {
            feed: FeedId::new(feeds::FEED_ID.into())
                .unwrap_or_else(|error| panic!("feed: {error}")),
            episode: EpisodeKey::resolve(Some("e1"), None, None)
                .unwrap_or_else(|error| panic!("episode key: {error}")),
        },
        QueueSource::Podcast {
            fallback: fallback
                .parse()
                .unwrap_or_else(|error| panic!("url: {error}")),
        },
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("podcast entry: {error}"))
}

fn seeded(entries: Vec<NewQueueEntry>) -> PersistedState {
    let mut session = Session::new(PersistedState::default());
    session
        .enqueue(session.state().playing(), entries)
        .unwrap_or_else(|error| panic!("fits: {error}"));
    session.state().clone()
}

fn podcast_rss(enclosure: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Radio-T</title><item><title>e1</title><guid>e1</guid><enclosure url="{enclosure}" type="audio/mpeg"/></item></channel></rss>"#
    )
    .into_bytes()
}

#[test]
fn restoring_enqueueing_and_browsing_remote_entries_make_no_requests() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));

    // Enrichment is on, with a probe that records every path it is handed:
    // only the one local file below may ever reach it.
    let probed = Arc::new(Mutex::new(Vec::new()));
    let probe: TagProbe = Arc::new({
        let probed = Arc::clone(&probed);
        move |path| {
            probed
                .lock()
                .unwrap_or_else(|error| panic!("probe log: {error}"))
                .push(path.clone());
            probe_local_tags(path)
        }
    });
    let mut rig = rig_with_probe(
        seeded(vec![
            remote_entry(&server.url("/a.mp3")),
            podcast_entry(&server.url("/ep.mp3")),
        ]),
        probe,
    );
    let episode = EpisodeCandidate {
        media: MediaId::PodcastEpisode {
            feed: FeedId::new(feeds::FEED_ID.into())
                .unwrap_or_else(|error| panic!("feed: {error}")),
            episode: EpisodeKey::resolve(Some("e2"), None, None)
                .unwrap_or_else(|error| panic!("episode key: {error}")),
        },
        enclosure: Some(
            server
                .url("/ep2.mp3")
                .parse()
                .unwrap_or_else(|error| panic!("url: {error}")),
        ),
        title: None,
        declared_duration: None,
        published: None,
    };
    let local = std::fs::canonicalize(LOCAL).unwrap_or_else(|error| panic!("fixture: {error}"));
    rig.runtime.handle(AppCommand::Enqueue(vec![
        EnqueueItem::Url(server.url("/b.mp3")),
        EnqueueItem::Episode(episode),
        // The positive control: enrichment is running in this rig.
        EnqueueItem::Path(local.clone()),
    ]));
    let view = rig.runtime.view();
    assert_eq!(view.rows.len(), 5, "{view:?}");
    pump_for(&mut rig.runtime, Duration::from_millis(300));
    let probed: Vec<_> = probed
        .lock()
        .unwrap_or_else(|error| panic!("probe log: {error}"))
        .iter()
        .map(|path| path.as_path().to_path_buf())
        .collect();
    assert_eq!(probed, vec![local], "enrichment probes local files only");

    let library = feeds::Rig::new().unwrap_or_else(|error| panic!("feeds rig: {error}"));
    library
        .seed(&podcast_rss(&server.url("/ep.mp3")), FEED_URL)
        .unwrap_or_else(|error| panic!("seed: {error}"));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let worker = BrowseWorker::spawn(Some(LibraryStores {
        subscriptions: SubscriptionStore::new(
            library.root.path().join("data/tenuto/subscriptions.json"),
            Arc::clone(&clock),
        ),
        cache: CacheStore::new(library.root.path().join("cache/tenuto/feeds")),
        stations: StationStore::new(library.root.path().join("data/tenuto/stations.json"), clock),
    }));
    worker.request(BrowseRequest::Feeds);
    assert!(
        matches!(wait_for_result(&worker), BrowseResult::Feeds(Ok(feeds)) if feeds.len() == 1),
        "the seeded feed is listed"
    );
    worker.request(BrowseRequest::Episodes {
        slug: "radio-t".to_owned(),
    });
    let expected = server.url("/ep.mp3");
    match wait_for_result(&worker) {
        BrowseResult::Episodes {
            slug,
            episodes: Ok(episodes),
        } => {
            assert_eq!(slug, "radio-t");
            assert!(
                matches!(
                    &episodes[..],
                    [only] if only.enclosure.as_ref().map(url::Url::as_str) == Some(expected.as_str())
                ),
                "{episodes:?}"
            );
        }
        other => panic!("expected an episode listing, got {other:?}"),
    }

    assert!(
        server.requests().is_empty(),
        "no request before an explicit play: {:?}",
        server.requests()
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

#[test]
fn only_an_explicit_play_prepares_the_remote_source() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut rig = rig_with(seeded(vec![remote_entry(&server.url("/a.mp3"))]));
    pump_for(&mut rig.runtime, Duration::from_millis(100));
    assert!(server.requests().is_empty());

    let remote = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(remote));
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.requests().is_empty() {
        assert!(
            Instant::now() < deadline,
            "an explicit play never reached the server: {:?}",
            rig.runtime.view()
        );
        rig.runtime.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

/// The state a process restart with a station *active* restores from (§9/§14
/// L20). Built from the persisted JSON the way `m5_state_recovery` builds its
/// fixtures: `active_entry` is written on disk, and nothing in the public API
/// sets it without a real `Loaded` event from the engine.
fn station_active(url: &str) -> PersistedState {
    let normalized = NormalizedUrl::parse(url).unwrap_or_else(|error| panic!("url: {error}"));
    // Spelled by `MediaId` itself rather than by hand, so the fixture cannot
    // drift from the encoding the store actually writes.
    let media = MediaId::RemoteUrl(normalized.clone()).to_string();
    serde_json::from_value(json!({
        "schema_version": 3,
        "current_media": media,
        "volume": 1.0,
        "checkpoints": {},
        "queue": [{
            "id": 1,
            "media": media,
            "source": { "kind": "remote", "url": normalized.as_str() },
        }],
        "active_entry": 1,
    }))
    .unwrap_or_else(|error| panic!("persisted state: {error}"))
}

/// M7 §10 / L20: a station is a remote entry like any other — restoring one
/// *as the active entry*, pumping and drawing it all happen with the radio
/// off. The active shape is the one that matters: it is where a runtime could
/// plausibly decide to prepare the source before anyone pressed Play.
#[test]
fn a_restored_active_station_is_listed_and_drawn_without_a_request() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let mut rig = rig_with(station_active(&server.url("/radio")));
    pump_for(&mut rig.runtime, Duration::from_millis(300));

    let view = rig.runtime.view();
    assert_eq!(view.rows.len(), 1, "{view:?}");
    // The guard against this test quietly degrading back to a queued-but-not-
    // active station, which is the weaker claim.
    assert_eq!(view.active, Some(view.rows[0].id), "{view:?}");
    let now = view
        .now_playing
        .as_ref()
        .unwrap_or_else(|| panic!("the active entry is the current one: {view:?}"));
    assert!(!now.loaded, "nothing was loaded by the restore: {view:?}");
    assert!(
        !view.live && !view.reconnecting,
        "nothing is loaded: {view:?}"
    );
    let mut terminal =
        Terminal::new(TestBackend::new(100, 30)).unwrap_or_else(|error| panic!("backend: {error}"));
    terminal
        .draw(|frame| {
            draw(frame, &view, &UiState::new(true), &Visuals::default());
        })
        .unwrap_or_else(|error| panic!("draw: {error}"));

    assert!(
        server.requests().is_empty(),
        "a station was contacted before an explicit play: {:?}",
        server.requests()
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

/// The Radio tab's counterpart to the test above (M7.1 design doc §6, R3): a
/// station already saved in the library is both restored as the active
/// queue entry and drawn, and listed by the browse worker's `Stations`
/// request, all without a request reaching the server.
#[test]
fn a_restored_station_is_listed_and_drawn_without_a_request() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let url = server.url("/radio");
    let mut rig = rig_with(station_active(&url));
    pump_for(&mut rig.runtime, Duration::from_millis(300));

    let view = rig.runtime.view();
    assert_eq!(view.rows.len(), 1, "{view:?}");
    assert_eq!(view.active, Some(view.rows[0].id), "{view:?}");
    let mut terminal =
        Terminal::new(TestBackend::new(100, 30)).unwrap_or_else(|error| panic!("backend: {error}"));
    terminal
        .draw(|frame| {
            draw(frame, &view, &UiState::new(true), &Visuals::default());
        })
        .unwrap_or_else(|error| panic!("draw: {error}"));

    // The same station, saved in the library, is listed by a fresh worker
    // that has never made a request either: R3 extends the no-network
    // invariant from restoring/drawing the queue entry above to reading the
    // Radio tab.
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let stations = StationStore::new(root.path().join("stations.json"), Arc::clone(&clock));
    let normalized = NormalizedUrl::parse(&url).unwrap_or_else(|error| panic!("url: {error}"));
    let added_at = stations.now();
    stations
        .save(&StationSnapshot {
            stations: vec![Station {
                slug: "radio".to_owned(),
                url: url.parse().unwrap_or_else(|error| panic!("url: {error}")),
                media: MediaId::RemoteUrl(normalized),
                identity: None,
                added_at,
                probed_at: None,
            }],
        })
        .unwrap_or_else(|error| panic!("save: {error}"));

    let worker = BrowseWorker::spawn(Some(LibraryStores {
        subscriptions: SubscriptionStore::new(
            root.path().join("subscriptions.json"),
            Arc::clone(&clock),
        ),
        cache: CacheStore::new(root.path().join("feeds")),
        stations,
    }));
    worker.request(BrowseRequest::Stations);
    match wait_for_result(&worker) {
        BrowseResult::Stations(Ok(rows)) => {
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].slug, "radio", "{rows:?}");
        }
        other => panic!("expected a station listing, got {other:?}"),
    }

    assert!(
        server.requests().is_empty(),
        "a station was contacted before an explicit play or a Radio-tab read: {:?}",
        server.requests()
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}
