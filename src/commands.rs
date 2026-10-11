//! The M4 presentation layer (design doc §6.1–§6.4): the only place that
//! formats a feed command's output, decides its exit status, and bridges
//! [`crate::library`]'s async functions into a synchronous `main`.
//!
//! [`crate::library`] returns values and prints nothing; this module turns
//! those values into the columns §6.1 specifies and into the `Err` that
//! §6.4's "a partial failure cannot exit successfully" requires. The split
//! is what lets M5 reuse the library with a different presentation, and what
//! keeps `block_on` out of `library`. The operations themselves, and the
//! one synchronous bridge into the runtime, are
//! [`crate::application::feed_ops`]'s; the artwork worker blocks on its own
//! (architecture §5).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use time::{OffsetDateTime, UtcOffset};

use crate::application::feed_ops::{self, ABSENT, FeedOp, Report};
use crate::application::runtime::LibraryStores;
use crate::cli::CliCommand;
use crate::clock::SystemClock;
use crate::feed::error::FeedError;
use crate::library::{EpisodeRow, FeedSummary, Progress};
use crate::persistence::PersistenceError;
use crate::persistence::store::StateStore;
use crate::telemetry::displayable;

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
            let LibraryStores {
                subscriptions: subs,
                cache,
                ..
            } = LibraryStores::platform()?;
            write_feeds(&mut out, &crate::library::list_feeds(&subs, &cache)?)
        }
        CliCommand::Episodes {
            slug,
            limit,
            reverse,
        } => {
            let LibraryStores {
                subscriptions: subs,
                cache,
                ..
            } = LibraryStores::platform()?;
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
        CliCommand::Subscribe { url, slug } => operate(&mut out, &FeedOp::Subscribe { url, slug })?,
        CliCommand::Unsubscribe { slug } => operate(&mut out, &FeedOp::Unsubscribe { slug })?,
        CliCommand::Refresh { slug } => operate(&mut out, &FeedOp::Refresh { slug })?,
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

/// The checkpoint store, on the same path playback itself uses —
/// [`StateStore::platform_path`] stays the one discovery point for it (§13).
pub(crate) fn platform_state_store() -> Result<StateStore, FeedError> {
    Ok(StateStore::new(
        StateStore::platform_path()?,
        Arc::new(SystemClock),
    ))
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

/// A feed operation, printed. The outer `Err` is an operation that failed
/// before it had anything to print, as a store, spawn or library call can:
/// it returns before the final flush, as it always has. The inner result is
/// the printed report's: a stdout failure outranks the operation's own
/// error, and the final flush outranks both.
fn operate(out: &mut dyn Write, op: &FeedOp) -> Result<Result<(), FeedError>, FeedError> {
    // Held until the report is written: dropping the last service shuts its
    // runtime down, which waits for blocking tasks such as a slow DNS lookup,
    // and that wait belongs after the output, not before it.
    let mut http = None;
    let report = feed_ops::run(op, &LibraryStores::platform()?, &mut http);
    if report.text.is_empty()
        && let Err(error) = report.outcome
    {
        return Err(error);
    }
    Ok(write_report(out, report))
}

/// The report's text on stdout, then its outcome. A command that could not
/// report its own result has not succeeded, however far its work got.
fn write_report(out: &mut dyn Write, report: Report) -> Result<(), FeedError> {
    out.write_all(report.text.as_bytes())
        .map_err(stdout_failure)
        .and(report.outcome)
}

// --- Formatting (§6.1, §6.2) -----------------------------------------

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use time::{Date, Month, OffsetDateTime, UtcOffset};

    use crate::library::{EpisodeRow, FeedSummary, Progress};
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

        // A report whose operation also failed: the stdout failure is what
        // the command reports, as it always was.
        let error = write_report(
            &mut Broken,
            Report {
                text: "radio-t: unsubscribed\n".to_string(),
                outcome: Err(FeedError::UnsupportedFormat),
            },
        )
        .err()
        .ok_or("a broken writer must not report success")?;
        assert_eq!(
            error.to_string(),
            "cannot write command output to \"<stdout>\""
        );
        Ok(())
    }
}
