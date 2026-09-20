//! The immutable snapshot a front end draws from: queue rows, what is
//! playing, the transport phase and the status line. Every string here has
//! already been made safe to print — a title can come from a feed or a
//! decoder tag, and a terminal must never receive its control characters.

use std::time::Duration;

use crate::application::transport::PlaybackPhase;
use crate::commands::displayable;
use crate::media::capabilities::SeekSupport;
use crate::media::display::{display_name, episode_name};
use crate::media::id::MediaId;
use crate::persistence::model::{PersistedCheckpoint, PersistedState};
use crate::playback::command::LoadRequestId;
use crate::playback::state::PlaybackState;
use crate::playback::volume::Volume;
use crate::playlist::PlaylistId;
use crate::queue::{DisplayDuration, QueueEntry, QueueEntryId};

/// What listening history says about a media, as a row shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SavedHistory {
    Position { at: Duration, estimated: bool },
    Played,
    Unknown,
}

/// The saved-history label for a checkpoint entry: completed first, then an
/// estimated location (the listener's most recent intent), then an
/// established position. An entry with neither is `Unknown`; no entry at all
/// has no label.
pub fn saved_history(entry: Option<&PersistedCheckpoint>) -> Option<SavedHistory> {
    let entry = entry?;
    Some(if entry.completed {
        SavedHistory::Played
    } else if let Some(at) = entry.estimated {
        SavedHistory::Position {
            at,
            estimated: true,
        }
    } else if let Some(at) = entry.position {
        SavedHistory::Position {
            at,
            estimated: false,
        }
    } else {
        SavedHistory::Unknown
    })
}

pub fn format_saved(history: SavedHistory) -> String {
    match history {
        SavedHistory::Position { at, estimated } => {
            let seconds = at.as_secs();
            let mark = if estimated { "~" } else { "" };
            format!("{mark}{:02}:{:02} saved", seconds / 60, seconds % 60)
        }
        SavedHistory::Played => "played".to_owned(),
        SavedHistory::Unknown => "position unknown".to_owned(),
    }
}

#[derive(Clone, Debug)]
pub struct QueueRow {
    pub id: QueueEntryId,
    pub media: MediaId,
    pub title: String,
    pub subtitle: Option<String>,
    pub duration: Option<DisplayDuration>,
    pub saved: Option<SavedHistory>,
}

#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub entry: Option<QueueEntryId>,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<String>,
    /// Whether the engine holds this entry's adopted playback. `false` for an
    /// active entry that has not been loaded in this session (or whose
    /// playback a later load displaced).
    pub loaded: bool,
    pub state: PlaybackState,
    pub position: Duration,
    pub duration: Option<DisplayDuration>,
    pub estimated_position: bool,
    pub degraded: bool,
    pub buffering: bool,
    pub seek: Option<SeekSupport>,
    pub saved: Option<SavedHistory>,
    pub session_rev: u64,
    pub load: Option<LoadRequestId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceStatus {
    Saving,
    /// Nothing this session writes reaches the disk.
    Unsaved,
    /// Writes should reach the disk, but the latest attempt failed.
    Failing,
}

/// One playlist as the tab strip shows it (M8 §10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistTab {
    pub id: PlaylistId,
    /// Already sanitized for the terminal.
    pub name: String,
    pub playing: bool,
    pub shuffled: bool,
}

#[derive(Clone, Debug)]
pub struct PlayerView {
    /// The *viewed* playlist's rows; `active` and `now_playing` stay the
    /// playing playlist's.
    pub rows: Vec<QueueRow>,
    pub tabs: Vec<PlaylistTab>,
    pub viewed: PlaylistId,
    pub active: Option<QueueEntryId>,
    pub now_playing: Option<NowPlaying>,
    pub phase: PlaybackPhase,
    pub volume: Volume,
    pub status: Option<String>,
    pub persistence: PersistenceStatus,
    pub last_requested: Option<QueueEntryId>,
    /// The engine is holding indefinite media: no timeline, no seeking.
    pub live: bool,
    /// A live source has lost its connection and is being reconnected.
    pub reconnecting: bool,
}

pub(crate) fn playlist_tabs(state: &PersistedState) -> Vec<PlaylistTab> {
    state
        .playlists()
        .iter()
        .map(|playlist| PlaylistTab {
            id: playlist.id(),
            name: displayable(playlist.name()),
            playing: playlist.id() == state.playing(),
            shuffled: playlist.shuffle().is_some(),
        })
        .collect()
}

