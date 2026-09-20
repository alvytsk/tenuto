#[path = "support/views.rs"]
mod views;

use std::time::Duration;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use tenuto::application::runtime::{AppCommand, EnqueueItem};
use tenuto::application::transport::PlaybackPhase;
use tenuto::tui::input::{Effect, handle_key, handle_mouse, routes_to_browser};
use tenuto::tui::render::{HitMap, TransportButton, Visuals, draw};
use tenuto::tui::state::{InputPurpose, Overlay, UiState};
use views::{decoded, ids, playing, sample_view, view};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

fn shift(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}

fn app(effects: &[Effect]) -> Vec<&AppCommand> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::App(c) => Some(c),
            _ => None,
        })
        .collect()
}

#[test]
fn selection_moves_without_touching_playback() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.reconcile(&view, None);
    assert_eq!(
        ui.selected,
        Some(view.rows[0].id),
        "default selection is the first row"
    );
    let effects = handle_key(key(KeyCode::Down), &mut ui, &view);
    assert!(app(&effects).is_empty());
    assert_eq!(ui.selected, Some(view.rows[1].id));
    handle_key(key(KeyCode::Char('k')), &mut ui, &view);
    assert_eq!(ui.selected, Some(view.rows[0].id));
}

#[test]
fn transport_keys_map_to_their_commands_and_enter_carries_the_selection() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.selected = Some(view.rows[2].id);
    let selected = ui.selected;
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char(' ')), &mut ui, &view))[..],
        [AppCommand::PlayPause]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Enter), &mut ui, &view))[..],
        [AppCommand::PlayEntry(id)] if Some(*id) == selected
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char('p')), &mut ui, &view))[..],
        [AppCommand::Play]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Left), &mut ui, &view))[..],
        [AppCommand::SeekBy(-10)]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Home), &mut ui, &view))[..],
        [AppCommand::Restart]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char(']')), &mut ui, &view))[..],
        [AppCommand::Next]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char('J')), &mut ui, &view))[..],
        [AppCommand::Move(_, tenuto::queue::Direction::Down)]
    ));
}

#[test]
fn both_spellings_of_each_volume_key_work() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    for (code, delta) in [('-', -0.05), ('_', -0.05), ('+', 0.05), ('=', 0.05)] {
        let effects = handle_key(key(KeyCode::Char(code)), &mut ui, &view);
        assert!(
            matches!(app(&effects)[..], [AppCommand::AdjustVolume(d)] if (*d - delta).abs() < f32::EPSILON),
            "{code}"
        );
    }
}

#[test]
fn typing_a_url_never_triggers_shortcuts_and_enter_enqueues_it() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    assert_eq!(
        ui.overlay,
        Overlay::Input(InputPurpose::AddUrl(view.viewed))
    );
    for c in "https://q.example/ p+.mp3".chars() {
        assert!(
            handle_key(key(KeyCode::Char(c)), &mut ui, &view).is_empty(),
            "{c}"
        );
    }
    let effects = handle_key(key(KeyCode::Enter), &mut ui, &view);
    assert!(matches!(
        app(&effects)[..],
        [AppCommand::Enqueue { dest, items }] if *dest == view.viewed
            && matches!(&items[..], [EnqueueItem::Url(u)] if u == "https://q.example/ p+.mp3")
    ));
    assert_eq!(ui.overlay, Overlay::None);
}

#[test]
fn ctrl_c_quits_even_while_typing_and_esc_cancels_input() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    handle_key(key(KeyCode::Char('x')), &mut ui, &view);
    handle_key(key(KeyCode::Esc), &mut ui, &view);
    assert_eq!((ui.overlay, ui.input.as_str()), (Overlay::None, ""));
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    assert!(matches!(
        handle_key(ctrl('c'), &mut ui, &view)[..],
        [Effect::Quit]
    ));
}

