//! The playlist set (M9.1): every playlist, which one is playing, both ID
//! allocators, and the rules that bind them (spec §3, S1–S8). Nothing
//! outside `crate::playlist` can change a `Queue`, a `Playlist` or a
//! `QueueEntry`; `PersistedState` owns one set and forwards to it.
//!
//! Nothing outside this module can build or change a queue:
//!
//! ```compile_fail
//! use tenuto::queue::{IdAllocator, Queue};
//! let mut queue = Queue::default();
//! let _ = queue.enqueue(Vec::new(), &mut IdAllocator::default());
//! ```
//!
//! nor build a playlist from unchecked parts:
//!
//! ```compile_fail
//! use tenuto::playlist::{Playlist, PlaylistId};
//! use tenuto::queue::Queue;
//! let _ = Playlist::from_parts(PlaylistId::from_raw_for_tests(1), "P".into(), None, Queue::default());
//! ```
//!
//! and the state file's envelope never lends its set out:
//!
//! ```compile_fail
//! let mut state = tenuto::persistence::model::PersistedState::default();
//! let playing = state.playlists().playing();
//! let _ = state.enqueue(playing, Vec::new());
//! ```

use std::collections::BTreeSet;
use std::ops::Deref;
use std::sync::Arc;

use url::Url;

use super::queue::{
    Direction, DisplayUpdate, IdAllocator, MAX_PLAYLIST_ENTRIES, NewQueueEntry, Queue, QueueEntry,
    QueueEntryId, QueueError, QueueSource, Removed,
};
use super::{
    DEFAULT_NAME, MAX_PLAYLISTS, Playlist, PlaylistError, PlaylistId, Shuffle, clean_name,
    splitmix64,
};
use crate::media::id::MediaId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistSet {
    /// Never empty (S1). Each slot is `Arc`-shared copy-on-write against
    /// the writer's retained snapshots: editing one playlist clones that
    /// one slot (`Arc::make_mut`) and every other slot keeps pointing at
    /// what it held.
    playlists: Vec<Arc<Playlist>>,
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
            playlists: vec![Arc::new(Playlist::new(playing, DEFAULT_NAME.to_owned()))],
            playing,
            entry_ids: IdAllocator::default(),
            playlist_ids: IdAllocator::starting_at(Some(2)),
        }
    }
}

/// Read-only slice access: `len()` counts playlists; `iter()` and indexing
/// read them in order, through each slot's `Arc`. There is no `DerefMut`.
impl Deref for PlaylistSet {
    type Target = [Arc<Playlist>];

    fn deref(&self) -> &[Arc<Playlist>] {
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
        self.index_of(id)
            .map(|index| self.playlists[index].as_ref())
    }

    pub fn playing_playlist(&self) -> &Playlist {
        &self.playlists[self.playing_index()]
    }

