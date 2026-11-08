//! Transport mappings for tap blocks (spec §10, decision 18).
//!
//! The output callback labels every tap block with `(instance, generation,
//! epoch)` and knows nothing about sessions. Before the playback worker lets
//! a transport run, it publishes the [`TapMapping`] that ties that label to
//! the `session_rev` and output format; the analysis worker looks every
//! block up here and discards what it cannot map.
//!
//! The registry lives behind a mutex and is touched only by the playback
//! worker (publish, retire) and the analysis worker (lookup) - never by the
//! audio callback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// What a published `Run` of one transport generation means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TapMapping {
    pub instance: u64,
    pub generation: u16,
    pub epoch: u32,
    pub session_rev: u64,
    pub sample_rate: u32,
    pub channels: u16,
}

/// The live mapping of each transport instance.
///
/// One mapping per instance: publishing a newer one (a seek's reinstall, or
/// a resume's fresh epoch) retires the older, and retiring the instance
/// (teardown) removes it. Instances are never reused, so a wrapped
/// generation or a recreated device can never match an old mapping.
#[derive(Clone, Debug, Default)]
pub struct TapRegistry {
    live: Arc<Mutex<HashMap<u64, TapMapping>>>,
}

impl TapRegistry {
    /// Publishes `mapping`, retiring every older mapping of its instance.
    pub fn publish(&self, mapping: TapMapping) {
        self.lock().insert(mapping.instance, mapping);
    }

    /// Retires every mapping of `instance`.
    pub fn retire_instance(&self, instance: u64) {
        self.lock().remove(&instance);
    }

    /// The live mapping for exactly this label, if there is one.
    pub fn lookup(&self, instance: u64, generation: u16, epoch: u32) -> Option<TapMapping> {
        self.lock()
            .get(&instance)
            .filter(|mapping| mapping.generation == generation && mapping.epoch == epoch)
            .copied()
    }

    /// The shared poison-carrying lock (`crate::playback::lock`): a poisoned
    /// lock still holds a consistent map, and every critical section above
    /// is a single `HashMap` call.
    fn lock(&self) -> MutexGuard<'_, HashMap<u64, TapMapping>> {
        crate::playback::lock(&self.live)
    }
}
