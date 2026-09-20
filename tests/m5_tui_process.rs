#![cfg(target_os = "linux")]
#[path = "support/process.rs"]
mod process;
#[path = "support/pty.rs"]
mod pty;
mod support;
#[path = "support/tagged_flac.rs"]
mod tagged_flac;
#[path = "support/tui_profile.rs"]
mod tui_profile;

use pty::PtyChild;
use std::time::Duration;
use support::server::{Script, TestServer};
use tui_profile::Seed;

const CONTENDED: &str = "Another Tenuto player is using this state profile";
const LEAVE_ALT: &str = "\x1b[?1049l";

#[test]
fn pty_children_never_see_the_host_terminal_or_multiplexer() {
    let profile = process::Profile::new().expect("profile");
    let command = pty::command(profile.root(), &["tui"], &[]);
    assert_eq!(
        command.get_env("TERM").and_then(|term| term.to_str()),
        Some("xterm-256color")
    );
    for variable in ["TMUX", "TMUX_PANE", "TERM_PROGRAM"] {
        assert!(command.get_env(variable).is_none(), "{variable} is removed");
    }
    assert_eq!(
        command.get_env("XDG_STATE_HOME"),
        Some(profile.root().join("state").as_os_str()),
        "still launched with the isolated profile"
    );
}

#[test]
fn tui_opens_idle_on_an_empty_queue_and_q_restores_the_terminal() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(
        child.wait_for("Queue is empty", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"q");
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(0));
    assert!(child.output().contains(LEAVE_ALT));
}

#[test]
fn a_bare_invocation_opens_the_player_and_q_restores_the_terminal() {
    let profile = process::Profile::new().expect("profile");
    // No arguments at all, where `["tui"]` would normally go.
    let mut child = PtyChild::spawn(profile.root(), &[], &[], 100, 30).expect("spawn");
    assert!(
        child.wait_for("Queue is empty", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"q");
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(0));
    assert!(child.output().contains(LEAVE_ALT));
}

#[test]
fn tui_refuses_a_held_profile_before_entering_raw_mode() {
    let profile = process::Profile::new().expect("profile");
    let _held = tenuto::lifecycle::lock::ProfileLock::acquire(&profile.state_file()).expect("hold");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    let code = child.wait_exit(Duration::from_secs(10));
    assert!(matches!(code, Some(c) if c != 0));
    let output = child.output();
    assert!(output.contains(CONTENDED), "{output}");
    assert!(
        !output.contains("\x1b[?1049h"),
        "never entered the alternate screen"
    );
}

#[test]
fn play_refuses_while_tui_holds_the_profile_and_tui_keeps_its_volume() {
    let profile = process::Profile::new().expect("profile");
    let mut tui = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(tui.wait_for("Queue is empty", Duration::from_secs(10)));
    tui.send(b"-");
    let play = profile
        .command()
        .args([
            "play",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac"),
        ])
        .env("TENUTO_AUDIO_OUTPUT", "null")
        .output()
        .expect("play");
    assert!(String::from_utf8_lossy(&play.stderr).contains(CONTENDED));
    tui.send(b"q");
    assert_eq!(tui.wait_exit(Duration::from_secs(10)), Some(0));
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.state_file()).expect("flushed"))
            .expect("json");
    assert_eq!(state["volume"], 0.95);
}

#[test]
fn sigterm_restores_the_terminal_and_exits_143() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(child.wait_for("Queue is empty", Duration::from_secs(10)));
    let pid = child.pid().expect("pid");
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .expect("kill")
            .success()
    );
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(143));
    assert!(child.output().contains(LEAVE_ALT));
}

