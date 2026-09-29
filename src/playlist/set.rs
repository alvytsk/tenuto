//! The playlist set (M9.1): every playlist, which one is playing, both ID
//! allocators, and the rules that bind them (spec §3, S1–S8). Nothing
//! outside `crate::playlist` can change a `Queue`, a `Playlist` or a
//! `QueueEntry`; `PersistedState` owns one set and forwards to it.

use std::ops::Deref;

use url::Url;

use super::queue::{
    Direction, DisplayUpdate, IdAllocator, MAX_PLAYLIST_ENTRIES, NewQueueEntry, QueueEntry,
    QueueEntryId, QueueError, QueueSource, Removed,
};
use super::{
    DEFAULT_NAME, MAX_PLAYLISTS, Playlist, PlaylistError, PlaylistId, Shuffle, clean_name,
    splitmix64,
};
use crate::media::id::MediaId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistSet {
    /// Never empty (S1).
    playlists: Vec<Playlist>,
    /// Always names a member (S4).
    playing: PlaylistId,
    /// The one allocator for every entry ID (S2).
    entry_ids: IdAllocator,
    /// The one allocator for every playlist ID (S2).
    playlist_ids: IdAllocator,
}

impl Default for PlaylistSet {
    /// One empty playlist named `Default`, ID 1, playing.
    fn default() -> Self {
        let playing = PlaylistId::from_raw(1);
        Self {
            playlists: vec![Playlist::new(playing, DEFAULT_NAME.to_owned())],
            playing,
            entry_ids: IdAllocator::default(),
            playlist_ids: IdAllocator::starting_at(Some(2)),
        }
    }
}

/// Read-only slice access: `len()` counts playlists; `iter()` and indexing
/// read them in order. There is no `DerefMut`.
impl Deref for PlaylistSet {
    type Target = [Playlist];

    fn deref(&self) -> &[Playlist] {
        &self.playlists
    }
}

/// What an operation did to the persisted current media, which the set
/// does not hold: `PersistedState` applies it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaEffect {
    Unchanged,
    Set(Option<MediaId>),
}

impl MediaEffect {
    pub fn apply(self, current: Option<MediaId>) -> Option<MediaId> {
        match self {
            Self::Unchanged => current,
            Self::Set(media) => media,
        }
    }
}

/// A successful delete: the playlist that took the deleted one's place
/// (S7), and what that did to the persisted current media.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deletion {
    pub successor: PlaylistId,
    pub current_media: MediaEffect,
}

/// The media under a playlist's cursor.
fn cursor_media(playlist: &Playlist) -> Option<MediaId> {
    let queue = playlist.queue();
    queue
        .active()
        .and_then(|id| queue.get(id))
        .map(|entry| entry.media().clone())
}

impl PlaylistSet {
    // ------------------------------------------------------------ reads

    pub fn playing(&self) -> PlaylistId {
        self.playing
    }

    fn index_of(&self, id: PlaylistId) -> Option<usize> {
        self.playlists
            .iter()
            .position(|playlist| playlist.id() == id)
    }

    /// `playing` always names a member; index 0 keeps this total without
    /// an `unwrap`.
    fn playing_index(&self) -> usize {
        self.index_of(self.playing).unwrap_or(0)
    }

    pub fn playlist(&self, id: PlaylistId) -> Option<&Playlist> {
        self.index_of(id).map(|index| &self.playlists[index])
    }

    pub fn playing_playlist(&self) -> &Playlist {
        &self.playlists[self.playing_index()]
    }

    pub fn owner_of(&self, entry: QueueEntryId) -> Option<PlaylistId> {
        self.playlists
            .iter()
            .find(|playlist| playlist.queue().get(entry).is_some())
            .map(Playlist::id)
    }

    pub fn find_entry(&self, entry: QueueEntryId) -> Option<&QueueEntry> {
        self.playlists
            .iter()
            .find_map(|playlist| playlist.queue().get(entry))
    }

    pub fn total_entries(&self) -> usize {
        self.playlists
            .iter()
            .map(|playlist| playlist.queue().len())
            .sum()
    }

