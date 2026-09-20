//! The checkpoint policy: what to persist, and when.
//!
//! Pure by construction — no I/O, no threads, no clock of its own. Every
//! decision is a function of the lossless event stream, the single `Progress`
//! sample the application takes per iteration, and an injected `ClockSample`.
//!
//! The application's order is load-bearing: drain events, then sample once.
//! §3 establishes that a command's event reaches the application on the pass
//! *after* the one that applied it, and that the pass publishes progress before
//! it flushes events — so the sample that follows a transition event is
//! strictly newer than the transition it reports.
//!
//! M5 §6 adds a second responsibility: `Session` is the only allocator of
//! `LoadRequestId`s and the only place that decides whether a `Loaded`
//! outcome may adopt a queue occurrence. Every media-specific event — not
//! only `Loaded` — is now gated on owning the currently adopted token before
//! it may touch history at all (D20); a stale or unowned event changes
//! nothing, not even the revision the policy tracks.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::clock::ClockSample;
use crate::media::capabilities::{Continuity, MediaCapabilities};
use crate::media::id::MediaId;
use crate::media::metadata::MediaMetadata;
use crate::persistence::model::{PersistedCheckpoint, PersistedState};
use crate::persistence::writer::Urgency;
use crate::playback::checkpoint::PlaybackCheckpoint;
use crate::playback::command::{LoadRequestId, ResumeIntent};
use crate::playback::event::{PlaybackEvent, Progress, ShutdownReport, StartDisposition};
use crate::playback::provenance::PositionProvenance;
use crate::playback::state::PlaybackState;
use crate::playback::volume::Volume;
use crate::playlist::{Playlist, PlaylistError, PlaylistId, Shuffle};
use crate::queue::{
    Direction, DisplayDuration, DurationSource, NewQueueEntry, Queue, QueueEntryId, QueueError,
    QueueSource,
};
use url::Url;
// Re-exported so `src/app.rs` and `tests/resume_contract.rs` keep importing
// these from `session` — the type and the function moved to `src/resume.rs`
// so the playback worker could depend on them too (G3), without dragging
// `crate::persistence` in behind them. The worker is `decide_resume`'s only
// production caller now (Ruling 5). Converting a stored `PersistedCheckpoint`
// into the persistence-free shape `decide_resume` reasons about is
// `resume::resume_candidate`'s job, not this module's — there used to be a
// second, infallible conversion here (`impl From<&PersistedCheckpoint> for
// ResumeCandidate`), but it had no production caller (`app.rs` always used
// `resume_candidate` instead) and its `Duration::ZERO` fallback for an
// absent `position` was exactly the "resume at start" loss this amendment
// exists to prevent, reachable by anyone who reached for the obvious `.into()`
// instead. Deleted rather than fixed in place: a function that must not be
// called with an unverified entry is safer removed than documented.
pub use crate::resume::{ResumeDecision, decide_resume};
use crate::resume::{restart_preference, resume_candidate};

/// The capture interval §6 requires while playing. With the writer's 2 s
/// coalescing window it bounds worst-case loss at 7 s.
pub const CAPTURE_INTERVAL: Duration = Duration::from_secs(5);

/// The application-wide cap on loads that have been sent but whose outcome
/// has not yet arrived (M5 §6, global constraint). Queue membership does not
/// pin a checkpoint — this is a bound on in-flight loads only, unrelated to
/// the 512-entry checkpoint cap.
pub const MAX_PENDING_LOADS: usize = 16;

#[derive(Debug)]
pub enum Action {
    None,
    Submit {
        state: PersistedState,
        urgency: Urgency,
    },
}

/// What a `Load` this session sent is for: a queue occurrence, or the
/// caller's own load with nothing queued behind it (the pre-M5 shape every
/// existing call site still uses).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadTarget {
    Queue(QueueEntryId),
    Legacy,
}

/// Why `register_load` refused a token.
#[derive(Debug, Eq, PartialEq)]
pub enum RegisterLoadError {
    /// `MAX_PENDING_LOADS` are already outstanding.
    Busy,
    /// `LoadTarget::Queue(id)` named an entry no longer in the queue.
    UnknownEntry,
    /// `LoadTarget::Queue(id)` named an entry whose media does not match.
    MediaMismatch,
}

/// The load `Session` currently believes owns playback — the request its
/// `Loaded` adopted, and what that load was for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdoptedLoad {
    pub request: LoadRequestId,
    pub target: LoadTarget,
}

/// What an eligible `EndOfTrack` asks the caller to do next (M5 §6).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Advance {
    Next(QueueEntryId),
    EndOfQueue,
}

/// The result of removing a queue entry: the submission it produced, whether
/// playback must stop because the removed entry was active, and which entry
/// (if any) the caller should offer to select next. `selection` is advisory
/// only — `Session` never activates it itself (§3: only a `Loaded` adopts an
/// occurrence).
#[derive(Debug)]
pub struct Removal {
    pub action: Action,
    pub stop_playback: bool,
    pub selection: Option<QueueEntryId>,
}

/// A partial update to a queue entry's display metadata: a field left `None`
/// leaves the entry's existing value alone. Distinct from `DisplayMetadata`
/// itself, whose `None` means "nothing known" and would blank out a field a
/// caller never meant to touch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DisplayUpdate {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<String>,
    pub duration: Option<DisplayDuration>,
}

/// A registered, not-yet-resolved load. Tracked by `Session` so that a
/// `Loaded` can be checked against what it was actually asked to load, and so
/// a queue mutation can invalidate a load racing the entry it targets.
#[derive(Clone, Debug)]
struct PendingLoad {
    target: LoadTarget,
    media: MediaId,
    /// Set by a queue mutation that removes or otherwise invalidates
    /// `target` before this load's outcome arrives (M5 §6). An invalidated
    /// `Loaded` is never adopted; its registration is still retired normally.
    invalidated: bool,
}

/// The position the policy would use for a checkpoint it can no longer
/// sample.
struct Sample {
    session_rev: u64,
    media: MediaId,
    position: Duration,
    /// Whether `position` was decoder-established or an estimate when this
    /// sample was taken (§3, Task 6). Read back by a fallback capture
    /// (`capture_current`) that has no fresher provenance of its own to
    /// offer.
    provenance: PositionProvenance,
}

