//! M14 §1.3: the visualizer mode persists as an additive raw string — the
//! M13 additive-field rule, with persistence carrying the spelling and
//! never parsing the vocabulary.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

#[path = "support/views.rs"]
mod views;

use std::time::{Duration, Instant};

use clap::Parser;
use runtime::rig_with;
use tenuto::application::runtime::AppCommand;
use tenuto::cli::{Cli, CliCommand};
use tenuto::persistence::model::PersistedState;
use tenuto::playback::output::Nanos;
use tenuto::playback::spectrum::worker::{
    AnalyzedWindow, FrameSchedule, MappingKey, SlotUpdate, SpectrumFrame, WAVEFORM_COLUMNS,
    decimate_waveform,
};
use tenuto::session::{Action, Session};

/// M14 §1.3: the visualizer mode round-trips through the state file. The
/// default state carries no mode: absent on disk means "the reader's
/// default" (bars), which is the *reader's* decision, not this layer's.
#[test]
fn the_selected_visualizer_mode_round_trips_through_json() {
    let mut state = PersistedState::default();
    assert_eq!(state.visualizer_mode(), None, "the default carries no mode");
    state.set_visualizer_mode(Some("flow".into()));

    let json = serde_json::to_string(&state).expect("serializes");
    assert!(json.contains("\"visualizer_mode\":\"flow\""), "{json}");

    let loaded: PersistedState = serde_json::from_str(&json).expect("round trips");
    assert_eq!(loaded.visualizer_mode(), Some("flow"));
}

/// Raw passthrough: persistence never parses the vocabulary, so an unknown
/// spelling survives the file untouched and the *reader* (which owns
/// `VisualizerMode`) defaults it.
#[test]
fn an_unknown_visualizer_mode_spelling_passes_through_raw() {
    let json = r#"{"schema_version":1,"checkpoints":{},"visualizer_mode":"plasma"}"#;
    let loaded: PersistedState = serde_json::from_str(json).expect("loads leniently");
    assert_eq!(loaded.visualizer_mode(), Some("plasma"));
}

/// M14 §1.3: `set_visualizer_mode` writes the raw, already-validated
/// spelling into state and submits — the `set_repeat` shape. `None` is a
/// real value: cycling back to bars clears the field rather than leaving a
/// stale spelling behind.
#[test]
fn set_visualizer_mode_writes_and_clears_the_raw_spelling() {
    let mut session = Session::new(PersistedState::default());
    assert_eq!(
        session.state().visualizer_mode(),
        None,
        "the default carries no mode"
    );

    let action = session.set_visualizer_mode(Some("flow".into()));
    assert!(
        matches!(action, Action::Submit { .. }),
        "a mode change persists"
    );
    assert_eq!(session.state().visualizer_mode(), Some("flow"));

    let action = session.set_visualizer_mode(None);
    assert!(
        matches!(action, Action::Submit { .. }),
        "clearing persists too"
    );
    assert_eq!(
        session.state().visualizer_mode(),
        None,
        "None clears the field"
    );
}

/// M14 §2.1: the same shape for the theme, still carrying the raw stored
/// spelling — the session layer never parses either vocabulary.
#[test]
fn set_theme_writes_and_clears_the_raw_spelling() {
    let mut session = Session::new(PersistedState::default());
    assert_eq!(
        session.state().theme(),
        None,
        "the default carries no theme"
    );

    let action = session.set_theme(Some("winamp".into()));
    assert!(
        matches!(action, Action::Submit { .. }),
        "a theme change persists"
    );
    assert_eq!(session.state().theme(), Some("winamp"));

    let action = session.set_theme(None);
    assert!(
        matches!(action, Action::Submit { .. }),
        "clearing persists too"
    );
    assert_eq!(session.state().theme(), None, "None clears the field");
}

/// M14 §1.3: `AppCommand::SetTheme` dispatched through the runtime's
/// command handling lands in state — the `SetRepeat` shape (spec §1.3's
/// precedent), so the command path and the direct call agree.
#[test]
fn the_set_theme_command_lands_in_state_through_the_runtime() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime
        .handle(AppCommand::SetTheme(Some("winamp".into())));
    assert_eq!(
        rig.runtime.session().state().theme(),
        Some("winamp"),
        "the dispatched theme lands in state"
    );
}

/// M14 §1.3: `--visualizer` accepts a stored spelling and rejects an
/// unknown one at parse time, naming the valid set.
#[test]
fn tui_visualizer_parses_a_stored_spelling_and_names_the_valid_set_when_rejected() {
    let parsed =
        Cli::try_parse_from(["tenuto", "tui", "--visualizer", "flow"]).expect("flow parses");
    let Some(CliCommand::Tui { visualizer, .. }) = parsed.command else {
        panic!("`tenuto tui --visualizer flow` parses to the tui subcommand");
    };
    assert_eq!(visualizer.as_deref(), Some("flow"));

    let error = Cli::try_parse_from(["tenuto", "tui", "--visualizer", "plasma"])
        .expect_err("an unknown visualizer is a usage error");
    assert!(
        error.to_string().contains("valid: bars, scope, flow"),
        "{error}"
    );
}

