//! The on-demand browser's own state and keys (design doc M5 §8): a Files
//! tab over one local directory at a time and a Podcasts tab over the cached
//! feeds and a feed's episodes. Everything here is a pure function over a
//! [`KeyEvent`] or a [`BrowseResult`]; reading the filesystem is
//! [`crate::application::browse`]'s worker's job, and `tui::run` is the only
//! thing that executes a [`BrowserEffect`].
//!
//! The visible list is always the answer to the latest request: moving to
//! another directory, feed or tab empties the list and marks it loading, and
//! an answer for anywhere else is dropped.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::application::browse::{BrowseRequest, BrowseResult, DirEntry, EntryKind};
use crate::application::runtime::EnqueueItem;
use crate::application::view::QueueRow;
use crate::library::{EpisodeCandidate, FeedSummary, StationRow};
use crate::media::id::MediaId;
use crate::playlist::PlaylistId;
use crate::queue::QueueEntryId;
use crate::tui::input::blocks_ordinary_bindings;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserTab {
    Files,
    Podcasts,
    /// The Radio tab: saved stations (M7.1 §7).
    Radio,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticeKind {
    /// A mutation is in flight.
    Working,
    Ok,
    Err,
}

/// A mutation's progress or outcome (design doc M6 §5), drawn above the
/// rows and kept until the next key that changes what is shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notice {
    pub text: String,
    pub kind: NoticeKind,
}

#[derive(Clone, Debug)]
pub struct BrowserState {
    pub tab: BrowserTab,
    /// The directory the Files tab shows; absolute, so its parent is known.
    pub cwd: PathBuf,
    pub entries: Vec<DirEntry>,
    pub feeds: Vec<FeedSummary>,
    /// The feed being viewed, by slug, and its episodes; `None` while the
    /// Podcasts tab shows the feed list.
    pub episodes: Option<(String, Vec<EpisodeCandidate>)>,
    /// The Radio tab's saved stations (M7.1 §7).
    pub stations: Vec<StationRow>,
    /// An index into the visible list.
    pub cursor: usize,
    /// Indices into the visible list, only ever of markable rows.
    pub marked: BTreeSet<usize>,
    /// The playlist every add from this browser lands in, captured once when
    /// it opened: one destination for the whole session, however many adds
    /// it makes (M8 §8, "One destination rule").
    pub dest: PlaylistId,
    /// Whether the visible list still waits on its request.
    pub loading: bool,
    /// Why the visible list could not be read.
    pub error: Option<String>,
    /// The feed URL being typed after `a`.
    pub prompt: Option<String>,
    /// The slug whose removal awaits `y`.
    pub confirm: Option<String>,
    /// The mutation in flight: blocks a second one and identifies its
    /// answer (M6 §5).
    pub pending: Option<BrowseRequest>,
    pub notice: Option<Notice>,
    /// What the queue holds, by media identity, as of the last
    /// [`sync_queue`](Self::sync_queue): a queued row draws a tick and Enter
    /// removes it instead of adding it again.
    pub queued: HashMap<MediaId, QueueEntryId>,
}

#[derive(Clone, Debug)]
pub enum BrowserEffect {
    Request(BrowseRequest),
    Enqueue {
        dest: PlaylistId,
        items: Vec<EnqueueItem>,
    },
    /// Enter on a row already in the queue takes it out again.
    Remove(QueueEntryId),
    Close,
}

impl BrowserState {
    /// A Files tab at `cwd`, loading: the caller issues
    /// `BrowseRequest::Directory(cwd)` alongside. `dest` is captured for the
    /// life of this browser: every add it makes, files or a folder walk
    /// alike, lands there (M8 §8).
    pub fn new(cwd: PathBuf, dest: PlaylistId) -> Self {
        Self {
            tab: BrowserTab::Files,
            cwd,
            entries: Vec::new(),
            feeds: Vec::new(),
            episodes: None,
            stations: Vec::new(),
            cursor: 0,
            marked: BTreeSet::new(),
            dest,
            loading: true,
            error: None,
            prompt: None,
            confirm: None,
            pending: None,
            notice: None,
            queued: HashMap::new(),
        }
    }

