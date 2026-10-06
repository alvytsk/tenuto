//! The playlist set's interface (M9.1 spec §3, §5): every playlist rule,
//! tested where it lives. Replaces m5_queue, m8_playlist and the
//! playlist-only tests of m8_session_playlists.

mod support;

use support::media;
use tenuto::media::id::{EpisodeKey, FeedId, MediaId};
use tenuto::playlist::{
    Deletion, Field, MAX_PLAYLISTS, MediaEffect, PlaylistError, PlaylistId, PlaylistSet,
    RecordParts, Repair, Shuffle, ShuffleField, Stage, clean_name,
};
use tenuto::queue::{
    Direction, DisplayMetadata, DisplayUpdate, IdAllocator, MAX_PLAYLIST_ENTRIES, NewQueueEntry,
    QueueEntryId, QueueError, QueueSource,
};
use url::Url;

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

fn many(prefix: &str, count: usize) -> Vec<NewQueueEntry> {
    (0..count).map(|i| entry(&format!("{prefix}{i}"))).collect()
}

fn url(raw: &str) -> Url {
    raw.parse()
        .unwrap_or_else(|error| panic!("a literal URL must parse: {error}"))
}

fn episode() -> NewQueueEntry {
    let feed = FeedId::new("f".repeat(32)).unwrap_or_else(|error| panic!("feed: {error}"));
    let key = EpisodeKey::resolve(Some("guid-1"), None, None)
        .unwrap_or_else(|error| panic!("episode: {error}"));
    NewQueueEntry::new(
        MediaId::PodcastEpisode { feed, episode: key },
        QueueSource::Podcast {
            fallback: url("https://podcasts.example.org/1.mp3"),
        },
        DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("valid: {error}"))
}

fn add(set: &mut PlaylistSet, dest: PlaylistId, batch: Vec<NewQueueEntry>) -> Vec<QueueEntryId> {
    set.enqueue(dest, batch)
        .unwrap_or_else(|error| panic!("fits: {error}"))
}

fn create(set: &mut PlaylistSet, name: &str) -> PlaylistId {
    set.create(name)
        .unwrap_or_else(|error| panic!("room: {error}"))
}

fn cursor(set: &PlaylistSet, id: PlaylistId) -> Option<QueueEntryId> {
    set.playlist(id).and_then(|p| p.queue().active())
}

/// Five entries a..e (IDs 1..=5) in the default playlist, the cursor on
/// `at` if given, then shuffle with `seed` — which pins that cursor.
fn five(at: Option<usize>, seed: Option<u64>) -> (PlaylistSet, Vec<QueueEntryId>) {
    let mut set = PlaylistSet::default();
    let playing = set.playing();
    let ids = add(
        &mut set,
        playing,
        ["a", "b", "c", "d", "e"].map(entry).to_vec(),
    );
    if let Some(index) = at {
        set.adopt(ids[index])
            .unwrap_or_else(|error| panic!("queued: {error}"));
    }
    set.set_shuffle(playing, seed)
        .unwrap_or_else(|error| panic!("exists: {error}"));
    (set, ids)
}

// ---- the fresh set, IDs and the cap (S1, S2, S5) ----

#[test]
fn a_fresh_set_has_one_playing_playlist_named_default() {
    let set = PlaylistSet::default();
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].name(), "Default");
    assert_eq!(set.playing(), set[0].id());
    assert_eq!(set.playing().get(), 1);
    assert_eq!(set.total_entries(), 0);
}

#[test]
fn duplicate_media_gets_distinct_entry_ids() {
    let mut set = PlaylistSet::default();
    let playing = set.playing();
    let ids = add(&mut set, playing, vec![entry("a"), entry("a")]);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        set.find_entry(ids[0]).map(|e| e.media()),
        set.find_entry(ids[1]).map(|e| e.media())
    );
}

#[test]
fn a_mismatched_source_is_refused_at_construction() {
    let MediaId::LocalFile(other) = media("b") else {
        unreachable!()
    };
    let result = NewQueueEntry::new(
        media("a"),
        QueueSource::LocalFile(other),
        DisplayMetadata::default(),
    );
    assert!(matches!(result, Err(QueueError::SourceMismatch)));
}

