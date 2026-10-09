//! What the status row shows for a track and its position — the human-facing
//! spellings of a `MediaId`, a duration and a row that has to fit one
//! terminal line. Nothing here reads playback state; each function turns one
//! already-known value into text.

use std::time::Duration;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use url::Url;

use crate::media::id::MediaId;
use crate::telemetry::redact_url;

/// What the status row calls a podcast episode: its title.
///
/// `display_name` gives a local file its basename and a remote URL its last
/// path segment, and a podcast episode fell through to the canonical id —
/// `podcast:<feed>/guid:https:%2F%2F…`, about 110 characters of identifier and
/// nothing a listener recognises. The decoder's title is the same name
/// `--probe-only` prints and the one `tenuto episodes` lists, so `play
/// radio-t 1` now reads the way the listing that chose it did. Untitled audio
/// gets the `(untitled)` spelling the probe and the listing already use.
pub fn episode_name(title: Option<&str>) -> String {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map_or_else(|| "(untitled episode)".to_owned(), str::to_owned)
}

pub fn display_name(media: &MediaId) -> String {
    match media {
        MediaId::LocalFile(path) => path
            .as_path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| media.to_string()),
        MediaId::RemoteUrl(url) => remote_display_name(url.as_str()),
        other => other.to_string(),
    }
}

/// §11: a status line must never carry a signed query or embedded
/// credentials, so this is built from `redact_url`'s output rather than the
/// URL itself — the last path segment, or the redacted host when there is
/// none.
pub fn remote_display_name(url: &str) -> String {
    let redacted = redact_url(url);
    match Url::parse(&redacted) {
        Ok(parsed) => parsed
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .or_else(|| parsed.host_str().map(str::to_string))
            .unwrap_or(redacted),
        Err(_) => redacted,
    }
}

/// Cut `text` to `width` terminal columns, marking the cut with an ellipsis.
///
/// A row wider than the terminal wraps onto a second physical line, and
/// `render`'s `MoveUp(1)` then lands inside the frame it was trying to
/// overwrite, so every repaint walks one row further down the screen. A
/// podcast episode's canonical id runs to about 110 characters and makes that
/// reachable with ordinary input, but a long enough filename always could.
///
/// Measured in terminal columns, not characters: a two-byte Cyrillic letter
/// is one column but a CJK glyph is two, and counting either as "a char"
/// would let a CJK title run past the edge and wrap. The prefix is taken by
/// accumulated column width, leaving one column for the ellipsis, so the
/// result never exceeds `width` even when the cut lands beside a wide glyph.
pub fn fit_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let budget = width - 1;
    let mut used = 0;
    let mut fitted: String = text
        .chars()
        .take_while(|glyph| {
            used += UnicodeWidthChar::width(*glyph).unwrap_or(0);
            used <= budget
        })
        .collect();
    fitted.push('…');
    fitted
}

pub fn format_hms(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::id::{EpisodeKey, FeedId, NormalizedUrl};

    // -------------------------------------------------------- display_name

    #[test]
    fn a_remote_display_name_is_the_last_path_segment_with_the_query_stripped() {
        let media = MediaId::RemoteUrl(
            NormalizedUrl::parse("https://cdn.example.com/shows/ep-1.mp3?token=secret")
                .unwrap_or_else(|error| panic!("a well-formed URL must parse: {error}")),
        );
        let name = display_name(&media);
        assert_eq!(name, "ep-1.mp3");
        assert!(
            !name.contains("token"),
            "the query leaked into the name: {name}"
        );
    }

    #[test]
    fn a_remote_display_name_falls_back_to_the_host_with_no_path() {
        let media = MediaId::RemoteUrl(
            NormalizedUrl::parse("https://cdn.example.com")
                .unwrap_or_else(|error| panic!("a well-formed URL must parse: {error}")),
        );
        assert_eq!(display_name(&media), "cdn.example.com");
    }

    // -------------------------------------------------------- fit_to_width

    /// The bug this exists for: a podcast episode's canonical id is far wider
    /// than a terminal, and an over-wide row made `render`'s repaint walk down
    /// the screen instead of overwriting itself.
    #[test]
    fn a_media_id_wider_than_the_terminal_is_cut_to_one_row() {
        let media = MediaId::PodcastEpisode {
            feed: FeedId::new("beface6be47994b61e579fb92384dfb9".to_owned())
                .expect("a valid feed id"),
            episode: EpisodeKey::resolve(
                Some("https://radio-t.com/p/2026/09/12//podcast-1030/"),
                None,
                None,
            )
            .expect("a valid episode key"),
        };
        let name = display_name(&media);
        assert!(name.chars().count() > 80, "precondition: {name}");
        let fitted = fit_to_width(&name, 80);
        assert_eq!(fitted.chars().count(), 80);
        assert!(fitted.ends_with('…'));
    }

    /// Counting by `char` rather than by byte: a byte-wise cut lands inside a
    /// two-byte Cyrillic letter and panics, and Cyrillic episode titles are
    /// the common case for the feed that surfaced this.
    #[test]
    fn a_cyrillic_row_is_cut_between_characters_not_inside_one() {
        let fitted = fit_to_width("Радио-Т 1030 играет прямо сейчас", 10);
        assert_eq!(fitted.chars().count(), 10);
        assert_eq!(fitted, "Радио-Т 1…");
    }

    /// Width is terminal columns, not characters: a CJK glyph takes two. Ten
    /// of them are twenty columns, and passing them through a ten-column
    /// budget untouched is exactly the wrap the function exists to prevent.
    #[test]
    fn a_wide_glyph_counts_as_two_columns() {
        let wide = "界".repeat(10);
        assert_eq!(fit_to_width(&wide, 20), wide);
        let fitted = fit_to_width(&wide, 10);
        assert_eq!(fitted, format!("{}…", "界".repeat(4)));
        assert_eq!(UnicodeWidthStr::width(fitted.as_str()), 9);
        // A budget that lands mid-glyph stops one column short rather than over.
        assert_eq!(UnicodeWidthStr::width(fit_to_width(&wide, 9).as_str()), 9);
    }

    #[test]
    fn a_row_that_already_fits_is_left_exactly_alone() {
        assert_eq!(fit_to_width("short", 80), "short");
        assert_eq!(fit_to_width("exactly-ten", 11), "exactly-ten");
        assert_eq!(fit_to_width("anything", 0), "");
    }

    // ------------------------------------------------ episode name and fit

    #[test]
    fn a_podcast_episode_is_named_by_its_title_not_its_canonical_id() {
        assert_eq!(episode_name(Some("Радио-Т 1030")), "Радио-Т 1030");
        assert_eq!(episode_name(Some("  padded  ")), "padded");
        assert_eq!(episode_name(None), "(untitled episode)");
        assert_eq!(episode_name(Some("   ")), "(untitled episode)");
    }
}