#[test]
fn clearing_requires_confirmation() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert!(app(&handle_key(key(KeyCode::Char('n')), &mut ui, &view)).is_empty());
    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char('y')), &mut ui, &view))[..],
        [AppCommand::ClearPlaylist(id)] if *id == view.viewed
    ));
}

#[test]
fn mouse_toggle_and_full_redraw() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    assert!(matches!(
        handle_key(key(KeyCode::Char('m')), &mut ui, &view)[..],
        [Effect::SetMouseCapture(false)]
    ));
    assert!(!ui.mouse_capture);
    assert!(matches!(
        handle_key(ctrl('l'), &mut ui, &view)[..],
        [Effect::FullRedraw]
    ));
    let release = KeyEvent {
        code: KeyCode::Char('q'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: KeyEventState::NONE,
    };
    assert!(handle_key(release, &mut ui, &view).is_empty());
}

#[test]
fn enter_on_an_empty_queue_is_a_notice_not_a_command() {
    let mut view = sample_view();
    view.rows.clear();
    let mut ui = UiState::new(true);
    ui.selected = None;
    let effects = handle_key(key(KeyCode::Enter), &mut ui, &view);
    match &effects[..] {
        [Effect::Notice(message)] => {
            assert_eq!(*message, tenuto::application::transport::QUEUE_EMPTY);
        }
        other => panic!("expected a single Notice effect, got {other:?}"),
    }
    assert!(app(&effects).is_empty());
}

#[test]
fn ctrl_and_alt_chords_never_fire_the_plain_letter_shortcut() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.selected = Some(view.rows[0].id);

    // Ctrl-D would otherwise remove the selected row.
    assert!(handle_key(ctrl('d'), &mut ui, &view).is_empty());
    assert_eq!(ui.selected, Some(view.rows[0].id), "nothing was removed");

    // Ctrl-S would otherwise stop playback.
    assert!(handle_key(ctrl('s'), &mut ui, &view).is_empty());

    // Alt-D must be blocked the same way as Ctrl-D.
    assert!(handle_key(alt('d'), &mut ui, &view).is_empty());
    assert_eq!(
        ui.selected,
        Some(view.rows[0].id),
        "still nothing was removed"
    );
}

#[test]
fn shift_still_reaches_bindings_that_need_an_uppercase_or_symbol_key() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.selected = Some(view.rows[1].id);
    assert!(matches!(
        app(&handle_key(shift('K'), &mut ui, &view))[..],
        [AppCommand::Move(_, tenuto::queue::Direction::Up)]
    ));
}

fn draw_hits(view: &tenuto::application::view::PlayerView, ui: &UiState) -> HitMap {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("backend");
    let mut hits = HitMap::default();
    terminal
        .draw(|frame| {
            hits = draw(frame, view, ui, &Visuals::default());
        })
        .expect("draw");
    hits
}

fn centre(rect: Rect) -> (u16, u16) {
    (rect.x + rect.width / 2, rect.y + rect.height / 2)
}

