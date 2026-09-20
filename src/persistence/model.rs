//! The state file's shape: one checkpoint per media identity (D2), plus the
//! session-wide facts the file carries.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::queue_codec::{self, QueueReset};
use crate::media::id::MediaId;
use crate::playback::checkpoint::PlaybackCheckpoint;
use crate::playback::volume::Volume;
use crate::playlist::{MAX_PLAYLISTS, Playlist, PlaylistError, PlaylistId, clean_name};
use crate::queue::{
    IdAllocator, MAX_PLAYLIST_ENTRIES, NewQueueEntry, Queue, QueueEntry, QueueEntryId, QueueError,
};

pub const SCHEMA_VERSION: u32 = 4;

/// Counting the current entry, which is never evictable (D2).
pub const MAX_ENTRIES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PersistedCheckpoint {
    /// The established position — decoder-confirmed, unchanged in meaning
    /// from M2. `None` when nothing has ever established one for this
    /// media: an entry that exists only to carry `estimated` (design doc
    /// §4.1, §4.2). Distinct from `Some(ZERO)`, which means established at
    /// the start; `decide_resume` must not conflate the two, since
    /// `ResumeDecision::AtStart` already means the latter.
    ///
    /// A v1 file's present value deserialises straight to `Some` (the store
    /// migrates the envelope's version; see `store::StateStore::load`), so
    /// the schema bump costs the migration nothing here.
    pub position: Option<Duration>,
    pub completed: bool,
    pub touch_seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Where an estimated seek left the listener, when one did (§4.1).
    /// Never a substitute for `position`: it records a location the engine
    /// believes but has not confirmed. Absent rather than `null` when
    /// unset, so a v2 file with no estimate stays comparable to what M2
    /// wrote.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub estimated: Option<Duration>,
}

/// Every field is private, so the paths that can write a stored position are an
/// enumeration the compiler keeps rather than one a reader has to trust: a
/// position reaches the map through [`PersistedState::record`] and nowhere else.
/// The accessors below are the whole surface.
///
/// `next_seq` is derived, never stored (§10). Deserialization goes through
/// `RawState` so that deriving it is the only way to build one from a file:
/// a `#[serde(skip)]` field would arrive as `0` and hand every caller a
/// sequence that regresses.
#[derive(Clone, Debug)]
pub struct PersistedState {
    schema_version: u32,
    current_media: Option<MediaId>,
    volume: f32,
    checkpoints: BTreeMap<MediaId, PersistedCheckpoint>,
    /// Never empty (M8 P1).
    playlists: Vec<Playlist>,
    /// Always names a member of `playlists`.
    playing: PlaylistId,
    next_seq: u64,
    /// The one allocator for every queue entry ID (M8 §4): outside `Queue`
    /// so every playlist shares it. Written as `next_entry_id` (a number, or
    /// `null` once exhausted) and read back raised past every ID the file
    /// holds, so an ID is never handed out twice across a restart.
    entry_ids: IdAllocator,
    /// The one allocator for every playlist ID (M8 §4), persisted as
    /// `next_playlist_id` under the same rule.
    playlist_ids: IdAllocator,
}

/// The state file's shape before the queue is validated: the playlist fields
/// (and schema 3's `queue`/`active_entry`, kept for migration) are held as
/// raw JSON so damaged queue data can be recovered field-by-field without
/// ever touching `checkpoints` (M5 §6, M8 §6).
#[derive(Deserialize)]
pub(super) struct RawState {
    schema_version: u32,
    #[serde(default)]
    current_media: Option<MediaId>,
    #[serde(default = "full_gain")]
    volume: f32,
    #[serde(default)]
    checkpoints: BTreeMap<MediaId, PersistedCheckpoint>,
    #[serde(default)]
    queue: Option<serde_json::Value>,
    #[serde(default)]
    active_entry: Option<serde_json::Value>,
    #[serde(default)]
    playlists: Option<serde_json::Value>,
    #[serde(default)]
    playing: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present")]
    next_entry_id: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present")]
    next_playlist_id: Option<serde_json::Value>,
}

/// Keeps `null` apart from an absent key: `Option<Value>` alone folds both
/// into `None`, and a `null` counter means "exhausted" (M8 §4).
fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

