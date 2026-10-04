//! The M4 presentation layer (design doc §6.1–§6.4): the only place that
//! formats a feed command's output, decides its exit status, and bridges
//! [`crate::library`]'s async functions into a synchronous `main`.
//!
//! [`crate::library`] returns values and prints nothing; this module turns
//! those values into the columns §6.1 specifies and into the `Err` that
//! §6.4's "a partial failure cannot exit successfully" requires. The split
//! is what lets M5 reuse the library with a different presentation, and what
//! keeps `block_on` out of `library`. `wait_http` is shared with the browse
//! worker; the artwork worker blocks on its own (architecture §5).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use time::{OffsetDateTime, UtcOffset};

use crate::cli::CliCommand;
use crate::clock::SystemClock;
use crate::feed::cache::CacheStore;
use crate::feed::error::FeedError;
use crate::http::error::redact_url;
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::http::source::StationIdentity;
use crate::library::{
    AddStationOutcome, EpisodeRow, FeedSummary, FollowupStep, Progress, RefreshOutcome,
    RemoveStationOutcome, SubscribeOutcome, UnsubscribeOutcome,
};
use crate::persistence::PersistenceError;
use crate::persistence::store::StateStore;
use crate::station::store::StationStore;
use crate::subscription::store::SubscriptionStore;

/// Runs one feed command to completion.
///
/// `Play` never arrives here: [`crate::app::run`] resolves both of its forms
/// itself (§6.5). The arm exists so that this function is total, and it
/// carries no argument text — a `play` source can be a URL, and echoing one
/// back is exactly what §7.2's redaction rule forbids.
pub fn run(command: CliCommand) -> Result<(), FeedError> {
    let mut out = std::io::stdout().lock();
    let outcome = match command {
        CliCommand::Feeds => {
            let (subs, cache) = platform_subscription_stores()?;
            write_feeds(&mut out, &crate::library::list_feeds(&subs, &cache)?)
        }
        CliCommand::Episodes {
            slug,
            limit,
            reverse,
        } => {
            let (subs, cache) = platform_subscription_stores()?;
            // Read before listing, and surfaced rather than defaulted: a
            // state file that cannot be read is not "nothing has played",
            // and printing every episode as unplayed would misreport a
            // listener's whole history as empty (§5.5).
            let state = platform_state_store()?.read_snapshot()?;
            // Reversing has to see every row before `-n` truncates, or it would
            // reverse the first N rather than show the last N.
            let fetch = if reverse { None } else { limit };
            let rows = crate::library::list_episodes(&subs, &cache, &state, &slug, fetch)?;
            let rows = if reverse { reversed(rows, limit) } else { rows };
            write_episodes(&mut out, &rows)
        }
        CliCommand::Subscribe { url, slug } => {
            let (subs, cache) = platform_subscription_stores()?;
            let service = HttpService::spawn(Limits::default())?;
            let outcome = wait_http(
                &service,
                crate::library::subscribe(&service, &subs, &cache, &url, slug.as_deref()),
            )?;
            finish_subscribe(&mut out, outcome)
        }
        CliCommand::Unsubscribe { slug } => {
            // No `HttpService`: removing a subscription is a local edit, and
            // spawning a runtime for it would be work with nothing to do.
            let (subs, cache) = platform_subscription_stores()?;
            let outcome = crate::library::unsubscribe(&subs, &cache, &slug)?;
            finish_unsubscribe(&mut out, outcome)
        }
        CliCommand::Refresh { slug: Some(slug) } => {
            let (subs, cache) = platform_subscription_stores()?;
            let service = HttpService::spawn(Limits::default())?;
            let outcome = wait_http(
                &service,
                crate::library::refresh(&service, &subs, &cache, &slug),
            )?;
            finish_refresh_one(&mut out, outcome)
        }
        CliCommand::Refresh { slug: None } => {
            let (subs, cache) = platform_subscription_stores()?;
            let service = HttpService::spawn(Limits::default())?;
            let outcomes = wait_http(
                &service,
                crate::library::refresh_all(&service, &subs, &cache),
            )?;
            finish_refresh_batch(&mut out, outcomes)
        }
        CliCommand::Play { .. } => Err(FeedError::Malformed {
            detail: "play is resolved by the application, not by the command table".to_string(),
        }),
        CliCommand::Tui { .. } => Err(FeedError::Malformed {
            detail: "tui is run by the application, not by the command table".to_string(),
        }),
    };
    // A command whose output never reached the terminal has not reported
    // anything, whatever it committed, so the flush decides the status too.
    out.flush().map_err(stdout_failure)?;
    outcome
}

/// The subscription and cache stores on this platform's directories.
///
/// Neither constructor touches the filesystem: `subscriptions.json` and the
/// `feeds` directory come into existence on the first write, so a listing on
/// a machine that has never subscribed creates nothing (§8.4). Checkpoints
/// are deliberately absent — [`platform_state_store`] is a separate call, so
/// that a command with no progress column to join against never opens
/// `state.json` at all.
pub(crate) fn platform_subscription_stores() -> Result<(SubscriptionStore, CacheStore), FeedError> {
    let dirs = directories::ProjectDirs::from("", "", "tenuto").ok_or_else(|| {
        FeedError::SubscriptionsUnreadable {
            reason: "no platform data directory is available".to_string(),
        }
    })?;
    let clock = Arc::new(SystemClock);
    Ok((
        SubscriptionStore::new(dirs.data_dir().join("subscriptions.json"), clock),
        CacheStore::new(dirs.cache_dir().join("feeds")),
    ))
}

/// The saved-station store on this platform's directory (M7.1 design doc §4,
/// §6), at `stations.json` beside `subscriptions.json` in the same
/// `data_dir`. A separate lookup from [`platform_subscription_stores`]
/// rather than a third element of its tuple: every one of that function's
/// callers is a feed command with no use for a station store, and widening
/// its return type would hand each of them a store they never touch.
pub(crate) fn platform_station_store() -> Result<StationStore, FeedError> {
    let dirs = directories::ProjectDirs::from("", "", "tenuto").ok_or_else(|| {
        FeedError::StationsUnreadable {
            reason: "no platform data directory is available".to_string(),
        }
    })?;
    Ok(StationStore::new(
        dirs.data_dir().join("stations.json"),
        Arc::new(SystemClock),
    ))
}

/// The checkpoint store, on the same path playback itself uses —
/// [`StateStore::platform_path`] stays the one discovery point for it (§13).
pub(crate) fn platform_state_store() -> Result<StateStore, FeedError> {
    Ok(StateStore::new(
        StateStore::platform_path()?,
        Arc::new(SystemClock),
    ))
}

