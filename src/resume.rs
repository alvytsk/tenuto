//! What §11's table says a resume should do, decided from a bare position and
//! completion flag rather than from anything persistence owns.
//!
//! Deliberately persistence-free: nothing here imports `PersistedCheckpoint`
//! or anything else from `crate::persistence`. That is what lets
//! `crate::playback` — the worker, resolving a candidate against the
//! duration only its own decode probe can report — depend on this module
//! without the playback engine ever learning that persistence exists (G3).
//! `crate::session` depends on it too, but only to convert a stored entry
//! into a `ResumeCandidate` and to re-export the type and the function so
//! `app::run` and the tests that predate this split keep their import paths
//! (Ruling 4); the decision itself is made wherever a duration actually is,
//! which since Ruling 5 is the worker alone. Whoever calls `decide_resume`
//! gets the same answer for the same inputs, which is what keeps a
//! worker-resolved resume and an application-resolved one from ever landing
//! somewhere the checkpoint never said.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::media::id::MediaId;
use crate::media::provenance::PositionProvenance;

/// Logical resume position, independent of current transport capabilities.
/// `updated_at` is for inspection, never ordering or merging updates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlaybackCheckpoint {
    pub media: MediaId,
    pub position: Duration,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// A duration `decide_resume` reasons against, carrying whether it was
/// derived from a real index/container header or extrapolated from a
/// byte-rate estimate (§5.5). The distinction matters because an
/// under-estimated duration is not evidence that a stored position is stale:
/// `decide_resume` treats `Estimated` exactly as it treats an absent
/// duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnownDuration {
    pub value: Duration,
    pub provenance: PositionProvenance,
}

impl From<Duration> for KnownDuration {
    /// For the callers this milestone must not change: an M1/M2-style
    /// duration a decoder actually reported. Do not reach for this to
    /// describe a value that came from `estimate_num_mpeg_frames` — construct
    /// `KnownDuration` explicitly with `PositionProvenance::Estimated` there.
    fn from(value: Duration) -> Self {
        Self {
            value,
            provenance: PositionProvenance::Established,
        }
    }
}

/// The two facts §11's table is a function of, however they were learned. A
/// caller with a `PersistedCheckpoint` in hand converts it into one of these
/// through [`resume_candidate`]; a caller with only a worker-reported target
/// builds one directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResumeCandidate {
    pub position: Duration,
    pub completed: bool,
}

/// Builds the candidate `decide_resume` reasons about from a checkpoint's raw
/// fields, without this module ever importing `PersistedCheckpoint` (G3): the
/// caller — `open_persistence`, resolving whatever a freshly loaded file
/// contains, and every test that wants the same conversion production uses —
/// hands over `position` and `completed` directly instead.
///
/// This is deliberately the *only* `PersistedCheckpoint` → `ResumeCandidate`
/// conversion in the crate. An earlier version of this code also had an
/// infallible `impl From<&PersistedCheckpoint> for ResumeCandidate` in
/// `session.rs`, which defaulted an absent `position` to `Duration::ZERO`.
/// It had no production caller — `open_persistence` always used this
/// function instead — but it was still reachable by anyone who reached for
/// the obvious `.into()`, and its default was exactly the "resume at start"
/// loss this construction exists to prevent, with no diagnostic. Deleted
/// rather than kept as a documented trap: a conversion that must not be
/// called with an unverified entry is safer removed than annotated.
///
/// An absent established position (design doc §4.2 — an entry that exists
/// only to carry an estimate, with nothing ever established) is its own
/// case, `None`, rather than a fabricated `Some(ResumeCandidate { position:
/// Duration::ZERO, .. })`: `decide_resume` would read that as
/// `ResumeDecision::AtStart`, an established position of zero, which is not
/// what an absent position means. Resuming a listener at the start because
/// only an estimate was ever stored for them is exactly the loss this
/// construction exists to prevent.
///
/// A completed entry is still reported even with no established position
/// (an estimated timeline can complete a track without ever establishing its
/// anchor, design doc §4.2): `decide_resume` decides `Completed` before it
/// ever looks at `position` (see below), so the filler `Duration::ZERO` built
/// here for that case is provably never read.
pub fn resume_candidate(position: Option<Duration>, completed: bool) -> Option<ResumeCandidate> {
    if completed {
        return Some(ResumeCandidate {
            position: position.unwrap_or(Duration::ZERO),
            completed: true,
        });
    }
    position.map(|position| ResumeCandidate {
        position,
        completed: false,
    })
}

