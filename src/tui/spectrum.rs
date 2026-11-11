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

use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::application::transport::PlaybackPhase;
use crate::application::view::NowPlaying;
use crate::playback::command::LoadRequestId;
use crate::playback::spectrum::worker::{SpectrumFrame, frame_is_fresh};

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
}

/// The levels the spectrum row draws, kept across draws so they can decay.
#[derive(Debug, Default)]
pub struct SpectrumDisplay {
    levels: Vec<f32>,
    peaks: Vec<f32>,
    /// When each peak was last pushed up by its bar.
    raised: Vec<Instant>,
    updated: Option<Instant>,
}

impl SpectrumDisplay {
    /// Takes fresh levels when `source` allows them at `now`; otherwise
    /// decays what was drawn last by [`DECAY_PER_DRAW`].
    pub fn update(&mut self, source: &DrawSource<'_>, now: Instant) {
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
}
