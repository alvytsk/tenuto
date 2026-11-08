use std::sync::{Mutex, MutexGuard};

pub mod callback;
pub mod command;
pub mod decode;
pub mod engine;
pub mod error;
pub mod event;
pub mod handshake;
pub mod link;
pub mod outbox;
pub mod output;
pub mod prepare;
pub mod reconnect;
pub mod resample;
pub mod spectrum;
pub mod state;
pub mod timeline;
pub mod wait;

/// Locks a mutex and carries on with the state a panicked thread left
/// behind: a poisoned lock still holds a consistent value, and these locks
/// guard plain data rather than invariants whose breach would justify
/// propagation. One implementation for `engine`, `prepare`, `wait`, the
/// outbox and the spectrum worker and registry, which each carried an
/// identical copy of this match.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
