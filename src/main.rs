use std::io::Write;
use std::{error::Error, process::ExitCode};

use clap::Parser;
use tenuto::{app, cli, telemetry};

fn main() -> ExitCode {
    if let Err(error) = telemetry::init() {
        eprintln!("tenuto: {error}");
        let fallback = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(fallback, || {
            let mut chain = vec![error.to_string()];
            let mut source = error.source();
            while let Some(cause) = source {
                let text = cause.to_string();
                if chain.last() != Some(&text) {
                    chain.push(text);
                }
                source = cause.source();
            }
            tracing::error!(error = %error, causes = ?chain, "application startup failed");
        });
        return ExitCode::FAILURE;
    }

    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // clap sends help and version to stdout with status 0 and usage errors
            // to stderr with status 2 (§4); printing it ourselves lost both.
            let _ = error.print();
            return ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(2));
        }
    };

    match app::run(cli) {
        Ok(outcome) => ExitCode::from(outcome.exit_status()),
        Err(error) => {
            // A closed terminal must not turn the report into a panic.
            let _ = writeln!(std::io::stderr(), "tenuto: {error}");
            tracing::error!(error = ?error, "command failed");
            ExitCode::FAILURE
        }
    }
}
