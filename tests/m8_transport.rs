mod support;

use support::media;
use tenuto::application::transport::*;
use tenuto::media::id::MediaId;
use tenuto::playlist::{Playlist, PlaylistId, PlaylistSet};
use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueEntryId, QueueSource};

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

/// Adds a playlist of `names` to `set`. Shuffle (seed `seed`, nothing
/// pinned) goes on before the cursor, so `first` stays `None`; the cursor
/// then goes on `cursor` (an index).
fn playlist(
    set: &mut PlaylistSet,
    names: &[&str],
    cursor: Option<usize>,
    seed: Option<u64>,
) -> (PlaylistId, Vec<QueueEntryId>) {
    let id = set
        .create("P")
        .unwrap_or_else(|error| panic!("room: {error}"));
    let entries = set
        .enqueue(id, names.iter().map(|name| entry(name)).collect())
        .unwrap_or_else(|error| panic!("fits: {error}"));
    set.set_shuffle(id, seed)
        .unwrap_or_else(|error| panic!("exists: {error}"));
    if let Some(index) = cursor {
        set.adopt(entries[index])
            .unwrap_or_else(|error| panic!("queued: {error}"));
    }
    (id, entries)
}

fn get(set: &PlaylistSet, id: PlaylistId) -> &Playlist {
    set.playlist(id)
        .unwrap_or_else(|| panic!("playlist {} exists", id.get()))
}

const ALL_PHASES: [PlaybackPhase; 8] = [
    PlaybackPhase::Unloaded,
    PlaybackPhase::Loading,
    PlaybackPhase::LoadFailed,
    PlaybackPhase::Playing,
    PlaybackPhase::Reconnecting,
    PlaybackPhase::Paused,
    PlaybackPhase::Stopped,
    PlaybackPhase::Ended,
];

#[test]
fn enter_plays_the_viewed_selection_in_every_phase_even_over_an_empty_playing_playlist() {
    let mut set = PlaylistSet::default();
    let (empty, _) = playlist(&mut set, &[], None, None);
    let (viewed, in_viewed) = playlist(&mut set, &["x", "y"], None, None);
    for phase in ALL_PHASES {
        let decision = decide(
            TransportInput::Enter,
            &TransportSituation {
                navigation: get(&set, empty),
                viewed: get(&set, viewed),
                selected: Some(in_viewed[1]),
                phase,
                retry: None,
                live: false,
            },
        );
        assert_eq!(decision, TransportDecision::Load(in_viewed[1]), "{phase:?}");
    }
}

#[test]
fn space_and_play_never_load_the_viewed_selection() {
    let mut set = PlaylistSet::default();
    let (playing, in_playing) = playlist(&mut set, &["a", "b"], None, None);
    let (viewed, in_viewed) = playlist(&mut set, &["x"], None, None);
    for phase in [
        PlaybackPhase::Unloaded,
        PlaybackPhase::Ended,
        PlaybackPhase::LoadFailed,
    ] {
        for input in [TransportInput::Space, TransportInput::Play] {
            let decision = decide(
                input,
                &TransportSituation {
                    navigation: get(&set, playing),
                    viewed: get(&set, viewed),
                    selected: Some(in_viewed[0]),
                    phase,
                    retry: None,
                    live: false,
                },
            );
            assert_eq!(
                decision,
                TransportDecision::Load(in_playing[0]),
                "{phase:?} {input:?}: first in playback order"
            );
            // The harder case: the selection is a *mid-list* row of the
            // navigation playlist itself, with no cursor. Space and `p`
            // still start at the first entry in playback order — a fallback
            // to the selection would be invisible if the selection were the
            // first row or lived in another playlist.
            assert_eq!(
                decide(
                    input,
                    &TransportSituation {
                        navigation: get(&set, playing),
                        viewed: get(&set, playing),
                        selected: Some(in_playing[1]),
                        phase,
                        retry: None,
                        live: false,
                    },
                ),
                TransportDecision::Load(in_playing[0]),
                "{phase:?} {input:?}: a mid-list selection is not a start point"
            );
        }
    }
}

#[test]
fn the_cursor_outranks_the_first_entry_and_shuffle_decides_what_first_means() {
    let mut set = PlaylistSet::default();
    let (with_cursor, entries) = playlist(&mut set, &["a", "b", "c"], Some(1), None);
    let situation = |navigation| TransportSituation {
        navigation,
        viewed: navigation,
        selected: None,
        phase: PlaybackPhase::Unloaded,
        retry: None,
        live: false,
    };
    assert_eq!(
        decide(TransportInput::Play, &situation(get(&set, with_cursor))),
        TransportDecision::Load(entries[1])
    );

    let mut set = PlaylistSet::default();
    let (shuffled, entries) = playlist(&mut set, &["a", "b", "c", "d", "e"], None, Some(42));
    assert_eq!(
        decide(TransportInput::Play, &situation(get(&set, shuffled))),
        TransportDecision::Load(entries[4]),
        "seed 42 orders IDs 1..=5 as 5 1 4 3 2"
    );
}

