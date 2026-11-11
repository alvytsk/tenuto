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
//! decimated waveform as a connected trace about a midline ([`draw_scope`]);
//! like the levels, the drawn waveform lives in [`SpectrumDisplay`] between
//! draws and decays when no fresh frame carries one. Flow keeps a scrolling
//! waterfall ([`FlowHistory`]): every fresh frame becomes the newest column
//! (the field's colors frozen as drawn), frames that stop cool the field
//! toward silence, and [`draw_flow`] paints columns of color right to left,
//! bass at the bottom, green → amber → cream by energy.
//! Storage folds the default to absent — `Bars` never serializes — but the
//! spelling vocabulary here is the CLI's `--visualizer` too, where `bars`
//! is a legal value; parsing is total over the vocabulary, and only
//! storage hides the default.

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
    /// A scrolling color field over the bars data.
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

    /// The frame's decimated waveform trace, under the same gate chain as
    /// [`Self::fresh_levels`]. `None` also when the frame carries no
    /// waveform, as when nobody has asked for scope since it was analyzed.
    pub fn fresh_waveform(&self, now: Instant) -> Option<&'a [f32]> {
        let frame = self.frame?;
        let waveform = frame.waveform.as_deref()?;
        self.drawable(frame, now).then_some(waveform)
    }
}

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

/// The flow's memory: the waterfall's columns — one stored frame per
/// column, that frame's levels resampled to the panel's height, oldest
/// column first, newest last. Stored columns anchor at the right edge, so
/// while fewer than `width` exist the field scrolls in from the right over
/// ~width frames. A width or height change rebuilds from scratch — the
/// columns of an old shape mean nothing to a new one — and frames that
/// stop cool the whole field toward silence by [`DECAY_PER_DRAW`].
#[derive(Debug, Default)]
pub struct FlowHistory {
    /// The width the columns were last pushed at.
    width: u16,
    /// The height each column was last pushed at.
    height: u16,
    /// One frame per column, oldest first, newest last; empty while the
    /// field warms up.
    columns: Vec<Vec<f32>>,
}

impl FlowHistory {
    /// Pushes one frame: `levels` resampled to `height` rows by the bars
    /// value rule (`resample_levels`), appended as the newest (rightmost)
    /// column, dropping the oldest once `width` columns are stored. `None`
    /// or empty levels push a column of zeros, so the field still scrolls
    /// through silence. A `width` or `height` different from the last
    /// push's rebuilds the field empty first. A `width` or `height` of 0 is
    /// no panel: nothing is pushed.
    pub fn push(&mut self, width: u16, height: u16, levels: Option<&[f32]>) {
        if width == 0 || height == 0 {
            return;
        }
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.columns.clear();
        }
        let rows = resample_levels(levels, usize::from(height));
        let rows = if rows.is_empty() {
            vec![0.0; usize::from(height)]
        } else {
            rows
        };
        if self.columns.len() == usize::from(width) {
            self.columns.remove(0);
        }
        self.columns.push(rows);
    }

    /// Cools every stored value by [`DECAY_PER_DRAW`]: frames stop, the
    /// field cools.
    pub fn decay(&mut self) {
        for column in &mut self.columns {
            for value in column {
                *value *= DECAY_PER_DRAW;
            }
        }
    }

    /// The display column's stored frame: `height` values, index 0 the
    /// bottom row (bass). Column index 0 is the leftmost (oldest); while
    /// fewer than `width` columns are stored they sit at the right edge and
    /// a column left of them is empty.
    pub fn column(&self, x: usize) -> &[f32] {
        let skip = usize::from(self.width).saturating_sub(self.columns.len());
        x.checked_sub(skip)
            .and_then(|index| self.columns.get(index))
            .map_or(&[][..], Vec::as_slice)
    }

    /// Whether no frame is stored: fresh out of the box, just rebuilt at a
    /// new width or height, or wiped by a mode switch.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// The levels the spectrum row draws, kept across draws so they can decay.
#[derive(Debug, Default)]
pub struct SpectrumDisplay {
    levels: Vec<f32>,
    peaks: Vec<f32>,
    /// The drawn waveform between draws, scope mode's state: the frame's
    /// signed points about the midline, decayed like the levels when no
    /// fresh frame carries one.
    waveform: Option<Vec<f32>>,
    /// The flow's scrolling field, kept across draws so each fresh frame
    /// can push into it and stopped frames can cool it.
    flow: FlowHistory,
    /// The mode of the previous update: a different one resets `flow`.
    last_mode: Option<VisualizerMode>,
    /// When each peak was last pushed up by its bar.
    raised: Vec<Instant>,
    updated: Option<Instant>,
}

