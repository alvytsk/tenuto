//! The on-demand browser (design doc M5 §8): a one-level directory listing,
//! and the browser's key handling as a pure function over its own state.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;
use tenuto::application::browse::{
    BrowseRequest, BrowseResult, BrowseWorker, DirEntry, EntryKind, list_directory,
};
use tenuto::application::runtime::EnqueueItem;
use tenuto::application::source::resolve_source;
use tenuto::application::view::QueueRow;
use tenuto::http::source::StationIdentity;
use tenuto::library::{EpisodeCandidate, FeedSummary, StationRow};
use tenuto::media::id::{AbsolutePath, EpisodeKey, FeedId, MediaId, NormalizedUrl};
use tenuto::playlist::PlaylistId;
use tenuto::queue::QueueEntryId;
use tenuto::tui::browser::{BrowserEffect, BrowserState, BrowserTab, NoticeKind};
use tenuto::tui::render::{Visuals, draw};
use tenuto::tui::state::{Overlay, UiState};
use time::OffsetDateTime;

#[path = "support/views.rs"]
mod views;

/// The destination every `BrowserState` in this file is built with; no test
/// here exercises a second playlist, so one captured value stands for all of
/// them (M8 §8).
const DEST: u64 = 1;

fn dest() -> PlaylistId {
    PlaylistId::from_raw_for_tests(DEST)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(state: &mut BrowserState, codes: &[KeyCode]) -> Vec<BrowserEffect> {
    codes
        .iter()
        .flat_map(|code| state.handle_key(key(*code)))
        .collect()
}

/// `b.MP3`, `a.flac`, `z/deep.flac` and `notes.txt` in a fresh directory.
fn sample_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let root = dir.path().to_path_buf();
    for name in ["b.MP3", "a.flac", "notes.txt"] {
        std::fs::write(root.join(name), b"x").unwrap_or_else(|error| panic!("write: {error}"));
    }
    std::fs::create_dir(root.join("z")).unwrap_or_else(|error| panic!("mkdir: {error}"));
    std::fs::write(root.join("z/deep.flac"), b"x").unwrap_or_else(|error| panic!("write: {error}"));
    (dir, root)
}

fn listed(root: &Path) -> BrowserState {
    let entries = list_directory(root).unwrap_or_else(|error| panic!("list: {error}"));
    let mut state = BrowserState::new(root.to_path_buf(), dest());
    state.apply(BrowseResult::Directory {
        path: root.to_path_buf(),
        entries: Ok(entries),
    });
    state
}

#[test]
fn a_listing_is_one_level_directories_first_and_classifies_audio() {
    let (_dir, root) = sample_dir();
    let entries = list_directory(&root).unwrap_or_else(|error| panic!("list: {error}"));
    let listed: Vec<(&str, &Path, EntryKind)> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.path.as_path(), entry.kind))
        .collect();
    let expected = [
        ("z", EntryKind::Directory),
        ("a.flac", EntryKind::Audio),
        ("b.MP3", EntryKind::Audio),
        ("notes.txt", EntryKind::Other),
    ]
    .map(|(name, kind)| (name, root.join(name), kind));
    let expected: Vec<(&str, &Path, EntryKind)> = expected
        .iter()
        .map(|(name, path, kind)| (*name, path.as_path(), *kind))
        .collect();
    assert_eq!(listed, expected);
    assert!(entries.iter().all(|entry| entry.name != "deep.flac"));

    // An audio file carries the identity the queue will use for it, so the
    // browser can tell which rows are already queued; nothing else does.
    let canonical = root
        .join("a.flac")
        .canonicalize()
        .unwrap_or_else(|error| panic!("canonicalize: {error}"));
    let expected_media = MediaId::LocalFile(
        AbsolutePath::new(canonical).unwrap_or_else(|error| panic!("absolute: {error}")),
    );
    assert_eq!(entries[1].media, Some(expected_media));
    assert!(entries[0].media.is_none() && entries[3].media.is_none());
}

#[test]
fn an_unreadable_directory_is_an_error_value() {
    let (_dir, root) = sample_dir();
    assert!(list_directory(&root.join("missing")).is_err());
    assert!(list_directory(&root.join("notes.txt")).is_err());
}

#[test]
fn marked_files_enqueue_together_in_listing_order() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    let effects = press(
        &mut state,
        &[
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Enter,
        ],
    );
    let (a, b) = (root.join("a.flac"), root.join("b.MP3"));
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Enqueue { items, .. }] if matches!(
                &items[..],
                [EnqueueItem::Path(first), EnqueueItem::Path(second)]
                    if *first == a && *second == b
            )
        ),
        "{effects:?}"
    );
    assert!(state.marked.is_empty(), "marks clear after an enqueue");
}

#[test]
fn enter_on_the_cursor_file_enqueues_it_alone_and_marks_skip_other_rows() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    // A non-audio, non-directory file cannot be marked (a directory can,
    // M8 §8 — covered by tests/m8_browser.rs instead).
    press(
        &mut state,
        &[
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Char(' '),
        ],
    );
    assert!(state.marked.is_empty());
    let effects = press(&mut state, &[KeyCode::Char('k'), KeyCode::Enter]);
    let b = root.join("b.MP3");
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Enqueue { items, .. }]
                if matches!(&items[..], [EnqueueItem::Path(only)] if *only == b)
        ),
        "{effects:?}"
    );
}

#[test]
fn enter_on_a_directory_requests_it_and_backspace_returns_to_the_parent() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    let effects = press(&mut state, &[KeyCode::Enter]);
    let z = root.join("z");
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Directory(path))] if *path == z),
        "{effects:?}"
    );
    assert_eq!(state.cwd, z);
    assert!(state.loading);

    // A late result for the directory just left is not shown.
    state.apply(BrowseResult::Directory {
        path: root.clone(),
        entries: list_directory(&root).map_err(|error| error.to_string()),
    });
    assert!(state.loading);
    assert!(state.entries.is_empty());

    state.apply(BrowseResult::Directory {
        path: z.clone(),
        entries: list_directory(&z).map_err(|error| error.to_string()),
    });
    assert!(!state.loading);
    assert_eq!(state.entries.len(), 1);

    let effects = press(&mut state, &[KeyCode::Backspace]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Directory(path))] if *path == root),
        "{effects:?}"
    );
    assert_eq!(state.cwd, root);
    let effects = press(&mut state, &[KeyCode::Left]);
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Request(BrowseRequest::Directory(_))]
        ),
        "Left goes up too: {effects:?}"
    );
}

