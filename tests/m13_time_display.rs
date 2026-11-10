//! M13 §1.3: the clock modes — `t` and a click on the time label cycle
//! elapsed → −remaining → total; −remaining and total need a decoded
//! duration and fall back to elapsed without one; a click on the time
//! never reaches the progress bar's seek.

#[path = "support/views.rs"]
mod views;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tenuto::application::transport::PlaybackPhase;
use tenuto::application::view::PlayerView;
use tenuto::queue::{DisplayDuration, DurationSource};
use tenuto::tui::input::{Effect, handle_key, handle_mouse};
use tenuto::tui::render::{HitMap, Visuals, draw};
use tenuto::tui::state::{ClockMode, UiState};
use views::{decoded, ids, playing, view};

fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn draw_content(view: &PlayerView, ui: &UiState) -> (String, HitMap) {
    let mut terminal = Terminal::new(TestBackend::new(100, 30))
        .unwrap_or_else(|error| panic!("backend: {error:?}"));
    let mut hits = HitMap::default();
    terminal
        .draw(|frame| {
            hits = draw(frame, view, ui, &Visuals::default());
        })
        .unwrap_or_else(|error| panic!("draw: {error:?}"));
    let content: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    (content, hits)
}

fn click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn loaded_view(duration: Option<DisplayDuration>, estimated: bool) -> PlayerView {
    let ids = ids();
    view(
        PlaybackPhase::Playing,
        Some(playing(ids[0], true, duration, estimated)),
    )
}

#[test]
fn t_cycles_the_three_modes_and_back() {
    let mut ui = UiState::new(true);
    assert_eq!(ui.clock_mode, ClockMode::Elapsed);
    for expected in [ClockMode::Remaining, ClockMode::Total, ClockMode::Elapsed] {
        let effects = handle_key(plain(KeyCode::Char('t')), &mut ui, &views::sample_view());
        assert!(effects.is_empty(), "cycling the clock sends no command");
        assert_eq!(ui.clock_mode, expected);
    }
}

#[test]
fn each_mode_changes_the_rendered_label() {
    let view = loaded_view(Some(decoded(185)), false);
    let mut ui = UiState::new(true);
    let (content, _) = draw_content(&view, &ui);
    assert!(content.contains("1:02 / 03:05"), "elapsed: {content:?}");

    ui.clock_mode = ClockMode::Remaining;
    let (content, _) = draw_content(&view, &ui);
    assert!(content.contains("-02:03 / 03:05"), "remaining: {content:?}");

    ui.clock_mode = ClockMode::Total;
    let (content, _) = draw_content(&view, &ui);
    assert!(content.contains("03:05 / 03:05"), "total: {content:?}");
}

#[test]
fn remaining_and_total_fall_back_to_elapsed_without_a_decoded_duration() {
    let declared = DisplayDuration {
        value: Duration::from_secs(3600),
        source: DurationSource::Declared,
    };
    let view = loaded_view(Some(declared), false);
    let mut ui = UiState::new(true);
    ui.clock_mode = ClockMode::Remaining;
    let (content, _) = draw_content(&view, &ui);
    assert!(
        content.contains("01:02 / (01:00:00)"),
        "declared keeps elapsed on the left: {content:?}"
    );
    ui.clock_mode = ClockMode::Total;
    let (content, _) = draw_content(&view, &ui);
    assert!(
        content.contains("01:02 / (01:00:00)"),
        "total also keeps elapsed on the left: {content:?}"
    );

    let view = loaded_view(None, false);
    let mut ui = UiState::new(true);
    ui.clock_mode = ClockMode::Remaining;
    let (content, _) = draw_content(&view, &ui);
    assert!(
        content.contains("01:02 / --:--"),
        "no duration at all: {content:?}"
    );
}

#[test]
fn an_unloaded_entry_keeps_its_saved_history_label_in_every_mode() {
    let ids = ids();
    let view = view(
        PlaybackPhase::Paused,
        Some(playing(ids[0], false, Some(decoded(185)), false)),
    );
    let mut ui = UiState::new(true);
    ui.clock_mode = ClockMode::Remaining;
    let (content, _) = draw_content(&view, &ui);
    assert!(
        content.contains("01:02 saved"),
        "the saved label, not a clock mode: {content:?}"
    );
}

#[test]
fn the_estimate_mark_stays_on_elapsed_alone() {
    let view = loaded_view(Some(decoded(185)), true);
    let mut ui = UiState::new(true);
    let (content, _) = draw_content(&view, &ui);
    assert!(
        content.contains("~01:02 / 03:05"),
        "elapsed marks: {content:?}"
    );
    ui.clock_mode = ClockMode::Remaining;
    let (content, _) = draw_content(&view, &ui);
    assert!(
        content.contains("-02:03 / 03:05"),
        "remaining is unmarked: {content:?}"
    );
}

#[test]
fn a_click_on_the_time_label_cycles_it_without_seeking() {
    let view = loaded_view(Some(decoded(185)), false);
    let mut ui = UiState::new(true);
    let (_, hits) = draw_content(&view, &ui);
    assert!(hits.time.width > 0, "the time label has its own region");
    let (column, row) = (hits.time.x + 1, hits.time.y);
    let effects = handle_mouse(click(column, row), &hits, &mut ui, || view.clone());
    assert!(effects.is_empty(), "cycling sends no command");
    assert_eq!(ui.clock_mode, ClockMode::Remaining);
    let effects = handle_mouse(click(column, row), &hits, &mut ui, || view.clone());
    assert!(effects.is_empty());
    assert_eq!(ui.clock_mode, ClockMode::Total);
}

#[test]
fn a_click_on_the_progress_bar_still_seeks() {
    let view = loaded_view(Some(decoded(185)), false);
    let mut ui = UiState::new(true);
    let (_, hits) = draw_content(&view, &ui);
    let column = hits.progress.x + hits.progress.width / 2;
    let effects = handle_mouse(click(column, hits.progress.y), &hits, &mut ui, || {
        view.clone()
    });
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::App(tenuto::application::runtime::AppCommand::SeekTo(_))
        )),
        "the bar seeks"
    );
    assert_eq!(ui.clock_mode, ClockMode::Elapsed);
}

#[test]
fn the_bar_ratio_is_mode_independent() {
    let view = loaded_view(Some(decoded(185)), false);
    let mut ui = UiState::new(true);
    let (content, _) = draw_content(&view, &ui);
    let elapsed_bars = content.matches('━').count();
    ui.clock_mode = ClockMode::Remaining;
    let (content, hits) = draw_content(&view, &ui);
    let remaining_bars = content.matches('━').count();
    assert_eq!(elapsed_bars, remaining_bars, "same fill either way");
    assert!(hits.progress.width > 0);
}