/// Both flags absent: `None` on each. That absence is what the startup
/// (Task 9) reads as "use the saved state", so it must stay distinguishable
/// from an explicitly passed value.
#[test]
fn absent_theme_and_visualizer_flags_parse_to_none() {
    let parsed = Cli::try_parse_from(["tenuto", "tui"]).expect("the bare tui parses");
    let Some(CliCommand::Tui {
        theme, visualizer, ..
    }) = parsed.command
    else {
        panic!("`tenuto tui` parses to the tui subcommand");
    };
    assert_eq!(theme, None);
    assert_eq!(visualizer, None);
}

/// M14 §1.3: the bare form parses the same flag — `tenuto --visualizer
/// flow` opens the player with that mode for the run, an unknown spelling
/// is refused with the valid set named, and the bare invocation with no
/// flags carries `None` (the startup's "use the saved state").
#[test]
fn bare_visualizer_parses_a_stored_spelling_and_names_the_valid_set_when_rejected() {
    let parsed = Cli::try_parse_from(["tenuto", "--visualizer", "flow"]).expect("flow parses");
    assert!(
        parsed.command.is_none(),
        "the bare form carries no subcommand"
    );
    assert_eq!(parsed.visualizer.as_deref(), Some("flow"));

    let error = Cli::try_parse_from(["tenuto", "--visualizer", "plasma"])
        .expect_err("an unknown visualizer is a usage error");
    assert!(
        error.to_string().contains("valid: bars, scope, flow"),
        "{error}"
    );

    let parsed = Cli::try_parse_from(["tenuto"]).expect("the bare form parses");
    assert_eq!(parsed.visualizer, None);
}

// ------------------------------------------------ the waveform trace points

// ------------------------------------------------ the decimated waveform

/// §1.1: a constant block decimates to the same `(min, peak)` pair in every
/// one of the 96 output columns.
#[test]
fn a_constant_block_decimates_to_the_same_pair_in_every_column() {
    let waveform = decimate_waveform(&vec![0.5_f32; 2048]);
    assert_eq!(waveform.len(), WAVEFORM_COLUMNS);
    assert!(
        waveform.iter().all(|pair| *pair == (0.5, 0.5)),
        "every column is (0.5, 0.5), got {waveform:?}"
    );
}

/// §1.1: column `c` covers samples `[c * len / 96, (c+1) * len / 96)`, so a
/// spike at sample 1024 of 2048 lands in column 48 (`48 * 2048 / 96 == 1024`)
/// and nowhere else.
#[test]
fn a_spike_lands_in_the_column_that_covers_its_sample() {
    let mut samples = vec![0.0_f32; 2048];
    samples[1024] = -1.0;
    let waveform = decimate_waveform(&samples);
    assert_eq!(waveform[48], (-1.0, 0.0), "column 48 covers sample 1024");
    assert!(
        waveform
            .iter()
            .enumerate()
            .all(|(c, pair)| c == 48 || *pair == (0.0, 0.0)),
        "every other column stays on the midline: {waveform:?}"
    );
}

/// §1.1: a block shorter than the columns it precedes still decimates to 96
/// pairs, and every column without samples is the flat midline. With ten
/// samples the samples tile into columns 9, 19, 28, ... (`c * 10 / 96`
/// reaches 1 only at `c == 10`), so the zeros after the first keep every
/// column from 10 up empty.
#[test]
fn a_short_block_zero_fills_the_columns_without_samples() {
    let mut samples = vec![0.0_f32; 10];
    samples[0] = 0.5;
    let waveform = decimate_waveform(&samples);
    assert_eq!(waveform.len(), WAVEFORM_COLUMNS);
    assert_eq!(waveform[9], (0.5, 0.5), "column 9 covers sample 0");
    assert!(
        waveform
            .iter()
            .enumerate()
            .all(|(c, pair)| c == 9 || *pair == (0.0, 0.0)),
        "columns without samples are zero-filled: {waveform:?}"
    );
    assert!(
        waveform[10..].iter().all(|pair| *pair == (0.0, 0.0)),
        "columns 10 and up carry no sample: {waveform:?}"
    );
}

/// §1.1: an empty block is silence — 96 midline pairs, the scope's flat line.
#[test]
fn an_empty_block_decimates_to_96_midline_pairs() {
    assert_eq!(
        decimate_waveform(&[]),
        vec![(0.0_f32, 0.0_f32); WAVEFORM_COLUMNS]
    );
}

/// §1.1: NaN is silence — the same rule the levels follow in
/// `Analysis::accept` — and finite values beyond ±1 clamp into the unit
/// interval the widget renders.
#[test]
fn non_finite_samples_are_silence_and_out_of_range_samples_clamp() {
    let waveform = decimate_waveform(&[f32::NAN; 2048]);
    assert!(
        waveform.iter().all(|pair| *pair == (0.0, 0.0)),
        "all-NaN input decimates to the midline: {waveform:?}"
    );

    // A NaN beside real samples vanishes from that column's envelope.
    let mut samples = vec![0.5_f32; 2048];
    samples[3] = f32::NAN;
    assert_eq!(decimate_waveform(&samples)[0], (0.0, 0.5));

    let mut samples = vec![0.0_f32; 2048];
    samples[10] = 2.5;
    samples[15] = -4.0;
    assert_eq!(decimate_waveform(&samples)[0], (-1.0, 1.0), "clamped");
}

// ------------------------------------------ the frame carries the waveform