#[test]
fn b_and_esc_close_and_ctrl_or_alt_chords_do_nothing() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    assert!(matches!(
        &press(&mut state, &[KeyCode::Char('b')])[..],
        [BrowserEffect::Close]
    ));
    assert!(matches!(
        &press(&mut state, &[KeyCode::Esc])[..],
        [BrowserEffect::Close]
    ));
    for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        for code in [KeyCode::Char('b'), KeyCode::Char('j'), KeyCode::Char(' ')] {
            assert!(state.handle_key(KeyEvent::new(code, modifier)).is_empty());
        }
    }
    assert_eq!(state.cursor, 0);
    assert!(state.marked.is_empty());
}

#[test]
fn an_error_listing_is_shown_as_a_value() {
    let root = PathBuf::from("/nonexistent/m5-browser");
    let mut state = BrowserState::new(root.clone(), dest());
    state.apply(BrowseResult::Directory {
        path: root,
        entries: Err("No such file or directory".to_owned()),
    });
    assert!(!state.loading);
    assert_eq!(state.error.as_deref(), Some("No such file or directory"));
    assert!(press(&mut state, &[KeyCode::Enter, KeyCode::Char(' ')]).is_empty());
}

fn feed(slug: &str) -> FeedSummary {
    FeedSummary {
        slug: slug.to_owned(),
        title: Some(format!("{slug} title")),
        episodes: Some(2),
        last_refreshed_at: None,
    }
}

fn episode(guid: &str, enclosure: Option<&str>) -> EpisodeCandidate {
    EpisodeCandidate {
        media: MediaId::PodcastEpisode {
            feed: FeedId::new("0123456789abcdef0123456789abcdef".into())
                .unwrap_or_else(|error| panic!("feed id: {error}")),
            episode: EpisodeKey::resolve(Some(guid), None, None)
                .unwrap_or_else(|error| panic!("episode key: {error}")),
        },
        enclosure: enclosure.map(|url| url.parse().unwrap_or_else(|error| panic!("url: {error}"))),
        title: Some(guid.to_owned()),
        declared_duration: None,
        published: None,
    }
}

fn dated(guid: &str, unix: i64) -> EpisodeCandidate {
    let mut candidate = episode(guid, Some("https://cdn.example.org/x.mp3"));
    candidate.published = Some(
        OffsetDateTime::from_unix_timestamp(unix).unwrap_or_else(|error| panic!("time: {error}")),
    );
    candidate
}

fn queue_row(id: QueueEntryId, media: MediaId) -> QueueRow {
    QueueRow {
        id,
        media,
        title: "queued".to_owned(),
        subtitle: None,
        duration: None,
        saved: None,
    }
}

fn guid_of(candidate: &EpisodeCandidate) -> Option<&str> {
    candidate.title.as_deref()
}

#[test]
fn podcasts_tab_lists_feeds_then_episodes_and_skips_unplayable_marks() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    let effects = press(&mut state, &[KeyCode::Tab]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Feeds)]),
        "{effects:?}"
    );
    assert_eq!(state.tab, BrowserTab::Podcasts);
    assert!(state.loading);
    state.apply(BrowseResult::Feeds(Ok(vec![feed("one"), feed("two")])));
    assert!(!state.loading);
    assert_eq!(state.feeds.len(), 2);

    // Feeds are not enqueueable.
    press(&mut state, &[KeyCode::Char(' ')]);
    assert!(state.marked.is_empty());

    let effects = press(&mut state, &[KeyCode::Char('j'), KeyCode::Enter]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Episodes { slug })] if slug == "two"),
        "{effects:?}"
    );
    // A late listing for a feed not being viewed is ignored.
    state.apply(BrowseResult::Episodes {
        slug: "one".to_owned(),
        episodes: Ok(vec![episode("stale", Some("https://example.com/s.mp3"))]),
    });
    assert!(state.loading);
    state.apply(BrowseResult::Episodes {
        slug: "two".to_owned(),
        episodes: Ok(vec![
            episode("e1", Some("https://example.com/1.mp3")),
            episode("e2", None),
            episode("e3", Some("https://example.com/3.mp3")),
        ]),
    });
    assert!(!state.loading);
    assert_eq!(
        state
            .episodes
            .as_ref()
            .map(|(slug, list)| (slug.as_str(), list.len())),
        Some(("two", 3))
    );

    // Mark e1, try to mark e2 (no enclosure), mark e3.
    press(
        &mut state,
        &[
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
        ],
    );
    assert_eq!(state.marked.iter().copied().collect::<Vec<_>>(), vec![0, 2]);
    let effects = press(&mut state, &[KeyCode::Enter]);
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Enqueue { items, .. }] if matches!(
                &items[..],
                [EnqueueItem::Episode(first), EnqueueItem::Episode(second)]
                    if guid_of(first) == Some("e1") && guid_of(second) == Some("e3")
            )
        ),
        "{effects:?}"
    );

    // Enter on an unplayable episode with nothing marked does nothing.
    assert!(press(&mut state, &[KeyCode::Up, KeyCode::Enter]).is_empty());

    // Back to the feed list, with the cursor on the feed just left.
    let effects = press(&mut state, &[KeyCode::Backspace]);
    assert_eq!(requests(&effects), [BrowseRequest::Feeds]);
    assert!(state.episodes.is_none());
    assert_eq!(state.cursor, 1);

    let effects = press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Radio);
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Request(BrowseRequest::Stations)]
        ),
        "{effects:?}"
    );

    let effects = press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Files);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Directory(path))] if *path == root),
        "{effects:?}"
    );
}

fn screen(state: &BrowserState) -> (String, ratatui::buffer::Buffer) {
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap_or_else(|e| panic!("{e}"));
    let mut ui = UiState::new(true);
    ui.overlay = Overlay::Browser;
    let visuals = Visuals {
        browser: Some(state),
        ..Visuals::default()
    };
    terminal
        .draw(|frame| {
            draw(frame, &views::sample_view(), &ui, &visuals);
        })
        .unwrap_or_else(|e| panic!("{e}"));
    let buffer = terminal.backend().buffer().clone();
    let width = usize::from(buffer.area.width);
    let text = buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    (text, buffer)
}