pub struct Session {
    /// The authoritative state. `Action::Submit` carries a clone, which becomes
    /// the writer's property; the session never shares a reference into it.
    state: PersistedState,
    session_rev: u64,
    playback: PlaybackState,
    current_media: Option<MediaId>,
    /// Whether `current_media` is complete, so a checkpoint written before any
    /// further event still carries the right completion. The pair moves through
    /// `adopt_media` and nowhere else; every other write here changes the flag
    /// for a media that is not moving.
    completed: bool,
    last_sample: Option<Sample>,
    /// Monotonic anchor for the 5 s rule; set when playback establishes.
    last_capture: Option<Instant>,
    /// Raised by an event that forces a checkpoint but carries no position,
    /// keyed by the revision that event carried (D13).
    pending_force: Option<u64>,
    /// A stopped seek's target, which supersedes `Progress.position` until the
    /// engine resolves it (D17).
    outstanding_target: Option<Duration>,
    /// Whether playback established, or the position changed explicitly, since
    /// the current media was loaded. It gates every checkpoint whose position
    /// comes from `Progress` — a resolved force, the ordinary 5 s capture, the
    /// shutdown snapshot and the outgoing entry a media switch records —
    /// because such a position is one the engine never validated until
    /// something established. D20 gates only the shutdown force and reads the
    /// flag at the current revision; §19 records why the shipped gate is wider
    /// and why the flag latches until the next `Loaded`. The two positions that
    /// arrive on events of their own are exempt, and say so where they are
    /// recorded (D6).
    established: bool,
    /// A positive checkpoint the current run must not overwrite, because
    /// playback fell back to zero on a source that cannot resume (§10).
    ///
    /// Deliberately not a max-position merge: the point is to recover the
    /// *earlier* resume point, and progress heard in a fallback run does not
    /// replace it however far it goes (R4). Set from `Loaded.disposition`
    /// rather than a later warning, so it is in force before any `Playing` or
    /// progress event can be observed (§5). Ends only on an *established*
    /// `RestartEstablished` (G1), an *established* `SeekCompleted`, or
    /// verified completion reached from an *established* timeline — never on
    /// `CapabilitiesChanged` alone (§10), and, since Task 6 (§4.4), never on
    /// any of those three reached from an *estimated* one either: an
    /// unconfirmed landing does not earn the right to clear a guard that
    /// exists to protect a confirmed position.
    protected: Option<Duration>,
    /// Whether the position this media is currently tracking is
    /// decoder-established or still carries an estimate forward (§3, §4.2).
    /// Kept current by every tick this session observes (mirroring the
    /// engine's own `position_provenance`, one pass behind) and by every
    /// event that reports a landing of its own — `SeekCompleted`,
    /// `RestartEstablished`, `EndOfTrack` — so that a write with no
    /// intervening tick (`SeekTargetStored`, R5) still reads the right
    /// answer. This is what every write path consults to decide between
    /// `record_current` (writes `position`) and `record_current_estimated`
    /// (writes only `estimated`, and never bootstraps or overwrites
    /// `position`) — the session policy reads this field and *never*
    /// `PositionQuality`, which is an orthogonal axis (see
    /// `playback::provenance`). Reset to `Established` on every `Loaded`:
    /// a fresh load is itself one of the acts that re-establishes the
    /// absolute position, and nothing has claimed otherwise for the
    /// incoming media yet.
    position_provenance: PositionProvenance,
    /// The last token issued by `register_load`. Strictly increasing, never
    /// reused; the first token is `1`.
    next_request: u64,
    /// Loads sent but not yet resolved, keyed by the token `register_load`
    /// handed out for them.
    pending: BTreeMap<LoadRequestId, PendingLoad>,
    /// The load whose `Loaded` this session most recently adopted, if any.
    adopted: Option<AdoptedLoad>,
    /// `session_rev` at the moment `adopted` was set. Presently informational
    /// — `session_rev` itself already tracks adopted playback — kept so a
    /// later task has it without redefining `session_rev`'s meaning.
    adopted_rev_floor: u64,
    /// The highest revision any accepted or protected event has carried,
    /// including a load outcome for a target this session never adopted.
    /// Distinct from `session_rev`, which belongs to adopted playback alone.
    latest_engine_rev: u64,
    /// The token of the most recent `Loaded` this session observed, current
    /// or not. Ownership of every other media event requires this to still
    /// name `adopted.request` (D3, D20): a `Loaded` for a load this session
    /// declined to adopt still moves this field, which is what stops a stale
    /// `EndOfTrack` from that declined load's playback reaching history
    /// before its stop lands.
    last_loaded: Option<LoadRequestId>,
    /// Raised when an adopted `Loaded` cannot be honoured (its target was
    /// invalidated, or the media no longer matches) and the caller must stop
    /// the engine rather than let it keep playing an occurrence this session
    /// will never adopt.
    stop_requested: bool,
    /// What an eligible `EndOfTrack` asks the caller to do next, until taken.
    advance: Option<Advance>,
    /// The `(adopted.request, session_rev)` pair of the last `EndOfTrack`
    /// this session recorded, so a duplicate delivery of the same completion
    /// cannot record history or advance the queue twice (§6).
    completion_seen: Option<(LoadRequestId, u64)>,
    /// Whether the adopted media may be checkpointed. `false` for
    /// `Continuity::Indefinite`: listening time is not a resume point (M7
    /// §9). Set last in `on_loaded`, so `record_outgoing` always runs under
    /// the outgoing media's value.
    checkpointable: bool,
}

impl Session {
    pub fn new(state: PersistedState) -> Self {
        Self {
            state,
            session_rev: 0,
            playback: PlaybackState::Idle,
            // Learned from `Loaded`, never from the file: what was current last
            // run says nothing about what this run is playing.
            current_media: None,
            completed: false,
            last_sample: None,
            last_capture: None,
            pending_force: None,
            outstanding_target: None,
            established: false,
            protected: None,
            position_provenance: PositionProvenance::Established,
            next_request: 0,
            pending: BTreeMap::new(),
            adopted: None,
            adopted_rev_floor: 0,
            latest_engine_rev: 0,
            last_loaded: None,
            stop_requested: false,
            advance: None,
            completion_seen: None,
            checkpointable: true,
        }
    }

    /// The state as it currently stands, for a reader that needs it directly
    /// rather than through whichever `Action` happens to submit next —
    /// `Action::Submit` only ever carries a clone of exactly this. The test
    /// suite is the one caller today: a protected capture can legitimately
    /// leave the state unchanged, so asserting on it this way is what lets a
    /// test tell "no submission happened" apart from "a submission happened
    /// and changed nothing."
    pub fn state(&self) -> &PersistedState {
        &self.state
    }

    // --------------------------------------------------------- load tokens

    /// Allocates a token for a `Load` this session is about to send, and
    /// records what that load is for. `Busy` at `MAX_PENDING_LOADS`, checked
    /// before the queue lookup below — a caller cannot be told `Busy` for one
    /// media and `UnknownEntry` for another from the very same call.
    pub fn register_load(
        &mut self,
        target: LoadTarget,
        media: &MediaId,
    ) -> Result<LoadRequestId, RegisterLoadError> {
        if self.pending.len() >= MAX_PENDING_LOADS {
            return Err(RegisterLoadError::Busy);
        }
        if let LoadTarget::Queue(id) = target {
            match self.state.find_entry(id) {
                None => return Err(RegisterLoadError::UnknownEntry),
                Some(entry) if entry.media() != media => {
                    return Err(RegisterLoadError::MediaMismatch);
                }
                Some(_) => {}
            }
        }
        self.next_request += 1;
        let request = LoadRequestId::from_raw(self.next_request);
        self.pending.insert(
            request,
            PendingLoad {
                target,
                media: media.clone(),
                invalidated: false,
            },
        );
        Ok(request)
    }

    /// Drops a registration this session's caller never sent, or no longer
    /// needs — a `Load` the command queue refused, or a superseded legacy
    /// load. A no-op if `request` has already resolved or was never
    /// registered.
    pub fn retract_load(&mut self, request: LoadRequestId) {
        self.pending.remove(&request);
    }

    pub fn pending_load_count(&self) -> usize {
        self.pending.len()
    }

    pub fn adopted(&self) -> Option<AdoptedLoad> {
        self.adopted
    }

    /// Whether `event` is one `observe` will actually act on: for `Loaded`, a
    /// current registration whose target and media still line up; for every
    /// other media event, ownership of the adopted token (D20).
    /// `VolumeChanged` is profile-wide and always accepted. Exposed so a
    /// caller can tell a legitimate `Action::None` apart from an event this
    /// session is simply not the owner of.
    pub fn accepts_media_event(&self, event: &PlaybackEvent) -> bool {
        match event {
            PlaybackEvent::Loaded { request, media, .. } => {
                event.session_rev() >= self.latest_engine_rev
                    && self.registered_target(*request, media).is_some()
            }
            PlaybackEvent::VolumeChanged { .. } => true,
            _ => self.owns_adopted(event.session_rev()),
        }
    }