    /// Whether `delete(id)` would succeed: `Unknown` first, then
    /// `LastPlaylist` (P1). Lets `Session` refuse before it releases
    /// anything.
    pub fn check_delete(&self, id: PlaylistId) -> Result<(), PlaylistError> {
        self.index_of(id).ok_or(PlaylistError::Unknown(id))?;
        if self.playlists.len() == 1 {
            return Err(PlaylistError::LastPlaylist);
        }
        Ok(())
    }

    /// For serialization: `None` is an exhausted namespace.
    #[expect(
        dead_code,
        reason = "Task 4's serializer is the first caller; drop this then"
    )]
    pub(crate) fn next_entry_id(&self) -> Option<u64> {
        self.entry_ids.next()
    }

    #[expect(
        dead_code,
        reason = "Task 4's serializer is the first caller; drop this then"
    )]
    pub(crate) fn next_playlist_id(&self) -> Option<u64> {
        self.playlist_ids.next()
    }

    fn playlist_mut(&mut self, id: PlaylistId) -> Option<&mut Playlist> {
        let index = self.index_of(id)?;
        self.playlists.get_mut(index)
    }

    fn entry_mut(&mut self, entry: QueueEntryId) -> Option<&mut QueueEntry> {
        let owner = self.owner_of(entry)?;
        self.playlist_mut(owner)?.queue.get_mut(entry)
    }

    // -------------------------------------------------------- mutations

    /// All or nothing, against the global cap (P7), with IDs from the one
    /// allocator (P2). A non-empty batch into a shuffled playlist reshuffles
    /// it in the same step: a seed derived from the old one, its cursor
    /// pinned first, so every added track lies ahead of the playing one. An
    /// empty batch changes nothing.
    pub fn enqueue(
        &mut self,
        dest: PlaylistId,
        batch: Vec<NewQueueEntry>,
    ) -> Result<Vec<QueueEntryId>, QueueError> {
        let index = self
            .index_of(dest)
            .ok_or(QueueError::UnknownPlaylist(dest.get()))?;
        let available = MAX_PLAYLIST_ENTRIES.saturating_sub(self.total_entries());
        if batch.len() > available {
            return Err(QueueError::Capacity {
                requested: batch.len(),
                available,
            });
        }
        let playlist = &mut self.playlists[index];
        let ids = playlist.queue.enqueue(batch, &mut self.entry_ids)?;
        if !ids.is_empty()
            && let Some(shuffle) = playlist.shuffle
        {
            playlist.shuffle = Some(Shuffle {
                seed: splitmix64(shuffle.seed),
                first: playlist.queue.active(),
            });
        }
        Ok(ids)
    }

    pub fn create(&mut self, name: &str) -> Result<PlaylistId, PlaylistError> {
        let name = clean_name(name).ok_or(PlaylistError::InvalidName)?;
        if self.playlists.len() >= MAX_PLAYLISTS {
            return Err(PlaylistError::TooMany);
        }
        let raw = self
            .playlist_ids
            .reserve(1)
            .map_err(|_| PlaylistError::IdExhausted)?;
        let id = PlaylistId::from_raw(*raw.start());
        self.playlists.push(Playlist::new(id, name));
        Ok(id)
    }

    pub fn rename(&mut self, id: PlaylistId, name: &str) -> Result<(), PlaylistError> {
        let name = clean_name(name).ok_or(PlaylistError::InvalidName)?;
        self.playlist_mut(id)
            .ok_or(PlaylistError::Unknown(id))?
            .name = name;
        Ok(())
    }

    /// `Some(seed)` turns shuffle on with the playlist's *own* cursor pinned
    /// first (S6); `None` turns it off.
    pub fn set_shuffle(&mut self, id: PlaylistId, seed: Option<u64>) -> Result<(), PlaylistError> {
        let playlist = self.playlist_mut(id).ok_or(PlaylistError::Unknown(id))?;
        let first = playlist.queue.active();
        playlist.shuffle = seed.map(|seed| Shuffle { seed, first });
        Ok(())
    }

    /// Whether the order actually changed; an edge is a no-op.
    pub fn move_entry(
        &mut self,
        id: QueueEntryId,
        direction: Direction,
    ) -> Result<bool, QueueError> {
        let owner = self.owner_of(id).ok_or(QueueError::UnknownEntry(id))?;
        self.playlist_mut(owner)
            .ok_or(QueueError::UnknownEntry(id))?
            .queue
            .move_entry(id, direction)
    }

    /// Removes `id` from whichever playlist holds it, clearing that
    /// playlist's cursor when it was the cursor.
    pub fn remove_entry(&mut self, id: QueueEntryId) -> Result<Removed, QueueError> {
        let owner = self.owner_of(id).ok_or(QueueError::UnknownEntry(id))?;
        self.playlist_mut(owner)
            .ok_or(QueueError::UnknownEntry(id))?
            .queue
            .remove(id)
    }

    pub fn clear(&mut self, id: PlaylistId) -> Result<(), PlaylistError> {
        self.playlist_mut(id)
            .ok_or(PlaylistError::Unknown(id))?
            .queue
            .clear();
        Ok(())
    }

    /// Removes `id` after the same check as [`check_delete`](Self::check_delete).
    /// Deleting the playing playlist moves `playing` to the successor and
    /// reports that playlist's cursor media (or `None`) as the new persisted
    /// current media; any other delete leaves it `Unchanged`.
    pub fn delete(&mut self, id: PlaylistId) -> Result<Deletion, PlaylistError> {
        self.check_delete(id)?;
        let index = self.index_of(id).ok_or(PlaylistError::Unknown(id))?;
        self.playlists.remove(index);
        let next = index.min(self.playlists.len().saturating_sub(1));
        let successor = self.playlists[next].id();
        let current_media = if self.playing == id {
            self.playing = successor;
            MediaEffect::Set(cursor_media(&self.playlists[next]))
        } else {
            MediaEffect::Unchanged
        };
        Ok(Deletion {
            successor,
            current_media,
        })
    }

    /// Makes `entry`'s owner playing and sets that playlist's cursor to it,
    /// in one step. Validates first: on failure nothing changes.
    pub fn adopt(&mut self, entry: QueueEntryId) -> Result<(), QueueError> {
        let owner = self
            .owner_of(entry)
            .ok_or(QueueError::UnknownEntry(entry))?;
        self.playlist_mut(owner)
            .ok_or(QueueError::UnknownEntry(entry))?
            .queue
            .set_active(Some(entry))?;
        self.playing = owner;
        Ok(())
    }

    /// The `LoadTarget::Legacy` path: a load that belongs to no playlist
    /// clears the playing playlist's cursor, as it did before M8.
    pub fn clear_playing_cursor(&mut self) {
        let index = self.playing_index();
        if let Some(playlist) = self.playlists.get_mut(index) {
            playlist.queue.clear_active();
        }
    }

    /// One entry's display: each present field that differs is written, an
    /// absent one is kept. Whether anything changed.
    pub fn update_display(
        &mut self,
        entry: QueueEntryId,
        update: &DisplayUpdate,
    ) -> Result<bool, QueueError> {
        let found = self
            .entry_mut(entry)
            .ok_or(QueueError::UnknownEntry(entry))?;
        Ok(update.apply_to(found.display_mut()))
    }

    /// `Ok(false)` when the URL already matches; `SourceMismatch` for an
    /// entry that is not a podcast episode.
    pub fn set_podcast_fallback(
        &mut self,
        entry: QueueEntryId,
        url: Url,
    ) -> Result<bool, QueueError> {
        let found = self
            .entry_mut(entry)
            .ok_or(QueueError::UnknownEntry(entry))?;
        if let QueueSource::Podcast { fallback } = found.source()
            && *fallback == url
        {
            return Ok(false);
        }
        found.set_source(QueueSource::Podcast { fallback: url })?;
        Ok(true)
    }
}
