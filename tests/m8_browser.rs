//! The browser's captured destination, markable directories, and the
//! Files-tab `a` key (design doc M8 §8, "Keys, Files tab" and "One
//! destination rule").

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tenuto::application::browse::{BrowseRequest, BrowseResult, DirEntry, EntryKind};
use tenuto::application::view::QueueRow;
use tenuto::media::id::MediaId;
use tenuto::playlist::PlaylistId;
use tenuto::queue::{DisplayMetadata, IdAllocator, NewQueueEntry, Queue, QueueSource};
use tenuto::tui::browser::{BrowserEffect, BrowserState};

mod support;

const DEST: u64 = 7;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn browser(rows: &[(&str, EntryKind)]) -> BrowserState {
    let mut state = BrowserState::new("/music".into(), PlaylistId::from_raw_for_tests(DEST));
    let entries = rows
        .iter()
        .map(|(name, kind)| DirEntry {
            name: (*name).to_owned(),
            path: format!("/music/{name}").into(),
            kind: *kind,
            media: (*kind == EntryKind::Audio).then(|| support::media(name)),
        })
        .collect();
    state.apply(BrowseResult::Directory {
        path: "/music".into(),
        entries: Ok(entries),
    });
    state
}

fn requests(effects: &[BrowserEffect]) -> Vec<BrowseRequest> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            BrowserEffect::Request(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

/// A `QueueRow` for `media`, whose `QueueEntryId` is `raw_id` — obtained by
/// enqueuing a single scratch entry into its own `Queue` with an
/// `IdAllocator` started at `raw_id`, since `QueueEntryId` has no public
/// constructor.
fn queued_row(media: MediaId, raw_id: u64) -> QueueRow {
    let path = tenuto::media::id::AbsolutePath::new(format!("/scratch/{raw_id}.flac").into())
        .unwrap_or_else(|error| panic!("absolute path: {error}"));
    let entry = NewQueueEntry::new(
        MediaId::LocalFile(path.clone()),
        QueueSource::LocalFile(path),
        DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("queue entry: {error}"));
    let mut queue = Queue::default();
    let mut ids = IdAllocator::starting_at(Some(raw_id));
    let id = queue
        .enqueue(vec![entry], &mut ids)
        .unwrap_or_else(|error| panic!("enqueue: {error}"))[0];
    QueueRow {
        id,
        media,
        title: String::new(),
        subtitle: None,
        duration: None,
        saved: None,
    }
}

#[test]
fn a_on_a_directory_asks_for_its_tree_with_the_captured_destination() {
    let mut state = browser(&[
        ("Album", EntryKind::Directory),
        ("loose.mp3", EntryKind::Audio),
    ]);
    let effects = state.handle_key(key(KeyCode::Char('a')));
    assert_eq!(
        requests(&effects),
        [BrowseRequest::CollectTree {
            roots: vec!["/music/Album".into()],
            dest: PlaylistId::from_raw_for_tests(DEST)
        }]
    );
}

#[test]
fn space_marks_directories_and_a_sends_files_and_directories_together_in_listing_order() {
    let mut state = browser(&[
        ("Album", EntryKind::Directory),
        ("loose.mp3", EntryKind::Audio),
        ("notes.txt", EntryKind::Other),
    ]);
    for _ in 0..3 {
        state.handle_key(key(KeyCode::Char(' ')));
        state.handle_key(key(KeyCode::Down));
    }
    assert_eq!(
        state.marked.iter().copied().collect::<Vec<_>>(),
        [0, 1],
        "a text file is not markable"
    );
    let effects = state.handle_key(key(KeyCode::Char('a')));
    assert_eq!(
        requests(&effects),
        [BrowseRequest::CollectTree {
            roots: vec!["/music/Album".into(), "/music/loose.mp3".into()],
            dest: PlaylistId::from_raw_for_tests(DEST)
        }]
    );
    assert!(state.marked.is_empty(), "marks clear once they are added");
}

#[test]
fn a_is_additive_where_enter_toggles() {
    let mut state = browser(&[("loose.mp3", EntryKind::Audio)]);
    state.sync_queue(&[queued_row(support::media("loose.mp3"), 5)]);
    assert!(matches!(
        state.handle_key(key(KeyCode::Enter))[..],
        [BrowserEffect::Remove(_)]
    ));
    assert!(
        state.handle_key(key(KeyCode::Char('a'))).is_empty(),
        "already queued: skipped, never removed"
    );
}

#[test]
fn enter_on_an_audio_row_with_marks_does_what_a_does_and_enter_on_a_directory_still_opens_it() {
    let mut state = browser(&[
        ("Album", EntryKind::Directory),
        ("loose.mp3", EntryKind::Audio),
    ]);
    state.handle_key(key(KeyCode::Char(' ')));
    state.handle_key(key(KeyCode::Down));
    let effects = state.handle_key(key(KeyCode::Enter));
    assert_eq!(
        requests(&effects),
        [BrowseRequest::CollectTree {
            roots: vec!["/music/Album".into()],
            dest: PlaylistId::from_raw_for_tests(DEST)
        }],
        "the marked directory is not silently ignored"
    );

    let mut state = browser(&[("Album", EntryKind::Directory)]);
    assert_eq!(
        requests(&state.handle_key(key(KeyCode::Enter))),
        [BrowseRequest::Directory("/music/Album".into())]
    );
}

#[test]
fn a_single_file_with_no_marks_still_enqueues_directly_with_the_destination() {
    let mut state = browser(&[("loose.mp3", EntryKind::Audio)]);
    let effects = state.handle_key(key(KeyCode::Char('a')));
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Enqueue { dest, items }] if dest.get() == DEST && items.len() == 1
        ),
        "{effects:?}"
    );
}

/// Not one of the brief's five scenarios, but called for by the brief's
/// implementation note ("Marks already clear when the directory changes;
/// confirm with the existing test in `tests/m5_browser.rs` and add one if
/// there is none"): before this task a directory could never be marked, so
/// this exact scenario — Enter opening a directory that is itself marked —
/// was unreachable. Enter on a directory row stays unconditional (it never
/// consults `marked`), and `open_directory`'s `start_loading` clears marks
/// synchronously, before any worker answer.
#[test]
fn opening_a_marked_directory_still_opens_it_and_clears_the_mark() {
    let mut state = browser(&[
        ("Album", EntryKind::Directory),
        ("loose.mp3", EntryKind::Audio),
    ]);
    state.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(state.marked.len(), 1);
    let effects = state.handle_key(key(KeyCode::Enter));
    assert_eq!(
        requests(&effects),
        [BrowseRequest::Directory(PathBuf::from("/music/Album"))]
    );
    assert!(
        state.marked.is_empty(),
        "opening a directory clears whatever was marked, including itself"
    );
}