const KEY: MappingKey = MappingKey {
    instance: 1,
    generation: 1,
    epoch: 2,
};

fn window(level: f32, waveform: Option<Vec<(f32, f32)>>) -> AnalyzedWindow {
    AnalyzedWindow {
        key: KEY,
        session_rev: 7,
        bands: vec![(40.0, 85.0)],
        levels: vec![level],
        waveform,
        predicted: Nanos(0),
    }
}

fn published(update: SlotUpdate) -> SpectrumFrame {
    match update {
        SlotUpdate::Publish(frame) => frame,
        other => panic!("expected a publication, got {other:?}"),
    }
}

/// §1.1: the waveform rides the frame exactly as the window carried it — no
/// smoothing, no reshaping — while the levels beside it still smooth, so the
/// pass-through is not vacuous.
#[test]
fn a_published_frame_carries_its_window_s_waveform_untouched() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    let first = vec![(0.25_f32, 0.75_f32), (-0.5_f32, 0.5_f32)];
    schedule.offer(window(1.0, Some(first.clone())));
    let frame = published(schedule.tick(t0, Nanos(0)));
    assert_eq!(frame.waveform, Some(first));

    // The second publication's level smooths against the first; its
    // waveform must arrive verbatim.
    let second = vec![(-1.0_f32, 1.0_f32); WAVEFORM_COLUMNS];
    schedule.offer(window(0.0, Some(second.clone())));
    let frame = published(schedule.tick(t0 + Duration::from_millis(50), Nanos(10)));
    assert_eq!(
        frame.waveform,
        Some(second),
        "the waveform is copied untouched"
    );
    assert_eq!(frame.levels, vec![0.85], "levels smoothed");
}

/// §1.1: a window analyzed while nobody wanted the waveform publishes a
/// frame with `waveform: None`.
#[test]
fn a_window_without_a_waveform_publishes_none() {
    let t0 = Instant::now();
    let mut schedule = FrameSchedule::default();
    schedule.offer(window(0.5, None));
    let frame = published(schedule.tick(t0, Nanos(0)));
    assert_eq!(frame.waveform, None);
}

// -------------------------------------------- the scope's view state

mod scope_display {
    use std::time::{Duration, Instant};

    use tenuto::application::transport::PlaybackPhase;
    use tenuto::application::view::NowPlaying;
    use tenuto::playback::command::LoadRequestId;
    use tenuto::playback::output::Nanos;
    use tenuto::playback::spectrum::worker::SpectrumFrame;
    use tenuto::tui::spectrum::{DrawSource, SpectrumDisplay, VisualizerMode};

    const TOKEN: LoadRequestId = LoadRequestId::from_raw(4);

    fn now_playing() -> NowPlaying {
        NowPlaying {
            session_rev: 9,
            load: Some(TOKEN),
            ..NowPlaying::unloaded()
        }
    }

    fn frame(published_at: Instant, waveform: Option<Vec<(f32, f32)>>) -> SpectrumFrame {
        SpectrumFrame {
            session_rev: 9,
            bands: vec![(40.0, 85.0)],
            levels: vec![0.5],
            waveform,
            at: Nanos(0),
            published_at,
        }
    }

    fn source<'a>(
        phase: PlaybackPhase,
        now_playing: &'a NowPlaying,
        frame: Option<&'a SpectrumFrame>,
    ) -> DrawSource<'a> {
        DrawSource {
            phase,
            now_playing: Some(now_playing),
            adopted: Some(TOKEN),
            frame,
        }
    }

    /// Both tuple entries decayed once toward zero by `DECAY_PER_DRAW`.
    fn decayed(display: &SpectrumDisplay, min: f32, peak: f32) -> bool {
        display.waveform().is_some_and(|waveform| {
            waveform.iter().all(|&(drawn_min, drawn_peak)| {
                (drawn_min - min * 0.85).abs() < 1e-6 && (drawn_peak - peak * 0.85).abs() < 1e-6
            })
        })
    }

    /// Scope takes the frame's fresh waveform, under the same gate chain the
    /// levels follow.
    #[test]
    fn scope_takes_a_fresh_matching_waveform() {
        let t0 = Instant::now();
        let np = now_playing();
        let pairs = vec![(-0.5_f32, 0.25_f32), (0.0_f32, 0.75_f32)];
        let fresh = frame(t0, Some(pairs.clone()));
        let mut display = SpectrumDisplay::default();
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&fresh)),
            t0,
            VisualizerMode::Scope,
            0,
        );
        assert_eq!(display.waveform(), Some(&pairs[..]));
    }

    /// A stale waveform (only the clock moved) decays both tuple entries
    /// toward zero, exactly as bars fall.
    #[test]
    fn a_stale_waveform_decays_both_entries() {
        let t0 = Instant::now();
        let np = now_playing();
        let stale = frame(t0, Some(vec![(-1.0_f32, 0.5_f32)]));
        let mut display = SpectrumDisplay::default();
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&stale)),
            t0,
            VisualizerMode::Scope,
            0,
        );
        let later = t0 + Duration::from_millis(150);
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&stale)),
            later,
            VisualizerMode::Scope,
            0,
        );
        assert!(decayed(&display, -1.0, 0.5), "{:?}", display.waveform());
    }

    /// Bars never take the waveform: its state stays `None` even when the
    /// frame carries one.
    #[test]
    fn bars_mode_never_takes_the_waveform() {
        let t0 = Instant::now();
        let np = now_playing();
        let fresh = frame(t0, Some(vec![(-1.0_f32, 1.0_f32)]));
        let mut display = SpectrumDisplay::default();
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&fresh)),
            t0,
            VisualizerMode::Bars,
            0,
        );
        assert_eq!(display.waveform(), None);
    }

    /// The same gate chain as `fresh_levels`: a frame for another revision or
    /// the unadopted token is refused, and a frame that carries no waveform
    /// (nobody wanted scope since it was analyzed) refuses too.
    #[test]
    fn fresh_waveform_refuses_a_foreign_revision_or_token() {
        let t0 = Instant::now();
        let np = now_playing();
        let fresh = frame(t0, Some(vec![(0.0_f32, 0.5_f32)]));
        let own = source(PlaybackPhase::Playing, &np, Some(&fresh));
        assert_eq!(own.fresh_waveform(t0), Some(&[(0.0, 0.5)][..]));

        let other_rev = SpectrumFrame {
            session_rev: 10,
            ..frame(t0, Some(vec![(0.0, 1.0)]))
        };
        let foreign = source(PlaybackPhase::Playing, &np, Some(&other_rev));
        assert_eq!(foreign.fresh_waveform(t0), None);

        let unadopted = DrawSource {
            adopted: Some(LoadRequestId::from_raw(5)),
            ..source(PlaybackPhase::Playing, &np, Some(&fresh))
        };
        assert_eq!(unadopted.fresh_waveform(t0), None);

        let bare = frame(t0, None);
        let sourceless = source(PlaybackPhase::Playing, &np, Some(&bare));
        assert_eq!(sourceless.fresh_waveform(t0), None);
    }
}

