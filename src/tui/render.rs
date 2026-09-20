//! Draws a [`PlayerView`] into the regions of its size tier and reports where
//! the clickable parts landed (design doc M5 §7). Every string it prints was
//! already made safe by the view; the renderer only adds fixed labels and
//! formatted times, except the browser overlay, which makes its own
//! filesystem and feed names safe (see `browser`).

mod browser;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::application::transport::PlaybackPhase;
use crate::application::view::{NowPlaying, PersistenceStatus, PlayerView, QueueRow, format_saved};
use crate::media::display::format_hms;
use crate::playback::provenance::PositionProvenance;
use crate::queue::{DisplayDuration, DurationSource, QueueEntryId};
use crate::tui::browser::BrowserState;
use crate::tui::layout::{
    Regions, Tier, inset, queue_body, regions, take_left, take_right, tier_for, visible_rows,
};
use crate::tui::state::{InputPurpose, Overlay, UiState};
use crate::tui::tabs;
use crate::tui::theme::Theme;

/// What the frame shows beyond the view: prepared artwork, spectrum levels
/// and the open browser.
#[derive(Default)]
pub struct Visuals<'a> {
    pub cover: CoverView<'a>,
    /// One level per band, 0.0 to 1.0.
    pub spectrum: Option<&'a [f32]>,
    /// Each band's falling peak, 0.0 to 1.0; drawn as a cap above its bar.
    pub peaks: Option<&'a [f32]>,
    /// Drawn while `UiState::overlay` is `Overlay::Browser`.
    pub browser: Option<&'a BrowserState>,
}

#[derive(Default)]
pub enum CoverView<'a> {
    #[default]
    Placeholder,
    Image(&'a dyn CoverWidget),
}

pub trait CoverWidget {
    fn render_cover(&self, area: Rect, buffer: &mut Buffer);
}

