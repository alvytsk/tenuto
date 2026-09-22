//! Queue fields of the state file, decoded apart from listening history so a
//! damaged queue can never cost a checkpoint (M5 §6).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::media::id::{AbsolutePath, MediaId, NormalizedUrl};
use crate::playback::provenance::PositionProvenance;
use crate::playlist::{MAX_PLAYLISTS, Playlist, PlaylistId, Shuffle, clean_name};
use crate::queue::{
    DisplayDuration, DisplayMetadata, DurationSource, IdAllocator, MAX_PLAYLIST_ENTRIES, Queue,
    QueueEntry, QueueEntryId, QueueSource, source_matches,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveProblem {
    Malformed,
    Dangling,
    MediaMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueProblem {
    Malformed,
    DuplicateIds,
    SourceMismatch,
    OverCapacity { found: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistProblem {
    Malformed,
    DuplicatePlaylistId,
    DuplicateEntryId,
    /// A repair needed a fresh ID and the namespace had none left, so the
    /// later conflicting entry or playlist was dropped instead (M8 §6).
    IdsExhausted,
    TooManyPlaylists {
        found: usize,
    },
    OverCapacity {
        found: usize,
    },
    DanglingPlaying,
    /// A `shuffle` value that is not an object with a numeric `seed`, or
    /// whose `first` is not a number (M8 §6 rule 10). A `first` that is
    /// merely no longer a member is *not* this: §7 calls that legal, and
    /// recovery drops it without a word.
    Shuffle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueReset {
    ActiveReference(ActiveProblem),
    WholeQueue(QueueProblem),
    Playlists(PlaylistProblem),
}

impl QueueReset {
    /// For the sanitized startup warning (§6): which fields were reset.
    pub fn fields_reset(self) -> &'static str {
        match self {
            Self::ActiveReference(_) => "the active queue entry",
            Self::WholeQueue(_) => "the queue and its active entry",
            Self::Playlists(_) => "one or more playlists",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SourceDto {
    Local { path: String },
    Remote { url: String },
    Podcast { fallback_url: String },
}

#[derive(Default, Serialize, Deserialize)]
struct DisplayDto {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    year: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    duration_source: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct QueueEntryDto {
    id: u64,
    media: MediaId,
    source: SourceDto,
    #[serde(default)]
    display: DisplayDto,
}

/// Serializes each entry back to its on-disk shape. The inverse of
/// [`entry_from_dto`], but never fails: an in-memory [`Queue`] is already
/// valid, so encoding is total.
pub(crate) fn encode(queue: &Queue) -> Vec<QueueEntryDto> {
    queue
        .entries()
        .iter()
        .map(|entry| {
            let source = match entry.source() {
                QueueSource::LocalFile(path) => SourceDto::Local {
                    path: path.as_str().to_string(),
                },
                QueueSource::RemoteUrl(url) => SourceDto::Remote {
                    url: url.as_str().to_string(),
                },
                QueueSource::Podcast { fallback } => SourceDto::Podcast {
                    fallback_url: fallback.to_string(),
                },
            };
            let display = entry.display();
            let (duration_ms, duration_source) = match &display.duration {
                Some(duration) => {
                    let ms = u64::try_from(duration.value.as_millis()).unwrap_or(u64::MAX);
                    let label = match duration.source {
                        DurationSource::Decoded(PositionProvenance::Established) => "decoded",
                        DurationSource::Decoded(PositionProvenance::Estimated) => {
                            "decoded_estimated"
                        }
                        DurationSource::Declared => "declared",
                    };
                    (Some(ms), Some(label.to_string()))
                }
                None => (None, None),
            };
            QueueEntryDto {
                id: entry.id().get(),
                media: entry.media().clone(),
                source,
                display: DisplayDto {
                    title: display.title.clone(),
                    artist: display.artist.clone(),
                    album: display.album.clone(),
                    year: display.year.clone(),
                    duration_ms,
                    duration_source,
                },
            }
        })
        .collect()
}

#[derive(Serialize)]
pub(crate) struct ShuffleDto {
    seed: u64,
    first: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct PlaylistDto {
    id: u64,
    name: String,
    shuffle: Option<ShuffleDto>,
    entries: Vec<QueueEntryDto>,
    active_entry: Option<u64>,
}

/// Schema 4's `playlists` array. Total, like [`encode`]: an in-memory
/// [`Playlist`] is already valid.
pub(crate) fn encode_playlists(playlists: &[Playlist]) -> Vec<PlaylistDto> {
    playlists
        .iter()
        .map(|playlist| PlaylistDto {
            id: playlist.id().get(),
            name: playlist.name().to_owned(),
            shuffle: playlist.shuffle().map(|shuffle| ShuffleDto {
                seed: shuffle.seed,
                first: shuffle.first.map(QueueEntryId::get),
            }),
            entries: encode(playlist.queue()),
            active_entry: playlist.queue().active().map(QueueEntryId::get),
        })
        .collect()
}

/// Schema 4's playlist fields, held as raw JSON for the same reason the
/// schema-3 queue fields were: a damaged playlist must never cost a
/// checkpoint (M8 §6).
pub(super) struct RawPlaylists<'a> {
    pub playlists: Option<&'a Value>,
    pub playing: Option<&'a Value>,
    /// `None` = the key is absent; `Some(Null)` = an exhausted namespace.
    pub next_entry_id: Option<&'a Value>,
    pub next_playlist_id: Option<&'a Value>,
}

/// Everything a load derives from a file's playlist data — recovered or
/// migrated — together with the one problem that is reported.
pub(super) struct Recovered {
    pub playlists: Vec<Playlist>,
    pub playing: PlaylistId,
    pub entry_ids: IdAllocator,
    pub playlist_ids: IdAllocator,
    pub current_media: Option<MediaId>,
    pub reset: Option<QueueReset>,
}

/// The first recorded problem wins the one `reset` slot.
fn note(reset: &mut Option<QueueReset>, problem: QueueReset) {
    if reset.is_none() {
        *reset = Some(problem);
    }
}

/// The stored counter raised past every well-formed ID in the file. Computed
/// before any repair, so a repair never mints an ID that appears later.
fn counter(stored: Option<&Value>, seen: impl Iterator<Item = u64>) -> IdAllocator {
    let start = match stored {
        Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().unwrap_or(1).max(1)),
        None => Some(1),
    };
    let mut ids = IdAllocator::starting_at(start);
    seen.for_each(|id| ids.observe(id));
    ids
}

fn fresh(ids: &mut IdAllocator) -> Option<u64> {
    ids.reserve(1).ok().map(|range| *range.start())
}

/// One empty `Default`, for a file with no usable playlists. Both counters
/// are the caller's — already raised past everything stored and seen — so a
/// fallback can neither reuse an ID nor revive an exhausted namespace. The
/// playlist takes a fresh ID. Only with playlist IDs exhausted does it fall
/// back to 1: P1 needs a playlist, and no in-flight result from before a
/// restart can name it.
fn default_playlists(
    current_media: Option<MediaId>,
    reset: Option<QueueReset>,
    entry_ids: IdAllocator,
    mut playlist_ids: IdAllocator,
) -> Recovered {
    let id = PlaylistId::from_raw(fresh(&mut playlist_ids).unwrap_or(1));
    Recovered {
        playlists: vec![Playlist::from_parts(
            id,
            "Default".into(),
            None,
            Queue::default(),
        )],
        playing: id,
        entry_ids,
        playlist_ids,
        current_media,
        reset,
    }
}

/// Schema 3 → 4: the one queue becomes `Default`, IDs and cursor carried over.
pub(super) fn migrate_queue(
    queue: Queue,
    current_media: Option<MediaId>,
    reset: Option<QueueReset>,
) -> Recovered {
    let mut entry_ids = IdAllocator::default();
    queue
        .entries()
        .iter()
        .for_each(|entry| entry_ids.observe(entry.id().get()));
    let id = PlaylistId::from_raw(1);
    Recovered {
        playlists: vec![Playlist::from_parts(id, "Default".into(), None, queue)],
        playing: id,
        entry_ids,
        playlist_ids: IdAllocator::starting_at(Some(2)),
        current_media,
        reset,
    }
}

/// Schema 4's ten ordered recovery rules (M8 §6). Never fails: each step
/// resets only what it affects and reports why, so damaged playlist data can
/// never cost a checkpoint (P8).
pub(super) fn recover_playlists(
    raw: RawPlaylists<'_>,
    current_media: Option<MediaId>,
) -> Recovered {
    let mut reset = None;
    // Rule 1. The stored counters are read before anything else, so every
    // fallback below carries them.
    let items = match raw.playlists {
        Some(Value::Array(items)) => items,
        other => {
            let problem = (!matches!(other, None | Some(Value::Null)))
                .then_some(QueueReset::Playlists(PlaylistProblem::Malformed));
            return default_playlists(
                current_media,
                problem,
                counter(raw.next_entry_id, std::iter::empty()),
                counter(raw.next_playlist_id, std::iter::empty()),
            );
        }
    };

    // Counters first, over the whole file.
    let playlist_seen = items.iter().filter_map(|item| item.get("id")?.as_u64());
    let entry_seen = items
        .iter()
        .filter_map(|item| item.get("entries")?.as_array())
        .flatten()
        .filter_map(|entry| entry.get("id")?.as_u64());
    let mut playlist_ids = counter(raw.next_playlist_id, playlist_seen);
    let mut entry_ids = counter(raw.next_entry_id, entry_seen);

    let found_entries: usize = items
        .iter()
        .filter_map(|item| item.get("entries")?.as_array())
        .map(Vec::len)
        .sum();
    if items.len() > MAX_PLAYLISTS {
        note(
            &mut reset,
            QueueReset::Playlists(PlaylistProblem::TooManyPlaylists { found: items.len() }),
        );
    }

    let mut seen_playlists = BTreeSet::new();
    let mut seen_entries = BTreeSet::new();
    let mut total = 0usize;
    let mut playlists = Vec::new();

    for item in items.iter().take(MAX_PLAYLISTS) {
        // Rule 3: a malformed or repeated playlist ID.
        let id = match item
            .get("id")
            .and_then(Value::as_u64)
            .filter(|id| seen_playlists.insert(*id))
        {
            Some(id) => id,
            None => match fresh(&mut playlist_ids) {
                Some(id) => {
                    note(
                        &mut reset,
                        QueueReset::Playlists(PlaylistProblem::DuplicatePlaylistId),
                    );
                    seen_playlists.insert(id);
                    id
                }
                None => {
                    note(
                        &mut reset,
                        QueueReset::Playlists(PlaylistProblem::IdsExhausted),
                    );
                    continue;
                }
            },
        };

        // Rule 2: this playlist's entries, under the existing per-queue rules.
        let decoded = match item.get("entries") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(entries)) => decode_entries(entries).unwrap_or_else(|problem| {
                note(&mut reset, QueueReset::WholeQueue(problem));
                Vec::new()
            }),
            Some(_) => {
                note(&mut reset, QueueReset::WholeQueue(QueueProblem::Malformed));
                Vec::new()
            }
        };

        // Rules 4 and 6: cross-playlist duplicates, then the global cap.
        let mut cursor = item.get("active_entry").and_then(Value::as_u64);
        let mut entries = Vec::with_capacity(decoded.len());
        for entry in decoded {
            if total >= MAX_PLAYLIST_ENTRIES {
                // Noted here, not after the loop: the truncated cursor's own
                // `Dangling` note below must not win the first-problem slot.
                note(
                    &mut reset,
                    QueueReset::Playlists(PlaylistProblem::OverCapacity {
                        found: found_entries,
                    }),
                );
                if cursor == Some(entry.id().get()) {
                    cursor = None;
                }
                continue;
            }
            let raw_id = entry.id().get();
            let entry = if seen_entries.insert(raw_id) {
                entry
            } else {
                if cursor == Some(raw_id) {
                    cursor = None;
                }
                match fresh(&mut entry_ids) {
                    Some(new_id) => {
                        note(
                            &mut reset,
                            QueueReset::Playlists(PlaylistProblem::DuplicateEntryId),
                        );
                        seen_entries.insert(new_id);
                        Queue::entry_from_parts(
                            QueueEntryId::from_raw(new_id),
                            entry.media().clone(),
                            entry.source().clone(),
                            entry.display().clone(),
                        )
                    }
                    None => {
                        note(
                            &mut reset,
                            QueueReset::Playlists(PlaylistProblem::IdsExhausted),
                        );
                        continue;
                    }
                }
            };
            total += 1;
            entries.push(entry);
        }

        // Rule 9, the membership half: every playlist's cursor must be a member.
        let active = match item.get("active_entry") {
            None | Some(Value::Null) => None,
            Some(_) => match cursor.filter(|id| entries.iter().any(|e| e.id().get() == *id)) {
                Some(id) => Some(QueueEntryId::from_raw(id)),
                None => {
                    note(
                        &mut reset,
                        QueueReset::ActiveReference(ActiveProblem::Dangling),
                    );
                    None
                }
            },
        };

        // Rule 10.
        let shuffle = match item.get("shuffle") {
            None | Some(Value::Null) => None,
            Some(value) => match value.get("seed").and_then(Value::as_u64) {
                Some(seed) => {
                    let named = value.get("first").filter(|first| !first.is_null());
                    let numbered = named.and_then(Value::as_u64);
                    let first = numbered
                        .filter(|id| entries.iter().any(|e| e.id().get() == *id))
                        .map(QueueEntryId::from_raw);
                    // A `first` that is no longer a member is dropped
                    // *silently*: §7 makes that a legal in-memory state —
                    // shuffle on mid-track pins the cursor, then that entry
                    // is removed — so reporting it would tell the listener
                    // their queue was reset when nothing was lost. Only a
                    // `first` that is not a number is damage.
                    if named.is_some() && numbered.is_none() {
                        note(&mut reset, QueueReset::Playlists(PlaylistProblem::Shuffle));
                    }
                    Some(Shuffle { seed, first })
                }
                None => {
                    note(&mut reset, QueueReset::Playlists(PlaylistProblem::Shuffle));
                    None
                }
            },
        };

        // Rule 5.
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .and_then(clean_name)
            .unwrap_or_else(|| format!("Playlist {id}"));

        playlists.push(Playlist::from_parts(
            PlaylistId::from_raw(id),
            name,
            shuffle,
            Queue::from_parts(entries, active),
        ));
    }
    // Rule 7.
    let Some(first) = playlists.first().map(Playlist::id) else {
        return default_playlists(current_media, reset, entry_ids, playlist_ids);
    };

    // Rule 8, then the media half of rule 9 — never both: a re-pointed
    // `playing` re-points `current_media` to match, so its cursor survives.
    let named = raw
        .playing
        .and_then(Value::as_u64)
        .map(PlaylistId::from_raw);
    let playing = named.filter(|id| playlists.iter().any(|p| p.id() == *id));
    let (playing, current_media) = match playing {
        Some(playing) => {
            if let Some(playlist) = playlists.iter_mut().find(|p| p.id() == playing) {
                let queue = playlist.queue();
                let cursor_media = queue
                    .active()
                    .and_then(|id| queue.get(id))
                    .map(|e| e.media());
                if cursor_media.is_some() && cursor_media != current_media.as_ref() {
                    note(
                        &mut reset,
                        QueueReset::ActiveReference(ActiveProblem::MediaMismatch),
                    );
                    let _ = playlist.queue_mut().set_active(None);
                }
            }
            (playing, current_media)
        }
        None => {
            note(
                &mut reset,
                QueueReset::Playlists(PlaylistProblem::DanglingPlaying),
            );
            let queue = playlists[0].queue();
            let media = queue
                .active()
                .and_then(|id| queue.get(id))
                .map(|e| e.media().clone());
            (first, media)
        }
    };

    Recovered {
        playlists,
        playing,
        entry_ids,
        playlist_ids,
        current_media,
        reset,
    }
}

/// Decodes the `queue`/`active_entry` fields of a state file, applying the
/// ten ordered recovery rules (task brief §"Recovery rules"). Never fails:
/// a problem at any step resets exactly the part it affects and reports why,
/// so a damaged queue can never cost a checkpoint.
pub fn recover_queue(
    queue: Option<&Value>,
    active: Option<&Value>,
    current_media: Option<&MediaId>,
) -> (Queue, Option<QueueReset>) {
    let entries = match queue {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => match decode_entries(items) {
            // Schema 3 had one queue, so the global cap was also its own cap.
            // Schema 4 spreads the same cap over every playlist, which is why
            // `decode_entries` no longer knows about it (M8 §6 rule 6).
            Ok(entries) if entries.len() > MAX_PLAYLIST_ENTRIES => {
                return (
                    Queue::default(),
                    Some(QueueReset::WholeQueue(QueueProblem::OverCapacity {
                        found: entries.len(),
                    })),
                );
            }
            Ok(entries) => entries,
            Err(problem) => return (Queue::default(), Some(QueueReset::WholeQueue(problem))),
        },
        Some(_) => {
            return (
                Queue::default(),
                Some(QueueReset::WholeQueue(QueueProblem::Malformed)),
            );
        }
    };
    let active_id = match active {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            None => {
                return (
                    Queue::from_parts(entries, None),
                    Some(QueueReset::ActiveReference(ActiveProblem::Malformed)),
                );
            }
            Some(raw) => {
                let id = QueueEntryId::from_raw(raw);
                match entries.iter().find(|e| e.id() == id) {
                    None => {
                        return (
                            Queue::from_parts(entries, None),
                            Some(QueueReset::ActiveReference(ActiveProblem::Dangling)),
                        );
                    }
                    Some(entry) if Some(entry.media()) != current_media => {
                        return (
                            Queue::from_parts(entries, None),
                            Some(QueueReset::ActiveReference(ActiveProblem::MediaMismatch)),
                        );
                    }
                    Some(_) => Some(id),
                }
            }
        },
    };
    (Queue::from_parts(entries, active_id), None)
}

fn decode_entries(items: &[Value]) -> Result<Vec<QueueEntry>, QueueProblem> {
    let dtos = items
        .iter()
        .map(|item| {
            serde_json::from_value::<QueueEntryDto>(item.clone())
                .map_err(|_| QueueProblem::Malformed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = BTreeSet::new();
    if !dtos.iter().all(|dto| seen.insert(dto.id)) {
        return Err(QueueProblem::DuplicateIds);
    }
    dtos.into_iter()
        .map(entry_from_dto)
        .collect::<Result<Vec<_>, _>>()
}

/// Builds a `QueueSource` from its DTO, failing `Malformed` when the source's
/// own path/URL does not construct, then `SourceMismatch` unless it names the
/// same identity as `media` (Task 3's [`source_matches`]). Duration decodes
/// to `DisplayDuration` only when `duration_ms` is present alongside a known
/// `duration_source` label; an unknown label drops the duration, never the
/// entry.
fn entry_from_dto(dto: QueueEntryDto) -> Result<QueueEntry, QueueProblem> {
    let source = match dto.source {
        SourceDto::Local { path } => {
            let path =
                AbsolutePath::new(PathBuf::from(path)).map_err(|_| QueueProblem::Malformed)?;
            QueueSource::LocalFile(path)
        }
        SourceDto::Remote { url } => {
            let url = NormalizedUrl::parse(&url).map_err(|_| QueueProblem::Malformed)?;
            QueueSource::RemoteUrl(url)
        }
        SourceDto::Podcast { fallback_url } => {
            let url = Url::parse(&fallback_url).map_err(|_| QueueProblem::Malformed)?;
            QueueSource::Podcast { fallback: url }
        }
    };
    if !source_matches(&dto.media, &source) {
        return Err(QueueProblem::SourceMismatch);
    }
    let duration = match (
        dto.display.duration_ms,
        dto.display.duration_source.as_deref(),
    ) {
        (Some(ms), Some("decoded")) => Some(DisplayDuration {
            value: Duration::from_millis(ms),
            source: DurationSource::Decoded(PositionProvenance::Established),
        }),
        (Some(ms), Some("decoded_estimated")) => Some(DisplayDuration {
            value: Duration::from_millis(ms),
            source: DurationSource::Decoded(PositionProvenance::Estimated),
        }),
        (Some(ms), Some("declared")) => Some(DisplayDuration {
            value: Duration::from_millis(ms),
            source: DurationSource::Declared,
        }),
        _ => None,
    };
    let display = DisplayMetadata {
        title: dto.display.title,
        artist: dto.display.artist,
        album: dto.display.album,
        year: dto.display.year,
        duration,
    };
    Ok(Queue::entry_from_parts(
        QueueEntryId::from_raw(dto.id),
        dto.media,
        source,
        display,
    ))
}
