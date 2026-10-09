//! Feed and station changes (M9 PR C): the one dispatch behind `tenuto
//! subscribe`, `unsubscribe` and `refresh` and the browser's feed and
//! station keys. Each operation returns a `Report`: the text the CLI
//! prints on stdout, and whether the operation completed. A partial success
//! never counts as complete (M4 design doc §6.4). The CLI prints the text
//! and maps the outcome to its exit status (`commands`); the browser shows
//! it as a notice (`Report::into_notice`). Listing columns stay in
//! `commands`.
//!
//! `wait_http` is the one place the CLI and the browse worker enter the
//! Tokio runtime, which keeps `block_on` out of `library`.
//!
//! Report text is built in a `String`, which cannot fail to grow, so each
//! `writeln!` result is discarded.

use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;

use crate::application::runtime::LibraryStores;
use crate::feed::error::FeedError;
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::http::source::StationIdentity;
use crate::library::{
    AddStationOutcome, FollowupStep, RefreshOutcome, RemoveStationOutcome, SubscribeOutcome,
    UnsubscribeOutcome, add_station, refresh, refresh_all, remove_station, reprobe_station,
    subscribe, unsubscribe,
};
use crate::telemetry::redact_url;

/// The em dash every absent value prints: no episode count, no publication
/// date, no checkpoint, no station identity. One spelling, so a reader never
/// has to decide whether two blanks mean the same thing.
pub(crate) const ABSENT: &str = "—";

/// A change to the feed library or the saved stations, from the CLI or the
/// browser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedOp {
    /// `tenuto subscribe <url> [--as slug]`; the browser derives the slug.
    Subscribe { url: String, slug: Option<String> },
    /// `tenuto unsubscribe <slug>`. Local-only.
    Unsubscribe { slug: String },
    /// `tenuto refresh [slug]`: one feed, or every feed for `None`.
    Refresh { slug: Option<String> },
    /// Validates, probes and saves a station by URL (M7.1 §6, §10).
    AddStation { url: String },
    /// Drops a saved station. Local-only, like `Unsubscribe` (M7.1 §6).
    RemoveStation { slug: String },
    /// Re-probes a saved station, refreshing its cached identity (M7.1 §6).
    ReprobeStation { slug: String },
}

/// What one [`FeedOp`] did.
#[derive(Debug)]
pub(crate) struct Report {
    /// Exactly what the CLI prints on stdout, trailing newline included.
    /// Empty when the operation failed before it had anything to report.
    pub text: String,
    /// `Err` when the operation is incomplete: the error the CLI's exit
    /// status reports, after `text` has been printed.
    pub outcome: Result<(), FeedError>,
}

impl Report {
    /// The report as the browser's notice: `Ok` is what stdout would have
    /// carried, `Err` that text (when any) followed by the error the exit
    /// status would have named. Trailing whitespace is dropped; the TUI
    /// splits on the rest.
    pub(crate) fn into_notice(self) -> Result<String, String> {
        let text = self.text.trim_end().to_owned();
        match self.outcome {
            Ok(()) => Ok(text),
            Err(error) if text.is_empty() => Err(error.to_string()),
            Err(error) => Err(format!("{text}\n{error}")),
        }
    }
}

/// Runs `op` against `stores`. An operation that needs the network takes
/// the service from `http`, spawning one into it first if it is empty;
/// `Unsubscribe` and `RemoveStation` never touch it (M7.1 §6, R3).
pub(crate) fn run(
    op: &FeedOp,
    stores: &LibraryStores,
    http: &mut Option<Arc<HttpService>>,
) -> Report {
    dispatch(op, stores, http).unwrap_or_else(|error| Report {
        text: String::new(),
        outcome: Err(error),
    })
}