#[test]
fn the_overlay_draws_safe_names_marks_and_dimmed_unplayable_episodes() {
    let root = PathBuf::from("/music");
    let mut state = BrowserState::new(root.clone(), dest());
    let (text, _) = screen(&state);
    assert!(text.contains("Loading"), "{text}");

    state.apply(BrowseResult::Directory {
        path: root.clone(),
        entries: Ok(vec![
            DirEntry {
                name: "albums".to_owned(),
                path: root.join("albums"),
                kind: EntryKind::Directory,
                media: None,
            },
            DirEntry {
                name: "evil\u{1b}[2Jname.mp3".to_owned(),
                path: root.join("evil.mp3"),
                kind: EntryKind::Audio,
                media: None,
            },
        ]),
    });
    press(&mut state, &[KeyCode::Down, KeyCode::Char(' ')]);
    let (text, buffer) = screen(&state);
    assert!(text.contains("/music"), "{text}");
    assert!(text.contains("albums/"), "{text}");
    assert!(text.contains("evil\\u{1b}[2Jname.mp3"), "{text}");
    assert!(
        !buffer
            .content()
            .iter()
            .any(|cell| cell.symbol().contains('\u{1b}'))
    );
    assert!(text.contains('●'), "the mark is drawn: {text}");

    press(&mut state, &[KeyCode::Tab]);
    state.apply(BrowseResult::Feeds(Ok(vec![feed("one")])));
    press(&mut state, &[KeyCode::Enter]);
    state.apply(BrowseResult::Episodes {
        slug: "one".to_owned(),
        episodes: Ok(vec![
            episode("playable", Some("https://example.com/1.mp3")),
            episode("gone", None),
        ]),
    });
    let (text, buffer) = screen(&state);
    assert!(text.contains("one title"), "{text}");
    let gone_row = text
        .lines()
        .position(|line| line.contains("gone"))
        .unwrap_or_else(|| panic!("the unplayable episode is listed: {text}"));
    let gone_y = u16::try_from(gone_row).unwrap_or_else(|e| panic!("{e}"));
    let dimmed = (0..buffer.area.width).any(|x| {
        buffer
            .cell((x, gone_y))
            .is_some_and(|cell| cell.symbol() == "g" && cell.modifier.contains(Modifier::DIM))
    });
    assert!(dimmed, "an episode without an enclosure is dimmed");
}

