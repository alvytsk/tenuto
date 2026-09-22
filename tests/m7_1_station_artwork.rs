//! M7.1 §8.1: a station's logo is fetched when it plays, and at no other
//! time.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

#[path = "support/tagged_flac.rs"]
mod tagged_flac;

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use runtime::enqueue;
use support::server::{Script, TestServer};
use tenuto::application::runtime::{AppCommand, EnqueueItem, LibraryStores, PlayerRuntime};
use tenuto::artwork::worker::{CoverSource, default_loader};
use tenuto::clock::SystemClock;
use tenuto::feed::cache::CacheStore;
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::library::{AddStationOutcome, add_station, list_stations, reprobe_station};
use tenuto::lifecycle::hooks::TestHook;
use tenuto::persistence::model::PersistedState;
use tenuto::station::store::StationStore;
use tenuto::subscription::store::SubscriptionStore;

/// A fresh `StationStore` rooted at `root/stations.json`, built repeatedly
/// (never cached), matching `tests/m7_1_station_probe.rs::store`.
fn store(root: &Path) -> StationStore {
    StationStore::new(root.join("stations.json"), Arc::new(SystemClock))
}

/// A `LibraryStores` rooted at `root`, matching
/// `tests/m7_1_station_probe.rs::stores`.
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

fn service() -> Arc<HttpService> {
    HttpService::spawn(Limits::brisk()).unwrap_or_else(|error| panic!("service: {error}"))
}

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

fn png_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30])))
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap_or_else(|error| panic!("encode: {error}"));
    bytes
}

/// Polls `active_cover` until it is `Some`, the same pattern
/// `tests/m5_artwork.rs`'s remote-embedded-cover test uses.
fn wait_for_cover(runtime: &mut PlayerRuntime) -> CoverSource {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        runtime.pump();
        if let Some((_, source)) = runtime.active_cover() {
            return source;
        }
        assert!(
            Instant::now() < deadline,
            "no cover source: {:?}",
            runtime.view()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Polls `active_cover` until it is `CoverSource::Remote` whose URL path is
/// exactly `path`.
fn wait_for_remote_logo_path(runtime: &mut PlayerRuntime, path: &str) -> CoverSource {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        runtime.pump();
        if let Some((_, source)) = runtime.active_cover() {
            let matched = matches!(&source, CoverSource::Remote { url, .. } if url.path() == path);
            if matched {
                return source;
            }
        }
        assert!(
            Instant::now() < deadline,
            "logo path {path} never appeared: {:?}",
            runtime.view()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn logo_requests(server: &TestServer, path: &str) -> usize {
    server
        .requests()
        .into_iter()
        .filter(|request| request.path == path)
        .count()
}

#[test]
fn listing_enqueueing_and_restoring_a_station_request_no_logo() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let logo_server = TestServer::start(Script::serving(png_bytes()));
    let station_server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo(logo_server.url("/logo.svg")),
    );
    let http = service();
    let st = store(root.path());

    let outcome =
        add(&http, &st, &station_server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    assert!(
        matches!(outcome, AddStationOutcome::Verified { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        logo_requests(&logo_server, "/logo.svg"),
        0,
        "adding a station must not fetch its logo"
    );

    // 1. list the Radio tab
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert_eq!(rows.len(), 1);
    assert_eq!(
        logo_requests(&logo_server, "/logo.svg"),
        0,
        "listing must not fetch the logo"
    );

    // 2. enqueue the station
    let mut rig = runtime::rig_with_parts(
        PersistedState::default(),
        Some(stores(root.path())),
        runtime::null_engine(),
    );
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::from_input(&station_server.url("/radio"))],
    );
    rig.runtime.pump();
    assert!(
        rig.runtime.active_cover().is_none(),
        "an enqueued, unplayed station has no active cover"
    );
    assert_eq!(
        logo_requests(&logo_server, "/logo.svg"),
        0,
        "enqueueing must not fetch the logo"
    );

    // 3. drop the runtime and rebuild it (a process restart)
    let restored_state = rig.runtime.session().state().clone();
    let _ = rig.runtime.shutdown();
    let mut rig2 = runtime::rig_with_parts(
        restored_state,
        Some(stores(root.path())),
        runtime::null_engine(),
    );
    rig2.runtime.pump();
    assert!(
        rig2.runtime.active_cover().is_none(),
        "a restored, unplayed station has no active cover"
    );
    assert_eq!(
        logo_requests(&logo_server, "/logo.svg"),
        0,
        "restoring must not fetch the logo"
    );

    let _ = rig2.runtime.shutdown();
    station_server.shutdown();
    logo_server.shutdown();
}

#[test]
fn playing_a_station_requests_its_logo_exactly_once() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let logo_server = TestServer::start(Script::serving(png_bytes()));
    let station_server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo(logo_server.url("/logo.svg")),
    );
    let http = service();
    let st = store(root.path());
    add(&http, &st, &station_server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));

    let mut rig = runtime::rig_with_parts(
        PersistedState::default(),
        Some(stores(root.path())),
        runtime::null_engine(),
    );
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::from_input(&station_server.url("/radio"))],
    );
    let station = runtime::row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(station));

    let source = wait_for_remote_logo_path(&mut rig.runtime, "/logo.svg");

    // `active_cover` alone never touches the network; only pushing the
    // source through the loader (what `Artwork::poll` does in the TUI) does.
    let image = default_loader(TestHook::None)(&source);
    assert!(image.is_ok(), "the logo should decode: {:?}", image.err());
    assert_eq!(
        logo_requests(&logo_server, "/logo.svg"),
        1,
        "exactly one logo request"
    );

    let _ = rig.runtime.shutdown();
    station_server.shutdown();
    logo_server.shutdown();
}

