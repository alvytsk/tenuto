use serde_json::json;
use std::sync::Arc;
use tenuto::clock::FakeClock;
use tenuto::persistence::queue_codec::{QueueProblem, QueueReset};
use tenuto::persistence::store::{LoadReason, QueueBackup, StateStore};

mod support;
use support::media;

#[allow(clippy::expect_used)] // Fallible serialization of a fixed test fixture.
fn write_state(dir: &std::path::Path, value: serde_json::Value) -> std::path::PathBuf {
    let path = dir.join("state.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&value).expect("json")).expect("write");
    path
}

fn over_capacity() -> serde_json::Value {
    json!({ "schema_version": 3, "current_media": "local:/music/a.flac", "volume": 0.3,
        "checkpoints": { "local:/music/a.flac": { "position": { "secs": 5, "nanos": 0 }, "completed": true,
            "touch_seq": 3, "updated_at": "2026-09-14T10:00:00Z" } },
        "queue": (1..=5000).map(|i| json!({ "id": i, "media": format!("local:/music/t{i}.flac"),
            "source": { "kind": "local", "path": format!("/music/t{i}.flac") } })).collect::<Vec<_>>(),
        "active_entry": 1 })
}

#[test]
fn a_repaired_queue_is_backed_up_byte_for_byte_and_writing_stays_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let original = std::fs::read(&path).expect("read");
    let outcome = StateStore::new(path.clone(), Arc::new(FakeClock::new())).load();

    assert!(outcome.writable);
    assert!(matches!(outcome.reason, LoadReason::Loaded));
    let repair = outcome.queue_repair.expect("repair reported");
    assert_eq!(
        repair.reset,
        QueueReset::WholeQueue(QueueProblem::OverCapacity { found: 5000 })
    );
    let QueueBackup::Saved(backup) = repair.backup else {
        panic!("backup must succeed")
    };
    assert_eq!(std::fs::read(&backup).expect("backup"), original);
    assert_eq!(std::fs::read(&path).expect("original untouched"), original);
    assert!(outcome.state.queue().is_empty());
    assert_eq!(outcome.state.volume().percent(), 30);
    assert!(outcome.state.completed_for(&media("a")));
    assert_eq!(outcome.state.current_media(), Some(&media("a")));
}

#[test]
fn a_second_repair_never_overwrites_an_earlier_backup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    let first = store.load().queue_repair.expect("repair");
    let second = store.load().queue_repair.expect("repair");
    let (QueueBackup::Saved(a), QueueBackup::Saved(b)) = (first.backup, second.backup) else {
        panic!("both saved")
    };
    assert_ne!(a, b);
}

#[test]
fn a_failed_backup_disables_writing_and_leaves_the_original_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let original = std::fs::read(&path).expect("read");
    // FakeClock's wall clock is the Unix epoch; occupy every candidate name.
    for suffix in 1..=100 {
        let name = if suffix == 1 {
            "state.json.queue-recovery-19700101T000000Z".to_string()
        } else {
            format!("state.json.queue-recovery-19700101T000000Z-{suffix}")
        };
        std::fs::write(dir.path().join(name), b"occupied").expect("occupy");
    }
    let outcome = StateStore::new(path.clone(), Arc::new(FakeClock::new())).load();
    assert!(!outcome.writable);
    assert!(matches!(
        outcome.queue_repair.expect("repair").backup,
        QueueBackup::Failed
    ));
    assert_eq!(std::fs::read(&path).expect("original"), original);
    assert_eq!(outcome.state.volume().percent(), 30);
}

#[test]
fn read_only_snapshots_ignore_invalid_queue_data_without_side_effects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let snapshot = StateStore::new(path, Arc::new(FakeClock::new()))
        .read_snapshot()
        .expect("history readable");
    assert!(snapshot.completed_for(&media("a")));
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("dir").count(),
        1,
        "no backup, no quarantine"
    );
}

#[test]
fn invalid_base_state_and_newer_versions_keep_their_existing_handling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(
        dir.path(),
        json!({ "schema_version": 3, "checkpoints": 5, "queue": [] }),
    );
    let outcome = StateStore::new(path.clone(), Arc::new(FakeClock::new())).load();
    assert!(matches!(outcome.reason, LoadReason::Quarantined { .. }));
    assert!(outcome.queue_repair.is_none());

    let path = write_state(dir.path(), json!({ "schema_version": 5, "queue": 7 }));
    let outcome = StateStore::new(path, Arc::new(FakeClock::new())).load();
    assert!(matches!(
        outcome.reason,
        LoadReason::UnsupportedVersion { found: 5 }
    ));
    assert!(!outcome.writable);
}
