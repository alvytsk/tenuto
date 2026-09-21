//! M8 §10: the playlist keys, the overlays that act on the playlist they
//! captured when they opened — not on whatever is viewed at confirm time —
//! and the drawn tab strip: the budget it is given inside the queue border,
//! the Minimal tier's row of its own, and what a hostile name renders as —
//! plus the height the help overlay has to stay inside.

#[path = "support/runtime.rs"]
mod runtime;
#[path = "support/views.rs"]
mod views;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::{Terminal, backend::TestBackend};
use tenuto::application::runtime::AppCommand;
use tenuto::application::view::{PlayerView, PlaylistTab};
use tenuto::persistence::model::PersistedState;
use tenuto::playlist::PlaylistId;
use tenuto::tui::input::{Effect, handle_key};
use tenuto::tui::render::{Visuals, draw};
use tenuto::tui::state::{InputPurpose, Overlay, UiState};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Every row of a `w`×`h` frame, as the terminal would show it. The same
/// helper `tests/m5_tui_render.rs` keeps for itself; one integration test
/// cannot import another's items without re-running its whole suite as a
/// module, so the dozen lines live here too.
fn screen(view: &PlayerView, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("backend");
    terminal
        .draw(|frame| {
            draw(frame, view, &UiState::new(true), &Visuals::default());
        })
        .expect("draw");
    let buffer: Buffer = terminal.backend().buffer().clone();
    let width = usize::from(buffer.area.width.max(1));
    buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The queue box's top border: the one row carrying the track count.
fn border_row(screen: &str) -> String {
    let rows: Vec<&str> = screen
        .lines()
        .filter(|line| line.contains("tracks") && line.contains('┌'))
        .collect();
    assert_eq!(rows.len(), 1, "one queue border row\n{screen}");
    rows[0].trim_end().to_owned()
}

fn two_tab_view() -> PlayerView {
    let mut view = views::sample_view();
    view.tabs.push(PlaylistTab {
        id: PlaylistId::from_raw_for_tests(2),
        name: "Jazz".into(),
        playing: false,
        shuffled: false,
    });
    view.viewed = PlaylistId::from_raw_for_tests(2);
    view
}

fn commands(effects: Vec<Effect>) -> Vec<String> {
    effects
        .into_iter()
        .map(|effect| format!("{effect:?}"))
        .collect()
}

#[test]
fn tab_and_shift_tab_cycle_the_view_and_z_toggles_shuffle_on_the_viewed_playlist() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    assert_eq!(
        commands(handle_key(key(KeyCode::Tab), &mut ui, &view)),
        ["App(ViewNext)"]
    );
    assert_eq!(
        commands(handle_key(key(KeyCode::BackTab), &mut ui, &view)),
        ["App(ViewPrevious)"]
    );
    assert_eq!(
        commands(handle_key(key(KeyCode::Char('z')), &mut ui, &view)),
        ["App(ToggleShuffle(PlaylistId(2)))"]
    );
}