impl SpectrumDisplay {
    /// Takes fresh levels — and, in flow mode, pushes them into the
    /// scrolling field; in scope mode, the fresh waveform — when `source`
    /// allows them at `now`; otherwise decays what was drawn last by
    /// [`DECAY_PER_DRAW`]. A `mode` different from the previous update's
    /// resets the flow field first: a mode switch starts fresh (spec
    /// §1.2). The field lives at `width` columns of `height` rows — the
    /// spectrum region's own size (0 is no row, and then nothing pushes) —
    /// so every panel column owns a frame and a resize rebuilds; the flow
    /// renderer reads the columns column-for-column.
    pub fn update(
        &mut self,
        source: &DrawSource<'_>,
        now: Instant,
        mode: VisualizerMode,
        width: u16,
        height: u16,
    ) {
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
                    self.flow.push(width, height, Some(&self.levels));
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
                    for point in waveform {
                        *point *= DECAY_PER_DRAW;
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

    /// The flow's scrolling field: what [`draw_flow`] paints.
    pub fn flow(&self) -> &FlowHistory {
        &self.flow
    }

    /// The drawn waveform between draws, `None` until scope took one: the
    /// row then shows the flat midline.
    pub fn waveform(&self) -> Option<&[f32]> {
        self.waveform
            .as_deref()
            .filter(|waveform| !waveform.is_empty())
    }
}

/// The scope's one glyph: a full block, one per cell of the trace.
const SCOPE_BLOCK: &str = "█";

/// The waveform trace about the midline: one column per `area.width`,
/// resampled from the frame's [`WAVEFORM_COLUMNS`] points by nearest index.
/// The column's cell sits at `midline - round(point * half)`, clamped into
/// the area, and the cells between it and the previous column's row fill in
/// too, so the trace reads as a line — the wave itself — and not dots;
/// column 0 has no predecessor. Every painted cell is `theme.green`.
///
/// Absent points, or points that are all zero (a silent block), are the
/// existing silence: muted midline cells while two rows can spare a line,
/// nothing one row tall. No idle animation — a silent block is a flat
/// line, which is honest.
pub fn draw_scope(buffer: &mut Buffer, area: Rect, waveform: Option<&[f32]>, theme: &Theme) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    if width == 0 || height == 0 {
        return;
    }
    let midline = area.y + area.height / 2;
    // Silence: nothing to trace, so the muted midline (and at one row,
    // nothing) is all a silent block ever draws.
    if waveform.is_none_or(|points| {
        points
            .iter()
            .all(|point| !point.is_finite() || *point == 0.0)
    }) {
        if height >= 2 {
            for x in area.x..area.right() {
                if let Some(cell) = buffer.cell_mut((x, midline)) {
                    cell.set_symbol(SCOPE_BLOCK).set_fg(theme.muted);
                }
            }
        }
        return;
    }
    let half = f32::from(area.height / 2);
    let top = i32::from(area.y);
    let bottom = i32::from(area.bottom()) - 1;
    // The previous column's trace row; column 0 has none, so it draws its
    // cell alone.
    let mut previous: Option<u16> = None;
    for x in area.x..area.right() {
        let column = usize::from(x - area.x) * WAVEFORM_COLUMNS / width;
        // A non-finite point is silence, the rule the levels draw by.
        let point = waveform
            .and_then(|points| points.get(column))
            .copied()
            .filter(|point| point.is_finite())
            .unwrap_or(0.0)
            .clamp(-1.0, 1.0);
        let row = (i32::from(midline) - (point * half).round() as i32).clamp(top, bottom) as u16;
        let from = previous.map_or(row, |previous| previous.min(row));
        let to = previous.map_or(row, |previous| previous.max(row));
        for y in from..=to {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(SCOPE_BLOCK).set_fg(theme.green);
            }
        }
        previous = Some(row);
    }
}

/// The flow's one glyph: a full block, one per cell of a stored frame.
const FLOW_BLOCK: &str = "█";

/// The flow's gradient: a value below one half blends `theme.green` into
/// `theme.amber` and the rest blends `theme.amber` into `theme.cream`, each
/// channel lerped in sRGB bytes, so the field runs green through amber to
/// cream as it heats.
fn flow_color(value: f32, theme: &Theme) -> Color {
    let (from, to, t) = if value < 0.5 {
        (theme.green, theme.amber, value / 0.5)
    } else {
        (theme.amber, theme.cream, (value - 0.5) / 0.5)
    };
    lerp_color(from, to, t)
}

/// Blends two colors channel-wise in sRGB bytes; a pair that is not RGB
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

/// The flow: a scrolling waterfall. Columns are time — each [`FlowHistory`]
/// column is one stored frame, the newest at the right edge, scrolling
/// left as frames arrive — and rows are frequency, the bass band's value in
/// the bottom row. Every painted cell is frozen as its column was drawn: a
/// full block colored by value through `flow_color`, green → amber →
/// cream. A column left of the warm-up (the field scrolls in from the
/// right over ~width frames) draws nothing, and a zero-value cell draws
/// nothing, so silence stays silent.
pub fn draw_flow(buffer: &mut Buffer, area: Rect, history: &FlowHistory, theme: &Theme) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    if width == 0 || height == 0 {
        return;
    }
    for x in area.x..area.right() {
        let column = usize::from(x - area.x);
        let values = history.column(column);
        for (from_bottom, y) in (area.top()..area.bottom()).rev().enumerate() {
            let value = values.get(from_bottom).copied().unwrap_or(0.0);
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
