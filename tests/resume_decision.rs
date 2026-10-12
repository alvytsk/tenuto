//! §11's table, tested against the persistence-free shape `decide_resume`
//! actually reasons about. Moved here from `src/session.rs` (Task 8): the
//! function itself moved to `src/resume.rs` so the playback worker can
//! resolve a candidate without depending on `crate::persistence` (G3).

use std::time::Duration;

use tenuto::media::id::{EpisodeKey, FeedId, MediaId};
use tenuto::media::provenance::PositionProvenance;
use tenuto::resume::{
    KnownDuration, ResumeCandidate, ResumeDecision, ResumeIntent, StoredProgress, decide_resume,
    resume_intent,
};

fn stored(secs: u64, completed: bool) -> ResumeCandidate {
    ResumeCandidate {
        position: Duration::from_secs(secs),
        completed,
    }
}

/// The one kind of media a fresh load resumes (M8 §7), so these tests see
/// the stored-progress rules rather than the media policy.
fn episode() -> MediaId {
    let enclosure =
        url::Url::parse("https://cdn.example/a.mp3").unwrap_or_else(|error| panic!("{error}"));
    MediaId::PodcastEpisode {
        feed: FeedId::new("subscription-1".into()).unwrap_or_else(|error| panic!("{error:?}")),
        episode: EpisodeKey::resolve(Some("guid"), Some(&enclosure), None)
            .unwrap_or_else(|error| panic!("{error:?}")),
    }
}

fn progress(position: Option<u64>, estimated: Option<u64>, completed: bool) -> StoredProgress {
    StoredProgress {
        position: position.map(Duration::from_secs),
        estimated: estimated.map(Duration::from_secs),
        completed,
    }
}

fn intent(stored: StoredProgress) -> ResumeIntent {
    resume_intent(&episode(), Some(stored))
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

// ------------------------------- resume intent for a fresh load (§4.3, M8 §7)

/// §4.2: an entry carrying only an estimate has no established candidate;
/// with no estimate either, nothing is stored to resume.
#[test]
fn resume_intent_with_nothing_established_and_no_estimate_starts_at_zero() {
    assert_eq!(
        intent(progress(None, None, false)),
        ResumeIntent::StartAt(Duration::ZERO)
    );
    assert_eq!(
        resume_intent(&episode(), None),
        ResumeIntent::StartAt(Duration::ZERO)
    );
}

/// Review Focus 4: established at the start is a candidate, distinct from
/// never established — `decide_resume` reads it as `AtStart`.
#[test]
fn resume_intent_keeps_a_position_established_at_zero_as_a_candidate() {
    assert_eq!(
        intent(progress(Some(0), None, false)),
        ResumeIntent::Candidate(stored(0, false))
    );
}

/// §4.2: an estimated timeline can complete a track without ever
/// establishing its anchor, so completion is still reported with no
/// established position.
#[test]
fn resume_intent_reports_completion_even_with_no_established_position() {
    assert_eq!(
        intent(progress(None, None, true)),
        ResumeIntent::Candidate(stored(0, true))
    );
    assert_eq!(
        decide_resume(Some(stored(0, true)), secs(300)),
        ResumeDecision::Completed
    );
}

#[test]
fn resume_intent_with_an_established_position_carries_it_through() {
    assert_eq!(
        intent(progress(Some(93), None, false)),
        ResumeIntent::Candidate(stored(93, false))
    );
}

/// §4.3: `estimated` is the listener's most recent expressed intent and
/// wins over an older established point, which is kept as the fallback.
#[test]
fn resume_intent_prefers_the_estimate_and_keeps_the_established_fallback() {
    assert_eq!(
        intent(progress(Some(40), Some(97), false)),
        ResumeIntent::EstimatedCandidate {
            target: Duration::from_secs(97),
            established: Some(Duration::from_secs(40)),
        }
    );
}

/// R8 / Review Focus 4: an entry that only ever carried an estimate reports
/// `established: None`, never a fabricated zero.
#[test]
fn resume_intent_with_no_established_fallback_reports_none_for_it() {
    assert_eq!(
        intent(progress(None, Some(97), false)),
        ResumeIntent::EstimatedCandidate {
            target: Duration::from_secs(97),
            established: None,
        }
    );
}

/// Review Focus 5: D1 replays a completed entry from zero, so a stray
/// estimate stored before completion never redirects it.
#[test]
fn resume_intent_completion_beats_an_estimate() {
    assert_eq!(
        intent(progress(Some(280), Some(97), true)),
        ResumeIntent::Candidate(stored(280, true))
    );
}

/// Review Focus 5, M8 §7: a local file or a plain URL never resumes on a
/// fresh load, whatever is stored — finished, part-way or estimated.
#[test]
fn resume_intent_never_resumes_a_local_file_or_url() {
    let local =
        MediaId::LocalFile(tenuto::media::id::AbsolutePath::new("/music/a.flac".into()).unwrap());
    let url = MediaId::RemoteUrl(
        tenuto::media::id::NormalizedUrl::parse("https://radio.example/a.mp3").unwrap(),
    );
    for media in [local, url] {
        for stored in [
            progress(Some(93), None, false),
            progress(Some(280), None, true),
            progress(Some(40), Some(97), false),
        ] {
            assert_eq!(
                resume_intent(&media, Some(stored)),
                ResumeIntent::StartAt(Duration::ZERO),
                "{media:?} {stored:?}"
            );
        }
    }
}
