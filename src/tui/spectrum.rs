//! What the spectrum row shows (design doc M5 §10, decision 25).
//!
//! Analysis runs only while the row exists and playback is `Playing`
//! ([`wants_analysis`]). [`SpectrumDisplay`] holds the drawn levels between
//! frames and is updated before each draw, never inside it: a published
//! frame is drawn only while it is fresh and still belongs to the adopted
//! playback; otherwise the drawn levels decay. The clock check is the view's
//! own, so a worker that is late clearing a stale frame cannot freeze the
//! row.
//!
//! Each band also keeps a peak: where its bar last reached, held for
//! [`PEAK_HOLD`] and then falling at [`PEAK_FALL_PER_SECOND`], slower than
//! the bar, and never below it.
//!
//! M14 makes the row selectable: [`VisualizerMode`] names what it draws,
//! and `v` cycles bars → scope → flow → bars. The scope draws the frame's
//! decimated waveform about a midline ([`draw_scope`]); like the levels, the
//! drawn waveform lives in [`SpectrumDisplay`] between draws and decays when
//! no fresh frame carries one. Flow keeps a per-column energy ring
//! ([`FlowHistory`]): every fresh frame resamples into it, frames that stop
//! cool it toward silence, and [`draw_flow`] stacks each column bottom-up,
//! colored green → amber → cream by energy. Storage folds the default to
//! absent — `Bars` never serializes — but the spelling vocabulary here is
//! the CLI's `--visualizer` too, where `bars` is a legal value; parsing is
//! total over the vocabulary, and only storage hides the default.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::application::transport::PlaybackPhase;
use crate::application::view::NowPlaying;
use crate::playback::command::LoadRequestId;
use crate::playback::spectrum::worker::{SpectrumFrame, WAVEFORM_COLUMNS, frame_is_fresh};
use crate::tui::theme::Theme;

/// What the spectrum row draws.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VisualizerMode {
    /// Frequency bars: the pre-M14 row.
    #[default]
    Bars,
    /// An oscilloscope of the waveform.
    Scope,
    /// An energy-history color field over the bars data.
    Flow,
}

impl VisualizerMode {
    /// The `v` key's cycle: bars → scope → flow → bars.
    pub fn next(self) -> Self {
        match self {
            Self::Bars => Self::Scope,
            Self::Scope => Self::Flow,
            Self::Flow => Self::Bars,
        }
    }

    /// The spelling for a mode, shared with the CLI's `--visualizer`: `bars`
    /// is a legal value there, so parsing is total over the vocabulary. An
    /// unknown string is refused and the caller defaults, as `RepeatMode`
    /// does for a hand-edited file.
    pub fn from_stored(raw: &str) -> Option<Self> {
        match raw {
            "bars" => Some(Self::Bars),
            "scope" => Some(Self::Scope),
            "flow" => Some(Self::Flow),
            _ => None,
        }
    }

    /// The storage spelling (`RawState`'s additive field): the default
    /// serializes as absent, like `RepeatMode::Off`.
    pub fn as_stored(self) -> Option<&'static str> {
        match self {
            Self::Bars => None,
            Self::Scope => Some("scope"),
            Self::Flow => Some("flow"),
        }
    }
}

/// How much of each drawn level survives a draw without a fresh frame.
pub const DECAY_PER_DRAW: f32 = 0.85;

/// How long a peak stays where its bar left it before it starts to fall.
pub const PEAK_HOLD: Duration = Duration::from_millis(500);
/// How much of the full height a falling peak loses each second.
pub const PEAK_FALL_PER_SECOND: f32 = 0.5;

/// Whether the analysis worker should be running for this frame.
pub fn wants_analysis(spectrum: Option<Rect>, phase: PlaybackPhase) -> bool {
    spectrum.is_some_and(|area| !area.is_empty()) && phase == PlaybackPhase::Playing
}

