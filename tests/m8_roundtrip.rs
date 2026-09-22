//! M8 §6 and §12, the property the per-rule codec tests cannot see: a state
//! that ordinary use produced must load back unrepaired. Every rule test
//! starts from a hand-written damaged file, so none of them can catch a
//! *legal* state the recovery pass mistakes for damage — which is exactly
//! what a shuffle pin outliving its entry used to be.
//!
//! Each step drives a real [`Session`] the way the runtime drives it, writes
//! the result through a real [`StateStore`] into a temporary directory, and
//! checks two things: the load found nothing to repair, and the file has
//! settled — two launches in a row save the same bytes.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use support::media;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::timeline::PositionQuality;
use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueEntryId, QueueSource};
use tenuto::session::{LoadTarget, Session};

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else {
        unreachable!()
    };
    NewQueueEntry::new(
        media(name),
        QueueSource::LocalFile(path),
        DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("a literal entry must be valid: {error}"))
}

fn progress() -> Progress {
    Progress {
        session_rev: 1,
        media: None,
        position: Duration::ZERO,
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: None,
    }
}

/// Registers and adopts a load of `id`, the only way `playing` ever moves.
fn adopt(session: &mut Session, id: QueueEntryId, name: &str) {
    let request = session
        .register_load(LoadTarget::Queue(id), &media(name))
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    session.observe(
        &PlaybackEvent::Loaded {
            session_rev: 1,
            request,
            media: media(name),
            metadata: MediaMetadata::default(),
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::ZERO,
            disposition: StartDisposition::Fresh,
        },
        FakeClock::new().sample(),
    );
}

/// Saves `state`, loads it back, and asserts the load repaired nothing —
/// then does it once more and asserts the two saves are byte-identical, so
/// the file has settled rather than drifting a little on every launch.
///
/// The fixed point is measured from the *loaded* state, not from the
/// session's own: a shuffle pin the session still holds in memory after its
/// entry was removed is dropped by the load (legally and silently, §7), so
/// the very first file can differ by that one field and nothing else.
fn saves_and_loads_unrepaired(state: &PersistedState, dir: &Path, step: &str) {
    let path = dir.join("state.json");
    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    let save = |state: &PersistedState| {
        store
            .write(state)
            .unwrap_or_else(|error| panic!("{step}: write: {error}"));
        std::fs::read(&path).unwrap_or_else(|error| panic!("{step}: read: {error}"))
    };
    let reload = |round: &str| {
        let outcome = store.load();
        assert!(
            outcome.queue_repair.is_none(),
            "{step} ({round}): an ordinary operation saved a state the next load repairs: {:?}",
            outcome.queue_repair.map(|repair| repair.reset)
        );
        assert!(
            outcome.writable,
            "{step} ({round}): the session became unwritable"
        );
        outcome.state
    };

    save(state);
    let once = save(&reload("first load"));
    let twice = save(&reload("second load"));
    assert!(
        once == twice,
        "{step}: saving what the loader returned is not a fixed point ({} vs {} bytes)",
        once.len(),
        twice.len()
    );
}

#[test]
fn ordinary_playlist_and_shuffle_use_never_saves_a_state_the_next_load_repairs() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut session = Session::new(PersistedState::default());
    let a = session.state().playing();
    let now = FakeClock::new().sample();

    let (b, _) = session
        .create_playlist("B")
        .unwrap_or_else(|error| panic!("room: {error}"));
    let (in_a, _) = session
        .enqueue(a, vec![entry("a1"), entry("a2")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    let (in_b, _) = session
        .enqueue(b, vec![entry("b1"), entry("b2")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    saves_and_loads_unrepaired(session.state(), dir.path(), "enqueued into two playlists");

    adopt(&mut session, in_a[0], "a1");
    saves_and_loads_unrepaired(session.state(), dir.path(), "adopted a1");

    // `z` mid-track: shuffle pins the playlist's own cursor as `first`.
    session
        .set_shuffle(a, Some(42))
        .unwrap_or_else(|error| panic!("shuffled: {error}"));
    assert_eq!(
        session
            .state()
            .playlist(a)
            .and_then(|playlist| playlist.shuffle())
            .and_then(|shuffle| shuffle.first),
        Some(in_a[0]),
        "the pin is what the next step orphans"
    );
    saves_and_loads_unrepaired(session.state(), dir.path(), "shuffle on, cursor pinned");

    // `d` on the pinned track: `first` now names an entry that is gone, which
    // §7 calls legal and rule 10 must not report.
    session
        .remove_entry(in_a[0], &progress(), now)
        .unwrap_or_else(|error| panic!("removed: {error}"));
    saves_and_loads_unrepaired(session.state(), dir.path(), "removed the pinned cursor");

    // `c` on a shuffled playlist that has a cursor orphans the pin the same
    // way, and takes the cursor with it. B becomes the playing playlist here.
    adopt(&mut session, in_b[0], "b1");
    assert_eq!(session.state().playing(), b, "B is playing now");
    session
        .set_shuffle(b, Some(7))
        .unwrap_or_else(|error| panic!("shuffled B: {error}"));
    session
        .clear_playlist(b, &progress(), now)
        .unwrap_or_else(|error| panic!("cleared: {error}"));
    saves_and_loads_unrepaired(session.state(), dir.path(), "cleared a shuffled playlist");

    let (scratch, _) = session
        .create_playlist("Scratch")
        .unwrap_or_else(|error| panic!("room: {error}"));
    session
        .enqueue(scratch, vec![entry("s1")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    saves_and_loads_unrepaired(session.state(), dir.path(), "created a playlist");
    session
        .delete_playlist(scratch, &progress(), now)
        .unwrap_or_else(|error| panic!("deleted: {error}"));
    saves_and_loads_unrepaired(session.state(), dir.path(), "deleted a playlist");

    // The playing playlist itself: `playing` and `current_media` move
    // together, and the state that leaves must still load clean.
    session
        .delete_playlist(b, &progress(), now)
        .unwrap_or_else(|error| panic!("deleted the playing playlist: {error}"));
    saves_and_loads_unrepaired(session.state(), dir.path(), "deleted the playing playlist");
}
