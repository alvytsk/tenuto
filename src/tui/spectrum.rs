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
//! no fresh frame carries one. Storage folds the default to
//! absent — `Bars` never serializes — but the spelling vocabulary here is
//! the CLI's `--visualizer` too, where `bars` is a legal value; parsing is
//! total over the vocabulary, and only storage hides the default.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

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

/// The levels the spectrum row draws, kept across draws so they can decay.
#[derive(Debug, Default)]
pub struct SpectrumDisplay {
    levels: Vec<f32>,
    peaks: Vec<f32>,
    /// The drawn waveform between draws, scope mode's state: `(min, peak)`
    /// pairs about the midline, decayed like the levels when no fresh frame
    /// carries one.
    waveform: Option<Vec<(f32, f32)>>,
    /// When each peak was last pushed up by its bar.
    raised: Vec<Instant>,
    updated: Option<Instant>,
}

impl SpectrumDisplay {
    /// Takes fresh levels — and, in scope mode, the fresh waveform — when
    /// `source` allows them at `now`; otherwise decays what was drawn last
    /// by [`DECAY_PER_DRAW`].
    pub fn update(&mut self, source: &DrawSource<'_>, now: Instant, mode: VisualizerMode) {
        match source.fresh_levels(now) {
            Some(levels) => {
                self.levels.clear();
                self.levels.extend(
                    levels
                        .iter()
                        .map(|level| if level.is_finite() { *level } else { 0.0 }),
                );
            }
            None => {
                for level in &mut self.levels {
                    *level *= DECAY_PER_DRAW;
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