fn full_gain() -> f32 {
    Volume::FULL.as_gain()
}

impl RawState {
    /// Three shapes reach here: a schema 1/2 file (and any read-only
    /// snapshot, which never examines queue data) starts from an empty
    /// `Default` playlist; a schema 3 file recovers its one queue and
    /// migrates it into that playlist; schema 4 recovers its playlists
    /// directly (M8 §6).
    pub(super) fn into_state(self, accept_queue: bool) -> (PersistedState, Option<QueueReset>) {
        let next_seq = self
            .checkpoints
            .values()
            .map(|entry| entry.touch_seq)
            .max()
            .map_or(1, |highest| highest.saturating_add(1));
        let recovered = if !accept_queue || self.schema_version < 3 {
            queue_codec::migrate_queue(Queue::default(), self.current_media, None)
        } else if self.schema_version == 3 {
            let (queue, reset) = queue_codec::recover_queue(
                self.queue.as_ref(),
                self.active_entry.as_ref(),
                self.current_media.as_ref(),
            );
            queue_codec::migrate_queue(queue, self.current_media, reset)
        } else {
            queue_codec::recover_playlists(
                queue_codec::RawPlaylists {
                    playlists: self.playlists.as_ref(),
                    playing: self.playing.as_ref(),
                    next_entry_id: self.next_entry_id.as_ref(),
                    next_playlist_id: self.next_playlist_id.as_ref(),
                },
                self.current_media,
            )
        };
        (
            PersistedState {
                schema_version: self.schema_version,
                current_media: recovered.current_media,
                volume: self.volume,
                checkpoints: self.checkpoints,
                playlists: recovered.playlists,
                playing: recovered.playing,
                next_seq,
                entry_ids: recovered.entry_ids,
                playlist_ids: recovered.playlist_ids,
            },
            recovered.reset,
        )
    }
}

impl<'de> Deserialize<'de> for PersistedState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(RawState::deserialize(deserializer)?.into_state(true).0)
    }
}

impl Serialize for PersistedState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a> {
            schema_version: u32,
            current_media: &'a Option<MediaId>,
            volume: f32,
            checkpoints: &'a BTreeMap<MediaId, PersistedCheckpoint>,
            playlists: Vec<queue_codec::PlaylistDto>,
            playing: u64,
            /// `None` serializes as `null`: an exhausted namespace (M8 §4).
            next_entry_id: Option<u64>,
            next_playlist_id: Option<u64>,
        }
        Out {
            schema_version: self.schema_version,
            current_media: &self.current_media,
            volume: self.volume,
            checkpoints: &self.checkpoints,
            playlists: queue_codec::encode_playlists(&self.playlists),
            playing: self.playing.get(),
            next_entry_id: self.entry_ids.next(),
            next_playlist_id: self.playlist_ids.next(),
        }
        .serialize(serializer)
    }
}

impl Default for PersistedState {
    fn default() -> Self {
        let playing = PlaylistId::from_raw(1);
        Self {
            schema_version: SCHEMA_VERSION,
            current_media: None,
            volume: Volume::FULL.as_gain(),
            checkpoints: BTreeMap::new(),
            playlists: vec![Playlist::new(playing, "Default".into())],
            playing,
            next_seq: 1,
            entry_ids: IdAllocator::default(),
            playlist_ids: IdAllocator::starting_at(Some(2)),
        }
    }
}

impl PersistedState {
    /// Read back through `Volume::new`, which already clamps to `[0, 1]` and
    /// maps non-finite input to `0.0` — so a hand-edited file needs no separate
    /// validation rule (D15).
    pub fn volume(&self) -> Volume {
        Volume::new(self.volume)
    }

    pub fn set_volume(&mut self, volume: Volume) {
        self.volume = volume.as_gain();
    }

