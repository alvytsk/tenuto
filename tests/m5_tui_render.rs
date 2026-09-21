#[path = "support/views.rs"]
mod views;

use std::cell::Cell;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use tenuto::application::transport::PlaybackPhase;
use tenuto::application::view::{PersistenceStatus, PlayerView, SavedHistory};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::queue::{DisplayDuration, DurationSource};
use tenuto::tui::layout::{Regions, Tier, regions, tier_for};
use tenuto::tui::render::{CoverView, CoverWidget, HitMap, TransportButton, Visuals, draw};
use tenuto::tui::state::UiState;
use tenuto::tui::theme::Theme;
use views::{decoded, ids, playing, view};

fn render(
    view: &PlayerView,
    ui: &UiState,
    visuals: &Visuals<'_>,
    w: u16,
    h: u16,
) -> (Buffer, HitMap) {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("backend");
    let mut hits = HitMap::default();
    terminal
        .draw(|frame| {
            hits = draw(frame, view, ui, visuals);
        })
        .expect("draw");
    (terminal.backend().buffer().clone(), hits)
}

fn screen(buffer: &Buffer) -> String {
    let width = usize::from(buffer.area.width.max(1));
    buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn text(view: &PlayerView, ui: &UiState, w: u16, h: u16) -> String {
    screen(&render(view, ui, &Visuals::default(), w, h).0)
}

/// The bracketed progress row of a normal-tier screen.
fn progress_row(screen: &str) -> Option<&str> {
    screen
        .lines()
        .find(|line| line.trim_start().starts_with("│ ["))
}

fn within(rect: Rect, area: Rect) -> bool {
    rect.x >= area.x
        && rect.y >= area.y
        && rect.right() <= area.right()
        && rect.bottom() <= area.bottom()
}

fn all_rects(regions: &Regions) -> Vec<Rect> {
    let mut rects = vec![
        regions.status,
        regions.player,
        regions.info,
        regions.transport,
        regions.progress,
        regions.queue,
        regions.footer,
    ];
    rects.extend(regions.cover);
    rects.extend(regions.spectrum);
    rects
}

#[test]
fn tiers_follow_the_smallest_matching_dimension() {
    assert_eq!(tier_for(100, 22), Tier::Compact);
    assert_eq!(tier_for(60, 40), Tier::Compact);
    assert_eq!(tier_for(100, 21), Tier::Short);
    assert_eq!(tier_for(60, 18), Tier::Short);
    assert_eq!(tier_for(80, 28), Tier::Normal);
    assert_eq!(tier_for(49, 40), Tier::Minimal);
    assert_eq!(tier_for(120, 17), Tier::Minimal);
    assert_eq!(tier_for(29, 40), Tier::Resize);
    assert_eq!(tier_for(100, 7), Tier::Resize);
}

#[test]
fn regions_are_valid_for_zero_and_tiny_areas() {
    let areas = [
        Rect::new(0, 0, 0, 0),
        Rect::new(0, 0, 1, 1),
        Rect::new(7, 3, 0, 0),
        Rect::new(7, 3, 1, 1),
        Rect::new(7, 3, 2, 3),
        Rect::new(7, 3, 20, 5),
        Rect::new(7, 3, 45, 16),
        Rect::new(7, 3, 100, 20),
        Rect::new(7, 3, 100, 24),
        Rect::new(7, 3, 100, 30),
        Rect::new(u16::MAX - 2, u16::MAX - 2, 2, 2),
    ];
    for tier in [
        Tier::Resize,
        Tier::Minimal,
        Tier::Short,
        Tier::Compact,
        Tier::Normal,
    ] {
        for area in areas {
            let regions = regions(area, tier, 3);
            for rect in all_rects(&regions) {
                assert!(within(rect, area), "{tier:?} {area:?}: {rect:?} escapes");
            }
        }
    }
    let ui = UiState::new(true);
    let _ = text(&view(PlaybackPhase::Unloaded, None), &ui, 1, 1);
    let _ = text(&view(PlaybackPhase::Unloaded, None), &ui, 0, 0);
}

#[test]
fn normal_layout_shows_cover_metadata_and_distinct_playing_and_selected_rows() {
    let mut now = playing(ids()[0], true, Some(decoded(185)), false);
    now.year = Some("1998".into());
    let v = view(PlaybackPhase::Playing, Some(now));
    let mut ui = UiState::new(true);
    ui.selected = Some(v.rows[2].id);
    let screen = text(&v, &ui, 100, 30);
    assert!(
        screen.contains("Harbor"),
        "normal shows secondary metadata\n{screen}"
    );
    assert!(screen.contains("Coast · 1998"), "album and year\n{screen}");
    assert!(screen.contains("▶ 01:02 / 03:05"), "{screen}");
    assert!(
        screen.contains('░') && screen.contains('♪'),
        "cover placeholder\n{screen}"
    );
    let playing_line = screen
        .lines()
        .find(|l| l.contains("Morning Tide") && l.contains('▶'))
        .expect("playing marker");
    assert!(!playing_line.contains("Done"));
    assert!(
        screen.contains("(01:00:00)"),
        "declared duration is parenthesized\n{screen}"
    );
    assert!(screen.contains("~01:02 saved") && screen.contains("played"));
}

#[test]
fn the_player_leaves_the_terminal_background_unpainted() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let (buffer, _) = render(&v, &UiState::new(true), &Visuals::default(), 100, 30);
    let corners = [(0, 0), (99, 0), (0, 29), (99, 29)];
    for (x, y) in corners {
        assert_eq!(
            buffer.cell((x, y)).expect("cell").bg,
            ratatui::style::Color::Reset
        );
    }
}

