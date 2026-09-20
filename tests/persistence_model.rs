//! The persisted model: ordering, eviction and the round trip.

mod support;

use std::time::Duration;

use support::media;
use tenuto::persistence::model::{MAX_ENTRIES, PersistedState, SCHEMA_VERSION};
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::playback::volume::Volume;
use time::OffsetDateTime;

fn checkpoint(name: &str, secs: u64) -> PlaybackCheckpoint {
    PlaybackCheckpoint {
        media: media(name),
        position: Duration::from_secs(secs),
        updated_at: OffsetDateTime::UNIX_EPOCH,
    }
}

#[test]
fn a_recorded_state_round_trips_through_json() {
    let mut state = PersistedState::default();
    state.set_volume(Volume::new(0.5));
    state.set_current_media(media("a"));
    state.record(&checkpoint("a", 93), false);

    let json = serde_json::to_string(&state).unwrap();
    assert!(json.contains("\"schema_version\":4"), "{json}");
    assert!(json.contains("local:/music/a.flac"), "{json}");
    assert!(
        !json.contains("next_seq"),
        "session counters stay out of the file: {json}"
    );

    let back: PersistedState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.schema_version(), SCHEMA_VERSION);
    assert_eq!(back.volume(), Volume::new(0.5));
    assert_eq!(back.current_media(), Some(&media("a")));
    assert_eq!(
        back.entry_for(&media("a")).unwrap().position,
        Some(Duration::from_secs(93))
    );
}

#[test]
fn touch_seq_is_assigned_only_by_record_and_never_regresses() {
    let mut state = PersistedState::default();
    state.record(&checkpoint("a", 10), false);
    state.record(&checkpoint("b", 20), false);
    state.record(&checkpoint("a", 30), false);

    let a = state.entry_for(&media("a")).unwrap().touch_seq;
    let b = state.entry_for(&media("b")).unwrap().touch_seq;
    assert!(a > b, "the later update to a must outrank b: {a} vs {b}");

    // Reading does not touch anything.
    let before = state.entry_for(&media("b")).unwrap().touch_seq;
    let _ = state.entry_for(&media("b"));
    let _ = state.completed_for(&media("b"));
    assert_eq!(state.entry_for(&media("b")).unwrap().touch_seq, before);
}

#[test]
fn next_seq_is_derived_at_load_and_beats_the_highest_stored() {
    let mut state = PersistedState::default();
    state.record(&checkpoint("a", 10), false);
    state.record(&checkpoint("b", 20), false);
    let highest = state.entry_for(&media("b")).unwrap().touch_seq;

    let json = serde_json::to_string(&state).unwrap();
    let mut reloaded: PersistedState = serde_json::from_str(&json).unwrap();
    reloaded.record(&checkpoint("c", 30), false);

    assert!(
        reloaded.entry_for(&media("c")).unwrap().touch_seq > highest,
        "a reloaded state must not reissue a sequence the file already holds"
    );
}

#[test]
fn a_truncated_file_cannot_produce_a_regressing_sequence() {
    // A hand-edited file whose entries carry large sequences: the next one
    // issued must still beat them.
    let json = r#"{
        "schema_version": 1,
        "current_media": null,
        "volume": 1.0,
        "checkpoints": {
            "local:/music/a.flac": {
                "position": { "secs": 5, "nanos": 0 },
                "completed": false,
                "touch_seq": 9000,
                "updated_at": "1970-01-01T00:00:00Z"
            }
        }
    }"#;
    let mut state: PersistedState = serde_json::from_str(json).unwrap();
    state.record(&checkpoint("b", 1), false);
    assert!(state.entry_for(&media("b")).unwrap().touch_seq > 9000);
}

#[test]
fn eviction_takes_the_lowest_touch_seq_that_is_not_current() {
    let mut state = PersistedState::default();
    for index in 0..MAX_ENTRIES {
        state.record(&checkpoint(&format!("m{index}"), index as u64), false);
    }
    // m0 is the oldest, so make it current and watch m1 go instead.
    state.set_current_media(media("m0"));
    assert_eq!(state.len(), MAX_ENTRIES);

    state.record(&checkpoint("incoming", 1), false);

    assert_eq!(state.len(), MAX_ENTRIES, "the cap counts the current entry");
    assert!(
        state.entry_for(&media("m0")).is_some(),
        "the current entry is never evictable"
    );
    assert!(
        state.entry_for(&media("m1")).is_none(),
        "the lowest non-current entry goes"
    );
    assert!(state.entry_for(&media("incoming")).is_some());
}