// ------------------------------------------- the scope's renderer

mod scope_renderer {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    use tenuto::playback::spectrum::worker::WAVEFORM_COLUMNS;
    use tenuto::tui::spectrum::draw_scope;
    use tenuto::tui::theme::Theme;

    /// Column `x` as one `(symbol, fg)` per row, top to bottom.
    fn column(buffer: &Buffer, x: u16) -> Vec<(&str, Color)> {
        (0..buffer.area.height)
            .map(|y| (buffer[(x, y)].symbol(), buffer[(x, y)].fg))
            .collect()
    }

    /// A spike renders green blocks above and below the midline at its
    /// column: width 48 resamples column `x` to the waveform's `2x`, so the
    /// spike at 48 lands on x 24; ten rows put the midline at 5, half at 5,
    /// so peak 0.4 reaches two rows up and min −0.6 three rows down. Every
    /// other column is silence: the muted midline and nothing else.
    #[test]
    fn a_spike_draws_green_blocks_about_the_midline_at_its_column() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 48, 10);
        let mut waveform = vec![(0.0_f32, 0.0_f32); WAVEFORM_COLUMNS];
        waveform[48] = (-0.6, 0.4);
        let mut buffer = Buffer::empty(area);
        draw_scope(&mut buffer, area, Some(&waveform), &theme);

        let blank = (" ", Color::Reset);
        let mut silent = vec![blank; 10];
        silent[5] = ("█", theme.muted);
        let mut spike = vec![blank; 10];
        for y in 3..=8 {
            spike[y as usize] = ("█", theme.green);
        }
        for x in 0..48 {
            let expected = match x {
                24 => &spike,
                _ => &silent,
            };
            assert_eq!(column(&buffer, x), *expected, "column {x}");
        }
    }

    /// §1.2: silence looks silent — an all-zero waveform is the flat muted
    /// midline and nothing else; an absent waveform falls flat the same way;
    /// at one row tall a silent column draws nothing at all.
    #[test]
    fn silence_draws_only_the_muted_midline_and_nothing_at_one_row() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 12, 6);
        let mut silent = vec![(" ", Color::Reset); 6];
        silent[3] = ("█", theme.muted);

        let mut buffer = Buffer::empty(area);
        draw_scope(
            &mut buffer,
            area,
            Some(&[(0.0_f32, 0.0_f32); WAVEFORM_COLUMNS][..]),
            &theme,
        );
        for x in 0..12 {
            assert_eq!(column(&buffer, x), silent, "column {x}");
        }

        let mut buffer = Buffer::empty(area);
        draw_scope(&mut buffer, area, None, &theme);
        for x in 0..12 {
            assert_eq!(column(&buffer, x), silent, "absent, column {x}");
        }

        let area = Rect::new(0, 0, 12, 1);
        let mut buffer = Buffer::empty(area);
        draw_scope(
            &mut buffer,
            area,
            Some(&[(0.0_f32, 0.0_f32); WAVEFORM_COLUMNS][..]),
            &theme,
        );
        assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
    }
}

// ------------------------------------------- the flow's per-column history

mod flow_history {
    use tenuto::tui::spectrum::{DECAY_PER_DRAW, FlowHistory, MAX_FLOW_DEPTH};