#[test]
fn entry_ids_are_unique_across_playlists_and_owner_lookup_finds_them() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let b = create(&mut set, "B");
    let in_a = add(&mut set, a, vec![entry("x"), entry("y")]);
    let in_b = add(&mut set, b, vec![entry("x")]);
    assert_eq!(
        [in_a[0].get(), in_a[1].get(), in_b[0].get()],
        [1, 2, 3],
        "one allocator for every playlist"
    );
    assert_eq!(set.owner_of(in_a[1]), Some(a));
    assert_eq!(set.owner_of(in_b[0]), Some(b));
    assert_eq!(
        set.playing_playlist().queue().len(),
        2,
        "B's adds stay in B"
    );
}

#[test]
fn enqueue_into_an_unknown_playlist_is_refused_even_when_empty() {
    let mut set = PlaylistSet::default();
    let gone = PlaylistId::from_raw_for_tests(99);
    assert_eq!(
        set.enqueue(gone, vec![entry("x")]),
        Err(QueueError::UnknownPlaylist(99))
    );
    assert_eq!(
        set.enqueue(gone, Vec::new()),
        Err(QueueError::UnknownPlaylist(99))
    );
}

#[test]
fn the_cap_is_global_and_a_refused_batch_changes_nothing() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let b = create(&mut set, "B");
    add(&mut set, a, many("x", MAX_PLAYLIST_ENTRIES - 1));
    let before = set.clone();
    assert_eq!(
        set.enqueue(b, vec![entry("y"), entry("z")]),
        Err(QueueError::Capacity {
            requested: 2,
            available: 1
        })
    );
    assert_eq!(set, before, "all or nothing (S8)");
}

#[test]
fn ids_are_never_reused_after_remove_clear_or_delete() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let first = add(&mut set, a, vec![entry("a")])[0];
    set.remove_entry(first)
        .unwrap_or_else(|error| panic!("queued: {error}"));
    let second = add(&mut set, a, vec![entry("a")])[0];
    set.clear(a)
        .unwrap_or_else(|error| panic!("exists: {error}"));
    let b = create(&mut set, "B");
    let third = add(&mut set, b, vec![entry("a")])[0];
    set.delete(b)
        .unwrap_or_else(|error| panic!("another exists: {error}"));
    let fourth = add(&mut set, a, vec![entry("a")])[0];
    assert!(first < second && second < third && third < fourth);
}

#[test]
fn playlist_ids_are_never_reused() {
    let mut set = PlaylistSet::default();
    let b = create(&mut set, "B");
    set.delete(b)
        .unwrap_or_else(|error| panic!("another exists: {error}"));
    let c = create(&mut set, "C");
    assert!(c > b);
}

#[test]
fn limits_and_names() {
    let mut set = PlaylistSet::default();
    assert_eq!(set.create("   "), Err(PlaylistError::InvalidName));
    for i in 1..MAX_PLAYLISTS {
        create(&mut set, &format!("P{i}"));
    }
    assert_eq!(set.create("one too many"), Err(PlaylistError::TooMany));
    let named = set[1].id();
    set.rename(named, "  Morning  ")
        .unwrap_or_else(|error| panic!("exists: {error}"));
    assert_eq!(set.playlist(named).map(|p| p.name()), Some("Morning"));
    assert_eq!(set.rename(named, " "), Err(PlaylistError::InvalidName));
    let gone = PlaylistId::from_raw_for_tests(999);
    assert_eq!(set.rename(gone, "x"), Err(PlaylistError::Unknown(gone)));
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
    assert_eq!(ids.reserve(3).unwrap_or_else(|e| panic!("{e}")), 1..=3);
    assert_eq!(ids.next(), Some(4));
    let mut last = IdAllocator::starting_at(Some(u64::MAX));
    assert_eq!(last.reserve(2), Err(QueueError::IdExhausted));
    assert_eq!(
        last.next(),
        Some(u64::MAX),
        "a refused batch changes nothing"
    );
    assert_eq!(
        last.reserve(1).unwrap_or_else(|e| panic!("{e}")),
        u64::MAX..=u64::MAX
    );
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

// ---- editing entries ----

#[test]
fn reordering_keeps_ids_and_stops_at_the_edges() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let ids = add(&mut set, a, many("r", 3));
    assert_eq!(set.move_entry(ids[2], Direction::Up), Ok(true));
    assert_eq!(
        set.playing_playlist()
            .queue()
            .entries()
            .iter()
            .map(|e| e.id())
            .collect::<Vec<_>>(),
        [ids[0], ids[2], ids[1]]
    );
    assert_eq!(set.move_entry(ids[0], Direction::Up), Ok(false));
    set.remove_entry(ids[1])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(
        set.move_entry(ids[1], Direction::Down),
        Err(QueueError::UnknownEntry(ids[1]))
    );
}

