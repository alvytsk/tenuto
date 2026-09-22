//! What the on-demand browser reads (design doc M5 §8): one level of a local
//! directory, the subscribed feeds and a feed's cached episodes, each read on
//! a worker thread so the render loop never waits on the filesystem.
//!
//! Only an explicit `Subscribe`, `Refresh`, `AddStation` or `ReprobeStation`
//! request touches the network.
//! The worker owns its own [`LibraryStores`] and builds an `HttpService`
//! lazily, on the first request that needs one; the feed listings are the
//! same read-only snapshot reads `tenuto feeds` uses, so opening the
//! browser or listing episodes never refreshes a feed. A listing is a
//! directory read and a `stat` per entry — no recursion and no media
//! metadata probing. Mutations run one at a time, in order with the
//! listings.
//!
//! ponytail: one thread for listings and mutations, so a directory read
//! issued during a refresh-all waits behind it; a second worker for
//! mutations is the upgrade if that wait ever matters.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, Sender};

use crate::application::runtime::LibraryStores;
use crate::application::source::resolve_path;
use crate::commands::{
    finish_add_station, finish_refresh_batch, finish_refresh_one, finish_remove_station,
    finish_reprobe_station, finish_subscribe, finish_unsubscribe, report, wait_http,
};
use crate::http::error::redact_url;
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::library::{
    EpisodeCandidate, FeedSummary, StationRow, add_station, episode_candidates, list_feeds,
    list_stations, refresh, refresh_all, remove_station, reprobe_station, subscribe, unsubscribe,
};
use crate::media::id::MediaId;
use crate::playlist::PlaylistId;
use crate::queue::MAX_PLAYLIST_ENTRIES;

/// The extensions a listing classifies as audio, compared ASCII
/// case-insensitively.
const AUDIO_EXTENSIONS: [&str; 4] = ["mp3", "flac", "wav", "m4a"];
const NO_LIBRARY: &str = "No subscription library is available";
const NOT_RUNNING: &str = "The browser's reader is not running";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    Directory,
    Audio,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// The file name as the filesystem spells it, lossily decoded; a front
    /// end still has to make it displayable before drawing it.
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    /// The identity the queue gives an audio file (its canonical path), so
    /// the browser can tell which rows are already queued; `None` for
    /// anything else or when the file cannot be resolved.
    pub media: Option<MediaId>,
}

/// One level of `path`: directories first, then the rest, each group by
/// case-insensitive name. A symlink is classified by what it points at; a
/// dangling one is `Other`. An unreadable directory is an `Err`, never a
/// panic.
pub fn list_directory(path: &Path) -> std::io::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        let is_dir = std::fs::metadata(&entry_path).is_ok_and(|metadata| metadata.is_dir());
        let kind = if is_dir {
            EntryKind::Directory
        } else if is_audio(&entry_path) {
            EntryKind::Audio
        } else {
            EntryKind::Other
        };
        let media = (kind == EntryKind::Audio)
            .then(|| resolve_path(&entry_path).ok().map(|(media, _)| media))
            .flatten();
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry_path,
            kind,
            media,
        });
    }
    entries.sort_by_cached_key(|entry| {
        (
            entry.kind != EntryKind::Directory,
            entry.name.to_lowercase(),
            entry.name.clone(),
        )
    });
    Ok(entries)
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|audio| extension.eq_ignore_ascii_case(audio))
        })
}

/// A folder add's outcome (M8 §8): every audio file `collect_tree` found
/// under the selected roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeCollected {
    pub dest: PlaylistId,
    /// Audio files in walk order, deduplicated by resolved `MediaId`.
    pub items: Vec<PathBuf>,
    /// Directories that could not be read. Logged, counted in the notice.
    pub unreadable: Vec<PathBuf>,
    /// The walk stopped at the limit: unvisited files may exist, and no
    /// count of them is known.
    pub scan_limit_reached: bool,
}

/// Every audio file under `roots`, for a folder add (M8 §8). Depth-first,
/// each level in `list_directory`'s own order, so the walk and the listing
/// the listener is looking at agree; that order also decides what a
/// truncated walk keeps. Filename order, not album track order.
pub fn collect_tree(roots: &[PathBuf], dest: PlaylistId, limit: usize) -> TreeCollected {
    let mut walk = Walk {
        limit,
        full: limit == 0,
        seen: HashSet::new(),
        tree: TreeCollected {
            dest,
            items: Vec::new(),
            unreadable: Vec::new(),
            scan_limit_reached: false,
        },
    };
    for root in roots {
        if walk.full {
            // Whatever is left was never looked at.
            walk.tree.scan_limit_reached = true;
            break;
        }
        // `metadata` follows symlinks, so a root that links to a directory
        // lands in `directory`, which is where directory symlinks are refused.
        if std::fs::metadata(root).is_ok_and(|metadata| metadata.is_dir()) {
            walk.directory(root);
        } else if is_audio(root) {
            walk.file(root);
        }
    }
    walk.tree
}

