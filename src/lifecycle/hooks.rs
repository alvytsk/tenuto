//! Deterministic test triggers for the `tenuto` binary (design doc M5,
//! implementation decision 8): process tests that need a panic at an exact
//! stage, or a probe on fd 2, cannot arrange that from outside the child
//! process. Instead the child reads `TENUTO_TEST_HOOK` once at startup and
//! a handful of call sites ask whether *they* are the requested stage.
//!
//! Absent or unrecognised values are indistinguishable from `None`: a typo
//! in the environment variable must never silently change which stage a
//! production run behaves like, so anything that is not one of the exact
//! kebab-case names below disables every hook rather than guessing.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestHook {
    None,
    PanicBeforeRedirect,
    PanicAfterRedirect,
    PanicAfterTerminal,
    StderrProbe,
    ArtworkJobPanic,
    ArtworkEncodingPanic,
    MetadataJobPanic,
    WorkerPanic,
    StallShutdown,
}

impl TestHook {
    /// Exact kebab-case names from decision 8. Case-sensitive and total:
    /// anything else, including a differently-cased spelling of a real
    /// name, becomes `None`.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("panic-before-redirect") => Self::PanicBeforeRedirect,
            Some("panic-after-redirect") => Self::PanicAfterRedirect,
            Some("panic-after-terminal") => Self::PanicAfterTerminal,
            Some("stderr-probe") => Self::StderrProbe,
            Some("artwork-job-panic") => Self::ArtworkJobPanic,
            Some("artwork-encoding-panic") => Self::ArtworkEncodingPanic,
            Some("metadata-job-panic") => Self::MetadataJobPanic,
            Some("worker-panic") => Self::WorkerPanic,
            Some("stall-shutdown") => Self::StallShutdown,
            _ => Self::None,
        }
    }

    /// Reads `TENUTO_TEST_HOOK` the first time this is called in the
    /// process and caches the result, so later calls (there may be many,
    /// scattered across call sites) never touch the environment again.
    pub fn from_env() -> Self {
        static HOOK: OnceLock<TestHook> = OnceLock::new();
        *HOOK.get_or_init(|| Self::parse(std::env::var("TENUTO_TEST_HOOK").ok().as_deref()))
    }

    /// Panics with `tenuto test hook: <name>` when this value is the
    /// requested `stage`; a no-op otherwise. Call sites pass their own
    /// stage as `self` after reading it once via [`Self::from_env`].
    pub fn panic_at(self, stage: Self) {
        if self == stage {
            panic!("tenuto test hook: {}", stage.name());
        }
    }

    /// Parks the calling thread forever when this value is the requested
    /// `stage`, after announcing the stall on stderr (`tenuto test hook:
    /// <name>`) so a process test can sync on the moment the wedge begins.
    /// Unlike [`Self::panic_at`] this leaves the process alive: the point is
    /// to hold a graceful path open while something external — a second
    /// Ctrl-C, a watchdog — acts on a run that cannot finish on its own.
    pub fn stall_at(self, stage: Self) {
        if self == stage {
            eprintln!("tenuto test hook: {}", stage.name());
            loop {
                std::thread::park();
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PanicBeforeRedirect => "panic-before-redirect",
            Self::PanicAfterRedirect => "panic-after-redirect",
            Self::PanicAfterTerminal => "panic-after-terminal",
            Self::StderrProbe => "stderr-probe",
            Self::ArtworkJobPanic => "artwork-job-panic",
            Self::ArtworkEncodingPanic => "artwork-encoding-panic",
            Self::MetadataJobPanic => "metadata-job-panic",
            Self::WorkerPanic => "worker-panic",
            Self::StallShutdown => "stall-shutdown",
        }
    }
}
