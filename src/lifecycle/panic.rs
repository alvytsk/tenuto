//! Panic containment for the terminal interface (design doc M5 §11).
//!
//! Two kinds of panic reach this module and must be handled completely
//! differently. A panic inside a background job — metadata lookup, artwork
//! decoding, an artwork encoding pass — must become an ordinary failed
//! result: it is contained with [`run_contained`], and if the global panic
//! hook still sees it (because the job did not itself call `catch_unwind`
//! on the same thread, or a nested job re-panics after being caught) it is
//! only ever logged, never allowed to touch the terminal or fd 2. A panic
//! anywhere else is fatal: the terminal must be restored, any redirected
//! stderr released, and the process's own shutdown machinery woken, before
//! the previous panic hook (which prints the message and location) runs.
//!
//! [`FatalCleanup`] is the single shared handle both paths use, installed
//! once via [`install_panic_hook`] and otherwise driven by ordinary
//! shutdown code calling [`FatalCleanup::restore_now`].

use std::fs::File;
use std::io::Write as _;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use crossbeam_channel::Sender;

use crate::lifecycle::terminal::TerminalCleanup;
use crate::telemetry::displayable;

thread_local! {
    /// Whether the current thread is inside a [`run_contained`] job. Read by
    /// the panic hook, which must never itself panic while checking this.
    static IN_CONTAINED_JOB: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Never panics: if the thread-local cannot be accessed (e.g. during thread
/// teardown) this reports "not contained" rather than propagating a panic
/// from a function whose whole purpose is to be safe to call from a panic
/// hook.
pub fn in_contained_job() -> bool {
    IN_CONTAINED_JOB
        .try_with(|flag| flag.get())
        .unwrap_or(false)
}

/// Sets the thread-local flag to `true` for its lifetime and restores
/// whatever value it held before, on every path out of scope — normal
/// return or unwind alike.
struct ContainmentGuard {
    previous: bool,
}

impl ContainmentGuard {
    fn enter() -> Self {
        let previous = IN_CONTAINED_JOB
            .try_with(std::cell::Cell::get)
            .unwrap_or(false);
        let _ = IN_CONTAINED_JOB.try_with(|flag| flag.set(true));
        Self { previous }
    }
}

impl Drop for ContainmentGuard {
    fn drop(&mut self) {
        let _ = IN_CONTAINED_JOB.try_with(|flag| flag.set(self.previous));
    }
}

/// A background job panicked; `label` names which kind (`"artwork"`,
/// `"metadata"`, ...) so the caller can log or display it without carrying
/// the original payload, which is not guaranteed to be `Send` in a useful
/// form.
#[derive(Debug, thiserror::Error)]
#[error("{label} job panicked")]
pub struct ContainedPanic {
    pub label: &'static str,
}

/// Runs `job` with the current thread marked as "in a contained job" and
/// catches any panic it raises, turning it into `Err(ContainedPanic)`.
/// Never call this around engine or `Session` code: it is for disposable
/// background work only, whose state a caught panic can simply discard.
pub fn run_contained<R>(label: &'static str, job: impl FnOnce() -> R) -> Result<R, ContainedPanic> {
    let _guard = ContainmentGuard::enter();
    panic::catch_unwind(AssertUnwindSafe(job)).map_err(|_| ContainedPanic { label })
}

/// What [`TakeOnceSlot::try_take`] found.
pub enum TakeResult<T> {
    Taken(T),
    Empty,
    Busy,
}

/// A value that can be published at most once and taken at most once,
/// shared between the thread that produces it (e.g. a stderr redirect
/// guard) and whichever of ordinary shutdown or the panic hook gets to it
/// first. `try_take` never blocks, so a hook racing a normal teardown can
/// never deadlock on it.
pub struct TakeOnceSlot<T> {
    inner: Arc<Mutex<Option<T>>>,
}

impl<T> Default for TakeOnceSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TakeOnceSlot<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Fails with the value back if the slot is already occupied; this
    /// slot is take-once, not a queue.
    pub fn publish(&self, value: T) -> Result<(), T> {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_some() {
            return Err(value);
        }
        *guard = Some(value);
        Ok(())
    }

    /// Never blocks. A poisoned lock (the holder panicked while it held the
    /// value) still yields whatever was published — the value itself is
    /// fine, only the code that was manipulating it panicked — so it is
    /// recovered rather than treated as lost. A lock actually held by
    /// another thread becomes `Busy` rather than blocking.
    pub fn try_take(&self) -> TakeResult<T> {
        let mut guard = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return TakeResult::Busy,
        };
        let taken = { guard.take() };
        drop(guard);
        match taken {
            Some(value) => TakeResult::Taken(value),
            None => TakeResult::Empty,
        }
    }

    /// Exists only so `tests/m5_contained_panic.rs` — an integration test,
    /// which cannot see `cfg(test)` items in this crate — can hold or
    /// poison the slot's lock from another thread to exercise `Busy` and
    /// poison recovery. No production code calls this.
    #[doc(hidden)]
    pub fn hold_for_test(&self, f: impl FnOnce()) {
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f();
        drop(guard);
    }
}

