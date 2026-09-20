//! Schema 4: playlists on disk, migration from schema 3, and the ten ordered
//! recovery rules (M8 §4, §6).

use serde_json::{Value, json};
use std::sync::Arc;
use tenuto::clock::FakeClock;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::queue_codec::{ActiveProblem, PlaylistProblem, QueueReset};
use tenuto::persistence::store::{LoadReason, StateStore};

mod support;

fn track(id: u64, name: &str) -> Value {
    json!({ "id": id, "media": format!("local:/music/{name}.flac"),
            "source": { "kind": "local", "path": format!("/music/{name}.flac") } })
}

fn checkpoints() -> Value {
    json!({ "local:/music/a.flac": { "position": { "secs": 5, "nanos": 0 }, "completed": false,
            "touch_seq": 3, "updated_at": "2026-09-14T10:00:00Z" } })
}

fn v4(playlists: Value, playing: Value, extra: Value) -> Value {
    let mut state = json!({ "schema_version": 4, "current_media": "local:/music/a.flac",
        "volume": 0.5, "checkpoints": checkpoints(), "playlists": playlists, "playing": playing });
    if let (Some(state), Some(extra)) = (state.as_object_mut(), extra.as_object()) {
        state.extend(extra.clone());
    }
    state
}

struct Loaded {
    state: PersistedState,
    reset: Option<QueueReset>,
    #[allow(dead_code)] // Task 16 reuses this helper.
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn load(value: Value) -> Loaded {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let path = dir.path().join("state.json");
    let bytes = serde_json::to_vec_pretty(&value).unwrap_or_else(|error| panic!("json: {error}"));
    std::fs::write(&path, bytes).unwrap_or_else(|error| panic!("write: {error}"));
    let outcome = StateStore::new(path.clone(), Arc::new(FakeClock::new())).load();
    assert!(matches!(outcome.reason, LoadReason::Loaded));
    assert!(outcome.writable);
    Loaded {
        reset: outcome.queue_repair.map(|repair| repair.reset),
        state: outcome.state,
        path,
        _dir: dir,
    }
}

fn kept_checkpoint(state: &PersistedState) {
    assert_eq!(
        state.len(),
        1,
        "no recovery step may cost a checkpoint (P8)"
    );
}

fn ids(state: &PersistedState, playlist: usize) -> Vec<u64> {
    state.playlists()[playlist]
        .queue()
        .entries()
        .iter()
        .map(|e| e.id().get())
        .collect()
}

#[test]
fn a_schema_3_queue_becomes_the_default_playlist_with_its_ids_and_cursor() {
    let loaded = load(
        json!({ "schema_version": 3, "current_media": "local:/music/a.flac",
        "volume": 0.5, "checkpoints": checkpoints(),
        "queue": [track(4, "a"), track(9, "b")], "active_entry": 4 }),
    );
    let state = &loaded.state;
    assert_eq!(loaded.reset, None);
    assert_eq!(state.playlists().len(), 1);
    assert_eq!(state.playlists()[0].name(), "Default");
    assert_eq!(state.playlists()[0].id().get(), 1);
    assert_eq!(state.playing().get(), 1);
    assert_eq!(ids(state, 0), [4, 9]);
    assert_eq!(state.queue().active().map(|id| id.get()), Some(4));
    assert_eq!(state.playlists()[0].shuffle(), None);
    assert_eq!(state.schema_version(), 4);
    kept_checkpoint(state);
}

#[test]
fn a_schema_4_file_round_trips() {
    let value = v4(
        json!([{ "id": 1, "name": "Default", "shuffle": null, "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "Jazz", "shuffle": { "seed": 42, "first": 3 },
                 "entries": [track(2, "b"), track(3, "c")], "active_entry": 3 }]),
        json!(1),
        json!({ "next_entry_id": 4, "next_playlist_id": 3 }),
    );
    let loaded = load(value);
    assert_eq!(loaded.reset, None);
    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["schema_version"], 4);
    assert_eq!(written["playing"], 1);
    assert_eq!(written["next_entry_id"], 4);
    assert_eq!(written["next_playlist_id"], 3);
    assert_eq!(
        written["playlists"][1]["shuffle"],
        json!({ "seed": 42, "first": 3 })
    );
    assert_eq!(written["playlists"][1]["active_entry"], 3);
    assert!(written.get("queue").is_none() && written.get("active_entry").is_none());
    let again: PersistedState = serde_json::from_value(written.clone()).expect("reads back");
    assert_eq!(serde_json::to_value(&again).expect("serializes"), written);
}

