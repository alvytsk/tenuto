//! The store: atomic replacement, permissions, and what happens to a file this
//! build cannot use.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use support::media;
use tenuto::clock::FakeClock;
use tenuto::persistence::model::{PersistedState, SCHEMA_VERSION};
use tenuto::persistence::store::{LoadReason, MAX_QUARANTINE_CANDIDATES, StateStore};
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use time::OffsetDateTime;

/// The stamp a `FakeClock` produces, which starts at the epoch.
const EPOCH_STAMP: &str = "19700101T000000Z";

fn store(dir: &Path) -> StateStore {
    StateStore::new(dir.join("state.json"), Arc::new(FakeClock::new()))
}

fn state_with(name: &str, secs: u64) -> PersistedState {
    let mut state = PersistedState::default();
    state.set_current_media(media(name));
    state.record(
        &PlaybackCheckpoint {
            media: media(name),
            position: Duration::from_secs(secs),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        },
        false,
    );
    state
}

fn temp_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        panic!("the tempdir must be readable");
    };
    entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("state.json.tmp-"))
        })
        .collect()
}

#[test]
fn a_missing_file_yields_empty_state_and_writing_stays_on() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = store(dir.path()).load();
    assert!(matches!(outcome.reason, LoadReason::Missing));
    assert!(outcome.writable);
    assert!(outcome.state.is_empty());
}

#[test]
fn a_write_is_readable_back_and_leaves_no_temp_behind() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.write(&state_with("a", 93)).unwrap();

    let outcome = store.load();
    assert!(matches!(outcome.reason, LoadReason::Loaded));
    assert!(outcome.writable);
    assert_eq!(
        outcome.state.entry_for(&media("a")).unwrap().position,
        Some(Duration::from_secs(93))
    );
    assert!(
        temp_files(dir.path()).is_empty(),
        "the temp file must not survive the write"
    );
}

#[test]
fn an_overwrite_replaces_rather_than_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());

    // A large state first, then a small one: an in-place write would leave the
    // tail of the large one behind and the result would not parse.
    let mut large = PersistedState::default();
    for index in 0..200 {
        large.record(
            &PlaybackCheckpoint {
                media: media(&format!("m{index}")),
                position: Duration::from_secs(index),
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            false,
        );
    }
    store.write(&large).unwrap();
    store.write(&state_with("a", 5)).unwrap();

    let outcome = store.load();
    assert!(matches!(outcome.reason, LoadReason::Loaded));
    assert_eq!(outcome.state.len(), 1);
}

#[test]
fn a_malformed_file_is_quarantined_and_writing_continues() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("state.json"), b"{ this is not json").unwrap();

    let outcome = store(dir.path()).load();
    let LoadReason::Quarantined { moved_to } = &outcome.reason else {
        panic!("expected a quarantine, got {:?}", outcome.reason);
    };
    assert!(
        outcome.writable,
        "garbage must not cost a session of persistence"
    );
    assert_eq!(
        fs::read(moved_to).unwrap(),
        b"{ this is not json",
        "the original bytes are preserved under the new name"
    );
    assert!(!dir.path().join("state.json").exists());
    assert_eq!(
        moved_to.file_name().unwrap().to_str().unwrap(),
        format!("state.json.rejected-{EPOCH_STAMP}")
    );
}

#[test]
fn a_quarantine_name_collision_is_suffixed_rather_than_clobbered() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("state.json"), b"garbage").unwrap();
    let taken = dir
        .path()
        .join(format!("state.json.rejected-{EPOCH_STAMP}"));
    fs::write(&taken, b"an earlier rejection").unwrap();

    let outcome = store(dir.path()).load();
    let LoadReason::Quarantined { moved_to } = &outcome.reason else {
        panic!("expected a quarantine, got {:?}", outcome.reason);
    };
    assert_eq!(
        moved_to.file_name().unwrap().to_str().unwrap(),
        format!("state.json.rejected-{EPOCH_STAMP}-2")
    );
    assert_eq!(fs::read(&taken).unwrap(), b"an earlier rejection");
}

