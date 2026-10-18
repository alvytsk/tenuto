//! A hung or missing session bus never blocks `play` or writes to its
//! stderr (M11 spec §7).
#![cfg(target_os = "linux")]
#[path = "support/process.rs"]
mod process;

use std::os::unix::net::UnixListener;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

const FIXTURE_SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");

fn wait_exit(child: &mut Child, patience: Duration) -> ExitStatus {
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return status;
        }
        assert!(Instant::now() < deadline, "the process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn play(profile: &process::Profile, mpris_off: bool, bus: &str) -> std::process::Command {
    let mut command = profile.command();
    command
        .args(["play", FIXTURE_SHORT])
        .env("TENUTO_AUDIO_OUTPUT", "null")
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={bus}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if mpris_off {
        command.env("TENUTO_MPRIS", "off");
    } else {
        command.env_remove("TENUTO_MPRIS");
    }
    command
}

#[test]
#[allow(clippy::expect_used)]
fn play_runs_and_exits_with_an_unresponsive_bus() {
    let profile = process::Profile::new().expect("profile");
    let dir = tempfile::tempdir().expect("dir");
    let socket = dir.path().join("bus");
    let listener = UnixListener::bind(&socket).expect("bind");
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    // Never reads or writes; keeps every stream open. The thread is left to
    // die with the test process.
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept() {
            held.push(stream);
            count.fetch_add(1, SeqCst);
        }
    });
    let mut child = play(&profile, false, socket.to_str().expect("utf8"))
        .spawn()
        .expect("spawn");
    let status = wait_exit(&mut child, Duration::from_secs(20));
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("stderr"), &mut stderr)
        .expect("read");
    assert!(status.success(), "{stderr}");
    assert!(accepted.load(SeqCst) >= 1, "media controls tried the bus");
    assert!(!stderr.to_lowercase().contains("mpris"), "{stderr}");
}

#[test]
#[allow(clippy::expect_used)]
fn play_without_a_bus_writes_nothing_new_to_stderr() {
    let profile = process::Profile::new().expect("profile");
    let missing = profile.root().join("nonexistent");
    let missing = missing.to_str().expect("utf8");
    let baseline = play(&profile, true, missing).output().expect("baseline");
    let run = play(&profile, false, missing).output().expect("run");
    assert!(run.status.success());
    assert_eq!(
        String::from_utf8_lossy(&run.stderr).lines().count(),
        String::from_utf8_lossy(&baseline.stderr).lines().count()
    );
}