#[test]
fn the_selected_row_is_highlighted_apart_from_the_playing_marker() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let mut ui = UiState::new(true);
    ui.selected = Some(v.rows[2].id);
    let (buffer, hits) = render(&v, &ui, &Visuals::default(), 100, 30);
    let theme = Theme::default();
    let row = |index: usize| {
        hits.rows
            .iter()
            .find(|(_, id)| *id == v.rows[index].id)
            .map(|(rect, _)| *rect)
            .expect("visible row")
    };
    let (selected, active) = (row(2), row(0));
    let cell = |x: u16, y: u16| buffer.cell((x, y)).expect("cell").clone();
    assert_eq!(cell(selected.x + 4, selected.y).bg, theme.amber);
    assert_ne!(cell(active.x + 4, active.y).bg, theme.amber);
    assert!((active.x..active.right()).any(|x| cell(x, active.y).symbol() == "▶"));
    assert!(!(selected.x..selected.right()).any(|x| cell(x, selected.y).symbol() == "▶"));
}

#[test]
fn the_hit_map_covers_visible_rows_the_progress_bar_and_transport() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let area = Rect::new(0, 0, 100, 30);
    let (_, hits) = render(&v, &UiState::new(true), &Visuals::default(), 100, 30);
    let listed: Vec<_> = hits.rows.iter().map(|(_, id)| *id).collect();
    assert_eq!(listed, v.rows.iter().map(|row| row.id).collect::<Vec<_>>());
    for (rect, _) in &hits.rows {
        assert!(within(*rect, hits.queue) && !rect.is_empty());
    }
    assert!(!hits.progress.is_empty() && within(hits.progress, area));
    let buttons: Vec<_> = hits.buttons.iter().map(|(_, b)| *b).collect();
    assert_eq!(
        buttons,
        [
            TransportButton::Previous,
            TransportButton::SeekBack,
            TransportButton::PlayPause,
            TransportButton::Stop,
            TransportButton::SeekForward,
            TransportButton::Next,
            TransportButton::Shuffle
        ]
    );
    for pair in hits.buttons.windows(2) {
        assert!(!pair[0].0.intersects(pair[1].0));
    }
}

#[test]
fn a_title_alone_gets_the_full_width_and_leaves_its_rows_to_the_spectrum() {
    let title = "#459 – DeepSeek, China, OpenAI, NVIDIA, xAI, TSMC, Stargate";
    let mut now = playing(ids()[0], true, Some(decoded(185)), false);
    now.title = title.into();
    let full = text(
        &view(PlaybackPhase::Playing, Some(now.clone())),
        &UiState::new(true),
        100,
        30,
    );
    assert!(full.contains(title), "{full}");
    (now.artist, now.album, now.year) = (None, None, None);
    let alone = text(
        &view(PlaybackPhase::Playing, Some(now)),
        &UiState::new(true),
        100,
        30,
    );
    assert!(alone.contains(title), "{alone}");
    // The rows the artist and album gave up go to the spectrum above the time.
    let time_row = |screen: &str| screen.lines().position(|row| row.contains("▶ 01:02"));
    assert_eq!(time_row(&full), time_row(&alone));
}