/// What a resumed load should target, and what it should report keeping in
/// reserve, when the stored checkpoint carries an `estimated` location
/// (design doc §4.3). Built by [`restart_preference`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartPreference {
    /// The location to seek to: always the estimate, per the rule below.
    pub target: Duration,
    /// The established `position` also on record beside the estimate that
    /// won, when one exists. `None` for an entry that only ever carried an
    /// estimate (design doc §4.3's R8) — distinct from "established at the
    /// start", which this function never fabricates from an absent value,
    /// the same discipline [`resume_candidate`] already applies.
    pub established: Option<Duration>,
}

/// Resolves which of a checkpoint's two locations — `estimated` and
/// `position` — a resumed load should target (§4.3). `estimated` wins
/// whenever it is present: it is the listener's most recent expressed
/// intent, and falling back to an older established point would silently
/// undo their last seek, which is the exact failure this ruling exists to
/// prevent. `position` is carried along as the fallback that is never
/// discarded, not merged into the target or averaged with it.
///
/// Returns `None` when there is no estimate to prefer — including when
/// there is no checkpoint at all — and a `None` here is the caller's signal
/// to fall through to the existing, unchanged `resume_candidate` /
/// `decide_resume` path: this function only ever *adds* a preference, never
/// removes the established path's own behaviour, which design doc §4.3
/// requires to stay bit-for-bit as it was.
///
/// A `completed` entry is the caller's own concern, exactly as it is for
/// `resume_candidate`: check it first, and do not call this function at all
/// when it is set, since D1 retains a completed entry's position and
/// nothing here should be seeked to.
///
/// This is deliberately a preference between two already-known durations,
/// decided before either is ever handed to a decoder — distinct from
/// `decide_resume`, which validates one chosen duration against the media's
/// own length once that is known. `src/app.rs`'s `resume_intent_for` is the
/// production caller: it calls this function and builds exactly the
/// `ResumeIntent::EstimatedCandidate` / `StartDisposition::ResumedEstimated`
/// pair this result feeds.
pub fn restart_preference(
    position: Option<Duration>,
    estimated: Option<Duration>,
) -> Option<RestartPreference> {
    estimated.map(|estimated| RestartPreference {
        target: estimated,
        established: position,
    })
}

/// What §11's table says about one resume candidate, and why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeDecision {
    /// No candidate at all.
    NoEntry,
    /// The entry is complete. D1 retains its position; the resume declines it.
    Completed,
    /// An entry that never got anywhere.
    AtStart,
    Resume(Duration),
    /// `position == duration`: preserved in storage, not usable as a start.
    DegenerateEnd,
    /// `position > duration`: the file no longer describes this media.
    StalePastEnd,
    /// The duration is unknown, so the position is retained unvalidated.
    Unvalidated(Duration),
}

impl ResumeDecision {
    pub fn start_at(&self) -> Duration {
        match self {
            Self::Resume(position) | Self::Unvalidated(position) => *position,
            Self::NoEntry
            | Self::Completed
            | Self::AtStart
            | Self::DegenerateEnd
            | Self::StalePastEnd => Duration::ZERO,
        }
    }
}

/// Applied against whatever duration the caller already has in hand — the
/// worker's own decode probe when resolving a `ResumeIntent::Candidate`, a
/// literal in a test — so deciding never opens a second file on its own
/// account. Completion is never inferred from `position >= duration`, and
/// there is no near-end heuristic anywhere.
///
/// An **estimated** duration is treated exactly as an absent one (§5.5):
/// `StalePastEnd` requires an established duration, because declaring a
/// listener's checkpoint stale is destructive and an estimate — which the
/// spike measured 40% short on a genuinely VBR file — is not evidence enough
/// to do it.
pub fn decide_resume(
    candidate: Option<ResumeCandidate>,
    duration: Option<KnownDuration>,
) -> ResumeDecision {
    let Some(candidate) = candidate else {
        return ResumeDecision::NoEntry;
    };
    if candidate.completed {
        return ResumeDecision::Completed;
    }
    if candidate.position.is_zero() {
        return ResumeDecision::AtStart;
    }
    let Some(duration) = duration else {
        return ResumeDecision::Unvalidated(candidate.position);
    };
    if duration.provenance == PositionProvenance::Estimated {
        return ResumeDecision::Unvalidated(candidate.position);
    }
    match candidate.position.cmp(&duration.value) {
        std::cmp::Ordering::Less => ResumeDecision::Resume(candidate.position),
        std::cmp::Ordering::Equal => ResumeDecision::DegenerateEnd,
        std::cmp::Ordering::Greater => ResumeDecision::StalePastEnd,
    }
}