/// Where the last frame put what a mouse can act on.
#[derive(Clone, Debug, Default)]
pub struct HitMap {
    /// Each visible queue row, all of its lines.
    pub rows: Vec<(Rect, QueueEntryId)>,
    pub queue: Rect,
    /// The bar alone, so a click's column maps to a fraction of the track.
    pub progress: Rect,
    pub buttons: Vec<(Rect, TransportButton)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportButton {
    Previous,
    SeekBack,
    PlayPause,
    Stop,
    SeekForward,
    Next,
}

const EMPTY_QUEUE: &str = "Queue is empty — press b to browse or a to add";
const BRAND: &str = "TENUTO";
/// Key, then what it does; drawn as bold key and muted label.
const KEY_HINTS: [(&str, &str); 6] = [
    ("Space", "Play/Pause"),
    ("Enter", "Play selected"),
    ("b", "Browse"),
    ("a", "Add"),
    ("?", "Help"),
    ("q", "Quit"),
];
const TOO_SMALL: &str = "Terminal too small (need 30×8)";
const RESIZE_HINTS: &str = "space play · q quit";
/// Verbatim per the design (§4/§7): confirming discards the queue, not
/// listening history.
const CONFIRM_CLEAR_TEXT: &str = "Clear the queue? Listening history is kept. y to confirm";
/// The §7 key table, one line per row.
const HELP_LINES: [&str; 22] = [
    "Space           Pause/resume; unloaded/ended behavior follows §4",
    "Enter           Play selected queue entry",
    "Up/Down or j/k  Move selection",
    "J/K             Move selected entry down/up",
    "Left/Right      Seek backward/forward 10 seconds",
    "Home            Explicit restart from beginning",
    "- _ / + =       Decrease / increase volume",
    "s / p           Stop / play",
    "[ / ]           Previous / next queue entry",
    "d               Remove selected entry",
    "b               Open/close browser",
    "  in Podcasts   a subscribe · r/R refresh one/all · d unsubscribe",
    "a               Open path/URL input",
    "c               Clear this playlist, with confirmation",
    "Tab/Shift-Tab   Next / previous playlist",
    "n / r / D       New / rename / delete playlist",
    "z               Shuffle this playlist",
    "?               Show help",
    "m               Toggle mouse capture",
    "Ctrl-L          Redraw the whole view",
    "Esc             Close the active overlay or cancel input",
    "q / Ctrl-C      Graceful quit",
];
const NOTHING_PLAYING: &str = "Nothing playing";
const UNKNOWN_TIME: &str = "--:--";
/// The played part of the bar in green, the rest in the line colour.
const BAR_FILLED: &str = "━";
const BAR_EMPTY: &str = "─";
const LEVELS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Bars drawn while no analysis is available.
const FLAT_BANDS: usize = 24;
/// A band at or above this level gets an amber cap on its top cell.
const PEAK_LEVEL: f32 = 0.8;
/// The spectrum height below which no row is given up to band numbers.
const LABELLED_MIN_ROWS: u16 = 5;
/// A band's falling peak: a light rule, thinner than the thinnest block, and
/// continuous across the columns of a wide bar.
const PEAK_CAP: &str = "─";
const VOLUME_COLUMNS: usize = 10;
/// The transport row width below which the compact tier has no slider: the
/// buttons, `reconnecting…` and the slider would no longer fit.
const SLIDER_MIN_ROW: u16 = 64;
const MARKER_COLUMNS: u16 = 2;
const NUMBER_COLUMNS: u16 = 4;
/// Rows below the last entry, like an editor past the end of a file.
const FILLER: &str = "~";
const DURATION_COLUMNS: u16 = 10;
const SAVED_COLUMNS: u16 = 16;
/// The row width below which the saved column is dropped.
const SAVED_MIN_ROW: u16 = 44;
/// The row width below which the duration column is dropped.
const DURATION_MIN_ROW: u16 = 24;

pub fn draw(
    frame: &mut Frame<'_>,
    view: &PlayerView,
    ui: &UiState,
    visuals: &Visuals<'_>,
) -> HitMap {
    let area = frame.area();
    let tier = tier_for(area.width, area.height);
    let regions = regions(area, tier, info_line_count(view));
    let theme = Theme::default();
    let buffer = frame.buffer_mut();
    buffer.set_style(area, Style::new().fg(theme.text));

    if tier == Tier::Resize {
        draw_resize(buffer, &regions, &theme);
        return HitMap::default();
    }

    if tier != Tier::Minimal {
        bordered(&theme).render(regions.player, buffer);
    }
    let slider = has_volume_slider(tier, regions.transport);
    draw_status(buffer, regions.status, view, ui, slider, &theme);
    if let Some(cover) = regions.cover {
        match visuals.cover {
            CoverView::Placeholder => draw_cover_placeholder(buffer, cover, &theme),
            CoverView::Image(widget) => widget.render_cover(cover, buffer),
        }
    }
    draw_info(buffer, &regions, view, &theme);
    if let Some(spectrum) = regions.spectrum {
        draw_spectrum(buffer, spectrum, visuals.spectrum, visuals.peaks, &theme);
    }
    let buttons = draw_transport(buffer, regions.transport, view, tier, slider, &theme);
    let progress = draw_progress(buffer, &regions, view, tier, &theme);
    let rows = draw_queue(buffer, regions.queue, view, ui, tier, &theme);
    draw_footer(buffer, regions.footer, view, &theme);
    draw_overlay(buffer, area, view, ui, visuals.browser, &theme);
    HitMap {
        rows,
        queue: regions.queue,
        progress,
        buttons,
    }
}

/// The help, confirm, input and browser overlays float over everything else
/// the frame drew. The browser overlay needs the browser itself; without it
/// there is nothing to draw.
fn draw_overlay(
    buffer: &mut Buffer,
    area: Rect,
    view: &PlayerView,
    ui: &UiState,
    browser: Option<&BrowserState>,
    theme: &Theme,
) {
    match ui.overlay {
        Overlay::Help => draw_help_overlay(buffer, area, theme),
        Overlay::ConfirmClear(_) => draw_confirm_overlay(buffer, area, CONFIRM_CLEAR_TEXT, theme),
        Overlay::ConfirmDelete(id) => {
            let name = view
                .tabs
                .iter()
                .find(|tab| tab.id == id)
                .map_or("", |tab| tab.name.as_str());
            draw_confirm_overlay(
                buffer,
                area,
                &format!(r#"Delete playlist "{name}"? y to confirm"#),
                theme,
            );
        }
        Overlay::Input(purpose) => draw_input_overlay(buffer, area, purpose, &ui.input, theme),
        Overlay::Browser => {
            if let Some(browser) = browser {
                browser::draw_browser(buffer, area, browser, theme);
            }
        }
        Overlay::None => {}
    }
}

/// A box centred in `area`, clamped to fit it even when `area` is smaller
/// than the requested size.
fn centered_box(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x.saturating_add((area.width - width) / 2),
        y: area.y.saturating_add((area.height - height) / 2),
        width,
        height,
    }
}

fn draw_confirm_overlay(buffer: &mut Buffer, area: Rect, text: &str, theme: &Theme) {
    let width = u16::try_from(text.width() + 4)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let box_area = centered_box(area, width, 3);
    Clear.render(box_area, buffer);
    Block::bordered()
        .title(" confirm ")
        .border_style(Style::new().fg(theme.amber))
        .render(box_area, buffer);
    Paragraph::new(text)
        .style(Style::new().fg(theme.cream))
        .wrap(Wrap { trim: true })
        .render(inset(box_area), buffer);
}

fn draw_help_overlay(buffer: &mut Buffer, area: Rect, theme: &Theme) {
    let content_width = HELP_LINES.iter().map(|line| line.len()).max().unwrap_or(0);
    let width = u16::try_from(content_width + 4)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let height = u16::try_from(HELP_LINES.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let box_area = centered_box(area, width, height);
    Clear.render(box_area, buffer);
    Block::bordered()
        .title(" help — ? or Esc to close ")
        .border_style(Style::new().fg(theme.line))
        .render(box_area, buffer);
    Paragraph::new(HELP_LINES.join("\n"))
        .style(Style::new().fg(theme.text))
        .render(inset(box_area), buffer);
}

/// Typed text reaches this overlay only through `tui::input`, which already
/// drops raw control characters before they land in `ui.input`, so the
/// string drawn here is always inert.
fn draw_input_overlay(
    buffer: &mut Buffer,
    area: Rect,
    purpose: InputPurpose,
    input: &str,
    theme: &Theme,
) {
    let title = match purpose {
        InputPurpose::AddUrl(_) => " add path or URL — Enter to add, Esc to cancel ",
        InputPurpose::NewPlaylist => " new playlist — Enter to create, Esc to cancel ",
        InputPurpose::Rename(_) => " rename playlist — Enter to rename, Esc to cancel ",
    };
    let width = area.width.min(60);
    let box_area = centered_box(area, width, 3);
    Clear.render(box_area, buffer);
    Block::bordered()
        .title(title)
        .border_style(Style::new().fg(theme.green))
        .render(box_area, buffer);
    Paragraph::new(format!("{input}▏"))
        .style(Style::new().fg(theme.cream))
        .render(inset(box_area), buffer);
}

fn bordered(theme: &Theme) -> Block<'static> {
    Block::bordered().border_style(Style::new().fg(theme.line))
}

fn draw_resize(buffer: &mut Buffer, regions: &Regions, theme: &Theme) {
    let message = regions.info;
    let middle = Rect {
        y: message.y.saturating_add(message.height / 2),
        height: message.height.min(1),
        ..message
    };
    let too_small = Line::styled(TOO_SMALL, Style::new().fg(theme.cream));
    centred(too_small, middle).render(middle, buffer);
    let hints = Line::styled(RESIZE_HINTS, Style::new().fg(theme.muted));
    centred(hints, regions.footer).render(regions.footer, buffer);
}

/// Centred when it fits; otherwise left-aligned, so a cut keeps the start of
/// the line instead of losing both ends.
fn centred(line: Line<'_>, area: Rect) -> Line<'_> {
    if line.width() <= usize::from(area.width) {
        line.alignment(Alignment::Center)
    } else {
        line
    }
}

/// Whether the transport row ends in the volume slider: in the normal tier,
/// and in the compact one when the row is wide enough to keep the buttons
/// and the state beside it.
fn has_volume_slider(tier: Tier, transport: Rect) -> bool {
    match tier {
        Tier::Normal => true,
        Tier::Compact => transport.width >= SLIDER_MIN_ROW,
        _ => false,
    }
}

/// The brand on the left and the session flags on the right. The volume
/// lives here unless the transport row has a slider.
fn draw_status(
    buffer: &mut Buffer,
    area: Rect,
    view: &PlayerView,
    ui: &UiState,
    slider: bool,
    theme: &Theme,
) {
    let mut rest = area;
    let brand = Line::styled(
        format!(" {BRAND} "),
        Style::new().fg(theme.cream).add_modifier(Modifier::BOLD),
    );
    let brand_columns = u16::try_from(brand.width()).unwrap_or(u16::MAX);
    brand.render(take_left(&mut rest, brand_columns), buffer);
    // A gap after the name; on a narrow row the right side loses its start
    // rather than writing over the name.
    take_left(&mut rest, 1);
    let muted = Style::new().fg(theme.muted);
    let mouse = if ui.mouse_capture { "on" } else { "off" };
    let flags = if slider {
        format!(" mouse {mouse} ")
    } else {
        format!(" vol {}% · mouse {mouse} ", view.volume.percent())
    };
    let mut spans = vec![Span::styled(flags, muted)];
    let persistence = match view.persistence {
        PersistenceStatus::Saving => None,
        PersistenceStatus::Unsaved => Some("unsaved"),
        PersistenceStatus::Failing => Some("not saving"),
    };
    if let Some(label) = persistence {
        spans.insert(1, Span::styled("· ", muted));
        spans.insert(
            2,
            Span::styled(format!("{label} "), Style::new().fg(theme.amber)),
        );
    }
    Line::from(spans)
        .alignment(Alignment::Right)
        .render(rest, buffer);
}

/// A stable stand-in until artwork is prepared: a shaded square with a note.
fn draw_cover_placeholder(buffer: &mut Buffer, area: Rect, theme: &Theme) {
    let shade = Style::new().fg(theme.line).bg(theme.panel);
    for y in area.top()..area.bottom() {
        Line::styled("░".repeat(usize::from(area.width)), shade).render(row(area, y), buffer);
    }
    let centre = (
        area.x.saturating_add(area.width / 2),
        area.y.saturating_add(area.height / 2),
    );
    if !area.is_empty()
        && let Some(cell) = buffer.cell_mut(centre)
    {
        cell.set_symbol("♪").set_fg(theme.amber);
    }
}

/// The information lines there are to show: the title, the artist when
/// known, and the album with its year when either is.
fn info_lines<'a>(view: &'a PlayerView, theme: &Theme) -> Vec<Line<'a>> {
    let Some(now) = &view.now_playing else {
        return vec![Line::styled(NOTHING_PLAYING, Style::new().fg(theme.muted))];
    };
    let mut lines = vec![Line::styled(
        now.title.as_str(),
        Style::new().fg(theme.cream).add_modifier(Modifier::BOLD),
    )];
    if let Some(artist) = &now.artist {
        lines.push(Line::styled(artist.as_str(), Style::new().fg(theme.text)));
    }
    let release: Vec<&str> = [now.album.as_deref(), now.year.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if !release.is_empty() {
        lines.push(Line::styled(
            release.join(" · "),
            Style::new().fg(theme.muted),
        ));
    }
    lines
}

/// How many information lines `view` has, for [`regions`].
pub fn info_line_count(view: &PlayerView) -> u16 {
    u16::try_from(info_lines(view, &Theme::default()).len()).unwrap_or(u16::MAX)
}

/// As many of the information lines as the tier gave rows to.
fn draw_info(buffer: &mut Buffer, regions: &Regions, view: &PlayerView, theme: &Theme) {
    let area = regions.info;
    for (line, y) in info_lines(view, theme)
        .into_iter()
        .zip(area.top()..area.bottom())
    {
        line.render(row(area, y), buffer);
    }
}

/// Flat bars without analysis; otherwise each bar's level in eighths of a
/// row, never below the floor glyph, with an amber cap on a loud bar. Bars
/// are equally wide with a one-column gap between them, as wide as one bar
/// per band allows, and as many as then fit: fewer than the bands sample the
/// nearest band, more than the bands interpolate between neighbours, so the
/// row is full at every width. They end at the right edge, in line with the
/// progress bar; less than a bar's worth of columns is left empty on the
/// left. When every bar is at least two columns wide and the area tall
/// enough to spare a row, the bottom row numbers the bars instead. A peak
/// is a cream cap in the row it has fallen to, while that row is above the
/// bar.
fn draw_spectrum(
    buffer: &mut Buffer,
    area: Rect,
    levels: Option<&[f32]>,
    peaks: Option<&[f32]>,
    theme: &Theme,
) {
    let levels = levels.filter(|levels| !levels.is_empty());
    let bands = levels.map_or(FLAT_BANDS, <[f32]>::len);
    let width = usize::from(area.width);
    if width == 0 {
        return;
    }
    // Every bar owns a slot ending in its gap; the last gap is off the area.
    let slot = ((width + 1) / bands).max(2);
    let bars_count = (width + 1) / slot;
    let spare = (width + 1) % slot;
    let labelled = slot >= 3 && area.height >= LABELLED_MIN_ROWS;
    let bars = Rect {
        height: area.height.saturating_sub(u16::from(labelled)),
        ..area
    };
    let height = usize::from(bars.height);
    if height == 0 {
        return;
    }
    let labels = Style::new().fg(theme.muted);
    let level_of = |levels: Option<&[f32]>, band: usize| {
        levels
            .and_then(|levels| levels.get(band))
            .copied()
            .filter(|level| level.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    };
    // A bar's value: its nearest band, or between two when bars outnumber bands.
    let value_of = |values: Option<&[f32]>, bar: usize| {
        if bars_count <= bands {
            return level_of(values, bar * bands / bars_count);
        }
        let at = (bar * (bands - 1)) as f32 / (bars_count - 1) as f32;
        let low = at.floor();
        // `at` is within [0, bands - 1].
        let band = low as usize;
        let next = level_of(values, (band + 1).min(bands - 1));
        level_of(values, band) + (next - level_of(values, band)) * (at - low)
    };
    // The float is clamped to [0, height × 8] before the cast.
    let eighths_of = |level: f32| ((level * (height * 8) as f32).round() as usize).max(1);
    for bar in 0..bars_count {
        // Below `width`, which came from a `u16`.
        let left = area.x.saturating_add((spare + bar * slot) as u16);
        if labelled {
            let number = format!("{:02}", bar + 1);
            let cell = Rect {
                x: left,
                y: bars.bottom(),
                width: 2,
                height: 1,
            }
            .intersection(area);
            Line::styled(number, labels).render(cell, buffer);
        }
        let level = value_of(levels, bar);
        let eighths = eighths_of(level);
        let top = (eighths - 1) / 8;
        let cap = Some((eighths_of(value_of(peaks, bar)) - 1) / 8).filter(|cap| *cap > top);
        for x in (left..).take(slot - 1) {
            for (from_bottom, y) in (bars.top()..bars.bottom()).rev().enumerate() {
                let Some(cell) = buffer.cell_mut((x, y)) else {
                    continue;
                };
                if cap == Some(from_bottom) {
                    cell.set_symbol(PEAK_CAP).set_fg(theme.cream);
                    continue;
                }
                let fill = eighths.saturating_sub(from_bottom * 8).min(8);
                let color = if from_bottom == top && level >= PEAK_LEVEL {
                    theme.amber
                } else {
                    theme.green
                };
                cell.set_symbol(LEVELS[fill]).set_fg(color);
            }
        }
    }
}

fn draw_transport(
    buffer: &mut Buffer,
    area: Rect,
    view: &PlayerView,
    tier: Tier,
    slider: bool,
    theme: &Theme,
) -> Vec<(Rect, TransportButton)> {
    // Reconnecting alongside Playing: Space means pause in both (M7 §6.3),
    // so the button has to say so.
    let pausing = matches!(
        view.phase,
        PlaybackPhase::Playing | PlaybackPhase::Reconnecting
    );
    let play_pause = if pausing { " ‖ " } else { " ▶ " };
    // Each label is a chip: the glyph centred in a padded, filled cell run.
    // Play and pause are both one cell wide, so the glyph holds its place
    // when one turns into the other.
    // The doubled triangles seek, as ← and → do. The bar on Previous and
    // Next is `ǀ`, the single form of the pause mark, so the two stand the
    // same height.
    let labels = [
        (" ǀ◀ ", TransportButton::Previous),
        (" ◀◀ ", TransportButton::SeekBack),
        (play_pause, TransportButton::PlayPause),
        (" ■ ", TransportButton::Stop),
        (" ▶▶ ", TransportButton::SeekForward),
        (" ▶ǀ ", TransportButton::Next),
    ];
    let mut rest = area;
    if slider {
        draw_volume(buffer, &mut rest, view, theme);
    }
    let mut buttons = Vec::with_capacity(labels.len());
    for (label, button) in labels {
        let style = button_style(button, theme);
        // `‖` and `ǀ` are the bars fonts centre in their cell; bold gives
        // them the weight of the triangles beside them.
        let line: Line = label
            .chars()
            .map(|glyph| {
                let bar = matches!(glyph, '‖' | 'ǀ');
                let style = if bar { style.bold() } else { style };
                Span::styled(glyph.to_string(), style)
            })
            .collect();
        let width = u16::try_from(line.width()).unwrap_or(u16::MAX);
        let rect = take_left(&mut rest, width);
        line.render(rect, buffer);
        if !rect.is_empty() {
            buttons.push((rect, button));
        }
        take_left(&mut rest, 1);
    }
    if tier != Tier::Minimal {
        take_left(&mut rest, 1);
        state_line(view, theme).render(rest, buffer);
    }
    buttons
}

/// `VOL ━━━━━━━───  70%` at the right end of the row, taken off `rest`.
fn draw_volume(buffer: &mut Buffer, rest: &mut Rect, view: &PlayerView, theme: &Theme) {
    let percent = view.volume.percent();
    let filled = (usize::from(percent) * VOLUME_COLUMNS)
        .div_ceil(100)
        .min(VOLUME_COLUMNS);
    let line = Line::from(vec![
        Span::styled("VOL ", Style::new().fg(theme.muted)),
        Span::styled(BAR_FILLED.repeat(filled), Style::new().fg(theme.text)),
        Span::styled(
            BAR_EMPTY.repeat(VOLUME_COLUMNS - filled),
            Style::new().fg(theme.line),
        ),
        Span::styled(format!("  {percent:>3}%"), Style::new().fg(theme.text)),
    ]);
    let width = u16::try_from(line.width()).unwrap_or(u16::MAX);
    let slider = take_right(rest, width);
    take_right(rest, 2);
    line.render(slider, buffer);
}

fn button_style(button: TransportButton, theme: &Theme) -> Style {
    match button {
        TransportButton::PlayPause => Style::new().bg(theme.green).fg(theme.ink),
        _ => Style::new().bg(theme.panel).fg(theme.text),
    }
}

/// The transport phase as a word, with ` buffering` while it waits on data.
fn state_line(view: &PlayerView, theme: &Theme) -> Line<'static> {
    let (label, color) = match view.phase {
        PlaybackPhase::Unloaded => ("idle", theme.muted),
        PlaybackPhase::Loading => ("loading", theme.amber),
        PlaybackPhase::LoadFailed => ("failed", theme.amber),
        PlaybackPhase::Playing => ("playing", theme.green),
        PlaybackPhase::Reconnecting => ("reconnecting…", theme.amber),
        PlaybackPhase::Paused => ("paused", theme.cyan),
        PlaybackPhase::Stopped => ("stopped", theme.muted),
        PlaybackPhase::Ended => ("ended", theme.muted),
    };
    let mut spans = vec![Span::styled(label, Style::new().fg(color))];
    if view.now_playing.as_ref().is_some_and(|now| now.buffering) {
        spans.push(Span::styled(" buffering", Style::new().fg(theme.amber)));
    }
    Line::from(spans)
}

/// Draws the time label and the bar; returns the bar's rectangle. With a
/// time row of its own the label goes there, with the phase glyph before
/// it, and the bar fills its row between brackets; otherwise the bar follows
/// the label on the shared row.
fn draw_progress(
    buffer: &mut Buffer,
    regions: &Regions,
    view: &PlayerView,
    tier: Tier,
    theme: &Theme,
) -> Rect {
    if view.live {
        return draw_live_progress(buffer, regions, view, tier, theme);
    }
    let (label, ratio) = progress_label(view.now_playing.as_ref());
    let mut bar = regions.progress;
    if regions.time == regions.progress {
        let mut spans = Vec::new();
        if tier == Tier::Minimal {
            spans.extend(state_line(view, theme).spans);
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(label, Style::new().fg(theme.text)));
        spans.push(Span::raw(" "));
        let line = Line::from(spans);
        let used = u16::try_from(line.width()).unwrap_or(u16::MAX);
        line.render(bar, buffer);
        take_left(&mut bar, used);
    } else {
        let green = Style::new().fg(theme.green);
        let glyph = match view.phase {
            PlaybackPhase::Playing | PlaybackPhase::Reconnecting => Span::styled("▶ ", green),
            PlaybackPhase::Paused => Span::styled("‖ ", green.add_modifier(Modifier::BOLD)),
            _ => Span::raw(""),
        };
        Line::from(vec![
            glyph,
            Span::styled(label, Style::new().fg(theme.green)),
        ])
        .render(regions.time, buffer);
        let bracket = Style::new().fg(theme.muted);
        Line::styled("[", bracket).render(take_left(&mut bar, 1), buffer);
        Line::styled("]", bracket).render(take_right(&mut bar, 1), buffer);
    }
    let width = usize::from(bar.width);
    // `ratio` is within [0, 1], so the product is within [0, width].
    let filled = ratio.map_or(0, |ratio| (ratio * width as f64).round() as usize);
    Line::from(vec![
        Span::styled(BAR_FILLED.repeat(filled), Style::new().fg(theme.green)),
        Span::styled(
            BAR_EMPTY.repeat(width.saturating_sub(filled)),
            Style::new().fg(theme.line),
        ),
    ])
    .render(bar, buffer);
    bar
}

/// A live source has no timeline: the listening time replaces the
/// position/total pair, and the bar and its brackets are skipped rather than
/// drawn empty. The label is never wider than the finite line it replaces,
/// so no region or tier changes.
fn draw_live_progress(
    buffer: &mut Buffer,
    regions: &Regions,
    view: &PlayerView,
    tier: Tier,
    theme: &Theme,
) -> Rect {
    let time = view
        .now_playing
        .as_ref()
        .map_or_else(|| UNKNOWN_TIME.to_owned(), |now| clock(now.position));
    let label = format!("LIVE  {time}");
    if regions.time == regions.progress {
        let mut spans = Vec::new();
        if tier == Tier::Minimal {
            spans.extend(state_line(view, theme).spans);
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(label, Style::new().fg(theme.text)));
        Line::from(spans).render(regions.progress, buffer);
    } else {
        Line::styled(label, Style::new().fg(theme.green)).render(regions.time, buffer);
    }
    regions.progress
}

/// The time label and how much of the bar it fills. Only a loaded entry with
/// a decoder-confirmed duration fills anything; before loading the label is
/// the saved history instead of a position.
fn progress_label(now: Option<&NowPlaying>) -> (String, Option<f64>) {
    let Some(now) = now else {
        return (format!("{UNKNOWN_TIME} / {UNKNOWN_TIME}"), None);
    };
    let duration = now.duration.map(duration_label);
    if !now.loaded {
        let saved = now
            .saved
            .map_or_else(|| UNKNOWN_TIME.to_owned(), format_saved);
        let label = match duration {
            Some(duration) => format!("{saved} · {duration}"),
            None => saved,
        };
        return (label, None);
    }
    let mark = if now.estimated_position { "~" } else { "" };
    let label = format!(
        "{mark}{} / {}",
        clock(now.position),
        duration.as_deref().unwrap_or(UNKNOWN_TIME)
    );
    let ratio = match now.duration {
        Some(DisplayDuration {
            value,
            source: DurationSource::Decoded(_),
        }) if !value.is_zero() => {
            Some((now.position.as_secs_f64() / value.as_secs_f64()).clamp(0.0, 1.0))
        }
        _ => None,
    };
    (label, ratio)
}

/// `mm:ss` under an hour, `HH:MM:SS` from then on.
fn clock(duration: std::time::Duration) -> String {
    let full = format_hms(duration);
    if let Some(short) = full.strip_prefix("00:") {
        return short.to_owned();
    }
    full
}

/// A declared duration is the feed's claim, not the decoder's, so it is
/// shown in parentheses; a decoded one derived from a byte-offset estimate
/// carries the same `~` an estimated position does.
fn duration_label(duration: DisplayDuration) -> String {
    match duration.source {
        DurationSource::Decoded(PositionProvenance::Established) => clock(duration.value),
        DurationSource::Decoded(PositionProvenance::Estimated) => {
            format!("~{}", clock(duration.value))
        }
        DurationSource::Declared => format!("({})", clock(duration.value)),
    }
}

fn draw_queue(
    buffer: &mut Buffer,
    area: Rect,
    view: &PlayerView,
    ui: &UiState,
    tier: Tier,
    theme: &Theme,
) -> Vec<(Rect, QueueEntryId)> {
    let body = if tier == Tier::Minimal {
        // No border to put the strip in: its first row carries the compact
        // form and the listing takes the rest.
        let head = row(area, area.y);
        let strip = tabs::compact(&view.tabs, view.viewed, usize::from(area.width));
        Line::styled(strip, Style::new().fg(theme.muted)).render(head, buffer);
        Rect {
            y: area.y.saturating_add(1),
            height: area.height.saturating_sub(1),
            ..area
        }
    } else {
        let count = match view.rows.len() {
            1 => " 1 track ".to_owned(),
            n => format!(" {n} tracks "),
        };
        // The strip sits where ` QUEUE ` did: inside the corners, with a
        // column of air at each end, and clear of the right-aligned count.
        let room = usize::from(area.width).saturating_sub(4 + count.width());
        bordered(theme)
            .title(tab_strip(view, room, theme))
            .title(Line::styled(count, Style::new().fg(theme.muted)).right_aligned())
            .render(area, buffer);
        queue_body(area)
    };
    if view.rows.is_empty() {
        Line::styled(EMPTY_QUEUE, Style::new().fg(theme.muted)).render(body, buffer);
        return Vec::new();
    }

    let selected = ui
        .selected
        .and_then(|id| view.rows.iter().position(|row| row.id == id));
    let capacity = usize::from(body.height);
    let window = visible_rows(view.rows.len(), ui.queue_offset, selected, capacity);
    let mut hits = Vec::with_capacity(window.len());
    let mut y = body.y;
    for index in window {
        let Some(entry) = view.rows.get(index) else {
            break;
        };
        let rect = row(body, y);
        draw_queue_row(
            buffer,
            rect,
            index,
            entry,
            view.active == Some(entry.id),
            selected == Some(index),
            theme,
        );
        hits.push((rect, entry.id));
        y = y.saturating_add(1);
    }
    if tier != Tier::Minimal {
        while y < body.bottom() {
            Line::styled(FILLER, Style::new().fg(theme.line)).render(row(body, y), buffer);
            y += 1;
        }
    }
    hits
}

/// The playlist tabs as a border title: the viewed one bold and cream, the
/// playing one in the text colour, the rest muted (§10).
fn tab_strip(view: &PlayerView, room: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (index, label) in tabs::strip(&view.tabs, view.viewed, room)
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            spans.push(Span::raw(tabs::GAP));
        }
        let style = if label.viewed {
            Style::new().fg(theme.cream).add_modifier(Modifier::BOLD)
        } else if label.playing {
            Style::new().fg(theme.text)
        } else {
            Style::new().fg(theme.muted)
        };
        spans.push(Span::styled(label.text, style));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// One row: playing marker, number, title, duration, saved history. Narrow
/// rows drop the saved column, then the duration, before the title.
fn draw_queue_row(
    buffer: &mut Buffer,
    rect: Rect,
    index: usize,
    entry: &QueueRow,
    playing: bool,
    selected: bool,
    theme: &Theme,
) {
    let (base, accent, muted) = if selected {
        let on_amber = Style::new().bg(theme.amber).fg(theme.ink);
        (on_amber.add_modifier(Modifier::BOLD), on_amber, on_amber)
    } else {
        (
            Style::new().fg(theme.text),
            Style::new().fg(theme.green),
            Style::new().fg(theme.muted),
        )
    };
    buffer.set_style(rect, base);

    let mut rest = rect;
    let marker = take_left(&mut rest, MARKER_COLUMNS);
    if playing {
        Line::styled("▶", accent).render(marker, buffer);
    }
    let number = take_left(&mut rest, NUMBER_COLUMNS);
    Line::styled(format!("{:02}", index + 1), muted).render(number, buffer);
    let saved = column(&mut rest, SAVED_COLUMNS, SAVED_MIN_ROW);
    let duration = column(&mut rest, DURATION_COLUMNS, DURATION_MIN_ROW);

    Line::styled(entry.title.as_str(), base).render(rest, buffer);
    if let Some(value) = entry.duration {
        Line::styled(duration_label(value), base)
            .alignment(Alignment::Right)
            .render(row(duration, duration.y), buffer);
    }
    if let Some(history) = entry.saved {
        Line::styled(format_saved(history), muted)
            .alignment(Alignment::Right)
            .render(row(saved, saved.y), buffer);
    }
}

fn draw_footer(buffer: &mut Buffer, area: Rect, view: &PlayerView, theme: &Theme) {
    let line = match &view.status {
        Some(status) => Line::styled(status.as_str(), Style::new().fg(theme.amber)),
        None => {
            let key = Style::new().fg(theme.cream).add_modifier(Modifier::BOLD);
            let label = Style::new().fg(theme.muted);
            let mut spans = vec![Span::raw(" ")];
            for (name, action) in KEY_HINTS {
                spans.push(Span::styled(name, key));
                spans.push(Span::styled(format!(" {action}   "), label));
            }
            Line::from(spans)
        }
    };
    line.render(area, buffer);
}

/// A right-hand column of `columns` plus a one-cell gap before it, taken
/// only while the row is at least `min_row` wide; otherwise an empty rect.
fn column(rest: &mut Rect, columns: u16, min_row: u16) -> Rect {
    if rest.width < min_row {
        return Rect { width: 0, ..*rest };
    }
    let column = take_right(rest, columns);
    take_right(rest, 1);
    column
}

/// Row `y` of `area`, empty when `y` is outside it.
fn row(area: Rect, y: u16) -> Rect {
    Rect {
        y,
        height: 1,
        ..area
    }
    .intersection(area)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The drawn runs of `y`, as (first column, width).
    fn runs(buffer: &Buffer, width: u16, y: u16) -> Vec<(u16, u16)> {
        let mut runs: Vec<(u16, u16)> = Vec::new();
        for x in 0..width {
            if buffer[(x, y)].symbol() == " " {
                continue;
            }
            match runs.last_mut() {
                Some((start, len)) if *start + *len == x => *len += 1,
                _ => runs.push((x, 1)),
            }
        }
        runs
    }

    #[test]
    fn the_spectrum_ends_at_the_right_edge_with_equal_bars() {
        // Widths a 21- or 24-band row does not divide, as a resized window gives.
        for (width, bands) in [(38, 24), (60, 21), (76, 21), (100, 24), (39, 10)] {
            let area = Rect::new(0, 0, width, 4);
            let mut buffer = Buffer::empty(area);
            draw_spectrum(
                &mut buffer,
                area,
                Some(&vec![1.0; bands]),
                None,
                &Theme::default(),
            );
            let bars = runs(&buffer, width, 0);
            let case = format!("{width} columns, {bands} bands: {bars:?}");
            assert!(
                bars.len() >= bands.min(usize::from(width).div_ceil(2)),
                "{case}"
            );
            // Less than a bar and its gap is spare, all of it on the left.
            assert!(bars.first().is_some_and(|bar| bar.0 <= bar.1), "{case}");
            assert_eq!(bars.last().map(|bar| bar.0 + bar.1), Some(width), "{case}");
            assert!(
                bars.windows(2)
                    .all(|pair| pair[0].0 + pair[0].1 + 1 == pair[1].0),
                "{case}"
            );
            assert!(bars.iter().all(|bar| bar.1 == bars[0].1), "{case}");
        }
    }

    #[test]
    fn bars_beyond_the_bands_interpolate_between_neighbours() {
        // Four bands in nine columns: five one-column bars.
        let area = Rect::new(0, 0, 9, 8);
        let mut buffer = Buffer::empty(area);
        draw_spectrum(
            &mut buffer,
            area,
            Some(&[0.0, 0.5, 0.5, 1.0]),
            None,
            &Theme::default(),
        );
        let full_rows = |x: u16| (0..8).filter(|y| buffer[(x, *y)].symbol() == "█").count();
        let heights: Vec<usize> = (0..9).step_by(2).map(full_rows).collect();
        // The ends are the outer bands; the second bar is between 0.0 and 0.5.
        assert_eq!(heights, [0, 3, 4, 5, 8]);
    }

    #[test]
    fn a_peak_cap_floats_above_its_bar() {
        let area = Rect::new(0, 0, 3, 4);
        let mut buffer = Buffer::empty(area);
        let theme = Theme::default();
        // Bars one row tall; the first band's peak is up in the third row.
        draw_spectrum(
            &mut buffer,
            area,
            Some(&[0.25, 0.25]),
            Some(&[0.75, 0.25]),
            &theme,
        );
        let column = |x: u16| -> Vec<&str> { (0..4).map(|y| buffer[(x, y)].symbol()).collect() };
        assert_eq!(column(0), [" ", PEAK_CAP, " ", "█"]);
        assert_eq!(buffer[(0, 1)].fg, theme.cream);
        // A peak resting on its bar's top cell is the bar itself.
        assert_eq!(column(2), [" ", " ", " ", "█"]);
    }
}
