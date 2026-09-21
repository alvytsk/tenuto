//! The browser overlay (design doc M5 §8): tabs, where the list is, the list
//! itself and a key hint, in one bordered box over the player. Every name
//! read from the filesystem or a feed goes through
//! [`displayable`] before it is drawn.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};

use super::{centered_box, clock, draw_scrollbar, row};
use crate::application::browse::EntryKind;
use crate::commands::displayable;
use crate::media::display::fit_to_width;
use crate::tui::browser::{BrowserState, BrowserTab, NoticeKind};
use crate::tui::layout::{inset, take_left, take_right, visible_rows};
use crate::tui::theme::Theme;

const HINTS: &str = "enter open/add/remove · space mark · tab files/podcasts/radio · \
    a add · ⌫ back · b close";
const PODCAST_HINTS: &str = "enter open/add/remove · space mark · tab files/podcasts/radio · \
    a subscribe · r/R refresh · d remove · ⌫ back · b close";
const NO_FEEDS: &str = "No subscriptions — press a to add a feed URL";
// The Radio tab's hints and empty-list text (M7.1 §7).
const RADIO_HINTS: &str = "enter add/remove · space mark · tab files/podcasts/radio · \
    a add · r re-probe · d remove · ⌫ back · b close";
const NO_STATIONS: &str = "No saved stations — press a to add a stream URL";
const PROMPT: &str = "Feed URL: ";
const RADIO_PROMPT: &str = "Stream URL: ";
const LOADING: &str = "Loading…";
const EMPTY_DIRECTORY: &str = "(empty directory)";
const NO_EPISODES: &str = "No cached episodes";
const UNTITLED: &str = "(untitled)";
const UNREACHED: &str = "(unreached)";
const MARK_COLUMNS: u16 = 2;
const DETAIL_COLUMNS: u16 = 14;
/// A verified station's identity ("genre · bitrate") needs more room than
/// `DETAIL_COLUMNS` gives the Files/Podcasts detail column — spec §7's own
/// example, `Lofi · 128 kbps`, is 15 characters, one past `DETAIL_COLUMNS`
/// — so the Radio tab gets a wider column of its own rather than widening
/// `DETAIL_COLUMNS` for every tab.
const RADIO_DETAIL_COLUMNS: u16 = 24;
/// The row width below which the detail column is dropped.
const DETAIL_MIN_ROW: u16 = 40;

pub(super) fn draw_browser(buffer: &mut Buffer, area: Rect, browser: &BrowserState, theme: &Theme) {
    let box_area = centered_box(
        area,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    Clear.render(box_area, buffer);
    Block::bordered()
        .title(" browse ")
        .border_style(Style::new().fg(theme.green))
        .style(Style::new().fg(theme.text))
        .render(box_area, buffer);
    let body = inset(box_area);
    if body.is_empty() {
        return;
    }

    tabs(browser.tab, theme).render(row(body, body.y), buffer);
    Line::styled(location(browser), Style::new().fg(theme.muted))
        .render(row(body, body.y.saturating_add(1)), buffer);
    let hint_y = body.bottom().saturating_sub(1);
    let hints = match browser.tab {
        BrowserTab::Files => HINTS,
        BrowserTab::Podcasts => PODCAST_HINTS,
        BrowserTab::Radio => RADIO_HINTS,
    };
    if body.height >= 4 {
        Line::styled(hints, Style::new().fg(theme.muted)).render(row(body, hint_y), buffer);
    }
    let list = Rect {
        y: body.y.saturating_add(2),
        height: body
            .height
            .saturating_sub(if body.height >= 4 { 3 } else { 2 }),
        ..body
    }
    .intersection(body);
    draw_list(buffer, list, browser, theme);
}

fn tabs(active: BrowserTab, theme: &Theme) -> Line<'static> {
    let style = |tab| {
        if tab == active {
            Style::new()
                .bg(theme.green)
                .fg(theme.ink)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.muted)
        }
    };
    Line::from(vec![
        Span::styled(" Files ", style(BrowserTab::Files)),
        Span::raw(" "),
        Span::styled(" Podcasts ", style(BrowserTab::Podcasts)),
        Span::raw(" "),
        Span::styled(" Radio ", style(BrowserTab::Radio)),
    ])
}

/// The directory shown, the feed whose episodes are shown, or the feed
/// list's own heading.
fn location(browser: &BrowserState) -> String {
    match (browser.tab, &browser.episodes) {
        (BrowserTab::Files, _) => displayable(&browser.cwd.display().to_string()),
        (BrowserTab::Podcasts, None) => "Subscriptions".to_owned(),
        (BrowserTab::Podcasts, Some((slug, _))) => {
            let title = browser
                .feeds
                .iter()
                .find(|feed| feed.slug == *slug)
                .and_then(|feed| feed.title.as_deref())
                .unwrap_or(slug);
            format!("Subscriptions › {}", displayable(title))
        }
        (BrowserTab::Radio, _) => "Radio".to_owned(),
    }
}