    /// Records which media the queue holds; with duplicates, the later row
    /// is the one Enter removes.
    pub fn sync_queue(&mut self, rows: &[QueueRow]) {
        self.queued = rows.iter().map(|row| (row.media.clone(), row.id)).collect();
    }

    /// The queue entry row `index` is already in, if any.
    pub fn queued_at(&self, index: usize) -> Option<QueueEntryId> {
        let media = match (self.tab, &self.episodes) {
            (BrowserTab::Files, _) => self.entries.get(index)?.media.as_ref()?,
            (BrowserTab::Podcasts, None) => return None,
            (BrowserTab::Podcasts, Some((_, episodes))) => &episodes.get(index)?.media,
            (BrowserTab::Radio, _) => &self.stations.get(index)?.media,
        };
        self.queued.get(media).copied()
    }

    /// Takes in a worker's answer when it is for the list on screen or for
    /// the mutation in flight; otherwise — a directory already left, a feed
    /// no longer viewed, the other tab, a mutation from a browser since
    /// closed — drops it. Returns the follow-up read a mutation calls for.
    pub fn apply(&mut self, result: BrowseResult) -> Option<BrowseRequest> {
        let mut follow_up = None;
        match result {
            BrowseResult::Directory { path, entries } => {
                if self.tab == BrowserTab::Files && path == self.cwd {
                    self.entries = self.settle(entries);
                }
            }
            BrowseResult::Feeds(feeds) => {
                if self.tab == BrowserTab::Podcasts && self.episodes.is_none() {
                    self.feeds = self.settle(feeds);
                }
            }
            BrowseResult::Episodes { slug, episodes } => {
                let viewing = matches!(&self.episodes, Some((current, _)) if *current == slug);
                if self.tab == BrowserTab::Podcasts && viewing {
                    let mut list = self.settle(episodes);
                    // Newest first; undated episodes keep their stored order
                    // after the dated ones. The CLI keeps stored order so
                    // `play <slug> <index>` stays stable.
                    list.sort_by_key(|episode| std::cmp::Reverse(episode.published));
                    self.episodes = Some((slug, list));
                }
            }
            BrowseResult::Stations(stations) => {
                if self.tab == BrowserTab::Radio {
                    self.stations = self.settle(stations);
                }
            }
            BrowseResult::Mutation { request, outcome } => {
                // ponytail: an identical mutation resubmitted after a close
                // and reopen adopts the earlier answer; both committed, and
                // the list is re-read either way.
                if self.pending.as_ref() != Some(&request) {
                    return None;
                }
                self.pending = None;
                if let BrowseRequest::Unsubscribe { slug } = &request
                    && matches!(&self.episodes, Some((open, _)) if open == slug)
                {
                    // Removed before any cache cleanup could fail, and an
                    // unknown slug was already gone: the view has to go.
                    self.leave_episodes();
                }
                self.notice = Some(match outcome {
                    Ok(text) => Notice {
                        text,
                        kind: NoticeKind::Ok,
                    },
                    Err(text) => Notice {
                        text,
                        kind: NoticeKind::Err,
                    },
                });
                match self.tab {
                    BrowserTab::Podcasts => {
                        self.loading = true;
                        follow_up = Some(match &self.episodes {
                            Some((slug, _)) => BrowseRequest::Episodes { slug: slug.clone() },
                            None => BrowseRequest::Feeds,
                        });
                    }
                    BrowserTab::Radio => {
                        self.loading = true;
                        follow_up = Some(BrowseRequest::Stations);
                    }
                    BrowserTab::Files => {}
                }
            }
            // Never reaches here: `Browsing::poll` (src/tui/mod.rs) intercepts
            // this variant before it is offered to `apply`, and hands it to
            // the application instead, so it lands whatever the browser is
            // doing by the time the walk finishes (M8 §8).
            BrowseResult::TreeCollected(_) => return None,
        }
        self.cursor = self.cursor.min(self.len().saturating_sub(1));
        follow_up
    }