    /// The still-valid registration for `request`, if `media` matches it, it
    /// has not been invalidated, and — for a queue target — the entry still
    /// exists carrying that same media.
    fn registered_target(&self, request: LoadRequestId, media: &MediaId) -> Option<LoadTarget> {
        let pending = self.pending.get(&request)?;
        if pending.invalidated || &pending.media != media {
            return None;
        }
        match pending.target {
            LoadTarget::Queue(id) => self
                .state
                .find_entry(id)
                .is_some_and(|entry| entry.media() == media)
                .then_some(pending.target),
            LoadTarget::Legacy => Some(pending.target),
        }
    }

    /// The shared ownership gate (D20): an adopted token exists, the most
    /// recent `Loaded` this session observed still names it, and `event_rev`
    /// is not behind what this session has already recognized.
    fn owns_adopted(&self, event_rev: u64) -> bool {
        match self.adopted {
            Some(adopted) => {
                self.last_loaded == Some(adopted.request) && event_rev >= self.latest_engine_rev
            }
            None => false,
        }
    }

    pub fn take_stop_request(&mut self) -> bool {
        std::mem::take(&mut self.stop_requested)
    }

    pub fn take_advance(&mut self) -> Option<Advance> {
        self.advance.take()
    }

    // --------------------------------------------------------------- queue

    /// Enqueues `batch` all-or-nothing into `dest`. `Ordinary` submit on
    /// success; the queue is untouched on failure, so nothing is submitted
    /// for it. Touches neither adoption nor any pending load's target.
    pub fn enqueue(
        &mut self,
        dest: PlaylistId,
        batch: Vec<NewQueueEntry>,
    ) -> Result<(Vec<QueueEntryId>, Action), QueueError> {
        let ids = self.state.enqueue(dest, batch)?;
        Ok((ids, self.submit(Urgency::Ordinary)))
    }

    /// Creates a new, empty playlist named `name`. `Ordinary` submit on
    /// success; nothing is submitted on failure.
    pub fn create_playlist(&mut self, name: &str) -> Result<(PlaylistId, Action), PlaylistError> {
        let id = self.state.create_playlist(name)?;
        Ok((id, self.submit(Urgency::Ordinary)))
    }

    /// Renames an existing playlist. `Ordinary` submit on success; nothing is
    /// submitted on failure.
    pub fn rename_playlist(&mut self, id: PlaylistId, name: &str) -> Result<Action, PlaylistError> {
        self.state.rename_playlist(id, name)?;
        Ok(self.submit(Urgency::Ordinary))
    }

    /// `Some(seed)` turns shuffle on, pinning the playlist's *own* cursor
    /// first; `None` turns it off. No engine command either way, so a
    /// playing track keeps playing (M8 §7).
    pub fn set_shuffle(
        &mut self,
        id: PlaylistId,
        seed: Option<u64>,
    ) -> Result<Action, PlaylistError> {
        let playlist = self
            .state
            .playlist_mut(id)
            .ok_or(PlaylistError::Unknown(id))?;
        let first = playlist.queue().active();
        playlist.set_shuffle(seed.map(|seed| Shuffle { seed, first }));
        Ok(self.submit(Urgency::Ordinary))
    }

    /// The queue that holds `id`, in whichever playlist owns it (M8 §5).
    fn owner_queue_mut(&mut self, id: QueueEntryId) -> Option<&mut Queue> {
        let owner = self.state.owner_of(id)?;
        self.state.playlist_mut(owner).map(Playlist::queue_mut)
    }

    /// Moves one entry a step in `direction`. `Ordinary` submit only when the
    /// move actually changed the order (an edge is a no-op, submitting
    /// nothing). Touches neither adoption nor any pending load's target: a
    /// registered `Loaded` still adopts the entry it named, wherever it now
    /// sits.
    pub fn move_entry(
        &mut self,
        id: QueueEntryId,
        direction: Direction,
    ) -> Result<Action, QueueError> {
        let moved = self
            .owner_queue_mut(id)
            .ok_or(QueueError::UnknownEntry(id))?
            .move_entry(id, direction)?;
        Ok(if moved {
            self.submit(Urgency::Ordinary)
        } else {
            Action::None
        })
    }

    /// Whether the adopted load is for `id` (M8 §5). A cursor is only a
    /// remembered entry; this is what ownership means.
    pub fn owns_entry(&self, id: QueueEntryId) -> bool {
        self.adopted
            .is_some_and(|adopted| adopted.target == LoadTarget::Queue(id))
    }

    /// Whether `id` is the playlist that owns the adopted entry (M8 §5).
    pub fn owns_playlist(&self, id: PlaylistId) -> bool {
        match self.adopted.map(|adopted| adopted.target) {
            Some(LoadTarget::Queue(entry)) => self.state.owner_of(entry) == Some(id),
            _ => false,
        }
    }

    /// Removes one entry, wherever it is queued. Unknown `id` is an error
    /// raised before anything changes. Every pending load targeting `id` is
    /// invalidated first, so a `Loaded` already in flight for it can never
    /// resurrect it (M5 §6). Playback is released only when `id` is the
    /// *owned* entry — a cursor alone is not ownership (M8 §5) — and its
    /// checkpoint is then captured through the same gated path
    /// `shutdown_snapshot` uses (`capture_current`); `current_media` and the
    /// checkpoints already on record are left exactly as they are.
    pub fn remove_entry(
        &mut self,
        id: QueueEntryId,
        progress: &Progress,
        now: ClockSample,
    ) -> Result<Removal, QueueError> {
        if self.state.find_entry(id).is_none() {
            return Err(QueueError::UnknownEntry(id));
        }
        self.invalidate_pending(|target| target == LoadTarget::Queue(id));
        let owned = self.owns_entry(id);
        if owned {
            self.release_active(progress, now);
        }
        // `Queue::remove` clears the cursor itself when `id` was the cursor.
        let removed = self
            .owner_queue_mut(id)
            .ok_or(QueueError::UnknownEntry(id))?
            .remove(id)?;
        Ok(Removal {
            action: self.submit(Urgency::Forced),
            stop_playback: owned,
            selection: removed.selection,
        })
    }

    /// Invalidates the pending loads into `id`, before its entries go, and
    /// releases playback only if `id` owns it (M8 §5). Loads for other
    /// playlists are untouched.
    fn vacate(&mut self, id: PlaylistId, progress: &Progress, now: ClockSample) -> bool {
        let entries: Vec<QueueEntryId> = self
            .state
            .playlist(id)
            .map(|playlist| playlist.queue().entries().iter().map(|e| e.id()).collect())
            .unwrap_or_default();
        self.invalidate_pending(
            |target| matches!(target, LoadTarget::Queue(entry) if entries.contains(&entry)),
        );
        let owning = self.owns_playlist(id);
        if owning {
            self.release_active(progress, now);
        }
        owning
    }

    /// Empties one playlist, keeping it. `current_media` and every checkpoint
    /// already on record are left exactly as they are — queue membership does
    /// not pin a checkpoint.
    pub fn clear_playlist(
        &mut self,
        id: PlaylistId,
        progress: &Progress,
        now: ClockSample,
    ) -> Result<Removal, PlaylistError> {
        if self.state.playlist(id).is_none() {
            return Err(PlaylistError::Unknown(id));
        }
        let owning = self.vacate(id, progress, now);
        if let Some(playlist) = self.state.playlist_mut(id) {
            playlist.queue_mut().clear();
        }
        Ok(Removal {
            action: self.submit(Urgency::Forced),
            stop_playback: owning,
            selection: None,
        })
    }