    /// §1.2: a width change rebuilds the history from scratch — the old
    /// columns are dropped, so nothing of the four-column past carries into
    /// the five-column present.
    #[test]
    fn a_width_change_rebuilds_from_scratch() {
        let mut history = FlowHistory::default();
        let first = [1.0_f32, 0.8, 0.6, 0.4];
        history.push(4, Some(&first));
        history.push(4, Some(&first));
        assert_eq!(history.column(0).len(), 2, "two pushes, newest last");

        let second = [0.9_f32, 0.7, 0.5, 0.3, 0.1];
        history.push(5, Some(&second));
        assert!(!history.is_empty());
        assert_eq!(history.column(0), [0.9], "one fresh entry, no carry-over");
        assert_eq!(history.column(4), [0.1], "the new width's last column");
        assert!(
            history.column(5).is_empty(),
            "the width owns the column count"
        );
    }

    /// Frames stop, the lava cools: every entry shrinks by
    /// [`DECAY_PER_DRAW`], not by a half.
    #[test]
    fn decay_multiplies_every_entry_by_the_draw_factor() {
        let mut history = FlowHistory::default();
        history.push(2, Some(&[1.0_f32; 2]));
        history.decay();
        let once = history.column(0)[0];
        assert!(
            (once - 0.85).abs() < 1e-6 && (once - DECAY_PER_DRAW).abs() < 1e-6,
            "× 0.85, not a half: {once}"
        );
        history.decay();
        assert!(
            (history.column(0)[0] - 0.85 * 0.85).abs() < 1e-6,
            "cools again: {:?}",
            history.column(0)
        );
    }

    /// The ring remembers [`MAX_FLOW_DEPTH`] entries per column, newest last.
    #[test]
    fn the_ring_is_capped_and_newest_last() {
        let mut history = FlowHistory::default();
        let last = (MAX_FLOW_DEPTH + 3) as f32;
        for entry in 1..=(MAX_FLOW_DEPTH + 3) as u16 {
            // A level per frame, in the unit interval the value rule clamps
            // to: the ramp marks the entries' order.
            history.push(1, Some(&[f32::from(entry) / (last + 1.0)]));
        }
        assert_eq!(history.column(0).len(), MAX_FLOW_DEPTH, "the cap holds");
        let first = history.column(0)[0];
        assert!(
            (first - 4.0 / (last + 1.0)).abs() < 1e-6,
            "the oldest survivors: {first}"
        );
        let newest = history.column(0)[MAX_FLOW_DEPTH - 1];
        assert!(
            (newest - last / (last + 1.0)).abs() < 1e-6,
            "newest last: {newest}"
        );
    }

    /// A push without levels advances nothing: time moves only when a frame
    /// (or `decay`) says so.
    #[test]
    fn a_push_without_levels_pushes_nothing() {
        let mut history = FlowHistory::default();
        history.push(3, Some(&[0.5_f32; 3]));
        history.push(3, None);
        history.push(3, Some(&[]));
        assert_eq!(history.column(0), [0.5], "absent levels are not a frame");
    }
}

// ---------------------------------------- the flow's view state

mod flow_display {
    use std::time::{Duration, Instant};

    use tenuto::application::transport::PlaybackPhase;
    use tenuto::application::view::NowPlaying;
    use tenuto::playback::command::LoadRequestId;
    use tenuto::playback::output::Nanos;
    use tenuto::playback::spectrum::worker::SpectrumFrame;
    use tenuto::tui::spectrum::{DrawSource, SpectrumDisplay, VisualizerMode};

    const TOKEN: LoadRequestId = LoadRequestId::from_raw(4);

    fn now_playing() -> NowPlaying {
        NowPlaying {
            session_rev: 9,
            load: Some(TOKEN),
            ..NowPlaying::unloaded()
        }
    }

    fn frame(published_at: Instant, level: f32) -> SpectrumFrame {
        SpectrumFrame {
            session_rev: 9,
            bands: vec![(40.0, 85.0)],
            levels: vec![level],
            waveform: None,
            at: Nanos(0),
            published_at,
        }
    }

    fn source<'a>(
        phase: PlaybackPhase,
        now_playing: &'a NowPlaying,
        frame: Option<&'a SpectrumFrame>,
    ) -> DrawSource<'a> {
        DrawSource {
            phase,
            now_playing: Some(now_playing),
            adopted: Some(TOKEN),
            frame,
        }
    }

    /// §1.2: fresh Flow frames push, newest last.
    #[test]
    fn fresh_flow_frames_push_newest_last() {
        let t0 = Instant::now();
        let np = now_playing();
        let mut display = SpectrumDisplay::default();
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t0, 0.5))),
            t0,
            VisualizerMode::Flow,
            1,
        );
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t0, 0.9))),
            t0,
            VisualizerMode::Flow,
            1,
        );
        assert_eq!(display.flow().column(0), [0.5, 0.9], "newest last");
    }

    /// §1.2: a mode switch starts a fresh history — the lava of a different
    /// visual is not this one's, and the reset happens at the switch itself.
    #[test]
    fn a_mode_switch_resets_the_history() {
        let t0 = Instant::now();
        let np = now_playing();
        let mut display = SpectrumDisplay::default();
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t0, 0.5))),
            t0,
            VisualizerMode::Flow,
            1,
        );
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t0, 0.9))),
            t0,
            VisualizerMode::Flow,
            1,
        );
        assert_eq!(display.flow().column(0), [0.5, 0.9]);

        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t0, 0.5))),
            t0,
            VisualizerMode::Bars,
            1,
        );
        assert!(
            display.flow().is_empty(),
            "the switch to bars wipes the history"
        );

        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t0, 0.2))),
            t0,
            VisualizerMode::Flow,
            1,
        );
        assert_eq!(
            display.flow().column(0),
            [0.2],
            "flow restarts from its own first frame"
        );
    }

    /// §1.2: a stale frame cools the history instead of pushing into it —
    /// the same rule the drawn levels follow.
    #[test]
    fn stale_frames_cool_the_history_instead_of_pushing() {
        let t0 = Instant::now();
        let np = now_playing();
        let fresh = frame(t0, 0.5);
        let mut display = SpectrumDisplay::default();
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&fresh)),
            t0,
            VisualizerMode::Flow,
            1,
        );
        let later = t0 + Duration::from_millis(150);
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&fresh)),
            later,
            VisualizerMode::Flow,
            1,
        );
        assert_eq!(
            display.flow().column(0).len(),
            1,
            "a stale frame pushes nothing"
        );
        let cooled = display.flow().column(0)[0];
        assert!(
            (cooled - 0.5 * 0.85).abs() < 1e-6,
            "it cools instead: {cooled}"
        );
    }
}

