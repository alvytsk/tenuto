#![cfg(target_os = "linux")]

#[path = "support/process.rs"]
mod process;
mod support;

use std::time::Duration;

#[allow(clippy::unwrap_used)] // Fallible spawn of a fixed test binary.
fn run(args: &[&str]) -> std::process::Output {
    let profile = process::Profile::new().unwrap();
    // `output()` waits for the child, so `profile` outlives it.
    profile.command().args(args).output().unwrap()
}

#[test]
fn no_arguments_opens_the_player_and_fails_cleanly_without_a_terminal() {
    // A bare `tenuto` opens the full-screen player (reference.md). With no
    // tty it cannot, and says so rather than panicking or printing help.
    let output = run(&[]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("cannot set up the terminal"),
        "a bare run must try the player: {text}"
    );
}

#[test]
fn an_absent_file_exits_nonzero_with_a_concise_message() {
    let output = run(&["play", "/nonexistent/definitely-not-here.flac"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("tenuto:"),
        "expected a prefixed message: {text}"
    );
    assert!(
        text.contains("definitely-not-here.flac"),
        "expected the path: {text}"
    );
}

#[test]
fn a_directory_is_rejected_as_not_a_regular_file() {
    let output = run(&["play", env!("CARGO_MANIFEST_DIR")]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("regular file"),
        "expected the regular-file rule: {text}"
    );
}

#[test]
fn a_relative_path_is_accepted_and_canonicalized() {
    // Canonicalization happens at the worker's source-opening boundary, so a
    // relative path must not be rejected by argument parsing.
    let profile = process::Profile::new().unwrap();
    let output = profile
        .command()
        .args(["play", "tests/fixtures/sine.flac", "--probe-only"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("44100"),
        "expected the negotiated rate: {text}"
    );
}

#[test]
fn a_stalled_remote_open_is_quittable_before_anything_loads() {
    // §5: "remains able to stop or quit". With no tty this exercises the
    // no-raw-mode path, where the loop still runs and still shuts down
    // cleanly when the open deadline fires; with a tty it is one keypress.
    let server = support::server::TestServer::start(
        support::server::Script::serving(b"x".to_vec()).stall_headers(),
    );
    let started = std::time::Instant::now();
    let output = run(&["play", &server.url("/audio.mp3")]);
    assert!(!output.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(45),
        "the process hung on a stalled open: {:?}",
        started.elapsed()
    );
    server.shutdown();
}

#[test]
fn a_rejected_file_still_reports_without_a_device_or_a_terminal() {
    // R5/G4. Preparation moved to the worker; this is the M1 property that
    // move must not cost. CI has neither an audio device nor a controlling
    // terminal, so a passing run here is the evidence.
    let output = run(&["play", env!("CARGO_MANIFEST_DIR")]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("regular file"),
        "expected the regular-file rule: {text}"
    );
}

/// `--probe-only` prints decoder metadata, which is untrusted: a tagged
/// title reaches stdout through the same escaping playback and the feed
/// listings use, never raw.
#[test]
fn probe_only_escapes_a_title_carrying_terminal_controls() {
    let profile = process::Profile::new().unwrap();
    let output = profile
        .command()
        .args(["play", "tests/fixtures/sine-tagged.flac", "--probe-only"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !text.contains('\u{1b}'),
        "raw escape reached stdout: {text:?}"
    );
    assert_eq!(
        text.lines().count(),
        1,
        "an injected newline split the line: {text:?}"
    );
    assert!(
        text.starts_with(r"Sine\u{1b}[2J\nInjected 44100"),
        "{text:?}"
    );
}