#[test]
fn compact_keeps_the_artist_and_short_drops_secondary_metadata() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, None, false)),
    );
    for (w, h) in [(100, 24), (60, 40)] {
        let screen = text(&v, &UiState::new(true), w, h);
        assert!(screen.contains("Morning Tide"), "{w}x{h}\n{screen}");
        assert!(screen.contains("Harbor"), "{w}x{h}\n{screen}");
        assert!(!screen.contains("Coast"), "{w}x{h}\n{screen}");
    }
    let short = text(&v, &UiState::new(true), 100, 20);
    assert!(short.contains("Morning Tide"), "{short}");
    assert!(
        !short.contains("Harbor") && !short.contains("Coast"),
        "{short}"
    );
}

#[test]
fn compact_puts_the_time_over_a_bracketed_bar_and_a_slider_where_it_fits() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let wide = text(&v, &UiState::new(true), 100, 24);
    let rows: Vec<&str> = wide.lines().collect();
    let time = rows
        .iter()
        .position(|row| row.contains("▶ "))
        .expect("time row");
    assert!(
        rows[time + 1].contains('[') && rows[time + 1].contains(']'),
        "{wide}"
    );
    assert!(rows[time + 2].contains("VOL ━━━━━━━━──   80%"), "{wide}");
    assert!(!wide.contains("vol 80%"), "{wide}");
    // Too narrow for the slider beside the buttons: the border keeps it.
    let narrow = text(&v, &UiState::new(true), 56, 24);
    assert!(
        narrow.contains("vol 80%") && !narrow.contains("VOL"),
        "{narrow}"
    );
    assert!(narrow.contains("playing"), "{narrow}");
}

#[test]
fn minimal_hides_cover_and_spectrum_but_keeps_title_state_and_queue() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, None, false)),
    );
    let screen = text(&v, &UiState::new(true), 45, 16);
    assert!(
        screen.contains("Morning Tide") && screen.contains("playing") && screen.contains("Done"),
        "{screen}"
    );
    assert!(!screen.contains('░'));
    assert!(!screen.contains('▁'), "no spectrum in minimal\n{screen}");
}

#[test]
fn the_resize_tier_keeps_quit_and_play_hints() {
    let screen = text(
        &view(PlaybackPhase::Unloaded, None),
        &UiState::new(true),
        25,
        6,
    );
    assert!(
        screen.contains("q quit") && screen.contains("space play"),
        "{screen}"
    );
    let wide = text(
        &view(PlaybackPhase::Unloaded, None),
        &UiState::new(true),
        29,
        40,
    );
    assert!(wide.contains("Terminal too small") && wide.contains("space play · q quit"));
}

#[test]
fn unknown_duration_and_estimated_position_are_honest() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, None, true)),
    );
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("~01:02 / --:--"), "{screen}");
    assert!(
        !progress_row(&screen).expect("progress row").contains('━'),
        "an unknown duration fills nothing\n{screen}"
    );
}

#[test]
fn a_declared_duration_is_parenthesized_and_never_fills_the_bar() {
    let declared = DisplayDuration {
        value: Duration::from_secs(120),
        source: DurationSource::Declared,
    };
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(declared), false)),
    );
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("01:02 / (02:00)"), "{screen}");
    assert!(
        !progress_row(&screen).expect("progress row").contains('━'),
        "{screen}"
    );

    let decoded = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(120)), false)),
    );
    let screen = text(&decoded, &UiState::new(true), 100, 30);
    assert!(
        progress_row(&screen).expect("progress row").contains('━'),
        "{screen}"
    );
}

