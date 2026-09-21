//! A named playlist (M8 §4): today's `Queue` plus an identity, a name and an
//! optional shuffle. Pure data and policy, like `queue.rs`: it never
//! renders, decodes or touches the filesystem, and it changes only through
//! `Session`.

use crate::queue::{Direction, Queue, QueueEntryId};

pub const MAX_PLAYLISTS: usize = 32;
pub const MAX_NAME_CHARS: usize = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PlaylistId(u64);

impl PlaylistId {
    pub fn get(self) -> u64 {
        self.0
    }
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
    #[doc(hidden)]
    pub fn from_raw_for_tests(raw: u64) -> Self {
        Self(raw)
    }
}

/// Shuffle is on exactly while a playlist holds one of these (§7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shuffle {
    pub seed: u64,
    /// Sorts ahead of every hashed key while it is still a member, so the
    /// track playing when shuffle was switched on has the whole list ahead.
    pub first: Option<QueueEntryId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlaylistError {
    #[error("at most {MAX_PLAYLISTS} playlists")]
    TooMany,
    #[error("the last playlist cannot be deleted")]
    LastPlaylist,
    #[error("playlist {} no longer exists", .0.get())]
    Unknown(PlaylistId),
    #[error("a playlist name is 1 to {MAX_NAME_CHARS} characters")]
    InvalidName,
    #[error("playlist IDs are exhausted")]
    IdExhausted,
}

/// SplitMix64's output function over `x + golden gamma`. Written out
/// because the standard library's hasher is not stable across releases and
/// a shuffled order must survive a restart. Pinned by a test vector.
pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Trimmed, cut to [`MAX_NAME_CHARS`] characters; `None` when nothing is left.
pub fn clean_name(raw: &str) -> Option<String> {
    let name: String = raw.trim().chars().take(MAX_NAME_CHARS).collect();
    let name = name.trim_end().to_owned();
    (!name.is_empty()).then_some(name)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Playlist {
    id: PlaylistId,
    name: String,
    shuffle: Option<Shuffle>,
    queue: Queue,
}

impl Playlist {
    pub(crate) fn new(id: PlaylistId, name: String) -> Self {
        Self::from_parts(id, name, None, Queue::default())
    }

    /// Persistence's constructor and the tests'. The caller has validated
    /// the name.
    pub fn from_parts(
        id: PlaylistId,
        name: String,
        shuffle: Option<Shuffle>,
        queue: Queue,
    ) -> Self {
        Self {
            id,
            name,
            shuffle,
            queue,
        }
    }

    pub fn id(&self) -> PlaylistId {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn shuffle(&self) -> Option<Shuffle> {
        self.shuffle
    }
    pub fn queue(&self) -> &Queue {
        &self.queue
    }
    pub(crate) fn queue_mut(&mut self) -> &mut Queue {
        &mut self.queue
    }
    #[doc(hidden)]
    pub fn queue_mut_for_tests(&mut self) -> &mut Queue {
        &mut self.queue
    }
    pub(crate) fn set_name(&mut self, name: String) {
        self.name = name;
    }
    pub(crate) fn set_shuffle(&mut self, shuffle: Option<Shuffle>) {
        self.shuffle = shuffle;
    }

    /// The order traversal follows: list order, or with shuffle on `first`
    /// then ascending `(splitmix64(seed + id), id)`.
    ///
    /// ponytail: O(n log n) per call, called on a keypress or a track end,
    /// never per frame. Cache the order if n ever grows past the 4,096 cap.
    ///
    /// An entry added while shuffle is on would land at its hash position,
    /// possibly behind the current track; `Session::enqueue` reshuffles so
    /// it cannot.
    pub fn playback_order(&self) -> Vec<QueueEntryId> {
        let mut ids: Vec<QueueEntryId> = self.queue.entries().iter().map(|e| e.id()).collect();
        if let Some(shuffle) = self.shuffle {
            ids.sort_by_key(|id| {
                (
                    shuffle.first != Some(*id),
                    splitmix64(shuffle.seed.wrapping_add(id.get())),
                    id.get(),
                )
            });
        }
        ids
    }

    /// One traversal policy for manual navigation and automatic advance
    /// (P6). `None` at either boundary: no wrap.
    pub fn neighbor(&self, anchor: QueueEntryId, direction: Direction) -> Option<QueueEntryId> {
        let order = self.playback_order();
        let index = order.iter().position(|id| *id == anchor)?;
        let target = match direction {
            Direction::Up => index.checked_sub(1)?,
            Direction::Down => index + 1,
        };
        order.get(target).copied()
    }

    pub fn first_in_order(&self) -> Option<QueueEntryId> {
        self.playback_order().first().copied()
    }
}