/// The shared synchronous bridge (§6.6). Every network command enters the
/// runtime here and nowhere else, and the browse worker's mutations
/// ([`crate::application::browse`]) use it too: `library.rs` stays free of
/// `block_on`, and `run_resolved`'s decoder path never enters a runtime at
/// all.
pub(crate) fn wait_http<F: std::future::Future>(service: &HttpService, future: F) -> F::Output {
    service.handle().block_on(future)
}

/// A command that could not report its own result has not succeeded, however
/// far its work got, so every write is checked rather than discarded.
fn stdout_failure(source: std::io::Error) -> FeedError {
    FeedError::Persistence(PersistenceError::Io {
        path: PathBuf::from("<stdout>"),
        op: "write command output to",
        source,
    })
}

/// The CLI's report of `outcome` as text, for a front end that shows it
/// instead of printing it: `Ok` is what stdout would have carried, `Err`
/// that text (when any) followed by the error the exit status would have
/// named. Trailing whitespace is dropped; the TUI splits on the rest.
pub(crate) fn report<T>(
    finish: impl FnOnce(&mut dyn Write, T) -> Result<(), FeedError>,
    outcome: T,
) -> Result<String, String> {
    let mut out = Vec::new();
    let status = finish(&mut out, outcome);
    let text = String::from_utf8_lossy(&out).trim_end().to_owned();
    match status {
        Ok(()) => Ok(text),
        Err(error) if text.is_empty() => Err(error.to_string()),
        Err(error) => Err(format!("{text}\n{error}")),
    }
}

// --- Formatting (§6.1, §6.2) -----------------------------------------

/// The em dash every absent value prints: no episode count, no publication
/// date, no checkpoint. One spelling, so a reader never has to decide
/// whether two blanks mean the same thing.
const ABSENT: &str = "—";

/// `H:MM:SS` only once there are hours to show, `M:SS` otherwise — a
/// leading `0:` on every podcast position would be noise, and a bare `MM:SS`
/// on a two-hour show would be a lie.
fn duration_text(value: std::time::Duration) -> String {
    let secs = value.as_secs();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// §6.2's cell, with its two marks: **parentheses** are a duration the feed
/// declared — never decoder-confirmed, since checkpoints store none — and a
/// **tilde** is an estimated position, which M3's provenance rule forbids
/// presenting as a confirmed one. A declared duration is shown only beside a
/// position, since "played / (1:42:00)" would suggest a comparison that the
/// cell is not making.
fn progress_text(progress: &Progress, declared: Option<std::time::Duration>) -> String {
    let position = match progress {
        Progress::None => return ABSENT.to_string(),
        Progress::Played => return "played".to_string(),
        Progress::Unknown => return "position unknown".to_string(),
        Progress::Established(value) => duration_text(*value),
        Progress::Estimated(value) => format!("~{}", duration_text(*value)),
    };
    match declared {
        Some(value) => format!("{position} / ({})", duration_text(value)),
        None => position,
    }
}

/// §6.1: every displayed timestamp is UTC and the header says so. No local
/// conversion is attempted — `time` without `local-offset` cannot determine
/// the offset reliably in a multithreaded process, and a mislabeled local
/// time is worse than an honest UTC one. Built from numeric fields so that
/// no new `time` feature is needed.
fn timestamp_text(value: OffsetDateTime) -> String {
    let value = value.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
    )
}

/// A publication date, to the day: a feed's own `pubDate` precision is not
/// something a listing should imply it has verified to the minute.
fn date_text(value: OffsetDateTime) -> String {
    let value = value.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}",
        value.year(),
        u8::from(value.month()),
        value.day(),
    )
}

/// Feed-supplied text reaching a terminal, made safe **at the formatting
/// boundary only**: the cached title and the identity derived from it are
/// untouched, so nothing here changes what a later refresh compares against.
/// A newline would break the row layout and an escape sequence would reach
/// the terminal, so every character that can do either becomes a visible
/// escape; all the rest — Cyrillic, CJK, emoji — pass through exactly as
/// stored, since transliterating a title would make it someone else's title.
/// The listing in reverse, cut to `limit` from the end.
///
/// §1.2 keeps feed document order and never renumbers, so this changes only
/// the order rows are shown in: each row keeps the index `play <slug> <index>`
/// resolves. It is deliberately not called newest-first. Reversing an
/// oldest-first feed such as web-standards does put the newest episode on top,
/// but reversing a newest-first feed such as Radio-T puts the oldest there, and
/// §1.2 declines to guess which kind a feed is from dates it does not trust.
fn reversed(mut rows: Vec<EpisodeRow>, limit: Option<std::num::NonZeroUsize>) -> Vec<EpisodeRow> {
    rows.reverse();
    rows.truncate(limit.map_or(usize::MAX, std::num::NonZeroUsize::get));
    rows
}

pub(crate) fn displayable(text: &str) -> String {
    if !text.chars().any(needs_escape) {
        return text.to_string();
    }
    text.chars()
        .map(|value| match value {
            '\n' => r"\n".to_string(),
            '\r' => r"\r".to_string(),
            '\t' => r"\t".to_string(),
            other if needs_escape(other) => format!("\\u{{{:x}}}", other as u32),
            other => other.to_string(),
        })
        .collect()
}

