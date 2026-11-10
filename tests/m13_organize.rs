//! M13 §1.5: the organize operations — sorts (with the duration refusal),
//! reverse, randomize, dedup by identity with the owned-entry exception,
//! prune-dead through the browse worker, and the menu's key flow.

#[path = "support/runtime.rs"]
mod runtime;

#[path = "support/views.rs"]
mod views;

mod support;

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use runtime::{pump_until, rig_with};
use support::media;
use tenuto::application::runtime::AppCommand;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::playlist::{PlaylistError, SortKey};
use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueEntryId, QueueSource};
use tenuto::session::{Action, Advance, LoadTarget, Session};
use tenuto::tui::input::{Effect, handle_key};
use tenuto::tui::state::{OrganizeOp, Overlay, UiState};

fn entry_named(name: &str, title: Option<&str>) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else {
        unreachable!()
    };
    let display = DisplayMetadata {
        title: title.map(|t| t.to_owned()),
        ..DisplayMetadata::default()
    };
    NewQueueEntry::new(media(name), QueueSource::LocalFile(path), display)
        .unwrap_or_else(|error| panic!("a literal entry must be valid: {error}"))
}

fn entry(name: &str) -> NewQueueEntry {
    entry_named(name, None)
}

/// Playlist A (the default, playing) holding `entries`.
struct One {
    session: Session,
    a: tenuto::playlist::PlaylistId,
    ids: Vec<QueueEntryId>,
}

fn one(entries: Vec<NewQueueEntry>) -> One {
    let mut session = Session::new(PersistedState::default());
    let a = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(a, entries)
        .unwrap_or_else(|error| panic!("fits: {error}"));
    One { session, a, ids }
}

fn order_of(session: &Session, a: tenuto::playlist::PlaylistId) -> Vec<usize> {
    session
        .state()
        .playlists()
        .playlist(a)
        .unwrap_or_else(|| panic!("playing"))
        .queue()
        .entries()
        .iter()
        .map(|entry| entry.id().get() as usize)
        .collect()
}

#[test]
fn sorting_by_title_uses_tags_and_falls_back_to_the_source_text() {
    // Entry IDs are 1, 2, 3 in enqueue order; the paths sort a.flac, b.flac,
    // c.flac. The untagged entry sorts by its source text, and '/' sorts
    // before every letter, so it lands first: c's path, then Alpha, then
    // zeta.
    let mut one = one(vec![
        entry_named("a", Some("zeta")),
        entry_named("b", Some("Alpha")),
        entry("c.flac_wrong"),
    ]);
    one.session
        .sort_playlist(one.a, SortKey::Title)
        .expect("sorts");
    let order = order_of(&one.session, one.a);
    assert_eq!(
        order,
        vec![
            one.ids[2].get() as usize,
            one.ids[1].get() as usize,
            one.ids[0].get() as usize
        ],
        "the untagged path, then Alpha, then zeta"
    );
}

#[test]
fn sorting_by_path_is_the_source_text_ascending() {
    let mut one = one(vec![entry("c"), entry("a"), entry("b")]);
    one.session
        .sort_playlist(one.a, SortKey::Path)
        .expect("sorts");
    assert_eq!(
        order_of(&one.session, one.a),
        vec![
            one.ids[1].get() as usize,
            one.ids[2].get() as usize,
            one.ids[0].get() as usize
        ]
    );
}

#[test]
fn sorting_by_duration_refuses_missing_or_declared_durations() {
    let mut one = one(vec![entry_named("a", None), entry_named("b", None)]);
    assert!(matches!(
        one.session.sort_playlist(one.a, SortKey::Duration),
        Err(PlaylistError::MissingDurations)
    ));
    let _ = one;
}

