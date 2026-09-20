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
use crate::queue::{
    DisplayDuration, DisplayMetadata, DurationSource, MAX_PLAYLIST_ENTRIES, Queue, QueueEntry,
    QueueEntryId, QueueSource, source_matches,
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
pub enum QueueReset {
    ActiveReference(ActiveProblem),
    WholeQueue(QueueProblem),
}

impl QueueReset {
    /// For the sanitized startup warning (§6): which fields were reset.
    pub fn fields_reset(self) -> &'static str {
        match self {
            Self::ActiveReference(_) => "the active queue entry",
            Self::WholeQueue(_) => "the queue and its active entry",
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
    let entries = dtos
        .into_iter()
        .map(entry_from_dto)
        .collect::<Result<Vec<_>, _>>()?;
    if entries.len() > MAX_PLAYLIST_ENTRIES {
        return Err(QueueProblem::OverCapacity {
            found: entries.len(),
        });
    }
    Ok(entries)
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