/// What a title may not carry into a row.
///
/// `char::is_control` alone is not the whole set: U+2028 LINE SEPARATOR and
/// U+2029 PARAGRAPH SEPARATOR are line breaks that it does not classify as
/// control characters, and the bidi overrides U+202A–U+202E can reorder
/// everything rendered after them — including the columns beside the title
/// — without being line breaks at all. Neither are the bidi **isolates**
/// U+2066–U+2069 (LRI/RLI/FSI/PDI): they reorder the same way the overrides
/// do, and are the half of the Trojan Source technique that survives in
/// modern Unicode, since isolates were added specifically so an override
/// could not leak its reordering past its own text — the isolate itself
/// still reorders whatever it wraps. U+200E/U+200F (LRM/RLM) and U+061C
/// (ALM) are direction marks rather than reordering ranges, but they are
/// invisible and feed-controlled, so they are escaped alongside the rest
/// for the same reason. A feed title is untrusted input on its way to a
/// terminal, so every one of these groups is escaped rather than displayed.
fn needs_escape(value: char) -> bool {
    value.is_control()
        || matches!(
            value,
            '\u{200e}'
                | '\u{200f}'
                | '\u{061c}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// A title cell. An item with no title at all still has to occupy its
/// column, and `(untitled)` is the same spelling `--probe-only` and
/// `NotPlayable` already use for it.
fn title_text(title: Option<&str>) -> String {
    title.map_or_else(|| "(untitled)".to_string(), displayable)
}

/// Column widths are measured in characters rather than bytes: every padded
/// column here is ASCII by construction (slugs are validated, the rest is
/// generated), and a byte count would still be the wrong tool if that ever
/// stopped being true.
fn width(value: &str) -> usize {
    value.chars().count()
}

/// Left-aligned padding to `width` characters, with nothing added beyond it
/// — a trailing column never gets trailing spaces.
fn pad(value: &str, to: usize) -> String {
    let count = to.saturating_sub(width(value));
    format!("{value}{:count$}", "")
}

/// Right-aligned padding, for the two numeric columns.
fn pad_left(value: &str, to: usize) -> String {
    let count = to.saturating_sub(width(value));
    format!("{:count$}{value}", "")
}

/// Every column is at least as wide as its own label, and grows to its
/// widest value.
fn column(label: &str, values: impl Iterator<Item = usize>) -> usize {
    values.fold(width(label), usize::max)
}

/// `tenuto feeds` (§6.1). A subscription that has never been refreshed
/// shows `—` episodes and `never`, which is the normal state directly after
/// `subscribe` rather than a failure.
fn write_feeds(out: &mut dyn Write, feeds: &[FeedSummary]) -> Result<(), FeedError> {
    let counts: Vec<String> = feeds
        .iter()
        .map(|feed| {
            feed.episodes
                .map_or_else(|| ABSENT.to_string(), |n| n.to_string())
        })
        .collect();
    let refreshed: Vec<String> = feeds
        .iter()
        .map(|feed| {
            feed.last_refreshed_at
                .map_or_else(|| "never".to_string(), timestamp_text)
        })
        .collect();

    // The minimum of 10 is the column §6.1 displays: a slug is up to 32
    // characters, and letting a one-feed listing collapse to `SLUG` width
    // would make two runs of the same command disagree about where the
    // table starts more often than not.
    let slug_width = column("SLUG", feeds.iter().map(|feed| width(&feed.slug))).max(10);
    let count_width = column("EPISODES", counts.iter().map(|value| width(value)));
    let refreshed_width = column(
        "REFRESHED (UTC)",
        refreshed.iter().map(|value| width(value)),
    );

    write_row(
        out,
        &[
            pad("SLUG", slug_width),
            pad_left("EPISODES", count_width),
            pad("REFRESHED (UTC)", refreshed_width),
            "TITLE".to_string(),
        ],
    )?;
    for ((feed, count), refreshed) in feeds.iter().zip(&counts).zip(&refreshed) {
        write_row(
            out,
            &[
                pad(&feed.slug, slug_width),
                pad_left(count, count_width),
                pad(refreshed, refreshed_width),
                title_text(feed.title.as_deref()),
            ],
        )?;
    }
    Ok(())
}

/// `tenuto episodes <slug>` (§6.1, §6.2). `AUDIO` is a column of its own
/// so that a removed enclosure never hides existing progress.
fn write_episodes(out: &mut dyn Write, episodes: &[EpisodeRow]) -> Result<(), FeedError> {
    let progress: Vec<String> = episodes
        .iter()
        .map(|row| progress_text(&row.progress, row.declared_duration))
        .collect();
    let published: Vec<String> = episodes
        .iter()
        .map(|row| row.published.map_or_else(|| ABSENT.to_string(), date_text))
        .collect();

    // Three digits minimum: the index column's label is one character wide,
    // and a feed's indices routinely are not.
    let index_width = column(
        "#",
        episodes.iter().map(|row| width(&row.index.to_string())),
    )
    .max(3);
    let progress_width = column("PROGRESS", progress.iter().map(|value| width(value)));
    // Fixed: both values this column can hold — `-` and `none` — are
    // shorter than its own label.
    let audio_width = width("AUDIO");
    let published_width = column(
        "PUBLISHED (UTC)",
        published.iter().map(|value| width(value)),
    );

    write_row(
        out,
        &[
            pad_left("#", index_width),
            pad("PROGRESS", progress_width),
            pad("AUDIO", audio_width),
            pad("PUBLISHED (UTC)", published_width),
            "TITLE".to_string(),
        ],
    )?;
    for ((row, progress), published) in episodes.iter().zip(&progress).zip(&published) {
        write_row(
            out,
            &[
                pad_left(&row.index.to_string(), index_width),
                pad(progress, progress_width),
                pad(if row.playable { "-" } else { "none" }, audio_width),
                pad(published, published_width),
                title_text(row.title.as_deref()),
            ],
        )?;
    }
    Ok(())
}

/// Two spaces between columns, and no trailing whitespace: the last cell is
/// written as it is, however short — an item whose title is empty leaves the
/// line ending where its last real character did.
fn write_row(out: &mut dyn Write, cells: &[String]) -> Result<(), FeedError> {
    writeln!(out, "{}", cells.join("  ").trim_end()).map_err(stdout_failure)
}

// --- Outcomes and exit status (§5.3, §6.4) ---------------------------

/// One refresh outcome, in full: what the fetch found, what committed, and —
/// where a §5.3 second step failed — exactly which step it was and why.
///
/// `url_moved` is redacted again here even though [`crate::library`] already
/// supplied redacted text: this is the presentation boundary, and a
/// redaction that depends on every producer having remembered to apply it is
/// one edit away from not holding.
fn write_refresh(out: &mut dyn Write, outcome: &RefreshOutcome) -> Result<(), FeedError> {
    let (slug, url_moved, followup) = match outcome {
        RefreshOutcome::Updated {
            slug,
            retained,
            skipped,
            url_moved,
            followup,
        } => {
            writeln!(
                out,
                "{slug}: updated, {retained} episodes retained, {skipped} skipped"
            )
            .map_err(stdout_failure)?;
            (slug, url_moved, followup)
        }
        RefreshOutcome::Unchanged {
            slug,
            url_moved,
            followup,
        } => {
            writeln!(out, "{slug}: unchanged").map_err(stdout_failure)?;
            (slug, url_moved, followup)
        }
        RefreshOutcome::Failed { slug, error } => {
            // Nothing committed, so there is no redirect to report and no
            // followup to name: the fetch, the parse or the cache write is
            // the whole story.
            return writeln!(out, "{slug}: failed: {error}").map_err(stdout_failure);
        }
    };

    if let Some(moved) = url_moved {
        writeln!(out, "{slug}: feed URL moved to {}", redact_url(moved)).map_err(stdout_failure)?;
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
        writeln!(
            out,
            "{slug}: the episode cache was saved, but {detail}: {}",
            failure.error
        )
        .map_err(stdout_failure)?;
    }
    Ok(())
}

/// `tenuto refresh` with no slug (§6.4): every feed is printed, and only
/// then does the count of feeds that did not complete decide the exit
/// status. One bad feed neither hides the others nor exits zero.
pub(crate) fn finish_refresh_batch(
    out: &mut dyn Write,
    outcomes: Vec<RefreshOutcome>,
) -> Result<(), FeedError> {
    let total = outcomes.len();
    let mut failed = 0;
    for outcome in outcomes {
        let bad = match &outcome {
            RefreshOutcome::Failed { .. } => true,
            RefreshOutcome::Updated { followup, .. }
            | RefreshOutcome::Unchanged { followup, .. } => followup.is_some(),
        };
        write_refresh(out, &outcome)?;
        failed += usize::from(bad);
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(FeedError::BatchIncomplete { failed, total })
    }
}

/// `tenuto refresh <slug>` (§6.4): the concrete error, after printing what
/// did commit. A batch count would tell a single-feed caller nothing it did
/// not already know.
pub(crate) fn finish_refresh_one(
    out: &mut dyn Write,
    outcome: RefreshOutcome,
) -> Result<(), FeedError> {
    write_refresh(out, &outcome)?;
    match outcome {
        RefreshOutcome::Failed { error, .. } => Err(error),
        RefreshOutcome::Updated { followup, .. } | RefreshOutcome::Unchanged { followup, .. } => {
            followup.map_or(Ok(()), |failure| Err(failure.error))
        }
    }
}

/// `tenuto subscribe` (§5.3, §6.4). The commit order is cache first,
/// subscription second, so the only step that can fail after something
/// landed is the subscription — and when it does, the cache file is left
/// behind unreferenced and *nothing is subscribed*. Reporting that as a
/// subscription would send the listener looking for a feed that `tenuto
/// feeds` will not show.
pub(crate) fn finish_subscribe(
    out: &mut dyn Write,
    outcome: SubscribeOutcome,
) -> Result<(), FeedError> {
    let SubscribeOutcome {
        slug,
        retained,
        skipped,
        followup,
        ..
    } = outcome;
    let Some(failure) = followup else {
        return writeln!(
            out,
            "{slug}: subscribed, {retained} episodes retained, {skipped} skipped"
        )
        .map_err(stdout_failure);
    };
    writeln!(
        out,
        "{slug}: {retained} episodes were cached, but the subscription itself \
         could not be saved; nothing is subscribed: {}",
        failure.error
    )
    .map_err(stdout_failure)?;
    Err(failure.error)
}

/// `tenuto unsubscribe` (§5.3, §6.4). The subscription is removed first,
/// so a followup failure means the subscription is genuinely gone and only
/// its cache file remains — recoverable, and reported rather than silently
/// left behind.
pub(crate) fn finish_unsubscribe(
    out: &mut dyn Write,
    outcome: UnsubscribeOutcome,
) -> Result<(), FeedError> {
    let UnsubscribeOutcome { slug, followup } = outcome;
    let Some(failure) = followup else {
        return writeln!(out, "{slug}: unsubscribed").map_err(stdout_failure);
    };
    writeln!(
        out,
        "{slug}: the subscription was removed, but its cached episodes \
         could not be deleted: {}",
        failure.error
    )
    .map_err(stdout_failure)?;
    Err(failure.error)
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
fn write_station_probe(
    out: &mut dyn Write,
    outcome: AddStationOutcome,
    action: &str,
) -> Result<(), FeedError> {
    match outcome {
        AddStationOutcome::Verified { slug, identity } => writeln!(
            out,
            "{slug}: {action}, verified — {}",
            station_identity_text(&identity)
        )
        .map_err(stdout_failure),
        AddStationOutcome::Unverified { slug, reason } => {
            writeln!(out, "{slug}: {action}, unverified: {reason}").map_err(stdout_failure)
        }
        // A duplicate add resolves to the station already saved (§10) and
        // is not itself an error.
        AddStationOutcome::AlreadySaved {
            slug,
            identity,
            reprobe_failure: None,
        } => writeln!(
            out,
            "{slug}: already saved, re-probed — {}",
            identity
                .as_ref()
                .map_or_else(|| ABSENT.to_string(), station_identity_text)
        )
        .map_err(stdout_failure),
        // A failed implicit re-probe is reported in the same line rather
        // than swallowed, exactly as an explicit `ReprobeStation`'s failure
        // is (`AddStationOutcome::AlreadySaved`'s own doc comment explains
        // why this field exists at all) — and, like `finish_subscribe` and
        // `finish_unsubscribe`'s own partial-failure arms, the text is
        // written and *then* the call still returns `Err`. Returning `Ok`
        // here would undo that fix at one remove: the reason would sit in
        // the string, but `report()` would call it a success, the tracing
        // line would drop its `failed:` prefix, and the browser would paint
        // it with `NoticeKind::Ok` instead of the error colour.
        AddStationOutcome::AlreadySaved {
            slug,
            identity,
            reprobe_failure: Some(reason),
        } => {
            writeln!(
                out,
                "{slug}: already saved; re-probe failed: {reason} (last known: {})",
                identity
                    .as_ref()
                    .map_or_else(|| ABSENT.to_string(), station_identity_text)
            )
            .map_err(stdout_failure)?;
            Err(FeedError::StationsUnreadable { reason })
        }
    }
}

/// `AddStation` (M7.1 design doc §6, §10).
pub(crate) fn finish_add_station(
    out: &mut dyn Write,
    outcome: AddStationOutcome,
) -> Result<(), FeedError> {
    write_station_probe(out, outcome, "added")
}

/// `ReprobeStation` (M7.1 design doc §6). [`reprobe_station`] only ever
/// produces [`AddStationOutcome::Verified`] on success — a station cannot
/// re-probe its way into being a duplicate of itself — but the outcome type
/// is shared with `AddStation`, so every arm is still handled.
///
/// [`reprobe_station`]: crate::library::reprobe_station
pub(crate) fn finish_reprobe_station(
    out: &mut dyn Write,
    outcome: AddStationOutcome,
) -> Result<(), FeedError> {
    write_station_probe(out, outcome, "re-probed")
}

/// `RemoveStation` (M7.1 design doc §6): a local edit, always successful once
/// [`crate::library::remove_station`] returns `Ok`.
pub(crate) fn finish_remove_station(
    out: &mut dyn Write,
    outcome: RemoveStationOutcome,
) -> Result<(), FeedError> {
    let RemoveStationOutcome { slug } = outcome;
    writeln!(out, "{slug}: removed").map_err(stdout_failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use time::{Date, Month, OffsetDateTime, UtcOffset};

    use crate::library::{
        EpisodeRow, FeedSummary, FollowupFailure, FollowupStep, Progress, RefreshOutcome,
        SubscribeOutcome, UnsubscribeOutcome,
    };
    use crate::media::id::{MediaId, NormalizedUrl};

    type Fallible = Result<(), Box<dyn std::error::Error>>;

    fn utc(
        year: i32,
        month: Month,
        day: u8,
        hour: u8,
        minute: u8,
    ) -> Result<OffsetDateTime, time::Error> {
        Ok(Date::from_calendar_date(year, month, day)?
            .with_hms(hour, minute, 0)?
            .assume_utc())
    }

    fn media(url: &str) -> Result<MediaId, Box<dyn std::error::Error>> {
        Ok(MediaId::RemoteUrl(NormalizedUrl::parse(url)?))
    }

    fn text(bytes: Vec<u8>) -> Result<String, Box<dyn std::error::Error>> {
        Ok(String::from_utf8(bytes)?)
    }

    fn minutes(value: u64) -> Duration {
        Duration::from_secs(value * 60)
    }

    /// A writer that fails on every call, so the "a command that could not
    /// report its result has not succeeded" rule can be asserted rather than
    /// read.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the pipe is gone"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("the pipe is gone"))
        }
    }

    #[test]
    fn durations_gain_an_hours_field_only_when_there_are_hours() {
        assert_eq!(duration_text(Duration::ZERO), "0:00");
        assert_eq!(duration_text(Duration::from_secs(74)), "1:14");
        assert_eq!(duration_text(Duration::from_secs(1394)), "23:14");
        assert_eq!(duration_text(Duration::from_secs(3600)), "1:00:00");
        assert_eq!(duration_text(Duration::from_secs(6120)), "1:42:00");
        assert_eq!(duration_text(Duration::from_secs(36061)), "10:01:01");
    }

    /// §6.2's two marks, each meaning one thing: parentheses are a duration
    /// the *feed* declared, a tilde is an estimated position. A declared
    /// duration never attaches to a cell that is not a position.
    #[test]
    fn the_progress_cell_marks_estimates_and_declared_durations_apart() {
        assert_eq!(
            progress_text(
                &Progress::Established(Duration::from_secs(1394)),
                Some(minutes(102))
            ),
            "23:14 / (1:42:00)"
        );
        assert_eq!(
            progress_text(
                &Progress::Estimated(Duration::from_secs(1082)),
                Some(minutes(99))
            ),
            "~18:02 / (1:39:00)"
        );
        assert_eq!(
            progress_text(&Progress::Established(Duration::from_secs(1394)), None),
            "23:14"
        );
        assert_eq!(
            progress_text(&Progress::Established(Duration::from_secs(4000)), None),
            "1:06:40"
        );
        assert_eq!(
            progress_text(&Progress::Played, Some(minutes(102))),
            "played"
        );
        assert_eq!(
            progress_text(&Progress::Unknown, Some(minutes(102))),
            "position unknown"
        );
        assert_eq!(progress_text(&Progress::None, Some(minutes(102))), "—");
    }

    /// The design doc's own §6.1 block, reproduced byte for byte: an
    /// unrefreshed subscription shows `—` episodes and `never`, and the
    /// timestamp is UTC with no seconds.
    #[test]
    fn the_feeds_table_matches_the_specified_columns() -> Fallible {
        let rows = vec![
            FeedSummary {
                slug: "radio-t".to_string(),
                title: Some("Радио-Т".to_string()),
                episodes: Some(412),
                last_refreshed_at: Some(utc(2026, Month::September, 11, 9, 14)?),
            },
            FeedSummary {
                slug: "sysdesign".to_string(),
                title: Some("System Design".to_string()),
                episodes: None,
                last_refreshed_at: None,
            },
        ];
        let mut out = Vec::new();
        write_feeds(&mut out, &rows)?;
        assert_eq!(
            text(out)?,
            "SLUG        EPISODES  REFRESHED (UTC)   TITLE\n\
             radio-t          412  2026-09-11 09:14  Радио-Т\n\
             sysdesign          —  never             System Design\n"
        );
        Ok(())
    }

    /// `--reverse` is display order only: `play <slug> <index>` resolves the
    /// index a row shows, so a reversed listing that renumbered would send the
    /// listener to a different episode than the one they read.
    #[test]
    fn reversing_keeps_every_index_and_counts_the_limit_from_the_end() -> Fallible {
        let rows = (1..=6)
            .map(|index| {
                Ok(EpisodeRow {
                    index,
                    media: media(&format!("https://cdn.example.org/{index}.mp3"))?,
                    title: Some(format!("{index}.")),
                    published: None,
                    declared_duration: None,
                    playable: true,
                    progress: Progress::None,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;

        let all: Vec<usize> = reversed(rows.clone(), None)
            .iter()
            .map(|row| row.index)
            .collect();
        assert_eq!(all, vec![6, 5, 4, 3, 2, 1]);

        let last_two: Vec<usize> = reversed(rows, std::num::NonZeroUsize::new(2))
            .iter()
            .map(|row| row.index)
            .collect();
        assert_eq!(
            last_two,
            vec![6, 5],
            "-n counts from the end, not the first N reversed"
        );
        Ok(())
    }

    /// The design doc's §6.1 episode block, reproduced byte for byte.
    #[test]
    fn the_episodes_table_matches_the_specified_columns() -> Fallible {
        let rows = vec![
            EpisodeRow {
                index: 1,
                media: media("https://cdn.example.org/987.mp3")?,
                title: Some("Радио-Т 987".to_string()),
                published: Some(utc(2026, Month::September, 6, 0, 0)?),
                declared_duration: None,
                playable: true,
                progress: Progress::None,
            },
            EpisodeRow {
                index: 2,
                media: media("https://cdn.example.org/986.mp3")?,
                title: Some("Радио-Т 986".to_string()),
                published: Some(utc(2026, Month::August, 30, 0, 0)?),
                declared_duration: Some(minutes(102)),
                playable: true,
                progress: Progress::Established(Duration::from_secs(1394)),
            },
            EpisodeRow {
                index: 3,
                media: media("https://cdn.example.org/985.mp3")?,
                title: Some("Радио-Т 985".to_string()),
                published: Some(utc(2026, Month::August, 23, 0, 0)?),
                declared_duration: None,
                playable: true,
                progress: Progress::Played,
            },
            EpisodeRow {
                index: 4,
                media: media("https://cdn.example.org/984.mp3")?,
                title: Some("Радио-Т 984".to_string()),
                published: Some(utc(2026, Month::August, 16, 0, 0)?),
                declared_duration: Some(minutes(99)),
                playable: true,
                progress: Progress::Estimated(Duration::from_secs(1082)),
            },
            EpisodeRow {
                index: 5,
                media: media("https://cdn.example.org/bonus.mp3")?,
                title: Some("Bonus: outtakes".to_string()),
                published: Some(utc(2026, Month::August, 9, 0, 0)?),
                declared_duration: None,
                playable: false,
                progress: Progress::Established(Duration::from_secs(1394)),
            },
        ];
        let mut out = Vec::new();
        write_episodes(&mut out, &rows)?;
        assert_eq!(
            text(out)?,
            "  #  PROGRESS            AUDIO  PUBLISHED (UTC)  TITLE\n  \
             1  —                   -      2026-09-06       Радио-Т 987\n  \
             2  23:14 / (1:42:00)   -      2026-08-30       Радио-Т 986\n  \
             3  played              -      2026-08-23       Радио-Т 985\n  \
             4  ~18:02 / (1:39:00)  -      2026-08-16       Радио-Т 984\n  \
             5  23:14               none   2026-08-09       Bonus: outtakes\n"
        );
        Ok(())
    }

    /// Every displayed timestamp is UTC and says so in the header, so an
    /// input carrying an offset has to be converted rather than printed as
    /// stored — a local-looking time under a `(UTC)` label is worse than no
    /// time at all.
    #[test]
    fn timestamps_are_converted_to_utc_not_printed_as_stored() -> Fallible {
        let moscow = Date::from_calendar_date(2026, Month::September, 11)?
            .with_hms(12, 14, 0)?
            .assume_offset(UtcOffset::from_hms(3, 0, 0)?);
        let rows = vec![FeedSummary {
            slug: "radio-t".to_string(),
            title: None,
            episodes: Some(1),
            last_refreshed_at: Some(moscow),
        }];
        let mut out = Vec::new();
        write_feeds(&mut out, &rows)?;
        let rendered = text(out)?;
        assert!(rendered.contains("2026-09-11 09:14"), "{rendered}");
        assert!(!rendered.contains("12:14"), "{rendered}");
        Ok(())
    }

    /// A title is feed-supplied text: a newline in it would break the row
    /// layout and an escape sequence would reach the terminal, so both are
    /// made visible at the formatting boundary. Nothing is transliterated —
    /// only control characters change.
    #[test]
    fn titles_are_defanged_without_being_rewritten() -> Fallible {
        let rows = vec![
            FeedSummary {
                slug: "broken".to_string(),
                title: Some("two\nlines\u{1b}[31m".to_string()),
                episodes: Some(1),
                last_refreshed_at: None,
            },
            FeedSummary {
                slug: "nameless".to_string(),
                title: None,
                episodes: Some(0),
                last_refreshed_at: None,
            },
        ];
        let mut out = Vec::new();
        write_feeds(&mut out, &rows)?;
        let rendered = text(out)?;
        assert_eq!(rendered.lines().count(), 3, "{rendered}");
        assert!(rendered.contains(r"two\nlines\u{1b}[31m"), "{rendered}");
        assert!(rendered.contains("(untitled)"), "{rendered}");
        Ok(())
    }

    /// Not every line breaker is a control character, and not every
    /// dangerous character is a line breaker. U+2028 LINE SEPARATOR would
    /// split a row in a terminal that honors it, a bidi override
    /// (U+202A–U+202E) can reorder everything rendered after it — including
    /// the columns beside the title — and a bidi isolate (U+2066–U+2069) can
    /// do the same reordering while wrapping only its own contents, which is
    /// what makes isolates the half of Trojan Source that survives once
    /// overrides alone are blocked. `char::is_control` classifies none of
    /// the three as a control.
    #[test]
    fn line_separators_and_bidi_overrides_are_escaped_too() -> Fallible {
        let rows = vec![EpisodeRow {
            index: 1,
            media: media("https://cdn.example.org/a.mp3")?,
            title: Some("split\u{2028}here\u{202e}reversed\u{2066}isolated\u{2069}".to_string()),
            published: None,
            declared_duration: None,
            playable: true,
            progress: Progress::Played,
        }];
        let mut out = Vec::new();
        write_episodes(&mut out, &rows)?;
        let rendered = text(out)?;
        assert_eq!(rendered.lines().count(), 2, "{rendered:?}");
        assert!(!rendered.contains('\u{2028}'), "{rendered:?}");
        assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
        assert!(!rendered.contains('\u{2066}'), "{rendered:?}");
        assert!(!rendered.contains('\u{2069}'), "{rendered:?}");
        assert!(
            rendered.contains(r"split\u{2028}here\u{202e}reversed\u{2066}isolated\u{2069}"),
            "{rendered:?}"
        );
        Ok(())
    }

    /// An empty listing is a successful listing: the header still names the
    /// columns, and nothing claims a subscription that is not there. The two
    /// minimum widths hold with no rows to measure, so an empty table starts
    /// where a populated one does.
    #[test]
    fn empty_listings_print_their_header_and_succeed() -> Fallible {
        let mut feeds = Vec::new();
        write_feeds(&mut feeds, &[])?;
        assert_eq!(
            text(feeds)?,
            "SLUG        EPISODES  REFRESHED (UTC)  TITLE\n"
        );

        let mut episodes = Vec::new();
        write_episodes(&mut episodes, &[])?;
        assert_eq!(
            text(episodes)?,
            "  #  PROGRESS  AUDIO  PUBLISHED (UTC)  TITLE\n"
        );
        Ok(())
    }

    /// A missing publication date is `—`, never today's date and never an
    /// invented one.
    ///
    /// The row deliberately carries a progress cell that is *not* `—`: with
    /// `Progress::None` the same dash appears two columns earlier, and the
    /// assertion would pass however the published column rendered.
    #[test]
    fn an_undated_episode_prints_an_em_dash() -> Fallible {
        let rows = vec![EpisodeRow {
            index: 1,
            media: media("https://cdn.example.org/a.mp3")?,
            title: Some("A".to_string()),
            published: None,
            declared_duration: None,
            playable: true,
            progress: Progress::Played,
        }];
        let mut out = Vec::new();
        write_episodes(&mut out, &rows)?;
        let rendered = text(out)?;
        let row = rendered.lines().nth(1).ok_or("expected one episode row")?;
        // Split at the progress cell, so the dash can only have come from
        // the published column.
        let (before, after) = row
            .split_once("played")
            .ok_or("expected the progress cell")?;
        assert!(!before.contains('—'), "{row}");
        assert!(after.contains('—'), "{row}");
        Ok(())
    }

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

    #[test]
    fn a_clean_refresh_reports_what_changed() -> Fallible {
        let mut out = Vec::new();
        write_refresh(
            &mut out,
            &RefreshOutcome::Updated {
                slug: "radio-t".to_string(),
                retained: 412,
                skipped: 3,
                url_moved: None,
                followup: None,
            },
        )?;
        assert_eq!(
            text(out)?,
            "radio-t: updated, 412 episodes retained, 3 skipped\n"
        );

        let mut out = Vec::new();
        write_refresh(
            &mut out,
            &RefreshOutcome::Unchanged {
                slug: "radio-t".to_string(),
                url_moved: None,
                followup: None,
            },
        )?;
        assert_eq!(text(out)?, "radio-t: unchanged\n");
        Ok(())
    }

    /// §6.6: a 304 can still follow a permanent redirect, and that commit can
    /// fail on its own. The redirect is reported, the cause is named, and the
    /// URL is redacted at presentation even though the library already
    /// supplied safe text.
    #[test]
    fn a_revalidated_feed_reports_a_failed_redirect_commit() -> Fallible {
        let mut out = Vec::new();
        write_refresh(
            &mut out,
            &RefreshOutcome::Unchanged {
                slug: "radio-t".to_string(),
                url_moved: Some("https://example.org/new?token=secret".to_string()),
                followup: saved(unreadable()),
            },
        )?;
        let rendered = text(out)?;
        assert!(rendered.starts_with("radio-t: unchanged\n"), "{rendered}");
        assert!(rendered.contains("https://example.org/new"), "{rendered}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(
            rendered.contains("the redirected feed URL could not be recorded"),
            "{rendered}"
        );
        assert!(rendered.contains("the disk is full"), "{rendered}");
        Ok(())
    }

    /// Without a redirect the same step means the feed's own metadata
    /// changed, and saying "redirect" there would describe something that
    /// did not happen.
    #[test]
    fn a_failed_metadata_commit_does_not_claim_a_redirect() -> Fallible {
        let mut out = Vec::new();
        write_refresh(
            &mut out,
            &RefreshOutcome::Updated {
                slug: "radio-t".to_string(),
                retained: 5,
                skipped: 0,
                url_moved: None,
                followup: saved(unreadable()),
            },
        )?;
        let rendered = text(out)?;
        assert!(!rendered.contains("redirect"), "{rendered}");
        assert!(
            rendered.contains("the feed's changed title could not be recorded"),
            "{rendered}"
        );
        Ok(())
    }

    /// §6.4: one bad feed neither hides the others nor exits zero. Every
    /// outcome is printed before the batch error is returned, and the error
    /// counts the feeds rather than naming one.
    #[test]
    fn a_mixed_batch_prints_everything_then_reports_the_count() -> Fallible {
        let outcomes = vec![
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
        ];
        let mut out = Vec::new();
        let error = finish_refresh_batch(&mut out, outcomes)
            .err()
            .ok_or("a batch with two bad feeds must not succeed")?;
        let rendered = text(out)?;
        assert!(rendered.contains("radio-t: updated"), "{rendered}");
        assert!(rendered.contains("sysdesign: failed"), "{rendered}");
        assert!(rendered.contains("late: unchanged"), "{rendered}");
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
        let mut out = Vec::new();
        finish_refresh_batch(
            &mut out,
            vec![RefreshOutcome::Unchanged {
                slug: "radio-t".to_string(),
                url_moved: None,
                followup: None,
            }],
        )?;
        assert_eq!(text(out)?, "radio-t: unchanged\n");

        let mut empty = Vec::new();
        finish_refresh_batch(&mut empty, Vec::new())?;
        assert!(empty.is_empty());
        Ok(())
    }

    /// §6.4: a single-feed command returns the concrete error — not a batch
    /// count — after printing what did commit.
    #[test]
    fn one_feed_returns_its_own_error_after_printing() -> Fallible {
        let mut out = Vec::new();
        let error = finish_refresh_one(
            &mut out,
            RefreshOutcome::Failed {
                slug: "radio-t".to_string(),
                error: FeedError::UnsupportedFormat,
            },
        )
        .err()
        .ok_or("a failed refresh must not succeed")?;
        assert!(text(out)?.contains("radio-t: failed"), "nothing printed");
        assert!(matches!(error, FeedError::UnsupportedFormat), "{error:?}");

        let mut out = Vec::new();
        let error = finish_refresh_one(
            &mut out,
            RefreshOutcome::Updated {
                slug: "radio-t".to_string(),
                retained: 2,
                skipped: 0,
                url_moved: None,
                followup: saved(unreadable()),
            },
        )
        .err()
        .ok_or("a followup failure must not succeed")?;
        let rendered = text(out)?;
        assert!(
            rendered.contains("radio-t: updated, 2 episodes"),
            "{rendered}"
        );
        assert!(
            matches!(error, FeedError::SubscriptionsUnreadable { .. }),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn subscribing_prints_its_slug_and_counts() -> Fallible {
        let mut out = Vec::new();
        finish_subscribe(
            &mut out,
            SubscribeOutcome {
                slug: "radio-t".to_string(),
                feed_id: crate::media::id::FeedId::new(
                    "0123456789abcdef0123456789abcdef".to_string(),
                )?,
                title: Some("Радио-Т".to_string()),
                retained: 412,
                skipped: 3,
                followup: None,
            },
        )?;
        assert_eq!(
            text(out)?,
            "radio-t: subscribed, 412 episodes retained, 3 skipped\n"
        );
        Ok(())
    }

    /// §5.3's commit order is cache first, subscription second, so a
    /// followup failure here means nothing is subscribed — and the line must
    /// say so rather than reporting a subscription that does not exist.
    #[test]
    fn a_half_committed_subscribe_never_claims_success() -> Fallible {
        let mut out = Vec::new();
        let error = finish_subscribe(
            &mut out,
            SubscribeOutcome {
                slug: "radio-t".to_string(),
                feed_id: crate::media::id::FeedId::new(
                    "0123456789abcdef0123456789abcdef".to_string(),
                )?,
                title: None,
                retained: 412,
                skipped: 0,
                followup: saved(unreadable()),
            },
        )
        .err()
        .ok_or("a half-committed subscribe must not succeed")?;
        let rendered = text(out)?;
        assert!(!rendered.contains("radio-t: subscribed,"), "{rendered}");
        assert!(rendered.contains("nothing is subscribed"), "{rendered}");
        assert!(rendered.contains("412 episodes were cached"), "{rendered}");
        assert!(
            matches!(error, FeedError::SubscriptionsUnreadable { .. }),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn unsubscribing_reports_a_cleanup_failure_as_a_failure() -> Fallible {
        let mut out = Vec::new();
        finish_unsubscribe(
            &mut out,
            UnsubscribeOutcome {
                slug: "radio-t".to_string(),
                followup: None,
            },
        )?;
        assert_eq!(text(out)?, "radio-t: unsubscribed\n");

        let mut out = Vec::new();
        let error = finish_unsubscribe(
            &mut out,
            UnsubscribeOutcome {
                slug: "radio-t".to_string(),
                followup: Some(FollowupFailure {
                    step: FollowupStep::RemoveCache,
                    error: FeedError::UnsupportedFormat,
                }),
            },
        )
        .err()
        .ok_or("a failed cleanup must not succeed")?;
        let rendered = text(out)?;
        assert!(
            rendered.contains("the subscription was removed"),
            "{rendered}"
        );
        assert!(
            rendered.contains("cached episodes could not be deleted"),
            "{rendered}"
        );
        assert!(matches!(error, FeedError::UnsupportedFormat), "{error:?}");
        Ok(())
    }

    #[test]
    fn adding_a_verified_station_prints_its_identity() -> Fallible {
        let mut out = Vec::new();
        finish_add_station(
            &mut out,
            AddStationOutcome::Verified {
                slug: "test-radio".to_string(),
                identity: StationIdentity {
                    name: Some("Test Radio".to_string()),
                    genre: Some("Lofi".to_string()),
                    bitrate_kbps: Some(128),
                    logo: None,
                },
            },
        )?;
        assert_eq!(
            text(out)?,
            "test-radio: added, verified — Test Radio · Lofi · 128 kbps\n"
        );
        Ok(())
    }

    /// `AddStationOutcome::AlreadySaved::reprobe_failure` exists so that a
    /// duplicate add's implicit re-probe failure reaches the caller instead
    /// of being swallowed by the "already saved" framing (M7.1 §6, §10); this
    /// is the presentation-layer half of that fix — the reason must show up
    /// in the printed line, not just in the value passed to `finish_add_station`.
    #[test]
    fn a_duplicate_adds_failed_reprobe_is_not_swallowed() -> Fallible {
        let mut out = Vec::new();
        let error = finish_add_station(
            &mut out,
            AddStationOutcome::AlreadySaved {
                slug: "test-radio".to_string(),
                identity: Some(StationIdentity {
                    name: Some("Test Radio".to_string()),
                    genre: None,
                    bitrate_kbps: None,
                    logo: None,
                }),
                reprobe_failure: Some("connection reset".to_string()),
            },
        )
        .err()
        .ok_or("a failed implicit re-probe must not report success")?;
        // Not just the text: `report()` (`src/commands.rs`) only calls
        // `tracing::info!` with a `failed:` prefix, and the browser
        // (`src/tui/browser.rs`) only paints `NoticeKind::Err`, when this
        // call returns `Err` — exactly like `finish_subscribe`'s and
        // `finish_unsubscribe`'s own partial-failure arms. A `contains()`
        // check on the string alone would pass even if this returned `Ok`.
        assert!(
            matches!(error, FeedError::StationsUnreadable { .. }),
            "{error:?}"
        );
        let rendered = text(out)?;
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
        let mut out = Vec::new();
        finish_remove_station(
            &mut out,
            RemoveStationOutcome {
                slug: "test-radio".to_string(),
            },
        )?;
        assert_eq!(text(out)?, "test-radio: removed\n");
        Ok(())
    }

    /// `play` is dispatched by `app::run`, which resolves both of its forms
    /// itself. If it ever reaches the command table anyway, the refusal must
    /// not echo the argument back: a `play` source can be a URL, and §7.2
    /// forbids repeating one under `Debug` as much as under `Display`.
    #[test]
    fn play_is_refused_here_without_repeating_its_argument() -> Fallible {
        let error = run(CliCommand::Play {
            source: "https://example.org/feed?token=secret".to_string(),
            index: None,
            probe_only: false,
        })
        .err()
        .ok_or("play must not be handled by the command table")?;
        assert!(
            !format!("{error:?} {error}").contains("secret"),
            "{error:?}"
        );
        assert!(matches!(error, FeedError::Malformed { .. }), "{error:?}");
        Ok(())
    }

    /// A write that failed is never reported as a command that succeeded,
    /// and the failure names the stream it could not reach.
    #[test]
    fn an_unwritable_stream_fails_the_command() -> Fallible {
        let rows = vec![FeedSummary {
            slug: "radio-t".to_string(),
            title: None,
            episodes: None,
            last_refreshed_at: None,
        }];
        let error = write_feeds(&mut Broken, &rows)
            .err()
            .ok_or("a broken writer must not report success")?;
        // The rendering is pinned, not just the fields (R10): this variant
        // covers `state.json`, `subscriptions.json`, the feed cache and
        // stdout, so its message must name what `op` and `path` say and
        // nothing else.
        assert_eq!(
            error.to_string(),
            "cannot write command output to \"<stdout>\""
        );
        match error {
            FeedError::Persistence(PersistenceError::Io { path, op, .. }) => {
                assert_eq!(path, PathBuf::from("<stdout>"));
                assert_eq!(op, "write command output to");
            }
            other => return Err(format!("expected an Io failure, got {other:?}").into()),
        }

        assert!(
            finish_unsubscribe(
                &mut Broken,
                UnsubscribeOutcome {
                    slug: "radio-t".to_string(),
                    followup: None,
                },
            )
            .is_err()
        );
        Ok(())
    }
}
