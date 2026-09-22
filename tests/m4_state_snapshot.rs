//! `StateStore::read_snapshot`: the pure decode step used by listings that
//! must never quarantine a malformed file or write one that was missing
//! (design doc §5.5).

mod support;

use std::sync::Arc;
use std::time::Duration;

use support::media;
use tenuto::clock::FakeClock;
use tenuto::persistence::store::StateStore;

#[test]
fn snapshot_read_does_not_quarantine() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.json");
    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    store.read_snapshot()?;
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
    std::fs::write(&path, b"broken")?;
    assert!(store.read_snapshot().is_err());
    assert_eq!(std::fs::read(&path)?, b"broken");
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

/// design doc §4.5's migration applies equally on the read-only path: a v1
/// file's established position must survive in memory, and — since a
/// snapshot never writes — the bytes on disk must stay exactly as they were.
#[test]
fn a_v1_file_is_normalised_in_memory_without_touching_disk()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.json");
    let v1 = br#"{
        "schema_version": 1,
        "current_media": "local:/music/a.flac",
        "volume": 0.75,
        "checkpoints": {
            "local:/music/a.flac": {
                "position": { "secs": 42, "nanos": 0 },
                "completed": false,
                "touch_seq": 7,
                "updated_at": "1970-01-01T00:00:00Z"
            }
        }
    }"#;
    std::fs::write(&path, v1)?;
    let before = std::fs::metadata(&path)?.modified()?;

    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    let snapshot = store.read_snapshot()?;

    let entry = snapshot
        .entry_for(&media("a"))
        .ok_or("the v1 entry must survive normalisation")?;
    assert_eq!(entry.position, Some(Duration::from_secs(42)));
    assert!(!entry.completed);

    assert_eq!(
        std::fs::read(&path)?,
        v1,
        "a read must never rewrite the file"
    );
    assert_eq!(
        std::fs::metadata(&path)?.modified()?,
        before,
        "a read must never touch the file's mtime"
    );
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

/// A v2 file with only an estimated position (no established one) must keep
/// both facts distinct: `position: null` stays `None`, and `estimated`
/// survives rather than being dropped or promoted to an established position.
#[test]
fn a_v2_estimated_only_entry_keeps_position_and_estimate_distinct()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.json");
    let v2 = br#"{
        "schema_version": 2,
        "current_media": null,
        "volume": 1.0,
        "checkpoints": {
            "local:/music/a.flac": {
                "position": null,
                "completed": false,
                "touch_seq": 1,
                "updated_at": "1970-01-01T00:00:00Z",
                "estimated": { "secs": 17, "nanos": 0 }
            }
        }
    }"#;
    std::fs::write(&path, v2)?;
    let before = std::fs::metadata(&path)?.modified()?;

    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    let snapshot = store.read_snapshot()?;

    let entry = snapshot
        .entry_for(&media("a"))
        .ok_or("the estimate-only entry must be present")?;
    assert_eq!(entry.position, None, "no established position exists yet");
    assert_eq!(entry.estimated, Some(Duration::from_secs(17)));
    assert!(!snapshot.completed_for(&media("a")));

    assert_eq!(
        std::fs::read(&path)?,
        v2,
        "a read must never rewrite the file"
    );
    assert_eq!(
        std::fs::metadata(&path)?.modified()?,
        before,
        "a read must never touch the file's mtime"
    );
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

/// A version this build does not know how to read is a visible error on the
/// read path too — never silently treated as "nothing played yet" — and the
/// file is left exactly as it was found.
#[test]
fn an_unsupported_version_is_a_visible_error_and_the_file_is_untouched()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.json");
    let newer = br#"{"schema_version":5,"checkpoints":{}}"#;
    std::fs::write(&path, newer)?;
    let before = std::fs::metadata(&path)?.modified()?;

    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    assert!(store.read_snapshot().is_err());

    assert_eq!(std::fs::read(&path)?, newer);
    assert_eq!(
        std::fs::metadata(&path)?.modified()?,
        before,
        "a rejected version must never touch the file's mtime"
    );
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

/// A directory at the state path is deterministically unreadable, including
/// when the test runs as root (root can still `open` a directory but cannot
/// read it as a file). It must surface as an `Err`, distinct from a missing
/// file, and read_snapshot must not attempt to create or replace anything.
#[test]
fn an_unreadable_path_is_a_visible_error() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.json");
    std::fs::create_dir(&path)?;
    let before = std::fs::metadata(&path)?.modified()?;

    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    assert!(store.read_snapshot().is_err());

    assert!(
        path.is_dir(),
        "the directory must be left exactly as it was"
    );
    assert_eq!(
        std::fs::metadata(&path)?.modified()?,
        before,
        "an unreadable path must never be touched, not even its mtime"
    );
    let entries: Vec<_> = std::fs::read_dir(dir.path())?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from("state.json")],
        "no file was created, renamed, or quarantined alongside it"
    );
    Ok(())
}