    /// Read-only from outside `persistence`: the only path that can change the
    /// version a snapshot carries is `Self::migrate_to_current_schema`,
    /// visible solely to the store's own load path, and the store asserts
    /// [`SCHEMA_VERSION`] again before it writes.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Stamps this snapshot with the build's current schema version. The only
    /// caller is the store's load path, immediately after it accepts a file
    /// at an older, migratable version (design doc §4.5) — a file whose
    /// shape already deserialised cleanly into the current [`PersistedState`]
    /// (a v1 file's `position` lands straight in `Some`, and an absent
    /// `estimated` defaults to `None`), so nothing here needs to touch the
    /// data, only the label. Without this call, the next write would
    /// serialise v2 data — `estimated`, an optional `position` — under a v1
    /// envelope: a file that claims v1 while holding v2 data, which the next
    /// v1 build would read and quietly discard.
    pub(super) fn migrate_to_current_schema(&mut self) {
        self.schema_version = SCHEMA_VERSION;
    }

    pub fn playlists(&self) -> &[Playlist] {
        &self.playlists
    }

    pub fn playing(&self) -> PlaylistId {
        self.playing
    }

    fn index_of(&self, id: PlaylistId) -> Option<usize> {
        self.playlists
            .iter()
            .position(|playlist| playlist.id() == id)
    }

    /// `playlists` is never empty and `playing` always names a member; index
    /// 0 is the fallback that keeps this total without an `unwrap`.
    fn playing_index(&self) -> usize {
        self.index_of(self.playing).unwrap_or(0)
    }

    pub fn playlist(&self, id: PlaylistId) -> Option<&Playlist> {
        self.index_of(id).map(|index| &self.playlists[index])
    }

    /// Wired up by the task that exposes playlist mutation through `Session`
    /// (rename/remove); unused until then.
    #[allow(dead_code)]
    pub(crate) fn playlist_mut(&mut self, id: PlaylistId) -> Option<&mut Playlist> {
        self.index_of(id).map(|index| &mut self.playlists[index])
    }

    pub fn playing_playlist(&self) -> &Playlist {
        &self.playlists[self.playing_index()]
    }

    /// The *playing* playlist's queue: a convenience for transport and
    /// advance. Everything else names its playlist or looks an entry's
    /// owner up (M8 §5).
    pub fn queue(&self) -> &Queue {
        self.playing_playlist().queue()
    }