#[test]
fn an_estimated_duration_is_marked_like_an_estimated_position() {
    let estimated = DisplayDuration {
        value: Duration::from_secs(120),
        source: DurationSource::Decoded(PositionProvenance::Estimated),
    };
    let mut v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(estimated), false)),
    );
    v.rows[2].duration = Some(estimated);
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("01:02 / ~02:00"), "{screen}");
    let done = screen
        .lines()
        .find(|line| line.contains("Done"))
        .expect("the third row");
    assert!(done.contains("~02:00"), "{done}");
    let established = screen
        .lines()
        .find(|line| line.contains("Morning Tide") && line.contains("03:05"))
        .expect("the first row");
    assert!(!established.contains("~03:05"), "{established}");
}

#[test]
fn a_live_entry_shows_live_and_listening_time_with_no_bar_or_duration() {
    let mut now = playing(ids()[0], true, None, false);
    now.position = Duration::from_secs(754);
    let mut v = view(PlaybackPhase::Playing, Some(now));
    v.live = true;
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("LIVE"), "{screen}");
    assert!(screen.contains("12:34"), "{screen}");
    assert!(
        !screen.contains(" / "),
        "a live entry has no total: {screen}"
    );
    // `progress_row` finds the bar row by its opening bracket, the very thing
    // a live row must not have. So the bar row is checked directly, by its
    // region, rather than through that helper.
    let bar_row_y = regions(Rect::new(0, 0, 100, 30), Tier::Normal, 3)
        .progress
        .y;
    let bar_line = screen
        .lines()
        .nth(usize::from(bar_row_y))
        .unwrap_or_default();
    assert!(
        !bar_line.contains('['),
        "a live entry draws no progress bar: {screen}"
    );
    // Scoped to the bar row alone, not the whole screen: the transport row's
    // volume slider legitimately draws '━' regardless of live status.
    assert!(
        !bar_line.contains('━'),
        "a live entry's bar is skipped, not zero-filled: {screen}"
    );
}

#[test]
fn a_reconnecting_station_says_so() {
    let mut now = playing(ids()[0], true, None, false);
    now.position = Duration::from_secs(5);
    let mut v = view(PlaybackPhase::Reconnecting, Some(now));
    v.live = true;
    v.reconnecting = true;
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("reconnecting…"), "{screen}");
}

#[test]
fn a_saved_but_unloaded_entry_shows_saved_history_not_live_progress() {
    let mut now = playing(ids()[0], false, None, false);
    now.saved = Some(SavedHistory::Position {
        at: Duration::from_secs(123),
        estimated: false,
    });
    let v = view(PlaybackPhase::Unloaded, Some(now));
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("02:03 saved"), "{screen}");
    assert!(!screen.contains("01:02 / "));
}

#[test]
fn buffering_and_unsaved_are_labelled() {
    let mut now = playing(ids()[0], true, Some(decoded(185)), false);
    now.buffering = true;
    let mut v = view(PlaybackPhase::Playing, Some(now));
    v.persistence = PersistenceStatus::Unsaved;
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("playing buffering"), "{screen}");
    let status = screen.lines().nth(1).expect("status row");
    assert!(
        status.contains("TENUTO") && status.contains("mouse on") && status.contains("unsaved"),
        "{status}"
    );
    assert!(screen.contains("VOL ━━━━━━━━──   80%"), "{screen}");
    let compact = text(&v, &UiState::new(true), 70, 20);
    assert!(compact.contains("vol 80%"), "{compact}");
}

#[test]
fn failing_persistence_is_labelled_apart_from_unsaved() {
    let mut v = view(PlaybackPhase::Unloaded, None);
    v.persistence = PersistenceStatus::Failing;
    let screen = text(&v, &UiState::new(true), 100, 30);
    let status = screen.lines().nth(1).expect("status row");
    assert!(status.contains("not saving"), "{status}");
    assert!(!status.contains("unsaved"), "{status}");

    v.persistence = PersistenceStatus::Saving;
    let screen = text(&v, &UiState::new(true), 100, 30);
    let status = screen.lines().nth(1).expect("status row");
    assert!(
        !status.contains("saving") && !status.contains("unsaved"),
        "{status}"
    );
}