// ------------------------------------------- the flow's renderer

mod flow_renderer {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    use tenuto::tui::spectrum::{FlowHistory, MAX_FLOW_DEPTH, draw_flow};
    use tenuto::tui::theme::Theme;

    /// The test's own sRGB-byte lerp, for the gradient's midpoints; the
    /// endpoints are asserted against the theme directly.
    fn lerp(from: Color, to: Color, t: f32) -> Color {
        let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        match (from, to) {
            (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
                Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
            }
            _ => from,
        }
    }

    /// A rising ramp stacks the column green-side down and cream-side up:
    /// the oldest, coolest entry sits at the bottom row and the newest at
    /// the top, so the gradient runs green → amber → cream upward. The two
    /// anchor rows land exactly on `flow_color`'s endpoints.
    #[test]
    fn a_rising_ramp_stacks_green_at_the_bottom_and_cream_at_the_top() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 2, 4);
        let mut history = FlowHistory::default();
        for level in [0.25_f32, 0.5, 0.75, 1.0] {
            history.push(2, Some(&[level; 2]));
        }
        let mut buffer = Buffer::empty(area);
        draw_flow(&mut buffer, area, &history, None, &theme);
        for x in 0..2 {
            assert_eq!(buffer[(x, 3)].fg, lerp(theme.green, theme.amber, 0.5));
            assert_eq!(buffer[(x, 2)].fg, theme.amber, "the gradient's midpoint");
            assert_eq!(buffer[(x, 1)].fg, lerp(theme.amber, theme.cream, 0.5));
            assert_eq!(buffer[(x, 0)].fg, theme.cream, "the top endpoint");
            assert_eq!(buffer[(x, 0)].symbol(), "█");
        }
    }

    /// A full ring resampled into three rows lands by nearest entry: the
    /// bottom row takes the oldest entry, the top row the newest, and the
    /// middle row the halfway entry — index 7 of a 16-deep ring, whose
    /// value `8/16` sits exactly on `flow_color`'s amber midpoint, which
    /// pins the arithmetic and not merely "some middle value".
    #[test]
    fn sixteen_entries_resample_into_three_rows_by_nearest_entry() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 1, 3);
        let mut history = FlowHistory::default();
        let last = MAX_FLOW_DEPTH as f32;
        for entry in 1..=MAX_FLOW_DEPTH as u16 {
            // A level per frame, in the unit interval the value rule clamps
            // to: the ramp marks the entries' order.
            history.push(1, Some(&[f32::from(entry) / last]));
        }
        let mut buffer = Buffer::empty(area);
        draw_flow(&mut buffer, area, &history, None, &theme);

        assert_eq!(
            buffer[(0, 2)].fg,
            lerp(theme.green, theme.amber, 0.125),
            "the oldest entry, 1/16, at the bottom"
        );
        assert_eq!(
            buffer[(0, 1)].fg,
            theme.amber,
            "the halfway entry, 8/16, in the middle"
        );
        assert_eq!(
            buffer[(0, 0)].fg,
            theme.cream,
            "the newest entry, 1.0, on top"
        );
        for y in 0..3 {
            assert_eq!(buffer[(0, y)].symbol(), "█", "row {y}");
        }
    }

    /// §1.2: silence stays silent — an all-zero history draws nothing, and
    /// an empty history with silent levels neither.
    #[test]
    fn an_all_zero_history_draws_nothing() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 3, 4);
        let mut history = FlowHistory::default();
        history.push(3, Some(&[0.0_f32; 3]));
        history.push(3, Some(&[0.0_f32; 3]));
        let mut buffer = Buffer::empty(area);
        draw_flow(&mut buffer, area, &history, Some(&[0.0_f32; 3]), &theme);
        assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));

        let mut buffer = Buffer::empty(area);
        draw_flow(&mut buffer, area, &history, None, &theme);
        assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
    }

    /// Without a history the live frame is the whole column: the first
    /// fresh frame shows immediately, the scope's absent-fallback shape.
    #[test]
    fn an_empty_history_falls_back_to_the_live_levels() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 3, 2);
        let mut buffer = Buffer::empty(area);
        draw_flow(
            &mut buffer,
            area,
            &FlowHistory::default(),
            Some(&[0.5_f32; 3]),
            &theme,
        );
        for x in 0..3 {
            for y in 0..2 {
                assert_eq!(buffer[(x, y)].symbol(), "█", "column {x}, row {y}");
                assert_eq!(buffer[(x, y)].fg, theme.amber);
            }
        }

        let mut buffer = Buffer::empty(area);
        draw_flow(&mut buffer, area, &FlowHistory::default(), None, &theme);
        assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
    }
}

