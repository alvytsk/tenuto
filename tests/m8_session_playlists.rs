mod support;

use std::time::Duration;

use support::media;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::command::LoadRequestId;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::timeline::PositionQuality;
use tenuto::playlist::PlaylistId;
use tenuto::queue::{
    Direction, DisplayMetadata, NewQueueEntry, QueueEntryId, QueueError, QueueSource,
};
#[allow(unused_imports)]
use tenuto::session::{Advance, DisplayUpdate, LoadTarget, Session};

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

fn loaded(request: LoadRequestId, rev: u64, name: &str) -> PlaybackEvent {
    PlaybackEvent::Loaded {
        session_rev: rev,
        request,
        media: media(name),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position: Duration::ZERO,
        disposition: StartDisposition::Fresh,
    }
}

#[allow(dead_code)]
fn progress(rev: u64, name: &str, load: Option<LoadRequestId>) -> Progress {
    Progress {
        session_rev: rev,
        media: Some(media(name)),
        position: Duration::from_secs(9),
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load,
    }
}

/// Playlist A (the default, playing) holds a1 a2; playlist B holds b1 b2.
#[allow(dead_code)]
struct Two {
    session: Session,
    a: PlaylistId,
    b: PlaylistId,
    in_a: Vec<QueueEntryId>,
    in_b: Vec<QueueEntryId>,
}

fn two() -> Two {
    let mut session = Session::new(PersistedState::default());
    let a = session.state().playing();
    let (b, _) = session
        .create_playlist("B")
        .unwrap_or_else(|error| panic!("room: {error}"));
    let (in_a, _) = session
        .enqueue(a, vec![entry("a1"), entry("a2")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    let (in_b, _) = session
        .enqueue(b, vec![entry("b1"), entry("b2")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    Two {
        session,
        a,
        b,
        in_a,
        in_b,
    }
}

/// Registers and adopts a load of `id`, at engine revision `rev`.
fn adopt(session: &mut Session, id: QueueEntryId, name: &str, rev: u64) -> LoadRequestId {
    let request = session
        .register_load(LoadTarget::Queue(id), &media(name))
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    session.observe(&loaded(request, rev, name), FakeClock::new().sample());
    request
}

#[test]
fn enqueue_lands_in_the_named_playlist_and_a_deleted_one_refuses() {
    let mut two = two();
    assert_eq!(
        two.session
            .state()
            .playlist(two.b)
            .expect("B")
            .queue()
            .len(),
        2
    );
    assert_eq!(
        two.session.state().queue().len(),
        2,
        "A, the playing playlist, is untouched by B's adds"
    );
    let gone = PlaylistId::from_raw_for_tests(99);
    assert_eq!(
        two.session.enqueue(gone, vec![entry("x")]).map(|_| ()),
        Err(QueueError::UnknownPlaylist(99))
    );
}

#[test]
fn moving_and_loading_find_an_entry_in_any_playlist() {
    let mut two = two();
    two.session
        .move_entry(two.in_b[0], Direction::Down)
        .expect("B's entry is known");
    let order: Vec<_> = two
        .session
        .state()
        .playlist(two.b)
        .expect("B")
        .queue()
        .entries()
        .iter()
        .map(|e| e.id())
        .collect();
    assert_eq!(order, [two.in_b[1], two.in_b[0]]);
    assert!(
        two.session
            .register_load(LoadTarget::Queue(two.in_b[0]), &media("b1"))
            .is_ok()
    );
}

#[test]
fn a_loaded_for_an_entry_outside_the_playing_playlist_is_accepted_and_adopted() {
    // `registered_target` must look the entry up through its owner, or this
    // `Loaded` is dropped as unknown. (`playing` follows in Task 6.)
    let mut two = two();
    let request = adopt(&mut two.session, two.in_b[0], "b1", 1);
    assert_eq!(
        two.session
            .adopted()
            .map(|adopted| (adopted.request, adopted.target)),
        Some((request, LoadTarget::Queue(two.in_b[0])))
    );
}

#[test]
fn a_display_update_reaches_every_occurrence_in_every_playlist() {
    let mut two = two();
    let (extra, _) = two.session.enqueue(two.b, vec![entry("a1")]).expect("fits");
    two.session.update_display(
        &media("a1"),
        DisplayUpdate {
            artist: Some("Artist".into()),
            ..DisplayUpdate::default()
        },
    );
    let state = two.session.state();
    for id in [two.in_a[0], extra[0]] {
        assert_eq!(
            state
                .find_entry(id)
                .expect("queued")
                .display()
                .artist
                .as_deref(),
            Some("Artist")
        );
    }
}