#[test]
fn the_worker_reports_failures_as_error_values() {
    let worker = BrowseWorker::spawn(None);
    let missing = PathBuf::from("/nonexistent/m5-browser-worker");
    worker.request(BrowseRequest::Directory(missing.clone()));
    worker.request(BrowseRequest::Feeds);
    worker.request(BrowseRequest::Episodes {
        slug: "radio-t".to_owned(),
    });
    let mut results = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while results.len() < 3 {
        assert!(std::time::Instant::now() < deadline, "{results:?}");
        match worker.try_result() {
            Some(result) => results.push(result),
            None => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
    assert!(
        matches!(
            &results[..],
            [
                BrowseResult::Directory { path, entries: Err(_) },
                BrowseResult::Feeds(Err(_)),
                BrowseResult::Episodes { slug, episodes: Err(_) },
            ] if *path == missing && slug == "radio-t"
        ),
        "{results:?}"
    );
}

/// A Podcasts tab showing `feeds`.
fn podcasts(feeds: Vec<FeedSummary>) -> BrowserState {
    let mut state = BrowserState::new(PathBuf::from("/music"), dest());
    press(&mut state, &[KeyCode::Tab]);
    state.apply(BrowseResult::Feeds(Ok(feeds)));
    state
}

/// A Radio tab showing `stations`.
fn radio(stations: Vec<StationRow>) -> BrowserState {
    let mut state = BrowserState::new(PathBuf::from("/music"), dest());
    press(&mut state, &[KeyCode::Tab, KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Radio);
    state.apply(BrowseResult::Stations(Ok(stations)));
    state
}

/// A `StationRow` whose `media` is derived exactly as
/// `library::station_identity_of` derives it (private to `library.rs`, so
/// reproduced here from its own two public steps): `Url::parse(url)`, then
/// `NormalizedUrl::parse` on *that parsed URL's* serialized text — the
/// canonical spelling, not the raw `url` argument.
fn station_row(slug: &str, url: &str) -> StationRow {
    let parsed = url::Url::parse(url).unwrap_or_else(|error| panic!("url: {error}"));
    let media = MediaId::RemoteUrl(
        NormalizedUrl::parse(parsed.as_str()).unwrap_or_else(|error| panic!("normalize: {error}")),
    );
    StationRow {
        slug: slug.to_owned(),
        url: parsed,
        media,
        identity: None,
    }
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

fn mutation(request: BrowseRequest, outcome: Result<&str, &str>) -> BrowseResult {
    BrowseResult::Mutation {
        request,
        outcome: outcome.map(str::to_owned).map_err(str::to_owned),
    }
}

#[test]
fn the_prompt_swallows_shortcuts_and_enter_subscribes() {
    let mut state = podcasts(vec![feed("one")]);
    assert!(press(&mut state, &[KeyCode::Char('a')]).is_empty());
    assert_eq!(state.prompt.as_deref(), Some(""));

    // `q`, `b`, space and `d` are text now, not shortcuts.
    let typed = press(
        &mut state,
        &[
            KeyCode::Char('q'),
            KeyCode::Char('b'),
            KeyCode::Char(' '),
            KeyCode::Char('d'),
        ],
    );
    assert!(typed.is_empty(), "{typed:?}");
    assert_eq!(state.prompt.as_deref(), Some("qb d"));
    assert!(state.confirm.is_none());

    press(
        &mut state,
        &[
            KeyCode::Backspace,
            KeyCode::Backspace,
            KeyCode::Backspace,
            KeyCode::Backspace,
        ],
    );
    assert!(
        press(&mut state, &[KeyCode::Enter]).is_empty(),
        "empty submits nothing"
    );
    assert!(state.prompt.is_none());

    press(&mut state, &[KeyCode::Char('a'), KeyCode::Char('x')]);
    assert!(
        press(&mut state, &[KeyCode::Esc]).is_empty(),
        "Esc cancels, never closes"
    );
    assert!(state.prompt.is_none());

    press(&mut state, &[KeyCode::Char('a')]);
    for c in "https://x.example/f ".chars() {
        press(&mut state, &[KeyCode::Char(c)]);
    }
    let effects = press(&mut state, &[KeyCode::Enter]);
    let expected = BrowseRequest::Subscribe {
        url: "https://x.example/f".to_owned(),
    };
    assert_eq!(requests(&effects), vec![expected.clone()]);
    assert_eq!(state.pending, Some(expected));
    assert_eq!(
        state.notice.as_ref().map(|n| n.kind),
        Some(NoticeKind::Working)
    );
    assert!(state.prompt.is_none());
}

#[test]
fn refresh_and_remove_need_a_feed_and_no_pending_mutation() {
    let mut empty = podcasts(Vec::new());
    for code in [KeyCode::Char('r'), KeyCode::Char('R'), KeyCode::Char('d')] {
        assert!(press(&mut empty, &[code]).is_empty(), "{code:?}");
    }
    assert!(empty.confirm.is_none());

    let mut state = podcasts(vec![feed("one"), feed("two")]);
    press(&mut state, &[KeyCode::Down]);
    let effects = press(&mut state, &[KeyCode::Char('r')]);
    let expected = BrowseRequest::Refresh {
        slug: Some("two".to_owned()),
    };
    assert_eq!(requests(&effects), vec![expected.clone()]);
    assert_eq!(state.pending, Some(expected.clone()));

    // Everything management-related waits while a mutation is pending.
    for code in [
        KeyCode::Char('a'),
        KeyCode::Char('r'),
        KeyCode::Char('R'),
        KeyCode::Char('d'),
    ] {
        assert!(press(&mut state, &[code]).is_empty(), "{code:?}");
    }
    assert!(state.prompt.is_none() && state.confirm.is_none());

    let mut all = podcasts(vec![feed("one")]);
    assert_eq!(
        requests(&press(&mut all, &[KeyCode::Char('R')])),
        [BrowseRequest::Refresh { slug: None }]
    );
}

#[test]
fn remove_asks_first_and_only_y_confirms() {
    let mut state = podcasts(vec![feed("one"), feed("two")]);
    assert!(press(&mut state, &[KeyCode::Char('d')]).is_empty());
    assert_eq!(state.confirm.as_deref(), Some("one"));
    assert!(press(&mut state, &[KeyCode::Char('n')]).is_empty());
    assert!(state.confirm.is_none() && state.pending.is_none());

    press(&mut state, &[KeyCode::Char('d')]);
    let effects = press(&mut state, &[KeyCode::Char('y')]);
    let expected = BrowseRequest::Unsubscribe {
        slug: "one".to_owned(),
    };
    assert_eq!(requests(&effects), vec![expected.clone()]);
    assert_eq!(state.pending, Some(expected));
}

#[test]
fn r_and_d_inside_an_open_feed_act_on_that_feed() {
    let mut state = podcasts(vec![feed("one"), feed("two")]);
    press(&mut state, &[KeyCode::Down, KeyCode::Enter]);
    state.apply(BrowseResult::Episodes {
        slug: "two".to_owned(),
        episodes: Ok(vec![episode("g1", Some("https://cdn.example.org/1.mp3"))]),
    });
    assert_eq!(
        requests(&press(&mut state, &[KeyCode::Char('r')])),
        [BrowseRequest::Refresh {
            slug: Some("two".to_owned())
        }]
    );
    let follow_up = state.apply(mutation(
        BrowseRequest::Refresh {
            slug: Some("two".to_owned()),
        },
        Ok("two: updated"),
    ));
    assert_eq!(
        follow_up,
        Some(BrowseRequest::Episodes {
            slug: "two".to_owned()
        }),
        "an open feed re-reads its episodes"
    );
    // The re-read is loading until its answer lands; management keys wait.
    assert!(press(&mut state, &[KeyCode::Char('d')]).is_empty());
    assert!(state.confirm.is_none());
    state.apply(BrowseResult::Episodes {
        slug: "two".to_owned(),
        episodes: Ok(Vec::new()),
    });
    press(&mut state, &[KeyCode::Char('d')]);
    assert_eq!(state.confirm.as_deref(), Some("two"));
}

#[test]
fn back_re_reads_the_feed_list_while_keeping_the_cached_rows() {
    let mut state = podcasts(vec![feed("one"), feed("two")]);
    press(&mut state, &[KeyCode::Down, KeyCode::Enter]);
    state.apply(BrowseResult::Episodes {
        slug: "two".to_owned(),
        episodes: Ok(Vec::new()),
    });
    let effects = press(&mut state, &[KeyCode::Backspace]);
    assert_eq!(requests(&effects), [BrowseRequest::Feeds]);
    assert!(state.episodes.is_none());
    assert_eq!(state.feeds.len(), 2, "cached rows stay up");
    assert_eq!(state.cursor, 1);
    assert!(!state.loading);
}

#[test]
fn a_matching_answer_shows_the_notice_and_re_reads_the_list() {
    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Char('r')]);
    let request = BrowseRequest::Refresh {
        slug: Some("one".to_owned()),
    };

    // Someone else's answer, and a listing answer, leave `pending` alone.
    assert_eq!(
        state.apply(mutation(BrowseRequest::Refresh { slug: None }, Ok("x"))),
        None
    );
    assert!(state.pending.is_some());

    let follow_up = state.apply(mutation(request.clone(), Ok("one: updated")));
    assert_eq!(follow_up, Some(BrowseRequest::Feeds));
    assert!(state.pending.is_none());
    assert!(state.loading);
    let notice = state.notice.clone().expect("a notice");
    assert_eq!(
        (notice.text.as_str(), notice.kind),
        ("one: updated", NoticeKind::Ok)
    );

    // The listing answer settles loading but keeps the notice.
    state.apply(BrowseResult::Feeds(Ok(vec![feed("one")])));
    assert!(!state.loading);
    assert_eq!(
        state.notice.as_ref().map(|n| n.text.as_str()),
        Some("one: updated")
    );

    press(&mut state, &[KeyCode::Char('r')]);
    state.apply(mutation(request, Err("one: failed: boom")));
    assert_eq!(state.notice.as_ref().map(|n| n.kind), Some(NoticeKind::Err));

    // A fresh browser has nothing pending, so a late answer changes nothing.
    let mut fresh = podcasts(vec![feed("one")]);
    assert_eq!(
        fresh.apply(mutation(BrowseRequest::Refresh { slug: None }, Ok("late"))),
        None
    );
    assert!(fresh.notice.is_none());
}

#[test]
fn removing_the_open_feed_returns_to_the_list_whatever_the_outcome() {
    for outcome in [
        Ok("two: unsubscribed"),
        Err("two: the subscription was removed, but its cached episodes could not be deleted: x"),
        Err("unknown feed: two"),
    ] {
        let mut state = podcasts(vec![feed("one"), feed("two")]);
        press(&mut state, &[KeyCode::Down, KeyCode::Enter]);
        state.apply(BrowseResult::Episodes {
            slug: "two".to_owned(),
            episodes: Ok(Vec::new()),
        });
        press(&mut state, &[KeyCode::Char('d'), KeyCode::Char('y')]);
        let follow_up = state.apply(mutation(
            BrowseRequest::Unsubscribe {
                slug: "two".to_owned(),
            },
            outcome,
        ));
        assert!(state.episodes.is_none(), "{outcome:?}");
        assert_eq!(follow_up, Some(BrowseRequest::Feeds), "{outcome:?}");
        assert!(state.notice.is_some());
    }
}

#[test]
fn an_answer_on_the_files_tab_shows_the_notice_and_requests_nothing() {
    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Char('R')]);
    press(&mut state, &[KeyCode::Tab, KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Files);
    assert!(state.pending.is_some(), "a tab switch keeps the mutation");
    let follow_up = state.apply(mutation(BrowseRequest::Refresh { slug: None }, Ok("done")));
    assert_eq!(follow_up, None);
    assert_eq!(state.notice.as_ref().map(|n| n.text.as_str()), Some("done"));
}

#[test]
fn the_overlay_draws_the_prompt_the_question_and_the_notice_above_rows() {
    let mut state = podcasts(vec![feed("one"), feed("two")]);
    press(&mut state, &[KeyCode::Char('a')]);
    for c in "https://x.example/f".chars() {
        press(&mut state, &[KeyCode::Char(c)]);
    }
    let (text, _) = screen(&state);
    assert!(text.contains("Feed URL: https://x.example/f"), "{text}");
    assert!(text.contains("a subscribe"), "podcasts hint: {text}");
    press(&mut state, &[KeyCode::Esc, KeyCode::Char('d')]);
    let (text, _) = screen(&state);
    assert!(text.contains("Remove one? y/N"), "{text}");
    press(&mut state, &[KeyCode::Char('y')]);
    let (text, _) = screen(&state);
    assert!(text.contains("Removing…"), "{text}");

    state.apply(mutation(
        BrowseRequest::Unsubscribe {
            slug: "one".to_owned(),
        },
        Ok("one: unsubscribed"),
    ));
    state.apply(BrowseResult::Feeds(Ok(vec![feed("two")])));
    let (text, _) = screen(&state);
    let notice_row = text
        .lines()
        .position(|line| line.contains("one: unsubscribed"))
        .unwrap_or_else(|| panic!("no notice: {text}"));
    let feed_row = text
        .lines()
        .position(|line| line.contains("two title"))
        .unwrap_or_else(|| panic!("no rows: {text}"));
    assert!(notice_row < feed_row, "notice above the rows: {text}");

    // An empty feed list points at the key, not the CLI.
    let empty = podcasts(Vec::new());
    let (text, _) = screen(&empty);
    assert!(text.contains("press a to add a feed URL"), "{text}");
}

#[test]
fn a_long_notice_is_cut_to_a_third_of_the_list_with_a_marker() {
    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Char('R')]);
    let lines: Vec<String> = (1..=8).map(|n| format!("line{n}")).collect();
    state.apply(mutation(
        BrowseRequest::Refresh { slug: None },
        Err(&lines.join("\n")),
    ));
    state.apply(BrowseResult::Feeds(Ok(vec![feed("one")])));
    let (text, buffer) = screen(&state);
    // The 90×24 screen gives the list 17 rows; a third is 5, the marker takes the fifth.
    assert!(text.contains("line4"), "{text}");
    assert!(!text.contains("line5"), "{text}");
    assert!(text.contains("+4 more lines, see log"), "{text}");
    assert!(text.contains("one title"), "rows still drawn: {text}");
    let marker_row = text
        .lines()
        .position(|line| line.contains("+4 more lines"))
        .unwrap_or_else(|| panic!("{text}"));
    let amber = buffer[(4, u16::try_from(marker_row).unwrap_or(0))].fg;
    let ok_state = {
        let mut s = podcasts(vec![feed("one")]);
        press(&mut s, &[KeyCode::Char('R')]);
        s.apply(mutation(BrowseRequest::Refresh { slug: None }, Ok("fine")));
        s
    };
    let (ok_text, ok_buffer) = screen(&ok_state);
    let ok_row = ok_text
        .lines()
        .position(|line| line.contains("fine"))
        .unwrap_or_else(|| panic!("{ok_text}"));
    let muted = ok_buffer[(4, u16::try_from(ok_row).unwrap_or(0))].fg;
    assert_ne!(amber, muted, "errors and successes differ in color");
}

