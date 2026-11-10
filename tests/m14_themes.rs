//! M14 §2.1: the selected theme persists as an additive raw string — the
//! M13 additive-field rule, with persistence carrying the spelling and
//! never parsing the vocabulary.

use clap::Parser;
use tenuto::cli::{Cli, CliCommand};
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

/// M14 §2.1: `--theme` accepts a stored spelling and rejects an unknown one
/// at parse time, naming the valid set. The accepted value stays the raw
/// string — the CLI, like persistence, carries the spelling and never
/// parses the vocabulary.
#[test]
fn tui_theme_parses_a_stored_spelling_and_names_the_valid_set_when_rejected() {
    let parsed =
        Cli::try_parse_from(["tenuto", "tui", "--theme", "winamp"]).expect("winamp parses");
    let Some(CliCommand::Tui { theme, .. }) = parsed.command else {
        panic!("`tenuto tui --theme winamp` parses to the tui subcommand");
    };
    assert_eq!(theme.as_deref(), Some("winamp"));

    let error = Cli::try_parse_from(["tenuto", "tui", "--theme", "dracula"])
        .expect_err("an unknown theme is a usage error");
    assert!(
        error
            .to_string()
            .contains("valid: gruvbox, winamp, nord, solarized-dark"),
        "{error}"
    );
}