#[test]
fn only_the_playing_cursor_is_judged_against_current_media() {
    // current_media is a.flac. Playlist 2's cursor is b.flac and must survive.
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "B", "entries": [track(2, "b")], "active_entry": 2 }]),
        json!(1),
        json!({}),
    ));
    assert_eq!(loaded.reset, None);
    assert_eq!(
        loaded.state.playlists()[1]
            .queue()
            .active()
            .map(|id| id.get()),
        Some(2)
    );

    let mismatch = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "z")], "active_entry": 1 }]),
        json!(1),
        json!({}),
    ));
    assert_eq!(
        mismatch.reset,
        Some(QueueReset::ActiveReference(ActiveProblem::MediaMismatch))
    );
    assert_eq!(mismatch.state.queue().active(), None);
    kept_checkpoint(&mismatch.state);
}

#[test]
fn malformed_playlists_become_one_empty_default() {
    let loaded = load(v4(json!("nope"), json!(1), json!({})));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::Malformed))
    );
    assert_eq!(loaded.state.playlists().len(), 1);
    assert_eq!(loaded.state.playlists()[0].name(), "Default");
    kept_checkpoint(&loaded.state);
}

#[test]
fn a_damaged_playlist_resets_alone_and_keeps_its_id_and_name() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "Good", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "Bad", "entries": [track(5, "b"), track(5, "c")], "active_entry": null }]),
        json!(1),
        json!({}),
    ));
    assert!(matches!(loaded.reset, Some(QueueReset::WholeQueue(_))));
    assert_eq!(ids(&loaded.state, 0), [1]);
    assert_eq!(loaded.state.playlists()[1].name(), "Bad");
    assert_eq!(loaded.state.playlists()[1].id().get(), 2);
    assert!(ids(&loaded.state, 1).is_empty());
    kept_checkpoint(&loaded.state);
}

#[test]
fn a_repeated_playlist_id_gets_a_fresh_one_that_appears_nowhere_in_the_file() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [], "active_entry": null },
               { "id": 1, "name": "B", "entries": [track(1, "b")], "active_entry": null },
               { "id": 7, "name": "C", "entries": [], "active_entry": null }]),
        json!(1),
        json!({}),
    ));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::DuplicatePlaylistId))
    );
    let got: Vec<u64> = loaded
        .state
        .playlists()
        .iter()
        .map(|p| p.id().get())
        .collect();
    assert_eq!(
        got,
        [1, 8, 7],
        "8, not 2..=7: the counter is computed over the whole file first"
    );
    assert_eq!(
        ids(&loaded.state, 1),
        [1],
        "the repaired playlist keeps its content"
    );
}

#[test]
fn an_entry_id_repeated_across_playlists_is_renumbered_and_its_cursor_clears() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "B", "entries": [track(1, "b"), track(6, "c")], "active_entry": 1 }]),
        json!(1),
        json!({}),
    ));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::DuplicateEntryId))
    );
    assert_eq!(ids(&loaded.state, 0), [1]);
    assert_eq!(ids(&loaded.state, 1), [7, 6]);
    assert_eq!(loaded.state.playlists()[1].queue().active(), None);
}

#[test]
fn with_entry_ids_exhausted_the_later_duplicate_is_dropped_and_the_repair_is_a_fixed_point() {
    let max = u64::MAX;
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(max, "a")], "active_entry": max },
               { "id": 2, "name": "B", "entries": [track(max, "b"), track(3, "c")], "active_entry": null }]),
        json!(1),
        json!({}),
    ));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::IdsExhausted))
    );
    assert_eq!(ids(&loaded.state, 0), [max]);
    assert_eq!(ids(&loaded.state, 1), [3]);
    kept_checkpoint(&loaded.state);

    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["next_entry_id"], Value::Null);
    let reopened = load(written.clone());
    assert_eq!(
        reopened.reset, None,
        "saving then reopening repairs nothing further"
    );
    assert_eq!(
        serde_json::to_value(&reopened.state).expect("serializes"),
        written
    );
}

#[test]
fn with_playlist_ids_exhausted_the_later_conflicting_playlist_is_dropped() {
    let max = u64::MAX;
    let loaded = load(v4(
        json!([{ "id": max, "name": "A", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": max, "name": "B", "entries": [track(2, "b")], "active_entry": null }]),
        json!(max),
        json!({}),
    ));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::IdsExhausted))
    );
    assert_eq!(loaded.state.playlists().len(), 1);
    assert_eq!(loaded.state.playlists()[0].name(), "A");
    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["next_playlist_id"], Value::Null);
    assert_eq!(load(written).reset, None);
}

#[test]
fn a_stored_null_counter_stays_exhausted_even_when_no_max_id_remains() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "a")], "active_entry": 1 }]),
        json!(1),
        json!({ "next_entry_id": null }),
    ));
    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["next_entry_id"], Value::Null);
}