#[test]
fn empty_loading_and_failed_screens() {
    let mut empty = view(PlaybackPhase::Unloaded, None);
    empty.rows.clear();
    let screen = text(&empty, &UiState::new(true), 100, 30);
    assert!(
        screen.contains("Queue is empty — press b to browse or a to add"),
        "{screen}"
    );
    assert!(
        screen.contains("Space Play/Pause") && screen.contains("q Quit"),
        "{screen}"
    );
    let loading = view(PlaybackPhase::Loading, None);
    assert!(text(&loading, &UiState::new(true), 100, 30).contains("loading"));
    let mut failed = view(PlaybackPhase::LoadFailed, None);
    failed.status = Some("cannot open media \"/x.flac\"".into());
    assert!(text(&failed, &UiState::new(false), 100, 30).contains("cannot open media"));
    assert!(text(&failed, &UiState::new(false), 100, 30).contains("mouse off"));
}

struct Probe(Cell<Option<Rect>>);

impl CoverWidget for Probe {
    fn render_cover(&self, area: Rect, _buffer: &mut Buffer) {
        self.0.set(Some(area));
    }
}

#[test]
fn a_prepared_cover_replaces_the_placeholder_in_the_cover_region() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let probe = Probe(Cell::new(None));
    let visuals = Visuals {
        cover: CoverView::Image(&probe),
        ..Default::default()
    };
    let (buffer, _) = render(&v, &UiState::new(true), &visuals, 100, 30);
    let expected = regions(Rect::new(0, 0, 100, 30), Tier::Normal, 3).cover;
    assert_eq!(probe.0.get(), expected);
    assert_eq!(expected.map(|r| (r.width, r.height)), Some((20, 10)));
    assert!(!screen(&buffer).contains('░'));
}

#[test]
fn selection_reconciles_to_the_hint_then_a_surviving_row_then_the_first() {
    let mut v = view(PlaybackPhase::Unloaded, None);
    let mut ui = UiState::new(true);
    ui.reconcile(&v, None);
    assert_eq!(ui.selected, Some(v.rows[0].id));
    ui.reconcile(&v, Some(v.rows[2].id));
    assert_eq!(ui.selected, Some(v.rows[2].id));
    ui.reconcile(&v, None);
    assert_eq!(
        ui.selected,
        Some(v.rows[2].id),
        "a surviving selection stays"
    );
    v.rows.remove(2);
    ui.queue_offset = 9;
    ui.reconcile(&v, None);
    assert_eq!(ui.selected, Some(v.rows[0].id));
    assert!(ui.queue_offset < v.rows.len());
    v.rows.clear();
    ui.reconcile(&v, Some(ids()[1]));
    assert_eq!((ui.selected, ui.queue_offset), (None, 0));
}

#[test]
fn the_spectrum_row_draws_levels_and_nothing_in_minimal() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let levels = [1.0_f32; 12];
    let visuals = Visuals {
        spectrum: Some(&levels),
        ..Default::default()
    };
    let (normal, _) = render(&v, &UiState::new(true), &visuals, 100, 30);
    assert!(screen(&normal).contains('█'), "{}", screen(&normal));
    let (minimal, _) = render(&v, &UiState::new(true), &visuals, 45, 16);
    assert!(!screen(&minimal).contains('█'), "{}", screen(&minimal));
}

// ----------------------------------------- what the spectrum row is fed

mod spectrum_display {
    use std::time::{Duration, Instant};

    use ratatui::layout::Rect;
    use tenuto::application::transport::PlaybackPhase;
    use tenuto::application::view::NowPlaying;
    use tenuto::playback::command::LoadRequestId;
    use tenuto::playback::output::Nanos;
    use tenuto::playback::spectrum::worker::SpectrumFrame;
    use tenuto::tui::layout::{Tier, regions};
    use tenuto::tui::spectrum::{DrawSource, SpectrumDisplay, wants_analysis};

    use super::views::{ids, playing};

    const TOKEN: LoadRequestId = LoadRequestId::from_raw(4);

    fn now_playing() -> NowPlaying {
        NowPlaying {
            session_rev: 9,
            load: Some(TOKEN),
            ..playing(ids()[0], true, None, false)
        }
    }

