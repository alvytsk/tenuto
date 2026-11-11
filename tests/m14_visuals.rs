//! The session appearance that survives the visualizer rollback: the
//! theme's raw spelling through the session and the runtime command path,
//! the startup resolution, and the bars-only spectrum row's render pins.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

#[path = "support/views.rs"]
mod views;

use clap::Parser;
use runtime::rig_with;
use tenuto::application::runtime::AppCommand;
use tenuto::cli::{Cli, CliCommand};
use tenuto::persistence::model::PersistedState;
use tenuto::session::{Action, Session};
use tenuto::tui::state::UiState;
use tenuto::tui::theme::{Theme, ThemeId};

/// M14 §2.1: the theme carries the raw stored spelling — the session layer
/// never parses the vocabulary. `None` is a real value: clearing removes the
/// field rather than leaving a stale spelling behind.
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

/// `AppCommand::SetTheme` dispatched through the runtime's command handling
/// lands in state — the `SetRepeat` shape — so the command path and the
/// direct call agree.
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

/// No `--theme` flag: `None`. That absence is what the startup reads as
/// "use the saved theme", so it must stay distinguishable from an
/// explicitly passed value.
#[test]
fn the_absent_theme_flag_parses_to_none() {
    let parsed = Cli::try_parse_from(["tenuto", "tui"]).expect("the bare tui parses");
    let Some(CliCommand::Tui { theme, .. }) = parsed.command else {
        panic!("`tenuto tui` parses to the tui subcommand");
    };
    assert_eq!(theme, None);
}

// ---------------------------------------------------- the startup theme

/// A fresh state is the default palette; `with_appearance` replaces the
/// theme — the startup's one resolution.
#[test]
fn with_appearance_sets_the_startup_theme() {
    let ui = UiState::new(true);
    assert_eq!(ui.theme, Theme::default());

    let ui = UiState::new(true).with_appearance(ThemeId::Winamp.theme());
    assert_eq!(ui.theme, ThemeId::Winamp.theme());
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

// --------------------------------- the bars row renders at every height

mod panel_render {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use tenuto::application::transport::PlaybackPhase;
    use tenuto::tui::render::{Visuals, draw};
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

    /// The one-row spectrum region (100×20, the short tier's
    /// Rect::new(12, 3, 85, 1)) renders the bars row, as before M14: a
    /// loud bar in amber, at every height.
    #[test]
    fn a_one_row_panel_renders_bars() {
        let theme = Theme::default();
        let visuals = Visuals {
            spectrum: Some(&[1.0_f32; 85]),
            ..Visuals::default()
        };
        let ui = UiState::new(true).with_appearance(theme);
        let buffer = draw_at(&ui, &visuals, 100, 20);
        assert_eq!(buffer[(12, 3)].symbol(), "█");
        assert_eq!(
            buffer[(12, 3)].fg,
            theme.amber,
            "the bars row, loud bar in amber"
        );
    }
}