// ------------------------------- the v key, the skip rule and the ui state

mod v_key {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use tenuto::application::runtime::AppCommand;
    use tenuto::tui::input::{Effect, handle_key};
    use tenuto::tui::spectrum::VisualizerMode;
    use tenuto::tui::state::UiState;
    use tenuto::tui::theme::{Theme, ThemeId};

    use crate::views;

    fn plain(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
    }

    /// The spelling the one `SetVisualizer` effect carried — `None` when the
    /// cycle landed back on the default, which is what clears the field.
    fn dispatched(effects: &[Effect]) -> Option<String> {
        match effects {
            [Effect::App(AppCommand::SetVisualizer(spelling))] => spelling.clone(),
            other => panic!("expected one SetVisualizer effect, got {other:?}"),
        }
    }

    /// M14 §1.2: `v` on a panel with rows cycles bars → scope → flow →
    /// bars, mutating the state and dispatching each new spelling — the
    /// bars leg as `None`, the shape that clears the stored field.
    #[test]
    fn v_cycles_the_modes_and_dispatches_each_spelling() {
        let view = views::sample_view();
        let mut ui = UiState::new(true);
        ui.spectrum_rows = 8;

        let effects = handle_key(plain('v'), &mut ui, &view);
        assert_eq!(ui.visualizer, VisualizerMode::Scope);
        assert_eq!(dispatched(&effects).as_deref(), Some("scope"));

        let effects = handle_key(plain('v'), &mut ui, &view);
        assert_eq!(ui.visualizer, VisualizerMode::Flow);
        assert_eq!(dispatched(&effects).as_deref(), Some("flow"));

        let effects = handle_key(plain('v'), &mut ui, &view);
        assert_eq!(ui.visualizer, VisualizerMode::Bars);
        assert_eq!(dispatched(&effects), None, "bars stores as absent");
    }

    /// Spec §1.2: flow needs more than one row to mean anything, so a
    /// one-row panel cycles bars → scope → bars — flow skipped.
    #[test]
    fn a_one_row_panel_skips_flow() {
        let view = views::sample_view();
        let mut ui = UiState::new(true);
        ui.spectrum_rows = 1;
        handle_key(plain('v'), &mut ui, &view);
        assert_eq!(ui.visualizer, VisualizerMode::Scope);

        let effects = handle_key(plain('v'), &mut ui, &view);
        assert_eq!(ui.visualizer, VisualizerMode::Bars, "flow skipped");
        assert_eq!(dispatched(&effects), None);
    }

    /// A fresh state is the default palette on bars with no panel seen yet;
    /// `with_appearance` replaces the theme and the mode — the startup's
    /// one resolution — and leaves the panel height alone.
    #[test]
    fn with_appearance_sets_the_startup_theme_and_mode() {
        let ui = UiState::new(true);
        assert_eq!(ui.theme, Theme::default());
        assert_eq!(ui.visualizer, VisualizerMode::Bars);
        assert_eq!(ui.spectrum_rows, 0, "no panel seen yet");

        let ui = UiState::new(true).with_appearance(
            ThemeId::Winamp.theme(),
            VisualizerMode::from_stored("flow").expect("flow parses"),
        );
        assert_eq!(ui.theme, ThemeId::Winamp.theme());
        assert_eq!(ui.visualizer, VisualizerMode::Flow);
        assert_eq!(ui.spectrum_rows, 0);
    }
}

// --------------------------------------- a restored mode renders at startup

mod restored_mode_render {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    use tenuto::application::transport::PlaybackPhase;
    use tenuto::tui::render::{Visuals, draw};
    use tenuto::tui::spectrum::{FlowHistory, VisualizerMode};
    use tenuto::tui::state::UiState;
    use tenuto::tui::theme::Theme;

    use crate::views::{self, decoded, ids, playing};