#[test]
fn updating_an_existing_entry_at_the_cap_evicts_nothing() {
    let mut state = PersistedState::default();
    for index in 0..MAX_ENTRIES {
        state.record(&checkpoint(&format!("m{index}"), index as u64), false);
    }
    state.record(&checkpoint("m5", 999), false);
    assert_eq!(state.len(), MAX_ENTRIES);
    assert_eq!(
        state.entry_for(&media("m5")).unwrap().position,
        Some(Duration::from_secs(999))
    );
}

#[test]
fn completion_is_stored_alongside_the_position_it_retains() {
    let mut state = PersistedState::default();
    state.record(&checkpoint("a", 240), true);
    assert!(state.completed_for(&media("a")));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Some(Duration::from_secs(240)),
        "D1 retains the position a completed entry finished at"
    );
}

#[test]
fn a_hand_edited_volume_cannot_deafen_or_silence() {
    let json = r#"{"schema_version":1,"volume":2.0,"checkpoints":{}}"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    assert_eq!(state.volume(), Volume::FULL);

    let json = r#"{"schema_version":1,"volume":-1.0,"checkpoints":{}}"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    assert_eq!(state.volume().as_gain(), 0.0);

    // A missing field is the value M1 starts at.
    let json = r#"{"schema_version":1,"checkpoints":{}}"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    assert_eq!(state.volume(), Volume::FULL);
}

/// design doc §4.1: the estimated location is a field alongside the
/// established one, carried through a v2 file's round trip like any other.
#[test]
fn a_v2_file_round_trips_its_estimated_location() {
    let json = r#"{
        "schema_version": 2,
        "current_media": null,
        "volume": 1.0,
        "checkpoints": {
            "local:/music/a.flac": {
                "position": { "secs": 30, "nanos": 0 },
                "completed": false,
                "touch_seq": 1,
                "updated_at": "1970-01-01T00:00:00Z",
                "estimated": { "secs": 45, "nanos": 0 }
            }
        }
    }"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(entry.position, Some(Duration::from_secs(30)));
    assert_eq!(entry.estimated, Some(Duration::from_secs(45)));

    let back = serde_json::to_string(&state).unwrap();
    let reparsed: PersistedState = serde_json::from_str(&back).unwrap();
    assert_eq!(
        reparsed.entry_for(&media("a")).unwrap().estimated,
        Some(Duration::from_secs(45)),
        "the estimated location must survive a full serialise/deserialise cycle: {back}"
    );
}

/// design doc §4.2: an entry can carry only an estimate, with nothing ever
/// established for it. `position` must read back exactly `None`, never a
/// zero the type cannot tell apart from an established start.
#[test]
fn an_entry_with_no_established_position_round_trips() {
    let json = r#"{
        "schema_version": 2,
        "current_media": null,
        "volume": 1.0,
        "checkpoints": {
            "local:/music/a.flac": {
                "position": null,
                "completed": false,
                "touch_seq": 1,
                "updated_at": "1970-01-01T00:00:00Z",
                "estimated": { "secs": 12, "nanos": 0 }
            }
        }
    }"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(
        entry.position, None,
        "an entry that exists only to carry an estimate has no established position"
    );
    assert_eq!(entry.estimated, Some(Duration::from_secs(12)));

    let back = serde_json::to_string(&state).unwrap();
    let reparsed: PersistedState = serde_json::from_str(&back).unwrap();
    let reparsed_entry = reparsed.entry_for(&media("a")).unwrap();
    assert_eq!(reparsed_entry.position, None, "{back}");
    assert_eq!(reparsed_entry.estimated, Some(Duration::from_secs(12)));
}

/// design doc §4.5: a v2 file with no estimate must stay comparable to what
/// M2 wrote — `"estimated":null` would not be, and would also make every v1
/// build's own quarantine-free tolerance for unknown-but-present fields moot
/// for no reason, since `record` never sets one.
#[test]
fn an_estimated_location_serialises_absent_rather_than_null_when_unset() {
    let mut state = PersistedState::default();
    state.record(&checkpoint("a", 5), false);

    let json = serde_json::to_string(&state).unwrap();
    assert!(
        !json.contains("\"estimated\""),
        "an unset estimate must be absent, not present as null: {json}"
    );
}