impl<T> Clone for TakeOnceSlot<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// The single handle shared by the panic hook and ordinary shutdown code:
/// what to restore, and how to tell the rest of the process a fatal panic
/// happened.
pub struct FatalCleanup {
    rendering_disabled: AtomicBool,
    fatal_requested: AtomicBool,
    terminal: TerminalCleanup,
    stderr: TakeOnceSlot<Box<dyn Send>>,
    diagnostic_log: Mutex<Option<File>>,
    wake: Sender<()>,
}

impl FatalCleanup {
    pub fn new(wake: Sender<()>) -> Self {
        Self {
            rendering_disabled: AtomicBool::new(false),
            fatal_requested: AtomicBool::new(false),
            terminal: TerminalCleanup::default(),
            stderr: TakeOnceSlot::new(),
            diagnostic_log: Mutex::new(None),
            wake,
        }
    }

    pub fn terminal(&self) -> &TerminalCleanup {
        &self.terminal
    }

    pub fn stderr_slot(&self) -> &TakeOnceSlot<Box<dyn Send>> {
        &self.stderr
    }

    pub fn set_diagnostic_log(&self, file: File) {
        let mut guard = match self.diagnostic_log.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(file);
    }

    pub fn rendering_disabled(&self) -> bool {
        self.rendering_disabled.load(Ordering::SeqCst)
    }

    pub fn fatal_requested(&self) -> bool {
        self.fatal_requested.load(Ordering::SeqCst)
    }

    /// Restores the terminal and drops the redirected-stderr guard, if one
    /// was published. Used by both the panic hook's fatal path and normal
    /// teardown, so it must be safe to call more than once and from either
    /// context.
    pub fn restore_now(&self) {
        self.terminal.restore();
        let _ = self.stderr.try_take();
    }

    /// Best-effort: a busy or absent log is silently skipped, never
    /// blocked on, because this can run from inside a panic hook.
    fn log_contained_panic(&self, message: &str) {
        if let Ok(mut guard) = self.diagnostic_log.try_lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = writeln!(file, "contained panic in background job: {message}");
        }
    }
}

/// The panic payload as a string, for logging. Not every panic carries a
/// `&str` or `String` payload (`std::panic::panic_any` can carry anything),
/// so this falls back to a fixed placeholder rather than failing to log at
/// all.
fn panic_message(info: &panic::PanicHookInfo<'_>) -> String {
    info.payload()
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// The panic message, escaped the same way a feed-supplied title is
/// (`telemetry::displayable`) and cut to 512 characters, so a pathological
/// or adversarial payload cannot inject terminal control sequences into a
/// log file or grow it unboundedly.
fn sanitized_panic_message(info: &panic::PanicHookInfo<'_>) -> String {
    displayable(&panic_message(info))
        .chars()
        .take(512)
        .collect()
}

/// Installs the process-global panic hook. Chains to whatever hook was
/// previously installed (the default hook, unless something else already
/// replaced it) so panic messages are still printed.
///
/// A panic on a thread inside [`run_contained`] is logged, if a diagnostic
/// log is set, and nothing else: it must not restore the terminal, touch
/// fd 2, disable rendering, call the previous hook, or request shutdown,
/// because the containing job's own `catch_unwind` is already turning it
/// into an ordinary failed result.
///
/// Any other panic is fatal: rendering is disabled, shutdown is requested
/// and the wake channel is nudged (never blocking on it), the terminal and
/// stderr are restored immediately, and only then does the previous hook
/// run.
pub fn install_panic_hook(cleanup: Arc<FatalCleanup>) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if in_contained_job() {
            cleanup.log_contained_panic(&sanitized_panic_message(info));
            return;
        }
        cleanup.rendering_disabled.store(true, Ordering::SeqCst);
        cleanup.fatal_requested.store(true, Ordering::SeqCst);
        let _ = cleanup.wake.try_send(());
        cleanup.restore_now();
        previous(info);
    }));
}