#[test]
fn a_quarantine_that_cannot_be_performed_disables_writing_and_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("state.json"), b"garbage").unwrap();
    // Every candidate name taken: the store must refuse to clobber any of them.
    for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
        let name = if suffix == 1 {
            format!("state.json.rejected-{EPOCH_STAMP}")
        } else {
            format!("state.json.rejected-{EPOCH_STAMP}-{suffix}")
        };
        fs::write(dir.path().join(name), b"taken").unwrap();
    }

    let outcome = store(dir.path()).load();
    assert!(matches!(outcome.reason, LoadReason::QuarantineFailed));
    assert!(
        !outcome.writable,
        "the only way to preserve a file whose quarantine failed is to stop writing"
    );
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), b"garbage");
}

#[test]
fn an_unsupported_version_is_preserved_in_place_and_disables_writing() {
    let dir = tempfile::tempdir().unwrap();
    let newer = br#"{"schema_version":5,"checkpoints":{}}"#;
    fs::write(dir.path().join("state.json"), newer).unwrap();

    let outcome = store(dir.path()).load();
    assert!(matches!(
        outcome.reason,
        LoadReason::UnsupportedVersion { found: 5 }
    ));
    assert!(!outcome.writable);
    assert_eq!(
        fs::read(dir.path().join("state.json")).unwrap(),
        newer,
        "a newer build's state must survive a downgrade"
    );
    assert!(
        fs::read_dir(dir.path()).unwrap().count() == 1,
        "an unsupported file is preserved in place, not quarantined"
    );
}

#[test]
fn a_newer_file_is_classified_by_version_even_when_its_shape_is_alien() {
    // Deserializing the model first would call this garbage; the envelope is
    // the only thing every future version is obliged to keep.
    let dir = tempfile::tempdir().unwrap();
    let alien = br#"{"schema_version":5,"checkpoints":[1,2,3],"queues":{"a":true}}"#;
    fs::write(dir.path().join("state.json"), alien).unwrap();

    let outcome = store(dir.path()).load();
    assert!(matches!(
        outcome.reason,
        LoadReason::UnsupportedVersion { found: 5 }
    ));
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), alien);
}

#[test]
fn a_version_one_file_that_will_not_deserialize_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("state.json"),
        br#"{"schema_version":1,"checkpoints":{"not a media id":{}}}"#,
    )
    .unwrap();

    let outcome = store(dir.path()).load();
    let LoadReason::Quarantined { moved_to } = &outcome.reason else {
        panic!(
            "expected the unparseable map key to fail deserialization and be quarantined, got {:?}",
            outcome.reason
        );
    };
    assert!(
        moved_to.exists(),
        "the quarantine must actually have moved the file aside"
    );
    assert!(
        !dir.path().join("state.json").exists(),
        "the original name must no longer be present once quarantined"
    );
    assert!(outcome.writable);
}

/// design doc §4.5: the bump needs a real migration, not just a rejected
/// older version. A v1 file must load, and its data must arrive exactly as
/// a v2 build would have written it - `position` in `Some`, `estimated`
/// absent as `None` - with writing still on. A v1 file that loads unwritable
/// is the regression this task exists to prevent.
#[test]
fn a_v1_file_is_accepted_and_normalised_to_v2() {
    let dir = tempfile::tempdir().unwrap();
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
    fs::write(dir.path().join("state.json"), v1).unwrap();

    let outcome = store(dir.path()).load();
    assert!(
        matches!(outcome.reason, LoadReason::Loaded),
        "a migratable older version loads rather than being rejected: {:?}",
        outcome.reason
    );
    assert!(
        outcome.writable,
        "a v1 file that loads unwritable is the regression this task exists to prevent"
    );
    assert_eq!(outcome.state.schema_version(), SCHEMA_VERSION);
    assert_eq!(outcome.state.volume().as_gain(), 0.75);
    assert_eq!(outcome.state.current_media(), Some(&media("a")));

    let entry = outcome
        .state
        .entry_for(&media("a"))
        .expect("the v1 entry must survive normalisation");
    assert_eq!(entry.position, Some(Duration::from_secs(42)));
    assert_eq!(entry.estimated, None);
    assert!(!entry.completed);
    assert_eq!(entry.touch_seq, 7);
}