/// Everything one draw decides the spectrum from.
#[derive(Clone, Copy)]
pub struct DrawSource<'a> {
    pub phase: PlaybackPhase,
    pub now_playing: Option<&'a NowPlaying>,
    /// The load token `Session` has adopted.
    pub adopted: Option<LoadRequestId>,
    /// The analysis worker's latest frame, `None` while disabled.
    pub frame: Option<&'a SpectrumFrame>,
}

impl<'a> DrawSource<'a> {
    /// Whether `frame` may be drawn at `now`: playback is still `Playing`,
    /// the frame belongs to the adopted playback's revision, that playback's
    /// token is the adopted one, and the frame is fresh.
    fn drawable(&self, frame: &SpectrumFrame, now: Instant) -> bool {
        let Some(now_playing) = self.now_playing else {
            return false;
        };
        self.phase == PlaybackPhase::Playing
            && frame.session_rev == now_playing.session_rev
            && now_playing.load.is_some()
            && now_playing.load == self.adopted
            && frame_is_fresh(frame, now)
    }

    /// The frame's levels, when they may be drawn at `now`: playback is still
    /// `Playing`, the frame belongs to the adopted playback's revision, that
    /// playback's token is the adopted one, and the frame is fresh.
    pub fn fresh_levels(&self, now: Instant) -> Option<&'a [f32]> {
        let frame = self.frame?;
        self.drawable(frame, now).then_some(frame.levels.as_slice())
    }

    /// The frame's decimated `(min, peak)` waveform, under the same gate
    /// chain as [`Self::fresh_levels`]. `None` also when the frame carries no
    /// waveform, as when nobody has asked for scope since it was analyzed.
    pub fn fresh_waveform(&self, now: Instant) -> Option<&'a [(f32, f32)]> {
        let frame = self.frame?;
        let waveform = frame.waveform.as_deref()?;
        self.drawable(frame, now).then_some(waveform)
    }
}

/// How many entries of one column's energy history survive: the ring depth
/// `draw_flow` resamples into the row's height.
pub const MAX_FLOW_DEPTH: usize = 16;

/// The levels resampled to `width` values, the bars value rule in
/// [`draw_spectrum`]: a value is its nearest band, or between two when the
/// columns outnumber the bands, so the row is full at every width. A
/// non-finite band is silence and every value clamps into the unit interval.
fn resample_levels(values: Option<&[f32]>, width: usize) -> Vec<f32> {
    let Some(values) = values.filter(|values| !values.is_empty()) else {
        return Vec::new();
    };
    let bands = values.len();
    let level_of = |band: usize| {
        values
            .get(band)
            .copied()
            .filter(|level| level.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    };
    (0..width)
        .map(|column| {
            if width <= bands {
                return level_of(column * bands / width);
            }
            let at = column as f32 * (bands - 1) as f32 / (width - 1) as f32;
            let low = at.floor();
            // `at` is within [0, bands - 1].
            let band = low as usize;
            let next = level_of((band + 1).min(bands - 1));
            level_of(band) + (next - level_of(band)) * (at - low)
        })
        .collect()
}

/// The flow's memory: one energy ring per column, [`MAX_FLOW_DEPTH`] entries
/// deep, each a frame's resampled level for that column, newest last. A
/// width change rebuilds from scratch — the columns of an old width mean
/// nothing to a new one — and frames that stop cool the whole ring toward
/// silence by [`DECAY_PER_DRAW`].
#[derive(Debug, Default)]
pub struct FlowHistory {
    /// The width the columns were last pushed at.
    width: u16,
    /// One ring per column, oldest first, so the newest entry is last.
    columns: Vec<Vec<f32>>,
}

impl FlowHistory {
    /// Pushes one frame: `levels` resampled to `width` columns by the bars
    /// value rule (`resample_levels`). A `width` different from the last
    /// push's rebuilds the columns empty first. `None` or empty levels push
    /// nothing — time moves only when a frame (or [`Self::decay`]) says so.
    pub fn push(&mut self, width: u16, levels: Option<&[f32]>) {
        if self.width != width {
            self.width = width;
            self.columns = vec![Vec::new(); usize::from(width)];
        }
        let Some(levels) = levels.filter(|levels| !levels.is_empty()) else {
            return;
        };
        let row = resample_levels(Some(levels), usize::from(width));
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
            if column.len() > MAX_FLOW_DEPTH {
                column.remove(0);
            }
        }
    }

    /// Cools every entry by [`DECAY_PER_DRAW`]: frames stop, the lava
    /// cools.
    pub fn decay(&mut self) {
        for column in &mut self.columns {
            for value in column {
                *value *= DECAY_PER_DRAW;
            }
        }
    }

    /// The column's history, oldest first and the newest frame last; empty
    /// past the width the history was pushed at.
    pub fn column(&self, x: usize) -> &[f32] {
        self.columns.get(x).map_or(&[][..], Vec::as_slice)
    }

    /// The width the columns live at: the flow renderer maps the row's
    /// columns onto it.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Whether no column carries an entry: fresh out of the box, just
    /// rebuilt at a new width, or wiped by a mode switch.
    pub fn is_empty(&self) -> bool {
        self.columns.iter().all(Vec::is_empty)
    }
}

