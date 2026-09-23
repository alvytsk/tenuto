#![cfg(target_os = "linux")]

#[path = "support/process.rs"]
mod process;

use clap::Parser;
use tenuto::cli::{Cli, CliCommand};

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
