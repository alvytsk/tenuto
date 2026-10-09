use std::time::Duration;

use crate::http::error::RemoteFailure;
use crate::media::capabilities::MediaCapabilities;
use crate::media::id::MediaId;
use crate::media::metadata::MediaMetadata;

use super::command::LoadRequestId;
use super::state::PlaybackState;
use super::timeline::PositionQuality;
use crate::media::provenance::PositionProvenance;
use crate::volume::Volume;

/// What a load actually did with its resume intent (§8). Carried on `Loaded`
/// rather than announced as a separate event, so a policy that must act on it
/// — lifting checkpoint protection, say — can do so before any `Playing` or
/// progress event has been observed (§5), and so the reserve arithmetic in
/// `engine.rs` never has to budget a fifth event for the widest command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartDisposition {
    /// No candidate, or one that resolved to zero.
    Fresh,
    /// The candidate's position was established by the decoder.
    Resumed,
    /// The entry was complete; M2's replay policy starts it over.
    CompletedReplay,
    /// A positive candidate could not be established, because this source
    /// cannot seek. Playback starts at zero and the entry is protected (§10).
    ResumeUnavailable { retained: Duration },
    /// The candidate that won was an `estimated` location, preferred over
    /// `position` as the listener's most recent expressed intent (§4.3).
    /// `established` carries the fallback `position` that was also on
    /// record, when one existed - `None` for an entry that only ever
    /// carried an estimate (R8), which the application must render without
    /// implying a fallback that does not exist. Distinct from `Resumed`:
    /// nothing here was decoder-confirmed by the resume decision itself,
    /// only by whatever landing the seek that follows actually produces.
    ResumedEstimated { established: Option<Duration> },
}

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    Loaded {
        session_rev: u64,
        /// The token the `Load` that produced this outcome carried. Echoed,
        /// never interpreted (M5 §6).
        request: LoadRequestId,
        media: MediaId,
        metadata: MediaMetadata,
        capabilities: MediaCapabilities,
        /// Where the load actually landed after its refined seek to `start_at`.
        /// Without it the application cannot report where a resume landed and
        /// would render 00:00:00 after one (D8).
        position: Duration,
        /// What the load did with its resume intent, and why. See
        /// `StartDisposition`.
        disposition: StartDisposition,
    },
    StateChanged {
        session_rev: u64,
        state: PlaybackState,
        /// `Some` only when `state == Loading`, carrying that load's token
        /// (Decision 2, M5 §6). Every other state carries `None`.
        request: Option<LoadRequestId>,
    },
    SeekCompleted {
        session_rev: u64,
        requested: Duration,
        actual: Duration,
        refinement_truncated: bool,
        /// Whether `actual` is a decoder-confirmed landing or a byte-offset
        /// estimate (§3). A real landing either way — playback continues
        /// correctly from it — but not, when `Estimated`, a measurement.
        provenance: PositionProvenance,
    },
    /// A seek accepted while stopped. Deliberately not `SeekCompleted`: the
    /// target is unvalidated until the decoder opens.
    SeekTargetStored {
        session_rev: u64,
        target: Duration,
    },
    SeekRejected {
        session_rev: u64,
        reason: String,
    },
    /// An accepted seek that a stop or shutdown cancelled before it committed.
    /// Terminal, so §8's "every accepted seek receives an outcome" survives a
    /// shutdown backlog: nothing that follows implies it the way an ordinary
    /// event can be inferred from a later one.
    SeekCancelled {
        session_rev: u64,
        requested: Duration,
    },
    VolumeChanged {
        session_rev: u64,
        volume: Volume,
    },
    EndOfTrack {
        session_rev: u64,
        position: Duration,
        /// Whether `position` - the landed anchor plus decoded frames - is
        /// itself decoder-established or still carries an estimated
        /// anchor's provenance forward (§3, R4). Reaching the end of the
        /// body is real evidence the recording finished either way, but
        /// only an `Established` terminal position may overwrite a stored
        /// `position`; an `Estimated` one still may not.
        provenance: PositionProvenance,
    },
    DeviceRecovered {
        session_rev: u64,
    },
    /// Capability evidence that arrived after `Loaded` — an on-demand seek
    /// probe resolving `Unknown`. Ordered, revision-keyed, consumed like
    /// `Loaded`. It never establishes or clears checkpoint protection (§5).
    CapabilitiesChanged {
        session_rev: u64,
        capabilities: MediaCapabilities,
    },
    /// An explicit restart that actually landed.
    ///
    /// `SeekCompleted` deliberately does not cover this (D17): `restart()`
    /// discards a stored target and seeks to zero without emitting one, so a
    /// policy that must tell an explicit restart from any other establishment
    /// has nothing else to key on (G1).
    RestartEstablished {
        session_rev: u64,
        position: Duration,
        /// Whether this landing was decoder-confirmed or is still an
        /// estimate (§4.4). Despite the event's own name, a restart that
        /// reuses an already-open decoder seeks like any other, and on MP3
        /// that seek can still land `Estimated` - only that case, plus an
        /// establishing seek, earns the right to clear a sticky estimate
        /// and overwrite an established checkpoint. A fresh reopen sets
        /// this `Established` unconditionally, since a byte-zero open
        /// seeks nowhere.
        provenance: PositionProvenance,
    },
    Warning {
        session_rev: u64,
        message: String,
    },
    /// A `Load` accepted but abandoned before it produced `Loaded` or
    /// `Failed` — a stop or shutdown landed during the open (M5 §6). The
    /// load's one guaranteed outcome in that case; nothing else is emitted
    /// for it.
    LoadCancelled {
        session_rev: u64,
        request: LoadRequestId,
    },
    Failed {
        session_rev: u64,
        message: String,
        /// A typed cause for a remote failure, so a policy can act on what
        /// went wrong rather than only read a string a human wrote. M1's
        /// local failures have none; `message` stays the field every existing
        /// test asserts on (Ruling 1) — the remaining stringiness there is
        /// known debt for a later task, not something to fix here.
        cause: Option<RemoteFailure>,
        /// `Some` only when this failure is the outcome of the load still in
        /// flight when it happened - set from `loading`, taken by the same
        /// `fail_with` that emits this event, so a failure after `Loaded`
        /// carries `None` and is not mistaken for a load outcome (M5 §6).
        request: Option<LoadRequestId>,
    },
}

