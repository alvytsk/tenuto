//! `SubscriptionStore`: durable subscription snapshots, with reads and
//! recovery kept separate (design doc §5.1, §5.5, §5.6).

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tenuto::clock::FakeClock;
use tenuto::persistence::store::LoadReason;
use tenuto::subscription::model::Subscription;
use tenuto::subscription::store::{SubscriptionSnapshot, SubscriptionStore};
use time::OffsetDateTime;

/// The stamp a `FakeClock` produces, which starts at the epoch.
const EPOCH_STAMP: &str = "19700101T000000Z";

fn store(path: PathBuf) -> SubscriptionStore {
    SubscriptionStore::new(path, Arc::new(FakeClock::new()))
}

/// One valid, explicit record (design doc §5.1's JSON example) as a mutable
/// `Value`, so each rejection test can flip exactly one field.
fn valid_file() -> Value {
    json!({
        "schema_version": 1,
        "subscriptions": [
            {
                "feed_id": "9f3c1a7e42b58d0c6f19ab3e5d72c840",
                "slug": "radio-t",
                "title": "Радио-Т",
                "fetch_url": "https://radio-t.com/rss/",
                "added_at": "2026-09-11T09:14:22Z"
            }
        ]
    })
}

/// Writes `file`, asserts `read_snapshot` returns `Err` without touching the
/// file, then asserts `load` quarantines it: moved aside, original name
/// gone, writing left on.
fn assert_malformed(path: &PathBuf, file: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = serde_json::to_vec(file)?;
    fs::write(path, &bytes)?;

    let read_store = store(path.clone());
    assert!(
        read_store.read_snapshot().is_err(),
        "expected a malformed file to be rejected: {file}"
    );
    assert_eq!(
        fs::read(path)?,
        bytes,
        "read_snapshot must never rewrite or move the file"
    );

    let load_store = store(path.clone());
    let result = load_store.load();
    let LoadReason::Quarantined { moved_to } = result.reason else {
        return Err(format!("expected a quarantine for {file}, got {:?}", result.reason).into());
    };
    assert!(!path.exists());
    assert_eq!(fs::read(&moved_to)?, bytes);
    assert!(result.writable);
    Ok(())
}

/// Wiring: the store hands the shared lifecycle its own file name and its
/// one version. The lifecycle itself is tested in `persistence::json_file`.
#[test]
fn only_mutating_load_quarantines() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let store = store(path.clone());

    for version in [0, 2] {
        let mut file = valid_file();
        file["schema_version"] = json!(version);
        fs::write(&path, serde_json::to_vec(&file)?)?;
        let result = store.load();
        assert!(
            matches!(result.reason, LoadReason::UnsupportedVersion { found } if found == version),
            "version {version}: {:?}",
            result.reason
        );
    }

    fs::write(&path, b"broken")?;
    assert!(store.read_snapshot().is_err());
    assert_eq!(fs::read(&path)?, b"broken");
    let result = store.load();
    let LoadReason::Quarantined { moved_to } = result.reason else {
        panic!("expected quarantine")
    };
    assert_eq!(
        moved_to.file_name().and_then(|name| name.to_str()),
        Some(format!("subscriptions.json.rejected-{EPOCH_STAMP}").as_str())
    );
    assert_eq!(fs::read(moved_to)?, b"broken");
    Ok(())
}

#[test]
fn a_valid_subscription_survives_save_and_load() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let store = store(path.clone());

    let subscription = Subscription {
        feed_id: tenuto::subscription::model::validate_feed_id("9f3c1a7e42b58d0c6f19ab3e5d72c840")?,
        slug: "radio-t".to_string(),
        title: Some("Радио-Т".to_string()),
        fetch_url: url::Url::parse("https://radio-t.com/rss/")?,
        added_at: OffsetDateTime::parse(
            "2026-09-11T09:14:22Z",
            &time::format_description::well_known::Rfc3339,
        )?,
    };
    store.save(&SubscriptionSnapshot {
        subscriptions: vec![subscription.clone()],
    })?;

    let snapshot = store.read_snapshot()?;
    assert_eq!(snapshot.subscriptions, vec![subscription.clone()]);

    let result = store.load();
    assert!(matches!(result.reason, LoadReason::Loaded));
    assert!(result.writable);
    assert_eq!(result.snapshot.subscriptions, vec![subscription]);
    Ok(())
}

// --- §5.6 rejection matrix -------------------------------------------------

#[test]
fn traversal_shaped_feed_id_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["feed_id"] = json!("../../etc/passwd");
    assert_malformed(&path, &file)
}

#[test]
fn uppercase_feed_id_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["feed_id"] = json!("9F3C1A7E42B58D0C6F19AB3E5D72C840");
    assert_malformed(&path, &file)
}

#[test]
fn wrong_length_feed_id_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["feed_id"] = json!("9f3c1a7e42b58d0c6f19ab3e5d72c8");
    assert_malformed(&path, &file)
}

#[test]
fn duplicate_feed_id_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    let mut second = file["subscriptions"][0].clone();
    second["slug"] = json!("other-slug");
    file["subscriptions"]
        .as_array_mut()
        .ok_or("subscriptions must be an array")?
        .push(second);
    assert_malformed(&path, &file)
}