#[test]
fn enter_on_a_queued_row_removes_it_and_marks_skip_queued_rows() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    let ids = views::ids();
    let a_flac = state.entries[1]
        .media
        .clone()
        .unwrap_or_else(|| panic!("audio has an identity"));
    state.sync_queue(&[queue_row(ids[0], a_flac)]);

    // The tick is the acknowledgement that the row is in the queue.
    let (text, _) = screen(&state);
    let row = text
        .lines()
        .find(|line| line.contains("a.flac"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(row.contains('✓'), "{row}");
    assert!(
        !text
            .lines()
            .any(|line| line.contains("b.MP3") && line.contains('✓'))
    );

    press(&mut state, &[KeyCode::Down]);
    let effects = press(&mut state, &[KeyCode::Enter]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Remove(id)] if *id == ids[0]),
        "{effects:?}"
    );

    // Marked together with an unqueued row, only the unqueued one is added.
    press(
        &mut state,
        &[KeyCode::Char(' '), KeyCode::Down, KeyCode::Char(' ')],
    );
    let effects = press(&mut state, &[KeyCode::Enter]);
    match &effects[..] {
        [BrowserEffect::Enqueue { items, .. }] => {
            assert_eq!(items.len(), 1, "{items:?}");
            assert!(
                matches!(&items[0], EnqueueItem::Path(path) if path.ends_with("b.MP3")),
                "{items:?}"
            );
        }
        other => panic!("{other:?}"),
    }

    // Once the queue no longer holds it, Enter adds it again.
    state.sync_queue(&[]);
    press(&mut state, &[KeyCode::Up]);
    let effects = press(&mut state, &[KeyCode::Enter]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Enqueue { .. }]),
        "{effects:?}"
    );
}

#[test]
fn a_queued_episode_is_ticked_and_enter_removes_it() {
    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Enter]);
    let queued = episode("g1", Some("https://cdn.example.org/1.mp3"));
    state.apply(BrowseResult::Episodes {
        slug: "one".to_owned(),
        episodes: Ok(vec![
            queued.clone(),
            episode("g2", Some("https://cdn.example.org/2.mp3")),
        ]),
    });
    let ids = views::ids();
    state.sync_queue(&[queue_row(ids[2], queued.media.clone())]);
    let (text, _) = screen(&state);
    assert!(
        text.lines()
            .any(|line| line.contains("g1") && line.contains('✓')),
        "{text}"
    );
    assert!(
        !text
            .lines()
            .any(|line| line.contains("g2") && line.contains('✓'))
    );
    let effects = press(&mut state, &[KeyCode::Enter]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Remove(id)] if *id == ids[2]),
        "{effects:?}"
    );
}