#[test]
fn sorting_an_already_sorted_playlist_changes_nothing() {
    let mut one = one(vec![entry("a"), entry("b"), entry("c")]);
    one.session.set_shuffle(one.a, Some(7)).expect("shuffled");
    // Path order equals enqueue order here, so the sort is a no-op — and a
    // no-op must not clear shuffle.
    one.session
        .sort_playlist(one.a, SortKey::Path)
        .expect("sorts");
    assert!(
        one.session
            .state()
            .playlists()
            .playlist(one.a)
            .expect("playing")
            .shuffle()
            .is_some(),
        "a no-op sort keeps the shuffle"
    );
}

#[test]
fn a_changing_sort_clears_shuffle() {
    let mut one = one(vec![
        entry_named("a", Some("z")),
        entry_named("b", Some("a")),
    ]);
    one.session.set_shuffle(one.a, Some(7)).expect("shuffled");
    one.session
        .sort_playlist(one.a, SortKey::Title)
        .expect("sorts");
    assert!(
        one.session
            .state()
            .playlists()
            .playlist(one.a)
            .expect("playing")
            .shuffle()
            .is_none(),
        "the list order is now the playback order"
    );
}

#[test]
fn reverse_twice_is_the_original_order_and_a_one_entry_reverse_is_a_no_op() {
    let mut three = one(vec![entry("a"), entry("b"), entry("c")]);
    let before = order_of(&three.session, three.a);
    three.session.reverse_playlist(three.a).expect("reverses");
    assert_ne!(order_of(&three.session, three.a), before);
    three.session.reverse_playlist(three.a).expect("reverses");
    assert_eq!(order_of(&three.session, three.a), before);

    let mut single = one(vec![entry("only")]);
    assert!(matches!(
        single.session.reverse_playlist(single.a),
        Ok(Action::None)
    ));
}

#[test]
fn randomize_is_deterministic_per_seed_and_can_return_a_no_op() {
    let build = || {
        let mut session = Session::new(PersistedState::default());
        let a = session.state().playlists().playing();
        let (ids, _) = session
            .enqueue(a, vec![entry("a"), entry("b"), entry("c"), entry("d")])
            .expect("fits");
        (session, a, ids)
    };
    let (mut left, la, _) = build();
    let (mut right, ra, _) = build();
    left.randomize_playlist(la, 42).expect("randomizes");
    right.randomize_playlist(ra, 42).expect("randomizes");
    assert_eq!(
        order_of(&left, la),
        order_of(&right, ra),
        "the same seed, the same order"
    );

    // A single-entry playlist cannot change under any seed.
    let mut solo = Session::new(PersistedState::default());
    let solo_a = solo.state().playlists().playing();
    let _ = solo.enqueue(solo_a, vec![entry("solo")]).expect("fits");
    assert!(matches!(
        solo.randomize_playlist(solo_a, 9),
        Ok(Action::None)
    ));
}

#[test]
fn the_cursor_follows_its_entry_through_a_sort() {
    let mut one = one(vec![
        entry_named("a", Some("zeta")),
        entry_named("b", Some("Alpha")),
        entry("c"),
    ]);
    let second = one.ids[1];
    // Adopt b (the queue's cursor); a sort that moves it keeps it active.
    let request = one
        .session
        .register_load(LoadTarget::Queue(second), &media("b"))
        .expect("registered");
    one.session.observe(
        &tenuto::playback::event::PlaybackEvent::Loaded {
            session_rev: 1,
            request,
            media: media("b"),
            metadata: Default::default(),
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::ZERO,
            disposition: tenuto::playback::event::StartDisposition::Fresh,
        },
        FakeClock::new().sample(),
    );
    one.session
        .sort_playlist(one.a, SortKey::Title)
        .expect("sorts");
    assert_eq!(
        one.session
            .state()
            .playlists()
            .playlist(one.a)
            .expect("playing")
            .queue()
            .active(),
        Some(second),
        "the cursor moved with its entry"
    );
    // And an end-of-track advances from the entry's new position.
    one.session.observe(
        &tenuto::playback::event::PlaybackEvent::EndOfTrack {
            session_rev: 2,
            position: Duration::from_secs(9),
            provenance: tenuto::media::provenance::PositionProvenance::Established,
        },
        FakeClock::new().sample(),
    );
    assert_eq!(
        one.session.take_advance(),
        Some(Advance::Next(one.ids[0])),
        "zeta is last after the sort"
    );
}