#[test]
fn removal_selects_the_successor_or_the_predecessor_and_clears_its_cursor() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let ids = add(&mut set, a, many("s", 3));
    set.adopt(ids[1])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    let removed = set
        .remove_entry(ids[1])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(removed.selection, Some(ids[2]));
    assert!(removed.was_active);
    assert_eq!(cursor(&set, a), None);
    let removed = set
        .remove_entry(ids[2])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(removed.selection, Some(ids[0]));
    let removed = set
        .remove_entry(ids[0])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(removed.selection, None);
}

#[test]
fn clear_empties_one_playlist_and_keeps_it() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let b = create(&mut set, "B");
    let in_a = add(&mut set, a, vec![entry("a")]);
    add(&mut set, b, vec![entry("b")]);
    set.adopt(in_a[0])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    set.clear(a)
        .unwrap_or_else(|error| panic!("exists: {error}"));
    assert!(set.playlist(a).is_some_and(|p| p.queue().is_empty()));
    assert_eq!(cursor(&set, a), None);
    assert_eq!(set.playlist(b).map(|p| p.queue().len()), Some(1));
    let gone = PlaylistId::from_raw_for_tests(99);
    assert_eq!(set.clear(gone), Err(PlaylistError::Unknown(gone)));
}

// ---- delete and the successor (S7) ----

#[test]
fn every_delete_returns_the_next_playlist_or_the_previous_one_at_the_end() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let b = create(&mut set, "B");
    let c = create(&mut set, "C");
    assert_eq!(
        set.delete(b),
        Ok(Deletion {
            successor: c,
            current_media: MediaEffect::Unchanged
        }),
        "a non-playing delete still names its successor"
    );
    assert_eq!(
        set.delete(c),
        Ok(Deletion {
            successor: a,
            current_media: MediaEffect::Unchanged
        }),
        "the last one's successor is the one before it"
    );
    assert_eq!(set.playing(), a);
}

#[test]
fn deleting_the_playing_playlist_moves_playing_and_reports_the_successors_cursor() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let b = create(&mut set, "B");
    let in_a = add(&mut set, a, vec![entry("old")]);
    let in_b = add(&mut set, b, vec![entry("kept")]);
    set.adopt(in_b[0])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    set.adopt(in_a[0])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(set.playing(), a);
    assert_eq!(
        set.delete(a),
        Ok(Deletion {
            successor: b,
            current_media: MediaEffect::Set(Some(media("kept")))
        })
    );
    assert_eq!(set.playing(), b);
    assert_eq!(cursor(&set, b), Some(in_b[0]));
}

#[test]
fn deleting_the_playing_playlist_reports_none_when_the_successor_has_no_cursor() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    create(&mut set, "B");
    let deletion = set
        .delete(a)
        .unwrap_or_else(|error| panic!("another exists: {error}"));
    assert_eq!(deletion.current_media, MediaEffect::Set(None));
    assert_eq!(MediaEffect::Set(None).apply(Some(media("x"))), None);
    assert_eq!(
        MediaEffect::Unchanged.apply(Some(media("x"))),
        Some(media("x"))
    );
}

#[test]
fn a_refused_delete_changes_nothing() {
    let mut set = PlaylistSet::default();
    let only = set.playing();
    let before = set.clone();
    assert_eq!(set.check_delete(only), Err(PlaylistError::LastPlaylist));
    assert_eq!(set.delete(only), Err(PlaylistError::LastPlaylist));
    let gone = PlaylistId::from_raw_for_tests(99);
    assert_eq!(set.check_delete(gone), Err(PlaylistError::Unknown(gone)));
    assert_eq!(set.delete(gone), Err(PlaylistError::Unknown(gone)));
    assert_eq!(set, before);
}