fn draw_list(buffer: &mut Buffer, area: Rect, browser: &BrowserState, theme: &Theme) {
    if area.is_empty() {
        return;
    }
    let rows = draw_notice_block(buffer, area, browser, theme);
    if rows.is_empty() {
        return;
    }
    let notice = if browser.loading {
        Some((LOADING.to_owned(), theme.muted))
    } else if let Some(error) = &browser.error {
        Some((displayable(error), theme.amber))
    } else if browser.is_empty() {
        let empty = match (browser.tab, &browser.episodes) {
            (BrowserTab::Files, _) => EMPTY_DIRECTORY,
            (BrowserTab::Podcasts, None) => NO_FEEDS,
            (BrowserTab::Podcasts, Some(_)) => NO_EPISODES,
            (BrowserTab::Radio, _) => NO_STATIONS,
        };
        Some((empty.to_owned(), theme.muted))
    } else {
        None
    };
    if let Some((text, color)) = notice {
        Line::styled(text, Style::new().fg(color)).render(row(rows, rows.y), buffer);
        return;
    }

    let window = visible_rows(
        browser.len(),
        0,
        Some(browser.cursor),
        usize::from(rows.height),
    );
    // `area` is the box's inside, so the column after it is the border.
    let track = Rect::new(area.right(), rows.y, 1, rows.height);
    draw_scrollbar(buffer, track, browser.len(), window.start, theme);
    let mut y = rows.y;
    for index in window {
        let Some(cells) = row_cells(browser, index, theme) else {
            break;
        };
        draw_row(
            buffer,
            row(rows, y),
            cells,
            browser.marked.contains(&index),
            browser.ticked(index),
            index == browser.cursor,
            theme,
        );
        y = y.saturating_add(1);
    }
}

/// What one list row shows: its label, an optional right-hand detail (drawn
/// in a column `detail_columns` wide when there is room), and the style both
/// take when the row is not under the cursor.
struct RowCells {
    label: String,
    detail: Option<String>,
    detail_columns: u16,
    style: Style,
}

fn row_cells(browser: &BrowserState, index: usize, theme: &Theme) -> Option<RowCells> {
    match (browser.tab, &browser.episodes) {
        (BrowserTab::Files, _) => browser.entries.get(index).map(|entry| {
            let name = displayable(&entry.name);
            let (label, color) = match entry.kind {
                EntryKind::Directory => (format!("{name}/"), theme.cyan),
                EntryKind::Audio => (name, theme.text),
                EntryKind::Other => (name, theme.muted),
            };
            RowCells {
                label,
                detail: None,
                detail_columns: DETAIL_COLUMNS,
                style: Style::new().fg(color),
            }
        }),
        (BrowserTab::Podcasts, None) => browser.feeds.get(index).map(|feed| RowCells {
            label: displayable(feed.title.as_deref().unwrap_or(&feed.slug)),
            detail: Some(match feed.episodes {
                Some(count) => format!("{count} episodes"),
                None => "not refreshed".to_owned(),
            }),
            detail_columns: DETAIL_COLUMNS,
            style: Style::new().fg(theme.text),
        }),
        (BrowserTab::Podcasts, Some((_, episodes))) => episodes.get(index).map(|episode| {
            let style = if episode.enclosure.is_some() {
                Style::new().fg(theme.text)
            } else {
                Style::new().fg(theme.muted).add_modifier(Modifier::DIM)
            };
            RowCells {
                label: episode
                    .title
                    .as_deref()
                    .map_or_else(|| UNTITLED.to_owned(), displayable),
                // A feed's declared duration, in parentheses as in the queue.
                detail: episode
                    .declared_duration
                    .map(|value| format!("({})", clock(value))),
                detail_columns: DETAIL_COLUMNS,
                style,
            }
        }),
        // Slug, then identity for a verified station: genre and bitrate,
        // joined and omitted when absent — the name is not drawn again, the
        // slug already stands for it (M7.1 §7). An unverified station draws
        // its URL and an unreached marker instead. Untrusted server text, so
        // escaped the same way the Files tab's names are at line 181.
        (BrowserTab::Radio, _) => {
            browser
                .stations
                .get(index)
                .map(|station| match &station.identity {
                    Some(identity) => {
                        let parts: Vec<String> = [
                            identity.genre.as_deref().map(displayable),
                            identity.bitrate_kbps.map(|kbps| format!("{kbps} kbps")),
                        ]
                        .into_iter()
                        .flatten()
                        .collect();
                        let detail = (!parts.is_empty()).then(|| {
                            // A hostile genre can be arbitrarily long and may
                            // be full-width (CJK, emoji); a right-aligned
                            // Line that overflows its column truncates from
                            // the left (keeps the tail), and counting `char`s
                            // rather than display columns would still let a
                            // wide genre overflow the column and hit that
                            // same left-clip. `fit_to_width` (already used
                            // for the status row) truncates by display width
                            // from the right instead, so the whole joined
                            // string renders untouched whenever it already
                            // fits, and a long one keeps its front.
                            fit_to_width(&parts.join(" · "), usize::from(RADIO_DETAIL_COLUMNS))
                        });
                        RowCells {
                            label: displayable(&station.slug),
                            detail,
                            detail_columns: RADIO_DETAIL_COLUMNS,
                            style: Style::new().fg(theme.text),
                        }
                    }
                    None => RowCells {
                        label: displayable(station.url.as_str()),
                        detail: Some(UNREACHED.to_owned()),
                        detail_columns: RADIO_DETAIL_COLUMNS,
                        style: Style::new().fg(theme.muted),
                    },
                })
        }
    }
}

