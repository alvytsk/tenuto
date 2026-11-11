//! M14 §2: the settings overlay — `o` opens it with the cursor on the
//! session's current theme, a cursor move re-themes the session and
//! dispatches the persisted spelling at once (the live preview), `Esc`/`o`
//! close without dispatching, and the drawn box lists the themes and the
//! app version over a frame that keeps the theme's background fill.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::{Terminal, backend::TestBackend};
use tenuto::application::runtime::AppCommand;
use tenuto::application::transport::PlaybackPhase;
use tenuto::tui::input::{Effect, handle_key};
use tenuto::tui::render::{Visuals, draw};
use tenuto::tui::state::{Overlay, UiState};
use tenuto::tui::theme::{THEME_ORDER, Theme, ThemeId};

#[path = "support/views.rs"]
mod views;

use views::{decoded, ids, playing, sample_view, view};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A session running `theme_id`, as startup would have resolved it.
fn themed(theme_id: ThemeId) -> UiState {
    UiState::new(true).with_appearance(theme_id.theme(), theme_id)
}

/// One plain key press through the real handler.
fn press(ui: &mut UiState, code: KeyCode) -> Vec<Effect> {
    handle_key(key(code), ui, &sample_view())
}

/// `o` from the main list opens the settings overlay with no effects, and
/// the cursor starts on the session's own theme.
#[test]
fn o_opens_settings_with_the_cursor_on_the_current_theme() {
    for (index, id) in THEME_ORDER.into_iter().enumerate() {
        let mut ui = themed(id);
        let effects = press(&mut ui, KeyCode::Char('o'));
        assert!(effects.is_empty(), "opening dispatches nothing");
        assert_eq!(ui.overlay, Overlay::Settings);
        assert_eq!(ui.settings_cursor, index, "{id:?} is THEME_ORDER[{index}]");
    }
}

/// Down (or j) from gruvbox: the session re-themes to winamp at once and
/// exactly one `SetTheme` carries the persisted spelling.
#[test]
fn a_move_down_rethemes_the_session_and_dispatches_the_spelling() {
    for code in [KeyCode::Down, KeyCode::Char('j')] {
        let mut ui = themed(ThemeId::Gruvbox);
        press(&mut ui, KeyCode::Char('o'));
        let effects = press(&mut ui, code);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::App(AppCommand::SetTheme(Some(spelling)))] if spelling == "winamp"
            ),
            "{code:?}: {effects:?}"
        );
        assert_eq!(ui.theme_id, ThemeId::Winamp);
        assert_eq!(ui.theme, ThemeId::Winamp.theme(), "the palette follows");
        assert_eq!(ui.settings_cursor, 1);
        assert_eq!(
            ui.overlay,
            Overlay::Settings,
            "a move keeps the overlay open"
        );
    }
}

/// The clamped edges: Up on gruvbox and Down on solarized-dark (either
/// spelling of the key) move nothing and dispatch nothing.
#[test]
fn the_clamped_edges_move_nothing_and_dispatch_nothing() {
    for code in [KeyCode::Up, KeyCode::Char('k')] {
        let mut ui = themed(ThemeId::Gruvbox);
        press(&mut ui, KeyCode::Char('o'));
        let effects = press(&mut ui, code);
        assert!(effects.is_empty(), "{code:?} at the top edge: {effects:?}");
        assert_eq!(ui.settings_cursor, 0);
        assert_eq!(ui.theme_id, ThemeId::Gruvbox);
        assert_eq!(ui.theme, ThemeId::Gruvbox.theme());
    }
    for code in [KeyCode::Down, KeyCode::Char('j')] {
        let mut ui = themed(ThemeId::SolarizedDark);
        press(&mut ui, KeyCode::Char('o'));
        let effects = press(&mut ui, code);
        assert!(
            effects.is_empty(),
            "{code:?} at the bottom edge: {effects:?}"
        );
        assert_eq!(ui.settings_cursor, 3);
        assert_eq!(ui.theme_id, ThemeId::SolarizedDark);
        assert_eq!(ui.theme, ThemeId::SolarizedDark.theme());
    }
}

/// `Esc` and `o` both close; closing dispatches nothing and leaves the
/// session's appearance as it was.
#[test]
fn esc_and_o_close_without_dispatching() {
    for code in [KeyCode::Esc, KeyCode::Char('o')] {
        let mut ui = themed(ThemeId::Winamp);
        press(&mut ui, KeyCode::Char('o'));
        assert_eq!(ui.overlay, Overlay::Settings);
        let effects = press(&mut ui, code);
        assert!(effects.is_empty(), "{code:?} closing dispatches nothing");
        assert_eq!(ui.overlay, Overlay::None);
        assert_eq!(ui.theme_id, ThemeId::Winamp);
        assert_eq!(ui.theme, ThemeId::Winamp.theme());
    }
}