    /// Deletes one playlist. The `LastPlaylist` refusal (P1) precedes
    /// `vacate`, so a refused delete releases nothing. Deleting the playing
    /// playlist moves `playing` to the adjacent one and re-points
    /// `current_media` at its cursor (M8 §5).
    pub fn delete_playlist(
        &mut self,
        id: PlaylistId,
        progress: &Progress,
        now: ClockSample,
    ) -> Result<Removal, PlaylistError> {
        if self.state.playlist(id).is_none() {
            return Err(PlaylistError::Unknown(id));
        }
        if self.state.playlists().len() == 1 {
            return Err(PlaylistError::LastPlaylist);
        }
        let owning = self.vacate(id, progress, now);
        self.state.remove_playlist(id)?;
        Ok(Removal {
            action: self.submit(Urgency::Forced),
            stop_playback: owning,
            selection: None,
        })
    }

    /// Marks every pending load whose target satisfies `matches` as
    /// invalidated, so a `Loaded` that later arrives for it is never adopted
    /// (M5 §6). Shared by `remove_entry` (one queue id) and `vacate` (the
    /// targeted playlist's entries) — `LoadTarget::Legacy` never matches
    /// either caller's predicate, so a legacy load is never invalidated by a
    /// queue mutation.
    fn invalidate_pending(&mut self, matches: impl Fn(LoadTarget) -> bool) {
        for pending in self.pending.values_mut() {
            if matches(pending.target) {
                pending.invalidated = true;
            }
        }
    }

    /// Releases whatever this session currently has adopted: captures its
    /// checkpoint through the same gated path `shutdown_snapshot` uses
    /// (`capture_current`), then clears `adopted` and `last_sample`. Called
    /// only for an owned entry or an owning playlist — the ownership check
    /// lives in its callers (M8 §5).
    fn release_active(&mut self, progress: &Progress, now: ClockSample) {
        self.capture_current(progress, now);
        self.adopted = None;
        self.last_sample = None;
    }

    /// Sets the output volume. `Ordinary` submit — used before an engine
    /// exists to relay it to, and whenever a volume command arrives outside
    /// `observe`'s own `VolumeChanged` handling.
    pub fn set_volume(&mut self, volume: Volume) -> Action {
        self.state.set_volume(volume);
        self.submit(Urgency::Ordinary)
    }

    /// Applies `update` to every queue entry whose media is `media`,
    /// replacing only the fields `update` actually carries — a field left
    /// `None` leaves the entry's existing value alone. A carried field is
    /// written only when it actually differs from the entry's current value:
    /// `Ordinary` submit only when at least one field on at least one entry
    /// truly changed; otherwise `Action::None`, so a caller re-sending
    /// metadata it already wrote (the plan's metadata-enrichment workers do
    /// this repeatedly) does not push an empty write on every call.
    pub fn update_display(&mut self, media: &MediaId, update: DisplayUpdate) -> Action {
        if update == DisplayUpdate::default() {
            return Action::None;
        }
        let ids: Vec<QueueEntryId> = self
            .state
            .playlists()
            .iter()
            .flat_map(|playlist| playlist.queue().entries())
            .filter(|entry| entry.media() == media)
            .map(|entry| entry.id())
            .collect();
        let mut changed = false;
        for id in ids {
            let Some(entry) = self.owner_queue_mut(id).and_then(|queue| queue.get_mut(id)) else {
                continue;
            };
            let display = entry.display_mut();
            if let Some(title) = &update.title
                && display.title.as_ref() != Some(title)
            {
                display.title = Some(title.clone());
                changed = true;
            }
            if let Some(artist) = &update.artist
                && display.artist.as_ref() != Some(artist)
            {
                display.artist = Some(artist.clone());
                changed = true;
            }
            if let Some(album) = &update.album
                && display.album.as_ref() != Some(album)
            {
                display.album = Some(album.clone());
                changed = true;
            }
            if let Some(year) = &update.year
                && display.year.as_ref() != Some(year)
            {
                display.year = Some(year.clone());
                changed = true;
            }
            if let Some(duration) = update.duration
                && display.duration != Some(duration)
            {
                display.duration = Some(duration);
                changed = true;
            }
        }
        if changed {
            self.submit(Urgency::Ordinary)
        } else {
            Action::None
        }
    }

    /// Replaces one entry's podcast fallback URL. Unknown `id` is an error;
    /// an entry whose source is not `QueueSource::Podcast` is refused by
    /// `set_source`'s own `SourceMismatch` — there is no fallback URL to
    /// compare against on a local file or a plain remote URL, so that is the
    /// natural answer rather than a silent no-op. A matching URL on an
    /// existing podcast source is a no-op (`Action::None`); a differing one
    /// replaces the source and submits `Ordinary`. `MediaId` and every
    /// checkpoint are untouched either way.
    pub fn update_podcast_fallback(
        &mut self,
        id: QueueEntryId,
        url: Url,
    ) -> Result<Action, QueueError> {
        let entry = self
            .owner_queue_mut(id)
            .and_then(|queue| queue.get_mut(id))
            .ok_or(QueueError::UnknownEntry(id))?;
        if let QueueSource::Podcast { fallback } = entry.source()
            && *fallback == url
        {
            return Ok(Action::None);
        }
        entry.set_source(QueueSource::Podcast { fallback: url })?;
        Ok(self.submit(Urgency::Ordinary))
    }

    /// Copies a decoder-reported title, artist, album and duration into the
    /// queue entry a `Loaded` just adopted, each only when the decoder
    /// actually reported it: `MediaMetadata`'s absent fields must never blank
    /// out what the entry already displayed.
    fn absorb_load_metadata(&mut self, id: QueueEntryId, metadata: &MediaMetadata) {
        let Some(entry) = self.owner_queue_mut(id).and_then(|queue| queue.get_mut(id)) else {
            return;
        };
        let display = entry.display_mut();
        if let Some(title) = &metadata.title {
            display.title = Some(title.clone());
        }
        if let Some(artist) = &metadata.artist {
            display.artist = Some(artist.clone());
        }
        if let Some(album) = &metadata.album {
            display.album = Some(album.clone());
        }
        if let Some(year) = &metadata.year {
            display.year = Some(year.clone());
        }
        if let Some(duration) = metadata.duration {
            display.duration = Some(DisplayDuration {
                value: duration,
                source: DurationSource::Decoded(metadata.duration_provenance),
            });
        }
    }

    // ------------------------------------------------------------- observe

