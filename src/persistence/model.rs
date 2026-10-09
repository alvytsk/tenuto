//! The state file's shape: one checkpoint per media identity (D2), plus the
//! session-wide facts the file carries.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::queue_codec::{self, QueueReset};
use crate::media::id::MediaId;
use crate::playlist::{PlaylistError, PlaylistId, PlaylistSet};
use crate::queue::{Direction, DisplayUpdate, NewQueueEntry, QueueEntryId, QueueError, Removed};
use crate::resume::PlaybackCheckpoint;
use crate::volume::Volume;
use url::Url;

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
    /// Every playlist, which one is playing, and both ID allocators (M9.1).
    /// Private: every change goes through the forwarding methods below, and
    /// nothing outside hands out `&mut PlaylistSet`.
    playlists: PlaylistSet,
    next_seq: u64,
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
            queue_codec::Recovered {
                set: PlaylistSet::default(),
                current_media: self.current_media,
                reset: None,
            }
        } else if self.schema_version == 3 {
            let (entries, cursor, reset) = queue_codec::recover_queue(
                self.queue.as_ref(),
                self.active_entry.as_ref(),
                self.current_media.as_ref(),
            );
            queue_codec::migrate_queue(entries, cursor, self.current_media, reset)
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
                playlists: recovered.set,
                next_seq,
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
            playing: self.playlists.playing().get(),
            next_entry_id: self.playlists.next_entry_id(),
            next_playlist_id: self.playlists.next_playlist_id(),
        }
        .serialize(serializer)
    }
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            current_media: None,
            volume: Volume::FULL.as_gain(),
            checkpoints: BTreeMap::new(),
            playlists: PlaylistSet::default(),
            next_seq: 1,
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

    /// The only read path to the playlists (M9.1 §6).
    pub fn playlists(&self) -> &PlaylistSet {
        &self.playlists
    }

    // Forwarding (M9.1 §6, option (a)): the envelope never hands out the
    // set mutably; each playlist rule lives in `PlaylistSet`, once.

    pub(crate) fn enqueue(
        &mut self,
        dest: PlaylistId,
        batch: Vec<NewQueueEntry>,
    ) -> Result<Vec<QueueEntryId>, QueueError> {
        self.playlists.enqueue(dest, batch)
    }

    pub(crate) fn create_playlist(&mut self, name: &str) -> Result<PlaylistId, PlaylistError> {
        self.playlists.create(name)
    }

    pub(crate) fn rename_playlist(
        &mut self,
        id: PlaylistId,
        name: &str,
    ) -> Result<(), PlaylistError> {
        self.playlists.rename(id, name)
    }

    pub(crate) fn set_shuffle(
        &mut self,
        id: PlaylistId,
        seed: Option<u64>,
    ) -> Result<(), PlaylistError> {
        self.playlists.set_shuffle(id, seed)
    }

    pub(crate) fn move_entry(
        &mut self,
        id: QueueEntryId,
        direction: Direction,
    ) -> Result<bool, QueueError> {
        self.playlists.move_entry(id, direction)
    }

    pub(crate) fn remove_entry(&mut self, id: QueueEntryId) -> Result<Removed, QueueError> {
        self.playlists.remove_entry(id)
    }

    pub(crate) fn clear_playlist(&mut self, id: PlaylistId) -> Result<(), PlaylistError> {
        self.playlists.clear(id)
    }

    /// The one forward that acts on the envelope too: the set's effect on
    /// the persisted current media is applied here, so no caller can drop
    /// it. Returns the successor.
    pub(crate) fn delete_playlist(&mut self, id: PlaylistId) -> Result<PlaylistId, PlaylistError> {
        let deletion = self.playlists.delete(id)?;
        self.current_media = deletion.current_media.apply(self.current_media.take());
        Ok(deletion.successor)
    }

    pub(crate) fn adopt(&mut self, entry: QueueEntryId) -> Result<(), QueueError> {
        self.playlists.adopt(entry)
    }

    pub(crate) fn update_display(
        &mut self,
        entry: QueueEntryId,
        update: &DisplayUpdate,
    ) -> Result<bool, QueueError> {
        self.playlists.update_display(entry, update)
    }

    pub(crate) fn set_podcast_fallback(
        &mut self,
        entry: QueueEntryId,
        url: Url,
    ) -> Result<bool, QueueError> {
        self.playlists.set_podcast_fallback(entry, url)
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
    use crate::queue::{DisplayMetadata, NewQueueEntry, QueueSource};

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
        assert_eq!(state.playlists().playing(), state.playlists()[0].id());
    }

    #[test]
    fn deleting_the_playing_playlist_moves_playing_and_repoints_current_media() {
        let mut state = PersistedState::default();
        let a = state.playlists().playing();
        let b = state.create_playlist("B").expect("room");
        let in_a = state.enqueue(a, vec![entry("old")]).expect("fits");
        let in_b = state.enqueue(b, vec![entry("kept")]).expect("fits");
        state.adopt(in_b[0]).expect("queued");
        state.adopt(in_a[0]).expect("queued");
        state.set_current_media(entry_media("old"));

        assert_eq!(state.delete_playlist(a), Ok(b));

        assert_eq!(state.playlists().playing(), b);
        assert_eq!(
            state.playlists().playing_playlist().queue().active(),
            Some(in_b[0]),
            "the cursor is not judged against the old media"
        );
        assert_eq!(state.current_media(), Some(&entry_media("kept")));
    }

    #[test]
    fn deleting_the_playing_playlist_clears_current_media_when_the_next_has_no_cursor() {
        let mut state = PersistedState::default();
        let a = state.playlists().playing();
        state.create_playlist("B").expect("room");
        state.set_current_media(entry_media("old"));
        state.delete_playlist(a).expect("another exists");
        assert_eq!(state.current_media(), None);
    }

    #[test]
    fn deleting_another_playlist_touches_neither_playing_nor_current_media() {
        let mut state = PersistedState::default();
        let a = state.playlists().playing();
        let b = state.create_playlist("B").expect("room");
        state.set_current_media(entry_media("old"));
        assert_eq!(state.delete_playlist(b), Ok(a));
        assert_eq!(state.playlists().playing(), a);
        assert_eq!(state.current_media(), Some(&entry_media("old")));
    }

    fn entry_media(name: &str) -> MediaId {
        entry(name).media().clone()
    }
}
