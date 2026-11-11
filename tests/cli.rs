#![cfg(target_os = "linux")]

#[path = "support/process.rs"]
mod process;

use clap::{CommandFactory, Parser};
use tenuto::cli::{Cli, CliCommand};
use tenuto::tui::MouseMode;
use tenuto::tui::images::ArtworkMode;

/// A bare `tenuto` is no longer a usage error: it resolves to no
/// subcommand, which `app::run` dispatches to the player.
#[test]
fn a_bare_invocation_parses_to_no_subcommand() -> Result<(), Box<dyn std::error::Error>> {
    let parsed = Cli::try_parse_from(["tenuto"])?;
    assert!(
        parsed.command.is_none(),
        "bare invocation carries no subcommand"
    );

    // Every existing subcommand still parses as it did.
    let tui = Cli::try_parse_from(["tenuto", "tui"])?;
    assert!(matches!(tui.command, Some(CliCommand::Tui { .. })));
    let feeds = Cli::try_parse_from(["tenuto", "feeds"])?;
    assert!(matches!(feeds.command, Some(CliCommand::Feeds)));
    Ok(())
}

/// The `RUST_LOG` filter-error path predates the CLI and still applies: it
/// fires during `telemetry::init`, before argument parsing runs, so it is
/// independent of whether a subcommand was given.
#[test]
fn an_invalid_rust_log_filter_fails_before_argument_parsing() {
    let profile = process::Profile::new().unwrap();
    let failure = profile
        .command()
        .env("RUST_LOG", "tenuto=not-a-level")
        .output()
        .unwrap();
    assert!(!failure.status.success());
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(stderr.contains("tenuto: invalid tracing filter"));
    assert!(stderr.contains("application startup failed"));
    assert!(stderr.contains("error parsing level filter"));
}

/// §4: usage errors exit 2. `play` with no source is still one; a bare
/// invocation is not, because it now opens the player.
#[test]
fn a_usage_error_still_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    let profile = process::Profile::new()?;
    let output = profile.command().arg("play").output()?;
    assert_eq!(output.status.code(), Some(2), "§4: usage errors exit 2");
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn help_is_a_successful_entry_point_on_stdout() -> Result<(), Box<dyn std::error::Error>> {
    let profile = process::Profile::new()?;
    let output = profile.command().arg("--help").output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("play"));
    Ok(())
}

/// `--version` is what the package smoke test compares against the release
/// tag (Linux packages spec §9.1), so its format is part of the contract:
/// exactly `tenuto <version>` and a newline, exit 0.
#[test]
fn version_flag_prints_the_package_version() {
    let profile = process::Profile::new().unwrap();
    let output = profile.command().arg("--version").output().unwrap();
    assert!(output.status.success(), "--version exits 0");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("tenuto {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// M14 rollback: the visualizer flag is gone from both command forms —
/// an unknown argument at parse time, on the bare invocation and on `tui`.
#[test]
fn the_visualizer_flag_is_gone_from_both_command_forms() {
    let bare = Cli::try_parse_from(["tenuto", "--visualizer", "scope"]);
    let tui = Cli::try_parse_from(["tenuto", "tui", "--visualizer", "scope"]);
    assert!(bare.is_err());
    assert!(tui.is_err());
}

/// The bare form takes the same three flags with the same help: the Options
/// block of the bare long help is pinned byte-exactly, and the bare
/// `--mouse`/`--artwork` defaults are the `tui` ones.
#[test]
fn the_bare_command_takes_the_player_flags_with_the_same_help()
-> Result<(), Box<dyn std::error::Error>> {
    const BARE_OPTIONS_HELP: &str = concat!(
        "Options:\n",
        "      --mouse <MOUSE>\n",
        "          Whether the player captures the mouse\n",
        "          \n",
        "          [default: on]\n",
        "          [possible values: on, off]\n",
        "\n",
        "      --artwork <ARTWORK>\n",
        "          How the player draws cover art\n",
        "          \n",
        "          [default: auto]\n",
        "          [possible values: auto, blocks, off]\n",
        "\n",
        "      --theme <THEME>\n",
        "          The theme to start with; saved as the new default\n",
        "\n",
        "  -h, --help\n",
        "          Print help\n",
        "\n",
        "  -V, --version\n",
        "          Print version\n",
    );
    let parsed = Cli::try_parse_from(["tenuto"])?;
    assert_eq!(
        (parsed.mouse, parsed.artwork),
        (MouseMode::On, ArtworkMode::Auto),
        "the bare form defaults like `tui`"
    );
    assert_eq!(parsed.theme, None);

    let bare = Cli::try_parse_from([
        "tenuto",
        "--mouse",
        "off",
        "--artwork",
        "blocks",
        "--theme",
        "winamp",
    ])?;
    assert_eq!(
        (bare.mouse, bare.artwork),
        (MouseMode::Off, ArtworkMode::Blocks)
    );
    assert_eq!(bare.theme.as_deref(), Some("winamp"));
    assert!(bare.command.is_none(), "still no subcommand");

    let mut command = Cli::command();
    let help = command.render_long_help().to_string();
    let options = help
        .split("Options:\n")
        .nth(1)
        .ok_or("the bare help has an Options block")?;
    assert_eq!(options, BARE_OPTIONS_HELP.trim_start_matches("Options:\n"));
    Ok(())
}

/// M9 foundation cleanup §5: `MouseMode` and `ArtworkMode` move modules,
/// and `tui`'s flags must parse, default and print exactly as before. A
/// bare `tenuto` uses `default()`, so that is pinned too.
#[test]
fn the_tui_flags_keep_their_defaults_and_help() -> Result<(), Box<dyn std::error::Error>> {
    const TUI_HELP: &str = concat!(
        "Open the terminal player on the saved queue\n",
        "\n",
        "Usage: tui [OPTIONS]\n",
        "\n",
        "Options:\n",
        "      --mouse <MOUSE>\n",
        "          Whether the player captures the mouse\n",
        "          \n",
        "          [default: on]\n",
        "          [possible values: on, off]\n",
        "\n",
        "      --artwork <ARTWORK>\n",
        "          How the player draws cover art\n",
        "          \n",
        "          [default: auto]\n",
        "          [possible values: auto, blocks, off]\n",
        "\n",
        "      --theme <THEME>\n",
        "          The theme to start with; saved as the new default\n",
        "\n",
        "  -h, --help\n",
        "          Print help\n",
    );
    let Some(CliCommand::Tui { mouse, artwork, .. }) =
        Cli::try_parse_from(["tenuto", "tui"])?.command
    else {
        panic!("`tenuto tui` parses to the tui subcommand");
    };
    assert_eq!((mouse, artwork), (MouseMode::On, ArtworkMode::Auto));
    assert_eq!(
        (MouseMode::default(), ArtworkMode::default()),
        (MouseMode::On, ArtworkMode::Auto)
    );

    let flags = ["tenuto", "tui", "--mouse", "off", "--artwork", "blocks"];
    let Some(CliCommand::Tui { mouse, artwork, .. }) = Cli::try_parse_from(flags)?.command else {
        panic!("the flags parse to the tui subcommand");
    };
    assert_eq!((mouse, artwork), (MouseMode::Off, ArtworkMode::Blocks));

    let mut command = Cli::command();
    let tui = command
        .find_subcommand_mut("tui")
        .expect("tui is a subcommand");
    assert_eq!(tui.render_long_help().to_string(), TUI_HELP);
    Ok(())
}
