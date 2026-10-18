//! The one way a test launches `tenuto` (M5 §12). Every child gets its own
//! state, data, cache and config directories, so no test can read, lock or
//! write the developer's playback profile. Keep the `Profile` alive until the
//! child has exited: dropping it deletes the directories under the child.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(clippy::assertions_on_constants)]
pub fn binary() -> &'static str {
    assert!(
        cfg!(target_os = "linux"),
        "subprocess profile isolation is verified only on Linux"
    );
    env!("CARGO_BIN_EXE_tenuto")
}

/// The environment that isolates a child's profile under `root`, laid out the
/// way the M4 CLI tests already seed it: `root/{state,data,cache,config}`.
/// Shared by [`command_in`] and the PTY helper, so both launch paths isolate
/// the same directories.
pub fn profile_env(root: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("XDG_STATE_HOME", root.join("state").into_os_string()),
        ("XDG_DATA_HOME", root.join("data").into_os_string()),
        ("XDG_CACHE_HOME", root.join("cache").into_os_string()),
        ("XDG_CONFIG_HOME", root.join("config").into_os_string()),
    ]
}

/// A command for `tenuto` whose profile lives under `root`.
pub fn command_in(root: &Path) -> Command {
    let mut command = Command::new(binary());
    command
        .envs(profile_env(root))
        .env_remove("TENUTO_TEST_HOOK")
        .env_remove("TENUTO_AUDIO_OUTPUT")
        .env("TENUTO_MPRIS", "off");
    command
}

pub struct Profile {
    root: tempfile::TempDir,
}

impl Profile {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            root: tempfile::tempdir()?,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Where `StateStore::platform_path` resolves on Linux under this profile.
    pub fn state_dir(&self) -> PathBuf {
        self.root().join("state").join("tenuto")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir().join("state.json")
    }

    pub fn command(&self) -> Command {
        command_in(self.root())
    }
}
