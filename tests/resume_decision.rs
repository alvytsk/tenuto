//! §11's table, tested against the persistence-free shape `decide_resume`
//! actually reasons about. Moved here from `src/session.rs` (Task 8): the
//! function itself moved to `src/resume.rs` so the playback worker can
//! resolve a candidate without depending on `crate::persistence` (G3).

use std::time::Duration;

use tenuto::media::provenance::PositionProvenance;
use tenuto::resume::{
    KnownDuration, RestartPreference, ResumeCandidate, ResumeDecision, decide_resume,
    restart_preference, resume_candidate,
};

fn stored(secs: u64, completed: bool) -> ResumeCandidate {
    ResumeCandidate {
        position: Duration::from_secs(secs),
        completed,
    }
}

/// Every existing test in this file predates provenance and means an
/// established duration - an M1/M2-style value a decoder actually reported.
fn secs(value: u64) -> Option<KnownDuration> {
    established(Duration::from_secs(value))
}

/// A duration extrapolated from a byte-rate estimate rather than observed
/// (§5.5) - `decide_resume` must treat this exactly as it treats `None`.
fn estimated(value: Duration) -> Option<KnownDuration> {
    Some(KnownDuration {
        value,
        provenance: PositionProvenance::Estimated,
    })
}

/// A duration from a real index, container header or seek table.
fn established(value: Duration) -> Option<KnownDuration> {
    Some(KnownDuration {
        value,
        provenance: PositionProvenance::Established,
    })
}

#[test]
fn no_entry_starts_at_the_beginning() {
    assert_eq!(decide_resume(None, secs(300)), ResumeDecision::NoEntry);
    assert_eq!(decide_resume(None, secs(300)).start_at(), Duration::ZERO);
}

#[test]
fn a_completed_entry_declines_the_resume_without_losing_its_position() {
    let entry = stored(300, true);
    assert_eq!(
        decide_resume(Some(entry), secs(300)),
        ResumeDecision::Completed
    );
    assert_eq!(
        decide_resume(Some(entry), secs(300)).start_at(),
        Duration::ZERO
    );
    assert_eq!(entry.position, Duration::from_secs(300));
    let short = stored(120, true);
    assert_eq!(
        decide_resume(Some(short), secs(300)),
        ResumeDecision::Completed
    );
    assert_eq!(short.position, Duration::from_secs(120));
}

#[test]
fn an_ordinary_position_inside_the_media_is_the_start() {
    assert_eq!(
        decide_resume(Some(stored(93, false)), secs(300)),
        ResumeDecision::Resume(Duration::from_secs(93))
    );
}

#[test]
fn a_position_of_zero_is_a_start_rather_than_a_resume() {
    assert_eq!(
        decide_resume(Some(stored(0, false)), secs(300)),
        ResumeDecision::AtStart
    );
    assert_eq!(
        decide_resume(Some(stored(0, false)), None),
        ResumeDecision::AtStart
    );
}

#[test]
fn a_position_exactly_at_the_end_is_degenerate_not_a_start() {
    assert_eq!(
        decide_resume(Some(stored(300, false)), secs(300)),
        ResumeDecision::DegenerateEnd
    );
    assert_eq!(
        decide_resume(Some(stored(300, false)), secs(300)).start_at(),
        Duration::ZERO
    );
}

#[test]
fn a_position_past_the_end_is_stale_state() {
    assert_eq!(
        decide_resume(Some(stored(400, false)), secs(300)),
        ResumeDecision::StalePastEnd
    );
    assert_eq!(
        decide_resume(Some(stored(400, false)), secs(300)).start_at(),
        Duration::ZERO
    );
}

#[test]
fn an_unknown_duration_keeps_the_position_unvalidated() {
    assert_eq!(
        decide_resume(Some(stored(93, false)), None),
        ResumeDecision::Unvalidated(Duration::from_secs(93))
    );
    assert_eq!(
        decide_resume(Some(stored(93, false)), None).start_at(),
        Duration::from_secs(93)
    );
}

#[test]
fn completion_outranks_every_position_rule() {
    assert_eq!(
        decide_resume(Some(stored(400, true)), secs(300)),
        ResumeDecision::Completed
    );
    assert_eq!(
        decide_resume(Some(stored(0, true)), secs(300)),
        ResumeDecision::Completed
    );
}