#[test]
fn n_names_a_new_playlist_and_r_renames_the_captured_one_prefilled() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    handle_key(key(KeyCode::Char('n')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::Input(InputPurpose::NewPlaylist));
    for c in "Dusk".chars() {
        handle_key(key(KeyCode::Char(c)), &mut ui, &view);
    }
    assert_eq!(
        commands(handle_key(key(KeyCode::Enter), &mut ui, &view)),
        [r#"App(CreatePlaylist("Dusk"))"#]
    );

    handle_key(key(KeyCode::Char('r')), &mut ui, &view);
    assert_eq!(
        ui.overlay,
        Overlay::Input(InputPurpose::Rename(PlaylistId::from_raw_for_tests(2)))
    );
    assert_eq!(ui.input, "Jazz");
    // The view moves on before Enter: the rename still targets playlist 2.
    let mut moved = view.clone();
    moved.viewed = PlaylistId::from_raw_for_tests(1);
    handle_key(key(KeyCode::Char('!')), &mut ui, &moved);
    assert_eq!(
        commands(handle_key(key(KeyCode::Enter), &mut ui, &moved)),
        [r#"App(RenamePlaylist(PlaylistId(2), "Jazz!"))"#]
    );
}

#[test]
fn delete_and_clear_confirm_against_the_playlist_captured_when_they_opened() {
    let view = two_tab_view();
    let mut moved = view.clone();
    moved.viewed = PlaylistId::from_raw_for_tests(1);
    let mut ui = UiState::new(false);

    handle_key(key(KeyCode::Char('D')), &mut ui, &view);
    assert_eq!(
        ui.overlay,
        Overlay::ConfirmDelete(PlaylistId::from_raw_for_tests(2))
    );
    assert_eq!(
        commands(handle_key(key(KeyCode::Char('y')), &mut ui, &moved)),
        ["App(DeletePlaylist(PlaylistId(2)))"]
    );

    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert_eq!(
        commands(handle_key(key(KeyCode::Char('y')), &mut ui, &moved)),
        ["App(ClearPlaylist(PlaylistId(2)))"]
    );

    handle_key(key(KeyCode::Char('D')), &mut ui, &view);
    assert!(
        handle_key(key(KeyCode::Char('n')), &mut ui, &view).is_empty(),
        "anything but y cancels"
    );
    assert_eq!(ui.overlay, Overlay::None);
}

#[test]
fn the_last_playlist_cannot_even_open_the_delete_confirmation() {
    let view = views::sample_view();
    let mut ui = UiState::new(false);
    let effects = handle_key(key(KeyCode::Char('D')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::None);
    assert!(matches!(effects[..], [Effect::Notice(_)]));
}

#[test]
fn an_added_url_goes_to_the_playlist_viewed_when_a_was_pressed() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    assert_eq!(
        ui.overlay,
        Overlay::Input(InputPurpose::AddUrl(PlaylistId::from_raw_for_tests(2)))
    );
}

#[test]
fn switching_the_viewed_playlist_starts_its_listing_at_the_top() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    ui.reconcile(&view, None);
    ui.queue_offset = 2;
    ui.reconcile(&view, None);
    assert_eq!(ui.queue_offset, 2, "the same playlist keeps its scroll");
    let mut moved = view.clone();
    moved.viewed = PlaylistId::from_raw_for_tests(1);
    ui.reconcile(&moved, None);
    assert_eq!(ui.queue_offset, 0);
}

#[test]
fn space_p_next_and_previous_carry_no_selection() {
    let view = two_tab_view();
    let mut ui = UiState::new(false);
    ui.selected = view.rows.first().map(|row| row.id);
    assert_eq!(
        commands(handle_key(key(KeyCode::Char(' ')), &mut ui, &view)),
        ["App(PlayPause)"]
    );
    assert_eq!(
        commands(handle_key(key(KeyCode::Char('p')), &mut ui, &view)),
        ["App(Play)"]
    );
    assert_eq!(
        commands(handle_key(key(KeyCode::Char(']')), &mut ui, &view)),
        ["App(Next)"]
    );
}

/// A wide bordered frame: the strip sits where ` QUEUE ` did, and the
/// right-aligned count keeps its place.
#[test]
fn the_strip_shares_the_queue_border_with_the_track_count() {
    let view = views::sample_view();
    assert_eq!(
        border_row(&screen(&view, 100, 30)),
        "  ┌ ▶Default ────────────────────────────────────────────────────────────────────────── 3 tracks ┐"
    );

    let mut three = view.clone();
    three.tabs.push(PlaylistTab {
        id: PlaylistId::from_raw_for_tests(2),
        name: "Morning Coffee Selection".into(),
        playing: false,
        shuffled: true,
    });
    three.tabs.push(PlaylistTab {
        id: PlaylistId::from_raw_for_tests(3),
        name: "Evening Wind Down".into(),
        playing: false,
        shuffled: false,
    });
    three.viewed = PlaylistId::from_raw_for_tests(3);
    assert_eq!(
        border_row(&screen(&three, 100, 30)),
        "  ┌ ▶Default  Morning Coffee Selection ⤮  Evening Wind Down ─────────────────────────── 3 tracks ┐"
    );
}

/// The budget is the border inside the corners, less a column of air at each
/// end and the count: a name that would run into the count is clipped, and
/// both the count and the viewed tab survive intact.
#[test]
fn a_strip_that_would_reach_the_count_is_clipped_instead() {
    let mut view = views::sample_view();
    view.tabs.push(PlaylistTab {
        id: PlaylistId::from_raw_for_tests(2),
        name: "Morning Coffee Selection".into(),
        playing: false,
        shuffled: true,
    });
    view.tabs.push(PlaylistTab {
        // 34 columns: two more than the 32 the 50-column frame leaves.
        id: PlaylistId::from_raw_for_tests(3),
        name: "Evening Wind Down Long Playlist!!!".into(),
        playing: false,
        shuffled: false,
    });
    view.viewed = PlaylistId::from_raw_for_tests(3);
    let row = border_row(&screen(&view, 50, 22));
    assert_eq!(row, "  ┌ Evening Wind Down Long Playlist…  3 tracks ┐");
    assert!(row.contains(" 3 tracks "), "the count is intact: {row}");
    assert!(
        row.contains("Evening Wind Down Long Playlist…"),
        "the viewed tab is still there, clipped: {row}"
    );
}

/// The Minimal tier has no border to put the strip in, so it takes the first
/// row of the listing and the tracks follow it.
#[test]
fn the_minimal_tier_draws_the_compact_strip_above_the_tracks() {
    let rows: Vec<String> = screen(&views::sample_view(), 45, 16)
        .lines()
        .map(|line| line.trim_end().to_owned())
        .collect();
    let first = rows
        .iter()
        .position(|row| row.contains("Default"))
        .unwrap_or_else(|| panic!("a compact strip row\n{}", rows.join("\n")));
    assert_eq!(rows[first], "▶Default 1/1");
    assert_eq!(
        rows[first + 1..first + 4]
            .iter()
            .map(|row| row.split_whitespace().nth(1).unwrap_or(""))
            .collect::<Vec<_>>(),
        ["Morning", "Long", "Done"],
        "three track rows below the strip\n{}",
        rows.join("\n")
    );
}

/// A name carrying an ANSI escape reaches the strip the way production
/// builds one — through the runtime's view, which runs every tab name
/// through `displayable` — so nothing executable is drawn.
#[test]
fn a_hostile_playlist_name_reaches_the_strip_defused() {
    let mut rig = runtime::rig_with(PersistedState::default());
    rig.runtime
        .handle(AppCommand::CreatePlaylist("Jazz\u{1b}[31m\u{7}".to_owned()));
    let view = rig.runtime.view();
    let row = border_row(&screen(&view, 100, 30));
    assert!(
        !row.chars().any(char::is_control),
        "no control character reaches the terminal: {row:?}"
    );
    assert!(row.contains(r"Jazz\u{1b}[31m\u{7}"), "{row}");
}

/// The help overlay must still fit the terminal it fitted before M8: 19
/// lines plus a border is 21 rows, and its widest line (the Podcasts one, 67
/// bytes — `draw_help_overlay` measures with `str::len`) plus the border and
/// padding is 71 columns. At 71x21 the last line, the one that says how to
/// leave, has to be on screen.
#[test]
fn the_help_overlay_still_shows_its_quit_line_at_the_size_it_fitted_before_m8() {
    let mut ui = UiState::new(false);
    ui.overlay = Overlay::Help;
    let mut terminal = Terminal::new(TestBackend::new(71, 21)).expect("backend");
    terminal
        .draw(|frame| {
            draw(frame, &views::sample_view(), &ui, &Visuals::default());
        })
        .expect("draw");
    let buffer: Buffer = terminal.backend().buffer().clone();
    let rendered: String = buffer
        .content()
        .chunks(71)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    for line in ["q / Ctrl-C", "? / Esc", "z ", "Tab/Shift-Tab", "n / r / D"] {
        assert!(
            rendered.contains(line),
            "the overlay lost {line:?} at 71x21\n{rendered}"
        );
    }
}
