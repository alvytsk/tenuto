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
use crate::playlist::{
    DEFAULT_NAME, Field, MAX_PLAYLISTS, Playlist, PlaylistSet, RecordParts, Repair, ShuffleField,
    Stage,
};
use crate::queue::{
    DisplayDuration, DisplayMetadata, DurationSource, IdAllocator, MAX_PLAYLIST_ENTRIES,
    NewQueueEntry, Queue, QueueEntryId, QueueSource,
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
    pub set: PlaylistSet,
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

/// The diagnostic this file reports for a set-level repair. `playlists` and
/// `entries` are the raw counts the file held.
fn reset_for(repair: Repair, playlists: usize, entries: usize) -> QueueReset {
    match repair {
        Repair::TooManyPlaylists => {
            QueueReset::Playlists(PlaylistProblem::TooManyPlaylists { found: playlists })
        }
        Repair::DuplicatePlaylistId => QueueReset::Playlists(PlaylistProblem::DuplicatePlaylistId),
        Repair::IdsExhausted => QueueReset::Playlists(PlaylistProblem::IdsExhausted),
        Repair::OverCapacity => {
            QueueReset::Playlists(PlaylistProblem::OverCapacity { found: entries })
        }
        Repair::DuplicateEntryId => QueueReset::Playlists(PlaylistProblem::DuplicateEntryId),
        Repair::DanglingCursor => QueueReset::ActiveReference(ActiveProblem::Dangling),
        Repair::Shuffle => QueueReset::Playlists(PlaylistProblem::Shuffle),
        Repair::DanglingPlaying => QueueReset::Playlists(PlaylistProblem::DanglingPlaying),
        Repair::CursorMediaMismatch => QueueReset::ActiveReference(ActiveProblem::MediaMismatch),
    }
}

/// Rules 7–9 through the builder; its repairs come after every record's.
fn finish(
    recovery: crate::playlist::Recovery,
    playing: Option<u64>,
    current_media: Option<MediaId>,
    mut reset: Option<QueueReset>,
    (playlists, entries): (usize, usize),
) -> Recovered {
    let done = recovery.finish(playing, current_media.clone());
    for repair in done.repairs {
        note(&mut reset, reset_for(repair, playlists, entries));
    }
    Recovered {
        set: done.set,
        current_media: done.current_media.apply(current_media),
        reset,
    }
}

/// One stored playlist as parts, plus its own JSON problem: a malformed or
/// internally duplicated entry list resets that list (rule 2), and belongs
/// to the Entries stage.
fn record_parts(item: &Value) -> (RecordParts, Option<QueueReset>) {
    let (entries, problem) = match item.get("entries") {
        None | Some(Value::Null) => (Vec::new(), None),
        Some(Value::Array(entries)) => match decode_entries(entries) {
            Ok(entries) => (entries, None),
            Err(problem) => (Vec::new(), Some(QueueReset::WholeQueue(problem))),
        },
        Some(_) => (
            Vec::new(),
            Some(QueueReset::WholeQueue(QueueProblem::Malformed)),
        ),
    };
    let field = |value: Option<&Value>| match value {
        None | Some(Value::Null) => Field::Absent,
        Some(value) => value.as_u64().map_or(Field::Malformed, Field::Value),
    };
    let shuffle = match item.get("shuffle") {
        None | Some(Value::Null) => ShuffleField::Absent,
        Some(value) => match value.get("seed").and_then(Value::as_u64) {
            None => ShuffleField::BadSeed,
            Some(seed) => ShuffleField::Seeded {
                seed,
                first: field(value.get("first")),
            },
        },
    };
    let parts = RecordParts {
        id: item.get("id").and_then(Value::as_u64),
        entries,
        cursor: field(item.get("active_entry")),
        shuffle,
        name: item.get("name").and_then(Value::as_str).map(str::to_owned),
    };
    (parts, problem)
}

/// Schema 3 → 4: the one queue becomes `Default` with ID 1, IDs and cursor
/// carried over. `recover_queue` has already validated it and chosen the
/// reset; the builder finds nothing more to repair.
pub(super) fn migrate_queue(
    entries: Vec<(u64, NewQueueEntry)>,
    cursor: Option<u64>,
    current_media: Option<MediaId>,
    mut reset: Option<QueueReset>,
) -> Recovered {
    let mut entry_ids = IdAllocator::default();
    entries.iter().for_each(|(id, _)| entry_ids.observe(*id));
    let found = entries.len();
    let mut recovery = PlaylistSet::recovery(entry_ids, IdAllocator::starting_at(Some(2)));
    let outcome = recovery.push_record(RecordParts {
        id: Some(1),
        entries,
        cursor: cursor.map_or(Field::Absent, Field::Value),
        shuffle: ShuffleField::Absent,
        name: Some(DEFAULT_NAME.to_owned()),
    });
    for (_, repair) in outcome.repairs {
        note(&mut reset, reset_for(repair, 1, found));
    }
    finish(recovery, Some(1), current_media, reset, (1, found))
}

/// Schema 4's ten ordered recovery rules (M8 §6), with the rules themselves
/// in `PlaylistSet`'s builder. This function owns what is about the file:
/// the counter scan over every raw record, the file-level playlist count,
/// the raw entry count, file order, and which problem is reported first.
/// Never fails: damaged playlist data can never cost a checkpoint (P8).
pub(super) fn recover_playlists(
    raw: RawPlaylists<'_>,
    current_media: Option<MediaId>,
) -> Recovered {
    let mut reset = None;
    // Rule 1.
    let items = match raw.playlists {
        Some(Value::Array(items)) => items,
        other => {
            let problem = (!matches!(other, None | Some(Value::Null)))
                .then_some(QueueReset::Playlists(PlaylistProblem::Malformed));
            let recovery = PlaylistSet::recovery(
                counter(raw.next_entry_id, std::iter::empty()),
                counter(raw.next_playlist_id, std::iter::empty()),
            );
            return finish(recovery, None, current_media, problem, (0, 0));
        }
    };

    // Counters first, over the whole file, discarded records included, so
    // that the IDs minted below are exactly 0.2.0's.
    let playlist_seen = items.iter().filter_map(|item| item.get("id")?.as_u64());
    let entry_seen = items
        .iter()
        .filter_map(|item| item.get("entries")?.as_array())
        .flatten()
        .filter_map(|entry| entry.get("id")?.as_u64());
    let mut recovery = PlaylistSet::recovery(
        counter(raw.next_entry_id, entry_seen),
        counter(raw.next_playlist_id, playlist_seen),
    );
    let found_entries: usize = items
        .iter()
        .filter_map(|item| item.get("entries")?.as_array())
        .map(Vec::len)
        .sum();
    let found = (items.len(), found_entries);
    if items.len() > MAX_PLAYLISTS {
        note(
            &mut reset,
            QueueReset::Playlists(PlaylistProblem::TooManyPlaylists { found: items.len() }),
        );
    }

    // Records in file order. Per record: the builder's Count and Id repairs,
    // then this record's own JSON problem (Entries stage), then the rest. A
    // record skipped at Count or Id drops its JSON problem, as 0.2.0 never
    // decoded one (spec §8.4).
    for item in items {
        let (parts, own) = record_parts(item);
        let outcome = recovery.push_record(parts);
        let (early, late): (Vec<_>, Vec<_>) = outcome
            .repairs
            .into_iter()
            .partition(|(stage, _)| *stage <= Stage::Id);
        for (_, repair) in early {
            note(&mut reset, reset_for(repair, found.0, found.1));
        }
        if outcome.kept.is_some()
            && let Some(problem) = own
        {
            note(&mut reset, problem);
        }
        for (_, repair) in late {
            note(&mut reset, reset_for(repair, found.0, found.1));
        }
    }

    // Rules 7–9.
    let playing = raw.playing.and_then(Value::as_u64);
    finish(recovery, playing, current_media, reset, found)
}

/// Decodes the `queue`/`active_entry` fields of a state file, applying the
/// ten ordered recovery rules (task brief §"Recovery rules"). Never fails:
/// a problem at any step resets exactly the part it affects and reports why,
/// so a damaged queue can never cost a checkpoint. Returns the stored
/// entries with their IDs and the checked cursor, for `migrate_queue`.
pub fn recover_queue(
    queue: Option<&Value>,
    active: Option<&Value>,
    current_media: Option<&MediaId>,
) -> (Vec<(u64, NewQueueEntry)>, Option<u64>, Option<QueueReset>) {
    let entries = match queue {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => match decode_entries(items) {
            // Schema 3 had one queue, so the global cap was also its own cap.
            // Schema 4 spreads the same cap over every playlist, which is why
            // `decode_entries` no longer knows about it (M8 §6 rule 6).
            Ok(entries) if entries.len() > MAX_PLAYLIST_ENTRIES => {
                return (
                    Vec::new(),
                    None,
                    Some(QueueReset::WholeQueue(QueueProblem::OverCapacity {
                        found: entries.len(),
                    })),
                );
            }
            Ok(entries) => entries,
            Err(problem) => return (Vec::new(), None, Some(QueueReset::WholeQueue(problem))),
        },
        Some(_) => {
            return (
                Vec::new(),
                None,
                Some(QueueReset::WholeQueue(QueueProblem::Malformed)),
            );
        }
    };
    let cursor = match active {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            None => {
                return (
                    entries,
                    None,
                    Some(QueueReset::ActiveReference(ActiveProblem::Malformed)),
                );
            }
            Some(raw) => match entries.iter().find(|(id, _)| *id == raw) {
                None => {
                    return (
                        entries,
                        None,
                        Some(QueueReset::ActiveReference(ActiveProblem::Dangling)),
                    );
                }
                Some((_, entry)) if Some(entry.media()) != current_media => {
                    return (
                        entries,
                        None,
                        Some(QueueReset::ActiveReference(ActiveProblem::MediaMismatch)),
                    );
                }
                Some(_) => Some(raw),
            },
        },
    };
    (entries, cursor, None)
}

fn decode_entries(items: &[Value]) -> Result<Vec<(u64, NewQueueEntry)>, QueueProblem> {
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
/// same identity as `media` (checked by [`NewQueueEntry::new`]). Duration decodes
/// to `DisplayDuration` only when `duration_ms` is present alongside a known
/// `duration_source` label; an unknown label drops the duration, never the
/// entry.
fn entry_from_dto(dto: QueueEntryDto) -> Result<(u64, NewQueueEntry), QueueProblem> {
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
    let new =
        NewQueueEntry::new(dto.media, source, display).map_err(|_| QueueProblem::SourceMismatch)?;
    Ok((dto.id, new))
}
