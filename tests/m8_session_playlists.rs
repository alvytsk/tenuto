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
use tenuto::playlist::{PlaylistId, Shuffle};
use tenuto::queue::{
    Direction, DisplayMetadata, NewQueueEntry, QueueEntryId, QueueError, QueueSource,
};
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

fn end(rev: u64) -> PlaybackEvent {
    PlaybackEvent::EndOfTrack {
        session_rev: rev,
        position: Duration::from_millis(500),
        provenance: PositionProvenance::Established,
    }
}

#[test]
fn playing_changes_at_adoption_and_the_old_playlist_keeps_its_cursor() {
    let mut two = two();
    adopt(&mut two.session, two.in_a[1], "a2", 1);
    let pending = two
        .session
        .register_load(LoadTarget::Queue(two.in_b[0]), &media("b1"))
        .expect("registered");
    assert_eq!(
        two.session.state().playing(),
        two.a,
        "a request alone changes nothing (P4)"
    );

    two.session
        .observe(&loaded(pending, 2, "b1"), FakeClock::new().sample());
    let state = two.session.state();
    assert_eq!(state.playing(), two.b);
    assert_eq!(state.queue().active(), Some(two.in_b[0]));
    assert_eq!(
        state.playlist(two.a).expect("A").queue().active(),
        Some(two.in_a[1])
    );
}

#[test]
fn a_failed_cross_playlist_load_changes_neither_playing_nor_any_cursor() {
    let mut two = two();
    adopt(&mut two.session, two.in_a[0], "a1", 1);
    let pending = two
        .session
        .register_load(LoadTarget::Queue(two.in_b[0]), &media("b1"))
        .expect("registered");
    two.session.retract_load(pending);
    let state = two.session.state();
    assert_eq!(state.playing(), two.a);
    assert_eq!(state.queue().active(), Some(two.in_a[0]));
    assert_eq!(state.playlist(two.b).expect("B").queue().active(), None);
}

#[test]
fn automatic_advance_follows_the_same_shuffled_order_as_neighbor() {
    let mut two = two();
    let (more, _) = two
        .session
        .enqueue(two.a, vec![entry("a3"), entry("a4"), entry("a5")])
        .expect("fits");
    let all = [two.in_a[0], two.in_a[1], more[0], more[1], more[2]];
    adopt(&mut two.session, all[2], "a3", 1);
    // Seed 42 (the brief's default) happens to put this fixture's a4 right
    // after the pinned a3 — indistinguishable from list order, which is the
    // one thing this test must rule out. 99 does not.
    two.session.set_shuffle(two.a, Some(99)).expect("A exists");

    let playlist = two.session.state().playlist(two.a).expect("A").clone();
    assert_eq!(
        playlist.shuffle(),
        Some(Shuffle {
            seed: 99,
            first: Some(all[2])
        })
    );
    let expected = playlist
        .neighbor(all[2], Direction::Down)
        .expect("the rest lies ahead of `first`");
    assert_ne!(
        expected, all[3],
        "the fixture must actually differ from list order"
    );

    two.session.observe(&end(1), FakeClock::new().sample());
    assert_eq!(two.session.take_advance(), Some(Advance::Next(expected)));
}

#[test]
fn shuffle_on_an_inactive_tab_pins_its_own_cursor_or_nothing() {
    let mut two = two();
    adopt(&mut two.session, two.in_a[0], "a1", 1);
    two.session.set_shuffle(two.b, Some(7)).expect("B exists");
    assert_eq!(
        two.session.state().playlist(two.b).expect("B").shuffle(),
        Some(Shuffle {
            seed: 7,
            first: None
        }),
        "never another playlist's track"
    );
    two.session.set_shuffle(two.b, None).expect("B exists");
    assert_eq!(
        two.session.state().playlist(two.b).expect("B").shuffle(),
        None
    );
    assert_eq!(
        two.session.adopted().map(|a| a.target),
        Some(LoadTarget::Queue(two.in_a[0])),
        "no release"
    );
}