    pub fn observe(&mut self, event: &PlaybackEvent, now: ClockSample) -> Action {
        let accepted = self.accepts_media_event(event);

        // Load outcomes are processed before every ordinary handler, and
        // their registration is retired first — even for a stale or unknown
        // one (M5 §6). `accepted` above was computed while that registration
        // still stood, which is what lets a `Loaded` that fails
        // `registered_target` be told apart from one this session never
        // asked for at all.
        if let Some(request) = event.load_outcome() {
            let Some(pending) = self.pending.remove(&request) else {
                // Unknown or duplicate: changes nothing at all, including
                // revision tracking, `last_loaded`, checkpoint fields and
                // advancement.
                return Action::None;
            };
            let event_rev = event.session_rev();
            if event_rev < self.latest_engine_rev {
                // Known, but superseded by a newer revision already
                // recognized: retiring the registration above is this
                // outcome's whole effect. It must never stop newer playback.
                return Action::None;
            }
            return match event {
                PlaybackEvent::Loaded {
                    media,
                    position,
                    disposition,
                    metadata,
                    capabilities,
                    ..
                } => {
                    self.latest_engine_rev = event_rev;
                    self.last_loaded = Some(request);
                    if accepted {
                        self.session_rev = event_rev;
                        self.adopt_loaded(
                            request,
                            pending.target,
                            media,
                            *position,
                            disposition,
                            metadata,
                            capabilities,
                            now,
                        )
                    } else {
                        // The target was invalidated, or the media no longer
                        // matches: the engine is playing something this
                        // session will never adopt. The previous adopted
                        // checkpoint state is left exactly as it stands.
                        self.stop_requested = true;
                        Action::None
                    }
                }
                PlaybackEvent::Failed { .. } | PlaybackEvent::LoadCancelled { .. } => {
                    self.latest_engine_rev = event_rev;
                    self.last_loaded = None;
                    Action::None
                }
                _ => unreachable!(
                    "PlaybackEvent::load_outcome() is Some only for Loaded, Failed and LoadCancelled"
                ),
            };
        }

        // A `Loading` announcement for a load this session has registered
        // retires the previous `last_loaded` before that new load's own
        // outcome does the same — ownership of the token now in flight is
        // undecided until it resolves, so nothing may act as though the
        // previous adopted load is still the most recent one heard from.
        // Ordinary, so it may be dropped under backlog: losing it is safe
        // because the protected outcome that follows performs the identical
        // update (M5 §6).
        if let PlaybackEvent::StateChanged {
            state: PlaybackState::Loading,
            request: Some(request),
            ..
        } = event
            && self.pending.contains_key(request)
        {
            // Not history, but the engine really is loading: without this,
            // `self.playback` would still read whatever it was for the
            // *previous* adopted media, and the launch `Paused` that follows
            // this new load would misread `previous == Playing` and raise a
            // pending force nothing asked for.
            self.playback = PlaybackState::Loading;
            let event_rev = event.session_rev();
            if event_rev >= self.latest_engine_rev {
                self.latest_engine_rev = event_rev;
                self.last_loaded = None;
            }
            return Action::None;
        }

        // Profile-wide: bypasses the media-ownership gate below entirely.
        if let PlaybackEvent::VolumeChanged { volume, .. } = event {
            self.state.set_volume(*volume);
            return self.submit(Urgency::Ordinary);
        }

        // Every remaining event is media-specific and must own the adopted
        // token before it may touch revision, completion, provenance,
        // protection, pending force or history (D20).
        if !accepted {
            return Action::None;
        }
        let event_rev = event.session_rev();
        self.latest_engine_rev = event_rev;
        self.session_rev = event_rev;
        // A newer revision re-keys a pending force rather than dropping it:
        // `rebuild` bumps the revision on device recovery with the position
        // continuous across it, so the force is still answerable — and dropping
        // it would lose a real pause for good, since no ordinary trigger fires
        // while paused. `Loaded` is the one exception, retired in `on_loaded`.
        if self.pending_force.is_some() {
            self.pending_force = Some(event_rev);
        }

        match event {
            PlaybackEvent::StateChanged { state, .. } => self.on_state(*state, now),
            PlaybackEvent::SeekCompleted { provenance, .. } => {
                self.resolve_target();
                self.established = true;
                self.completed = false;
                // Re-establishment: the same (adopted token, session_rev) can
                // legitimately complete again after this landing — a
                // finished track can be sought back into and replayed to the
                // end without either changing (D3's key would otherwise
                // treat the second EndOfTrack as the first's duplicate).
                self.completion_seen = None;
                self.position_provenance = *provenance;
                if *provenance == PositionProvenance::Established {
                    // An established user seek (§10): the listener steered
                    // the position themselves and the decoder confirmed it,
                    // so whatever fallback zero was protected no longer
                    // needs protecting. One of exactly two acts (§4.4) that
                    // earns the right to overwrite an established checkpoint.
                    self.protected = None;
                }
                // An estimated landing is still a real landing the listener
                // can keep playing from, and still deserves a checkpoint
                // (§4.2) — just not this one: `protected` stays in force
                // when set, and the deferred write this force raises will
                // route through `record_current_estimated` once
                // `position_provenance` above is read back at the next tick.
                self.pending_force = Some(event_rev);
                Action::None
            }
            // This event exists only because nothing else lets the policy tell
            // an explicit restart from any other establishment (G1) — which is
            // exactly the distinction clearing protection needs, so it clears
            // it and otherwise behaves like `SeekCompleted`. Despite its name,
            // a restart's landing is not always `Established`: reusing an
            // already-open decoder seeks like any other, and on MP3 that seek
            // can still land `Estimated` (see the emission site's comment) —
            // so this reads the event's own `provenance` exactly as
            // `SeekCompleted` does, rather than assuming.
            PlaybackEvent::RestartEstablished { provenance, .. } => {
                self.resolve_target();
                self.established = true;
                self.completed = false;
                // See the identical note on `SeekCompleted`: an explicit
                // restart is exactly the "finish, Home, play again" sequence
                // this dedup must not suppress a second time.
                self.completion_seen = None;
                self.position_provenance = *provenance;
                if *provenance == PositionProvenance::Established {
                    self.protected = None;
                }
                self.pending_force = Some(event_rev);
                Action::None
            }
            // One of the two positions that never come from `Progress` (D6),
            // so the establishment gate does not apply: the target is the
            // listener's own, not a number the engine has yet to validate.
            PlaybackEvent::SeekTargetStored { target, .. } => {
                self.outstanding_target = Some(*target);
                self.established = true;
                // §12 has a seek clear completion, and a stopped seek is the
                // same listener intent one step earlier: a target persisted
                // beside `completed` would be thrown away by the resume it
                // exists to steer.
                self.completed = false;
                // See the identical note on `SeekCompleted`.
                self.completion_seen = None;
                // R5: a stored target is arithmetic on whatever position was
                // current when the seek was accepted — target = that position
                // plus or minus a listener-chosen delta — and arithmetic
                // cannot make an unconfirmed number confirmed. So the target
                // inherits that position's provenance rather than being
                // treated as established by virtue of being the listener's
                // own number. `self.position_provenance` already tracks
                // exactly that position: every tick and every landing event
                // keeps it current, and nothing has touched it since.
                if self.position_provenance == PositionProvenance::Established {
                    self.record_current(*target, now);
                } else {
                    self.record_current_estimated(*target, now);
                }
                self.submit(Urgency::Forced)
            }
            // The other one (D6): the end of the track is a position the engine
            // reached, carried by the event that reports it.
            PlaybackEvent::EndOfTrack {
                position,
                provenance,
                ..
            } => {
                // `accepted` above guarantees `self.adopted` is `Some` here.
                let Some(adopted) = self.adopted else {
                    return Action::None;
                };
                let key = (adopted.request, self.session_rev);
                if self.completion_seen == Some(key) {
                    // Deduplicated *before* any checkpoint handling (§6):
                    // neither history nor advancement moves twice for one
                    // completion.
                    return Action::None;
                }
                self.completion_seen = Some(key);

                self.resolve_target();
                self.established = true;
                self.completed = true;
                self.position_provenance = *provenance;
                if *provenance == PositionProvenance::Established {
                    // Cleared before `record_current`, not after: verified
                    // completion reached from an established timeline is
                    // itself the thing worth writing, and clearing afterwards
                    // would have gated that very write (§10).
                    self.protected = None;
                    self.record_current(*position, now);
                } else {
                    // R4: the engine computes this terminal position as the
                    // landed anchor plus decoded frames, so an estimated
                    // anchor yields an estimated terminal position — HTTP
                    // verification establishes only that the *body*
                    // finished, not that the anchor was right. `completed`
                    // is still set above, because reaching the end is real
                    // evidence either way, but the position it is written
                    // beside must not promote to `position`, and `protected`
                    // is deliberately left untouched: an estimated
                    // completion retains checkpoint protection exactly as it
                    // retains estimated provenance (§4.4) — this is not a
                    // third clearing exit.
                    self.record_current_estimated(*position, now);
                }
                // Both provenances advance (§6); only a queue target has
                // anywhere to advance to.
                if let LoadTarget::Queue(id) = adopted.target {
                    let next = self
                        .state
                        .owner_of(id)
                        .and_then(|owner| self.state.playlist(owner))
                        .and_then(|playlist| playlist.neighbor(id, Direction::Down));
                    self.advance = Some(next.map_or(Advance::EndOfQueue, Advance::Next));
                }
                self.submit(Urgency::Forced)
            }
            // §10: "Capability changes alone never delete, clear or replace
            // checkpoints." A server that starts advertising ranges mid-session
            // must not be able to discard a protected entry by saying so —
            // this is otherwise `Action::None` and falls through to the
            // catch-all below for exactly that reason: there is nothing else
            // here to touch. M7 §9's gate is the one exception, and only in
            // the direction that protects a checkpoint further: evidence that
            // arrives late that this media is actually live must shut the
            // gate, but nothing here may reopen one `on_loaded` already shut.
            PlaybackEvent::CapabilitiesChanged { capabilities, .. } => {
                if capabilities.continuity == Continuity::Indefinite {
                    self.checkpointable = false;
                }
                Action::None
            }
            // A cancelled seek commits no target — `resolve_target()` is
            // deliberately not called here, because the stored target it
            // would discard belongs to a stopped seek that is still
            // outstanding, not to this one.
            PlaybackEvent::SeekCancelled { .. } => Action::None,
            _ => Action::None,
        }
    }