#[test]
fn b_browses_the_working_directory_and_enter_enqueues_a_file() {
    let profile = process::Profile::new().expect("profile");
    // Sorts ahead of the profile's own state/data/cache/config directories.
    let music = profile.root().join("0-music");
    std::fs::create_dir(&music).expect("music dir");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac"),
        music.join("track.flac"),
    )
    .expect("copy fixture");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(child.wait_for("Queue is empty", Duration::from_secs(10)));
    child.send(b"b");
    assert!(
        child.wait_for("0-music/", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"\r");
    assert!(
        child.wait_for("track.flac", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"\r");
    child.send(b"b");
    assert!(
        child.wait_for("1 track", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"q");
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(0));
    let state = std::fs::read_to_string(profile.state_file()).expect("flushed");
    assert!(state.contains("track.flac"), "{state}");
}

// ---------------------------------------------------------------------------
// Task 29: the process-level lifecycle guarantees (§4, §11, §12). Each case
// runs its own child under its own profile, so a panic, a signal or an fd-2
// redirect cannot reach the test runner.
// ---------------------------------------------------------------------------

const SIGNALS: [(&str, u32); 3] = [("INT", 2), ("HUP", 1), ("TERM", 15)];
const FIXTURE_5S: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");
const FIXTURE_SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const NULL_OUTPUT: (&str, &str) = ("TENUTO_AUDIO_OUTPUT", "null");
/// Shown on the key-hint row of every frame with a queue or without one.
const DRAWN: &str = "Quit";
const HOOK_PREFIX: &str = "tenuto test hook";
const CONTAINED: &str = "contained panic in background job";
const PATIENCE: Duration = Duration::from_secs(10);

#[allow(clippy::expect_used)] // A fixed shell command against a child this test just spawned.
fn send_signal(child: &PtyChild, name: &str) {
    let pid = child.pid().expect("pid");
    let status = std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status()
        .expect("kill");
    assert!(status.success(), "kill -{name} {pid}");
}

/// A fresh `tui` under `profile` starts, draws and exits 0 on `q`: whatever
/// ran before released the profile lock.
#[allow(clippy::expect_used)] // Fallible spawn of a fixed test binary.
fn a_fresh_tui_acquires(profile: &process::Profile) {
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(child.wait_for(DRAWN, PATIENCE), "{}", child.output());
    child.send(b"q");
    assert_eq!(child.wait_exit(PATIENCE), Some(0), "{}", child.output());
    assert!(!child.output().contains(CONTENDED));
}

#[test]
fn tui_signals_during_playback_flush_and_exit_128_plus_n() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let media = tui_profile::seed_queue(&profile, &[Seed::local(FIXTURE_5S, None)], Some(0));
        let mut child =
            PtyChild::spawn(profile.root(), &["tui"], &[NULL_OUTPUT], 100, 30).expect("spawn");
        assert!(child.wait_for(DRAWN, PATIENCE), "{}", child.output());
        child.send(b" ");
        // The loaded position label, drawn once playback has run a second.
        assert!(
            child.wait_for("00:01 /", PATIENCE),
            "SIG{name}: {}",
            child.output()
        );
        send_signal(&child, name);
        assert_eq!(child.wait_exit(PATIENCE), Some(128 + number), "SIG{name}");
        assert!(child.output().contains(LEAVE_ALT), "SIG{name}");
        let state = tui_profile::read_state(&profile);
        let secs = state["checkpoints"][&media[0]]["position"]["secs"]
            .as_u64()
            .unwrap_or_else(|| panic!("SIG{name}: no checkpoint in {state}"));
        assert!(secs >= 1, "SIG{name}: position {secs}");
        a_fresh_tui_acquires(&profile);
    }
}

#[test]
fn tui_signals_during_stalled_http_preparation_exit_128_plus_n() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
        tui_profile::seed_queue(
            &profile,
            &[Seed::Remote {
                url: server.url("/a.mp3"),
            }],
            Some(0),
        );
        let mut child =
            PtyChild::spawn(profile.root(), &["tui"], &[NULL_OUTPUT], 100, 30).expect("spawn");
        assert!(child.wait_for(DRAWN, PATIENCE), "{}", child.output());
        child.send(b" ");
        assert!(
            server.wait_until_stalled(PATIENCE),
            "SIG{name}: {}",
            child.output()
        );
        send_signal(&child, name);
        assert_eq!(child.wait_exit(PATIENCE), Some(128 + number), "SIG{name}");
        a_fresh_tui_acquires(&profile);
        server.shutdown();
    }
}

#[test]
fn a_direct_fd2_write_reaches_the_log_not_the_pty() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(
        profile.root(),
        &["tui"],
        &[("TENUTO_TEST_HOOK", "stderr-probe")],
        100,
        30,
    )
    .expect("spawn");
    assert!(
        child.wait_for("Queue is empty", PATIENCE),
        "{}",
        child.output()
    );
    child.send(b"q");
    assert_eq!(child.wait_exit(PATIENCE), Some(0), "{}", child.output());
    let log = tui_profile::newest_log_text(&profile);
    // The shell's write has no `-rust` suffix, so it is the second match.
    assert_eq!(log.matches("tenuto-stderr-probe").count(), 2, "{log}");
    assert!(log.contains("tenuto-stderr-probe-rust"), "{log}");
    let output = child.output();
    assert!(!output.contains("tenuto-stderr-probe"), "{output}");
}

