//! M13 §1.2: the jump-to-time grammar, the Ctrl-J overlay's effects, the
//! parse-error line, and the guard reuse for unloaded tracks.

#[path = "support/views.rs"]
mod views;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tenuto::application::runtime::AppCommand;
use tenuto::tui::input::{Effect, handle_key};
use tenuto::tui::jump::parse_hms;
use tenuto::tui::state::{InputPurpose, Overlay, UiState};
use views::sample_view;

fn press(ui: &mut UiState, code: KeyCode) -> Vec<Effect> {
    handle_key(KeyEvent::new(code, KeyModifiers::NONE), ui, &sample_view())
}

#[test]
fn the_grammar_accepts_each_form() {
    // One-digit parts are valid: the spec's own example has one.
    assert_eq!(parse_hms("1:23:00"), Ok(Duration::from_secs(4980)));
    assert_eq!(parse_hms("1:2"), Ok(Duration::from_secs(62)));
    assert_eq!(parse_hms("1:2:3"), Ok(Duration::from_secs(3723)));
    let cases = [
        ("07", Duration::from_secs(7)),
        ("07.5", Duration::new(7, 500_000_000)),
        ("00", Duration::ZERO),
        ("01:07", Duration::from_secs(67)),
        ("01:07.25", Duration::new(67, 250_000_000)),
        ("99:59:59", Duration::from_secs(359_999)),
        ("00:00.001", Duration::new(0, 1_000_000)),
    ];
    for (raw, expected) in cases {
        assert_eq!(parse_hms(raw), Ok(expected), "{raw:?}");
    }
}

#[test]
fn the_grammar_rejects_everything_else_with_a_static_error() {
    for raw in [
        "", "007", ":07", "07:", "1:07:", "0:07.", "07.25.5", "01:60", "01:23:60", "a:07", "+7",
        "-7", " 7", "07 ", "01.5:07", "07.1234",
    ] {
        let error = parse_hms(raw).expect_err(&format!("{raw:?} must not parse"));
        assert!(
            error.starts_with("Use ") || error.contains("must be") || error.contains("too long"),
            "{raw:?}: unexpected error {error:?}"
        );
    }
}

#[test]
fn ctrl_j_opens_the_overlay_and_a_good_time_seeks() {
    let mut ui = UiState::new(true);
    let effects = handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &mut ui,
        &sample_view(),
    );
    assert!(
        effects.is_empty(),
        "opening the box has no immediate effect"
    );
    assert_eq!(ui.overlay, Overlay::Input(InputPurpose::JumpToTime));

    for c in "1:23:00".chars() {
        press(&mut ui, KeyCode::Char(c));
    }
    let effects = press(&mut ui, KeyCode::Enter);
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::App(AppCommand::SeekTo(t)) if *t == Duration::from_secs(4980))
        ),
        "Enter seeks to the parsed time"
    );
    assert_eq!(ui.overlay, Overlay::None);
    assert!(ui.input_error.is_none());
}

#[test]
fn a_malformed_time_stays_in_the_box_with_the_error() {
    let mut ui = UiState::new(true);
    handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &mut ui,
        &sample_view(),
    );
    for c in "0:07.".chars() {
        press(&mut ui, KeyCode::Char(c));
    }
    let effects = press(&mut ui, KeyCode::Enter);
    assert!(effects.is_empty(), "a rejected time sends nothing");
    assert_eq!(ui.overlay, Overlay::Input(InputPurpose::JumpToTime));
    assert!(ui.input_error.is_some(), "the error line shows");

    // Fixing the time and resubmitting seeks and closes.
    press(&mut ui, KeyCode::Backspace);
    let effects = press(&mut ui, KeyCode::Enter);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::App(AppCommand::SeekTo(_))))
    );
    assert_eq!(ui.overlay, Overlay::None);
    assert!(ui.input_error.is_none());
}

#[test]
fn esc_cancels_and_clears_the_error() {
    let mut ui = UiState::new(true);
    handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &mut ui,
        &sample_view(),
    );
    for c in "1:2".chars() {
        press(&mut ui, KeyCode::Char(c));
    }
    press(&mut ui, KeyCode::Enter);
    let effects = press(&mut ui, KeyCode::Esc);
    assert!(effects.is_empty());
    assert_eq!(ui.overlay, Overlay::None);
    assert!(ui.input.is_empty());
    assert!(ui.input_error.is_none());
}

#[test]
fn an_empty_submit_closes_the_box_without_an_error() {
    // The typing boxes' shared rule: an empty submit closes silently.
    let mut ui = UiState::new(true);
    handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &mut ui,
        &sample_view(),
    );
    let effects = press(&mut ui, KeyCode::Enter);
    assert!(effects.is_empty());
    assert_eq!(ui.overlay, Overlay::None);
    assert!(ui.input_error.is_none());
}

#[test]
fn ctrl_j_never_fires_inside_an_open_overlay() {
    let mut ui = UiState::new(true);
    press(&mut ui, KeyCode::Char('n'));
    assert_eq!(ui.overlay, Overlay::Input(InputPurpose::NewPlaylist));
    let effects = handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &mut ui,
        &sample_view(),
    );
    assert!(effects.is_empty());
    assert_eq!(
        ui.overlay,
        Overlay::Input(InputPurpose::NewPlaylist),
        "the open box keeps its purpose"
    );
}

#[test]
fn a_plain_j_is_not_the_chord() {
    let mut ui = UiState::new(true);
    let view = sample_view();
    let before = ui.selected;
    let effects = handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        &mut ui,
        &view,
    );
    assert!(effects.is_empty(), "j moves the selection only");
    assert_eq!(ui.overlay, Overlay::None);
    assert_ne!(ui.selected, before, "selection moved down one row");
}

#[test]
fn a_press_event_only_reaches_the_map() {
    let mut ui = UiState::new(true);
    let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
    let released = KeyEvent {
        kind: KeyEventKind::Release,
        ..key
    };
    assert!(handle_key(released, &mut ui, &sample_view()).is_empty());
    assert_eq!(ui.overlay, Overlay::None);
}
