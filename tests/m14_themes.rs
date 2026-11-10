//! M14 §2.1: the selected theme persists as an additive raw string — the
//! M13 additive-field rule, with persistence carrying the spelling and
//! never parsing the vocabulary.

use tenuto::persistence::model::PersistedState;

/// M14 §2.1: the theme round-trips through the state file. The default
/// state carries no theme: absent on disk means "the reader's default"
/// (gruvbox), which is the *reader's* decision, not this layer's.
#[test]
fn the_selected_theme_round_trips_through_json() {
    let mut state = PersistedState::default();
    assert_eq!(state.theme(), None, "the default carries no theme");
    state.set_theme(Some("winamp".into()));

    let json = serde_json::to_string(&state).expect("serializes");
    assert!(json.contains("\"theme\":\"winamp\""), "{json}");

    let loaded: PersistedState = serde_json::from_str(&json).expect("round trips");
    assert_eq!(loaded.theme(), Some("winamp"));
}

/// Raw passthrough: persistence never parses the vocabulary, so an unknown
/// spelling survives the file untouched and the *reader* (which owns
/// `ThemeId`) defaults it.
#[test]
fn an_unknown_theme_spelling_passes_through_raw() {
    let json = r#"{"schema_version":1,"checkpoints":{},"theme":"dracula"}"#;
    let loaded: PersistedState = serde_json::from_str(json).expect("loads leniently");
    assert_eq!(loaded.theme(), Some("dracula"));
}