#[test]
fn a_hung_up_pty_still_flushes_and_releases_the_profile() {
    hang_up_after_a_volume_change(PtyChild::close_master);
}

/// A closed pane types nothing, so the hangup is the only thing the input
/// reader sees: end of file on the terminal, with no key to act on first.
#[test]
fn a_silently_closed_pty_still_flushes_and_releases_the_profile() {
    hang_up_after_a_volume_change(PtyChild::close_master_silently);
}

/// Lowers the volume in a fresh `tui`, hangs up with `hang_up`, and checks
/// the run ends, saved the volume and released the profile.
#[allow(clippy::expect_used)] // Fallible spawn of a fixed test binary.
fn hang_up_after_a_volume_change(hang_up: fn(&mut PtyChild)) {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(
        child.wait_for("Queue is empty", PATIENCE),
        "{}",
        child.output()
    );
    child.send(b"-");
    assert!(child.wait_for(" 95%", PATIENCE), "{}", child.output());
    hang_up(&mut child);
    let code = child.wait_exit(PATIENCE);
    assert!(
        matches!(code, Some(129 | 0)),
        "exit {code:?}; log: {}",
        tui_profile::newest_log_text(&profile)
    );
    assert_eq!(tui_profile::read_state(&profile)["volume"], 0.95);
    a_fresh_tui_acquires(&profile);
}

#[test]
fn a_panic_before_redirection_reaches_the_terminal() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(
        profile.root(),
        &["tui"],
        &[("TENUTO_TEST_HOOK", "panic-before-redirect")],
        100,
        30,
    )
    .expect("spawn");
    assert_eq!(child.wait_exit(PATIENCE), Some(101), "{}", child.output());
    let output = child.output();
    assert!(
        output.contains("tenuto test hook: panic-before-redirect"),
        "{output}"
    );
    for log in tui_profile::session_logs(&profile) {
        let text = std::fs::read_to_string(&log).expect("log");
        assert!(!text.contains(HOOK_PREFIX), "{}: {text}", log.display());
    }
}

#[test]
fn a_panic_right_after_redirection_is_printed_on_restored_stderr() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(
        profile.root(),
        &["tui"],
        &[("TENUTO_TEST_HOOK", "panic-after-redirect")],
        100,
        30,
    )
    .expect("spawn");
    assert_eq!(child.wait_exit(PATIENCE), Some(101), "{}", child.output());
    let output = child.output();
    assert!(
        output.contains("tenuto test hook: panic-after-redirect"),
        "{output}"
    );
}

#[test]
fn a_panic_after_terminal_entry_leaves_the_alternate_screen_first() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(
        profile.root(),
        &["tui"],
        &[("TENUTO_TEST_HOOK", "panic-after-terminal")],
        100,
        30,
    )
    .expect("spawn");
    assert_eq!(child.wait_exit(PATIENCE), Some(101), "{}", child.output());
    let output = child.output();
    let message = output
        .find("tenuto test hook: panic-after-terminal")
        .unwrap_or_else(|| panic!("no hook message: {output:?}"));
    let left = output
        .rfind(LEAVE_ALT)
        .unwrap_or_else(|| panic!("never left the alternate screen: {output:?}"));
    assert!(left < message, "{output:?}");
}

/// Two tagged FLACs with embedded covers, `a.flac` then `b.flac`, in one
/// directory under the profile, plus an untagged `0-extra.flac` that sorts
/// first in the browser. `a.flac` is active; each is titled unless its title
/// is `None`.
#[allow(clippy::expect_used)] // Fixture files written into a fresh temporary profile.
fn two_covered_tracks(profile: &process::Profile, titles: [Option<&'static str>; 2]) {
    let music = profile.root().join("0-music");
    std::fs::create_dir(&music).expect("music dir");
    let mut cover = Vec::new();
    image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 100, 50, 255]))
        .write_to(
            &mut std::io::Cursor::new(&mut cover),
            image::ImageFormat::Png,
        )
        .expect("png");
    let mut seeds = Vec::new();
    for ((name, tag), title) in [("a.flac", "Alpha Tag"), ("b.flac", "Beta Tag")]
        .into_iter()
        .zip(titles)
    {
        let built = tagged_flac::tagged_flac(&music, tag, "Artist", "Album", Some(&cover));
        let path = music.join(name);
        std::fs::rename(built, &path).expect("rename");
        seeds.push(Seed::local(&path, title));
    }
    std::fs::copy(FIXTURE_SHORT, music.join("0-extra.flac")).expect("copy extra");
    tui_profile::seed_queue(profile, &seeds, Some(0));
}

