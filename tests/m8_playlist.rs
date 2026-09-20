mod support;

use support::media;
use tenuto::media::id::MediaId;
use tenuto::playlist::{Playlist, PlaylistId, Shuffle, clean_name, splitmix64};
use tenuto::queue::{Direction, DisplayMetadata, NewQueueEntry, Queue, QueueEntryId, QueueSource};

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
    let ids = queue
        .enqueue(["a", "b", "c", "d", "e"].map(entry).to_vec())
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
