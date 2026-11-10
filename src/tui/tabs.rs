//! The playlist tab strip (M8 §10): which labels fit, in terminal columns.
//! Pure — `render` draws what this returns. Names arrive already sanitized
//! (`PlaylistTab::name`).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::application::view::PlaylistTab;
use crate::playlist::PlaylistId;

pub const PLAYING_MARK: &str = "▶";
/// Letters, not a glyph: no monospace font carries a shuffle sign, and one
/// drawn from a fallback font drifts off the cell grid or comes out tiny.
pub const SHUFFLE_MARK: &str = " ·shfl";
pub const GAP: &str = "  ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabLabel {
    pub id: PlaylistId,
    pub text: String,
    pub viewed: bool,
    pub playing: bool,
}

fn label(tab: &PlaylistTab, repeat: Option<&str>) -> String {
    let badge = match repeat {
        // M13 §1.1: the badge is global, so it rides the playing tab —
        // where the playing mark already is.
        Some(badge) if tab.playing => format!(" {badge}"),
        _ => String::new(),
    };
    format!(
        "{}{}{}{}",
        if tab.playing { PLAYING_MARK } else { "" },
        tab.name,
        if tab.shuffled { SHUFFLE_MARK } else { "" },
        badge,
    )
}

/// `text` cut to `width` columns, ending in `…` when anything was cut.
///
/// ponytail: walks `char`s, not grapheme clusters, so a cut can land between
/// a base character and its combining mark. Upgrade path: `unicode-segmentation`
/// — a new dependency for a playlist name, hence deferred.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// The labels that fit in `width` columns, always including the viewed tab:
/// as many tabs before it as fit with it, then as many after.
pub fn strip(
    tabs: &[PlaylistTab],
    viewed: PlaylistId,
    width: usize,
    repeat: Option<&str>,
) -> Vec<TabLabel> {
    if width == 0 || tabs.is_empty() {
        return Vec::new();
    }
    let at = tabs.iter().position(|tab| tab.id == viewed).unwrap_or(0);
    let texts: Vec<String> = tabs.iter().map(|tab| label(tab, repeat)).collect();
    let cost = |index: usize| texts[index].width() + GAP.width();

    let mut used = texts[at].width().min(width);
    let (mut start, mut end) = (at, at + 1);
    while end < tabs.len() && used + cost(end) <= width {
        used += cost(end);
        end += 1;
    }
    while start > 0 && used + cost(start - 1) <= width {
        used += cost(start - 1);
        start -= 1;
    }
    (start..end)
        .map(|index| TabLabel {
            id: tabs[index].id,
            text: clip(&texts[index], width),
            // `at` is the viewed tab, or the first tab for an ID that is gone.
            viewed: index == at,
            playing: tabs[index].playing,
        })
        .collect()
}

/// The Minimal tier's one-row form: the viewed label and `n/m`.
pub fn compact(
    tabs: &[PlaylistTab],
    viewed: PlaylistId,
    width: usize,
    repeat: Option<&str>,
) -> String {
    let at = tabs.iter().position(|tab| tab.id == viewed).unwrap_or(0);
    let Some(tab) = tabs.get(at) else {
        return String::new();
    };
    let place = format!(" {}/{}", at + 1, tabs.len());
    let room = width.saturating_sub(place.width());
    clip(
        &format!("{}{place}", clip(&label(tab, repeat), room)),
        width,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn tab(id: u64, name: &str, playing: bool, shuffled: bool) -> PlaylistTab {
        PlaylistTab {
            id: PlaylistId::from_raw_for_tests(id),
            name: name.to_owned(),
            playing,
            shuffled,
        }
    }

    fn texts(labels: &[TabLabel]) -> Vec<&str> {
        labels.iter().map(|label| label.text.as_str()).collect()
    }

    fn columns(labels: &[TabLabel]) -> usize {
        labels.iter().map(|label| label.text.width()).sum::<usize>()
            + GAP.width() * labels.len().saturating_sub(1)
    }

    #[test]
    fn marks_sit_on_the_playing_and_the_shuffled_tab() {
        let tabs = [
            tab(1, "Default", false, false),
            tab(2, "Morning", false, true),
            tab(3, "Workout", true, true),
        ];
        let labels = strip(&tabs, PlaylistId::from_raw_for_tests(2), 80, None);
        assert_eq!(
            texts(&labels),
            ["Default", "Morning ·shfl", "▶Workout ·shfl"]
        );
        assert_eq!(
            labels
                .iter()
                .map(|l| (l.viewed, l.playing))
                .collect::<Vec<_>>(),
            [(false, false), (true, false), (false, true)]
        );
    }

    #[test]
    fn the_window_scrolls_to_keep_the_viewed_tab_and_never_exceeds_the_width() {
        let tabs: Vec<_> = (1..=9)
            .map(|i| tab(i, &format!("Playlist{i}"), false, false))
            .collect();
        for viewed in 1..=9 {
            let labels = strip(&tabs, PlaylistId::from_raw_for_tests(viewed), 34, None);
            assert!(
                labels.iter().any(|label| label.viewed),
                "viewed {viewed} is on screen"
            );
            assert!(
                columns(&labels) <= 34,
                "viewed {viewed}: {} columns",
                columns(&labels)
            );
        }
    }

    #[test]
    fn width_is_measured_in_columns_not_bytes_or_chars() {
        // Each CJK character is two columns and three bytes.
        let tabs = [tab(1, "音楽音楽", false, false), tab(2, "ab", false, false)];
        let labels = strip(&tabs, PlaylistId::from_raw_for_tests(1), 10, None);
        assert_eq!(
            texts(&labels),
            ["音楽音楽"],
            "8 columns + gap + 2 = 12 > 10"
        );
    }

    #[test]
    fn a_single_over_long_name_is_clipped_with_an_ellipsis() {
        let tabs = [tab(1, "An extraordinarily long playlist name", true, true)];
        let labels = strip(&tabs, PlaylistId::from_raw_for_tests(1), 12, None);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].text.width(), 12);
        assert!(labels[0].text.ends_with('…'));
    }

    #[test]
    fn zero_width_and_an_unknown_viewed_id_are_harmless() {
        let tabs = [tab(1, "Default", true, false)];
        assert!(strip(&tabs, PlaylistId::from_raw_for_tests(1), 0, None).is_empty());
        assert_eq!(
            texts(&strip(&tabs, PlaylistId::from_raw_for_tests(9), 40, None)),
            ["▶Default"]
        );
    }

    #[test]
    fn compact_names_the_viewed_playlist_and_its_place() {
        let tabs = [
            tab(1, "Default", false, false),
            tab(2, "Morning", true, true),
        ];
        assert_eq!(
            compact(&tabs, PlaylistId::from_raw_for_tests(2), 40, None),
            "▶Morning ·shfl 2/2"
        );
        assert!(compact(&tabs, PlaylistId::from_raw_for_tests(2), 8, None).width() <= 8);
    }

    #[test]
    fn the_repeat_badge_rides_the_playing_tab() {
        let tabs = [
            tab(1, "Default", false, false),
            tab(2, "Morning", true, true),
        ];
        let labels = strip(&tabs, PlaylistId::from_raw_for_tests(2), 80, Some("R:*"));
        assert_eq!(texts(&labels), ["Default", "▶Morning ·shfl R:*"]);
        assert_eq!(
            compact(&tabs, PlaylistId::from_raw_for_tests(2), 40, Some("R:1")),
            "▶Morning ·shfl R:1 2/2"
        );
    }
}
