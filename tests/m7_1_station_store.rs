//! M7.1 §4: `stations.json` is user-authored data — no cap, no eviction, and
//! a malformed file is quarantined rather than silently truncated.

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use tenuto::clock::FakeClock;
use tenuto::feed::error::FeedError;
use tenuto::http::source::StationIdentity;
use tenuto::persistence::store::LoadReason;
use tenuto::station::model::{Station, choose_station_slug};
use tenuto::station::store::{StationSnapshot, StationStore};

/// `SubscriptionStore`'s tests (`tests/m4_subscription_store.rs`) construct
/// their store the same way: there is no `support::test_clock()` helper, so
/// a fresh `FakeClock` (frozen at the Unix epoch) stands in for it here too.
fn clock() -> Arc<dyn tenuto::clock::Clock> {
    Arc::new(FakeClock::new())
}

fn station(slug: &str, url: &str, identity: Option<StationIdentity>) -> Station {
    let (media, _) =
        tenuto::application::source::resolve_source(url).unwrap_or_else(|error| panic!("{error}"));
    Station {
        slug: slug.to_owned(),
        url: url::Url::parse(url).unwrap_or_else(|error| panic!("{error}")),
        media,
        identity,
        added_at: time::OffsetDateTime::UNIX_EPOCH,
        probed_at: None,
    }
}

fn verified() -> StationIdentity {
    StationIdentity {
        name: Some("Lofi".to_owned()),
        genre: Some("Lofi".to_owned()),
        bitrate_kbps: Some(128),
        logo: Some(
            url::Url::parse("https://radio.example/logo.svg")
                .unwrap_or_else(|error| panic!("{error}")),
        ),
    }
}

#[test]
fn a_verified_and_an_unverified_station_round_trip() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(dir.path().join("stations.json"), clock());

    let snapshot = StationSnapshot {
        stations: vec![
            station(
                "lofi",
                "https://radio.example/lofi/stream",
                Some(verified()),
            ),
            station("jazz", "https://radio.example/jazz/stream", None),
        ],
    };
    store
        .save(&snapshot)
        .unwrap_or_else(|error| panic!("save: {error}"));

    let read = store
        .read_snapshot()
        .unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(read.stations.len(), 2);
    assert_eq!(read.stations[0].identity.as_ref(), Some(&verified()));
    assert!(
        read.stations[1].identity.is_none(),
        "an unverified station stays unverified across a round trip",
    );
}

/// The distinction a flat, field-level encoding of `StationIdentity` cannot
/// make: a station that was genuinely probed and classified live, but whose
/// response carried none of the four decorative ICY fields, must still
/// round-trip as verified (`Some`), never fall back to "not yet reached"
/// (`None`) just because every field inside it happens to be absent. This
/// exact shape is unreachable through the HTTP layer today — `is_icy`
/// requires `icy-name` or `icy-br` before classifying a response as live —
/// which is exactly why the store's own encoding, rather than that
/// invariant, must be what preserves it.
#[test]
fn a_verified_but_entirely_unnamed_station_round_trips_as_verified() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(dir.path().join("stations.json"), clock());

    let blank_but_verified = StationIdentity::default();
    let snapshot = StationSnapshot {
        stations: vec![station(
            "silent",
            "https://radio.example/silent/stream",
            Some(blank_but_verified.clone()),
        )],
    };
    store
        .save(&snapshot)
        .unwrap_or_else(|error| panic!("save: {error}"));

    let read = store
        .read_snapshot()
        .unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(
        read.stations[0].identity.as_ref(),
        Some(&blank_but_verified),
        "an all-absent identity must still round-trip as verified, not fall back to unverified",
    );
}

#[test]
fn a_missing_file_is_an_empty_list_not_an_error() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(dir.path().join("stations.json"), clock());
    let read = store
        .read_snapshot()
        .unwrap_or_else(|error| panic!("read: {error}"));
    assert!(read.stations.is_empty());
}

#[test]
fn read_snapshot_reports_a_malformed_file_and_never_quarantines_it() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let path = dir.path().join("stations.json");
    std::fs::write(&path, b"{ not json").unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(path.clone(), clock());

    assert!(
        matches!(
            store.read_snapshot(),
            Err(FeedError::StationsUnreadable { .. })
        ),
        "a read-only command reports the problem",
    );
    assert!(path.exists(), "read_snapshot never moves the file");
}

#[test]
fn load_quarantines_a_malformed_file_and_starts_empty() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let path = dir.path().join("stations.json");
    std::fs::write(&path, b"{ not json").unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(path.clone(), clock());

    let load = store.load();
    assert!(
        load.writable,
        "a quarantined file leaves the session writable"
    );
    assert!(load.snapshot.stations.is_empty());
    assert!(matches!(load.reason, LoadReason::Quarantined { .. }));
    assert!(!path.exists(), "the malformed file moved aside");

    let moved: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap_or_else(|error| panic!("{error}"))
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("stations.json.rejected-")
        })
        .collect();
    assert_eq!(moved.len(), 1, "exactly one quarantine file");
}