/// The arm that must not regress: an ordinary remote URL no station claims
/// still falls back to the stream's own embedded front cover.
#[test]
fn a_remote_url_no_station_claims_still_uses_its_embedded_cover() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let track =
        tagged_flac::tagged_flac(dir.path(), "Title", "Artist", "Album", Some(&png_bytes()));
    let server = TestServer::start(Script::serving(
        std::fs::read(&track).unwrap_or_else(|error| panic!("read track: {error}")),
    ));

    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut rig = runtime::rig_with_parts(
        PersistedState::default(),
        Some(stores(root.path())),
        runtime::null_engine(),
    );
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::from_input(&server.url("/a.flac"))],
    );
    let remote = runtime::row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(remote));

    let source = wait_for_cover(&mut rig.runtime);
    assert!(
        matches!(source, CoverSource::Embedded(_)),
        "no station claims this URL, so its own embedded cover must win"
    );

    let _ = rig.runtime.shutdown();
    server.shutdown();
}

/// A saved station whose identity carries `logo: None` falls back the same
/// way, rather than the lookup panicking or wedging on a missing URL.
#[test]
fn a_station_with_no_logo_falls_back_to_the_embedded_cover() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let track =
        tagged_flac::tagged_flac(dir.path(), "Title", "Artist", "Album", Some(&png_bytes()));
    let track_bytes = std::fs::read(&track).unwrap_or_else(|error| panic!("read track: {error}"));

    // Connection 1 (the add probe) sees the icy fixture, so the add is
    // verified as a live station with no `icy-logo`; every later connection
    // (playback) sees the finite, embedded-cover fixture instead — the two
    // scripts share one URL, so both add and play resolve the same
    // `MediaId`.
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .then(Script::serving(track_bytes)),
    );
    let http = service();
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let st = store(root.path());
    let outcome = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified { identity, .. } = outcome else {
        panic!("expected Verified, got {outcome:?}");
    };
    assert_eq!(identity.logo, None, "the fixture sends no icy-logo");

    let mut rig = runtime::rig_with_parts(
        PersistedState::default(),
        Some(stores(root.path())),
        runtime::null_engine(),
    );
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::from_input(&server.url("/radio"))],
    );
    let station = runtime::row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(station));

    let source = wait_for_cover(&mut rig.runtime);
    assert!(
        matches!(source, CoverSource::Embedded(_)),
        "a station with no logo must fall back to the embedded cover"
    );

    let _ = rig.runtime.shutdown();
    server.shutdown();
}

/// Controller ruling: `Artwork::poll` is private to the TUI and cannot be
/// driven from an integration test, so this proves the mechanism at the
/// seam this test *can* reach — `active_cover` returning the re-probed
/// logo's URL on the entry's next load, a source unequal (widened dedup
/// key) to the one captured on the first play, and the loader actually
/// reaching the new path.
#[test]
fn a_re_probed_logo_replaces_the_old_one_on_the_next_load() {
    let logo_server = TestServer::start(Script::serving(png_bytes()));
    let logo_a = logo_server.url("/logo-a.svg");
    let logo_b = logo_server.url("/logo-b.svg");
    // Connections: 1st = add's probe, 2nd = first play, 3rd = re-probe,
    // 4th+ = second play (falls back to the last sequenced script).
    let station_server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo(logo_a.clone())
            .then(
                Script::from_fixture("sine-noxing.mp3")
                    .icy_station()
                    .icy_logo(logo_a.clone()),
            )
            .then(
                Script::from_fixture("sine-noxing.mp3")
                    .icy_station()
                    .icy_logo(logo_b.clone()),
            ),
    );
    let http = service();
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let st = store(root.path());
    let outcome =
        add(&http, &st, &station_server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified { slug, .. } = outcome else {
        panic!("expected Verified, got {outcome:?}");
    };

    let mut rig = runtime::rig_with_parts(
        PersistedState::default(),
        Some(stores(root.path())),
        runtime::null_engine(),
    );
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::from_input(&station_server.url("/radio"))],
    );
    let station = runtime::row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(station));

    let first_source = wait_for_remote_logo_path(&mut rig.runtime, "/logo-a.svg");

    reprobe(&http, &st, &slug).unwrap_or_else(|error| panic!("reprobe: {error}"));

    rig.runtime.handle(AppCommand::PlayEntry(station));
    let second_source = wait_for_remote_logo_path(&mut rig.runtime, "/logo-b.svg");

    assert!(
        second_source != first_source,
        "the widened dedup key must see the new URL as a different source"
    );

    let image = default_loader(TestHook::None)(&second_source);
    assert!(
        image.is_ok(),
        "the re-probed logo should decode: {:?}",
        image.err()
    );
    assert_eq!(
        logo_requests(&logo_server, "/logo-b.svg"),
        1,
        "exactly one request for the re-probed logo"
    );

    let _ = rig.runtime.shutdown();
    station_server.shutdown();
    logo_server.shutdown();
}