fn dispatch(
    op: &FeedOp,
    stores: &LibraryStores,
    http: &mut Option<Arc<HttpService>>,
) -> Result<Report, FeedError> {
    let subs = &stores.subscriptions;
    let cache = &stores.cache;
    let stations = &stores.stations;
    Ok(match op {
        FeedOp::Subscribe { url, slug } => {
            let service = service(http)?;
            finish_subscribe(wait_http(
                &service,
                subscribe(&service, subs, cache, url, slug.as_deref()),
            )?)
        }
        FeedOp::Unsubscribe { slug } => finish_unsubscribe(unsubscribe(subs, cache, slug)?),
        FeedOp::Refresh { slug: Some(slug) } => {
            let service = service(http)?;
            finish_refresh_one(wait_http(&service, refresh(&service, subs, cache, slug))?)
        }
        FeedOp::Refresh { slug: None } => {
            let service = service(http)?;
            finish_refresh_batch(wait_http(&service, refresh_all(&service, subs, cache))?)
        }
        FeedOp::AddStation { url } => {
            let service = service(http)?;
            finish_add_station(wait_http(&service, add_station(&service, stations, url))?)
        }
        FeedOp::RemoveStation { slug } => finish_remove_station(remove_station(stations, slug)?),
        FeedOp::ReprobeStation { slug } => {
            let service = service(http)?;
            finish_reprobe_station(wait_http(
                &service,
                reprobe_station(&service, stations, slug),
            )?)
        }
    })
}

/// The caller's HTTP service, spawned into `slot` on first use. A failure
/// to start is this operation's error; the slot stays empty, so the next
/// operation tries again.
fn service(slot: &mut Option<Arc<HttpService>>) -> Result<Arc<HttpService>, FeedError> {
    if let Some(service) = slot {
        return Ok(Arc::clone(service));
    }
    let service = HttpService::spawn(Limits::default())?;
    *slot = Some(Arc::clone(&service));
    Ok(service)
}

/// The shared synchronous bridge (M4 design doc §6.6). Every feed operation
/// enters the runtime here and nowhere else: `library.rs` stays free of
/// `block_on`, and `run_resolved`'s decoder path never enters a runtime at
/// all.
pub(crate) fn wait_http<F: Future>(service: &HttpService, future: F) -> F::Output {
    service.handle().block_on(future)
}

/// One refresh outcome, in full: what the fetch found, what committed, and —
/// where a §5.3 second step failed — exactly which step it was and why.
///
/// `url_moved` is redacted again here even though [`crate::library`] already
/// supplied redacted text: this is the presentation boundary, and a
/// redaction that depends on every producer having remembered to apply it is
/// one edit away from not holding.
fn write_refresh(text: &mut String, outcome: &RefreshOutcome) {
    let (slug, url_moved, followup) = match outcome {
        RefreshOutcome::Updated {
            slug,
            retained,
            skipped,
            url_moved,
            followup,
        } => {
            let _ = writeln!(
                text,
                "{slug}: updated, {retained} episodes retained, {skipped} skipped"
            );
            (slug, url_moved, followup)
        }
        RefreshOutcome::Unchanged {
            slug,
            url_moved,
            followup,
        } => {
            let _ = writeln!(text, "{slug}: unchanged");
            (slug, url_moved, followup)
        }
        RefreshOutcome::Failed { slug, error } => {
            // Nothing committed, so there is no redirect to report and no
            // followup to name: the fetch, the parse or the cache write is
            // the whole story.
            let _ = writeln!(text, "{slug}: failed: {error}");
            return;
        }
    };

    if let Some(moved) = url_moved {
        let _ = writeln!(text, "{slug}: feed URL moved to {}", redact_url(moved));
    }
    if let Some(failure) = followup {
        // The cache is the first half of every refresh commit (§5.3), so it
        // is what did land whichever step failed after it.
        let detail = match (failure.step, url_moved.is_some()) {
            (FollowupStep::SaveSubscription, true) => {
                "the redirected feed URL could not be recorded"
            }
            (FollowupStep::SaveSubscription, false) => {
                "the feed's changed title could not be recorded"
            }
            (FollowupStep::RemoveCache, _) => "a stale cache entry could not be removed",
        };
        let _ = writeln!(
            text,
            "{slug}: the episode cache was saved, but {detail}: {}",
            failure.error
        );
    }
}

