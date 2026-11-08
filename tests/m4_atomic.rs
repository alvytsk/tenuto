//! `persistence::atomic::replace_bytes`: the shared atomic-replacement
//! routine extracted from `StateStore::write` so the subscription and cache
//! stores call it rather than copy it (design doc §1.3 item 1).

use tenuto::persistence::atomic::replace_bytes;

#[test]
fn independent_destinations_replace_whole_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let state = dir.path().join("state.json");
    let subs = dir.path().join("subscriptions.json");
    replace_bytes(&state, br#"{"old":"a much longer value"}"#)?;
    replace_bytes(&subs, br#"{"subscriptions":[]}"#)?;
    replace_bytes(&state, b"{}")?;
    assert_eq!(std::fs::read(&state)?, b"{}");
    assert_eq!(std::fs::read(&subs)?, br#"{"subscriptions":[]}"#);
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 2);
    Ok(())
}

/// A destination that is itself a nonempty directory cannot be replaced by
/// `rename`; the failure must surface as an error, the sentinel underneath
/// the destination must be untouched, and no temp file is left behind.
#[test]
fn a_rename_failure_leaves_the_destination_and_no_temp_behind()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let destination = dir.path().join("subscriptions.json");
    std::fs::create_dir(&destination)?;
    let sentinel = destination.join("sentinel");
    std::fs::write(&sentinel, b"do not touch")?;

    let result = replace_bytes(&destination, b"{}");
    assert!(
        result.is_err(),
        "renaming over a nonempty directory must fail"
    );
    assert_eq!(
        std::fs::read(&sentinel)?,
        b"do not touch",
        "the untouched destination must survive the failed replace"
    );

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("subscriptions.json.tmp-"))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp file may survive a failed replace, found {leftovers:?}"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn an_orphan_of_a_dead_process_is_swept_and_a_live_ones_survives()
-> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    // A pid that is certainly gone: a child this test has already reaped.
    let mut reaped = Command::new("true").spawn()?;
    let dead = reaped.id();
    reaped.wait()?;
    // A pid that is certainly alive for the duration: a sleeping child.
    let mut live_child = Command::new("sleep").arg("30").spawn()?;
    let live = live_child.id();

    let dir = tempfile::tempdir()?;
    let destination = dir.path().join("state.json");
    let dead_orphan = dir.path().join(format!("state.json.tmp-{dead}-0"));
    let live_orphan = dir.path().join(format!("state.json.tmp-{live}-3"));
    let own_orphan = dir
        .path()
        .join(format!("state.json.tmp-{}-7", std::process::id()));
    std::fs::write(&dead_orphan, b"crashed mid-write")?;
    std::fs::write(&live_orphan, b"another tenuto is writing")?;
    std::fs::write(&own_orphan, b"our own concurrent write")?;

    replace_bytes(&destination, b"{}")?;

    assert_eq!(std::fs::read(&destination)?, b"{}");
    assert!(
        !dead_orphan.exists(),
        "an orphan whose writer is gone must be swept"
    );
    assert!(
        live_orphan.exists(),
        "a live writer's temporary must survive the sweep"
    );
    assert!(
        own_orphan.exists(),
        "this process's own temporaries must survive the sweep"
    );

    live_child.kill()?;
    live_child.wait()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn the_destination_file_and_a_new_parent_directory_are_private()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir()?;
    let nested = root.path().join("tenuto");
    let destination = nested.join("subscriptions.json");
    replace_bytes(&destination, br#"{"subscriptions":[]}"#)?;

    let dir_mode = std::fs::metadata(&nested)?.permissions().mode() & 0o777;
    let file_mode = std::fs::metadata(&destination)?.permissions().mode() & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "a directory created for a new destination must be private"
    );
    assert_eq!(
        file_mode, 0o600,
        "the destination file's mode must be exactly 0o600, explicitly set"
    );
    Ok(())
}
