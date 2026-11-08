//! Shutdown signals for `play` (design doc M5 §6.5): SIGINT, SIGHUP and
//! SIGTERM are caught rather than left to kill the process outright, so the
//! session can still flush its final checkpoint and release the profile
//! lock before it exits. A **second** SIGINT force-quits: the graceful path
//! had its chance at the first one, and the next must not be swallowed by a
//! shutdown that may itself be wedged.
//!
//! The listener thread this installs does exactly one thing per signal: it
//! records the first one, flags a shutdown as requested, and wakes anyone
//! polling [`ShutdownSignals::wake`]. It never renders, loads state or
//! flushes — that all still happens on the caller's own thread, in
//! `app::run_resolved_locked`'s ordinary shutdown path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crossbeam_channel::{Receiver, Sender};

use crate::lifecycle::RunOutcome;

/// Shared between the listener thread (or `request()`, for a `q`/Ctrl-C
/// shutdown) and whoever polls the outcome.
struct Shared {
    /// Set the moment anything asks the run to stop, signal or key alike.
    requested: AtomicBool,
    /// The first OS signal number received, `0` while none has arrived —
    /// no signal this module registers for is numbered `0`, so that value is
    /// free to serve as the sentinel the listener's compare-exchange targets.
    first_signal: AtomicI32,
}

/// Installed once, near the top of a `play` session, and closed once at the
/// end of it. See the module documentation for what the listener thread
/// does and does not do.
pub struct ShutdownSignals {
    shared: Arc<Shared>,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
    platform: Platform,
}

impl ShutdownSignals {
    /// Installs SIGINT, SIGHUP and SIGTERM handling on Unix. Elsewhere there
    /// is nothing to install; the returned value is simply inert — `request`
    /// is still how such a build ever sees a shutdown asked for.
    pub fn install() -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            requested: AtomicBool::new(false),
            first_signal: AtomicI32::new(0),
        });
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let platform = Platform::install(&shared, wake_tx.clone())?;
        Ok(Self {
            shared,
            wake_tx,
            wake_rx,
            platform,
        })
    }

    /// Records a shutdown with no signal — what `q` and the Ctrl-C key
    /// route through, so both keep exiting `0` (`outcome()` stays
    /// `Completed` unless a real signal also arrived).
    pub fn request(&self) {
        self.shared.requested.store(true, Ordering::SeqCst);
        let _ = self.wake_tx.try_send(());
    }

    pub fn requested(&self) -> bool {
        self.shared.requested.load(Ordering::SeqCst)
    }

    pub fn first_signal(&self) -> Option<i32> {
        match self.shared.first_signal.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    /// Readable alongside whatever else a loop already waits on, so a
    /// signal arriving mid-wait is noticed within that same pass rather than
    /// only after the wait's own deadline.
    pub fn wake(&self) -> &Receiver<()> {
        &self.wake_rx
    }

    /// `Signalled(first)` if a signal was recorded, regardless of whether
    /// `request()` was also called; `Completed` otherwise.
    pub fn outcome(&self) -> RunOutcome {
        match self.first_signal() {
            Some(signal) => RunOutcome::Signalled(signal),
            None => RunOutcome::Completed,
        }
    }

    /// Closes the platform handle and joins the listener thread. Idempotent:
    /// `Drop` runs the same teardown for a value this was never called on.
    pub fn close(mut self) {
        self.platform.shutdown();
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        self.platform.shutdown();
    }
}

#[cfg(unix)]
struct Platform {
    handle: Option<signal_hook::iterator::Handle>,
    listener: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl Platform {
    fn install(shared: &Arc<Shared>, wake_tx: Sender<()>) -> std::io::Result<Self> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        use signal_hook::iterator::Signals;
        use signal_hook::low_level::emulate_default_handler;

        let mut signals = Signals::new([SIGINT, SIGHUP, SIGTERM])?;
        let handle = signals.handle();
        let shared = Arc::clone(shared);
        let listener = std::thread::Builder::new()
            .name("tenuto-signal-listener".to_string())
            .spawn(move || {
                // `forever()` blocks until a signal arrives or `handle.close()`
                // is called; the latter is exactly how this loop ends.
                let mut sigints = 0usize;
                for signal in signals.forever() {
                    let _ = shared.first_signal.compare_exchange(
                        0,
                        signal,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    shared.requested.store(true, Ordering::SeqCst);
                    let _ = wake_tx.try_send(());
                    // The graceful path got its chance at the first SIGINT; a
                    // second one performs the signal's default action, killing
                    // the process outright. `emulate_default_handler` only
                    // returns when the signal is somehow undeliverable, and
                    // then the loop simply keeps serving the graceful path.
                    if signal == SIGINT {
                        sigints += 1;
                        if sigints == 2 {
                            let _ = emulate_default_handler(SIGINT);
                        }
                    }
                }
            })?;
        Ok(Self {
            handle: Some(handle),
            listener: Some(listener),
        })
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.close();
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

#[cfg(not(unix))]
struct Platform;

#[cfg(not(unix))]
impl Platform {
    fn install(_shared: &Arc<Shared>, _wake_tx: Sender<()>) -> std::io::Result<Self> {
        Ok(Self)
    }

    fn shutdown(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_driven_request_records_no_signal() {
        let signals = ShutdownSignals::install().expect("install");
        assert!(!signals.requested());
        signals.request();
        assert!(signals.requested());
        assert_eq!(signals.first_signal(), None);
        assert_eq!(signals.outcome(), RunOutcome::Completed);
        signals.close();
    }

    #[test]
    fn wake_fires_on_request() {
        let signals = ShutdownSignals::install().expect("install");
        signals.request();
        assert!(signals.wake().try_recv().is_ok());
        signals.close();
    }
}
