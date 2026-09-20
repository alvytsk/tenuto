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

/// The queue row title: `Artist – Title` when both tags are known, else the
/// plain title (M8 §9).
pub(crate) fn entry_title(entry: &QueueEntry) -> String {
    let filled = |text: &str| {
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    };
    match entry.media() {
        MediaId::PodcastEpisode { .. } => entry_plain_title(entry),
        _ => {
            let display = entry.display();
            match (
                display.artist.as_deref().and_then(filled),
                display.title.as_deref().and_then(filled),
            ) {
                (Some(artist), Some(title)) => displayable(&format!("{artist} – {title}")),
                _ => entry_plain_title(entry),
            }
        }
    }
}

/// The entry's display title, or the name its identity implies, escaped.
/// Used where the artist already has its own line (the now-playing pane).
pub(crate) fn entry_plain_title(entry: &QueueEntry) -> String {
    let title = entry.display().title.as_deref();
    displayable(&match entry.media() {
        MediaId::PodcastEpisode { .. } => episode_name(title),
        media => title
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(|| display_name(media), str::to_owned),
    })
}

/// The album, escaped. The artist moved up into the title line (M8 §9).
fn entry_subtitle(entry: &QueueEntry) -> Option<String> {
    entry
        .display()
        .album
        .as_deref()
        .map(str::trim)
        .filter(|album| !album.is_empty())
        .map(displayable)
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
                album: Some("live\u{7}album".to_owned()),
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
        assert!(rows[0].title.contains("title") && rows[0].title.contains("artist"));
        let subtitle = rows[0].subtitle.as_deref().expect("album");
        assert!(!subtitle.contains('\u{7}'));
        assert_eq!(
            rows[0].saved,
            Some(SavedHistory::Position {
                at: Duration::from_secs(62),
                estimated: false
            })
        );
    }

    fn local(title: Option<&str>, artist: Option<&str>, album: Option<&str>) -> QueueEntry {
        let path = AbsolutePath::new("/music/file.flac".into())
            .unwrap_or_else(|error| panic!("absolute: {error}"));
        let new = NewQueueEntry::new(
            MediaId::LocalFile(path.clone()),
            QueueSource::LocalFile(path),
            DisplayMetadata {
                title: title.map(str::to_owned),
                artist: artist.map(str::to_owned),
                album: album.map(str::to_owned),
                ..DisplayMetadata::default()
            },
        )
        .unwrap_or_else(|error| panic!("valid: {error}"));
        let mut queue = crate::queue::Queue::default();
        let ids = queue
            .enqueue(vec![new], &mut crate::queue::IdAllocator::default())
            .unwrap_or_else(|error| panic!("fits: {error}"));
        queue
            .get(ids[0])
            .cloned()
            .unwrap_or_else(|| panic!("just enqueued"))
    }

    #[test]
    fn a_tagged_track_reads_artist_dash_title_with_the_album_below() {
        let entry = local(Some("So What"), Some("Miles Davis"), Some("Kind of Blue"));
        assert_eq!(entry_title(&entry), "Miles Davis – So What");
        assert_eq!(entry_subtitle(&entry).as_deref(), Some("Kind of Blue"));
    }

    #[test]
    fn without_an_artist_or_without_a_title_the_row_is_as_before() {
        assert_eq!(entry_title(&local(Some("So What"), None, None)), "So What");
        assert_eq!(
            entry_title(&local(Some("So What"), Some("  "), None)),
            "So What"
        );
        assert_eq!(
            entry_title(&local(None, Some("Miles Davis"), None)),
            "file.flac",
            "no title: the file name, not 'Artist – file.flac'"
        );
        assert_eq!(
            entry_subtitle(&local(None, Some("Miles Davis"), None)),
            None
        );
    }

    #[test]
    fn control_characters_in_either_tag_never_reach_the_row() {
        let entry = local(Some("So\u{1b}[31m What"), Some("Miles\u{7}"), None);
        let title = entry_title(&entry);
        assert!(!title.chars().any(char::is_control), "{title:?}");
    }
}