fn mouse_down(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn clicking_a_row_selects_it_then_a_second_click_plays_it() {
    let view = sample_view();
    let hits = draw_hits(&view, &UiState::new(true));
    let mut ui = UiState::new(true);
    let row2 = view.rows[2].id;
    let rect = hits
        .rows
        .iter()
        .find(|(_, id)| *id == row2)
        .expect("row 2 is visible")
        .0;
    let (col, row) = centre(rect);

    let effects = handle_mouse(mouse_down(col, row), &hits, &mut ui, &view);
    assert!(app(&effects).is_empty(), "a first click only selects");
    assert_eq!(ui.selected, Some(row2));

    let effects = handle_mouse(mouse_down(col, row), &hits, &mut ui, &view);
    assert!(matches!(
        app(&effects)[..],
        [AppCommand::PlayEntry(id)] if *id == row2
    ));
}

#[test]
fn scrolling_inside_the_queue_moves_the_selection() {
    let view = sample_view();
    let hits = draw_hits(&view, &UiState::new(true));
    let mut ui = UiState::new(true);
    ui.reconcile(&view, None);
    assert_eq!(ui.selected, Some(view.rows[0].id));

    let (col, row) = (hits.queue.x + 1, hits.queue.y + 1);
    let effects = handle_mouse(
        mouse(MouseEventKind::ScrollDown, col, row),
        &hits,
        &mut ui,
        &view,
    );
    assert!(effects.is_empty());
    assert_eq!(ui.selected, Some(view.rows[1].id));
}

#[test]
fn clicking_play_pause_is_the_play_pause_command() {
    let view = sample_view();
    let hits = draw_hits(&view, &UiState::new(true));
    let mut ui = UiState::new(true);
    ui.selected = Some(view.rows[1].id);
    let rect = hits
        .buttons
        .iter()
        .find(|(_, b)| *b == TransportButton::PlayPause)
        .expect("play/pause button")
        .0;
    let (col, row) = centre(rect);
    let effects = handle_mouse(mouse_down(col, row), &hits, &mut ui, &view);
    assert!(matches!(app(&effects)[..], [AppCommand::PlayPause]));
}

#[test]
fn clicking_a_seek_button_seeks_like_its_arrow_key() {
    let view = sample_view();
    let hits = draw_hits(&view, &UiState::new(true));
    for (button, key) in [
        (TransportButton::SeekBack, KeyCode::Left),
        (TransportButton::SeekForward, KeyCode::Right),
    ] {
        let mut ui = UiState::new(true);
        let rect = hits
            .buttons
            .iter()
            .find(|(_, b)| *b == button)
            .expect("seek button")
            .0;
        let (col, row) = centre(rect);
        let clicked = handle_mouse(mouse_down(col, row), &hits, &mut ui, &view);
        let pressed = handle_key(KeyEvent::from(key), &mut ui, &view);
        let step = |effects: &[Effect]| match app(effects)[..] {
            [AppCommand::SeekBy(step)] => Some(*step),
            _ => None,
        };
        assert!(step(&clicked).is_some(), "{button:?}");
        assert_eq!(step(&clicked), step(&pressed), "{button:?}");
    }
}

#[test]
fn clicking_progress_seeks_a_loaded_decoded_track_but_not_an_undecoded_one() {
    let loaded = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(10)), false)),
    );
    let hits = draw_hits(&loaded, &UiState::new(true));
    let mut ui = UiState::new(true);
    let (col, row) = centre(hits.progress);
    let effects = handle_mouse(mouse_down(col, row), &hits, &mut ui, &loaded);
    match &app(&effects)[..] {
        [AppCommand::SeekTo(target)] => {
            assert!(
                *target >= Duration::from_secs(4) && *target <= Duration::from_secs(6),
                "{target:?}"
            );
        }
        other => panic!("expected a single SeekTo, got {other:?}"),
    }

    let unloaded = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, None, false)),
    );
    let hits_unloaded = draw_hits(&unloaded, &UiState::new(true));
    let (col, row) = centre(hits_unloaded.progress);
    let effects = handle_mouse(mouse_down(col, row), &hits_unloaded, &mut ui, &unloaded);
    assert!(
        effects.is_empty(),
        "no decoded duration means no seek: {effects:?}"
    );
}

#[test]
fn mouse_capture_off_ignores_every_event() {
    let view = sample_view();
    let hits = draw_hits(&view, &UiState::new(true));
    let mut ui = UiState::new(false);
    let row2 = view.rows[2].id;
    let (col, row) = centre(hits.rows.iter().find(|(_, id)| *id == row2).unwrap().0);
    assert!(handle_mouse(mouse_down(col, row), &hits, &mut ui, &view).is_empty());
    assert_eq!(ui.selected, None, "no selection without mouse capture");

    let (bcol, brow) = centre(hits.buttons[0].0);
    assert!(handle_mouse(mouse_down(bcol, brow), &hits, &mut ui, &view).is_empty());

    let (qcol, qrow) = (hits.queue.x + 1, hits.queue.y + 1);
    assert!(
        handle_mouse(
            mouse(MouseEventKind::ScrollDown, qcol, qrow),
            &hits,
            &mut ui,
            &view
        )
        .is_empty()
    );
}

