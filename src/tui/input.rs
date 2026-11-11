//! The keyboard and mouse maps (design doc M5 §7): a key or mouse event, the
//! current [`UiState`] and the last drawn [`PlayerView`] (and, for the
//! mouse, where that frame's clickable parts landed) in, a list of
//! [`Effect`]s out. The mouse takes the view as a builder and calls it only
//! for a kind that reads it, so ignored kinds never pay for one. Nothing
//! here touches the terminal or the runtime — `tui::run` is the only thing
//! that executes an effect.

use std::time::Duration;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::application::runtime::{AppCommand, EnqueueItem};
use crate::application::transport::QUEUE_EMPTY;
use crate::application::view::PlayerView;
use crate::playlist::{PlaylistId, SortKey};
use crate::queue::{Direction, DisplayDuration, DurationSource, QueueEntryId};
use crate::tui::jump;
use crate::tui::render::{HitMap, TransportButton};
use crate::tui::state::{InputPurpose, OrganizeOp, Overlay, UiState};

const VOLUME_STEP: f32 = 0.05;
const SEEK_STEP: i64 = 10;

/// What a key press or mouse event asks the caller to do; `tui::run` is the
/// only executor.
#[derive(Clone, Debug)]
pub enum Effect {
    App(AppCommand),
    Quit,
    SetMouseCapture(bool),
    FullRedraw,
    OpenBrowser,
    CloseBrowser,
    /// A status message with no runtime effect, for a rule §4 defines as a
    /// no-op (an empty queue's Enter, for instance).
    Notice(&'static str),
}

/// Maps one key press to zero or more effects, following §7's table. Ctrl-C
/// always quits, even mid-keystroke in the input overlay, and Ctrl-L always
/// redraws (§11 recovery), leaving whichever overlay is open as it was;
/// every other rule is scoped to the open overlay, so a shortcut like `q` or
/// space can never fire while the user is typing.
pub fn handle_key(key: KeyEvent, ui: &mut UiState, view: &PlayerView) -> Vec<Effect> {
    if key.kind != KeyEventKind::Press {
        return Vec::new();
    }
    if is_ctrl_c(&key) {
        return vec![Effect::Quit];
    }
    if is_ctrl_l(&key) {
        return vec![Effect::FullRedraw];
    }
    match ui.overlay {
        Overlay::Input(purpose) => input_overlay(key, ui, purpose),
        Overlay::ConfirmClear(id) => confirm_overlay(key, ui, AppCommand::ClearPlaylist(id)),
        Overlay::ConfirmDelete(id) => confirm_overlay(key, ui, AppCommand::DeletePlaylist(id)),
        Overlay::Help => help_overlay(key, ui),
        Overlay::JumpToFile => jump_file_overlay(key, ui, view),
        Overlay::SortMenu(id) => sort_menu_overlay(key, ui, id),
        Overlay::ConfirmOrganize(id, op) => confirm_overlay(key, ui, organize_command(id, op)),
        // Every other key belongs to the browser: `tui::run` forwards it to
        // `BrowserState::handle_key` (see `routes_to_browser`), whose own
        // close becomes `Effect::CloseBrowser`. Ctrl-C and Ctrl-L never get
        // here.
        Overlay::Browser => Vec::new(),
        Overlay::None => no_overlay(key, ui, view),
    }
}

/// Whether `key` goes to the open browser's own key handling instead of
/// [`handle_key`]: every key while the browser overlay is open except
/// Ctrl-C, which quits from anywhere, and Ctrl-L, which redraws from
/// anywhere.
pub fn routes_to_browser(key: &KeyEvent, ui: &UiState) -> bool {
    ui.overlay == Overlay::Browser && !is_ctrl_c(key) && !is_ctrl_l(key)
}

/// Maps one mouse event to zero or more effects, mirroring `handle_key`:
/// pure, and the only executor is `tui::run`. Ignored entirely while mouse
/// capture is off or an overlay is open — a click must never act behind a
/// help/confirm/input/browser overlay. Only `Down(Left)` activates or
/// selects; `ScrollUp`/`ScrollDown` move the selection while the cursor is
/// over `hits.queue`; every other kind — drag, release, move, right or
/// middle click — is ignored.
///
/// `view` builds the player view, and is called only by a kind that reads
/// it: the ignored kinds, and a click blocked by capture or an overlay,
/// return without ever calling `view`, so a pass that drains nothing but
/// mouse motion constructs no view at all.
pub fn handle_mouse(
    event: MouseEvent,
    hits: &HitMap,
    ui: &mut UiState,
    view: impl FnOnce() -> PlayerView,
) -> Vec<Effect> {
    if !ui.mouse_capture || ui.overlay != Overlay::None {
        return Vec::new();
    }
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => left_click(event, hits, ui, &view()),
        MouseEventKind::ScrollUp if hits.queue.contains((event.column, event.row).into()) => {
            move_selection(ui, &view(), Direction::Up);
            Vec::new()
        }
        MouseEventKind::ScrollDown if hits.queue.contains((event.column, event.row).into()) => {
            move_selection(ui, &view(), Direction::Down);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// A playlist tab first, then a transport button, then a queue row — selecting it, or on the
/// already-selected row, playing it — then the progress bar.
fn left_click(
    event: MouseEvent,
    hits: &HitMap,
    ui: &mut UiState,
    view: &PlayerView,
) -> Vec<Effect> {
    let point = (event.column, event.row).into();
    if let Some((_, id)) = hits.tabs.iter().find(|(rect, _)| rect.contains(point)) {
        return vec![Effect::App(AppCommand::View(*id))];
    }
    if let Some((_, button)) = hits.buttons.iter().find(|(rect, _)| rect.contains(point)) {
        return vec![transport_effect(*button, view)];
    }
    if let Some((_, id)) = hits.rows.iter().find(|(rect, _)| rect.contains(point)) {
        return if ui.selected == Some(*id) {
            vec![Effect::App(AppCommand::PlayEntry(*id))]
        } else {
            ui.selected = Some(*id);
            Vec::new()
        };
    }
    // M13 §1.3: a click on the time label cycles it, mirroring Winamp's
    // click on its time display. It must precede the progress bar, whose
    // seek is the fallback below.
    if hits.time.contains(point) {
        ui.clock_mode = ui.clock_mode.next();
        return Vec::new();
    }
    seek_effect(point, hits.progress, view)
        .into_iter()
        .collect()
}

fn transport_effect(button: TransportButton, view: &PlayerView) -> Effect {
    Effect::App(match button {
        TransportButton::Previous => AppCommand::Previous,
        TransportButton::SeekBack => AppCommand::SeekBy(-SEEK_STEP),
        TransportButton::SeekForward => AppCommand::SeekBy(SEEK_STEP),
        TransportButton::PlayPause => AppCommand::PlayPause,
        TransportButton::Stop => AppCommand::Stop,
        TransportButton::Next => AppCommand::Next,
        TransportButton::Shuffle => AppCommand::ToggleShuffle(view.viewed),
    })
}

/// `SeekTo` the fraction of `progress` the point falls at, only when
/// `now_playing` is loaded with a decoder-confirmed duration; otherwise
/// `None` — including when the click landed outside `progress` at all
/// (`Rect::contains` is false for every point when `progress` is
/// zero-width, so that case needs no separate check). The runtime's own
/// `SeekTo` handling still applies its capability checks on top of this.
fn seek_effect(
    point: ratatui::layout::Position,
    progress: Rect,
    view: &PlayerView,
) -> Option<Effect> {
    if !progress.contains(point) {
        return None;
    }
    let now = view.now_playing.as_ref()?;
    if !now.loaded {
        return None;
    }
    let Some(DisplayDuration {
        value,
        source: DurationSource::Decoded(_),
    }) = now.duration
    else {
        return None;
    };
    let fraction = (f64::from(point.x - progress.x) / f64::from(progress.width)).clamp(0.0, 1.0);
    let target = Duration::from_secs_f64(value.as_secs_f64() * fraction);
    Some(Effect::App(AppCommand::SeekTo(target)))
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_ctrl_l(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Whether `key` carries a modifier that must turn off every ordinary
/// binding: CONTROL or ALT. Crossterm's legacy decoding reports Ctrl+letter
/// as the same `KeyCode::Char` the plain letter uses (Ctrl-D looks like
/// `d`), so without this check a Ctrl chord would silently fire a shortcut
/// — including a destructive one like `Remove` on Ctrl-D. SHIFT is exempt:
/// some terminals report `K`, `J`, `+`, `_`, `?`, `[` and `]` only with
/// SHIFT set, and those must keep working. The two exceptions that need
/// CONTROL — Ctrl-C and Ctrl-L — are checked before this and never reach
/// the callers of this function.
pub(crate) fn blocks_ordinary_bindings(key: &KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Printable characters append to `ui.input` (so `q`, space and `+` never
/// act as shortcuts here); a control character is dropped rather than
/// stored, since typed input is user text that later reaches the screen and
/// must never carry a raw control character.
fn input_overlay(key: KeyEvent, ui: &mut UiState, purpose: InputPurpose) -> Vec<Effect> {
    match key.code {
        KeyCode::Backspace => {
            ui.input.pop();
            Vec::new()
        }
        KeyCode::Enter => {
            let trimmed = ui.input.trim().to_owned();
            if let InputPurpose::JumpToTime = purpose {
                if trimmed.is_empty() {
                    ui.overlay = Overlay::None;
                    ui.input.clear();
                    return Vec::new();
                }
                return match jump::parse_hms(&trimmed) {
                    Ok(target) => {
                        ui.overlay = Overlay::None;
                        ui.input.clear();
                        ui.input_error = None;
                        vec![Effect::App(AppCommand::SeekTo(target))]
                    }
                    Err(error) => {
                        ui.input_error = Some(error);
                        Vec::new()
                    }
                };
            }
            ui.overlay = Overlay::None;
            ui.input.clear();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![Effect::App(match purpose {
                    InputPurpose::AddUrl(dest) => AppCommand::Enqueue {
                        dest,
                        items: vec![EnqueueItem::from_input(&trimmed)],
                    },
                    InputPurpose::NewPlaylist => AppCommand::CreatePlaylist(trimmed),
                    InputPurpose::Rename(id) => AppCommand::RenamePlaylist(id, trimmed),
                    // Returned above with its own Enter rule; this arm only
                    // keeps the match exhaustive.
                    InputPurpose::JumpToTime => return Vec::new(),
                })]
            }
        }
        KeyCode::Esc => {
            ui.overlay = Overlay::None;
            ui.input.clear();
            ui.input_error = None;
            Vec::new()
        }
        KeyCode::Char(c) if !c.is_control() && !blocks_ordinary_bindings(&key) => {
            ui.input.push(c);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// `y` (unmodified or shifted) runs `on_yes`, which carries the playlist the
/// confirmation captured when it opened; any other key, including a Ctrl/Alt
/// chord on `y`, closes the confirmation without effect.
fn confirm_overlay(key: KeyEvent, ui: &mut UiState, on_yes: AppCommand) -> Vec<Effect> {
    ui.overlay = Overlay::None;
    if key.code == KeyCode::Char('y') && !blocks_ordinary_bindings(&key) {
        vec![Effect::App(on_yes)]
    } else {
        Vec::new()
    }
}

/// `?` or Esc closes the help overlay; every other key is ignored.
fn help_overlay(key: KeyEvent, ui: &mut UiState) -> Vec<Effect> {
    if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
        ui.overlay = Overlay::None;
    }
    Vec::new()
}

/// The jump-to-file filter (M13 §1.4): typing narrows the viewed playlist's
/// matches, the arrows move within them, Enter plays the selected match —
/// the same door as Enter on a row — and Esc cancels. The overlay itself is
/// display-only: it never mutates the playlist.
fn jump_file_overlay(key: KeyEvent, ui: &mut UiState, view: &PlayerView) -> Vec<Effect> {
    let matches = jump::file_matches(view, &ui.filter);
    match key.code {
        KeyCode::Backspace => {
            ui.filter.pop();
            Vec::new()
        }
        KeyCode::Enter => {
            if let Some(id) = ui.selected
                && matches.contains(&id)
            {
                ui.overlay = Overlay::None;
                ui.filter.clear();
                vec![Effect::App(AppCommand::PlayEntry(id))]
            } else {
                Vec::new()
            }
        }
        KeyCode::Esc => {
            ui.overlay = Overlay::None;
            ui.filter.clear();
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_within_matches(ui, &matches, Direction::Up);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_within_matches(ui, &matches, Direction::Down);
            Vec::new()
        }
        KeyCode::Char(c) if !c.is_control() && !blocks_ordinary_bindings(&key) => {
            ui.filter.push(c);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Moves the selection within the filtered matches, never past either end;
/// with nothing selected, Down takes the first match and Up the last.
fn move_within_matches(ui: &mut UiState, matches: &[QueueEntryId], direction: Direction) {
    if matches.is_empty() {
        return;
    }
    let index = ui
        .selected
        .and_then(|id| matches.iter().position(|m| *m == id));
    let next = match (index, direction) {
        (Some(index), Direction::Up) => index.saturating_sub(1),
        (Some(index), Direction::Down) => (index + 1).min(matches.len() - 1),
        (None, Direction::Up) => matches.len() - 1,
        (None, Direction::Down) => 0,
    };
    ui.selected = Some(matches[next]);
}

/// The organize menu's items in display order (M13 §1.5); the cursor's
/// index into this table is what Enter runs. The last two are destructive
/// and route through a confirmation first.
pub(crate) const ORGANIZE_ITEMS: [&str; 8] = [
    "Sort by title",
    "Sort by artist",
    "Sort by path",
    "Sort by duration",
    "Reverse",
    "Randomize",
    "Remove duplicates",
    "Remove dead entries",
];

/// The organize menu (M13 §1.5): arrows move, Enter runs the item, Esc
/// closes. The two removals close into a confirmation instead of acting.
fn sort_menu_overlay(key: KeyEvent, ui: &mut UiState, id: PlaylistId) -> Vec<Effect> {
    let last = ORGANIZE_ITEMS.len() - 1;
    match key.code {
        KeyCode::Esc => {
            ui.overlay = Overlay::None;
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            ui.sort_cursor = ui.sort_cursor.saturating_sub(1);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            ui.sort_cursor = (ui.sort_cursor + 1).min(last);
            Vec::new()
        }
        KeyCode::Enter => {
            ui.overlay = Overlay::None;
            match ui.sort_cursor {
                0 => vec![Effect::App(AppCommand::SortPlaylist(id, SortKey::Title))],
                1 => vec![Effect::App(AppCommand::SortPlaylist(id, SortKey::Artist))],
                2 => vec![Effect::App(AppCommand::SortPlaylist(id, SortKey::Path))],
                3 => vec![Effect::App(AppCommand::SortPlaylist(id, SortKey::Duration))],
                4 => vec![Effect::App(AppCommand::ReversePlaylist(id))],
                5 => vec![Effect::App(AppCommand::RandomizePlaylist(id))],
                6 => {
                    ui.overlay = Overlay::ConfirmOrganize(id, OrganizeOp::Deduplicate);
                    Vec::new()
                }
                7 => {
                    ui.overlay = Overlay::ConfirmOrganize(id, OrganizeOp::Prune);
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

fn organize_command(id: PlaylistId, op: OrganizeOp) -> AppCommand {
    match op {
        OrganizeOp::Deduplicate => AppCommand::DeduplicatePlaylist(id),
        OrganizeOp::Prune => AppCommand::PruneDeadPlaylist(id),
    }
}

fn no_overlay(key: KeyEvent, ui: &mut UiState, view: &PlayerView) -> Vec<Effect> {
    // Ctrl-J (M13 §1.2) is the third chord with an effect, and like Ctrl-C
    // and Ctrl-L it is intercepted before the CONTROL check below, which
    // turns every other chord off.
    if key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return open_input(ui, InputPurpose::JumpToTime, "");
    }
    // Ctrl-C and Ctrl-L are the only chords with an effect here, and both
    // are handled in `handle_key` before any overlay; every other CONTROL or
    // ALT chord — Ctrl-D, Alt-d, and so on — must not fall through to the
    // plain-letter bindings below.
    if blocks_ordinary_bindings(&key) {
        return Vec::new();
    }
    match key.code {
        KeyCode::Char(' ') => vec![Effect::App(AppCommand::PlayPause)],
        KeyCode::Enter => match ui.selected {
            Some(id) => vec![Effect::App(AppCommand::PlayEntry(id))],
            None => vec![Effect::Notice(QUEUE_EMPTY)],
        },
        KeyCode::Up | KeyCode::Char('k') => {
            move_selection(ui, view, Direction::Up);
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_selection(ui, view, Direction::Down);
            Vec::new()
        }
        KeyCode::Char('K') => move_entry(ui, Direction::Up),
        KeyCode::Char('J') => move_entry(ui, Direction::Down),
        KeyCode::Left => vec![Effect::App(AppCommand::SeekBy(-SEEK_STEP))],
        KeyCode::Right => vec![Effect::App(AppCommand::SeekBy(SEEK_STEP))],
        KeyCode::Home => vec![Effect::App(AppCommand::Restart)],
        KeyCode::Char('-' | '_') => vec![Effect::App(AppCommand::AdjustVolume(-VOLUME_STEP))],
        KeyCode::Char('+' | '=') => vec![Effect::App(AppCommand::AdjustVolume(VOLUME_STEP))],
        KeyCode::Char('s') => vec![Effect::App(AppCommand::Stop)],
        KeyCode::Char('p') => vec![Effect::App(AppCommand::Play)],
        KeyCode::Char('[') => vec![Effect::App(AppCommand::Previous)],
        KeyCode::Char(']') => vec![Effect::App(AppCommand::Next)],
        KeyCode::Char('d') => ui
            .selected
            .map_or_else(Vec::new, |id| vec![Effect::App(AppCommand::Remove(id))]),
        KeyCode::Char('b') => vec![Effect::OpenBrowser],
        KeyCode::Char('a') => open_input(ui, InputPurpose::AddUrl(view.viewed), ""),
        KeyCode::Char('c') => {
            ui.overlay = Overlay::ConfirmClear(view.viewed);
            Vec::new()
        }
        KeyCode::Tab => vec![Effect::App(AppCommand::ViewNext)],
        KeyCode::BackTab => vec![Effect::App(AppCommand::ViewPrevious)],
        KeyCode::Char('z') => vec![Effect::App(AppCommand::ToggleShuffle(view.viewed))],
        KeyCode::Char('R') => vec![Effect::App(AppCommand::CycleRepeat)],
        KeyCode::Char('f') => {
            ui.overlay = Overlay::JumpToFile;
            ui.filter.clear();
            Vec::new()
        }
        KeyCode::Char('S') => {
            ui.overlay = Overlay::SortMenu(view.viewed);
            ui.sort_cursor = 0;
            Vec::new()
        }
        KeyCode::Char('t') => {
            ui.clock_mode = ui.clock_mode.next();
            Vec::new()
        }
        KeyCode::Char('n') => open_input(ui, InputPurpose::NewPlaylist, ""),
        KeyCode::Char('r') => {
            let name = view
                .tabs
                .iter()
                .find(|tab| tab.id == view.viewed)
                .map_or("", |tab| tab.name.as_str());
            open_input(ui, InputPurpose::Rename(view.viewed), name)
        }
        KeyCode::Char('D') if view.tabs.len() <= 1 => {
            vec![Effect::Notice("The last playlist cannot be deleted")]
        }
        KeyCode::Char('D') => {
            ui.overlay = Overlay::ConfirmDelete(view.viewed);
            Vec::new()
        }
        KeyCode::Char('?') => {
            ui.overlay = Overlay::Help;
            Vec::new()
        }
        KeyCode::Char('m') => {
            ui.mouse_capture = !ui.mouse_capture;
            vec![Effect::SetMouseCapture(ui.mouse_capture)]
        }
        KeyCode::Esc => Vec::new(),
        KeyCode::Char('q') => vec![Effect::Quit],
        _ => Vec::new(),
    }
}

/// Opens the input overlay for `purpose`, with `initial` already typed (the
/// current name, for a rename).
fn open_input(ui: &mut UiState, purpose: InputPurpose, initial: &str) -> Vec<Effect> {
    ui.overlay = Overlay::Input(purpose);
    ui.input.clear();
    ui.input.push_str(initial);
    ui.input_error = None;
    Vec::new()
}

/// Moves the selection by one row, never wrapping; an empty queue selects
/// nothing.
fn move_selection(ui: &mut UiState, view: &PlayerView, direction: Direction) {
    if view.rows.is_empty() {
        ui.selected = None;
        return;
    }
    let index = ui
        .selected
        .and_then(|id| view.rows.iter().position(|row| row.id == id));
    let next = match (index, direction) {
        (Some(index), Direction::Up) => index.saturating_sub(1),
        (Some(index), Direction::Down) => (index + 1).min(view.rows.len() - 1),
        (None, _) => 0,
    };
    ui.selected = view.rows.get(next).map(|row| row.id);
}

/// `Move` needs a concrete entry, so there is nothing to send with no
/// selection.
fn move_entry(ui: &UiState, direction: Direction) -> Vec<Effect> {
    ui.selected.map_or_else(Vec::new, |id| {
        vec![Effect::App(AppCommand::Move(id, direction))]
    })
}