#[test]
fn contained_artwork_and_metadata_panics_keep_the_player_running() {
    let cases = [
        ("artwork-job-panic", "artwork job completed"),
        ("artwork-encoding-panic", "artwork encoding completed"),
        ("metadata-job-panic", "metadata job completed"),
    ];
    for (hook, success) in cases {
        let profile = process::Profile::new().expect("profile");
        let metadata = hook == "metadata-job-panic";
        // Under the metadata hook only `a.flac` is untitled, so the one job
        // restoring the queue asks for is the one that panics.
        let titles = if metadata {
            [None, Some("Second")]
        } else {
            [Some("First"), Some("Second")]
        };
        two_covered_tracks(&profile, titles);
        let env = [
            NULL_OUTPUT,
            ("TENUTO_TEST_HOOK", hook),
            ("RUST_LOG", "tenuto=debug"),
        ];
        let mut child = PtyChild::spawn(profile.root(), &["tui"], &env, 100, 30).expect("spawn");
        assert!(
            tui_profile::wait_for_log(&profile, PATIENCE, |log| log.contains(CONTAINED)),
            "{hook}: no contained failure; log: {}",
            tui_profile::newest_log_text(&profile)
        );
        assert!(
            child.wait_for(DRAWN, PATIENCE),
            "{hook}: {}",
            child.output()
        );
        if !metadata {
            // The failed cover leaves the placeholder and says why.
            assert!(
                child.wait_for("Cover art unavailable", PATIENCE),
                "{hook}: {}",
                child.output()
            );
        }
        child.send(b"\x1b[B");
        child.send(b"\r");
        // `b.flac` (half a second long) loaded and drawn: playback and
        // rendering carried on past the contained panic.
        assert!(
            child.wait_for("00:00 / 00:00", PATIENCE),
            "{hook}: {}",
            child.output()
        );
        if metadata {
            // Activating an entry asks for no tags, so the job that must
            // follow the contained panic comes from enqueueing a file.
            child.send(b"b");
            assert!(
                child.wait_for("0-extra.flac", PATIENCE),
                "{hook}: {}",
                child.output()
            );
            // The open browser covers the queue's title, so close it first.
            child.send(b"\r");
            child.send(b"b");
            assert!(
                child.wait_for("3 tracks", PATIENCE),
                "{hook}: {}",
                child.output()
            );
        }
        assert!(
            tui_profile::wait_for_log(&profile, PATIENCE, |log| {
                tui_profile::occurs_after(log, CONTAINED, success)
            }),
            "{hook}: no later `{success}`; log: {}",
            tui_profile::newest_log_text(&profile)
        );
        child.send(b"q");
        assert_eq!(
            child.wait_exit(PATIENCE),
            Some(0),
            "{hook}: {}",
            child.output()
        );
        let output = child.output();
        assert!(!output.contains(HOOK_PREFIX), "{hook}: {output}");
        let log = tui_profile::newest_log_text(&profile);
        assert!(tui_profile::occurs_after(&log, CONTAINED, success), "{log}");
        if metadata {
            // The panicked probe's entry kept its unknown metadata.
            let state = tui_profile::read_state(&profile);
            assert!(
                state["playlists"][0]["entries"][0]["display"]["title"].is_null(),
                "{state}"
            );
        }
    }
}

#[test]
fn an_uncontained_worker_panic_restores_the_terminal_and_fails() {
    let profile = process::Profile::new().expect("profile");
    // One untitled local entry: restoring it hands the metadata workers
    // their first job once the terminal is up.
    tui_profile::seed_queue(&profile, &[Seed::local(FIXTURE_SHORT, None)], None);
    let mut child = PtyChild::spawn(
        profile.root(),
        &["tui"],
        &[("TENUTO_TEST_HOOK", "worker-panic")],
        100,
        30,
    )
    .expect("spawn");
    let code = child.wait_exit(PATIENCE);
    let output = child.output();
    assert!(
        matches!(code, Some(c) if c != 0),
        "exit {code:?}: {output:?}"
    );
    let message = output
        .find("a background worker panicked")
        .unwrap_or_else(|| panic!("no fatal message: {output:?}"));
    let left = output
        .rfind(LEAVE_ALT)
        .unwrap_or_else(|| panic!("never left the alternate screen: {output:?}"));
    assert!(left < message, "{output:?}");
}