#[test]
fn a_checkpoint_past_an_estimated_duration_is_retained_rather_than_declared_stale() {
    // The spike measured a 361 s estimate for a 600 s VBR file. Under the old
    // rule a listener 70 % in resumes at zero and their entry is discarded as
    // stale — silent loss, from a number nothing ever measured.
    let candidate = ResumeCandidate {
        position: Duration::from_secs(420),
        completed: false,
    };
    assert_eq!(
        decide_resume(Some(candidate), estimated(Duration::from_secs(361))),
        ResumeDecision::Unvalidated(Duration::from_secs(420))
    );
}

#[test]
fn a_checkpoint_past_an_established_duration_is_still_stale() {
    // The M2 rule is unchanged where the duration was actually observed:
    // a position past a known end really is a file that changed underneath us.
    let candidate = ResumeCandidate {
        position: Duration::from_secs(420),
        completed: false,
    };
    assert_eq!(
        decide_resume(Some(candidate), established(Duration::from_secs(361))),
        ResumeDecision::StalePastEnd
    );
}

/// Persistence model §4.2: an entry can carry only an estimate, with nothing
/// ever established for it. `resume_candidate` must answer `None` — no
/// established candidate at all — rather than fabricating one at zero, which
/// `decide_resume` would read as `AtStart` and could not tell apart from a
/// position genuinely established at the start.
#[test]
fn resume_candidate_with_no_established_position_and_not_completed_is_no_candidate() {
    assert_eq!(resume_candidate(None, false), None);
    assert_eq!(
        decide_resume(resume_candidate(None, false), secs(300)),
        ResumeDecision::NoEntry
    );
}

/// §4.2: an estimated timeline can complete a track without ever
/// establishing its anchor, so completion must still be reported even with
/// no established position — `decide_resume` decides `Completed` before it
/// ever looks at the position `resume_candidate` filled in for this case.
#[test]
fn resume_candidate_reports_completion_even_with_no_established_position() {
    assert_eq!(
        resume_candidate(None, true),
        Some(ResumeCandidate {
            position: Duration::ZERO,
            completed: true,
        })
    );
    assert_eq!(
        decide_resume(resume_candidate(None, true), secs(300)),
        ResumeDecision::Completed
    );
}

/// The ordinary case is unaffected: an established position still produces
/// exactly the candidate it always did.
#[test]
fn resume_candidate_with_an_established_position_carries_it_through() {
    assert_eq!(
        resume_candidate(Some(Duration::from_secs(93)), false),
        Some(ResumeCandidate {
            position: Duration::from_secs(93),
            completed: false,
        })
    );
}

// ------------------------------------------- restart preference (§4.3, R8)

/// §4.3: `estimated` is the listener's most recent expressed intent and
/// must win over an older established point, never be averaged with it or
/// discarded in its favour. A wrong implementation that prefers `position`
/// instead — or that returns the established value as the target — fails
/// this on the `target` field alone.
#[test]
fn restart_preference_prefers_the_estimate_and_keeps_the_established_fallback() {
    let position = Some(Duration::from_secs(40));
    let estimated = Some(Duration::from_secs(97));
    assert_eq!(
        restart_preference(position, estimated),
        Some(RestartPreference {
            target: Duration::from_secs(97),
            established: Some(Duration::from_secs(40)),
        })
    );
}

/// R8: an entry that only ever carried an estimate must report `None` for
/// `established`, never `Some(Duration::ZERO)` — "never established" and
/// "established at the start" are different facts, and conflating them is
/// exactly the loss `resume_candidate` was already written to avoid for the
/// position side of this same rule. A wrong implementation that defaults
/// the absent position to zero, or that reads it through
/// `.unwrap_or_default()`, fails this on the `established` field alone
/// while `restart_preference_prefers_the_estimate_and_keeps_the_established_fallback`
/// above still passes — which is why this is its own test.
#[test]
fn restart_preference_with_no_established_fallback_reports_none_for_it() {
    let estimated = Some(Duration::from_secs(97));
    assert_eq!(
        restart_preference(None, estimated),
        Some(RestartPreference {
            target: Duration::from_secs(97),
            established: None,
        })
    );
}

/// No estimate at all: this function must add nothing, leaving the
/// established path (`resume_candidate` / `decide_resume`) completely
/// unchanged and in charge. A wrong implementation that falls back to
/// `position` as the target here would make `ResumedEstimated` reachable
/// for an entry that never held an estimate, which #4.3 does not allow.
#[test]
fn restart_preference_with_no_estimate_falls_through_to_the_established_path() {
    assert_eq!(
        restart_preference(Some(Duration::from_secs(40)), None),
        None
    );
}

#[test]
fn restart_preference_with_neither_location_is_none() {
    assert_eq!(restart_preference(None, None), None);
}