// ---- adoption and cursors (S3, S4) ----

#[test]
fn adopting_an_entry_makes_its_owner_playing_and_sets_that_cursor_only() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let b = create(&mut set, "B");
    let in_a = add(&mut set, a, vec![entry("a1")]);
    let in_b = add(&mut set, b, vec![entry("b1")]);
    set.adopt(in_a[0])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    set.adopt(in_b[0])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(set.playing(), b);
    assert_eq!(cursor(&set, b), Some(in_b[0]));
    assert_eq!(
        cursor(&set, a),
        Some(in_a[0]),
        "the old playlist keeps its cursor"
    );
}

#[test]
fn a_failed_adoption_changes_neither_playing_nor_any_cursor() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let ids = add(&mut set, a, vec![entry("a1"), entry("a2")]);
    set.adopt(ids[0])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    set.remove_entry(ids[1])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    let before = set.clone();
    assert_eq!(set.adopt(ids[1]), Err(QueueError::UnknownEntry(ids[1])));
    assert_eq!(set, before);
}

// ---- shuffle and playback order (S6) ----

#[test]
fn list_order_is_used_when_shuffle_is_off_and_neighbors_do_not_wrap() {
    let (set, ids) = five(None, None);
    let playlist = set.playing_playlist();
    assert_eq!(playlist.playback_order(), ids);
    assert_eq!(playlist.neighbor(ids[0], Direction::Down), Some(ids[1]));
    assert_eq!(playlist.neighbor(ids[0], Direction::Up), None);
    assert_eq!(playlist.neighbor(ids[4], Direction::Down), None);
    assert_eq!(playlist.first_in_order(), Some(ids[0]));
}

#[test]
fn seed_42_orders_ids_one_to_five_as_5_1_4_3_2() {
    // Pinned: sorted by (splitmix64(42 + id), id). Computed independently.
    let (set, ids) = five(None, Some(42));
    let playlist = set.playing_playlist();
    assert_eq!(
        playlist.shuffle(),
        Some(Shuffle {
            seed: 42,
            first: None
        })
    );
    assert_eq!(
        playlist.playback_order(),
        [ids[4], ids[0], ids[3], ids[2], ids[1]]
    );
    assert_eq!(playlist.neighbor(ids[4], Direction::Down), Some(ids[0]));
    assert_eq!(playlist.neighbor(ids[1], Direction::Down), None, "no wrap");
    assert_eq!(playlist.neighbor(ids[4], Direction::Up), None, "no wrap");
}

#[test]
fn turning_shuffle_on_pins_the_playlists_own_cursor_first() {
    let (set, ids) = five(Some(2), Some(42));
    assert_eq!(
        set.playing_playlist().playback_order(),
        [ids[2], ids[4], ids[0], ids[3], ids[1]]
    );
}

#[test]
fn shuffle_on_a_playlist_without_a_cursor_pins_nothing() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let b = create(&mut set, "B");
    let in_a = add(&mut set, a, vec![entry("a1")]);
    add(&mut set, b, vec![entry("b1")]);
    set.adopt(in_a[0])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    set.set_shuffle(b, Some(7))
        .unwrap_or_else(|error| panic!("exists: {error}"));
    assert_eq!(
        set.playlist(b).and_then(|p| p.shuffle()),
        Some(Shuffle {
            seed: 7,
            first: None
        }),
        "never another playlist's cursor"
    );
    set.set_shuffle(b, None)
        .unwrap_or_else(|error| panic!("exists: {error}"));
    assert_eq!(set.playlist(b).and_then(|p| p.shuffle()), None);
}

#[test]
fn a_pin_that_is_no_longer_a_member_is_ignored() {
    let (mut set, ids) = five(Some(2), Some(42));
    set.remove_entry(ids[2])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(
        set.playing_playlist().playback_order(),
        [ids[4], ids[0], ids[3], ids[1]]
    );
}

