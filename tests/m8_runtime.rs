//! M8 §5, §7, §10: the runtime's viewed playlist, the playlist commands and
//! the side effects scoped to the playlist they touch.

#[path = "support/runtime.rs"]
mod runtime;

#[path = "support/tagged_flac.rs"]
mod tagged_flac;

use std::time::{Duration, Instant};

use runtime::{pump_for, pump_until, rig_with, rig_with_probe, row_ids};
use tenuto::application::enrich::default_probe;
use tenuto::application::runtime::{AppCommand, EnqueueItem, PlayerRuntime};
use tenuto::lifecycle::hooks::TestHook;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::queue::{NewQueueEntry, QueueSource};

const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const FIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");

fn local_entry(path: &std::path::Path) -> NewQueueEntry {
    let absolute =
        AbsolutePath::new(path.to_path_buf()).unwrap_or_else(|error| panic!("absolute: {error}"));
    NewQueueEntry::new(
        MediaId::LocalFile(absolute.clone()),
        QueueSource::LocalFile(absolute),
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("entry: {error}"))
}

fn tab_names(runtime: &PlayerRuntime) -> Vec<String> {
    runtime
        .view()
        .tabs
        .iter()
        .map(|tab| tab.name.clone())
        .collect()
}

#[test]
fn a_new_playlist_is_viewed_and_enqueue_goes_where_it_was_told() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime
        .handle(AppCommand::CreatePlaylist("Jazz".into()));
    let jazz = rig.runtime.viewed();
    assert_ne!(jazz, first);
    assert_eq!(tab_names(&rig.runtime), ["Default", "Jazz"]);

    rig.runtime.handle(AppCommand::Enqueue {
        dest: first,
        items: vec![EnqueueItem::Path(SHORT.into())],
    });
    assert!(
        rig.runtime.view().rows.is_empty(),
        "the view is on Jazz; the add went to Default"
    );
    assert_eq!(rig.runtime.rows_of(first).len(), 1);
}

#[test]
fn enter_in_another_playlist_moves_playing_only_once_the_load_is_adopted() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime
        .handle(AppCommand::CreatePlaylist("Jazz".into()));
    let jazz = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue {
        dest: jazz,
        items: vec![EnqueueItem::Path(SHORT.into())],
    });
    let id = row_ids(&rig.runtime)[0];
    assert!(
        rig.runtime
            .view()
            .tabs
            .iter()
            .any(|tab| tab.id == first && tab.playing)
    );

    rig.runtime.handle(AppCommand::PlayEntry(id));
    pump_until(&mut rig.runtime, "Jazz is the playing playlist", |view| {
        view.active == Some(id) && view.tabs.iter().any(|tab| tab.id == jazz && tab.playing)
    });
}

#[test]
fn deleting_the_viewed_playlist_moves_the_view_and_the_last_one_is_refused() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime
        .handle(AppCommand::CreatePlaylist("Jazz".into()));
    let jazz = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::DeletePlaylist(jazz));
    assert_eq!(rig.runtime.viewed(), first);
    rig.runtime.handle(AppCommand::DeletePlaylist(first));
    assert_eq!(tab_names(&rig.runtime), ["Default"]);
    assert!(
        rig.runtime
            .view()
            .status
            .is_some_and(|status| status.contains("last playlist"))
    );
}

#[test]
fn clearing_an_inactive_playlist_neither_stops_playback_nor_loses_anothers_enrichment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tagged = tagged_flac::tagged_flac(dir.path(), "Title", "Artist", "Album", None);
    let mut rig = rig_with_probe(PersistedState::default(), default_probe(TestHook::None));
    let first = rig.runtime.viewed();
    rig.runtime
        .handle(AppCommand::CreatePlaylist("Scratch".into()));
    let scratch = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue {
        dest: scratch,
        items: vec![EnqueueItem::Path(SHORT.into())],
    });
    rig.runtime.handle(AppCommand::Enqueue {
        dest: first,
        items: vec![EnqueueItem::Path(tagged.clone())],
    });

    rig.runtime.handle(AppCommand::ClearPlaylist(scratch));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        rig.runtime.pump();
        if rig
            .runtime
            .rows_of(first)
            .iter()
            .any(|row| row.title.contains("Title"))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Default's pending tag probe was lost when Scratch was cleared");
}