fn draw_row(
    buffer: &mut Buffer,
    rect: Rect,
    cells: RowCells,
    marked: bool,
    queued: bool,
    under_cursor: bool,
    theme: &Theme,
) {
    let style = if under_cursor {
        cells.style.bg(theme.green).fg(theme.ink)
    } else {
        cells.style
    };
    buffer.set_style(rect, style);
    let mut rest = rect;
    let mark = take_left(&mut rest, MARK_COLUMNS);
    if marked {
        let mark_style = if under_cursor {
            style
        } else {
            Style::new().fg(theme.amber)
        };
        Line::styled("●", mark_style).render(mark, buffer);
    } else if queued {
        // The acknowledgement that the row is in the queue.
        let tick_style = if under_cursor {
            style
        } else {
            Style::new().fg(theme.green)
        };
        Line::styled("✓", tick_style).render(mark, buffer);
    }
    if let Some(detail) = cells.detail
        && rest.width >= DETAIL_MIN_ROW
    {
        let column = take_right(&mut rest, cells.detail_columns);
        take_right(&mut rest, 1);
        Line::styled(detail, style)
            .alignment(Alignment::Right)
            .render(column, buffer);
    }
    Line::styled(cells.label, style).render(rest, buffer);
}

/// The prompt, the confirmation question or the mutation notice, as a block
/// of up to a third of `area` (two rows at minimum when the text is cut), the
/// last row of a cut block being the marker; returns what is left for the rows.
/// The full text is logged once at info level through `tracing`.
fn draw_notice_block(
    buffer: &mut Buffer,
    area: Rect,
    browser: &BrowserState,
    theme: &Theme,
) -> Rect {
    let Some((lines, color)) = notice_lines(browser, theme) else {
        return area;
    };
    let third = usize::from(area.height / 3);
    let (shown, hidden) = if lines.len() <= third.max(1) {
        (lines.len(), 0)
    } else {
        // The marker takes the last budgeted row; two rows at minimum so a
        // tiny area still shows one line above it.
        let budget = third.max(2);
        (budget - 1, lines.len() - (budget - 1))
    };
    let height = u16::try_from(shown + usize::from(hidden > 0)).unwrap_or(u16::MAX);
    let mut y = area.y;
    for line in lines.iter().take(shown) {
        Line::styled(line.clone(), Style::new().fg(color)).render(row(area, y), buffer);
        y = y.saturating_add(1);
    }
    if hidden > 0 {
        Line::styled(
            format!("+{hidden} more lines, see log"),
            Style::new().fg(color).add_modifier(Modifier::DIM),
        )
        .render(row(area, y), buffer);
    }
    Rect {
        y: area.y.saturating_add(height),
        height: area.height.saturating_sub(height),
        ..area
    }
    .intersection(area)
}

fn notice_lines(browser: &BrowserState, theme: &Theme) -> Option<(Vec<String>, Color)> {
    if let Some(prompt) = &browser.prompt {
        let label = match browser.tab {
            BrowserTab::Radio => RADIO_PROMPT,
            BrowserTab::Files | BrowserTab::Podcasts => PROMPT,
        };
        return Some((vec![format!("{label}{}▏", displayable(prompt))], theme.text));
    }
    if let Some(slug) = &browser.confirm {
        return Some((
            vec![format!("Remove {}? y/N", displayable(slug))],
            theme.amber,
        ));
    }
    let notice = browser.notice.as_ref()?;
    let color = match notice.kind {
        NoticeKind::Err => theme.amber,
        NoticeKind::Working | NoticeKind::Ok => theme.muted,
    };
    Some((notice.text.lines().map(displayable).collect(), color))
}