    fn frame(published_at: Instant, level: f32) -> SpectrumFrame {
        SpectrumFrame {
            session_rev: 9,
            bands: vec![(40.0, 85.0), (85.0, 170.0)],
            levels: vec![level, level],
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

    fn levels(display: &SpectrumDisplay) -> Vec<f32> {
        display.levels().map(<[f32]>::to_vec).unwrap_or_default()
    }

    fn decayed(display: &SpectrumDisplay, from: f32) -> bool {
        levels(display)
            .iter()
            .all(|level| (level - from * 0.85).abs() < 1e-6)
    }

    #[test]
    fn a_fresh_matching_frame_is_drawn() {
        let t0 = Instant::now();
        let np = now_playing();
        let fresh = frame(t0, 0.6);
        let mut display = SpectrumDisplay::default();
        display.update(&source(PlaybackPhase::Playing, &np, Some(&fresh)), t0);
        assert_eq!(levels(&display), vec![0.6, 0.6]);
    }

    #[test]
    fn pause_without_transport_retirement_decays() {
        let t0 = Instant::now();
        let np = now_playing();
        let fresh = frame(t0, 0.8);
        let mut display = SpectrumDisplay::default();
        display.update(&source(PlaybackPhase::Playing, &np, Some(&fresh)), t0);
        // Paused: the mapping and the frame are still there and still fresh.
        display.update(
            &source(PlaybackPhase::Paused, &np, Some(&fresh)),
            t0 + Duration::from_millis(10),
        );
        assert!(decayed(&display, 0.8), "{:?}", levels(&display));
        // And past 150 ms, still paused, it keeps falling.
        for step in 0..40 {
            display.update(
                &source(PlaybackPhase::Paused, &np, Some(&fresh)),
                t0 + Duration::from_millis(200 + step),
            );
        }
        assert!(levels(&display).iter().all(|level| *level < 0.01));
    }

    #[test]
    fn starvation_without_new_pcm_expires_the_latest_frame() {
        let t0 = Instant::now();
        let np = now_playing();
        let stale = frame(t0, 0.8);
        let mut display = SpectrumDisplay::default();
        display.update(&source(PlaybackPhase::Playing, &np, Some(&stale)), t0);
        // Still Playing, same revision and token, same frame: only the clock
        // moved. A delayed worker cleanup must not freeze the display.
        let later = t0 + Duration::from_millis(150);
        display.update(&source(PlaybackPhase::Playing, &np, Some(&stale)), later);
        assert!(decayed(&display, 0.8), "{:?}", levels(&display));
        // A new window resumes the display.
        let resumed = frame(later, 0.5);
        display.update(&source(PlaybackPhase::Playing, &np, Some(&resumed)), later);
        assert_eq!(levels(&display), vec![0.5, 0.5]);
    }

    #[test]
    fn a_peak_holds_then_falls_slower_than_its_bar() {
        let t0 = Instant::now();
        let np = now_playing();
        let mut display = SpectrumDisplay::default();
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t0, 0.8))),
            t0,
        );
        assert_eq!(display.peaks(), Some(&[0.8_f32, 0.8][..]));

        // The bar drops at once; the peak stays where the bar reached.
        let t1 = t0 + Duration::from_millis(100);
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t1, 0.2))),
            t1,
        );
        assert_eq!(levels(&display), vec![0.2, 0.2]);
        assert_eq!(display.peaks(), Some(&[0.8_f32, 0.8][..]));

        // Past the hold it falls, still well above the bar.
        let t2 = t0 + Duration::from_millis(800);
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t2, 0.2))),
            t2,
        );
        let peak = display.peaks().unwrap()[0];
        assert!(peak < 0.8 && peak > 0.2, "{peak}");

        // It never sinks below its bar, and a louder bar lifts it again.
        let t3 = t0 + Duration::from_secs(10);
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t3, 0.2))),
            t3,
        );
        assert_eq!(display.peaks(), Some(&[0.2_f32, 0.2][..]));
        display.update(
            &source(PlaybackPhase::Playing, &np, Some(&frame(t3, 0.9))),
            t3,
        );
        assert_eq!(display.peaks(), Some(&[0.9_f32, 0.9][..]));
    }

    #[test]
    fn a_frame_for_another_revision_or_token_decays() {
        let t0 = Instant::now();
        let np = now_playing();
        let fresh = frame(t0, 0.8);
        let mut display = SpectrumDisplay::default();
        display.update(&source(PlaybackPhase::Playing, &np, Some(&fresh)), t0);

        let other_rev = SpectrumFrame {
            session_rev: 10,
            ..frame(t0, 1.0)
        };
        display.update(&source(PlaybackPhase::Playing, &np, Some(&other_rev)), t0);
        assert!(decayed(&display, 0.8), "{:?}", levels(&display));

        let mut display = SpectrumDisplay::default();
        display.update(&source(PlaybackPhase::Playing, &np, Some(&fresh)), t0);
        let unadopted = DrawSource {
            adopted: Some(LoadRequestId::from_raw(5)),
            ..source(PlaybackPhase::Playing, &np, Some(&fresh))
        };
        display.update(&unadopted, t0);
        assert!(decayed(&display, 0.8), "{:?}", levels(&display));
    }

    #[test]
    fn disabled_analysis_decays_and_re_enabling_draws_only_a_new_frame() {
        let t0 = Instant::now();
        let np = now_playing();
        let old = frame(t0, 0.8);
        let mut display = SpectrumDisplay::default();
        display.update(&source(PlaybackPhase::Playing, &np, Some(&old)), t0);
        // Disabled: the handle reports no frame.
        display.update(&source(PlaybackPhase::Playing, &np, None), t0);
        assert!(decayed(&display, 0.8));
        // Re-enabled without new audio: still nothing, and the old frame is
        // stale by now even if something handed it back.
        let later = t0 + Duration::from_millis(300);
        display.update(&source(PlaybackPhase::Playing, &np, Some(&old)), later);
        assert!(levels(&display).iter().all(|level| *level < 0.8 * 0.85));
        let new = frame(later, 0.3);
        display.update(&source(PlaybackPhase::Playing, &np, Some(&new)), later);
        assert_eq!(levels(&display), vec![0.3, 0.3]);
    }

    #[test]
    fn analysis_runs_only_while_the_row_exists_and_playback_is_playing() {
        let normal = regions(Rect::new(0, 0, 100, 30), Tier::Normal, 3).spectrum;
        let minimal = regions(Rect::new(0, 0, 45, 16), Tier::Minimal, 3).spectrum;
        assert!(wants_analysis(normal, PlaybackPhase::Playing));
        assert!(!wants_analysis(minimal, PlaybackPhase::Playing));
        for phase in [
            PlaybackPhase::Paused,
            PlaybackPhase::Stopped,
            PlaybackPhase::Loading,
            PlaybackPhase::Ended,
        ] {
            assert!(!wants_analysis(normal, phase), "{phase:?}");
        }
    }
}