#[test]
fn dedup_keeps_the_first_occurrence() {
    let mut one = one(vec![entry("twin"), entry("other")]);
    // A second occurrence of the same media, under a new id.
    let (twin2, _) = one
        .session
        .enqueue(
            one.a,
            vec![{
                let MediaId::LocalFile(path) = media("twin") else {
                    unreachable!()
                };
                NewQueueEntry::new(
                    media("twin"),
                    QueueSource::LocalFile(path),
                    Default::default(),
                )
                .expect("valid")
            }],
        )
        .expect("fits");
    let (action, removed) = one.session.deduplicate_playlist(one.a).expect("dedups");
    assert!(matches!(action, Action::Submit { .. }));
    assert_eq!(removed, 1);
    let after = order_of(&one.session, one.a);
    assert_eq!(after.len(), 2);
    assert!(
        !after.contains(&(twin2[0].get() as usize)),
        "the later duplicate went, the first stayed"
    );
}

#[test]
fn dedup_keeps_the_owned_occurrence_over_the_first() {
    let mut one = one(vec![entry("twin"), entry("other")]);
    let (twin2, _) = one
        .session
        .enqueue(
            one.a,
            vec![{
                let MediaId::LocalFile(path) = media("twin") else {
                    unreachable!()
                };
                NewQueueEntry::new(
                    media("twin"),
                    QueueSource::LocalFile(path),
                    Default::default(),
                )
                .expect("valid")
            }],
        )
        .expect("fits");
    // The later occurrence is the one playing: it stays, the first goes.
    let request = one
        .session
        .register_load(LoadTarget::Queue(twin2[0]), &media("twin"))
        .expect("registered");
    one.session.observe(
        &tenuto::playback::event::PlaybackEvent::Loaded {
            session_rev: 1,
            request,
            media: media("twin"),
            metadata: Default::default(),
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::ZERO,
            disposition: tenuto::playback::event::StartDisposition::Fresh,
        },
        FakeClock::new().sample(),
    );
    let (_action, removed) = one.session.deduplicate_playlist(one.a).expect("dedups");
    assert_eq!(removed, 1);
    let after = order_of(&one.session, one.a);
    assert!(
        after.contains(&(twin2[0].get() as usize)),
        "the owned occurrence is the one that stays"
    );
}

fn state_with_playlist(entries: Vec<NewQueueEntry>) -> (PersistedState, Vec<QueueEntryId>) {
    let mut session = Session::new(PersistedState::default());
    let a = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(a, entries)
        .unwrap_or_else(|error| panic!("fits: {error:?}"));
    (session.state().clone(), ids)
}

fn local_entry(path: PathBuf) -> NewQueueEntry {
    let absolute =
        AbsolutePath::new(path.clone()).unwrap_or_else(|error| panic!("absolute: {error:?}"));
    let media = MediaId::LocalFile(absolute.clone());
    NewQueueEntry::new(media, QueueSource::LocalFile(absolute), Default::default())
        .unwrap_or_else(|error| panic!("valid: {error:?}"))
}

#[test]
fn prune_dead_removes_only_gone_files_through_the_worker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let alive = dir.path().join("alive.flac");
    std::fs::write(&alive, b"placeholder").expect("written");
    let gone = dir.path().join("gone.flac");
    let (state, ids) = state_with_playlist(vec![local_entry(alive.clone()), local_entry(gone)]);
    let mut rig = rig_with(state);
    rig.runtime
        .handle(AppCommand::PruneDeadPlaylist(rig.runtime.view().viewed));
    pump_until(&mut rig, "the dead check", |view| {
        view.status
            .as_deref()
            .is_some_and(|status| status.contains("Removed"))
    });
    let view = rig.runtime.view();
    assert_eq!(view.rows.len(), 1, "the dead entry went");
    assert_eq!(view.rows[0].id, ids[0], "the live file stayed");
    assert!(
        view.status
            .as_deref()
            .is_some_and(|status| status.contains("Removed 1 dead entries")),
        "{:?}",
        view.status
    );
}