    /// The whole of `Loaded`'s adoption: the checkpoint/identity mutations
    /// `on_loaded` still owns, then the queue and metadata moves, then the
    /// submission — built only once every earlier mutation has landed, so it
    /// is never a snapshot cloned before them (D19).
    #[allow(clippy::too_many_arguments)] // one call site, unpacking `Loaded`'s own fields.
    fn adopt_loaded(
        &mut self,
        request: LoadRequestId,
        target: LoadTarget,
        media: &MediaId,
        position: Duration,
        disposition: &StartDisposition,
        metadata: &MediaMetadata,
        capabilities: &MediaCapabilities,
        now: ClockSample,
    ) -> Action {
        let previous = (self.state.playing(), self.state.queue().active());
        let switched_media = self.on_loaded(media, position, disposition, capabilities, now);
        match target {
            LoadTarget::Queue(id) => {
                // Validated against its owner a moment ago in `observe`.
                if let Some(owner) = self.state.owner_of(id) {
                    self.state.set_playing(owner);
                }
                let _ = self.state.queue_mut().set_active(Some(id));
                self.absorb_load_metadata(id, metadata);
            }
            // A legacy load belongs to no playlist: it clears the playing
            // playlist's cursor, as it cleared the one queue's before M8.
            LoadTarget::Legacy => {
                let _ = self.state.queue_mut().set_active(None);
            }
        }
        self.adopted = Some(AdoptedLoad { request, target });
        self.adopted_rev_floor = self.session_rev;
        let now_at = (self.state.playing(), self.state.queue().active());
        if switched_media || previous != now_at {
            self.submit(Urgency::Forced)
        } else {
            Action::None
        }
    }

    pub fn tick(&mut self, progress: &Progress, now: ClockSample) -> Action {
        if !self.accepts_live_sample(progress) {
            return Action::None;
        }
        let Some(media) = self.current_media.clone() else {
            return Action::None;
        };
        self.last_sample = Some(Sample {
            session_rev: progress.session_rev,
            media,
            position: progress.position,
            provenance: progress.provenance,
        });
        // Kept current on every tick, whether or not it ends up due for a
        // capture: `checkpoint_from_progress` (below) and `record_outgoing`
        // both read this to route a write, and both can fire on a pass this
        // method never reaches the capture-interval check on.
        self.position_provenance = progress.provenance;

        // The sample the pending force has been waiting for. §3's pass ordering
        // makes it newer than the transition that raised the force, so this is
        // a resolution rather than a delay (D13).
        if self.pending_force == Some(progress.session_rev) {
            self.pending_force = None;
            // The force is answered by recording nothing when the gate is shut:
            // there is no validated position for it to checkpoint.
            if !self.checkpoint_from_progress(progress.position, now) {
                return Action::None;
            }
            self.last_capture = Some(now.monotonic);
            return self.submit(Urgency::Forced);
        }

        if self.playback != PlaybackState::Playing {
            return Action::None;
        }
        let due = self
            .last_capture
            .is_none_or(|last| now.monotonic.duration_since(last) >= CAPTURE_INTERVAL);
        if !due {
            return Action::None;
        }
        // `playback` can still read `Playing` here across a media switch, since
        // the `StateChanged{Loading}` that precedes `Loaded` is an ordinary
        // event the engine may drop under backlog. The gate is what makes the
        // interval harmless in that window: nothing has established the
        // incoming media, so its sampled position is not a checkpoint.
        if !self.checkpoint_from_progress(progress.position, now) {
            // While protected (§10), this is the arm that answers every due
            // tick: `last_capture` is not advanced below, so `due` stays true
            // and this re-evaluates on every subsequent tick rather than once
            // per `CAPTURE_INTERVAL`. Harmless — it is a plain comparison
            // followed by two guard checks, with no write and no submission —
            // just not the interval's usual cadence.
            return Action::None;
        }
        self.last_capture = Some(now.monotonic);
        self.submit(Urgency::Ordinary)
    }

    /// Whether `progress` is a live sample the policy may act on directly:
    /// its revision and media match adopted playback, its own `load` token
    /// names the adopted request, and the most recent `Loaded` this session
    /// observed still names that same request too (§3, D20). `tick` and
    /// `capture_current` (which `shutdown_snapshot` and `remove_entry` share)
    /// both read this rather than each keeping their own copy of the check.
    fn accepts_live_sample(&self, progress: &Progress) -> bool {
        let Some(adopted) = self.adopted else {
            return false;
        };
        self.last_loaded == Some(adopted.request)
            && progress.session_rev == self.session_rev
            && progress.load == Some(adopted.request)
            && progress.media.as_ref() == self.current_media.as_ref()
    }

    /// Checkpoint a position that came from `Progress`, and report whether one
    /// was recorded. Every such position goes through here, which is what makes
    /// the two rules that qualify them impossible to forget: an outstanding
    /// stopped-seek target supersedes the sample (D17), and a sample taken
    /// before anything established is one the engine never validated (D20) —
    /// §11 resumes a completed entry at zero, and `load()` reports `Loaded`
    /// before it opens the device, so an ungated sample writes that zero over
    /// the position D1 retains. The two positions that arrive on events of their
    /// own do not come through here and are not gated (D6).
    fn checkpoint_from_progress(&mut self, sampled: Duration, now: ClockSample) -> bool {
        if !self.checkpointable {
            return false;
        }
        if !self.established {
            return false;
        }
        // Mirrors the `established` check just above rather than leaving this
        // to `record_current`'s own gate: `record_current` would still refuse
        // to write, but the caller (`tick`, `shutdown_snapshot`) would read a
        // bare `true` as "recorded" and resubmit an unchanged state anyway —
        // every capture interval, for as long as the run stays protected
        // (§10). `CAPTURE_INTERVAL` exceeds the writer's coalescing window, so
        // those would not merge: each is its own read-classify-replace write
        // of byte-identical content. Returning `false` here is what makes the
        // caller answer `Action::None` instead.
        if self.protected.is_some() {
            return false;
        }
        let position = self.position_for(sampled);
        // §4.2: route on the position's own provenance, never on whether an
        // established checkpoint already exists for this media — the two
        // write rules (never overwrite one, never bootstrap one) collapse
        // into the same routing decision this way, with nothing to forget.
        if self.position_provenance == PositionProvenance::Established {
            self.record_current(position, now);
        } else {
            self.record_current_estimated(position, now);
        }
        true
    }

