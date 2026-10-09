use std::time::Duration;

use crate::media::id::MediaId;
use crate::media::source::SourceLocation;
use crate::resume::ResumeCandidate;

use crate::volume::Volume;

/// How a load's start is decided. `Load` carries one of these rather than a
/// bare `Duration` because a `Candidate` cannot be resolved into a position
/// until a decoder has reported a duration to resolve it against — and only
/// the worker has one (Ruling 5): hoisting the decision into `app::run` would
/// mean opening the media twice, once to decide and once to play.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeIntent {
    /// The application has already decided — there was no candidate to
    /// resolve, or a test wants a known start. `Restart` is unaffected: it is
    /// its own command, always to zero, and never goes through `Load`.
    StartAt(Duration),
    /// Resolved by the worker after its single probe, using the same rules the
    /// application would apply if it had a duration to apply them to.
    Candidate(ResumeCandidate),
    /// §4.3: the stored checkpoint's `estimated` location won the preference
    /// over `position` (`resume::restart_preference`). `target` is the
    /// estimate itself and is what the load seeks to, unconditionally —
    /// §4.3 is a preference between two already-known locations, not a
    /// duration-validated choice, so nothing resolves this the way
    /// `Candidate` resolves against a decoded duration. `established` is the
    /// stored `position` also on record beside the estimate, carried
    /// straight through for `Loaded`'s `StartDisposition::ResumedEstimated`
    /// to report — `None` for an entry that only ever held an estimate
    /// (R8), never a fabricated zero.
    EstimatedCandidate {
        target: Duration,
        established: Option<Duration>,
    },
}

/// A caller-chosen identity for one `Load` (M5 §6). The engine echoes it on
/// that load's outcome and never interprets it. Allocation belongs to
/// `Session`; `from_raw` exists for that allocator and for tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LoadRequestId(u64);

impl LoadRequestId {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug)]
pub enum PlaybackCommand {
    Load {
        request: LoadRequestId,
        media: MediaId,
        source: SourceLocation,
        resume: ResumeIntent,
    },
    Play,
    /// The automatic-start command queued right after its `Load`. Dispatched
    /// only when the worker still owns `request`'s media and is `Paused`
    /// after a successful load/device open (Decision 21); otherwise it does
    /// nothing. Unlike `Play`, a stale or failed load cannot trigger an
    /// implicit reopen through this command.
    PlayLoaded {
        request: LoadRequestId,
    },
    Pause,
    TogglePause,
    SeekTo(Duration),
    /// Signed seconds; the engine clamps at zero and at a known duration.
    SeekBy(i64),
    /// Validate first, start output only on success. One command rather than an
    /// app-sequenced `SeekTo(0)` + `Play`, because a stopped seek never emits
    /// `SeekCompleted` for the app to sequence on.
    Restart,
    SetVolume(Volume),
    Stop,
    Shutdown,
}

/// Whether a submission entered the queue. §8: saturation is visible, never a
/// block on the application thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Accepted,
    Busy,
    Gone,
}