#[test]
fn duplicate_slug_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    let mut second = file["subscriptions"][0].clone();
    second["feed_id"] = json!("00000000000000000000000000000000"[..32]);
    file["subscriptions"]
        .as_array_mut()
        .ok_or("subscriptions must be an array")?
        .push(second);
    assert_malformed(&path, &file)
}

#[test]
fn invalid_slug_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["slug"] = json!("Not A Slug!");
    assert_malformed(&path, &file)
}

#[test]
fn non_http_fetch_url_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["fetch_url"] = json!("ftp://radio-t.com/rss/");
    assert_malformed(&path, &file)
}

#[test]
fn bad_rfc3339_timestamp_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["added_at"] = json!("not a timestamp");
    assert_malformed(&path, &file)
}

// --- No cap, no eviction ----------------------------------------------------

/// design doc §5.1: unlike `state.json`, subscriptions have no entry cap and
/// no eviction — silently dropping one to respect a limit would be data
/// loss. Saving many more subscriptions than any checkpoint cap must not
/// drop a single one.
#[test]
fn saving_513_distinct_subscriptions_all_survive() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let store = store(path.clone());

    const COUNT: usize = 513;
    let mut subscriptions = Vec::with_capacity(COUNT);
    for index in 0..COUNT {
        let feed_id = tenuto::subscription::model::validate_feed_id(&format!("{index:032x}"))?;
        subscriptions.push(Subscription {
            feed_id,
            slug: format!("feed-{index}"),
            title: None,
            fetch_url: url::Url::parse(&format!("https://example.org/feed/{index}"))?,
            added_at: OffsetDateTime::UNIX_EPOCH,
        });
    }

    store.save(&SubscriptionSnapshot {
        subscriptions: subscriptions.clone(),
    })?;

    let result = store.load();
    assert!(matches!(result.reason, LoadReason::Loaded));
    assert_eq!(result.snapshot.subscriptions.len(), COUNT);

    let ids: BTreeSet<_> = result
        .snapshot
        .subscriptions
        .iter()
        .map(|subscription| subscription.feed_id.as_str().to_string())
        .collect();
    assert_eq!(ids.len(), COUNT, "no subscription was dropped or merged");

    let slugs: BTreeSet<_> = result
        .snapshot
        .subscriptions
        .iter()
        .map(|subscription| subscription.slug.clone())
        .collect();
    assert_eq!(slugs.len(), COUNT);

    let snapshot = store.read_snapshot()?;
    assert_eq!(snapshot.subscriptions.len(), COUNT);
    Ok(())
}

// --- Quarantine collision handling ------------------------------------------

// --- Unreadable path ---------------------------------------------------------

/// Review Focus 3: a listing shows exactly today's reason for each way the
/// file can be unusable.
#[test]
fn read_errors_keep_their_text() -> Result<(), Box<dyn std::error::Error>> {
    use tenuto::feed::error::FeedError;

    let reason = |path: &PathBuf| match store(path.clone()).read_snapshot() {
        Err(FeedError::SubscriptionsUnreadable { reason }) => reason,
        other => panic!("expected SubscriptionsUnreadable, got {other:?}"),
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");

    fs::create_dir(&path)?;
    assert!(
        reason(&path).starts_with("cannot read subscriptions file: "),
        "{}",
        reason(&path)
    );
    fs::remove_dir(&path)?;

    let mut newer = valid_file();
    newer["schema_version"] = json!(2);
    fs::write(&path, serde_json::to_vec(&newer)?)?;
    assert_eq!(
        reason(&path),
        "subscriptions file is schema version 2, and this build supports 1"
    );

    fs::write(&path, b"broken")?;
    assert_eq!(
        reason(&path),
        "subscriptions file is malformed (syntax error at line 1, column 1)"
    );

    let mut invalid = valid_file();
    invalid["subscriptions"][0]["slug"] = json!("Not A Slug");
    fs::write(&path, serde_json::to_vec(&invalid)?)?;
    assert_eq!(reason(&path), "a slug is not valid");
    Ok(())
}

/// Spec §2: a public snapshot can carry a timestamp RFC 3339 cannot write.
/// The save fails as a persistence error and the file on disk is untouched.
#[test]
fn a_save_that_cannot_serialize_leaves_the_file_as_it_was() -> Result<(), Box<dyn std::error::Error>>
{
    use tenuto::feed::error::FeedError;
    use tenuto::persistence::PersistenceError;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    fs::write(&path, serde_json::to_vec(&valid_file())?)?;
    let before = fs::read(&path)?;
    let store = store(path.clone());
    let mut snapshot = store.read_snapshot()?;
    snapshot.subscriptions[0].added_at =
        time::Date::from_calendar_date(-1, time::Month::January, 1)?
            .midnight()
            .assume_utc();

    let result = store.save(&snapshot);

    assert!(
        matches!(
            result,
            Err(FeedError::Persistence(PersistenceError::Serialize { .. }))
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}
