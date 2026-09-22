//! What the terminal front end remembers between frames that the player
//! itself does not: the selected queue row, the open overlay, whether mouse
//! capture is on, the text being typed and how far the queue is scrolled.

use crate::application::view::PlayerView;
use crate::playlist::PlaylistId;
use crate::queue::QueueEntryId;

/// What the input overlay is collecting, and for which playlist (§10): the
/// target is captured when the overlay opens, so a view that moves on before
/// Enter cannot redirect it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputPurpose {
    AddUrl(PlaylistId),
    NewPlaylist,
    Rename(PlaylistId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Overlay {
    None,
    Help,
    ConfirmClear(PlaylistId),
    ConfirmDelete(PlaylistId),
    Input(InputPurpose),
    Browser,
}

#[derive(Clone, Debug)]
pub struct UiState {
    /// Independent of what is playing: moving it never touches playback.
    pub selected: Option<QueueEntryId>,
    pub overlay: Overlay,
    pub mouse_capture: bool,
    pub input: String,
    /// The first queue row the listing tries to show; drawing still scrolls
    /// as far as it must to keep the selection visible.
    pub queue_offset: usize,
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
            queue_offset: 0,
            viewed: None,
        }
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