#[test]
fn an_open_overlay_blocks_every_mouse_event() {
    let view = sample_view();
    let hits = draw_hits(&view, &UiState::new(true));
    let mut ui = UiState::new(true);
    ui.overlay = Overlay::Help;
    let (col, row) = centre(hits.buttons[0].0);
    assert!(handle_mouse(mouse_down(col, row), &hits, &mut ui, &view).is_empty());
    assert_eq!(ui.overlay, Overlay::Help, "overlay untouched by the click");
}

#[test]
fn only_left_button_down_activates_or_selects() {
    let view = sample_view();
    let hits = draw_hits(&view, &UiState::new(true));
    let mut ui = UiState::new(true);
    let row0 = view.rows[0].id;
    let (col, row) = centre(hits.rows.iter().find(|(_, id)| *id == row0).unwrap().0);
    for kind in [
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Down(MouseButton::Middle),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Moved,
    ] {
        assert!(
            handle_mouse(mouse(kind, col, row), &hits, &mut ui, &view).is_empty(),
            "{kind:?}"
        );
        assert_eq!(ui.selected, None, "{kind:?} must not select");
    }
}

#[test]
fn ctrl_l_redraws_from_every_overlay_without_closing_it() {
    let view = sample_view();
    for overlay in [
        Overlay::None,
        Overlay::Help,
        Overlay::Input(InputPurpose::AddUrl(view.viewed)),
        Overlay::ConfirmClear(view.viewed),
        Overlay::Browser,
    ] {
        let mut ui = UiState::new(true);
        ui.overlay = overlay;
        ui.input = "typed".into();
        assert!(
            matches!(
                &handle_key(ctrl('l'), &mut ui, &view)[..],
                [Effect::FullRedraw]
            ),
            "{overlay:?}"
        );
        assert_eq!(ui.overlay, overlay, "{overlay:?} stays open");
        assert_eq!(ui.input, "typed", "{overlay:?} keeps typed input");
    }
}

#[test]
fn a_ctrl_chord_on_y_does_not_confirm_the_clear() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::ConfirmClear(view.viewed));
    assert!(app(&handle_key(ctrl('y'), &mut ui, &view)).is_empty());
    assert_eq!(ui.overlay, Overlay::None, "any key closes the confirmation");
}

#[test]
fn the_open_browser_takes_every_key_but_ctrl_c_and_ctrl_l() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    assert!(
        matches!(
            &handle_key(key(KeyCode::Char('b')), &mut ui, &view)[..],
            [Effect::OpenBrowser]
        ),
        "b opens the browser"
    );
    assert!(!routes_to_browser(&key(KeyCode::Char('q')), &ui));

    ui.overlay = Overlay::Browser;
    for event in [
        key(KeyCode::Char('q')),
        key(KeyCode::Char('b')),
        key(KeyCode::Esc),
        key(KeyCode::Char(' ')),
        alt('d'),
    ] {
        assert!(routes_to_browser(&event, &ui), "{event:?}");
        assert!(handle_key(event, &mut ui, &view).is_empty(), "{event:?}");
    }
    assert!(
        !routes_to_browser(&ctrl('l'), &ui),
        "Ctrl-L is a recovery redraw"
    );
    assert!(matches!(
        &handle_key(ctrl('l'), &mut ui, &view)[..],
        [Effect::FullRedraw]
    ));
    assert_eq!(
        ui.overlay,
        Overlay::Browser,
        "a redraw leaves the browser open"
    );
    assert!(!routes_to_browser(&ctrl('c'), &ui));
    assert!(matches!(
        &handle_key(ctrl('c'), &mut ui, &view)[..],
        [Effect::Quit]
    ));
    assert_eq!(ui.overlay, Overlay::Browser);
}