/// The levels the spectrum row draws, kept across draws so they can decay.
#[derive(Debug, Default)]
pub struct SpectrumDisplay {
    levels: Vec<f32>,
    peaks: Vec<f32>,
    /// The drawn waveform between draws, scope mode's state: `(min, peak)`
    /// pairs about the midline, decayed like the levels when no fresh frame
    /// carries one.
    waveform: Option<Vec<(f32, f32)>>,
    /// The flow's per-column energy history, kept across draws so each
    /// fresh frame can push into it and stopped frames can cool it.
    flow: FlowHistory,
    /// The mode of the previous update: a different one resets `flow`.
    last_mode: Option<VisualizerMode>,
    /// When each peak was last pushed up by its bar.
    raised: Vec<Instant>,
    updated: Option<Instant>,
}

impl SpectrumDisplay {
    /// Takes fresh levels — and, in flow mode, pushes them into the
    /// per-column history; in scope mode, the fresh waveform — when
    /// `source` allows them at `now`; otherwise decays what was drawn last
    /// by [`DECAY_PER_DRAW`]. A `mode` different from the previous update's
    /// resets the flow history first: a mode switch starts fresh (spec
    /// §1.2). The history lives at the levels' own band count; the flow
    /// renderer maps the row's columns onto it.
    pub fn update(&mut self, source: &DrawSource<'_>, now: Instant, mode: VisualizerMode) {
        if self.last_mode != Some(mode) {
            self.flow = FlowHistory::default();
        }
        self.last_mode = Some(mode);
        match source.fresh_levels(now) {
            Some(levels) => {
                self.levels.clear();
                self.levels.extend(
                    levels
                        .iter()
                        .map(|level| if level.is_finite() { *level } else { 0.0 }),
                );
                if mode == VisualizerMode::Flow {
                    let width = u16::try_from(self.levels.len()).unwrap_or(u16::MAX);
                    self.flow.push(width, Some(&self.levels));
                }
            }
            None => {
                for level in &mut self.levels {
                    *level *= DECAY_PER_DRAW;
                }
                if mode == VisualizerMode::Flow {
                    self.flow.decay();
                }
            }
        }
        // Only scope takes the waveform; everything it held otherwise decays
        // toward the midline exactly as bars fall.
        let fresh = if mode == VisualizerMode::Scope {
            source.fresh_waveform(now)
        } else {
            None
        };
        match fresh {
            Some(waveform) => self.waveform = Some(waveform.to_vec()),
            None => {
                if let Some(waveform) = &mut self.waveform {
                    for (min, peak) in waveform {
                        *min *= DECAY_PER_DRAW;
                        *peak *= DECAY_PER_DRAW;
                    }
                }
            }
        }
        self.settle_peaks(now);
    }