#[test]
fn a_restored_inactive_playlist_is_enriched_on_the_first_pump() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tagged = tagged_flac::tagged_flac(dir.path(), "Title", "Artist", "Album", None);
    // A state as a previous run left it: an untitled entry in a playlist that
    // is not the playing one.
    let mut session = tenuto::session::Session::new(PersistedState::default());
    let (other, _) = session.create_playlist("Other").expect("room");
    session
        .enqueue(other, vec![local_entry(&tagged)])
        .expect("fits");
    let state = session.state().clone();
    assert_ne!(state.playing(), other);

    let mut rig = rig_with_probe(state, default_probe(TestHook::None));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        rig.runtime.pump();
        if rig
            .runtime
            .rows_of(other)
            .iter()
            .any(|row| row.title.contains("Title"))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "the restored entries of a non-playing playlist were never offered to the metadata workers"
    );
}

#[test]
fn neither_an_unowned_removal_nor_another_playlists_clear_cancels_a_standing_seek() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue {
        dest: first,
        items: vec![
            EnqueueItem::Path(FIVE.into()),
            EnqueueItem::Path(FIVE.into()),
        ],
    });
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "the first row is playing", |view| {
        view.now_playing
            .as_ref()
            .is_some_and(|now| now.loaded && now.entry == Some(ids[0]) && now.duration.is_some())
    });
    rig.runtime
        .handle(AppCommand::CreatePlaylist("Scratch".into()));
    let scratch = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue {
        dest: scratch,
        items: vec![EnqueueItem::Path(SHORT.into())],
    });

    let before = rig
        .runtime
        .view()
        .now_playing
        .map(|now| now.position)
        .unwrap_or_default();
    rig.runtime.handle(AppCommand::SeekBy(3));
    // Neither touches the playback the burst belongs to (M8 §5).
    rig.runtime.handle(AppCommand::Remove(ids[1]));
    rig.runtime.handle(AppCommand::ClearPlaylist(scratch));
    // Past `KeyRouter`'s 250ms quiet window, so a burst still standing has
    // flushed; a cancelled one never will, and 400ms of playback cannot
    // account for a three-second jump.
    pump_for(&mut rig.runtime, Duration::from_millis(400));
    let position = rig
        .runtime
        .view()
        .now_playing
        .map(|now| now.position)
        .unwrap_or_default();
    assert!(
        position >= before + Duration::from_millis(2500),
        "the seek was cancelled: {before:?} -> {position:?}"
    );
    let _ = rig.runtime.shutdown();
}

#[test]
fn a_removal_outside_the_viewed_playlist_offers_no_selection_hint() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue {
        dest: first,
        items: vec![
            EnqueueItem::Path(SHORT.into()),
            EnqueueItem::Path(SHORT.into()),
        ],
    });
    let ids = row_ids(&rig.runtime);

    // Viewing Default, the successor of the removed row is a row on screen.
    rig.runtime.handle(AppCommand::Remove(ids[0]));
    assert_eq!(rig.runtime.take_selection_hint(), Some(ids[1]));

    rig.runtime.handle(AppCommand::Enqueue {
        dest: first,
        items: vec![EnqueueItem::Path(SHORT.into())],
    });
    rig.runtime
        .handle(AppCommand::CreatePlaylist("Jazz".into()));
    assert_ne!(rig.runtime.viewed(), first);
    rig.runtime.handle(AppCommand::Remove(ids[1]));
    assert_eq!(
        rig.runtime.take_selection_hint(),
        None,
        "the successor is a row of Default, which is not on screen"
    );
}

#[test]
fn toggling_shuffle_marks_the_tab_and_keeps_the_track_playing() {
    let mut rig = rig_with(PersistedState::default());
    let first = rig.runtime.viewed();
    rig.runtime.handle(AppCommand::Enqueue {
        dest: first,
        items: vec![EnqueueItem::Path(SHORT.into())],
    });
    let id = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(id));
    pump_until(&mut rig.runtime, "playing", |view| view.active == Some(id));

    rig.runtime.handle(AppCommand::ToggleShuffle(first));
    let view = rig.runtime.view();
    assert!(view.tabs[0].shuffled);
    assert_eq!(view.active, Some(id));
    rig.runtime.handle(AppCommand::ToggleShuffle(first));
    assert!(!rig.runtime.view().tabs[0].shuffled);
}
