//! M14 §1.3: the visualizer mode persists as an additive raw string — the
//! M13 additive-field rule, with persistence carrying the spelling and
//! never parsing the vocabulary.

use tenuto::persistence::model::PersistedState;

/// M14 §1.3: the visualizer mode round-trips through the state file. The
/// default state carries no mode: absent on disk means "the reader's
/// default" (bars), which is the *reader's* decision, not this layer's.
#[test]
fn the_selected_visualizer_mode_round_trips_through_json() {
    let mut state = PersistedState::default();
    assert_eq!(state.visualizer_mode(), None, "the default carries no mode");
    state.set_visualizer_mode(Some("flow".into()));

    let json = serde_json::to_string(&state).expect("serializes");
    assert!(json.contains("\"visualizer_mode\":\"flow\""), "{json}");

    let loaded: PersistedState = serde_json::from_str(&json).expect("round trips");
    assert_eq!(loaded.visualizer_mode(), Some("flow"));
}

/// Raw passthrough: persistence never parses the vocabulary, so an unknown
/// spelling survives the file untouched and the *reader* (which owns
/// `VisualizerMode`) defaults it.
#[test]
fn an_unknown_visualizer_mode_spelling_passes_through_raw() {
    let json = r#"{"schema_version":1,"checkpoints":{},"visualizer_mode":"plasma"}"#;
    let loaded: PersistedState = serde_json::from_str(json).expect("loads leniently");
    assert_eq!(loaded.visualizer_mode(), Some("plasma"));
}