    /// Lifts each peak to its bar; one past its hold falls by the time since
    /// the last update, down to the bar at most.
    fn settle_peaks(&mut self, now: Instant) {
        let elapsed = self.updated.map_or(Duration::ZERO, |updated| {
            now.saturating_duration_since(updated)
        });
        self.updated = Some(now);
        let fall = PEAK_FALL_PER_SECOND * elapsed.as_secs_f32();
        self.peaks.resize(self.levels.len(), 0.0);
        self.raised.resize(self.levels.len(), now);
        for ((peak, raised), level) in self
            .peaks
            .iter_mut()
            .zip(&mut self.raised)
            .zip(&self.levels)
        {
            if now.saturating_duration_since(*raised) > PEAK_HOLD {
                *peak -= fall;
            }
            if *peak <= *level {
                *peak = *level;
                *raised = now;
            }
        }
    }

    /// `None` until any levels were drawn: the row then shows flat bars.
    pub fn levels(&self) -> Option<&[f32]> {
        (!self.levels.is_empty()).then_some(self.levels.as_slice())
    }

    /// Each band's peak, alongside [`Self::levels`].
    pub fn peaks(&self) -> Option<&[f32]> {
        (!self.peaks.is_empty()).then_some(self.peaks.as_slice())
    }

    /// The flow's per-column energy history: what [`draw_flow`] stacks.
    pub fn flow(&self) -> &FlowHistory {
        &self.flow
    }

    /// The drawn waveform between draws, `None` until scope took one: the
    /// row then shows the flat midline.
    pub fn waveform(&self) -> Option<&[(f32, f32)]> {
        self.waveform
            .as_deref()
            .filter(|waveform| !waveform.is_empty())
    }
}

/// The scope's one glyph: a full block, stacked from the peak's reach above
/// the midline down to the minimum's below it.
const SCOPE_BLOCK: &str = "█";

/// The waveform's `(min, peak)` envelope about the midline: one column per
/// `area.width`, resampled from the frame's [`WAVEFORM_COLUMNS`] by nearest
/// index, green from where the peak reaches up to where the minimum dips
/// down, both clamped into the area. A silent column (`min == peak == 0`) is
/// the midline cell in the muted color while two rows can spare a line,
/// nothing one row tall; absent or short data is silence, so a stale or
/// waveform-less frame falls flat exactly as bars fall.
pub fn draw_scope(buffer: &mut Buffer, area: Rect, waveform: Option<&[(f32, f32)]>, theme: &Theme) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    if width == 0 || height == 0 {
        return;
    }
    let midline = area.y + area.height / 2;
    let half = f32::from(area.height / 2);
    for x in area.x..area.right() {
        let column = usize::from(x - area.x) * WAVEFORM_COLUMNS / width;
        // A non-finite bound is silence, the rule the levels draw by.
        let (min, peak) = waveform
            .and_then(|waveform| waveform.get(column))
            .and_then(|&(min, peak)| {
                min.is_finite()
                    .then_some(min)
                    .zip(peak.is_finite().then_some(peak))
            })
            .unwrap_or((0.0, 0.0));
        if min == 0.0 && peak == 0.0 {
            if height >= 2
                && let Some(cell) = buffer.cell_mut((x, midline))
            {
                cell.set_symbol(SCOPE_BLOCK).set_fg(theme.muted);
            }
            continue;
        }
        // Both extents round into [0, half], so they stay in the area.
        let up = (peak.clamp(0.0, 1.0) * half).round() as u16;
        let down = (min.abs().clamp(0.0, 1.0) * half).round() as u16;
        let top = midline.saturating_sub(up);
        let bottom = midline
            .saturating_add(down)
            .min(area.bottom().saturating_sub(1));
        for y in top..=bottom {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(SCOPE_BLOCK).set_fg(theme.green);
            }
        }
    }
}

