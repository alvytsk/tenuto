mod support;

use support::media;
use tenuto::media::id::MediaId;
use tenuto::playlist::{Playlist, PlaylistId, Shuffle, clean_name, splitmix64};
use tenuto::queue::{
    Direction, DisplayMetadata, IdAllocator, NewQueueEntry, Queue, QueueEntryId, QueueError,
    QueueSource,
};

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

/// A playlist of five entries whose IDs are 1..=5.
fn five(shuffle: Option<Shuffle>) -> (Playlist, Vec<QueueEntryId>) {
    let mut queue = Queue::default();
    let mut ids_alloc = IdAllocator::default();
    let ids = queue
        .enqueue(
            ["a", "b", "c", "d", "e"].map(entry).to_vec(),
            &mut ids_alloc,
        )
        .unwrap_or_else(|error| panic!("fits: {error}"));
    let playlist = Playlist::from_parts(
        PlaylistId::from_raw_for_tests(1),
        "P".into(),
        shuffle,
        queue,
    );
    (playlist, ids)
}

#[test]
fn splitmix64_matches_the_reference_vector() {
    assert_eq!(splitmix64(0), 0xE220_A839_7B1D_CDAF);
    assert_eq!(splitmix64(0x9E37_79B9_7F4A_7C15), 0x6E78_9E6A_A1B9_65F4);
}

#[test]
fn list_order_is_used_when_shuffle_is_off() {
    let (playlist, ids) = five(None);
    assert_eq!(playlist.playback_order(), ids);
    assert_eq!(playlist.neighbor(ids[0], Direction::Down), Some(ids[1]));
    assert_eq!(playlist.neighbor(ids[0], Direction::Up), None);
    assert_eq!(playlist.neighbor(ids[4], Direction::Down), None);
    assert_eq!(playlist.first_in_order(), Some(ids[0]));
}

#[test]
fn seed_42_orders_ids_one_to_five_as_5_1_4_3_2() {
    // Pinned: sorted by (splitmix64(42 + id), id). Computed independently.
    let (playlist, ids) = five(Some(Shuffle {
        seed: 42,
        first: None,
    }));
    let expected = [ids[4], ids[0], ids[3], ids[2], ids[1]];
    assert_eq!(playlist.playback_order(), expected);
    assert_eq!(playlist.first_in_order(), Some(ids[4]));
    assert_eq!(playlist.neighbor(ids[4], Direction::Down), Some(ids[0]));
    assert_eq!(playlist.neighbor(ids[0], Direction::Up), Some(ids[4]));
    assert_eq!(
        playlist.neighbor(ids[1], Direction::Down),
        None,
        "no wrap at the end"
    );
    assert_eq!(
        playlist.neighbor(ids[4], Direction::Up),
        None,
        "no wrap at the start"
    );
}

#[test]
fn first_is_pinned_ahead_of_the_hashed_order_while_it_is_a_member() {
    let (playlist, ids) = five(Some(Shuffle {
        seed: 42,
        first: Some(ids_third()),
    }));
    assert_eq!(
        playlist.playback_order(),
        [ids[2], ids[4], ids[0], ids[3], ids[1]]
    );
}

fn ids_third() -> QueueEntryId {
    five(None).1[2]
}

#[test]
fn a_first_that_is_not_a_member_is_ignored() {
    let (mut playlist, ids) = five(Some(Shuffle {
        seed: 42,
        first: Some(ids_third()),
    }));
    playlist
        .queue_mut_for_tests()
        .remove(ids[2])
        .expect("queued");
    assert_eq!(playlist.playback_order(), [ids[4], ids[0], ids[3], ids[1]]);
}

#[test]
fn removing_or_moving_an_entry_leaves_the_others_relative_order_alone() {
    let (mut playlist, ids) = five(Some(Shuffle {
        seed: 42,
        first: None,
    }));
    playlist
        .queue_mut_for_tests()
        .move_entry(ids[0], Direction::Down)
        .expect("queued");
    assert_eq!(
        playlist.playback_order(),
        [ids[4], ids[0], ids[3], ids[2], ids[1]],
        "moving rows does not change the shuffled order"
    );
    playlist
        .queue_mut_for_tests()
        .remove(ids[3])
        .expect("queued");
    assert_eq!(playlist.playback_order(), [ids[4], ids[0], ids[2], ids[1]]);
}

#[test]
fn an_unknown_anchor_has_no_neighbor() {
    let (mut playlist, ids) = five(Some(Shuffle {
        seed: 42,
        first: None,
    }));
    playlist
        .queue_mut_for_tests()
        .remove(ids[0])
        .expect("queued");
    assert_eq!(playlist.neighbor(ids[0], Direction::Down), None);
}

#[test]
fn names_are_trimmed_truncated_to_forty_chars_and_never_empty() {
    assert_eq!(clean_name("  Morning  ").as_deref(), Some("Morning"));
    assert_eq!(clean_name("   "), None);
    let long = "é".repeat(50);
    assert_eq!(clean_name(&long).map(|name| name.chars().count()), Some(40));
}

#[test]
fn an_allocator_reserves_a_contiguous_range_or_nothing() {
    let mut ids = IdAllocator::default();
    assert_eq!(ids.reserve(3).expect("room"), 1..=3);
    assert_eq!(ids.next(), Some(4));

    let mut last = IdAllocator::starting_at(Some(u64::MAX));
    assert_eq!(last.reserve(2), Err(QueueError::IdExhausted));
    assert_eq!(
        last.next(),
        Some(u64::MAX),
        "a refused batch changes nothing"
    );
    assert_eq!(last.reserve(1).expect("the last ID"), u64::MAX..=u64::MAX);
    assert_eq!(
        last.next(),
        None,
        "handing out u64::MAX exhausts the namespace"
    );
    assert_eq!(last.reserve(1), Err(QueueError::IdExhausted));
}

#[test]
fn observing_an_id_never_lowers_the_counter_and_max_exhausts_it() {
    let mut ids = IdAllocator::default();
    ids.observe(9);
    assert_eq!(ids.next(), Some(10));
    ids.observe(3);
    assert_eq!(ids.next(), Some(10));
    ids.observe(u64::MAX);
    assert_eq!(ids.next(), None);
}

#[test]
fn two_queues_sharing_an_allocator_never_share_an_id() {
    let mut ids = IdAllocator::default();
    let (mut a, mut b) = (Queue::default(), Queue::default());
    let first = a
        .enqueue(vec![entry("a"), entry("b")], &mut ids)
        .expect("ids");
    let second = b.enqueue(vec![entry("a")], &mut ids).expect("ids");
    assert_eq!(first.iter().map(|id| id.get()).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(second[0].get(), 3);
}
