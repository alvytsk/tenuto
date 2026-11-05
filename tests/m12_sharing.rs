//! M12: what the writer receives stays its own. A submitted snapshot shares
//! its subtrees with the session (that is the point of the `Arc`s), and the
//! one law that makes that safe is that every mutation goes through
//! `Arc::make_mut`. This suite drives the real writer path: submit S1,
//! mutate, submit S2 — the retained S1 still holds exactly what was
//! submitted first, none of the later mutation.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

use std::sync::{Arc, Mutex};

use runtime::{enqueue, pump_until, rig_with_sink};
use tenuto::application::runtime::EnqueueItem;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::writer::StateSink;
use tenuto::queue::{NewQueueEntry, QueueSource};
use tenuto::session::Session;

/// Keeps every snapshot the writer hands it, in order.
#[derive(Clone, Default)]
struct RetainingSink {
    written: Arc<Mutex<Vec<PersistedState>>>,
}

impl RetainingSink {
    /// How many snapshots have been written so far.
    fn count(&self) -> usize {
        self.written
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl StateSink for RetainingSink {
    fn write(&self, state: &PersistedState) -> Result<(), tenuto::persistence::PersistenceError> {
        self.written
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(state.clone());
        Ok(())
    }
}

/// A state whose queue holds exactly one entry, built through a `Session`
/// the way `m8_snapshot_size.rs` builds its fixtures.
fn state_with_one_entry() -> PersistedState {
    let path = AbsolutePath::new("/music/first.flac".into())
        .unwrap_or_else(|error| panic!("absolute: {error}"));
    let first = NewQueueEntry::new(
        MediaId::LocalFile(path.clone()),
        QueueSource::LocalFile(path),
        tenuto::queue::DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("valid: {error}"));
    let mut session = Session::new(PersistedState::default());
    let dest = session.state().playlists().playing();
    session
        .enqueue(dest, vec![first])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    session.state().clone()
}

/// The ids of the playing playlist's queue, in order.
fn queue_ids(state: &PersistedState) -> Vec<u64> {
    state
        .playlists()
        .playing_playlist()
        .queue()
        .entries()
        .iter()
        .map(|entry| entry.id().get())
        .collect()
}

#[test]
fn a_submitted_snapshot_does_not_change_underneath_the_writer() {
    let sink = RetainingSink::default();
    let mut rig = rig_with_sink(state_with_one_entry(), Box::new(sink.clone()));

    // First submission: one more entry joins the initial one. The writer's
    // 2-s coalesce window passes inside the stepping, so this lands as S1.
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::Url(
            "https://example.org/m12-second.flac".into(),
        )],
    );
    pump_until(&mut rig, "the first snapshot is written", |_| {
        sink.count() >= 1
    });

    // Second submission: a third entry, into an emptied slot.
    enqueue(
        &mut rig.runtime,
        vec![EnqueueItem::Url(
            "https://example.org/m12-third.flac".into(),
        )],
    );
    pump_until(&mut rig, "the second snapshot is written", |_| {
        sink.count() >= 2
    });

    let written = sink
        .written
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(written.len(), 2, "each submission produced one write");

    // S1 is exactly what the first submission handed over: the initial
    // entry and the first, in that order — none of the second.
    assert_eq!(
        queue_ids(&written[0]),
        [1, 2],
        "S1 keeps the submitted entries in the submitted order"
    );
    // S2 has both later submissions, and the initial entry before them.
    assert_eq!(queue_ids(&written[1]), [1, 2, 3]);
}