    pub fn owner_of(&self, entry: QueueEntryId) -> Option<PlaylistId> {
        self.playlists
            .iter()
            .find(|playlist| playlist.queue().get(entry).is_some())
            .map(|playlist| playlist.id())
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
    pub(crate) fn next_entry_id(&self) -> Option<u64> {
        self.entry_ids.next()
    }

    pub(crate) fn next_playlist_id(&self) -> Option<u64> {
        self.playlist_ids.next()
    }

    fn playlist_mut(&mut self, id: PlaylistId) -> Option<&mut Playlist> {
        let index = self.index_of(id)?;
        Some(Arc::make_mut(&mut self.playlists[index]))
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
        let playlist = Arc::make_mut(&mut self.playlists[index]);
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
        self.playlists.push(Arc::new(Playlist::new(id, name)));
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

    /// Whether the order actually changed; an edge is a no-op. The edge
    /// test runs before the writable borrow is taken, so a held arrow key
    /// at the top or bottom of a full playlist never clones it per
    /// keypress.
    pub fn move_entry(
        &mut self,
        id: QueueEntryId,
        direction: Direction,
    ) -> Result<bool, QueueError> {
        let owner = self.owner_of(id).ok_or(QueueError::UnknownEntry(id))?;
        let playlist = self.playlist(owner).ok_or(QueueError::UnknownEntry(id))?;
        if !playlist.queue().move_would_change(id, direction) {
            return Ok(false);
        }
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
    ///
    /// `Vec::remove` moves the slot's `Arc` out; there is no `Arc::make_mut`
    /// here, so deleting never clones the playlist it deletes nor any
    /// survivor.
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

    /// One entry's display: each present field that differs is written, an
    /// absent one is kept. Whether anything changed. The read-only probe
    /// runs before the writable borrow, so a matching resend never clones
    /// the entry's playlist.
    pub fn update_display(
        &mut self,
        entry: QueueEntryId,
        update: &DisplayUpdate,
    ) -> Result<bool, QueueError> {
        let found = self
            .find_entry(entry)
            .ok_or(QueueError::UnknownEntry(entry))?;
        if !update.would_change(found.display()) {
            return Ok(false);
        }
        let found = self
            .entry_mut(entry)
            .ok_or(QueueError::UnknownEntry(entry))?;
        Ok(update.apply_to(found.display_mut()))
    }

    /// `Ok(false)` when the URL already matches; `SourceMismatch` for an
    /// entry that is not a podcast episode. The equality check runs before
    /// the writable borrow, so a matching resend never clones.
    pub fn set_podcast_fallback(
        &mut self,
        entry: QueueEntryId,
        url: Url,
    ) -> Result<bool, QueueError> {
        let found = self
            .find_entry(entry)
            .ok_or(QueueError::UnknownEntry(entry))?;
        if let QueueSource::Podcast { fallback } = found.source()
            && *fallback == url
        {
            return Ok(false);
        }
        let found = self
            .entry_mut(entry)
            .ok_or(QueueError::UnknownEntry(entry))?;
        found.set_source(QueueSource::Podcast { fallback: url })?;
        Ok(true)
    }
}

// ------------------------------------------------------------ recovery

/// A field of a stored record: missing (or null), present but not what it
/// should be, or a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field<T> {
    Absent,
    Malformed,
    Value(T),
}

/// A stored shuffle: missing, a seed that is not a number (shuffle off), or
/// a seed with its `first` pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShuffleField {
    Absent,
    BadSeed,
    Seeded { seed: u64, first: Field<u64> },
}

/// One stored playlist, parsed but not yet trusted. The caller (the codec)
/// has checked each entry's source against its media, through
/// `NewQueueEntry::new`; the builder checks everything that spans records.
#[derive(Clone, Debug)]
pub struct RecordParts {
    /// `None` when absent or not a number.
    pub id: Option<u64>,
    /// In list order, each with its stored ID.
    pub entries: Vec<(u64, NewQueueEntry)>,
    pub cursor: Field<u64>,
    pub shuffle: ShuffleField,
    /// As stored; the builder cleans it.
    pub name: Option<String>,
}

/// Where in a record's recovery a repair was made (spec §8.3). Ordered as
/// the stages run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Stage {
    Count,
    Id,
    Entries,
    Cursor,
    Shuffle,
}

/// A repair recovery made. Domain-level: the codec maps each to its own
/// diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Repair {
    TooManyPlaylists,
    DuplicatePlaylistId,
    IdsExhausted,
    OverCapacity,
    DuplicateEntryId,
    DanglingCursor,
    Shuffle,
    DanglingPlaying,
    CursorMediaMismatch,
}

/// One record's result: the ID it was kept under, or `None` when it was
/// skipped, and its repairs in the order they were made.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordOutcome {
    pub kept: Option<PlaylistId>,
    pub repairs: Vec<(Stage, Repair)>,
}