impl PlaybackEvent {
    /// Every event carries the revision it was emitted under. A reader that
    /// adopts it from **every** event — not only the ones it acts on — bounds
    /// its exposure to a dropped `DeviceRecovered` to "until the next event of
    /// any kind" (§7).
    pub fn session_rev(&self) -> u64 {
        match self {
            Self::Loaded { session_rev, .. }
            | Self::StateChanged { session_rev, .. }
            | Self::SeekCompleted { session_rev, .. }
            | Self::SeekTargetStored { session_rev, .. }
            | Self::SeekRejected { session_rev, .. }
            | Self::SeekCancelled { session_rev, .. }
            | Self::VolumeChanged { session_rev, .. }
            | Self::EndOfTrack { session_rev, .. }
            | Self::DeviceRecovered { session_rev }
            | Self::CapabilitiesChanged { session_rev, .. }
            | Self::RestartEstablished { session_rev, .. }
            | Self::Warning { session_rev, .. }
            | Self::LoadCancelled { session_rev, .. }
            | Self::Failed { session_rev, .. } => *session_rev,
        }
    }

    /// Terminal outcomes may occupy the reserved tail of the event channel;
    /// ordinary events may not. An outcome the application cannot infer from
    /// anything later — the run ended, the device died, the session stopped,
    /// an accepted seek was cancelled rather than completed — is terminal.
    /// `RestartEstablished` and `CapabilitiesChanged` are not: both are
    /// implied by whatever ordinary event comes next in the same way any
    /// other establishment is.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Failed { .. } | Self::EndOfTrack { .. } | Self::SeekCancelled { .. } => true,
            Self::StateChanged { state, .. } => matches!(
                state,
                PlaybackState::Stopped | PlaybackState::Ended | PlaybackState::Failed
            ),
            _ => false,
        }
    }

    /// The load this event concludes, if it is a load outcome (M5 §6). Each
    /// accepted `Load` produces exactly one.
    pub fn load_outcome(&self) -> Option<LoadRequestId> {
        match self {
            Self::Loaded { request, .. } | Self::LoadCancelled { request, .. } => Some(*request),
            Self::Failed { request, .. } => *request,
            _ => None,
        }
    }

    /// Protected outcomes are never dropped or displaced, and may use the
    /// reserved tail: correlation cannot be inferred from anything later.
    pub fn is_protected(&self) -> bool {
        self.load_outcome().is_some()
    }
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub session_rev: u64,
    pub media: Option<MediaId>,
    pub position: Duration,
    pub quality: PositionQuality,
    /// Whether `position` is decoder-established or a byte-offset estimate
    /// (§3). Orthogonal to `quality`: a `Degraded` position whose media time
    /// was established is still `Established` here, and the session policy
    /// must read this field, never `quality`, to decide.
    pub provenance: PositionProvenance,
    /// True exactly while a source read is blocked on the network and the
    /// hook, not the worker's own loop pass, is what is keeping progress
    /// alive (Ruling 4). Distinct from `quality == Degraded`, which reports a
    /// timing base that jumped - an unrelated fact this field must never be
    /// derived from.
    pub buffering: bool,
    /// The load token the worker currently owns (Decision 2, M5 §6): set
    /// when the worker emits `Loaded`, kept across stop, pause and recovery,
    /// and cleared when the next `load` starts. Mirrored from
    /// `Worker::adopted_load` via `SessionFacts::load`.
    pub load: Option<LoadRequestId>,
}

/// What a shutdown hands back: the position the worker captured on its way out,
/// and every event that never reached the application — whether it was still in
/// the worker's backlog or already in the channel when the interrupt landed
/// (D14, D19).
#[derive(Debug)]
pub struct ShutdownReport {
    pub progress: Progress,
    pub events: Vec<PlaybackEvent>,
}