/// Re-opening after a move starts on the moved-to theme, and opening
/// dispatches nothing even then.
#[test]
fn reopening_after_a_move_opens_on_the_moved_to_theme() {
    let mut ui = themed(ThemeId::Gruvbox);
    press(&mut ui, KeyCode::Char('o'));
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Down);
    assert_eq!(ui.theme_id, ThemeId::Nord);
    press(&mut ui, KeyCode::Esc);
    assert_eq!(ui.overlay, Overlay::None);
    let effects = press(&mut ui, KeyCode::Char('o'));
    assert!(effects.is_empty(), "reopening dispatches nothing");
    assert_eq!(ui.overlay, Overlay::Settings);
    assert_eq!(ui.settings_cursor, 2, "nord is THEME_ORDER[2]");
}

// ---------------------------------------------------- the drawn box

/// A full `draw` of the three-row playing view into a `width`×`height`
/// buffer with `ui`'s theme and overlay applied (the `m14_themes`
/// helper's shape).
fn filled_buffer(width: u16, height: u16, ui: &UiState) -> Buffer {
    let view = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("backend");
    terminal
        .draw(|frame| {
            draw(frame, &view, ui, &Visuals::default());
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// The frame's text, row by row.
fn rows_of(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area().width;
    (0..buffer.area().height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect()
        })
        .collect()
}

/// The box lists the four storage spellings in `THEME_ORDER` order, the
/// cursor line carries the `▶` marker, and a muted footer below them names
/// the app version.
#[test]
fn the_box_lists_the_spellings_marks_the_cursor_and_names_the_version() {
    let mut ui = themed(ThemeId::Winamp);
    press(&mut ui, KeyCode::Char('o'));
    let rows = rows_of(&filled_buffer(100, 30, &ui));

    let mut floor = 0;
    for id in THEME_ORDER {
        let spelling = id.as_stored().expect("total over the four");
        let y = rows
            .iter()
            .position(|row| row.contains(spelling))
            .unwrap_or_else(|| panic!("{spelling} is drawn in the box"));
        assert!(y >= floor, "{spelling} sits below the earlier rows");
        floor = y + 1;
        let row = &rows[y];
        let at = row.find(spelling).expect("the spelling's column");
        // Two columns of marker before the spelling: `▶ ` on the cursor
        // line (opened on winamp), air on the others.
        let expected_marker = if id == ThemeId::Winamp { "▶ " } else { "  " };
        let before: Vec<char> = row[..at].chars().collect();
        let marker_column: String = before[before.len().saturating_sub(2)..].iter().collect();
        assert_eq!(marker_column, expected_marker, "{spelling}'s marker");
    }
    let expected = format!("tenuto {}", env!("CARGO_PKG_VERSION"));
    let footer = rows
        .iter()
        .position(|row| row.contains(&expected))
        .unwrap_or_else(|| panic!("a footer carrying {expected:?} is drawn"));
    assert!(footer >= floor, "the footer sits below the theme rows");
}

/// The live preview behind the open box, at the normal tier's reference
/// size and a minimal-tier size: a cursor move re-colors the whole frame
/// on the same redraw — down to nord paints nord's background, back up to
/// winamp paints winamp's — and no cell is ever left on `Color::Reset`
/// (the M14 background-fill property, with the overlay open).
#[test]
fn a_move_live_previews_the_fill_behind_the_open_box_at_both_tiers() {
    for (width, height) in [(100, 30), (49, 17)] {
        let view = sample_view();
        let mut ui = themed(ThemeId::Winamp);
        press(&mut ui, KeyCode::Char('o'));
        assert_eq!(ui.overlay, Overlay::Settings);

        handle_key(key(KeyCode::Down), &mut ui, &view);
        assert_eq!(ui.theme_id, ThemeId::Nord);
        let nord = ThemeId::Nord.theme();
        let base = nord.bg.expect("nord paints a background");
        assert_backgrounds(
            &filled_buffer(width, height, &ui),
            width,
            height,
            &nord,
            base,
        );

        handle_key(key(KeyCode::Up), &mut ui, &view);
        assert_eq!(ui.settings_cursor, 1, "back on winamp");
        let winamp = ThemeId::Winamp.theme();
        let base = winamp.bg.expect("winamp paints a background");
        assert_backgrounds(
            &filled_buffer(width, height, &ui),
            width,
            height,
            &winamp,
            base,
        );
    }
}

/// The theme-chosen backgrounds widgets may paint over the fill (the
/// `m14_themes` helper's shape): the cover placeholder's panel and the
/// transport buttons' panel, green and shuffle amber — and, with the
/// settings box open, its cursor line's green. `base` is what a cell
/// carries where nothing drew over the fill; anything else means a cell
/// the fill did not cover, which would let the terminal's own background
/// through.
fn assert_backgrounds(buffer: &Buffer, width: u16, height: u16, theme: &Theme, base: Color) {
    let widget_bgs = [theme.panel, theme.green, theme.amber];
    for y in 0..height {
        for x in 0..width {
            let bg = buffer[(x, y)].bg;
            assert!(
                bg == base || widget_bgs.contains(&bg),
                "{width}×{height}: cell ({x}, {y}) carries {bg:?}"
            );
        }
    }
    // Where nothing draws, the base is exact.
    for (x, y) in [
        (0, 0),
        (width - 1, 0),
        (0, height - 1),
        (width - 1, height - 1),
    ] {
        assert_eq!(
            buffer[(x, y)].bg,
            base,
            "{width}×{height}: corner ({x}, {y})"
        );
    }
}