/// §6: `media` is derived from `url`, never an independent field. A
/// hand-edited `url` whose `media` was left stale (the record a user is
/// most likely to touch by hand) must be quarantined exactly like any
/// other malformed file, mirroring `load_quarantines_a_malformed_file_and_starts_empty`.
#[test]
fn a_media_that_disagrees_with_its_url_is_malformed() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let path = dir.path().join("stations.json");
    std::fs::write(
        &path,
        br#"{
            "schema_version": 1,
            "stations": [
                {
                    "slug": "lofi",
                    "url": "https://radio.example/lofi/stream",
                    "media": "remote:https://radio.example/jazz/stream",
                    "added_at": "1970-01-01T00:00:00Z"
                }
            ]
        }"#,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(path.clone(), clock());

    let load = store.load();
    assert!(
        load.writable,
        "a quarantined file leaves the session writable"
    );
    assert!(load.snapshot.stations.is_empty());
    assert!(matches!(load.reason, LoadReason::Quarantined { .. }));
    assert!(!path.exists(), "the malformed file moved aside");

    let moved: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap_or_else(|error| panic!("{error}"))
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("stations.json.rejected-")
        })
        .collect();
    assert_eq!(moved.len(), 1, "exactly one quarantine file");
}

#[test]
fn a_duplicate_slug_makes_the_whole_file_malformed() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(dir.path().join("stations.json"), clock());
    let snapshot = StationSnapshot {
        stations: vec![
            station("lofi", "https://radio.example/a", None),
            station("lofi", "https://radio.example/b", None),
        ],
    };
    assert!(
        matches!(
            store.save(&snapshot),
            Err(FeedError::StationsUnreadable { .. })
        ),
        "save re-runs the load-side validation, so a hand-built snapshot cannot write invalid data",
    );
}

fn media_of(url: &str) -> tenuto::media::id::MediaId {
    tenuto::application::source::resolve_source(url)
        .unwrap_or_else(|error| panic!("{error}"))
        .0
}

#[test]
fn logo_for_answers_only_for_a_url_a_station_claims() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(dir.path().join("stations.json"), clock());
    store
        .save(&StationSnapshot {
            stations: vec![station(
                "lofi",
                "https://radio.example/lofi/stream",
                Some(verified()),
            )],
        })
        .unwrap_or_else(|error| panic!("{error}"));

    let claimed = media_of("https://radio.example/lofi/stream");
    let unclaimed = media_of("https://elsewhere.example/track.mp3");
    assert_eq!(
        store.logo_for(&claimed).map(|url| url.to_string()),
        Some("https://radio.example/logo.svg".to_owned()),
    );
    assert_eq!(
        store.logo_for(&unclaimed),
        None,
        "a non-station URL claims nothing"
    );
}

#[test]
fn a_slug_is_derived_from_the_name_and_falls_back_to_the_host() {
    let url = url::Url::parse("https://www.radio.example/lofi/stream")
        .unwrap_or_else(|error| panic!("{error}"));
    let mut occupied = BTreeSet::new();

    assert_eq!(
        choose_station_slug(Some("Lofi"), &url, &occupied).unwrap_or_else(|e| panic!("{e}")),
        "lofi",
    );
    assert_eq!(
        choose_station_slug(None, &url, &occupied).unwrap_or_else(|e| panic!("{e}")),
        "radio-example",
        "no name falls back to the host with www. stripped",
    );

    occupied.insert("lofi".to_owned());
    assert_eq!(
        choose_station_slug(Some("Lofi"), &url, &occupied).unwrap_or_else(|e| panic!("{e}")),
        "lofi-2",
        "a collision takes the first free suffix",
    );
}

#[test]
fn is_station_answers_for_a_claimed_url_even_without_a_logo() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let store = StationStore::new(dir.path().join("stations.json"), clock());
    store
        .save(&StationSnapshot {
            stations: vec![
                station(
                    "lofi",
                    "https://radio.example/lofi/stream",
                    Some(verified()),
                ),
                station("bare", "https://radio.example/bare/stream", None),
            ],
        })
        .unwrap_or_else(|error| panic!("{error}"));

    assert!(
        store.is_station(&media_of("https://radio.example/lofi/stream")),
        "a station with a logo is a station"
    );
    assert!(
        store.is_station(&media_of("https://radio.example/bare/stream")),
        "a station is a station whether or not it stored a logo — this is \
         what `logo_for` cannot answer",
    );
    assert!(
        !store.is_station(&media_of("https://elsewhere.example/track.mp3")),
        "a URL no station claims is not a station"
    );
}
