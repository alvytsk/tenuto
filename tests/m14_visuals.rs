//! M14 §1.3: the visualizer mode persists as an additive raw string — the
//! M13 additive-field rule, with persistence carrying the spelling and
//! never parsing the vocabulary.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

use clap::Parser;
use runtime::rig_with;
use tenuto::application::runtime::AppCommand;
use tenuto::cli::{Cli, CliCommand};
use tenuto::persistence::model::PersistedState;
use tenuto::session::{Action, Session};

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

/// M14 §1.3: `set_visualizer_mode` writes the raw, already-validated
/// spelling into state and submits — the `set_repeat` shape. `None` is a
/// real value: cycling back to bars clears the field rather than leaving a
/// stale spelling behind.
#[test]
fn set_visualizer_mode_writes_and_clears_the_raw_spelling() {
    let mut session = Session::new(PersistedState::default());
    assert_eq!(
        session.state().visualizer_mode(),
        None,
        "the default carries no mode"
    );

    let action = session.set_visualizer_mode(Some("flow".into()));
    assert!(
        matches!(action, Action::Submit { .. }),
        "a mode change persists"
    );
    assert_eq!(session.state().visualizer_mode(), Some("flow"));

    let action = session.set_visualizer_mode(None);
    assert!(
        matches!(action, Action::Submit { .. }),
        "clearing persists too"
    );
    assert_eq!(
        session.state().visualizer_mode(),
        None,
        "None clears the field"
    );
}

/// M14 §2.1: the same shape for the theme, still carrying the raw stored
/// spelling — the session layer never parses either vocabulary.
#[test]
fn set_theme_writes_and_clears_the_raw_spelling() {
    let mut session = Session::new(PersistedState::default());
    assert_eq!(
        session.state().theme(),
        None,
        "the default carries no theme"
    );

    let action = session.set_theme(Some("winamp".into()));
    assert!(
        matches!(action, Action::Submit { .. }),
        "a theme change persists"
    );
    assert_eq!(session.state().theme(), Some("winamp"));

    let action = session.set_theme(None);
    assert!(
        matches!(action, Action::Submit { .. }),
        "clearing persists too"
    );
    assert_eq!(session.state().theme(), None, "None clears the field");
}

/// M14 §1.3: `AppCommand::SetTheme` dispatched through the runtime's
/// command handling lands in state — the `SetRepeat` shape (spec §1.3's
/// precedent), so the command path and the direct call agree.
#[test]
fn the_set_theme_command_lands_in_state_through_the_runtime() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime
        .handle(AppCommand::SetTheme(Some("winamp".into())));
    assert_eq!(
        rig.runtime.session().state().theme(),
        Some("winamp"),
        "the dispatched theme lands in state"
    );
}

/// M14 §1.3: `--visualizer` accepts a stored spelling and rejects an
/// unknown one at parse time, naming the valid set.
#[test]
fn tui_visualizer_parses_a_stored_spelling_and_names_the_valid_set_when_rejected() {
    let parsed =
        Cli::try_parse_from(["tenuto", "tui", "--visualizer", "flow"]).expect("flow parses");
    let Some(CliCommand::Tui { visualizer, .. }) = parsed.command else {
        panic!("`tenuto tui --visualizer flow` parses to the tui subcommand");
    };
    assert_eq!(visualizer.as_deref(), Some("flow"));

    let error = Cli::try_parse_from(["tenuto", "tui", "--visualizer", "plasma"])
        .expect_err("an unknown visualizer is a usage error");
    assert!(
        error.to_string().contains("valid: bars, scope, flow"),
        "{error}"
    );
}

/// Both flags absent: `None` on each. That absence is what the startup
/// (Task 9) reads as "use the saved state", so it must stay distinguishable
/// from an explicitly passed value.
#[test]
fn absent_theme_and_visualizer_flags_parse_to_none() {
    let parsed = Cli::try_parse_from(["tenuto", "tui"]).expect("the bare tui parses");
    let Some(CliCommand::Tui {
        theme, visualizer, ..
    }) = parsed.command
    else {
        panic!("`tenuto tui` parses to the tui subcommand");
    };
    assert_eq!(theme, None);
    assert_eq!(visualizer, None);
}