    /// Ends loading for the visible list, recording an error in its place.
    fn settle<T>(&mut self, list: Result<Vec<T>, String>) -> Vec<T> {
        self.loading = false;
        self.marked.clear();
        match list {
            Ok(list) => {
                self.error = None;
                list
            }
            Err(message) => {
                self.error = Some(message);
                Vec::new()
            }
        }
    }

    /// Up/Down/`j`/`k` move, Tab switches tabs, Enter opens or enqueues,
    /// Space marks (a directory too, on the Files tab), Backspace/Left goes
    /// back, `b`/Esc closes. On the Files tab `a` adds the marked rows, or
    /// the cursor row, recursively and additively (M8 §8). On the Podcasts
    /// tab `a` prompts for a feed URL, `r`/`R` refresh one/all and
    /// `d` asks before removing (M6 §4). The Radio tab's `a`/`r`/`d` mirror
    /// this exactly, sending `AddStation`/`ReprobeStation`/`RemoveStation`
    /// instead (M7.1 §7); `R` stays Podcasts-only, since refreshing every feed
    /// has no Radio equivalent. The prompt and the question take every key
    /// while they are up. A Ctrl or Alt chord does nothing, as in the rest
    /// of the keyboard map.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<BrowserEffect> {
        if key.kind != KeyEventKind::Press {
            return Vec::new();
        }
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        if let Some(slug) = self.confirm.take() {
            return if key.code == KeyCode::Char('y') && !blocks_ordinary_bindings(&key) {
                let request = match self.tab {
                    BrowserTab::Radio => BrowseRequest::RemoveStation { slug },
                    _ => BrowseRequest::Unsubscribe { slug },
                };
                self.submit(request)
            } else {
                Vec::new()
            };
        }
        if blocks_ordinary_bindings(&key) {
            return Vec::new();
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.cursor + 1 < self.len() {
                    self.cursor += 1;
                }
                Vec::new()
            }
            KeyCode::Tab | KeyCode::BackTab => self.switch_tab(),
            KeyCode::Enter => self.activate(),
            KeyCode::Char(' ') => {
                if self.markable(self.cursor) && !self.marked.remove(&self.cursor) {
                    self.marked.insert(self.cursor);
                }
                Vec::new()
            }
            // Scoped to the Files tab so it cannot shadow the Podcasts/Radio
            // `a` below, which prompts for a URL instead (M8 §8).
            KeyCode::Char('a') if self.tab == BrowserTab::Files => self.add_selection(),
            KeyCode::Backspace | KeyCode::Left => self.back(),
            KeyCode::Char('b') | KeyCode::Esc => vec![BrowserEffect::Close],
            KeyCode::Char('a') if self.can_manage() => {
                self.notice = None;
                self.prompt = Some(String::new());
                Vec::new()
            }
            KeyCode::Char('r') if self.can_manage() => match self.target_slug() {
                Some(slug) => {
                    let request = match self.tab {
                        BrowserTab::Radio => BrowseRequest::ReprobeStation { slug },
                        _ => BrowseRequest::Refresh { slug: Some(slug) },
                    };
                    self.submit(request)
                }
                None => Vec::new(),
            },
            KeyCode::Char('R')
                if self.can_manage()
                    && self.tab == BrowserTab::Podcasts
                    && !self.feeds.is_empty() =>
            {
                self.submit(BrowseRequest::Refresh { slug: None })
            }
            KeyCode::Char('d') if self.can_manage() => {
                if let Some(slug) = self.target_slug() {
                    self.notice = None;
                    self.confirm = Some(slug);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// How many rows the visible list has.
    pub fn len(&self) -> usize {
        match (self.tab, &self.episodes) {
            (BrowserTab::Files, _) => self.entries.len(),
            (BrowserTab::Podcasts, None) => self.feeds.len(),
            (BrowserTab::Podcasts, Some((_, episodes))) => episodes.len(),
            (BrowserTab::Radio, _) => self.stations.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether row `index` of the visible list can be marked and enqueued:
    /// an audio file, or an episode with an enclosure.
    pub fn enqueueable(&self, index: usize) -> bool {
        match (self.tab, &self.episodes) {
            (BrowserTab::Files, _) => self
                .entries
                .get(index)
                .is_some_and(|entry| entry.kind == EntryKind::Audio),
            (BrowserTab::Podcasts, None) => false,
            (BrowserTab::Podcasts, Some((_, episodes))) => episodes
                .get(index)
                .is_some_and(|episode| episode.enclosure.is_some()),
            (BrowserTab::Radio, _) => self.stations.get(index).is_some(),
        }
    }

    /// Whether Space may mark row `index`: anything enqueueable, and on the
    /// Files tab a directory too, which `a` adds recursively (M8 §8).
    pub fn markable(&self, index: usize) -> bool {
        self.enqueueable(index)
            || (self.tab == BrowserTab::Files
                && self
                    .entries
                    .get(index)
                    .is_some_and(|entry| entry.kind == EntryKind::Directory))
    }

    /// Whether a management key may act: the Podcasts or Radio tab, nothing
    /// loading, no mutation in flight.
    fn can_manage(&self) -> bool {
        matches!(self.tab, BrowserTab::Podcasts | BrowserTab::Radio)
            && !self.loading
            && self.pending.is_none()
    }

    /// The slug `r` and `d` act on: on Podcasts, the open feed, else the
    /// cursor's row; on Radio, always the cursor's station.
    fn target_slug(&self) -> Option<String> {
        match self.tab {
            BrowserTab::Radio => self.stations.get(self.cursor).map(|row| row.slug.clone()),
            _ => match &self.episodes {
                Some((slug, _)) => Some(slug.clone()),
                None => self.feeds.get(self.cursor).map(|feed| feed.slug.clone()),
            },
        }
    }

    /// Sends a mutation and remembers it until its answer arrives.
    fn submit(&mut self, request: BrowseRequest) -> Vec<BrowserEffect> {
        let text = match &request {
            BrowseRequest::Subscribe { .. } => "Subscribing…",
            BrowseRequest::Refresh { .. } => "Refreshing…",
            BrowseRequest::Unsubscribe { .. } => "Removing…",
            // Sent from the Radio tab's `a`, `r` and `d` (M7.1 §7).
            BrowseRequest::AddStation { .. } => "Adding…",
            BrowseRequest::RemoveStation { .. } => "Removing…",
            BrowseRequest::ReprobeStation { .. } => "Re-probing…",
            // Never actually reaches `submit`: `add_selection` (the Files-tab
            // `a` key, M8 §8) emits `CollectTree` straight through
            // `BrowserEffect::Request`, bypassing `submit`, because its
            // answer is `BrowseResult::TreeCollected`, which `Browsing::poll`
            // (src/tui/mod.rs) diverts to the application before it ever
            // reaches `BrowserState::apply` — so `submit`'s `pending` guard
            // would never be cleared for it. This arm exists only so the
            // match stays exhaustive over the wider `BrowseRequest` enum.
            BrowseRequest::CollectTree { .. } => "Adding…",
            BrowseRequest::Directory(_)
            | BrowseRequest::Feeds
            | BrowseRequest::Episodes { .. }
            | BrowseRequest::Stations => "Loading…",
        };
        self.notice = Some(Notice {
            text: text.to_owned(),
            kind: NoticeKind::Working,
        });
        self.pending = Some(request.clone());
        vec![BrowserEffect::Request(request)]
    }

    /// Printable characters append, Backspace pops, Enter submits the
    /// trimmed URL (nothing when empty) as `AddStation` on the Radio tab and
    /// `Subscribe` on Podcasts (M7.1 §7), Esc cancels. Shortcuts never fire.
    fn prompt_key(&mut self, key: KeyEvent) -> Vec<BrowserEffect> {
        match key.code {
            KeyCode::Esc => {
                self.prompt = None;
                Vec::new()
            }
            KeyCode::Backspace => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.pop();
                }
                Vec::new()
            }
            KeyCode::Enter => {
                let url = self.prompt.take().unwrap_or_default().trim().to_owned();
                if url.is_empty() {
                    Vec::new()
                } else {
                    let request = match self.tab {
                        BrowserTab::Radio => BrowseRequest::AddStation { url },
                        _ => BrowseRequest::Subscribe { url },
                    };
                    self.submit(request)
                }
            }
            KeyCode::Char(c) if !c.is_control() && !blocks_ordinary_bindings(&key) => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.push(c);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Returns from an episode view to the feed list, cursor on the feed
    /// just left, cached rows still showing.
    fn leave_episodes(&mut self) {
        if let Some((slug, _)) = self.episodes.take() {
            self.cursor = self
                .feeds
                .iter()
                .position(|feed| feed.slug == slug)
                .unwrap_or(0);
            self.marked.clear();
            self.loading = false;
            self.error = None;
        }
    }

    /// Empties the visible list and marks it loading.
    fn start_loading(&mut self) {
        self.notice = None;
        self.cursor = 0;
        self.marked.clear();
        self.loading = true;
        self.error = None;
    }

    /// Files → Podcasts → Radio → Files (M7.1 §7).
    fn switch_tab(&mut self) -> Vec<BrowserEffect> {
        self.start_loading();
        let request = match self.tab {
            BrowserTab::Files => {
                self.tab = BrowserTab::Podcasts;
                self.episodes = None;
                self.feeds.clear();
                BrowseRequest::Feeds
            }
            BrowserTab::Podcasts => {
                self.tab = BrowserTab::Radio;
                self.episodes = None;
                self.stations.clear();
                BrowseRequest::Stations
            }
            BrowserTab::Radio => {
                self.tab = BrowserTab::Files;
                self.entries.clear();
                BrowseRequest::Directory(self.cwd.clone())
            }
        };
        vec![BrowserEffect::Request(request)]
    }

    fn activate(&mut self) -> Vec<BrowserEffect> {
        match self.tab {
            BrowserTab::Files => match self.entries.get(self.cursor) {
                Some(entry) if entry.kind == EntryKind::Directory => {
                    let path = entry.path.clone();
                    self.open_directory(path)
                }
                Some(entry) if entry.kind == EntryKind::Audio => {
                    if self.marked.is_empty() {
                        self.enqueue_selection()
                    } else {
                        self.add_selection()
                    }
                }
                _ => Vec::new(),
            },
            BrowserTab::Podcasts => match &self.episodes {
                None => match self.feeds.get(self.cursor) {
                    Some(feed) => {
                        let slug = feed.slug.clone();
                        self.start_loading();
                        self.episodes = Some((slug.clone(), Vec::new()));
                        vec![BrowserEffect::Request(BrowseRequest::Episodes { slug })]
                    }
                    None => Vec::new(),
                },
                Some(_) if self.enqueueable(self.cursor) => self.enqueue_selection(),
                Some(_) => Vec::new(),
            },
            BrowserTab::Radio => self.enqueue_selection(),
        }
    }

    fn open_directory(&mut self, path: PathBuf) -> Vec<BrowserEffect> {
        self.start_loading();
        self.entries.clear();
        self.cwd = path.clone();
        vec![BrowserEffect::Request(BrowseRequest::Directory(path))]
    }

    /// The marked rows in listing order, or the cursor's row when nothing is
    /// marked; the marks clear once they are enqueued. Rows already in the
    /// queue are skipped, and Enter on one alone takes it out instead.
    ///
    /// A Radio row enqueues as `EnqueueItem::Station`, titled by its icy-name
    /// or else its slug, with `station.url.to_string()` as the URL, which `resolve_source` (M7.1 §7) resolves through the exact same
    /// `NormalizedUrl::parse` call `station_identity_of` used to derive
    /// `StationRow::media` at add time, from the same canonical `url::Url`
    /// text — so the identity this enqueue produces and the tick
    /// `queued_at` draws for the row can never disagree.
    fn enqueue_selection(&mut self) -> Vec<BrowserEffect> {
        if self.marked.is_empty()
            && let Some(id) = self.queued_at(self.cursor)
        {
            return vec![BrowserEffect::Remove(id)];
        }
        let indices: Vec<usize> = if self.marked.is_empty() {
            vec![self.cursor]
        } else {
            std::mem::take(&mut self.marked).into_iter().collect()
        };
        let items: Vec<EnqueueItem> = indices
            .into_iter()
            .filter(|index| self.enqueueable(*index) && self.queued_at(*index).is_none())
            .filter_map(|index| match (self.tab, &self.episodes) {
                (BrowserTab::Files, _) => self
                    .entries
                    .get(index)
                    .map(|entry| EnqueueItem::Path(entry.path.clone())),
                (BrowserTab::Podcasts, Some((_, episodes))) => episodes
                    .get(index)
                    .map(|episode| EnqueueItem::Episode(episode.clone())),
                (BrowserTab::Podcasts, None) => None,
                (BrowserTab::Radio, _) => {
                    self.stations
                        .get(index)
                        .map(|station| EnqueueItem::Station {
                            url: station.url.to_string(),
                            title: station
                                .identity
                                .as_ref()
                                .and_then(|identity| identity.name.clone())
                                .unwrap_or_else(|| station.slug.clone()),
                        })
                }
            })
            .collect();
        if items.is_empty() {
            Vec::new()
        } else {
            vec![BrowserEffect::Enqueue {
                dest: self.dest,
                items,
            }]
        }
    }

    /// `a` on the Files tab: the marked rows, or the cursor row when nothing
    /// is marked; files and directories alike; strictly additive. Any
    /// directory makes it a worker walk, which also dedupes overlapping
    /// picks; files alone enqueue directly, as Enter does.
    fn add_selection(&mut self) -> Vec<BrowserEffect> {
        let indices: Vec<usize> = if self.marked.is_empty() {
            vec![self.cursor]
        } else {
            std::mem::take(&mut self.marked).into_iter().collect()
        };
        let picked: Vec<&DirEntry> = indices
            .into_iter()
            .filter(|index| self.markable(*index) && self.queued_at(*index).is_none())
            .filter_map(|index| self.entries.get(index))
            .collect();
        if picked.is_empty() {
            return Vec::new();
        }
        if picked
            .iter()
            .any(|entry| entry.kind == EntryKind::Directory)
        {
            let roots = picked.iter().map(|entry| entry.path.clone()).collect();
            return vec![BrowserEffect::Request(BrowseRequest::CollectTree {
                roots,
                dest: self.dest,
            })];
        }
        let items = picked
            .iter()
            .map(|entry| EnqueueItem::Path(entry.path.clone()))
            .collect();
        vec![BrowserEffect::Enqueue {
            dest: self.dest,
            items,
        }]
    }

    fn back(&mut self) -> Vec<BrowserEffect> {
        match self.tab {
            BrowserTab::Files => match self.cwd.parent() {
                Some(parent) => {
                    let parent = parent.to_path_buf();
                    self.open_directory(parent)
                }
                None => Vec::new(),
            },
            BrowserTab::Podcasts => {
                if self.episodes.is_none() {
                    return Vec::new();
                }
                self.leave_episodes();
                self.notice = None;
                // Re-read rather than trust rows cached before a mutation;
                // the cached rows stay up until the answer lands (M6 §4).
                vec![BrowserEffect::Request(BrowseRequest::Feeds)]
            }
            // The Radio tab has no sub-view to leave (M7.1 §7).
            BrowserTab::Radio => Vec::new(),
        }
    }
}