#[derive(Clone, Debug)]
pub struct RecoveredSet {
    pub set: PlaylistSet,
    pub current_media: MediaEffect,
    pub repairs: Vec<Repair>,
}

/// Builds a set from stored records (spec §8.3). Records must be pushed in
/// file order: that order decides which duplicate keeps its ID and which
/// entries survive truncation. Whatever counters it is given, no ID it keeps
/// or mints is ever handed out again (S2). `finish` is the only way out.
#[derive(Debug)]
pub struct Recovery {
    playlists: Vec<Playlist>,
    entry_ids: IdAllocator,
    playlist_ids: IdAllocator,
    seen_playlists: BTreeSet<u64>,
    seen_entries: BTreeSet<u64>,
    /// Every `push_record` call, skipped records included: the playlist cap
    /// is on input records.
    records: usize,
    /// Entries kept so far, against the global cap.
    total: usize,
}

fn fresh(ids: &mut IdAllocator) -> Option<u64> {
    ids.reserve(1).ok().map(|range| *range.start())
}

impl PlaylistSet {
    /// The counters as stored. The codec raises them past every ID in the
    /// raw file first, so that the minted IDs match 0.2.0's; safety does not
    /// depend on it.
    pub fn recovery(entry_ids: IdAllocator, playlist_ids: IdAllocator) -> Recovery {
        Recovery {
            playlists: Vec::new(),
            entry_ids,
            playlist_ids,
            seen_playlists: BTreeSet::new(),
            seen_entries: BTreeSet::new(),
            records: 0,
            total: 0,
        }
    }
}

impl Recovery {
    /// Stages in a fixed order: Count, Id, Entries, Cursor, Shuffle, Name.
    /// A record skipped at Count or Id runs no later stage.
    pub fn push_record(&mut self, parts: RecordParts) -> RecordOutcome {
        // Count: the cap is on input records (M8 §6 rule 6).
        self.records += 1;
        if self.records > MAX_PLAYLISTS {
            return RecordOutcome {
                kept: None,
                repairs: vec![(Stage::Count, Repair::TooManyPlaylists)],
            };
        }
        let mut repairs = Vec::new();

        // Id (rule 3).
        let raw = match parts.id.filter(|id| self.seen_playlists.insert(*id)) {
            Some(id) => {
                self.playlist_ids.observe(id);
                id
            }
            None => match fresh(&mut self.playlist_ids) {
                Some(id) => {
                    repairs.push((Stage::Id, Repair::DuplicatePlaylistId));
                    self.seen_playlists.insert(id);
                    id
                }
                None => {
                    return RecordOutcome {
                        kept: None,
                        repairs: vec![(Stage::Id, Repair::IdsExhausted)],
                    };
                }
            },
        };

        // Entries (rules 4 and 6): the cap before duplicate reassignment,
        // entry by entry, so a dropped entry never costs a fresh ID.
        let mut cursor = match parts.cursor {
            Field::Value(id) => Some(id),
            Field::Absent | Field::Malformed => None,
        };
        let mut entries = Vec::with_capacity(parts.entries.len());
        let mut over_capacity = false;
        for (stored, new) in parts.entries {
            if self.total >= MAX_PLAYLIST_ENTRIES {
                if !over_capacity {
                    repairs.push((Stage::Entries, Repair::OverCapacity));
                    over_capacity = true;
                }
                if cursor == Some(stored) {
                    cursor = None;
                }
                continue;
            }
            let id = if self.seen_entries.insert(stored) {
                self.entry_ids.observe(stored);
                stored
            } else {
                if cursor == Some(stored) {
                    cursor = None;
                }
                match fresh(&mut self.entry_ids) {
                    Some(id) => {
                        repairs.push((Stage::Entries, Repair::DuplicateEntryId));
                        self.seen_entries.insert(id);
                        id
                    }
                    None => {
                        repairs.push((Stage::Entries, Repair::IdsExhausted));
                        continue;
                    }
                }
            };
            self.total += 1;
            entries.push(QueueEntry::from_new(QueueEntryId::from_raw(id), new));
        }
        let is_member = |id: u64| entries.iter().any(|entry| entry.id().get() == id);

        // Cursor (rule 9, membership half): a present field that does not
        // resolve to a member is dangling, including one Entries cleared.
        let active = match parts.cursor {
            Field::Absent => None,
            Field::Malformed | Field::Value(_) => match cursor.filter(|id| is_member(*id)) {
                Some(id) => Some(QueueEntryId::from_raw(id)),
                None => {
                    repairs.push((Stage::Cursor, Repair::DanglingCursor));
                    None
                }
            },
        };

        // Shuffle (rule 10): a bad seed turns it off; a malformed `first`
        // keeps it and clears the pin; a foreign `first` clears silently.
        let shuffle = match parts.shuffle {
            ShuffleField::Absent => None,
            ShuffleField::BadSeed => {
                repairs.push((Stage::Shuffle, Repair::Shuffle));
                None
            }
            ShuffleField::Seeded { seed, first } => {
                let first = match first {
                    Field::Absent => None,
                    Field::Malformed => {
                        repairs.push((Stage::Shuffle, Repair::Shuffle));
                        None
                    }
                    Field::Value(id) => is_member(id).then_some(QueueEntryId::from_raw(id)),
                };
                Some(Shuffle { seed, first })
            }
        };

        // Name (rule 5): silent.
        let name = parts
            .name
            .as_deref()
            .and_then(clean_name)
            .unwrap_or_else(|| format!("Playlist {raw}"));

        let id = PlaylistId::from_raw(raw);
        self.playlists.push(Playlist::from_parts(
            id,
            name,
            shuffle,
            Queue::from_parts(entries, active),
        ));
        RecordOutcome {
            kept: Some(id),
            repairs,
        }
    }