#[test]
fn removing_or_moving_an_entry_leaves_the_others_relative_order_alone() {
    let (mut set, ids) = five(None, Some(42));
    set.move_entry(ids[0], Direction::Down)
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(
        set.playing_playlist().playback_order(),
        [ids[4], ids[0], ids[3], ids[2], ids[1]],
        "moving rows does not change the shuffled order"
    );
    set.remove_entry(ids[3])
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(
        set.playing_playlist().playback_order(),
        [ids[4], ids[0], ids[2], ids[1]]
    );
    assert_eq!(
        set.playing_playlist().neighbor(ids[3], Direction::Down),
        None
    );
}

#[test]
fn adding_to_a_shuffled_playlist_reshuffles_with_the_cursor_first() {
    let (mut set, ids) = five(Some(1), Some(99));
    let playing = set.playing();
    let added = add(&mut set, playing, many("new", 20));
    let playlist = set.playing_playlist();
    let shuffle = playlist
        .shuffle()
        .unwrap_or_else(|| panic!("still shuffled"));
    assert_ne!(shuffle.seed, 99, "a new order, not the old one patched");
    assert_eq!(shuffle.first, Some(ids[1]));
    let order = playlist.playback_order();
    assert_eq!(order[0], ids[1]);
    assert!(added.iter().all(|id| order[1..].contains(id)));
}

#[test]
fn adding_to_a_shuffled_playlist_without_a_cursor_reshuffles_with_nothing_pinned() {
    let (mut set, _) = five(None, Some(99));
    let playing = set.playing();
    let added = add(&mut set, playing, many("new", 3));
    let playlist = set.playing_playlist();
    let shuffle = playlist
        .shuffle()
        .unwrap_or_else(|| panic!("still shuffled"));
    assert_ne!(shuffle.seed, 99);
    assert_eq!(shuffle.first, None);
    let order = playlist.playback_order();
    assert!(added.iter().all(|id| order.contains(id)));
}

#[test]
fn an_empty_batch_changes_nothing_even_in_a_shuffled_playlist() {
    let (mut set, _) = five(Some(0), Some(5));
    let playing = set.playing();
    let before = set.clone();
    assert_eq!(set.enqueue(playing, Vec::new()), Ok(Vec::new()));
    assert_eq!(set, before, "no reshuffle and no IDs");
}

#[test]
fn an_unshuffled_playlist_stays_unshuffled_on_enqueue() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    add(&mut set, a, vec![entry("plain")]);
    assert_eq!(set.playing_playlist().shuffle(), None);
}

// ---- entry updates ----

#[test]
fn an_update_leaves_absent_fields_alone_and_reports_only_real_changes() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let id = add(&mut set, a, vec![entry("a")])[0];
    let full = DisplayUpdate {
        title: Some("Title".into()),
        artist: Some("Artist".into()),
        ..DisplayUpdate::default()
    };
    assert_eq!(set.update_display(id, &full), Ok(true));
    assert_eq!(
        set.update_display(id, &full),
        Ok(false),
        "same values: no change"
    );
    let partial = DisplayUpdate {
        album: Some("Album".into()),
        ..DisplayUpdate::default()
    };
    assert_eq!(set.update_display(id, &partial), Ok(true));
    let display = set
        .find_entry(id)
        .map(|e| e.display().clone())
        .unwrap_or_default();
    assert_eq!(display.title.as_deref(), Some("Title"), "absent title kept");
    assert_eq!(display.artist.as_deref(), Some("Artist"));
    assert_eq!(display.album.as_deref(), Some("Album"));
    assert_eq!(set.update_display(id, &DisplayUpdate::default()), Ok(false));
    set.remove_entry(id)
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(
        set.update_display(id, &full),
        Err(QueueError::UnknownEntry(id))
    );
}