fn remote_entry(url: &str) -> NewQueueEntry {
    let parsed = tenuto::media::id::NormalizedUrl::parse(url)
        .unwrap_or_else(|error| panic!("url: {error:?}"));
    NewQueueEntry::new(
        MediaId::RemoteUrl(parsed.clone()),
        QueueSource::RemoteUrl(parsed),
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("valid: {error:?}"))
}

#[test]
fn prune_dead_skips_playlists_without_local_entries() {
    let (state, _) = state_with_playlist(vec![remote_entry("https://example.com/stream")]);
    let mut rig = rig_with(state);
    let viewed = rig.runtime.view().viewed;
    rig.runtime.handle(AppCommand::PruneDeadPlaylist(viewed));
    let view = rig.runtime.view();
    assert!(
        view.status
            .as_deref()
            .is_some_and(|status| status.contains("no local entries")),
        "{:?}",
        view.status
    );
    assert_eq!(view.rows.len(), 1);
}

fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(ui: &mut UiState, code: KeyCode) -> Vec<Effect> {
    handle_key(
        KeyEvent::new(code, KeyModifiers::NONE),
        ui,
        &views::sample_view(),
    )
}

#[test]
fn the_menu_opens_runs_a_sort_and_confirms_the_removals() {
    let mut ui = UiState::new(true);
    let effects = press(&mut ui, KeyCode::Char('S'));
    assert!(effects.is_empty());
    assert!(matches!(ui.overlay, Overlay::SortMenu(_)));

    // Item 0: sort by title, straight to the command.
    let effects = press(&mut ui, KeyCode::Enter);
    assert!(matches!(
        effects.as_slice(),
        [Effect::App(AppCommand::SortPlaylist(_, SortKey::Title))]
    ));

    // Down six times lands on Remove duplicates; Enter confirms first.
    press(&mut ui, KeyCode::Char('S'));
    for _ in 0..6 {
        press(&mut ui, KeyCode::Down);
    }
    assert_eq!(ui.sort_cursor, 6);
    let effects = press(&mut ui, KeyCode::Enter);
    assert!(effects.is_empty(), "the removal waits for confirmation");
    assert!(matches!(
        ui.overlay,
        Overlay::ConfirmOrganize(_, OrganizeOp::Deduplicate)
    ));
    let effects = handle_key(plain(KeyCode::Char('y')), &mut ui, &views::sample_view());
    assert!(matches!(
        effects.as_slice(),
        [Effect::App(AppCommand::DeduplicatePlaylist(_))]
    ));

    // The prune item likewise.
    press(&mut ui, KeyCode::Char('S'));
    for _ in 0..7 {
        press(&mut ui, KeyCode::Down);
    }
    assert_eq!(ui.sort_cursor, 7);
    press(&mut ui, KeyCode::Enter);
    assert!(matches!(
        ui.overlay,
        Overlay::ConfirmOrganize(_, OrganizeOp::Prune)
    ));
    let effects = handle_key(plain(KeyCode::Char('y')), &mut ui, &views::sample_view());
    assert!(matches!(
        effects.as_slice(),
        [Effect::App(AppCommand::PruneDeadPlaylist(_))]
    ));

    // Esc closes the menu without acting.
    press(&mut ui, KeyCode::Char('S'));
    press(&mut ui, KeyCode::Down);
    let effects = press(&mut ui, KeyCode::Esc);
    assert!(effects.is_empty());
    assert_eq!(ui.overlay, Overlay::None);
}
