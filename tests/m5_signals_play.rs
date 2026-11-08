#![cfg(target_os = "linux")]
#[path = "support/process.rs"]
mod process;
#[path = "support/pty.rs"]
mod pty;
mod support;

use pty::PtyChild;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use support::server::{Script, TestServer};

const SIGNALS: [(&str, i32); 3] = [("INT", 2), ("HUP", 1), ("TERM", 15)];
const FIXTURE_5S: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");
const FIXTURE_SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const PATIENCE: Duration = Duration::from_secs(15);

#[allow(clippy::expect_used)] // A fixed shell command against a child this test just spawned.
fn send_signal(child: &Child, name: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .status()
        .expect("kill");
    assert!(status.success());
}

fn wait_exit(child: &mut Child, patience: Duration) -> ExitStatus {
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return status;
        }
        if Instant::now() >= deadline {
            // Kill on the deadline rather than leaving the child running: a
            // hung process must fail this test, not leak onto the machine.
            let _ = child.kill();
            let _ = child.wait();
            panic!("the process did not exit within {patience:?}; killed");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[allow(clippy::expect_used)] // Fallible spawn of a fixed test binary.
fn next_invocation_acquires_the_profile(profile: &process::Profile) {
    let output = profile
        .command()
        .args(["play", FIXTURE_SHORT])
        .env("TENUTO_AUDIO_OUTPUT", "null")
        .output()
        .expect("next");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn every_shutdown_signal_during_stalled_preparation_exits_128_plus_n() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
        let mut child = profile
            .command()
            .args(["play", &server.url("/a.mp3")])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        assert!(server.wait_until_stalled(Duration::from_secs(10)));
        send_signal(&child, name);
        let status = wait_exit(&mut child, Duration::from_secs(15));
        assert_eq!(status.code(), Some(128 + number), "SIG{name}");
        next_invocation_acquires_the_profile(&profile);
        server.shutdown();
    }
}

#[test]
fn every_shutdown_signal_during_playback_flushes_a_checkpoint() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let mut child = profile
            .command()
            .args(["play", FIXTURE_5S])
            .env("TENUTO_AUDIO_OUTPUT", "null")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        std::thread::sleep(Duration::from_millis(1_500));
        send_signal(&child, name);
        let status = wait_exit(&mut child, Duration::from_secs(15));
        assert_eq!(status.code(), Some(128 + number), "SIG{name}");
        let secs = checkpoint_secs(&profile);
        assert!(secs >= 1, "SIG{name}: position {secs}");
        next_invocation_acquires_the_profile(&profile);
    }
}

/// The flushed checkpoint position of `FIXTURE_5S`, in whole seconds.
#[allow(clippy::expect_used)] // Reads the state file a test run just flushed.
fn checkpoint_secs(profile: &process::Profile) -> u64 {
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.state_file()).expect("flushed"))
            .expect("json");
    let key = format!(
        "local:{}",
        std::fs::canonicalize(FIXTURE_5S)
            .expect("fixture")
            .display()
    );
    state["checkpoints"][&key]["position"]["secs"]
        .as_u64()
        .expect("checkpoint written")
}

/// `play` of `FIXTURE_5S` on a PTY, so it runs in raw mode reading keys,
/// once its status line shows a second of playback.
#[allow(clippy::expect_used)] // Fallible spawn of a fixed test binary.
fn playing_under_a_pty(profile: &process::Profile) -> PtyChild {
    let child = PtyChild::spawn(
        profile.root(),
        &["play", FIXTURE_5S],
        &[("TENUTO_AUDIO_OUTPUT", "null")],
        100,
        30,
    )
    .expect("spawn");
    assert!(child.wait_for("00:01", PATIENCE), "{}", child.output());
    child
}

#[test]
fn every_shutdown_signal_in_raw_mode_under_a_pty_flushes_a_checkpoint() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let mut child = playing_under_a_pty(&profile);
        let status = std::process::Command::new("kill")
            .arg(format!("-{name}"))
            .arg(child.pid().expect("pid").to_string())
            .status()
            .expect("kill");
        assert!(status.success());
        assert_eq!(
            child.wait_exit(PATIENCE),
            Some((128 + number).unsigned_abs()),
            "SIG{name}: {}",
            child.output()
        );
        let secs = checkpoint_secs(&profile);
        assert!(secs >= 1, "SIG{name}: position {secs}");
        next_invocation_acquires_the_profile(&profile);
    }
}

/// A closed pane types nothing: the hangup is all the key reader sees.
#[test]
fn a_silently_closed_pty_ends_play_and_flushes_a_checkpoint() {
    let profile = process::Profile::new().expect("profile");
    let mut child = playing_under_a_pty(&profile);
    child.close_master_silently();
    let code = child.wait_exit(PATIENCE);
    assert!(matches!(code, Some(129 | 1)), "exit {code:?}");
    let secs = checkpoint_secs(&profile);
    assert!(secs >= 1, "position {secs}");
    next_invocation_acquires_the_profile(&profile);
}

#[test]
fn exit_status_arithmetic() {
    use tenuto::lifecycle::RunOutcome;
    assert_eq!(RunOutcome::Completed.exit_status(), 0);
    assert_eq!(RunOutcome::Signalled(15).exit_status(), 143);
    assert_eq!(RunOutcome::Signalled(200).exit_status(), 255);
}

/// The point of the second Ctrl-C: the first one is caught and the graceful
/// shutdown starts, but here that shutdown is wedged mid-flush by the
/// `stall-shutdown` hook — exactly the state where swallowing the second
/// SIGINT would hang forever. The second SIGINT must kill the process by
/// the signal itself, not exit through the graceful path.
#[test]
fn a_second_sigint_kills_a_shutdown_wedged_mid_flush() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::sync::mpsc;

    let profile = process::Profile::new().expect("profile");
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let mut child = profile
        .command()
        .args(["play", &server.url("/a.mp3")])
        .env("TENUTO_AUDIO_OUTPUT", "null")
        .env("TENUTO_TEST_HOOK", "stall-shutdown")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    assert!(server.wait_until_stalled(Duration::from_secs(10)));

    // Collect the child's stderr from the start, so nothing written before
    // the first signal is lost.
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // First SIGINT: the graceful path breaks out, reaches the final flush
    // and parks in the hook, which announces itself on stderr.
    send_signal(&child, "INT");
    let deadline = Instant::now() + PATIENCE;
    let mut wedged = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains("tenuto test hook: stall-shutdown") => {
                wedged = true;
                break;
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    assert!(
        wedged,
        "the graceful shutdown never reached the wedged flush"
    );

    // Second SIGINT: the flush cannot be waited out, so this one kills the
    // process outright — death by SIGINT, not a graceful exit code.
    send_signal(&child, "INT");
    let status = wait_exit(&mut child, PATIENCE);
    assert_eq!(status.code(), None, "killed by the signal: {status:?}");
    assert_eq!(status.signal(), Some(2), "SIGINT: {status:?}");
    next_invocation_acquires_the_profile(&profile);
    server.shutdown();
}