    /// Mutable access for `Session`, which is the only thing allowed to
    /// change what is queued (§3). The playing playlist's.
    pub(crate) fn queue_mut(&mut self) -> &mut Queue {
        let index = self.playing_index();
        self.playlists[index].queue_mut()
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

    /// All or nothing, against the global cap, with IDs from the one
    /// allocator (M8 P2, P7).
    pub(crate) fn enqueue(
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
        self.playlists[index]
            .queue_mut()
            .enqueue(batch, &mut self.entry_ids)
    }

    /// Wired up by the task that exposes playlist management through
    /// `Session`; unused until then.
    #[allow(dead_code)]
    pub(crate) fn create_playlist(&mut self, name: &str) -> Result<PlaylistId, PlaylistError> {
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

    /// Wired up by the task that exposes playlist management through
    /// `Session`; unused until then.
    #[allow(dead_code)]
    pub(crate) fn rename_playlist(
        &mut self,
        id: PlaylistId,
        name: &str,
    ) -> Result<(), PlaylistError> {
        let name = clean_name(name).ok_or(PlaylistError::InvalidName)?;
        self.playlist_mut(id)
            .ok_or(PlaylistError::Unknown(id))?
            .set_name(name);
        Ok(())
    }

    /// Removing the playing playlist moves `playing` to the next playlist,
    /// else the previous, and re-points `current_media` at that playlist's
    /// cursor — or clears it — so the cursor is never judged against the
    /// deleted playlist's media (M8 §5). Checkpoints are untouched.
    ///
    /// Wired up by the task that exposes playlist management through
    /// `Session`; unused until then.
    #[allow(dead_code)]
    pub(crate) fn remove_playlist(&mut self, id: PlaylistId) -> Result<Playlist, PlaylistError> {
        let index = self.index_of(id).ok_or(PlaylistError::Unknown(id))?;
        if self.playlists.len() == 1 {
            return Err(PlaylistError::LastPlaylist);
        }
        let removed = self.playlists.remove(index);
        if self.playing == id {
            let next = index.min(self.playlists.len() - 1);
            self.playing = self.playlists[next].id();
            self.repoint_current_media();
        }
        Ok(removed)
    }

    /// Only reachable through `remove_playlist` today; unused in a build
    /// that does not exercise it (this task's tests are the only caller).
    #[allow(dead_code)]
    pub(super) fn repoint_current_media(&mut self) {
        let queue = self.queue();
        self.current_media = queue
            .active()
            .and_then(|id| queue.get(id))
            .map(|entry| entry.media().clone());
    }

    /// Wired up by the task that exposes playlist management through
    /// `Session`; unused until then.
    #[allow(dead_code)]
    pub(crate) fn set_playing(&mut self, id: PlaylistId) {
        if self.index_of(id).is_some() {
            self.playing = id;
        }
    }

    pub fn current_media(&self) -> Option<&MediaId> {
        self.current_media.as_ref()
    }

    /// Named rather than ambient: the policy moves `current_media` in the same
    /// mutation that records the outgoing entry, and a plain field would let a
    /// later edit move it from anywhere.
    pub fn set_current_media(&mut self, media: MediaId) {
        self.current_media = Some(media);
    }

    /// How many media identities the file remembers, which is what the cap
    /// bounds (D2).
    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    pub fn entry_for(&self, media: &MediaId) -> Option<&PersistedCheckpoint> {
        self.checkpoints.get(media)
    }

    pub fn completed_for(&self, media: &MediaId) -> bool {
        self.checkpoints
            .get(media)
            .is_some_and(|entry| entry.completed)
    }

    /// The only place a `touch_seq` is ever assigned (D4). Loading, reading and
    /// restoring never touch one.
    ///
    /// The cap is a guard, not a hope (D2). If a fresh entry arrives at
    /// `MAX_ENTRIES` and eviction cannot find a victim — unreachable while
    /// `MAX_ENTRIES > 1`, since the current entry is the only protected one and
    /// the incoming media is by definition not yet in the map — the incoming
    /// checkpoint is dropped rather than let the map grow past the cap, and the
    /// fact is logged instead of swallowed.
    pub fn record(&mut self, checkpoint: &PlaybackCheckpoint, completed: bool) {
        let fresh = !self.checkpoints.contains_key(&checkpoint.media);
        if fresh && self.checkpoints.len() >= MAX_ENTRIES {
            let evicted = self.evict_one(&checkpoint.media);
            if !evicted {
                debug_assert!(
                    false,
                    "MAX_ENTRIES ({MAX_ENTRIES}) left no evictable entry; the cap would be exceeded"
                );
                tracing::warn!(
                    media = %checkpoint.media,
                    "no entry could be evicted at the MAX_ENTRIES cap; dropping the incoming checkpoint rather than exceeding it"
                );
                return;
            }
        }
        let touch_seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.checkpoints.insert(
            checkpoint.media.clone(),
            PersistedCheckpoint {
                position: Some(checkpoint.position),
                completed,
                touch_seq,
                updated_at: checkpoint.updated_at,
                // `record` is the established-checkpoint path unchanged from
                // M1/M2 (§4.2's estimated write path is Task 6's, not this
                // one's), and it always replaces the entry wholesale rather
                // than merging into it — so any previously stored estimate
                // for this media is cleared here too.
                estimated: None,
            },
        );
    }

    /// The estimated-checkpoint write path (§4.2, Task 6): sets `estimated`
    /// and, when this media completed, `completed` — and touches nothing
    /// else. **Never writes `position`.** That is what makes the two write
    /// rules one code path rather than two: whether an established
    /// checkpoint already exists for this media or not, an estimated
    /// location is never allowed to become it, so this method simply has no
    /// way to. `completed` is written exactly as given rather than merged,
    /// the same as `record`'s — the only caller with a `true` to pass is an
    /// estimated `EndOfTrack`, and it is itself the evidence that finished
    /// the entry.
    ///
    /// Participates in the same `touch_seq`/eviction bookkeeping as
    /// `record`: an estimate-only entry is still a real entry for §2's cap
    /// and LRU eviction, not a second-class one.
    pub fn record_estimated(
        &mut self,
        media: MediaId,
        estimated: Duration,
        updated_at: OffsetDateTime,
        completed: bool,
    ) {
        let fresh = !self.checkpoints.contains_key(&media);
        if fresh && self.checkpoints.len() >= MAX_ENTRIES {
            let evicted = self.evict_one(&media);
            if !evicted {
                debug_assert!(
                    false,
                    "MAX_ENTRIES ({MAX_ENTRIES}) left no evictable entry; the cap would be exceeded"
                );
                tracing::warn!(
                    media = %media,
                    "no entry could be evicted at the MAX_ENTRIES cap; dropping the incoming checkpoint rather than exceeding it"
                );
                return;
            }
        }
        let touch_seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let entry = self
            .checkpoints
            .entry(media)
            .or_insert_with(|| PersistedCheckpoint {
                // No established position exists yet for a fresh entry (§4.1,
                // §4.2): an estimate must never bootstrap one into being.
                position: None,
                completed: false,
                touch_seq,
                updated_at,
                estimated: None,
            });
        entry.estimated = Some(estimated);
        entry.completed = completed;
        entry.touch_seq = touch_seq;
        entry.updated_at = updated_at;
    }

    /// The lowest `touch_seq` among entries that are neither current nor the
    /// one arriving. Returns whether a victim was found and removed; `record`
    /// treats a `false` result as the cap guard firing.
    fn evict_one(&mut self, incoming: &MediaId) -> bool {
        let victim = self
            .checkpoints
            .iter()
            .filter(|(key, _)| Some(*key) != self.current_media.as_ref() && *key != incoming)
            .min_by_key(|(_, entry)| entry.touch_seq)
            .map(|(key, _)| key.clone());
        match victim {
            Some(victim) => {
                self.checkpoints.remove(&victim);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::id::AbsolutePath;

    fn id(name: &str) -> MediaId {
        match AbsolutePath::new(format!("/music/{name}.flac").into()) {
            Ok(path) => MediaId::LocalFile(path),
            Err(error) => panic!("a literal absolute path must parse: {error}"),
        }
    }

    /// `evict_one` must report "no victim" rather than evict the current entry,
    /// when the current entry is the only one in the map. This
    /// state is unreachable through `record` at `MAX_ENTRIES = 512` (the
    /// current entry is the only protected one, so 511 candidates always
    /// remain), so the guard is pinned directly against the private helper.
    #[test]
    fn evict_one_reports_no_victim_when_the_only_entry_is_current() {
        let mut state = PersistedState::default();
        let only = id("only");
        state.checkpoints.insert(
            only.clone(),
            PersistedCheckpoint {
                position: Some(Duration::ZERO),
                completed: false,
                touch_seq: 1,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                estimated: None,
            },
        );
        state.current_media = Some(only.clone());

        let evicted = state.evict_one(&id("incoming"));

        assert!(!evicted, "the current entry must never be evicted");
        assert_eq!(state.checkpoints.len(), 1, "nothing was removed");
        assert!(state.entry_for(&only).is_some());
    }
}

#[cfg(test)]
mod playlist_tests {
    use super::*;
    use crate::media::id::AbsolutePath;
    use crate::playlist::{MAX_PLAYLISTS, PlaylistError};
    use crate::queue::{
        DisplayMetadata, MAX_PLAYLIST_ENTRIES, NewQueueEntry, QueueError, QueueSource,
    };

    fn entry(name: &str) -> NewQueueEntry {
        let path = AbsolutePath::new(format!("/music/{name}.flac").into())
            .unwrap_or_else(|error| panic!("absolute: {error}"));
        NewQueueEntry::new(
            MediaId::LocalFile(path.clone()),
            QueueSource::LocalFile(path),
            DisplayMetadata::default(),
        )
        .unwrap_or_else(|error| panic!("valid: {error}"))
    }

    #[test]
    fn a_fresh_state_has_one_playing_playlist_named_default() {
        let state = PersistedState::default();
        assert_eq!(state.playlists().len(), 1);
        assert_eq!(state.playlists()[0].name(), "Default");
        assert_eq!(state.playing(), state.playlists()[0].id());
    }

    #[test]
    fn entry_ids_are_unique_across_playlists_and_owner_lookup_finds_them() {
        let mut state = PersistedState::default();
        let a = state.playing();
        let b = state.create_playlist("B").expect("room");
        let in_a = state.enqueue(a, vec![entry("x")]).expect("fits");
        let in_b = state.enqueue(b, vec![entry("x")]).expect("fits");
        assert_ne!(in_a[0], in_b[0]);
        assert_eq!(state.owner_of(in_a[0]), Some(a));
        assert_eq!(state.owner_of(in_b[0]), Some(b));
        assert_eq!(state.total_entries(), 2);
        assert!(
            state.queue().get(in_b[0]).is_none(),
            "queue() is the playing playlist only"
        );
    }

    #[test]
    fn the_cap_is_global_and_a_refused_batch_changes_nothing() {
        let mut state = PersistedState::default();
        let a = state.playing();
        let b = state.create_playlist("B").expect("room");
        state
            .enqueue(
                a,
                (0..MAX_PLAYLIST_ENTRIES - 1)
                    .map(|i| entry(&format!("t{i}")))
                    .collect(),
            )
            .expect("fits");
        let before = state.clone();
        assert_eq!(
            state.enqueue(b, vec![entry("y"), entry("z")]),
            Err(QueueError::Capacity {
                requested: 2,
                available: 1
            })
        );
        assert_eq!(state.total_entries(), before.total_entries());
        assert_eq!(state.entry_ids, before.entry_ids, "no ID was burned");
    }

    #[test]
    fn enqueue_into_a_deleted_playlist_is_refused() {
        let mut state = PersistedState::default();
        let b = state.create_playlist("B").expect("room");
        state.remove_playlist(b).expect("another exists");
        assert_eq!(
            state.enqueue(b, vec![entry("x")]),
            Err(QueueError::UnknownPlaylist(b.get()))
        );
    }

    #[test]
    fn playlist_ids_are_never_reused() {
        let mut state = PersistedState::default();
        let b = state.create_playlist("B").expect("room");
        state.remove_playlist(b).expect("another exists");
        let c = state.create_playlist("C").expect("room");
        assert!(c.get() > b.get());
    }

    #[test]
    fn limits_and_names() {
        let mut state = PersistedState::default();
        assert_eq!(
            state.create_playlist("   "),
            Err(PlaylistError::InvalidName)
        );
        for i in 1..MAX_PLAYLISTS {
            state.create_playlist(&format!("P{i}")).expect("room");
        }
        assert_eq!(
            state.create_playlist("one too many"),
            Err(PlaylistError::TooMany)
        );
        let only = PersistedState::default();
        let mut only = only;
        assert_eq!(
            only.remove_playlist(only.playing()).map(|_| ()),
            Err(PlaylistError::LastPlaylist)
        );
    }

    #[test]
    fn deleting_the_playing_playlist_moves_playing_and_repoints_current_media() {
        let mut state = PersistedState::default();
        let a = state.playing();
        let b = state.create_playlist("B").expect("room");
        let in_b = state.enqueue(b, vec![entry("kept")]).expect("fits");
        let _ = state
            .playlist_mut(b)
            .map(|p| p.queue_mut().set_active(Some(in_b[0])));
        state.set_current_media(entry_media("old"));

        state.remove_playlist(a).expect("another exists");

        assert_eq!(state.playing(), b);
        assert_eq!(
            state.queue().active(),
            Some(in_b[0]),
            "the cursor is not judged against the old media"
        );
        assert_eq!(state.current_media(), Some(&entry_media("kept")));
    }

    #[test]
    fn deleting_the_playing_playlist_clears_current_media_when_the_next_has_no_cursor() {
        let mut state = PersistedState::default();
        let a = state.playing();
        state.create_playlist("B").expect("room");
        state.set_current_media(entry_media("old"));
        state.remove_playlist(a).expect("another exists");
        assert_eq!(state.current_media(), None);
    }

    #[test]
    fn deleting_another_playlist_touches_neither_playing_nor_current_media() {
        let mut state = PersistedState::default();
        let a = state.playing();
        let b = state.create_playlist("B").expect("room");
        state.set_current_media(entry_media("old"));
        state.remove_playlist(b).expect("another exists");
        assert_eq!(state.playing(), a);
        assert_eq!(state.current_media(), Some(&entry_media("old")));
    }

    fn entry_media(name: &str) -> MediaId {
        entry(name).media().clone()
    }
}