/// `tenuto refresh` with no slug (§6.4): every feed is reported, and only
/// then does the count of feeds that did not complete decide the outcome.
/// One bad feed neither hides the others nor completes.
fn finish_refresh_batch(outcomes: Vec<RefreshOutcome>) -> Report {
    let total = outcomes.len();
    let mut text = String::new();
    let mut failed = 0;
    for outcome in &outcomes {
        let bad = match outcome {
            RefreshOutcome::Failed { .. } => true,
            RefreshOutcome::Updated { followup, .. }
            | RefreshOutcome::Unchanged { followup, .. } => followup.is_some(),
        };
        write_refresh(&mut text, outcome);
        failed += usize::from(bad);
    }
    let outcome = if failed == 0 {
        Ok(())
    } else {
        Err(FeedError::BatchIncomplete { failed, total })
    };
    Report { text, outcome }
}

/// `tenuto refresh <slug>` (§6.4): the concrete error, after reporting what
/// did commit. A batch count would tell a single-feed caller nothing it did
/// not already know.
fn finish_refresh_one(outcome: RefreshOutcome) -> Report {
    let mut text = String::new();
    write_refresh(&mut text, &outcome);
    let outcome = match outcome {
        RefreshOutcome::Failed { error, .. } => Err(error),
        RefreshOutcome::Updated { followup, .. } | RefreshOutcome::Unchanged { followup, .. } => {
            followup.map_or(Ok(()), |failure| Err(failure.error))
        }
    };
    Report { text, outcome }
}

/// `tenuto subscribe` (§5.3, §6.4). The commit order is cache first,
/// subscription second, so the only step that can fail after something
/// landed is the subscription — and when it does, the cache file is left
/// behind unreferenced and *nothing is subscribed*. Reporting that as a
/// subscription would send the listener looking for a feed that `tenuto
/// feeds` will not show.
fn finish_subscribe(outcome: SubscribeOutcome) -> Report {
    let SubscribeOutcome {
        slug,
        retained,
        skipped,
        followup,
        ..
    } = outcome;
    let mut text = String::new();
    let Some(failure) = followup else {
        let _ = writeln!(
            text,
            "{slug}: subscribed, {retained} episodes retained, {skipped} skipped"
        );
        return Report {
            text,
            outcome: Ok(()),
        };
    };
    let _ = writeln!(
        text,
        "{slug}: {retained} episodes were cached, but the subscription itself \
         could not be saved; nothing is subscribed: {}",
        failure.error
    );
    Report {
        text,
        outcome: Err(failure.error),
    }
}

/// `tenuto unsubscribe` (§5.3, §6.4). The subscription is removed first,
/// so a followup failure means the subscription is genuinely gone and only
/// its cache file remains — recoverable, and reported rather than silently
/// left behind.
fn finish_unsubscribe(outcome: UnsubscribeOutcome) -> Report {
    let UnsubscribeOutcome { slug, followup } = outcome;
    let mut text = String::new();
    let Some(failure) = followup else {
        let _ = writeln!(text, "{slug}: unsubscribed");
        return Report {
            text,
            outcome: Ok(()),
        };
    };
    let _ = writeln!(
        text,
        "{slug}: the subscription was removed, but its cached episodes \
         could not be deleted: {}",
        failure.error
    );
    Report {
        text,
        outcome: Err(failure.error),
    }
}

/// A verified station's identity, on one line: name, genre and bitrate,
/// joined by ` · ` and each omitted when absent, matching the Radio tab's
/// own row rendering (M7.1 design doc §7). `ABSENT` when nothing came back at
/// all — legitimate for a station whose ICY headers carry neither a name
/// nor a bitrate (§4).
fn station_identity_text(identity: &StationIdentity) -> String {
    let mut parts = Vec::new();
    if let Some(name) = &identity.name {
        parts.push(name.clone());
    }
    if let Some(genre) = &identity.genre {
        parts.push(genre.clone());
    }
    if let Some(bitrate) = identity.bitrate_kbps {
        parts.push(format!("{bitrate} kbps"));
    }
    if parts.is_empty() {
        ABSENT.to_string()
    } else {
        parts.join(" · ")
    }
}