#[test]
fn a_podcast_fallback_changes_only_when_it_differs_and_only_on_a_podcast() {
    let mut set = PlaylistSet::default();
    let a = set.playing();
    let ids = add(&mut set, a, vec![episode(), entry("local")]);
    let same = url("https://podcasts.example.org/1.mp3");
    let new = url("https://cdn.example.org/1.mp3");
    assert_eq!(set.set_podcast_fallback(ids[0], same), Ok(false));
    assert_eq!(set.set_podcast_fallback(ids[0], new.clone()), Ok(true));
    assert_eq!(
        set.find_entry(ids[0]).map(|e| e.source().clone()),
        Some(QueueSource::Podcast {
            fallback: new.clone()
        })
    );
    assert_eq!(
        set.set_podcast_fallback(ids[1], new),
        Err(QueueError::SourceMismatch)
    );
}

// ---- recovery (spec §8.3) ----

fn record(id: Option<u64>, entries: &[(u64, &str)]) -> RecordParts {
    RecordParts {
        id,
        entries: entries
            .iter()
            .map(|(raw, name)| (*raw, entry(name)))
            .collect(),
        cursor: Field::Absent,
        shuffle: ShuffleField::Absent,
        name: Some("P".into()),
    }
}

fn entry_ids_of(set: &PlaylistSet) -> Vec<u64> {
    set.iter()
        .flat_map(|p| p.queue().entries())
        .map(|e| e.id().get())
        .collect()
}

#[test]
fn records_keep_their_ids_names_and_cursor() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    let mut parts = record(Some(4), &[(10, "a"), (11, "b")]);
    parts.cursor = Field::Value(11);
    parts.name = Some("  Morning  ".into());
    let outcome = recovery.push_record(parts);
    assert_eq!(outcome.kept, Some(PlaylistId::from_raw_for_tests(4)));
    assert!(outcome.repairs.is_empty());
    let done = recovery.finish(Some(4), Some(media("b")));
    assert!(done.repairs.is_empty());
    assert_eq!(done.current_media, MediaEffect::Unchanged);
    assert_eq!(done.set.playing().get(), 4);
    assert_eq!(done.set[0].name(), "Morning");
    assert_eq!(done.set[0].queue().active().map(|id| id.get()), Some(11));
    assert_eq!(entry_ids_of(&done.set), [10, 11]);
}

#[test]
fn default_counters_never_reissue_a_kept_id() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    recovery.push_record(record(Some(1), &[(1, "a")]));
    let mut set = recovery.finish(Some(1), None).set;
    assert_ne!(create(&mut set, "new").get(), 1);
    let playing = set.playing();
    assert_ne!(add(&mut set, playing, vec![entry("b")])[0].get(), 1);
}

#[test]
fn a_raw_playlist_id_equal_to_a_minted_one_is_reassigned() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    let first = recovery.push_record(record(None, &[]));
    assert_eq!(first.kept.map(PlaylistId::get), Some(1));
    assert_eq!(first.repairs, [(Stage::Id, Repair::DuplicatePlaylistId)]);
    let second = recovery.push_record(record(Some(1), &[]));
    assert_eq!(second.kept.map(PlaylistId::get), Some(2));
    assert_eq!(second.repairs, [(Stage::Id, Repair::DuplicatePlaylistId)]);
}

#[test]
fn a_raw_entry_id_equal_to_a_minted_one_is_reassigned() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    recovery.push_record(record(Some(1), &[(5, "a")]));
    let second = recovery.push_record(record(Some(2), &[(5, "b")]));
    assert_eq!(second.repairs, [(Stage::Entries, Repair::DuplicateEntryId)]);
    let third = recovery.push_record(record(Some(3), &[(6, "c")]));
    assert_eq!(
        third.repairs,
        [(Stage::Entries, Repair::DuplicateEntryId)],
        "6 was minted for b"
    );
    let set = recovery.finish(Some(1), None).set;
    assert_eq!(entry_ids_of(&set), [5, 6, 7]);
}

#[test]
fn counters_below_a_kept_id_are_raised_past_it() {
    let mut recovery =
        PlaylistSet::recovery(IdAllocator::default(), IdAllocator::starting_at(Some(2)));
    recovery.push_record(record(Some(7), &[]));
    let minted = recovery.push_record(record(None, &[]));
    assert_eq!(minted.kept.map(PlaylistId::get), Some(8));
    let mut set = recovery.finish(Some(7), None).set;
    assert_eq!(create(&mut set, "live").get(), 9);
}