#[test]
fn episodes_list_newest_first_with_undated_ones_last() {
    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Enter]);
    state.apply(BrowseResult::Episodes {
        slug: "one".to_owned(),
        episodes: Ok(vec![
            dated("old", 1_000),
            episode("undated-a", None),
            dated("new", 2_000),
            episode("undated-b", None),
        ]),
    });
    let order: Vec<Option<&str>> = state
        .episodes
        .as_ref()
        .map(|(_, episodes)| episodes.iter().map(guid_of).collect())
        .unwrap_or_default();
    assert_eq!(
        order,
        [
            Some("new"),
            Some("old"),
            Some("undated-a"),
            Some("undated-b")
        ]
    );
}

// --- M7.1 §7: the Radio tab -----------------------------------------------

#[test]
fn tab_cycles_files_podcasts_radio_and_back() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    assert_eq!(state.tab, BrowserTab::Files);

    let effects = press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Podcasts);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Feeds)]),
        "{effects:?}"
    );

    let effects = press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Radio);
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Request(BrowseRequest::Stations)]
        ),
        "{effects:?}"
    );

    let effects = press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Files);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Directory(path))] if *path == root),
        "{effects:?}"
    );
}

#[test]
fn a_on_the_radio_tab_opens_the_url_prompt_and_enter_sends_add_station() {
    let mut state = radio(Vec::new());
    assert!(press(&mut state, &[KeyCode::Char('a')]).is_empty());
    assert_eq!(state.prompt.as_deref(), Some(""));

    for c in "https://x.example/stream".chars() {
        press(&mut state, &[KeyCode::Char(c)]);
    }
    let effects = press(&mut state, &[KeyCode::Enter]);
    let expected = BrowseRequest::AddStation {
        url: "https://x.example/stream".to_owned(),
    };
    assert_eq!(requests(&effects), vec![expected.clone()]);
    assert_eq!(state.pending, Some(expected));
    assert_eq!(
        state.notice.as_ref().map(|n| n.text.as_str()),
        Some("Adding…")
    );
}

#[test]
fn r_and_d_then_y_on_the_radio_tab_send_reprobe_and_remove() {
    let mut state = radio(vec![
        station_row("one", "https://one.example/stream"),
        station_row("two", "https://two.example/stream"),
    ]);
    press(&mut state, &[KeyCode::Down]);
    let effects = press(&mut state, &[KeyCode::Char('r')]);
    let expected = BrowseRequest::ReprobeStation {
        slug: "two".to_owned(),
    };
    assert_eq!(requests(&effects), vec![expected.clone()]);
    assert_eq!(state.pending, Some(expected.clone()));
    assert_eq!(
        state.notice.as_ref().map(|n| n.text.as_str()),
        Some("Re-probing…")
    );

    // Clear the pending mutation before exercising `d`.
    state.apply(mutation(expected, Ok("two: reprobed")));
    state.apply(BrowseResult::Stations(Ok(vec![
        station_row("one", "https://one.example/stream"),
        station_row("two", "https://two.example/stream"),
    ])));

    press(&mut state, &[KeyCode::Char('d')]);
    assert_eq!(state.confirm.as_deref(), Some("two"));
    let effects = press(&mut state, &[KeyCode::Char('y')]);
    let expected = BrowseRequest::RemoveStation {
        slug: "two".to_owned(),
    };
    assert_eq!(requests(&effects), vec![expected.clone()]);
    assert_eq!(state.pending, Some(expected));
    assert_eq!(
        state.notice.as_ref().map(|n| n.text.as_str()),
        Some("Removing…")
    );
}

#[test]
fn every_station_management_key_is_refused_while_pending() {
    let mut state = radio(vec![station_row("one", "https://one.example/stream")]);
    press(&mut state, &[KeyCode::Char('r')]);
    assert!(state.pending.is_some());

    for code in [KeyCode::Char('a'), KeyCode::Char('r'), KeyCode::Char('d')] {
        assert!(press(&mut state, &[code]).is_empty(), "{code:?}");
    }
    assert!(state.prompt.is_none() && state.confirm.is_none());
}

#[test]
fn refresh_all_does_nothing_on_the_radio_tab() {
    // `can_manage` no longer implies Podcasts once Radio is added, so `R`
    // needs its own guard; without it this would submit a `Refresh` the
    // Radio tab has no business sending.
    let mut state = radio(vec![station_row("one", "https://one.example/stream")]);
    assert!(press(&mut state, &[KeyCode::Char('R')]).is_empty());
    assert!(state.pending.is_none());
}

#[test]
fn enter_on_a_station_enqueues_and_enter_again_removes_it() {
    let url = "https://one.example/stream";
    let mut state = radio(vec![station_row("one", url)]);
    let effects = press(&mut state, &[KeyCode::Enter]);
    let (media, _) = resolve_source(url).unwrap_or_else(|error| panic!("resolve: {error}"));
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Enqueue { items, .. }] if matches!(
                &items[..],
                [EnqueueItem::Station { url: enqueued, .. }] if enqueued == url
            )
        ),
        "{effects:?}"
    );

    let ids = views::ids();
    state.sync_queue(&[queue_row(ids[0], media)]);
    let effects = press(&mut state, &[KeyCode::Enter]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Remove(id)] if *id == ids[0]),
        "{effects:?}"
    );
}

#[test]
fn a_queued_station_draws_a_tick() {
    let row = station_row("one", "https://one.example/stream");
    let mut state = radio(vec![row.clone()]);
    let ids = views::ids();
    state.sync_queue(&[queue_row(ids[0], row.media.clone())]);

    let (text, _) = screen(&state);
    let line = text
        .lines()
        .find(|line| line.contains("one"))
        .unwrap_or_else(|| panic!("no row for the station: {text}"));
    assert!(line.contains('✓'), "{line}");
}

/// A hostile station can put anything it likes in `icy-genre` (M7.1 §5
/// probes it straight off the response header); `StationIdentity` carries
/// its own doc comment obligation ("the caller escapes it before drawing
/// it"). This is the same terminal-injection hazard
/// `the_overlay_draws_safe_names_marks_and_dimmed_unplayable_episodes`
/// covers for a directory entry's name — an unescaped ESC byte reaching a
/// real terminal can rewrite rows the user never asked to see. The name
/// field is not exercised here: the row renderer never draws it (spec §7
/// — the slug already stands for the station's name), so it carries no
/// injection risk of its own.
#[test]
fn a_stations_identity_text_is_escaped() {
    // Short enough to survive the identity column's width whole (unlike the
    // label, the identity detail is right-aligned; bitrate is left out so
    // the joined detail is exactly this string, with nothing to hide it).
    let mut row = station_row("one", "https://one.example/stream");
    row.identity = Some(StationIdentity {
        name: None,
        genre: Some("ev\u{1b}il".to_owned()),
        bitrate_kbps: None,
        logo: None,
    });
    let state = radio(vec![row]);

    let (text, buffer) = screen(&state);
    assert!(text.contains("ev\\u{1b}il"), "{text}");
    assert!(
        !buffer
            .content()
            .iter()
            .any(|cell| cell.symbol().contains('\u{1b}')),
        "no raw escape byte reaches the buffer"
    );
}

