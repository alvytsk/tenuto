//! M8 §10: the playlist keys, and the overlays that act on the playlist they
//! captured when they opened — not on whatever is viewed at confirm time.

#[path = "support/views.rs"]
mod views;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tenuto::application::view::{PlayerView, PlaylistTab};
use tenuto::playlist::PlaylistId;
use tenuto::tui::input::{Effect, handle_key};
use tenuto::tui::state::{InputPurpose, Overlay, UiState};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
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