#[test]
fn every_fallback_to_a_default_playlist_keeps_both_stored_counters() {
    for playlists in [json!("nope"), Value::Null] {
        let loaded = load(v4(
            playlists,
            json!(1),
            json!({ "next_entry_id": 50, "next_playlist_id": 9 }),
        ));
        let written = serde_json::to_value(&loaded.state).expect("serializes");
        assert_eq!(
            written["next_entry_id"], 50,
            "entry IDs below 50 were handed out once; never again"
        );
        assert_eq!(
            written["playlists"][0]["id"], 9,
            "the default takes a fresh playlist ID"
        );
        assert_eq!(written["next_playlist_id"], 10);
        kept_checkpoint(&loaded.state);
    }
}

#[test]
fn a_fallback_never_revives_an_exhausted_namespace() {
    let loaded = load(v4(
        json!("nope"),
        json!(1),
        json!({ "next_entry_id": null, "next_playlist_id": null }),
    ));
    let written = serde_json::to_value(&loaded.state).expect("serializes");
    assert_eq!(written["next_entry_id"], Value::Null);
    assert_eq!(written["next_playlist_id"], Value::Null);
    assert_eq!(loaded.state.playlists().len(), 1, "P1 still holds");

    // Every playlist dropped by rule 3's exhausted fallback: same guarantee.
    let max = u64::MAX;
    let all_dropped = load(v4(
        json!([{ "id": "bad", "name": "A", "entries": [track(max, "a")], "active_entry": null }]),
        json!(1),
        json!({ "next_playlist_id": null }),
    ));
    let written = serde_json::to_value(&all_dropped.state).expect("serializes");
    assert_eq!(
        written["next_entry_id"],
        Value::Null,
        "u64::MAX was seen in the file"
    );
    assert_eq!(written["next_playlist_id"], Value::Null);
}

#[test]
fn names_are_cleaned_and_an_empty_one_is_named_after_its_id() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": format!("  {}  ", "x".repeat(60)), "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "   ", "entries": [], "active_entry": null },
               { "id": 3, "entries": [], "active_entry": null }]),
        json!(1),
        json!({}),
    ));
    let names: Vec<&str> = loaded.state.playlists().iter().map(|p| p.name()).collect();
    assert_eq!(names[0].chars().count(), 40);
    assert_eq!(&names[1..], ["Playlist 2", "Playlist 3"]);
}

#[test]
fn both_caps_truncate_in_file_order_and_say_so() {
    let many: Vec<Value> = (1..=40)
        .map(|i| json!({ "id": i, "name": format!("P{i}"), "entries": [], "active_entry": null }))
        .collect();
    let loaded = load(v4(json!(many), json!(1), json!({})));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::TooManyPlaylists {
            found: 40
        }))
    );
    assert_eq!(loaded.state.playlists().len(), 32);

    let first: Vec<Value> = (1..=4000).map(|i| track(i, &format!("t{i}"))).collect();
    let second: Vec<Value> = (4001..=4200).map(|i| track(i, &format!("t{i}"))).collect();
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": first, "active_entry": null },
               { "id": 2, "name": "B", "entries": second, "active_entry": 4200 }]),
        json!(1),
        json!({ "current_media": null }),
    ));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::OverCapacity {
            found: 4200
        }))
    );
    assert_eq!(loaded.state.total_entries(), 4096);
    assert_eq!(ids(&loaded.state, 1).last(), Some(&4096));
    assert_eq!(
        loaded.state.playlists()[1].queue().active(),
        None,
        "a truncated cursor clears"
    );
}

#[test]
fn a_dangling_playing_falls_back_to_the_first_playlist_and_repoints_current_media() {
    // current_media is a.flac; the first playlist's cursor is b.flac and must survive.
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "entries": [track(1, "b")], "active_entry": 1 }]),
        json!(99),
        json!({}),
    ));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::DanglingPlaying))
    );
    assert_eq!(loaded.state.playing().get(), 1);
    assert_eq!(loaded.state.queue().active().map(|id| id.get()), Some(1));
    assert_eq!(loaded.state.current_media(), Some(&support::media("b")));
    kept_checkpoint(&loaded.state);
}

#[test]
fn a_bad_shuffle_turns_shuffle_off_and_a_foreign_first_becomes_none() {
    let loaded = load(v4(
        json!([{ "id": 1, "name": "A", "shuffle": "yes", "entries": [track(1, "a")], "active_entry": 1 },
               { "id": 2, "name": "B", "shuffle": { "seed": 7, "first": 1 }, "entries": [track(2, "b")], "active_entry": null }]),
        json!(1),
        json!({}),
    ));
    assert_eq!(
        loaded.reset,
        Some(QueueReset::Playlists(PlaylistProblem::Shuffle))
    );
    assert_eq!(loaded.state.playlists()[0].shuffle(), None);
    let shuffle = loaded.state.playlists()[1].shuffle().expect("kept");
    assert_eq!((shuffle.seed, shuffle.first), (7, None));
}