/// `AddStation`'s and `ReprobeStation`'s shared report (M7.1 design doc §10),
/// `action` naming which one so the same taxonomy reads as "added" or
/// "re-probed" rather than composing two near-identical formatters.
fn station_probe(outcome: AddStationOutcome, action: &str) -> Report {
    let mut text = String::new();
    let outcome = match outcome {
        AddStationOutcome::Verified { slug, identity } => {
            let _ = writeln!(
                text,
                "{slug}: {action}, verified — {}",
                station_identity_text(&identity)
            );
            Ok(())
        }
        AddStationOutcome::Unverified { slug, reason } => {
            let _ = writeln!(text, "{slug}: {action}, unverified: {reason}");
            Ok(())
        }
        // A duplicate add resolves to the station already saved (§10) and
        // is not itself an error.
        AddStationOutcome::AlreadySaved {
            slug,
            identity,
            reprobe_failure: None,
        } => {
            let _ = writeln!(
                text,
                "{slug}: already saved, re-probed — {}",
                identity
                    .as_ref()
                    .map_or_else(|| ABSENT.to_string(), station_identity_text)
            );
            Ok(())
        }
        // A failed implicit re-probe is reported in the same line rather
        // than swallowed, exactly as an explicit `ReprobeStation`'s failure
        // is (`AddStationOutcome::AlreadySaved`'s own doc comment explains
        // why this field exists at all) — and, like `finish_subscribe` and
        // `finish_unsubscribe`'s own partial-failure arms, the text is
        // reported and the outcome is still `Err`. `Ok` here would undo that
        // fix at one remove: the reason would sit in the string, but
        // `into_notice` would call it a success, the tracing line would drop
        // its `failed:` prefix, and the browser would paint it with
        // `NoticeKind::Ok` instead of the error colour.
        AddStationOutcome::AlreadySaved {
            slug,
            identity,
            reprobe_failure: Some(reason),
        } => {
            let _ = writeln!(
                text,
                "{slug}: already saved; re-probe failed: {reason} (last known: {})",
                identity
                    .as_ref()
                    .map_or_else(|| ABSENT.to_string(), station_identity_text)
            );
            Err(FeedError::StationsUnreadable { reason })
        }
    };
    Report { text, outcome }
}

/// `AddStation` (M7.1 design doc §6, §10).
fn finish_add_station(outcome: AddStationOutcome) -> Report {
    station_probe(outcome, "added")
}

/// `ReprobeStation` (M7.1 design doc §6). [`reprobe_station`] only ever
/// produces [`AddStationOutcome::Verified`] on success — a station cannot
/// re-probe its way into being a duplicate of itself — but the outcome type
/// is shared with `AddStation`, so every arm is still handled.
fn finish_reprobe_station(outcome: AddStationOutcome) -> Report {
    station_probe(outcome, "re-probed")
}