#[test]
fn an_exhausted_counter_stays_exhausted_after_recovery() {
    let mut recovery = PlaylistSet::recovery(
        IdAllocator::starting_at(None),
        IdAllocator::starting_at(None),
    );
    recovery.push_record(record(Some(3), &[(4, "a")]));
    let mut set = recovery.finish(Some(3), None).set;
    assert_eq!(set.create("new"), Err(PlaylistError::IdExhausted));
    let playing = set.playing();
    assert_eq!(
        set.enqueue(playing, vec![entry("b")]),
        Err(QueueError::IdExhausted)
    );
}

#[test]
fn the_playlist_cap_counts_input_records_not_survivors() {
    let mut recovery =
        PlaylistSet::recovery(IdAllocator::default(), IdAllocator::starting_at(None));
    let mut outcomes = Vec::new();
    for i in 1..=33u64 {
        let id = (i != 5).then_some(i);
        outcomes.push(recovery.push_record(record(id, &[])));
    }
    assert_eq!(outcomes[4].kept, None);
    assert_eq!(outcomes[4].repairs, [(Stage::Id, Repair::IdsExhausted)]);
    assert_eq!(outcomes[32].kept, None);
    assert_eq!(
        outcomes[32].repairs,
        [(Stage::Count, Repair::TooManyPlaylists)]
    );
    let set = recovery.finish(Some(1), None).set;
    let expected: Vec<u64> = (1..=4).chain(6..=32).collect();
    assert_eq!(
        set.iter().map(|p| p.id().get()).collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn the_entry_cap_is_checked_before_duplicates_and_a_dropped_entry_costs_no_id() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    let full: Vec<(u64, String)> = (1..=MAX_PLAYLIST_ENTRIES as u64)
        .map(|i| (i, format!("t{i}")))
        .collect();
    let full: Vec<(u64, &str)> = full.iter().map(|(i, n)| (*i, n.as_str())).collect();
    recovery.push_record(record(Some(1), &full));
    let second = recovery.push_record(record(Some(2), &[(1, "dup"), (2, "dup2")]));
    assert_eq!(
        second.repairs,
        [(Stage::Entries, Repair::OverCapacity)],
        "reported once, and never as a duplicate"
    );
    let mut set = recovery.finish(Some(1), None).set;
    let playing = set.playing();
    let removed = set.playing_playlist().queue().entries()[0].id();
    set.remove_entry(removed)
        .unwrap_or_else(|error| panic!("queued: {error}"));
    assert_eq!(
        add(&mut set, playing, vec![entry("next")])[0].get(),
        MAX_PLAYLIST_ENTRIES as u64 + 1,
        "no fresh ID went to a dropped entry"
    );
}

#[test]
fn a_reassigned_cursor_reports_the_duplicate_then_the_dangling_cursor() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    recovery.push_record(record(Some(1), &[(1, "a")]));
    let mut parts = record(Some(2), &[(1, "b")]);
    parts.cursor = Field::Value(1);
    let outcome = recovery.push_record(parts);
    assert_eq!(
        outcome.repairs,
        [
            (Stage::Entries, Repair::DuplicateEntryId),
            (Stage::Cursor, Repair::DanglingCursor)
        ]
    );
    let set = recovery.finish(Some(1), None).set;
    assert_eq!(set[1].queue().active(), None);
}

#[test]
fn a_malformed_or_foreign_cursor_is_dangling_and_an_absent_one_is_silent() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    let mut malformed = record(Some(1), &[(1, "a")]);
    malformed.cursor = Field::Malformed;
    assert_eq!(
        recovery.push_record(malformed).repairs,
        [(Stage::Cursor, Repair::DanglingCursor)]
    );
    let mut foreign = record(Some(2), &[(2, "b")]);
    foreign.cursor = Field::Value(99);
    assert_eq!(
        recovery.push_record(foreign).repairs,
        [(Stage::Cursor, Repair::DanglingCursor)]
    );
    assert!(
        recovery
            .push_record(record(Some(3), &[(3, "c")]))
            .repairs
            .is_empty()
    );
}

