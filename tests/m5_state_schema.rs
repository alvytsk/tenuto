//! Schema 3's queue fields, decoded apart from listening history (task
//! brief: "Schema 3 with independently decoded queue fields").

use serde_json::json;
use tenuto::persistence::model::{PersistedState, SCHEMA_VERSION};
use tenuto::persistence::queue_codec::{ActiveProblem, QueueProblem, QueueReset, recover_queue};

mod support;
use support::media;

fn entry(id: u64, name: &str) -> serde_json::Value {
    json!({ "id": id, "media": format!("local:/music/{name}.flac"),
            "source": { "kind": "local", "path": format!("/music/{name}.flac") } })
}

fn history() -> serde_json::Value {
    json!({ "local:/music/a.flac": { "position": { "secs": 42, "nanos": 0 }, "completed": false,
             "touch_seq": 7, "updated_at": "2026-09-14T10:00:00Z" } })
}

#[test]
fn schema_three_migrates_its_queue_into_the_default_playlist() {
    assert_eq!(SCHEMA_VERSION, 4);
    let file = json!({ "schema_version": 3, "current_media": "local:/music/a.flac", "volume": 0.5,
        "checkpoints": history(), "queue": [entry(4, "a"), entry(9, "a")], "active_entry": 9 });
    let state: PersistedState = serde_json::from_value(file).expect("valid");
    assert_eq!(state.queue().len(), 2);
    assert_eq!(state.queue().active().map(|id| id.get()), Some(9));
    // `from_value` decodes without the store's version stamp, so the label is
    // still the file's; only `StateStore::load` migrates it.
    let back = serde_json::to_value(&state).expect("serializes");
    assert_eq!(back["schema_version"], 3);
    assert_eq!(back["playlists"][0]["active_entry"], 9);
    assert_eq!(back["playlists"][0]["entries"][1]["id"], 9);
    assert_eq!(back["playing"], 1);
}

#[test]
fn a_version_two_file_migrates_to_an_empty_queue_keeping_current_media() {
    let file = json!({ "schema_version": 2, "current_media": "local:/music/a.flac", "volume": 0.5,
        "checkpoints": history(), "queue": "ignored before schema 3" });
    let state: PersistedState = serde_json::from_value(file).expect("valid");
    assert!(state.queue().is_empty());
    assert_eq!(state.current_media(), Some(&media("a")));
    assert!(state.entry_for(&media("a")).is_some());
}

#[test]
fn a_wrong_queue_type_resets_only_the_queue() {
    let file = json!({ "schema_version": 3, "current_media": "local:/music/a.flac", "volume": 0.25,
        "checkpoints": history(), "queue": 7, "active_entry": "x" });
    let state: PersistedState = serde_json::from_value(file).expect("base survives");
    assert!(state.queue().is_empty());
    assert_eq!(state.volume().percent(), 25);
    assert_eq!(
        state
            .entry_for(&media("a"))
            .and_then(|e| e.position)
            .map(|p| p.as_secs()),
        Some(42)
    );
    assert_eq!(state.current_media(), Some(&media("a")));
}

#[test]
fn every_whole_queue_problem_is_classified() {
    let cur = media("a");
    let cases = [
        (json!([entry(1, "a"), { "id": 2 }]), QueueProblem::Malformed),
        (
            json!([entry(1, "a"), entry(1, "b")]),
            QueueProblem::DuplicateIds,
        ),
        (
            json!([{ "id": 1, "media": "local:/music/a.flac", "source": { "kind": "local", "path": "/music/b.flac" } }]),
            QueueProblem::SourceMismatch,
        ),
        (
            serde_json::Value::Array((1..=4097).map(|i| entry(i, &format!("t{i}"))).collect()),
            QueueProblem::OverCapacity { found: 4097 },
        ),
    ];
    for (queue, problem) in cases {
        let (recovered, reset) = recover_queue(Some(&queue), Some(&json!(1)), Some(&cur));
        assert!(recovered.is_empty() && recovered.active().is_none());
        assert_eq!(reset, Some(QueueReset::WholeQueue(problem)));
    }
}

#[test]
fn active_reference_problems_keep_the_entries() {
    let queue = json!([entry(1, "a"), entry(2, "b")]);
    let cur = media("a");
    for (active, current, problem) in [
        (json!("one"), Some(&cur), ActiveProblem::Malformed),
        (json!(99), Some(&cur), ActiveProblem::Dangling),
        (json!(2), Some(&cur), ActiveProblem::MediaMismatch),
        (json!(1), None, ActiveProblem::MediaMismatch),
    ] {
        let (recovered, reset) = recover_queue(Some(&queue), Some(&active), current);
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered.active(), None);
        assert_eq!(reset, Some(QueueReset::ActiveReference(problem)));
    }
}

#[test]
fn a_missing_or_null_active_reference_needs_no_recovery() {
    let queue = json!([entry(1, "a")]);
    assert_eq!(recover_queue(Some(&queue), None, None).1, None);
    assert_eq!(
        recover_queue(Some(&queue), Some(&serde_json::Value::Null), None).1,
        None
    );
    assert_eq!(recover_queue(None, None, None).1, None);
}

#[test]
fn a_duplicate_occurrence_is_never_inferred_active_from_media() {
    let file = json!({ "schema_version": 3, "current_media": "local:/music/a.flac", "volume": 1.0,
        "checkpoints": {}, "queue": [entry(1, "a"), entry(2, "a")] });
    let state: PersistedState = serde_json::from_value(file).expect("valid");
    assert_eq!(state.queue().active(), None);
}

#[test]
fn a_valid_maximum_id_is_preserved_but_cannot_be_reallocated() {
    use tenuto::media::id::MediaId;
    use tenuto::queue::{DisplayMetadata, IdAllocator, NewQueueEntry, QueueError, QueueSource};
    let file = json!({ "schema_version": 3, "queue": [entry(u64::MAX, "a")] });
    let state: PersistedState = serde_json::from_value(file).expect("valid maximum ID");
    let mut queue = state.queue().clone();
    let before = queue.clone();
    let MediaId::LocalFile(path) = media("b") else {
        unreachable!()
    };
    let item = NewQueueEntry::new(
        media("b"),
        QueueSource::LocalFile(path),
        DisplayMetadata::default(),
    )
    .expect("entry");
    // Mirrors `RawState::into_state`, which raises the shared allocator past
    // every recovered entry's ID: the maximum has already been observed.
    let mut ids = IdAllocator::default();
    ids.observe(u64::MAX);
    assert_eq!(
        queue.enqueue(vec![item], &mut ids),
        Err(QueueError::IdExhausted)
    );
    assert_eq!(queue, before);
    assert_eq!(queue.entries()[0].id().get(), u64::MAX);
}