/// `RemoveStation` (M7.1 design doc §6): a local edit, always complete once
/// [`crate::library::remove_station`] returns `Ok`.
fn finish_remove_station(outcome: RemoveStationOutcome) -> Report {
    let RemoveStationOutcome { slug } = outcome;
    Report {
        text: format!("{slug}: removed\n"),
        outcome: Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::clock::SystemClock;
    use crate::feed::cache::CacheStore;
    use crate::library::FollowupFailure;
    use crate::station::store::StationStore;
    use crate::subscription::store::SubscriptionStore;

    type Fallible = Result<(), Box<dyn std::error::Error>>;

    fn saved(error: FeedError) -> Option<FollowupFailure> {
        Some(FollowupFailure {
            step: FollowupStep::SaveSubscription,
            error,
        })
    }

    fn unreadable() -> FeedError {
        FeedError::SubscriptionsUnreadable {
            reason: "the disk is full".to_string(),
        }
    }

    fn refreshed(outcome: &RefreshOutcome) -> String {
        let mut text = String::new();
        write_refresh(&mut text, outcome);
        text
    }

    #[test]
    fn a_clean_refresh_reports_what_changed() {
        assert_eq!(
            refreshed(&RefreshOutcome::Updated {
                slug: "radio-t".to_string(),
                retained: 412,
                skipped: 3,
                url_moved: None,
                followup: None,
            }),
            "radio-t: updated, 412 episodes retained, 3 skipped\n"
        );
        assert_eq!(
            refreshed(&RefreshOutcome::Unchanged {
                slug: "radio-t".to_string(),
                url_moved: None,
                followup: None,
            }),
            "radio-t: unchanged\n"
        );
    }

    /// §6.6: a 304 can still follow a permanent redirect, and that commit can
    /// fail on its own. The redirect is reported, the cause is named, and the
    /// URL is redacted at presentation even though the library already
    /// supplied safe text.
    #[test]
    fn a_revalidated_feed_reports_a_failed_redirect_commit() {
        let rendered = refreshed(&RefreshOutcome::Unchanged {
            slug: "radio-t".to_string(),
            url_moved: Some("https://example.org/new?token=secret".to_string()),
            followup: saved(unreadable()),
        });
        assert!(rendered.starts_with("radio-t: unchanged\n"), "{rendered}");
        assert!(rendered.contains("https://example.org/new"), "{rendered}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(
            rendered.contains("the redirected feed URL could not be recorded"),
            "{rendered}"
        );
        assert!(rendered.contains("the disk is full"), "{rendered}");
    }

    /// Without a redirect the same step means the feed's own metadata
    /// changed, and saying "redirect" there would describe something that
    /// did not happen.
    #[test]
    fn a_failed_metadata_commit_does_not_claim_a_redirect() {
        let rendered = refreshed(&RefreshOutcome::Updated {
            slug: "radio-t".to_string(),
            retained: 5,
            skipped: 0,
            url_moved: None,
            followup: saved(unreadable()),
        });
        assert!(!rendered.contains("redirect"), "{rendered}");
        assert!(
            rendered.contains("the feed's changed title could not be recorded"),
            "{rendered}"
        );
    }

    fn mixed_batch() -> Vec<RefreshOutcome> {
        vec![
            RefreshOutcome::Updated {
                slug: "radio-t".to_string(),
                retained: 2,
                skipped: 0,
                url_moved: None,
                followup: None,
            },
            RefreshOutcome::Failed {
                slug: "sysdesign".to_string(),
                error: FeedError::UnsupportedFormat,
            },
            RefreshOutcome::Unchanged {
                slug: "late".to_string(),
                url_moved: None,
                followup: saved(unreadable()),
            },
        ]
    }

    /// §6.4: one bad feed neither hides the others nor completes. Every
    /// outcome is reported, and the error counts the feeds rather than
    /// naming one.
    #[test]
    fn a_mixed_batch_prints_everything_then_reports_the_count() -> Fallible {
        let report = finish_refresh_batch(mixed_batch());
        let error = report
            .outcome
            .err()
            .ok_or("a batch with two bad feeds must not succeed")?;
        assert!(report.text.contains("radio-t: updated"), "{}", report.text);
        assert!(report.text.contains("sysdesign: failed"), "{}", report.text);
        assert!(report.text.contains("late: unchanged"), "{}", report.text);
        assert!(
            matches!(
                error,
                FeedError::BatchIncomplete {
                    failed: 2,
                    total: 3
                }
            ),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn a_clean_batch_exits_zero() -> Fallible {
        let report = finish_refresh_batch(vec![RefreshOutcome::Unchanged {
            slug: "radio-t".to_string(),
            url_moved: None,
            followup: None,
        }]);
        report.outcome?;
        assert_eq!(report.text, "radio-t: unchanged\n");

        let empty = finish_refresh_batch(Vec::new());
        empty.outcome?;
        assert!(empty.text.is_empty());
        Ok(())
    }

    /// §6.4: a single-feed command returns the concrete error — not a batch
    /// count — after reporting what did commit.
    #[test]
    fn one_feed_returns_its_own_error_after_printing() -> Fallible {
        let report = finish_refresh_one(RefreshOutcome::Failed {
            slug: "radio-t".to_string(),
            error: FeedError::UnsupportedFormat,
        });
        assert!(report.text.contains("radio-t: failed"), "nothing printed");
        let error = report
            .outcome
            .err()
            .ok_or("a failed refresh must not succeed")?;
        assert!(matches!(error, FeedError::UnsupportedFormat), "{error:?}");

        let report = finish_refresh_one(RefreshOutcome::Updated {
            slug: "radio-t".to_string(),
            retained: 2,
            skipped: 0,
            url_moved: None,
            followup: saved(unreadable()),
        });
        assert!(
            report.text.contains("radio-t: updated, 2 episodes"),
            "{}",
            report.text
        );
        let error = report
            .outcome
            .err()
            .ok_or("a followup failure must not succeed")?;
        assert!(
            matches!(error, FeedError::SubscriptionsUnreadable { .. }),
            "{error:?}"
        );
        Ok(())
    }

    fn feed_id() -> Result<crate::media::id::FeedId, Box<dyn std::error::Error>> {
        Ok(crate::media::id::FeedId::new(
            "0123456789abcdef0123456789abcdef".to_string(),
        )?)
    }

    #[test]
    fn subscribing_prints_its_slug_and_counts() -> Fallible {
        let report = finish_subscribe(SubscribeOutcome {
            slug: "radio-t".to_string(),
            feed_id: feed_id()?,
            title: Some("Радио-Т".to_string()),
            retained: 412,
            skipped: 3,
            followup: None,
        });
        report.outcome?;
        assert_eq!(
            report.text,
            "radio-t: subscribed, 412 episodes retained, 3 skipped\n"
        );
        Ok(())
    }

    /// §5.3's commit order is cache first, subscription second, so a
    /// followup failure here means nothing is subscribed — and the line must
    /// say so rather than reporting a subscription that does not exist.
    #[test]
    fn a_half_committed_subscribe_never_claims_success() -> Fallible {
        let report = finish_subscribe(SubscribeOutcome {
            slug: "radio-t".to_string(),
            feed_id: feed_id()?,
            title: None,
            retained: 412,
            skipped: 0,
            followup: saved(unreadable()),
        });
        let rendered = &report.text;
        assert!(!rendered.contains("radio-t: subscribed,"), "{rendered}");
        assert!(rendered.contains("nothing is subscribed"), "{rendered}");
        assert!(rendered.contains("412 episodes were cached"), "{rendered}");
        let error = report
            .outcome
            .err()
            .ok_or("a half-committed subscribe must not succeed")?;
        assert!(
            matches!(error, FeedError::SubscriptionsUnreadable { .. }),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn unsubscribing_reports_a_cleanup_failure_as_a_failure() -> Fallible {
        let report = finish_unsubscribe(UnsubscribeOutcome {
            slug: "radio-t".to_string(),
            followup: None,
        });
        report.outcome?;
        assert_eq!(report.text, "radio-t: unsubscribed\n");

        let report = finish_unsubscribe(UnsubscribeOutcome {
            slug: "radio-t".to_string(),
            followup: Some(FollowupFailure {
                step: FollowupStep::RemoveCache,
                error: FeedError::UnsupportedFormat,
            }),
        });
        let rendered = &report.text;
        assert!(
            rendered.contains("the subscription was removed"),
            "{rendered}"
        );
        assert!(
            rendered.contains("cached episodes could not be deleted"),
            "{rendered}"
        );
        let error = report
            .outcome
            .err()
            .ok_or("a failed cleanup must not succeed")?;
        assert!(matches!(error, FeedError::UnsupportedFormat), "{error:?}");
        Ok(())
    }

    #[test]
    fn adding_a_verified_station_prints_its_identity() -> Fallible {
        let report = finish_add_station(AddStationOutcome::Verified {
            slug: "test-radio".to_string(),
            identity: StationIdentity {
                name: Some("Test Radio".to_string()),
                genre: Some("Lofi".to_string()),
                bitrate_kbps: Some(128),
                logo: None,
            },
        });
        report.outcome?;
        assert_eq!(
            report.text,
            "test-radio: added, verified — Test Radio · Lofi · 128 kbps\n"
        );
        Ok(())
    }

    /// `AddStationOutcome::AlreadySaved::reprobe_failure` exists so that a
    /// duplicate add's implicit re-probe failure reaches the caller instead
    /// of being swallowed by the "already saved" framing (M7.1 §6, §10); this
    /// is the presentation-layer half of that fix — the reason must show up
    /// in the reported line, not just in the value passed to
    /// `finish_add_station`.
    #[test]
    fn a_duplicate_adds_failed_reprobe_is_not_swallowed() -> Fallible {
        let report = finish_add_station(AddStationOutcome::AlreadySaved {
            slug: "test-radio".to_string(),
            identity: Some(StationIdentity {
                name: Some("Test Radio".to_string()),
                genre: None,
                bitrate_kbps: None,
                logo: None,
            }),
            reprobe_failure: Some("connection reset".to_string()),
        });
        let rendered = report.text.clone();
        // Not just the text: `into_notice` and the browse worker's log only
        // mark a failure, and the browser (`src/tui/browser.rs`) only paints
        // `NoticeKind::Err`, when the outcome is `Err` — exactly like
        // `finish_subscribe`'s and `finish_unsubscribe`'s own partial-failure
        // arms. A `contains()` check on the string alone would pass even if
        // the outcome were `Ok`.
        let error = report
            .outcome
            .err()
            .ok_or("a failed implicit re-probe must not report success")?;
        assert!(
            matches!(error, FeedError::StationsUnreadable { .. }),
            "{error:?}"
        );
        assert!(rendered.contains("already saved"), "{rendered}");
        assert!(
            rendered.contains("re-probe failed: connection reset"),
            "the re-probe failure must be surfaced, not swallowed: {rendered}"
        );
        assert!(
            rendered.contains("Test Radio"),
            "the station's prior identity is still shown: {rendered}"
        );
        Ok(())
    }

    #[test]
    fn removing_a_station_prints_its_slug() -> Fallible {
        let report = finish_remove_station(RemoveStationOutcome {
            slug: "test-radio".to_string(),
        });
        report.outcome?;
        assert_eq!(report.text, "test-radio: removed\n");
        Ok(())
    }

    /// The browser's notice is today's `commands::report` text: trailing
    /// whitespace trimmed, then the text alone, the error alone, or the text
    /// and the error on the next line.
    #[test]
    fn a_report_becomes_the_notice_the_browser_shows() {
        let complete = Report {
            text: "radio-t: unchanged\n \n".to_string(),
            outcome: Ok(()),
        };
        assert_eq!(complete.into_notice(), Ok("radio-t: unchanged".to_string()));

        let early = Report {
            text: String::new(),
            outcome: Err(FeedError::UnsupportedFormat),
        };
        assert_eq!(
            early.into_notice(),
            Err(FeedError::UnsupportedFormat.to_string())
        );

        let partial = Report {
            text: "radio-t: unsubscribed  \n".to_string(),
            outcome: Err(FeedError::UnsupportedFormat),
        };
        assert_eq!(
            partial.into_notice(),
            Err(format!(
                "radio-t: unsubscribed\n{}",
                FeedError::UnsupportedFormat
            ))
        );
    }

    /// Review focus 1: a refresh-all with failures is one red notice that
    /// lists every feed, then the count.
    #[test]
    fn a_mixed_batch_becomes_one_failed_notice() -> Fallible {
        let notice = finish_refresh_batch(mixed_batch())
            .into_notice()
            .err()
            .ok_or("a batch with failures must be a failed notice")?;
        let count = FeedError::BatchIncomplete {
            failed: 2,
            total: 3,
        }
        .to_string();
        let lines: Vec<&str> = notice.lines().collect();
        assert_eq!(
            lines.first().copied(),
            Some("radio-t: updated, 2 episodes retained, 0 skipped")
        );
        assert_eq!(lines.last().copied(), Some(count.as_str()));
        Ok(())
    }

    /// Review focus 2: an unsubscribe or a station removal is a local edit.
    /// Even when it fails, it never spawns the HTTP service. It also fails
    /// before it has anything to report, so its text is empty.
    #[test]
    fn local_operations_leave_the_http_slot_empty() -> Fallible {
        let dir = tempfile::tempdir()?;
        let stores = LibraryStores {
            subscriptions: SubscriptionStore::new(
                dir.path().join("subscriptions.json"),
                Arc::new(SystemClock),
            ),
            cache: CacheStore::new(dir.path().join("feeds")),
            stations: StationStore::new(dir.path().join("stations.json"), Arc::new(SystemClock)),
        };
        let mut http = None;
        for op in [
            FeedOp::Unsubscribe {
                slug: "nope".to_string(),
            },
            FeedOp::RemoveStation {
                slug: "nope".to_string(),
            },
        ] {
            let report = run(&op, &stores, &mut http);
            assert!(report.text.is_empty(), "{op:?}: {}", report.text);
            assert!(
                matches!(report.outcome, Err(FeedError::UnknownSlug { .. })),
                "{op:?}: {:?}",
                report.outcome
            );
            assert!(http.is_none(), "{op:?} spawned an HTTP service");
        }
        Ok(())
    }
}
