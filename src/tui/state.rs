//! What the terminal front end remembers between frames that the player
//! itself does not: the selected queue row, the open overlay, whether mouse
//! capture is on, the text being typed, how far the queue is scrolled, and
//! the session's appearance — its theme.

use crate::application::view::PlayerView;
use crate::playlist::PlaylistId;
use crate::queue::QueueEntryId;
use crate::tui::theme::Theme;

/// What the input overlay is collecting, and for which playlist (§10): the
/// target is captured when the overlay opens, so a view that moves on before
/// Enter cannot redirect it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputPurpose {
    AddUrl(PlaylistId),
    NewPlaylist,
    Rename(PlaylistId),
    JumpToTime,
}

/// Which side of the clock the time label shows (M13 §1.3). Transient
/// `UiState`: the player always opens on elapsed, and nothing persists it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClockMode {
    #[default]
    Elapsed,
    Remaining,
    Total,
}

impl ClockMode {
    /// The `t` key's cycle: elapsed → −remaining → total → elapsed.
    pub fn next(self) -> Self {
        match self {
            Self::Elapsed => Self::Remaining,
            Self::Remaining => Self::Total,
            Self::Total => Self::Elapsed,
        }
    }
}

/// What the organize confirmation asks about (M13 §1.5): the two destructive
/// operations. Sorts run at once — they are reversible by re-sorting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrganizeOp {
    Deduplicate,
    Prune,
}

impl OrganizeOp {
    /// The confirmation's line, `y` to run.
    pub fn question(self) -> &'static str {
        match self {
            Self::Deduplicate => "Remove duplicate entries? y to confirm",
            Self::Prune => "Remove entries whose files are gone? y to confirm",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Overlay {
    None,
    Help,
    ConfirmClear(PlaylistId),
    ConfirmDelete(PlaylistId),
    ConfirmOrganize(PlaylistId, OrganizeOp),
    Input(InputPurpose),
    JumpToFile,
    SortMenu(PlaylistId),
    Browser,
}

#[derive(Clone, Debug)]
pub struct UiState {
    /// Independent of what is playing: moving it never touches playback.
    pub selected: Option<QueueEntryId>,
    pub overlay: Overlay,
    pub mouse_capture: bool,
    pub input: String,
    /// The typed-input overlay's parse error (M13 §1.2), shown under the
    /// input until the overlay closes or Enter runs again.
    pub input_error: Option<&'static str>,
    /// The time label's mode (M13 §1.3); `t` and a click on the label cycle
    /// it.
    pub clock_mode: ClockMode,
    /// The jump-to-file filter (M13 §1.4), live only while
    /// [`Overlay::JumpToFile`] is open.
    pub filter: String,
    /// The organize menu's cursor (M13 §1.5), valid while
    /// [`Overlay::SortMenu`] is open.
    pub sort_cursor: usize,
    /// The first queue row the listing tries to show; drawing still scrolls
    /// as far as it must to keep the selection visible.
    pub queue_offset: usize,
    /// The palette this session opened with (M14 §2.1): themes apply at
    /// startup and never switch live, so nothing mutates it afterwards.
    pub theme: Theme,
    /// The playlist the last reconciled view showed, so switching playlists
    /// starts its listing at the top instead of inheriting a scroll.
    viewed: Option<PlaylistId>,
}

impl UiState {
    pub fn new(mouse_capture: bool) -> Self {
        Self {
            selected: None,
            overlay: Overlay::None,
            mouse_capture,
            input: String::new(),
            input_error: None,
            clock_mode: ClockMode::Elapsed,
            filter: String::new(),
            sort_cursor: 0,
            queue_offset: 0,
            theme: Theme::default(),
            viewed: None,
        }
    }

    /// The session's appearance, resolved once at startup: the theme this
    /// session opens with.
    pub fn with_appearance(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Keeps the selection pointing at a row that exists after the queue
    /// changed: the `hint` when it names a row (the runtime's choice after a
    /// removal, say), else the current selection while it survives, else the
    /// first row. An empty queue selects nothing and scrolls to the top.
    /// A different playlist is a different listing, so it starts at the top.
    pub fn reconcile(&mut self, view: &PlayerView, hint: Option<QueueEntryId>) {
        if self.viewed.replace(view.viewed) != Some(view.viewed) {
            self.queue_offset = 0;
        }
        let present =
            |id: Option<QueueEntryId>| id.filter(|id| view.rows.iter().any(|row| row.id == *id));
        self.selected = present(hint)
            .or_else(|| present(self.selected))
            .or_else(|| view.rows.first().map(|row| row.id));
        self.queue_offset = self.queue_offset.min(view.rows.len().saturating_sub(1));
    }
}