/// design doc §4.5: deserialisation preserves the file's version while
/// writing asserts the current one, so a migration that reads v1 but writes
/// without updating the envelope produces a file claiming v1 while holding
/// v2 data - which the next v1 build would read and quietly discard. This
/// test drives the whole cycle, not just the read, and inspects the raw
/// bytes on disk rather than trusting a re-load to catch a mislabelled file.
#[test]
fn the_upgrade_cycle_writes_v2_and_survives_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let v1 = br#"{
        "schema_version": 1,
        "current_media": null,
        "volume": 1.0,
        "checkpoints": {
            "local:/music/a.flac": {
                "position": { "secs": 10, "nanos": 0 },
                "completed": false,
                "touch_seq": 3,
                "updated_at": "1970-01-01T00:00:00Z"
            }
        }
    }"#;
    fs::write(dir.path().join("state.json"), v1).unwrap();

    let store = store(dir.path());
    let mut state = store.load().state;

    state.record(
        &PlaybackCheckpoint {
            media: media("b"),
            position: Duration::from_secs(20),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        },
        false,
    );
    store.write(&state).unwrap();

    let bytes = fs::read(dir.path().join("state.json")).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        raw["schema_version"], 4,
        "the file on disk must claim the current schema once it holds current-shaped data: {raw}"
    );

    let reloaded = store.load();
    assert!(matches!(reloaded.reason, LoadReason::Loaded));
    assert_eq!(
        reloaded
            .state
            .entry_for(&media("a"))
            .expect("the migrated entry must not have been lost")
            .position,
        Some(Duration::from_secs(10))
    );
    assert_eq!(
        reloaded
            .state
            .entry_for(&media("b"))
            .expect("the newly recorded entry must survive the round trip")
            .position,
        Some(Duration::from_secs(20))
    );
    assert_eq!(
        reloaded.state.len(),
        2,
        "no entry was lost across the cycle"
    );
}

/// design doc §4.5: the migratable band widens what `load` accepts by
/// exactly the versions this build knows how to bring forward, not
/// unconditionally. A version above `SCHEMA_VERSION` and one below the
/// oldest migratable version are both still genuinely unreadable.
#[test]
fn a_genuinely_unknown_version_is_still_preserved_unwritten() {
    let dir = tempfile::tempdir().unwrap();

    let too_new = br#"{"schema_version":5,"checkpoints":{}}"#;
    fs::write(dir.path().join("state.json"), too_new).unwrap();
    let outcome = store(dir.path()).load();
    assert!(matches!(
        outcome.reason,
        LoadReason::UnsupportedVersion { found: 5 }
    ));
    assert!(!outcome.writable);
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), too_new);

    let too_old = br#"{"schema_version":0,"checkpoints":{}}"#;
    fs::write(dir.path().join("state.json"), too_old).unwrap();
    let outcome = store(dir.path()).load();
    assert!(matches!(
        outcome.reason,
        LoadReason::UnsupportedVersion { found: 0 }
    ));
    assert!(!outcome.writable);
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), too_old);
}

#[cfg(unix)]
#[test]
fn the_directory_and_the_file_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("tenuto");
    let store = StateStore::new(nested.join("state.json"), Arc::new(FakeClock::new()));
    store.write(&state_with("a", 1)).unwrap();

    let dir_mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
    let file_mode = fs::metadata(nested.join("state.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "a permissive umask must not expose listening history"
    );
    assert_eq!(
        file_mode, 0o600,
        "the destination file's mode must be exactly 0o600, explicitly set"
    );
}

#[cfg(unix)]
#[test]
fn an_existing_permissive_directory_is_tightened() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("tenuto");
    fs::create_dir_all(&nested).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o755)).unwrap();

    let store = StateStore::new(nested.join("state.json"), Arc::new(FakeClock::new()));
    store.write(&state_with("a", 1)).unwrap();

    assert_eq!(
        fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
        0o700,
        "a directory that already existed is exactly the one a create-time mode never reaches"
    );
}