/// Spec §7: "A row draws the slug, then the identity: `Lofi · 128 kbps`,
/// with genre and bitrate omitted when absent." Name is set equal to genre
/// here deliberately — a station legitimately naming its genre after itself
/// — to prove the renderer does not draw the name a second time, which a
/// weaker assertion (a substring that also matches a deduplicated *or* a
/// clipped, non-deduplicated string) would not catch.
#[test]
fn a_verified_row_draws_its_slug_then_genre_and_bitrate() {
    let mut row = station_row("lofi", "https://one.example/stream");
    row.identity = Some(StationIdentity {
        name: Some("Lofi".to_owned()),
        genre: Some("Lofi".to_owned()),
        bitrate_kbps: Some(128),
        logo: None,
    });
    let state = radio(vec![row]);

    let (text, _) = screen(&state);
    let line = text
        .lines()
        .find(|line| line.contains("lofi"))
        .unwrap_or_else(|| panic!("no row for the station: {text}"));
    assert!(line.contains("lofi"), "the slug is drawn: {line}");
    assert!(
        line.contains("Lofi · 128 kbps"),
        "genre and bitrate are drawn whole, unclipped: {line}"
    );
    assert!(
        !line.contains("Lofi · Lofi"),
        "the name is not drawn a second time: {line}"
    );
}

#[test]
fn a_verified_row_with_only_a_bitrate_draws_just_the_bitrate() {
    let mut row = station_row("one", "https://one.example/stream");
    row.identity = Some(StationIdentity {
        name: Some("Anything".to_owned()),
        genre: None,
        bitrate_kbps: Some(128),
        logo: None,
    });
    let state = radio(vec![row]);

    let (text, _) = screen(&state);
    let line = text
        .lines()
        .find(|line| line.contains("one"))
        .unwrap_or_else(|| panic!("no row for the station: {text}"));
    assert!(line.contains("128 kbps"), "{line}");
    assert!(
        !line.contains('·'),
        "no separator with nothing to join: {line}"
    );
}

#[test]
fn a_verified_row_with_only_a_genre_draws_just_the_genre() {
    let mut row = station_row("one", "https://one.example/stream");
    row.identity = Some(StationIdentity {
        name: Some("Anything".to_owned()),
        genre: Some("Lofi".to_owned()),
        bitrate_kbps: None,
        logo: None,
    });
    let state = radio(vec![row]);

    let (text, _) = screen(&state);
    let line = text
        .lines()
        .find(|line| line.contains("one"))
        .unwrap_or_else(|| panic!("no row for the station: {text}"));
    assert!(line.contains("Lofi"), "{line}");
    assert!(!line.contains("kbps"), "no bitrate to join: {line}");
    assert!(
        !line.contains('·'),
        "no separator with nothing to join: {line}"
    );
}

/// `RADIO_DETAIL_COLUMNS` is 24; a genre long enough that `genre · 128 kbps`
/// overflows it must be truncated from the right (front kept, tail
/// dropped), never left-clipped the way `ratatui`'s own right-aligned
/// overflow handling would clip it.
#[test]
fn a_long_ascii_genre_is_truncated_from_the_right_not_clipped_from_the_left() {
    let mut row = station_row("one", "https://one.example/stream");
    row.identity = Some(StationIdentity {
        name: None,
        genre: Some("Extremely Long Genre Name".to_owned()),
        bitrate_kbps: Some(128),
        logo: None,
    });
    let state = radio(vec![row]);

    let (text, _) = screen(&state);
    let line = text
        .lines()
        .find(|line| line.contains("one"))
        .unwrap_or_else(|| panic!("no row for the station: {text}"));
    assert!(
        line.contains("Extremely Long Genre Na"),
        "the genre's leading characters survive: {line}"
    );
    assert!(
        !line.contains("Genre Name"),
        "the genre's trailing characters are dropped: {line}"
    );
    assert!(
        !line.contains("kbps"),
        "the genre alone already overflows the column, so the joined \
         bitrate never appears at all: {line}"
    );
}

/// The same overflow, but with full-width characters: `.chars().count()`
/// would undercount a CJK genre's display width and let it run past
/// `RADIO_DETAIL_COLUMNS`, right back into `ratatui`'s left-clip. Distinct
/// leading and trailing characters (`統` once, then `一` repeated) so a
/// left-clip — which would drop the front and keep some of the repeated
/// tail — is distinguishable from the intended right-truncation.
#[test]
fn a_wide_character_genre_is_not_left_clipped() {
    let mut row = station_row("wide", "https://one.example/stream");
    let genre = format!("統{}", "一".repeat(19));
    row.identity = Some(StationIdentity {
        name: None,
        genre: Some(genre),
        bitrate_kbps: None,
        logo: None,
    });
    let state = radio(vec![row]);

    let (text, _) = screen(&state);
    let line = text
        .lines()
        .find(|line| line.contains("wide"))
        .unwrap_or_else(|| panic!("no row for the station: {text}"));
    assert!(
        line.contains("wide"),
        "the row still starts with the slug: {line}"
    );
    assert!(
        line.contains('統'),
        "the genre's leading (and distinct) character is not left-clipped: {line}"
    );
}

#[test]
fn an_unverified_station_draws_its_url_and_an_unreached_marker() {
    let row = station_row("one", "https://one.example/stream");
    let state = radio(vec![row]);

    let (text, _) = screen(&state);
    let line = text
        .lines()
        .find(|line| line.contains("one.example"))
        .unwrap_or_else(|| panic!("no row for the station: {text}"));
    assert!(
        line.contains("https://one.example/stream"),
        "the url is drawn: {line}"
    );
    assert!(line.contains("(unreached)"), "{line}");
}

#[test]
fn an_empty_radio_tab_says_so_and_the_hints_name_three_tabs() {
    let state = radio(Vec::new());
    let (text, _) = screen(&state);
    assert!(
        text.contains("No saved stations — press a to add a stream URL"),
        "{text}"
    );
    assert!(text.contains("files/podcasts/radio"), "{text}");
}

#[test]
fn the_tab_bar_names_all_three_tabs() {
    let (_dir, root) = sample_dir();
    let files_state = listed(&root);
    let (files_text, _) = screen(&files_state);
    for label in [" Files ", " Podcasts ", " Radio "] {
        assert!(files_text.contains(label), "{files_text}");
    }

    let radio_state = radio(Vec::new());
    let (radio_text, _) = screen(&radio_state);
    for label in [" Files ", " Podcasts ", " Radio "] {
        assert!(radio_text.contains(label), "{radio_text}");
    }
}