#[test]
fn a_valid_retry_outranks_the_empty_check_whatever_is_viewed() {
    // A is playing and empty; the failed request was in B; the view is on C.
    let mut set = PlaylistSet::default();
    let (a, _) = playlist(&mut set, &[], None, None);
    let (_b, in_b) = playlist(&mut set, &["b1"], None, None);
    let (c, in_c) = playlist(&mut set, &["c1"], None, None);
    for input in [TransportInput::Space, TransportInput::Play] {
        let decision = decide(
            input,
            &TransportSituation {
                navigation: get(&set, a),
                viewed: get(&set, c),
                selected: Some(in_c[0]),
                phase: PlaybackPhase::LoadFailed,
                retry: Some(in_b[0]),
                live: false,
            },
        );
        assert_eq!(decision, TransportDecision::Load(in_b[0]));
    }
    let nothing_to_retry = decide(
        TransportInput::Play,
        &TransportSituation {
            navigation: get(&set, a),
            viewed: get(&set, c),
            selected: Some(in_c[0]),
            phase: PlaybackPhase::LoadFailed,
            retry: None,
            live: false,
        },
    );
    assert_eq!(nothing_to_retry, TransportDecision::Notice(QUEUE_EMPTY));
}

#[test]
fn next_and_previous_step_from_the_cursor_in_playback_order_and_stop_at_the_ends() {
    let mut set = PlaylistSet::default();
    let (shuffled, e) = playlist(&mut set, &["a", "b", "c", "d", "e"], Some(0), Some(42));
    let at = |navigation, input| {
        decide(
            input,
            &TransportSituation {
                navigation,
                viewed: navigation,
                selected: None,
                phase: PlaybackPhase::Playing,
                retry: None,
                live: false,
            },
        )
    };
    // Order 5 1 4 3 2; the cursor is ID 1.
    assert_eq!(
        at(get(&set, shuffled), TransportInput::Next),
        TransportDecision::Load(e[3])
    );
    assert_eq!(
        at(get(&set, shuffled), TransportInput::Previous),
        TransportDecision::Load(e[4])
    );

    let mut set = PlaylistSet::default();
    let (at_end, _) = playlist(&mut set, &["a", "b"], Some(1), None);
    assert_eq!(
        at(get(&set, at_end), TransportInput::Next),
        TransportDecision::Nothing,
        "a boundary does not disturb playback"
    );
    let mut set = PlaylistSet::default();
    let (no_cursor, in_no_cursor) = playlist(&mut set, &["a", "b", "c"], None, None);
    assert_eq!(
        at(get(&set, no_cursor), TransportInput::Next),
        TransportDecision::Nothing,
        "no anchor, no step"
    );
    // A mid-list selection in the navigation playlist itself: still nothing.
    // Anything but `Nothing` here means the pre-M8 selection fallback came
    // back, and the middle row is the only one that shows it — stepping from
    // the first row and stepping from no anchor both look like a boundary.
    for input in [TransportInput::Next, TransportInput::Previous] {
        assert_eq!(
            decide(
                input,
                &TransportSituation {
                    navigation: get(&set, no_cursor),
                    viewed: get(&set, no_cursor),
                    selected: Some(in_no_cursor[1]),
                    phase: PlaybackPhase::Playing,
                    retry: None,
                    live: false,
                },
            ),
            TransportDecision::Nothing,
            "{input:?}: the selection is not an anchor"
        );
    }
}

#[test]
fn during_loading_navigation_anchors_on_the_retry_in_its_own_playlist() {
    let mut set = PlaylistSet::default();
    let (_a, _) = playlist(&mut set, &["a1", "a2"], Some(0), None);
    let (b, in_b) = playlist(&mut set, &["b1", "b2"], None, None);
    // The caller made B the navigation playlist because last_requested lives there.
    let decision = decide(
        TransportInput::Next,
        &TransportSituation {
            navigation: get(&set, b),
            viewed: get(&set, b),
            selected: None,
            phase: PlaybackPhase::Loading,
            retry: Some(in_b[0]),
            live: false,
        },
    );
    assert_eq!(decision, TransportDecision::Load(in_b[1]));
}

#[test]
fn engine_phases_keep_their_engine_commands_over_an_emptied_playlist() {
    let mut set = PlaylistSet::default();
    let (empty, _) = playlist(&mut set, &[], None, None);
    for phase in [
        PlaybackPhase::Playing,
        PlaybackPhase::Paused,
        PlaybackPhase::Reconnecting,
    ] {
        let situation = TransportSituation {
            navigation: get(&set, empty),
            viewed: get(&set, empty),
            selected: None,
            phase,
            retry: None,
            live: false,
        };
        assert_eq!(
            decide(TransportInput::Space, &situation),
            TransportDecision::TogglePause
        );
        assert_eq!(
            decide(TransportInput::Play, &situation),
            TransportDecision::Play
        );
        assert_eq!(
            decide(TransportInput::Enter, &situation),
            TransportDecision::Notice(QUEUE_EMPTY)
        );
    }
}