    fn on_state(&mut self, state: PlaybackState, now: ClockSample) -> Action {
        let previous = self.playback;
        self.playback = state;
        match state {
            PlaybackState::Playing => {
                // The engine can resolve a stored target by *discarding* it:
                // `restart()` clears it, seeks to zero and lands here with no
                // SeekCompleted ever emitted (D17).
                self.resolve_target();
                self.established = true;
                // §12: a successful establishment after a completed state
                // clears it. Persistence restoration alone does not.
                self.completed = false;
                // See the identical note on `SeekCompleted`: `Playing`
                // landing here (a bare Play from Ended, with no seek or
                // restart event of its own) is itself a re-establishment.
                self.completion_seen = None;
                self.last_capture = Some(now.monotonic);
            }
            // A pause that interrupts no playback is not a checkpoint: every
            // launch emits one before the queued Play is dispatched.
            PlaybackState::Paused if previous == PlaybackState::Playing => {
                self.pending_force = Some(self.session_rev);
            }
            // `do_stop` returns early from Idle, Stopped and Failed, so this
            // event only exists when something was actually running.
            PlaybackState::Stopped => {
                self.pending_force = Some(self.session_rev);
            }
            _ => {}
        }
        Action::None
    }

    /// A `Loaded` for a different media is **one** snapshot: the outgoing entry
    /// is recorded from `last_sample` and `current_media` moves in a single
    /// mutation. A keep-latest slot cannot promise that an intermediate
    /// submission reaches disk, so "flush, then move" is unenforceable — and
    /// unnecessary, since the snapshot is the whole state.
    ///
    /// Returns whether the incoming media differs from the outgoing one.
    /// `adopt_loaded` is the only caller and owns the submission this used to
    /// build itself — the queue and metadata moves it makes belong in the
    /// same snapshot, so the decision of *whether* to submit had to move
    /// outward with them.
    fn on_loaded(
        &mut self,
        media: &MediaId,
        position: Duration,
        disposition: &StartDisposition,
        capabilities: &MediaCapabilities,
        now: ClockSample,
    ) -> bool {
        let switching = self.current_media.as_ref() != Some(media);

        // First, while every per-media field still describes the media on its
        // way out — `protected` among them, so this reads the outgoing media's
        // protection, not the incoming one's.
        if switching {
            self.record_outgoing(now);
        }

        // Only now: a force raised against the previous media cannot answer for
        // this one, and none of these carry across a load.
        self.pending_force = None;
        self.resolve_target();
        self.established = false;
        self.last_capture = None;
        // A fresh load re-establishes the absolute position for the incoming
        // media (§4.4's "fresh load" exit): nothing has claimed otherwise for
        // it yet, and the write gate below only starts writing once
        // `established` above goes true again, so this is read no earlier
        // than the first tick or landing event that follows.
        self.position_provenance = PositionProvenance::Established;
        // Set from the disposition this `Loaded` carries, not from a later
        // warning: this is what puts protection in force before any `Playing`
        // or progress event for the incoming media can be observed (§5).
        // `ResumeUnavailable` is the only disposition that sets it — every
        // other one, including a plain reload of the same media, clears it.
        self.protected = match disposition {
            StartDisposition::ResumeUnavailable { retained } => Some(*retained),
            // A resumed estimate lands the listener at the estimated location
            // itself, not a fallback zero — there is no earlier point to
            // protect the way `ResumeUnavailable` protects one. What keeps
            // `position` from being overwritten by the estimated playback
            // that follows is the write-routing gate above
            // (`position_provenance`, §4.2), not this field.
            StartDisposition::ResumedEstimated { .. } => None,
            _ => None,
        };

        let completed = self.state.completed_for(media);
        self.adopt_media(media.clone(), completed);
        self.last_sample = Some(Sample {
            session_rev: self.session_rev,
            media: media.clone(),
            position,
            provenance: self.position_provenance,
        });

        // Last, deliberately: everything above that writes for the outgoing
        // media has already run under the outgoing media's gate.
        self.checkpointable = capabilities.continuity != Continuity::Indefinite;

        switching
    }

    /// The entry for the media on its way out, written from the position the
    /// session retained for it. §3: `load()` overwrites the engine's own
    /// position with `start_at` before anything publishes, so `last_sample` is
    /// the only place that position still exists.
    ///
    /// **Call this before the per-media fields reset.** `completed`,
    /// `established`, `outstanding_target`, `protected` and
    /// `position_provenance` are all read here, and all five describe the
    /// outgoing media only until `on_loaded` resets them — read afterwards
    /// they describe the incoming one, and the mistake would be silent. They
    /// are read nowhere else in `on_loaded`.
    ///
    /// Three things make a retained sample not worth writing. A completed
    /// entry's position is the one `EndOfTrack` recorded and the sample can
    /// only be behind it (D1). A sample for a media nothing established is the
    /// zero §11 resumes at, not a position the engine ever validated — `load()`
    /// reports `Loaded` before it opens the device, so switching away from a
    /// launch that failed would otherwise carry that zero out as the media's
    /// final word (D20). And a protected entry (§10) must not be overwritten
    /// by this path either: this is *not* `record_current`, it writes the
    /// previous media's entry straight from `last_sample`, so a gate placed
    /// only in `record_current` would leave a media switch free to overwrite
    /// the very checkpoint protection exists to keep.
    ///
    /// A fourth, added by Task 6: the outgoing media's own `position_provenance`
    /// gates *which* entry gets written, never whether one does — this is the
    /// same reasoning M3 already applied to `protected`, carried to the write
    /// rules of §4.2. `checkpoint_from_progress` cannot cover this path:
    /// a media switch writes the previous media's entry straight from
    /// `last_sample`, never through `record_current`, so a routing decision
    /// placed only there would leave a switch away from an estimated landing
    /// free to promote it to `position` anyway.
    ///
    /// A fifth, added by M7 §9: `self.checkpointable` still describes the
    /// *outgoing* media here, since `on_loaded` calls this before resetting
    /// it — a station's listening time must not be written just because
    /// something else is loading next. This writes straight to `self.state`
    /// rather than through `record_current`/`record_current_estimated`, so it
    /// carries the same gate on its own rather than inheriting theirs.
    fn record_outgoing(&mut self, now: ClockSample) {
        if !self.checkpointable {
            return;
        }
        let Some(previous) = self.last_sample.take() else {
            return;
        };
        if self.protected.is_some() {
            return;
        }
        if self.completed || !self.established {
            return;
        }
        // A stopped seek's target is the outgoing media's real position, and
        // resolving it first would write the pre-seek sample back over it (D17).
        let position = self.position_for(previous.position);
        if self.position_provenance == PositionProvenance::Established {
            self.state.record(
                &PlaybackCheckpoint {
                    media: previous.media,
                    position,
                    updated_at: now.wall,
                },
                false,
            );
        } else {
            self.state
                .record_estimated(previous.media, position, now.wall, false);
        }
    }

    /// The only write path for the current media and its completion. Nothing in
    /// the type system keeps two sibling fields in step, so this method exists
    /// to make sure no edit can move `current_media` without deciding
    /// `completed` in the same breath.
    fn adopt_media(&mut self, media: MediaId, completed: bool) {
        self.current_media = Some(media.clone());
        self.state.set_current_media(media);
        self.completed = completed;
    }