#[test]
fn shuffle_damage_is_graded() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    let mut bad_seed = record(Some(1), &[(1, "a")]);
    bad_seed.shuffle = ShuffleField::BadSeed;
    assert_eq!(
        recovery.push_record(bad_seed).repairs,
        [(Stage::Shuffle, Repair::Shuffle)]
    );
    let mut bad_first = record(Some(2), &[(2, "b")]);
    bad_first.shuffle = ShuffleField::Seeded {
        seed: 7,
        first: Field::Malformed,
    };
    assert_eq!(
        recovery.push_record(bad_first).repairs,
        [(Stage::Shuffle, Repair::Shuffle)]
    );
    let mut foreign_first = record(Some(3), &[(3, "c")]);
    foreign_first.shuffle = ShuffleField::Seeded {
        seed: 8,
        first: Field::Value(99),
    };
    assert!(
        recovery.push_record(foreign_first).repairs.is_empty(),
        "silent"
    );
    let mut member_first = record(Some(4), &[(4, "d")]);
    member_first.shuffle = ShuffleField::Seeded {
        seed: 9,
        first: Field::Value(4),
    };
    assert!(recovery.push_record(member_first).repairs.is_empty());
    let set = recovery.finish(Some(1), None).set;
    let shuffles: Vec<Option<Shuffle>> = set.iter().map(|p| p.shuffle()).collect();
    assert_eq!(
        shuffles,
        [
            None,
            Some(Shuffle {
                seed: 7,
                first: None
            }),
            Some(Shuffle {
                seed: 8,
                first: None
            }),
            Some(Shuffle {
                seed: 9,
                first: set[3].queue().first()
            }),
        ]
    );
}

#[test]
fn a_missing_or_blank_name_is_named_after_the_resolved_id() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    let mut blank = record(Some(3), &[]);
    blank.name = Some("   ".into());
    recovery.push_record(blank);
    let mut missing = record(Some(3), &[]);
    missing.name = None;
    recovery.push_record(missing);
    let set = recovery.finish(Some(3), None).set;
    assert_eq!(set[0].name(), "Playlist 3");
    assert_eq!(set[1].name(), "Playlist 4", "the reassigned ID names it");
}

#[test]
fn no_surviving_record_gives_one_empty_default_with_a_fresh_id() {
    let recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::starting_at(Some(9)));
    let done = recovery.finish(Some(1), Some(media("a")));
    assert_eq!(done.set.len(), 1);
    assert_eq!(done.set[0].name(), "Default");
    assert_eq!(done.set.playing().get(), 9);
    assert_eq!(done.current_media, MediaEffect::Unchanged);
    assert!(done.repairs.is_empty());

    let exhausted = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::starting_at(None));
    assert_eq!(exhausted.finish(None, None).set.playing().get(), 1);
}

#[test]
fn a_dangling_playing_falls_back_to_the_first_and_moves_the_persisted_media() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    let mut parts = record(Some(1), &[(1, "a")]);
    parts.cursor = Field::Value(1);
    recovery.push_record(parts);
    let done = recovery.finish(Some(42), Some(media("other")));
    assert_eq!(done.repairs, [Repair::DanglingPlaying]);
    assert_eq!(done.set.playing().get(), 1);
    assert_eq!(done.current_media, MediaEffect::Set(Some(media("a"))));
    assert_eq!(done.set[0].queue().active().map(|id| id.get()), Some(1));
}

#[test]
fn a_playing_cursor_on_other_media_is_cleared_and_the_media_is_unchanged() {
    let mut recovery = PlaylistSet::recovery(IdAllocator::default(), IdAllocator::default());
    let mut playing = record(Some(1), &[(1, "a")]);
    playing.cursor = Field::Value(1);
    recovery.push_record(playing);
    let mut other = record(Some(2), &[(2, "b")]);
    other.cursor = Field::Value(2);
    recovery.push_record(other);
    let done = recovery.finish(Some(1), Some(media("x")));
    assert_eq!(done.repairs, [Repair::CursorMediaMismatch]);
    assert_eq!(done.current_media, MediaEffect::Unchanged);
    assert_eq!(done.set[0].queue().active(), None);
    assert_eq!(
        done.set[1].queue().active().map(|id| id.get()),
        Some(2),
        "only the playing cursor is judged against the media"
    );
}