    /// Rules 7, 8 and 9's media half, then the set. Rules 8 and 9 never both
    /// apply: a re-pointed `playing` re-points the media to match.
    pub fn finish(mut self, playing: Option<u64>, current_media: Option<MediaId>) -> RecoveredSet {
        let mut repairs = Vec::new();
        let Some(first) = self.playlists.first().map(Playlist::id) else {
            // Rule 7. With the counter exhausted, 1: P1 needs a playlist, and
            // nothing was kept that could hold that ID.
            let id = PlaylistId::from_raw(fresh(&mut self.playlist_ids).unwrap_or(1));
            return RecoveredSet {
                set: PlaylistSet {
                    playlists: vec![Arc::new(Playlist::new(id, DEFAULT_NAME.to_owned()))],
                    playing: id,
                    entry_ids: self.entry_ids,
                    playlist_ids: self.playlist_ids,
                },
                current_media: MediaEffect::Unchanged,
                repairs,
            };
        };
        let named = playing
            .map(PlaylistId::from_raw)
            .filter(|id| self.playlists.iter().any(|p| p.id() == *id));
        let (playing, effect) = match named {
            Some(playing) => {
                if let Some(playlist) = self.playlists.iter_mut().find(|p| p.id() == playing) {
                    let cursor = cursor_media(playlist);
                    if cursor.is_some() && cursor != current_media {
                        repairs.push(Repair::CursorMediaMismatch);
                        playlist.queue.clear_active();
                    }
                }
                (playing, MediaEffect::Unchanged)
            }
            None => {
                repairs.push(Repair::DanglingPlaying);
                (
                    first,
                    MediaEffect::Set(self.playlists.first().and_then(cursor_media)),
                )
            }
        };
        RecoveredSet {
            set: PlaylistSet {
                playlists: self.playlists.into_iter().map(Arc::new).collect(),
                playing,
                entry_ids: self.entry_ids,
                playlist_ids: self.playlist_ids,
            },
            current_media: effect,
            repairs,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::media::id::{AbsolutePath, EpisodeKey, FeedId};

    /// A local-file entry, shaped like `model.rs`'s `playlist_tests` helper.
    fn entry(name: &str) -> NewQueueEntry {
        let path = AbsolutePath::new(format!("/music/{name}.flac").into()).expect("absolute");
        NewQueueEntry::new(
            MediaId::LocalFile(path.clone()),
            QueueSource::LocalFile(path),
            crate::queue::DisplayMetadata::default(),
        )
        .expect("valid")
    }

    /// A podcast episode whose enclosure is `fallback`, for the fallback
    /// paths that need a real URL to compare against.
    fn podcast_entry(fallback: Url) -> NewQueueEntry {
        let feed = FeedId::new("m12".into()).expect("feed id");
        let episode = EpisodeKey::resolve(Some("guid-m12"), None, None).expect("episode key");
        NewQueueEntry::new(
            MediaId::PodcastEpisode { feed, episode },
            QueueSource::Podcast { fallback },
            crate::queue::DisplayMetadata::default(),
        )
        .expect("valid")
    }

    /// Indices whose slots stopped sharing between `before` and `after`:
    /// exactly the playlists a mutation cloned.
    fn diverged(before: &PlaylistSet, after: &PlaylistSet) -> Vec<usize> {
        before
            .playlists
            .iter()
            .zip(&after.playlists)
            .enumerate()
            .filter_map(|(index, (old, new))| (!Arc::ptr_eq(old, new)).then_some(index))
            .collect()
    }

    /// Where `id` sits in `set`.
    fn slot(set: &PlaylistSet, id: PlaylistId) -> usize {
        set.index_of(id).expect("playlist is present")
    }

    #[test]
    fn a_clone_shares_every_playlist() {
        let mut set = PlaylistSet::default();
        let p1 = set.playing();
        let p2 = set.create("Two").expect("room");
        set.enqueue(p1, vec![entry("a")]).expect("fits");
        set.enqueue(p2, vec![entry("b"), entry("c")]).expect("fits");

        let clone = set.clone();

        assert!(
            diverged(&set, &clone).is_empty(),
            "a clone shares every playlist"
        );
    }

    #[test]
    fn renaming_one_playlist_clones_only_that_playlist() {
        let mut set = PlaylistSet::default();
        let p1 = set.playing();
        let p2 = set.create("Two").expect("room");
        set.enqueue(p2, vec![entry("b")]).expect("fits");
        let clone = set.clone();

        set.rename(p1, "Renamed").expect("valid name");

        assert_eq!(
            diverged(&set, &clone),
            vec![slot(&set, p1)],
            "only the renamed playlist is cloned"
        );
    }

    #[test]
    fn enqueuing_clones_only_the_destination() {
        let mut set = PlaylistSet::default();
        let p1 = set.playing();
        let p2 = set.create("Two").expect("room");
        set.enqueue(p1, vec![entry("a")]).expect("fits");
        let clone = set.clone();

        set.enqueue(p2, vec![entry("b")]).expect("fits");

        assert_eq!(
            diverged(&set, &clone),
            vec![slot(&set, p2)],
            "only the destination playlist is cloned"
        );
    }

    #[test]
    fn a_move_at_an_edge_clones_nothing() {
        let mut set = PlaylistSet::default();
        let only = set.enqueue(set.playing(), vec![entry("a")]).expect("fits")[0];
        let clone = set.clone();

        assert_eq!(set.move_entry(only, Direction::Up), Ok(false));
        assert!(
            diverged(&set, &clone).is_empty(),
            "an Up move at the top edge clones nothing"
        );

        // The bottom edge on a one-entry set behaves the same way.
        let mut solo = PlaylistSet::default();
        let id = solo
            .enqueue(solo.playing(), vec![entry("a")])
            .expect("fits")[0];
        let solo_clone = solo.clone();
        assert_eq!(solo.move_entry(id, Direction::Down), Ok(false));
        assert!(
            diverged(&solo, &solo_clone).is_empty(),
            "a Down move at the bottom edge clones nothing"
        );
    }

    #[test]
    fn a_real_move_clones_the_owner_and_shares_the_rest() {
        let mut set = PlaylistSet::default();
        let p1 = set.playing();
        let p2 = set.create("Two").expect("room");
        let in_p1 = set.enqueue(p1, vec![entry("a"), entry("b")]).expect("fits");
        set.enqueue(p2, vec![entry("c")]).expect("fits");
        let clone = set.clone();

        assert_eq!(set.move_entry(in_p1[1], Direction::Up), Ok(true));
        assert_eq!(
            diverged(&set, &clone),
            vec![slot(&set, p1)],
            "only the owner of the moved entry is cloned"
        );

        // Value-level: the held snapshot still iterates in the old order,
        // the live set in the moved one.
        let order_of = |set: &PlaylistSet| -> Vec<u64> {
            set.playlist(p1)
                .expect("present")
                .queue()
                .entries()
                .iter()
                .map(|entry| entry.id().get())
                .collect()
        };
        assert_eq!(order_of(&clone), [in_p1[0].get(), in_p1[1].get()]);
        assert_eq!(order_of(&set), [in_p1[1].get(), in_p1[0].get()]);
    }

    #[test]
    fn a_matching_metadata_resend_clones_nothing() {
        let mut set = PlaylistSet::default();
        let id = set.enqueue(set.playing(), vec![entry("a")]).expect("fits")[0];
        let update = DisplayUpdate {
            title: Some("Titled".into()),
            ..DisplayUpdate::default()
        };
        assert_eq!(set.update_display(id, &update), Ok(true));
        let clone = set.clone();

        assert_eq!(set.update_display(id, &update), Ok(false));
        assert!(
            diverged(&set, &clone).is_empty(),
            "a matching resend clones nothing"
        );
    }

    #[test]
    fn a_matching_fallback_url_clones_nothing() {
        let mut set = PlaylistSet::default();
        let stored: Url = "https://podcasts.example.org/original.mp3"
            .parse()
            .expect("url");
        let set_to: Url = "https://podcasts.example.org/resolved.mp3"
            .parse()
            .expect("url");
        let id = set
            .enqueue(set.playing(), vec![podcast_entry(stored)])
            .expect("fits")[0];

        assert_eq!(set.set_podcast_fallback(id, set_to.clone()), Ok(true));
        let clone = set.clone();
        assert_eq!(set.set_podcast_fallback(id, set_to), Ok(false));
        assert!(
            diverged(&set, &clone).is_empty(),
            "a matching fallback URL clones nothing"
        );
    }

    #[test]
    fn a_non_podcast_entry_still_reports_source_mismatch() {
        let mut set = PlaylistSet::default();
        let id = set.enqueue(set.playing(), vec![entry("a")]).expect("fits")[0];
        let url: Url = "https://podcasts.example.org/resolved.mp3"
            .parse()
            .expect("url");

        assert_eq!(
            set.set_podcast_fallback(id, url),
            Err(QueueError::SourceMismatch)
        );
    }

    #[test]
    fn deletion_moves_arcs_without_make_mut() {
        let mut set = PlaylistSet::default();
        let p2 = set.create("Two").expect("room");
        set.enqueue(p2, vec![entry("b")]).expect("fits");
        let clone = set.clone();

        set.delete(p2).expect("another playlist exists");

        assert_eq!(set.playlists.len(), 1, "the deleted playlist is gone");
        assert!(
            Arc::ptr_eq(&set.playlists[0], &clone.playlists[0]),
            "the survivor's slot moved out of the vec, never cloned"
        );
        assert_eq!(
            clone.playlists.len(),
            2,
            "the retained snapshot still holds both playlists"
        );
    }
}