    /// The engine has taken the stopped seek's target somewhere the sampled
    /// position can be trusted again — by adopting it, or by discarding it.
    fn resolve_target(&mut self) {
        self.outstanding_target = None;
    }

    /// A stopped seek stores a target and leaves the engine's position where it
    /// was, so any position-derived checkpoint that follows would write the
    /// pre-seek value back over it (D17).
    fn position_for(&self, sampled: Duration) -> Duration {
        self.outstanding_target.unwrap_or(sampled)
    }

    /// The checkpoint for whatever media is current, captured from a live
    /// sample when one is available (`accepts_live_sample`) and from the
    /// retained `last_sample` otherwise — reading that sample's own
    /// provenance rather than assuming it matches whatever this session most
    /// recently tracked. Shared by `shutdown_snapshot` and `remove_entry`
    /// (Ruling 1): both need exactly this capture, and duplicating it would
    /// let the two drift apart.
    fn capture_current(&mut self, progress: &Progress, now: ClockSample) {
        let Some(media) = self.current_media.clone() else {
            return;
        };
        let sampled = if self.accepts_live_sample(progress) {
            // The live sample is fresher than anything `tick` last recorded,
            // so its provenance is read back too — the same field every
            // write path routes on. The `last_sample` fallback below has no
            // fresher provenance to offer than what the last accepted tick or
            // landing event already left in place, so it is left untouched.
            self.position_provenance = progress.provenance;
            Some(progress.position)
        } else {
            let session_rev = self.session_rev;
            let fallback = self.last_sample.as_ref().and_then(|sample| {
                (sample.session_rev == session_rev && sample.media == media)
                    .then_some((sample.position, sample.provenance))
            });
            match fallback {
                Some((position, provenance)) => {
                    self.position_provenance = provenance;
                    Some(position)
                }
                None => None,
            }
        };
        if let Some(sampled) = sampled {
            self.checkpoint_from_progress(sampled, now);
        }
    }

    /// The final snapshot. `volume` and `current_media` are written whatever
    /// happened; the position goes through the same gate every other sampled
    /// position does, and is never taken from a session the policy was not
    /// tracking.
    pub fn shutdown_snapshot(&mut self, progress: &Progress, now: ClockSample) -> PersistedState {
        self.capture_current(progress, now);
        self.state.clone()
    }

    /// The whole of the shutdown handoff that belongs to the policy: replay the
    /// events the application never drained, then take the forced snapshot
    /// (D19). Both halves in one place, because each without the other is a
    /// lost checkpoint — the replay is what makes the snapshot one taken from a
    /// session that has seen everything the run produced.
    ///
    /// The replays are reconciliation rather than submission: whatever they
    /// would have submitted on their own is superseded by the snapshot this
    /// returns. Every replayed event and the final snapshot both route
    /// through the same ownership gates `observe`, `tick` and
    /// `shutdown_snapshot` already apply (D20) — reconciliation is not a
    /// second, looser path into history.
    ///
    /// The engine-facing half — the shutdown interrupt and the `join` that
    /// produces the report — stays at the call site: it touches the handle,
    /// not the policy.
    pub fn reconcile_shutdown(
        &mut self,
        report: &ShutdownReport,
        now: ClockSample,
    ) -> PersistedState {
        for event in &report.events {
            let _ = self.observe(event, now);
        }
        self.shutdown_snapshot(&report.progress, now)
    }

    /// The write path for the current media's *established* checkpoint: the
    /// periodic 5 s capture, the pause and stop forces, the resolved shutdown
    /// snapshot and an established `SeekTargetStored` all reach the stored
    /// state through here, so gating here alone covers all of them at once.
    /// It does **not** cover `record_outgoing`: that path writes the
    /// *previous* media's entry from `last_sample` on a media switch, never
    /// through this function, so it carries the identical gate on its own
    /// (§10). Every call site decides `Established` vs. `Estimated` first
    /// (§4.2) — this function itself has no branch on provenance, because a
    /// caller only reaches it once that decision already went one way.
    fn record_current(&mut self, position: Duration, now: ClockSample) {
        if !self.checkpointable {
            return;
        }
        if self.protected.is_some() {
            return;
        }
        let Some(media) = self.current_media.clone() else {
            return;
        };
        let completed = self.completed;
        self.state.record(
            &PlaybackCheckpoint {
                media,
                position,
                updated_at: now.wall,
            },
            completed,
        );
    }

    /// The estimated-write counterpart to `record_current` (§4.2): every call
    /// site that reaches this one instead has already read
    /// `position_provenance == Estimated`. Writes only `estimated` — never
    /// `position`, whether or not an established checkpoint already exists
    /// for this media, which is what makes the two write rules ("never
    /// overwrite", "never bootstrap") one code path instead of two that could
    /// drift apart. Carries the identical `protected` gate `record_current`
    /// does, for the same reason: a caller that already checked `protected`
    /// itself (`checkpoint_from_progress`) and one that has not
    /// (`EndOfTrack`'s handler, which must decide *before* touching
    /// `protected` — see its own comment) both reach here safely either way.
    fn record_current_estimated(&mut self, position: Duration, now: ClockSample) {
        if !self.checkpointable {
            return;
        }
        if self.protected.is_some() {
            return;
        }
        let Some(media) = self.current_media.clone() else {
            return;
        };
        let completed = self.completed;
        self.state
            .record_estimated(media, position, now.wall, completed);
    }

    /// `resume_intent_for(self.state.entry_for(media))`, defaulting to a
    /// start of zero for a media with no stored entry at all — no entry is
    /// not itself a resume intent to resolve, it is the absence of one.
    pub fn resume_intent(&self, media: &MediaId) -> ResumeIntent {
        resume_intent_for(self.state.entry_for(media))
            .unwrap_or(ResumeIntent::StartAt(Duration::ZERO))
    }

    fn submit(&self, urgency: Urgency) -> Action {
        Action::Submit {
            state: self.state.clone(),
            urgency,
        }
    }
}

/// Builds the worker-facing resume intent from a stored checkpoint entry,
/// §4.3's estimated-preference rule folded in beside the established path
/// left unchanged.
///
/// A completed entry never reaches `restart_preference` — its own doc says
/// so: the caller's concern, and calling it anyway would let a stray
/// estimate stored before completion redirect a replay that D1 already
/// says starts over at zero regardless. So a completed entry always goes
/// through `resume_candidate` exactly as it did before this function
/// existed, and only a live, uncompleted entry's `estimated` field is ever
/// consulted.
///
/// This is where §4.3 actually gets wired into the load path (Task 6's fix
/// round 1): `restart_preference`'s pure preference decision — implemented
/// and tested since Task 6 itself — had no production caller until this
/// function. `decide_resume` is deliberately not consulted here for the
/// estimate branch: §4.3 is a preference between two already-known
/// locations, not a duration-validated choice, and running the estimate
/// through duration validation would be inventing a rule the design doc
/// does not state. `decide_resume` keeps governing the established
/// position exactly as before wherever that path is actually taken — the
/// `resume_candidate` branch below, reached whenever there is no estimate
/// to prefer.
pub fn resume_intent_for(entry: Option<&PersistedCheckpoint>) -> Option<ResumeIntent> {
    let entry = entry?;
    if entry.completed {
        return resume_candidate(entry.position, entry.completed).map(ResumeIntent::Candidate);
    }
    match restart_preference(entry.position, entry.estimated) {
        Some(preference) => Some(ResumeIntent::EstimatedCandidate {
            target: preference.target,
            established: preference.established,
        }),
        None => resume_candidate(entry.position, entry.completed).map(ResumeIntent::Candidate),
    }
}