    fn draw_at(ui: &UiState, visuals: &Visuals<'_>, width: u16, height: u16) -> Buffer {
        let view = views::view(
            PlaybackPhase::Playing,
            Some(playing(ids()[0], true, Some(decoded(185)), false)),
        );
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("backend");
        terminal
            .draw(|frame| {
                draw(frame, &view, ui, visuals);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// A rising ramp history, pushed at the panel's own width so every
    /// column owns a ring.
    fn ramp(width: u16) -> FlowHistory {
        let mut history = FlowHistory::default();
        for level in [0.25_f32, 0.5, 0.75, 1.0] {
            history.push(width, Some(&vec![level; usize::from(width)]));
        }
        history
    }

    /// The test's own sRGB-byte lerp, for the gradient's midpoints; the
    /// endpoints are asserted against the theme directly.
    fn lerp(from: Color, to: Color, t: f32) -> Color {
        let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        match (from, to) {
            (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
                Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
            }
            _ => from,
        }
    }

    /// A persisted `flow` mode, restored at startup through
    /// `with_appearance`, renders the lava through `draw`: 100×30 is the
    /// normal tier with three information lines, so the panel is
    /// Rect::new(25, 6, 71, 6), the newest frame's value on the top row in
    /// cream and the oldest at the bottom at the green→amber midpoint.
    /// Under the pre-M14 hard-coded bars the top row would be an amber bar
    /// instead, so the cream pins the mode to the UI's.
    #[test]
    fn a_restored_flow_mode_renders_flow_through_draw() {
        let theme = Theme::default();
        let visuals = Visuals {
            flow: Some(&ramp(71)),
            ..Visuals::default()
        };
        let ui = UiState::new(true).with_appearance(theme, VisualizerMode::Flow);
        let buffer = draw_at(&ui, &visuals, 100, 30);
        assert_eq!(buffer[(25, 6)].symbol(), "█", "the panel's top row draws");
        assert_eq!(
            buffer[(25, 6)].fg,
            theme.cream,
            "the newest entry, 1.0, on top"
        );
        assert_eq!(
            buffer[(25, 11)].fg,
            lerp(theme.green, theme.amber, 0.5),
            "the oldest entry, 0.25, at the bottom"
        );
    }

    /// The same restored mode at a one-row panel (100×20, the short tier's
    /// Rect::new(12, 3, 85, 1)): every mode renders the bars row there
    /// (spec §1.2) — a loud bar in amber, which neither the flow gradient
    /// nor the scope's midline would paint.
    #[test]
    fn a_restored_flow_mode_at_one_row_renders_bars() {
        let theme = Theme::default();
        let visuals = Visuals {
            spectrum: Some(&[1.0_f32; 85]),
            flow: Some(&ramp(85)),
            ..Visuals::default()
        };
        let ui = UiState::new(true).with_appearance(theme, VisualizerMode::Flow);
        let buffer = draw_at(&ui, &visuals, 100, 20);
        assert_eq!(buffer[(12, 3)].symbol(), "█");
        assert_eq!(
            buffer[(12, 3)].fg,
            theme.amber,
            "the bars row, not the flow gradient"
        );
    }
}

// ------------------------------------------ startup resolution precedence

mod startup_resolution {
    use clap::Parser;
    use tenuto::cli::{Cli, CliCommand};
    use tenuto::persistence::model::PersistedState;
    use tenuto::tui::resolve_visualizer;
    use tenuto::tui::spectrum::VisualizerMode;
    use tenuto::tui::theme::{Theme, ThemeId};

    use crate::runtime::rig_with;

    /// M14 §1.3: the `--visualizer` flag is run-scoped — it beats the saved
    /// mode, the saved mode beats the default.
    #[test]
    fn the_flag_beats_the_saved_mode_and_the_saved_beats_the_default() {
        assert_eq!(
            resolve_visualizer(Some("flow"), Some("scope")),
            VisualizerMode::Flow,
            "the run-scoped flag wins"
        );
        assert_eq!(
            resolve_visualizer(None, Some("scope")),
            VisualizerMode::Scope,
            "the saved mode beats the default"
        );
        assert_eq!(
            resolve_visualizer(None, None),
            VisualizerMode::Bars,
            "nothing saved is the default"
        );
    }

    /// A hand-edited state file never errors: an unknown saved spelling is
    /// the default (plan risk 1), and a valid flag still beats it.
    #[test]
    fn a_saved_unknown_spelling_loses_to_the_default() {
        assert_eq!(
            resolve_visualizer(None, Some("plasma")),
            VisualizerMode::Bars
        );
        assert_eq!(
            resolve_visualizer(Some("scope"), Some("plasma")),
            VisualizerMode::Scope
        );
    }

    /// Spec §2.1/§7: an explicit `--theme` writes through — the startup
    /// dispatches the flag's value as `SetTheme`, so it lands in state and
    /// becomes later runs' default — and the startup resolution reads the
    /// saved spelling back into its palette, defaulting what it cannot
    /// read.
    #[test]
    fn the_theme_flag_writes_through_and_startup_resolution_reads_it_back() {
        let parsed =
            Cli::try_parse_from(["tenuto", "tui", "--theme", "winamp"]).expect("winamp parses");
        let Some(CliCommand::Tui { theme, .. }) = parsed.command else {
            panic!("`tenuto tui --theme winamp` parses to the tui subcommand");
        };

        let mut rig = rig_with(PersistedState::default());
        assert_eq!(rig.runtime.session().state().theme(), None);
        if let Some(theme) = &theme {
            rig.runtime
                .handle(tenuto::application::runtime::AppCommand::SetTheme(Some(
                    theme.clone(),
                )));
        }
        assert_eq!(
            rig.runtime.session().state().theme(),
            Some("winamp"),
            "the flag lands in state"
        );
        let saved = rig.runtime.session().state().theme().unwrap_or("");
        assert_eq!(
            ThemeId::from_stored(saved).unwrap_or_default().theme(),
            ThemeId::Winamp.theme(),
            "the saved spelling resolves to its palette"
        );
        assert_eq!(
            ThemeId::from_stored("").unwrap_or_default().theme(),
            Theme::default(),
            "nothing saved is the default palette"
        );
    }
}
