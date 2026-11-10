//! M13 §1.4: jump to file — `f` opens the filter, typing narrows the viewed
//! playlist's matches, the arrows move within them, Enter plays the
//! selected match through the same door as Enter on a row, Esc cancels.

#[path = "support/views.rs"]
mod views;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tenuto::application::runtime::AppCommand;
use tenuto::tui::input::{Effect, handle_key};
use tenuto::tui::jump::file_matches;
use tenuto::tui::state::{Overlay, UiState};
use views::sample_view;

fn press(ui: &mut UiState, code: KeyCode) -> Vec<Effect> {
    handle_key(KeyEvent::new(code, KeyModifiers::NONE), ui, &sample_view())
}

fn type_filter(ui: &mut UiState, text: &str) {
    for c in text.chars() {
        press(ui, KeyCode::Char(c));
    }
}

#[test]
fn the_filter_matches_artist_and_title_case_insensitively() {
    let view = sample_view();
    assert_eq!(file_matches(&view, "").len(), view.rows.len());
    let hits = file_matches(&view, "harbor");
    assert_eq!(hits, vec![view.rows[0].id], "artist matches");
    let hits = file_matches(&view, "TIDE");
    assert_eq!(hits, vec![view.rows[0].id], "title matches");
    assert!(file_matches(&view, "zzz").is_empty());
}

#[test]
fn opening_and_typing_narrows_the_matches() {
    let mut ui = UiState::new(true);
    let effects = press(&mut ui, KeyCode::Char('f'));
    assert!(effects.is_empty(), "opening sends nothing");
    assert_eq!(ui.overlay, Overlay::JumpToFile);
    assert!(ui.filter.is_empty());

    type_filter(&mut ui, "harbor");
    assert_eq!(ui.filter, "harbor");

    // The selection moves within the matches: Down takes the first match.
    press(&mut ui, KeyCode::Down);
    assert_eq!(ui.selected, Some(file_matches(&sample_view(), "harbor")[0]));
}

#[test]
fn enter_plays_the_selected_match_and_closes() {
    let mut ui = UiState::new(true);
    press(&mut ui, KeyCode::Char('f'));
    type_filter(&mut ui, "morning");
    press(&mut ui, KeyCode::Down);
    let selected = ui.selected;
    let effects = press(&mut ui, KeyCode::Enter);
    assert!(
        matches!(effects.as_slice(), [Effect::App(AppCommand::PlayEntry(id))] if Some(*id) == selected),
        "Enter plays the match like Enter on a row"
    );
    assert_eq!(ui.overlay, Overlay::None);
    assert!(ui.filter.is_empty());
}

#[test]
fn enter_with_no_match_does_nothing() {
    let mut ui = UiState::new(true);
    press(&mut ui, KeyCode::Char('f'));
    type_filter(&mut ui, "zzz");
    press(&mut ui, KeyCode::Down);
    let effects = press(&mut ui, KeyCode::Enter);
    assert!(effects.is_empty(), "nothing to play");
    assert_eq!(ui.overlay, Overlay::JumpToFile);
}

#[test]
fn esc_cancels_and_clears_the_filter() {
    let mut ui = UiState::new(true);
    press(&mut ui, KeyCode::Char('f'));
    type_filter(&mut ui, "harbor");
    let effects = press(&mut ui, KeyCode::Esc);
    assert!(effects.is_empty());
    assert_eq!(ui.overlay, Overlay::None);
    assert!(ui.filter.is_empty());
}

#[test]
fn the_arrows_never_leave_the_matches() {
    let mut ui = UiState::new(true);
    press(&mut ui, KeyCode::Char('f'));
    type_filter(&mut ui, "harbor");
    let matches = file_matches(&sample_view(), "harbor");
    assert_eq!(matches.len(), 1, "one match in the sample");
    // Down from nothing takes the match; further Downs stay there.
    press(&mut ui, KeyCode::Down);
    let first = ui.selected;
    press(&mut ui, KeyCode::Down);
    assert_eq!(ui.selected, first, "one match clamps both directions");
    press(&mut ui, KeyCode::Up);
    assert_eq!(ui.selected, first);
}

#[test]
fn a_moving_filter_keeps_the_selection_when_it_still_matches() {
    let mut ui = UiState::new(true);
    press(&mut ui, KeyCode::Char('f'));
    type_filter(&mut ui, "harbor");
    press(&mut ui, KeyCode::Down);
    let selected = ui.selected;
    type_filter(&mut ui, " tide");
    assert_eq!(
        ui.selected, selected,
        "the selection survives while it still matches"
    );
}

#[test]
fn the_overlay_shows_matches_over_the_player() {
    let mut ui = UiState::new(true);
    press(&mut ui, KeyCode::Char('f'));
    type_filter(&mut ui, "harbor");
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).expect("backend");
    let sample = sample_view();
    terminal
        .draw(|frame| {
            tenuto::tui::render::draw(
                frame,
                &sample,
                &ui,
                &tenuto::tui::render::Visuals::default(),
            );
        })
        .expect("draw");
    let content: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        content.contains("jump to file"),
        "the overlay opens: {content:?}"
    );
    assert!(
        content.contains("1 matched"),
        "the match count: {content:?}"
    );
    assert!(content.contains("Harbor"), "the match row: {content:?}");
}