#[test]
fn the_radio_prompt_asks_for_a_stream_url() {
    let mut state = radio(Vec::new());
    press(&mut state, &[KeyCode::Char('a')]);
    let (text, _) = screen(&state);
    assert!(text.contains("Stream URL: "), "{text}");

    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Char('a')]);
    let (text, _) = screen(&state);
    assert!(text.contains("Feed URL: "), "{text}");
}

#[test]
fn a_stations_answer_for_a_tab_already_left_is_dropped() {
    let mut state = radio(vec![station_row("one", "https://one.example/stream")]);
    press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Files);
    state.apply(BrowseResult::Stations(Ok(vec![station_row(
        "late",
        "https://late.example/stream",
    )])));
    assert_eq!(state.stations.len(), 1);
    assert_eq!(state.stations[0].slug, "one");
}

#[test]
fn a_station_mutation_re_requests_the_visible_list() {
    // AddStation, through the prompt.
    let mut state = radio(vec![station_row("one", "https://one.example/stream")]);
    press(&mut state, &[KeyCode::Char('a')]);
    for c in "https://two.example/stream".chars() {
        press(&mut state, &[KeyCode::Char(c)]);
    }
    press(&mut state, &[KeyCode::Enter]);
    let follow_up = state.apply(mutation(
        BrowseRequest::AddStation {
            url: "https://two.example/stream".to_owned(),
        },
        Ok("two: added"),
    ));
    assert_eq!(follow_up, Some(BrowseRequest::Stations));
    assert!(state.loading);

    // ReprobeStation, through `r`.
    let mut state = radio(vec![station_row("one", "https://one.example/stream")]);
    press(&mut state, &[KeyCode::Char('r')]);
    let follow_up = state.apply(mutation(
        BrowseRequest::ReprobeStation {
            slug: "one".to_owned(),
        },
        Ok("one: reprobed"),
    ));
    assert_eq!(follow_up, Some(BrowseRequest::Stations));
    assert!(state.loading);

    // RemoveStation, through `d`, `y`.
    let mut state = radio(vec![station_row("one", "https://one.example/stream")]);
    press(&mut state, &[KeyCode::Char('d'), KeyCode::Char('y')]);
    let follow_up = state.apply(mutation(
        BrowseRequest::RemoveStation {
            slug: "one".to_owned(),
        },
        Ok("one: removed"),
    ));
    assert_eq!(follow_up, Some(BrowseRequest::Stations));
    assert!(state.loading);
}

#[test]
fn a_station_mutation_answer_on_another_tab_re_requests_nothing() {
    let mut state = radio(vec![station_row("one", "https://one.example/stream")]);
    press(&mut state, &[KeyCode::Char('d'), KeyCode::Char('y')]);
    press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Files);
    assert!(state.pending.is_some(), "a tab switch keeps the mutation");
    let follow_up = state.apply(mutation(
        BrowseRequest::RemoveStation {
            slug: "one".to_owned(),
        },
        Ok("one: removed"),
    ));
    assert_eq!(follow_up, None);
    assert_eq!(
        state.notice.as_ref().map(|n| n.text.as_str()),
        Some("one: removed")
    );
}

#[test]
fn a_station_queued_elsewhere_draws_a_tick_on_the_radio_tab() {
    // Adversarial: `Url::parse` rewrites a mixed-case host to lowercase, so
    // this spelling only proves the identity claim if both derivations —
    // `station_identity_of`'s canonical one (`station_row` above) and
    // `resolve_source`'s raw one (below) — agree on the rewrite. An
    // already-canonical URL would pass this test without exercising either
    // derivation's own normalization at all.
    let raw = "https://Radio.Example/Stream";
    let row = station_row("one", raw);
    let mut state = radio(vec![row.clone()]);

    let (elsewhere_media, _) =
        resolve_source(raw).unwrap_or_else(|error| panic!("resolve_source: {error}"));
    assert_eq!(
        elsewhere_media, row.media,
        "station_identity_of and resolve_source must derive the same MediaId \
         for a URL Url::parse rewrites, or a station added through the Radio \
         tab and the same URL enqueued elsewhere would draw no tick"
    );

    let ids = views::ids();
    state.sync_queue(&[queue_row(ids[0], elsewhere_media)]);
    assert_eq!(state.queued_at(0), Some(ids[0]));

    // The toggle's other half: Enter on a row that draws a tick removes it.
    let effects = press(&mut state, &[KeyCode::Enter]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Remove(id)] if *id == ids[0]),
        "{effects:?}"
    );
    state.sync_queue(&[]);
    assert_eq!(state.queued_at(0), None, "the queue is now empty");
}

/// A station URL usually ends in `/stream`, and that is what the playlist row
/// read before the enqueue carried the station's own name.
#[test]
fn a_station_enqueues_under_its_icy_name_or_else_its_slug() {
    let mut named = station_row("one", "https://one.example/stream");
    named.identity = Some(StationIdentity {
        name: Some("Radio One".to_owned()),
        ..StationIdentity::default()
    });
    let unnamed = station_row("two", "https://two.example/stream");
    for (row, expected) in [(named, "Radio One"), (unnamed, "two")] {
        let mut state = radio(vec![row]);
        let effects = press(&mut state, &[KeyCode::Enter]);
        assert!(
            matches!(
                &effects[..],
                [BrowserEffect::Enqueue { items, .. }] if matches!(
                    &items[..],
                    [EnqueueItem::Station { title, .. }] if title == expected
                )
            ),
            "{effects:?}"
        );
    }
}

#[test]
fn enter_on_an_unqueued_station_enqueues_it() {
    let row = station_row("one", "https://one.example/stream");
    let mut state = radio(vec![row.clone()]);
    assert_eq!(state.queued_at(0), None);

    let effects = press(&mut state, &[KeyCode::Enter]);
    let items = match &effects[..] {
        [BrowserEffect::Enqueue { items, .. }] => items,
        other => panic!("{other:?}"),
    };
    assert_eq!(items.len(), 1, "{items:?}");
    let media = match &items[0] {
        EnqueueItem::Station { url, .. } => {
            resolve_source(url)
                .unwrap_or_else(|error| panic!("resolve: {error}"))
                .0
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        media, row.media,
        "the enqueued item's identity matches the row's"
    );

    let ids = views::ids();
    state.sync_queue(&[queue_row(ids[0], media)]);
    let (text, _) = screen(&state);
    let line = text
        .lines()
        .find(|line| line.contains("one"))
        .unwrap_or_else(|| panic!("no row for the station: {text}"));
    assert!(line.contains('✓'), "the row now draws a tick: {line}");
}
