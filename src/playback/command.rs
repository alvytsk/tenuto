use std::time::Duration;

use crate::media::id::MediaId;
use crate::media::source::SourceLocation;

use crate::volume::Volume;

pub use crate::resume::ResumeIntent;

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