/// The column of the queue's right border, top to bottom.
fn queue_border(view: &PlayerView, ui: &UiState) -> String {
    let (buffer, hits) = render(view, ui, &Visuals::default(), 100, 30);
    let x = hits.queue.right() - 1;
    (hits.queue.y..hits.queue.bottom())
        .map(|y| buffer[(x, y)].symbol().to_owned())
        .collect()
}

#[test]
fn a_playlist_longer_than_its_pane_gets_a_scrollbar_that_follows_the_selection() {
    let short = view(PlaybackPhase::Unloaded, None);
    assert!(
        !queue_border(&short, &UiState::new(true)).contains('┃'),
        "three rows fit: no scrollbar"
    );

    let mut long = view(PlaybackPhase::Unloaded, None);
    let template = long.rows[0].clone();
    let names: Vec<String> = (0..300).map(|n| format!("t{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    long.rows = views::ids_for(&names)
        .into_iter()
        .map(|id| {
            let mut row = template.clone();
            row.id = id;
            row
        })
        .collect();
    let mut ui = UiState::new(true);
    ui.selected = Some(long.rows[0].id);
    let top = queue_border(&long, &ui);
    ui.selected = Some(long.rows[299].id);
    let bottom = queue_border(&long, &ui);

    let thumb = |column: &str| column.chars().position(|c| c == '┃');
    // Row 0 of the column is the corner; the thumb starts right under it.
    assert_eq!(thumb(&top), Some(1), "{top}");
    assert!(thumb(&bottom) > thumb(&top), "{bottom}");
    assert!(
        bottom.ends_with(['┘', '╯']),
        "the corner survives: {bottom}"
    );
}