/// The flow's one glyph: a full block, one per cell of the stacked column.
const FLOW_BLOCK: &str = "█";

/// The flow's gradient: a value below one half blends `theme.green` into
/// `theme.amber` and the rest blends `theme.amber` into `theme.cream`, each
/// channel a linear-RGB lerp, so the lava runs green through amber to cream
/// as it heats.
fn flow_color(value: f32, theme: &Theme) -> Color {
    let (from, to, t) = if value < 0.5 {
        (theme.green, theme.amber, value / 0.5)
    } else {
        (theme.amber, theme.cream, (value - 0.5) / 0.5)
    };
    lerp_color(from, to, t)
}

/// Blends two colors channel-wise in linear RGB; a pair that is not RGB
/// passes the first through (the built-in palettes are all RGB).
fn lerp_color(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix =
                |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        _ => from,
    }
}

/// The flow: each column of `area` is one [`FlowHistory`] column stacked
/// bottom-up — the oldest frame at the bottom row, the newest at the top —
/// resampled to `area.height` rows by nearest entry and colored by value
/// through `flow_color`. Columns map onto the history's own width by
/// nearest, the bars rule, so the row is full at every width. A zero-value
/// cell draws nothing, so silence stays silent; a history without any
/// entries falls back to the live `levels`, so the first fresh frame shows
/// at once.
pub fn draw_flow(
    buffer: &mut Buffer,
    area: Rect,
    history: &FlowHistory,
    levels: Option<&[f32]>,
    theme: &Theme,
) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    if width == 0 || height == 0 {
        return;
    }
    // No history yet: the live frame is every column's whole stack.
    let live = history.is_empty().then(|| resample_levels(levels, width));
    let hist_width = usize::from(history.width());
    for x in area.x..area.right() {
        let column = usize::from(x - area.x);
        let values: &[f32] = match live.as_deref() {
            Some(live) => live,
            None => history.column(column * hist_width / width),
        };
        if values.is_empty() {
            continue;
        }
        // The row's value: its nearest entry, bottom row oldest. One entry
        // (or one row) is that entry everywhere.
        let at = |from_bottom: usize| {
            if values.len() == 1 || height <= 1 {
                values[0]
            } else {
                values[from_bottom * (values.len() - 1) / (height - 1)]
            }
        };
        for (from_bottom, y) in (area.top()..area.bottom()).rev().enumerate() {
            let value = at(from_bottom);
            if value <= 0.0 {
                continue;
            }
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(FLOW_BLOCK).set_fg(flow_color(value, theme));
            }
        }
    }
}

#[cfg(test)]
mod visualizer_mode_tests {
    use super::VisualizerMode;

    #[test]
    fn the_cycle_returns_to_bars() {
        assert_eq!(VisualizerMode::Bars.next(), VisualizerMode::Scope);
        assert_eq!(VisualizerMode::Scope.next(), VisualizerMode::Flow);
        assert_eq!(VisualizerMode::Flow.next(), VisualizerMode::Bars);
    }

    #[test]
    fn storage_round_trips_and_defaults_unknown_values() {
        assert_eq!(VisualizerMode::Bars.as_stored(), None);
        for mode in [VisualizerMode::Scope, VisualizerMode::Flow] {
            let stored = mode.as_stored().expect("a spelling for a non-default mode");
            assert_eq!(VisualizerMode::from_stored(stored), Some(mode));
        }
        assert_eq!(
            VisualizerMode::from_stored("bars"),
            Some(VisualizerMode::Bars)
        );
        assert_eq!(VisualizerMode::from_stored("Scope"), None);
        assert_eq!(VisualizerMode::from_stored(""), None);
        assert_eq!(VisualizerMode::from_stored("plasma"), None);
    }
}