/// Every row of one playlist, in queue order; empty for a playlist that is
/// no longer there.
pub(crate) fn queue_rows(state: &PersistedState, playlist: PlaylistId) -> Vec<QueueRow> {
    let Some(playlist) = state.playlist(playlist) else {
        return Vec::new();
    };
    playlist
        .queue()
        .entries()
        .iter()
        .map(|entry| QueueRow {
            id: entry.id(),
            media: entry.media().clone(),
            title: entry_title(entry),
            subtitle: entry_subtitle(entry),
            duration: entry.display().duration,
            saved: saved_history(state.entry_for(entry.media())),
        })
        .collect()
}

/// The entry's display title, or the name its identity implies, escaped.
pub(crate) fn entry_title(entry: &QueueEntry) -> String {
    let title = entry.display().title.as_deref();
    displayable(&match entry.media() {
        MediaId::PodcastEpisode { .. } => episode_name(title),
        media => title
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(|| display_name(media), str::to_owned),
    })
}

/// Artist and album, each escaped, joined when both are known.
fn entry_subtitle(entry: &QueueEntry) -> Option<String> {
    let display = entry.display();
    let parts: Vec<String> = [display.artist.as_deref(), display.album.as_deref()]
        .into_iter()
        .flatten()
        .map(displayable)
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::id::AbsolutePath;
    use crate::playback::checkpoint::PlaybackCheckpoint;
    use crate::queue::{DisplayMetadata, NewQueueEntry, QueueSource};
    use time::OffsetDateTime;

    fn checkpoint(
        position: Option<u64>,
        estimated: Option<u64>,
        completed: bool,
    ) -> PersistedCheckpoint {
        PersistedCheckpoint {
            position: position.map(Duration::from_secs),
            completed,
            touch_seq: 1,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            estimated: estimated.map(Duration::from_secs),
        }
    }

    #[test]
    fn saved_history_labels_follow_completion_then_estimate_then_position() {
        let played = saved_history(Some(&checkpoint(Some(62), Some(70), true)));
        assert_eq!(played, Some(SavedHistory::Played));
        assert_eq!(format_saved(SavedHistory::Played), "played");

        let estimated = saved_history(Some(&checkpoint(Some(10), Some(62), false)));
        assert_eq!(
            estimated,
            Some(SavedHistory::Position {
                at: Duration::from_secs(62),
                estimated: true
            })
        );
        assert_eq!(format_saved(estimated.expect("label")), "~01:02 saved");

        let position = saved_history(Some(&checkpoint(Some(62), None, false)));
        assert_eq!(
            position,
            Some(SavedHistory::Position {
                at: Duration::from_secs(62),
                estimated: false
            })
        );
        assert_eq!(format_saved(position.expect("label")), "01:02 saved");

        let unknown = saved_history(Some(&checkpoint(None, None, false)));
        assert_eq!(unknown, Some(SavedHistory::Unknown));
        assert_eq!(format_saved(SavedHistory::Unknown), "position unknown");

        assert_eq!(saved_history(None), None);
    }

    #[test]
    fn a_title_with_terminal_controls_is_rendered_inert() {
        let path = AbsolutePath::new("/music/evil.flac".into()).expect("absolute");
        let media = MediaId::LocalFile(path.clone());
        let entry = NewQueueEntry::new(
            media.clone(),
            QueueSource::LocalFile(path),
            DisplayMetadata {
                title: Some("evil\u{1b}[2Jtitle".to_owned()),
                artist: Some("bad\u{1b}]0;x\u{7}artist".to_owned()),
                ..DisplayMetadata::default()
            },
        )
        .expect("entry");
        let mut state = PersistedState::default();
        let playing = state.playing();
        state.enqueue(playing, vec![entry]).expect("fits");
        state.record(
            &PlaybackCheckpoint {
                media,
                position: Duration::from_secs(62),
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            false,
        );

        let rows = queue_rows(&state, playing);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].title.contains('\u{1b}'), "{:?}", rows[0].title);
        assert!(rows[0].title.contains("title"));
        let subtitle = rows[0].subtitle.as_deref().expect("artist");
        assert!(!subtitle.contains('\u{1b}') && !subtitle.contains('\u{7}'));
        assert_eq!(
            rows[0].saved,
            Some(SavedHistory::Position {
                at: Duration::from_secs(62),
                estimated: false
            })
        );
    }
}
