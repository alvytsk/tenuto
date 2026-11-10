//! M14 §2.1: the selected theme persists as an additive raw string — the
//! M13 additive-field rule, with persistence carrying the spelling and
//! never parsing the vocabulary.

use clap::Parser;
use tenuto::cli::{Cli, CliCommand};
use tenuto::persistence::model::PersistedState;

#[path = "support/views.rs"]
mod views;

/// M14 §2.1: the theme round-trips through the state file. The default
/// state carries no theme: absent on disk means "the reader's default"
/// (gruvbox), which is the *reader's* decision, not this layer's.
#[test]
fn the_selected_theme_round_trips_through_json() {
    let mut state = PersistedState::default();
    assert_eq!(state.theme(), None, "the default carries no theme");
    state.set_theme(Some("winamp".into()));

    let json = serde_json::to_string(&state).expect("serializes");
    assert!(json.contains("\"theme\":\"winamp\""), "{json}");

    let loaded: PersistedState = serde_json::from_str(&json).expect("round trips");
    assert_eq!(loaded.theme(), Some("winamp"));
}

/// Raw passthrough: persistence never parses the vocabulary, so an unknown
/// spelling survives the file untouched and the *reader* (which owns
/// `ThemeId`) defaults it.
#[test]
fn an_unknown_theme_spelling_passes_through_raw() {
    let json = r#"{"schema_version":1,"checkpoints":{},"theme":"dracula"}"#;
    let loaded: PersistedState = serde_json::from_str(json).expect("loads leniently");
    assert_eq!(loaded.theme(), Some("dracula"));
}

/// M14 §2.1: `--theme` accepts a stored spelling and rejects an unknown one
/// at parse time, naming the valid set. The accepted value stays the raw
/// string — the CLI, like persistence, carries the spelling and never
/// parses the vocabulary.
#[test]
fn tui_theme_parses_a_stored_spelling_and_names_the_valid_set_when_rejected() {
    let parsed =
        Cli::try_parse_from(["tenuto", "tui", "--theme", "winamp"]).expect("winamp parses");
    let Some(CliCommand::Tui { theme, .. }) = parsed.command else {
        panic!("`tenuto tui --theme winamp` parses to the tui subcommand");
    };
    assert_eq!(theme.as_deref(), Some("winamp"));

    let error = Cli::try_parse_from(["tenuto", "tui", "--theme", "dracula"])
        .expect_err("an unknown theme is a usage error");
    assert!(
        error
            .to_string()
            .contains("valid: gruvbox, winamp, nord, solarized-dark"),
        "{error}"
    );
}

// ----------------------------------------- §2.2: the background fill pass

mod background_fill {
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::{Terminal, backend::TestBackend};

    use tenuto::application::transport::PlaybackPhase;
    use tenuto::tui::render::{Visuals, draw};
    use tenuto::tui::spectrum::VisualizerMode;
    use tenuto::tui::state::UiState;
    use tenuto::tui::theme::{Theme, ThemeId};

    use crate::views::{self, decoded, ids, playing};

    /// A `draw` of the three-row playing view into a `width`×`height`
    /// buffer, with `ui`'s theme applied (the `m5_tui_render` helper's
    /// shape, minus the hit map this suite does not need).
    fn filled_buffer(width: u16, height: u16, ui: &UiState) -> Buffer {
        let view = views::view(
            PlaybackPhase::Playing,
            Some(playing(ids()[0], true, Some(decoded(185)), false)),
        );
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("backend");
        terminal
            .draw(|frame| {
                draw(frame, &view, ui, &Visuals::default());
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The theme-chosen backgrounds widgets may paint over the fill
    /// (spec §2.3): the cover placeholder's panel and the transport
    /// buttons' panel, green and shuffle amber. `base` is what a cell
    /// carries where nothing drew over the fill — the theme's own
    /// background for a filled theme, `Color::Reset` for gruvbox — and
    /// anything else means a cell the fill did not cover, which would
    /// let the terminal's own background through (Review Focus 5).
    fn assert_backgrounds(buffer: &Buffer, width: u16, height: u16, theme: &Theme, base: Color) {
        let widget_bgs = [theme.panel, theme.green, theme.amber];
        for y in 0..height {
            for x in 0..width {
                let bg = buffer[(x, y)].bg;
                assert!(
                    bg == base || widget_bgs.contains(&bg),
                    "{width}×{height}: cell ({x}, {y}) carries {bg:?}"
                );
            }
        }
        // Where nothing draws, the base is exact.
        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
        ] {
            assert_eq!(
                buffer[(x, y)].bg,
                base,
                "{width}×{height}: corner ({x}, {y})"
            );
        }
    }

    /// §2.2 / Review Focus 5: with `winamp` applied at startup, `draw`
    /// fills the whole 100×30 frame with the theme's background before
    /// any widget draws.
    #[test]
    fn a_winamp_themed_frame_fills_every_cell() {
        let theme = ThemeId::Winamp.theme();
        let base = theme.bg.expect("winamp paints a background");
        let ui = UiState::new(true).with_appearance(theme, VisualizerMode::Bars);
        let buffer = filled_buffer(100, 30, &ui);
        assert_backgrounds(&buffer, 100, 30, &theme, base);
    }

    /// Today's property, kept: the gruvbox default paints no background,
    /// so the frame leaves the terminal's own background visible — the
    /// pre-M14 render the existing suites pin, byte-identical (spec §7).
    #[test]
    fn the_gruvbox_default_leaves_the_terminal_background_visible() {
        let theme = Theme::default();
        assert_eq!(theme.bg, None, "gruvbox paints no background");
        let buffer = filled_buffer(100, 30, &UiState::new(true));
        assert_backgrounds(&buffer, 100, 30, &theme, Color::Reset);
    }

    /// §2.2's tier walk: the fill is the frame's first step, before the
    /// layout is carved, so each tier's reference size fills completely —
    /// 80×28 (Normal), 79×27 (Compact), 60×21 (Short), 49×17 (Minimal).
    #[test]
    fn every_tier_fills_completely() {
        let theme = ThemeId::Winamp.theme();
        let base = theme.bg.expect("winamp paints a background");
        let ui = UiState::new(true).with_appearance(theme, VisualizerMode::Bars);
        for (width, height) in [(80, 28), (79, 27), (60, 21), (49, 17)] {
            let buffer = filled_buffer(width, height, &ui);
            assert_backgrounds(&buffer, width, height, &theme, base);
        }
    }
}