struct Walk {
    limit: usize,
    /// `limit` candidates are held: stop now, do not keep reading the tree.
    full: bool,
    seen: HashSet<MediaId>,
    tree: TreeCollected,
}

impl Walk {
    fn file(&mut self, path: &Path) {
        let Ok((media, _)) = resolve_path(path) else {
            return;
        };
        if !self.seen.insert(media) {
            return;
        }
        self.tree.items.push(path.to_path_buf());
        self.full = self.tree.items.len() >= self.limit;
    }

    /// The one entry point for traversing a directory, so the one place a
    /// directory symlink is refused — as a descendant or as a selected root.
    /// That refusal is what rules out a cycle (M8 §8).
    ///
    /// ponytail: recursion depth equals directory depth; a tree thousands of
    /// levels deep would overflow the worker's stack. An explicit stack is
    /// the upgrade.
    fn directory(&mut self, path: &Path) {
        let is_link =
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
        if is_link {
            return;
        }
        let entries = match list_directory(path) {
            Ok(entries) => entries,
            Err(_) => {
                self.tree.unreadable.push(path.to_path_buf());
                return;
            }
        };
        let mut entries = entries.into_iter();
        while let Some(entry) = entries.next() {
            match entry.kind {
                EntryKind::Directory => self.directory(&entry.path),
                EntryKind::Audio => self.file(&entry.path),
                EntryKind::Other => {}
            }
            if self.full {
                // Stop here. Anything audio-or-directory still unvisited at
                // this level means files may exist that were never seen; the
                // callers above make the same check on their own levels.
                if entries.any(|rest| rest.kind != EntryKind::Other) {
                    self.tree.scan_limit_reached = true;
                }
                return;
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowseRequest {
    Directory(PathBuf),
    Feeds,
    Episodes {
        slug: String,
    },
    /// `tenuto subscribe <url>`, slug derived.
    Subscribe {
        url: String,
    },
    /// `tenuto refresh [slug]`: one feed, or every feed for `None`.
    Refresh {
        slug: Option<String>,
    },
    /// `tenuto unsubscribe <slug>`.
    Unsubscribe {
        slug: String,
    },
    /// The Radio tab's listing (M7.1 design doc §6). Read-only, like `Feeds`:
    /// never touches the network.
    Stations,
    /// Validates, probes and saves a station by URL (M7.1 §6, §10).
    AddStation {
        url: String,
    },
    /// Drops a saved station. Local-only, like `Unsubscribe` (M7.1 §6).
    RemoveStation {
        slug: String,
    },
    /// Re-probes a saved station, refreshing its cached identity (M7.1 §6).
    ReprobeStation {
        slug: String,
    },
    /// A Files-tab folder add (M8 §8): the recursive walk behind `a`.
    CollectTree {
        roots: Vec<PathBuf>,
        dest: PlaylistId,
    },
}

/// A request's answer, naming what it was for so a caller can tell a late
/// answer for a place it already left from the one it is waiting on.
#[derive(Clone, Debug)]
pub enum BrowseResult {
    Directory {
        path: PathBuf,
        entries: Result<Vec<DirEntry>, String>,
    },
    Feeds(Result<Vec<FeedSummary>, String>),
    /// The Radio tab's listing (M7.1 design doc §6).
    Stations(Result<Vec<StationRow>, String>),
    Episodes {
        slug: String,
        episodes: Result<Vec<EpisodeCandidate>, String>,
    },
    /// A mutation's answer, echoing the request so the browser can tell
    /// whose answer it is. The text is the CLI's own wording (§3).
    Mutation {
        request: BrowseRequest,
        outcome: Result<String, String>,
    },
    /// A folder add's outcome (M8 §8).
    TreeCollected(TreeCollected),
}

/// One background thread answering [`BrowseRequest`]s in order. It exits
/// once this handle is dropped and it has finished the request in hand.
pub struct BrowseWorker {
    requests: Sender<BrowseRequest>,
    results: Receiver<BrowseResult>,
    /// For answering a request the thread will never see.
    failures: Sender<BrowseResult>,
}

impl BrowseWorker {
    pub fn spawn(library: Option<LibraryStores>) -> Self {
        let (requests, request_rx) = crossbeam_channel::unbounded();
        let (result_tx, results) = crossbeam_channel::unbounded();
        // Detached: a directory read stuck on a slow mount must not hold up
        // whoever drops the handle.
        let _detached = thread::Builder::new().name("tenuto-browse".into()).spawn({
            let result_tx = result_tx.clone();
            move || serve(library.as_ref(), &request_rx, &result_tx)
        });
        Self {
            requests,
            results,
            failures: result_tx,
        }
    }

    /// Queues `request`; never blocks. When the thread could not start, the
    /// request is answered at once with an error instead.
    pub fn request(&self, request: BrowseRequest) {
        if let Err(refused) = self.requests.send(request) {
            let _ = self
                .failures
                .send(answer_with(refused.into_inner(), NOT_RUNNING));
        }
    }

    pub fn try_result(&self) -> Option<BrowseResult> {
        self.results.try_recv().ok()
    }
}

fn serve(
    library: Option<&LibraryStores>,
    requests: &Receiver<BrowseRequest>,
    results: &Sender<BrowseResult>,
) {
    let mut http = None;
    for request in requests {
        if results.send(answer(library, &mut http, request)).is_err() {
            return;
        }
    }
}

/// `request`'s answer when all it can say is `message`.
fn answer_with(request: BrowseRequest, message: &str) -> BrowseResult {
    match request {
        BrowseRequest::Directory(path) => BrowseResult::Directory {
            path,
            entries: Err(message.to_owned()),
        },
        BrowseRequest::Feeds => BrowseResult::Feeds(Err(message.to_owned())),
        BrowseRequest::Episodes { slug } => BrowseResult::Episodes {
            slug,
            episodes: Err(message.to_owned()),
        },
        BrowseRequest::Stations => BrowseResult::Stations(Err(message.to_owned())),
        request @ (BrowseRequest::Subscribe { .. }
        | BrowseRequest::Refresh { .. }
        | BrowseRequest::Unsubscribe { .. }
        | BrowseRequest::AddStation { .. }
        | BrowseRequest::RemoveStation { .. }
        | BrowseRequest::ReprobeStation { .. }) => BrowseResult::Mutation {
            request,
            outcome: Err(message.to_owned()),
        },
        BrowseRequest::CollectTree { roots, dest } => BrowseResult::TreeCollected(TreeCollected {
            dest,
            items: Vec::new(),
            unreadable: roots,
            scan_limit_reached: false,
        }),
    }
}

fn answer(
    library: Option<&LibraryStores>,
    http: &mut Option<Arc<HttpService>>,
    request: BrowseRequest,
) -> BrowseResult {
    match request {
        BrowseRequest::Directory(path) => {
            let entries = list_directory(&path).map_err(|error| error.to_string());
            BrowseResult::Directory { path, entries }
        }
        BrowseRequest::Feeds => match library {
            Some(stores) => BrowseResult::Feeds(
                list_feeds(&stores.subscriptions, &stores.cache).map_err(|error| error.to_string()),
            ),
            None => answer_with(request, NO_LIBRARY),
        },
        BrowseRequest::Episodes { slug } => match library {
            Some(stores) => {
                let episodes = episode_candidates(&stores.subscriptions, &stores.cache, &slug)
                    .map_err(|error| error.to_string());
                BrowseResult::Episodes { slug, episodes }
            }
            None => answer_with(BrowseRequest::Episodes { slug }, NO_LIBRARY),
        },
        BrowseRequest::Stations => match library {
            Some(stores) => BrowseResult::Stations(
                list_stations(&stores.stations).map_err(|error| error.to_string()),
            ),
            None => answer_with(request, NO_LIBRARY),
        },
        request @ (BrowseRequest::Subscribe { .. }
        | BrowseRequest::Refresh { .. }
        | BrowseRequest::Unsubscribe { .. }
        | BrowseRequest::AddStation { .. }
        | BrowseRequest::RemoveStation { .. }
        | BrowseRequest::ReprobeStation { .. }) => {
            let outcome = match library {
                Some(stores) => mutate(stores, http, &request),
                None => Err(NO_LIBRARY.to_owned()),
            };
            match &outcome {
                Ok(text) => tracing::info!(request = %describe(&request), "{text}"),
                Err(text) => tracing::info!(request = %describe(&request), "failed: {text}"),
            }
            BrowseResult::Mutation { request, outcome }
        }
        BrowseRequest::CollectTree { roots, dest } => {
            BrowseResult::TreeCollected(collect_tree(&roots, dest, MAX_PLAYLIST_ENTRIES))
        }
    }
}

/// Runs one mutation the way its CLI command does, and reports it the way
/// the CLI prints it (§3). Only `Subscribe`, `Refresh`, `AddStation` and
/// `ReprobeStation` need the service; `RemoveStation` is a local edit like
/// `Unsubscribe` and must never call [`http_service`] (M7.1 §6, R3).
fn mutate(
    stores: &LibraryStores,
    http: &mut Option<Arc<HttpService>>,
    request: &BrowseRequest,
) -> Result<String, String> {
    let subs = &stores.subscriptions;
    let cache = &stores.cache;
    let stations = &stores.stations;
    match request {
        BrowseRequest::Subscribe { url } => {
            let service = http_service(http)?;
            let outcome = wait_http(&service, subscribe(&service, subs, cache, url, None))
                .map_err(|error| error.to_string())?;
            report(finish_subscribe, outcome)
        }
        BrowseRequest::Refresh { slug: Some(slug) } => {
            let service = http_service(http)?;
            let outcome = wait_http(&service, refresh(&service, subs, cache, slug))
                .map_err(|error| error.to_string())?;
            report(finish_refresh_one, outcome)
        }
        BrowseRequest::Refresh { slug: None } => {
            let service = http_service(http)?;
            let outcomes = wait_http(&service, refresh_all(&service, subs, cache))
                .map_err(|error| error.to_string())?;
            report(finish_refresh_batch, outcomes)
        }
        BrowseRequest::Unsubscribe { slug } => {
            let outcome = unsubscribe(subs, cache, slug).map_err(|error| error.to_string())?;
            report(finish_unsubscribe, outcome)
        }
        BrowseRequest::AddStation { url } => {
            let service = http_service(http)?;
            let outcome = wait_http(&service, add_station(&service, stations, url))
                .map_err(|error| error.to_string())?;
            report(finish_add_station, outcome)
        }
        BrowseRequest::RemoveStation { slug } => {
            let outcome = remove_station(stations, slug).map_err(|error| error.to_string())?;
            report(finish_remove_station, outcome)
        }
        BrowseRequest::ReprobeStation { slug } => {
            let service = http_service(http)?;
            let outcome = wait_http(&service, reprobe_station(&service, stations, slug))
                .map_err(|error| error.to_string())?;
            report(finish_reprobe_station, outcome)
        }
        BrowseRequest::Directory(_)
        | BrowseRequest::Feeds
        | BrowseRequest::Episodes { .. }
        | BrowseRequest::Stations
        | BrowseRequest::CollectTree { .. } => Err("not a mutation".to_owned()),
    }
}

/// The worker's HTTP service, built on the first request that needs one
/// and kept for the thread's lifetime. A failure to start is this request's
/// error; the next request tries again.
fn http_service(slot: &mut Option<Arc<HttpService>>) -> Result<Arc<HttpService>, String> {
    if let Some(service) = slot {
        return Ok(Arc::clone(service));
    }
    let service = HttpService::spawn(Limits::default()).map_err(|error| error.to_string())?;
    *slot = Some(Arc::clone(&service));
    Ok(service)
}

/// Returns a redacted description of the request for logging, redacting any
/// URLs to prevent userinfo or signed queries from reaching the log.
fn describe(request: &BrowseRequest) -> String {
    match request {
        BrowseRequest::Directory(_) => "Directory".to_string(),
        BrowseRequest::Feeds => "Feeds".to_string(),
        BrowseRequest::Episodes { slug } => format!("Episodes({})", slug),
        BrowseRequest::Subscribe { url } => format!("Subscribe({})", redact_url(url)),
        BrowseRequest::Refresh { slug: Some(slug) } => format!("Refresh({})", slug),
        BrowseRequest::Refresh { slug: None } => "Refresh(all)".to_string(),
        BrowseRequest::Unsubscribe { slug } => format!("Unsubscribe({})", slug),
        BrowseRequest::Stations => "Stations".to_string(),
        // A station URL can carry userinfo exactly as a feed URL can, so it
        // is redacted here for the same reason `Subscribe` is (§7.2).
        BrowseRequest::AddStation { url } => format!("AddStation({})", redact_url(url)),
        BrowseRequest::RemoveStation { slug } => format!("RemoveStation({})", slug),
        BrowseRequest::ReprobeStation { slug } => format!("ReprobeStation({})", slug),
        BrowseRequest::CollectTree { roots, .. } => format!("CollectTree({} roots)", roots.len()),
    }
}
