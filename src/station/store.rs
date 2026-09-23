//! `stations.json`: the durable, user-authored list of saved radio stations
//! (design doc §4). Reads and recovery are kept apart, exactly as
//! [`crate::subscription::store::SubscriptionStore`] separates them:
//!
//! - [`StationStore::read_snapshot`] is for every read-only command. It
//!   never renames and never writes; a missing file yields an empty set and
//!   anything else unreadable is a visible `Err`.
//! - [`StationStore::load`] is for the mutating commands (adding, removing
//!   or re-probing a station). It applies `SubscriptionStore::load`'s
//!   policy: unreadable or an unsupported schema preserve the file and
//!   disable writing for the session; malformed quarantines it.
//!
//! Like `subscriptions.json` there is no entry cap and no eviction: this is
//! user-authored data, and silently dropping a station to respect a limit
//! would be data loss.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::clock::Clock;
use crate::feed::error::FeedError;
use crate::http::source::StationIdentity;
use crate::media::id::{MediaId, NormalizedUrl};
use crate::persistence::atomic::replace_bytes;
use crate::persistence::store::{LoadReason, MAX_QUARANTINE_CANDIDATES};

use super::model::{Station, validate_station_slug};

/// The only schema version this build writes or accepts (§4).
const SCHEMA_VERSION: u32 = 1;

/// The on-disk envelope (§4's JSON shape).
#[derive(Serialize, Deserialize)]
struct StationFile {
    schema_version: u32,
    stations: Vec<StationRecord>,
}

/// One record inside `stations.json`. Deliberately a plain DTO, separate
/// from [`Station`]: deserializing straight into the domain type would let
/// a `slug` or `media` skip validation.
#[derive(Clone, Serialize, Deserialize)]
struct StationRecord {
    slug: String,
    url: Url,
    /// `Station::media`'s `MediaId::to_string()` form, validated back
    /// through `MediaId`'s own parser on load — never deserialized straight
    /// into the domain type, for the same reason `SubscriptionRecord`
    /// validates `feed_id`.
    media: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<IdentityRecord>,
    #[serde(with = "time::serde::rfc3339")]
    added_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    probed_at: Option<OffsetDateTime>,
}

/// [`StationIdentity`]'s on-disk shape, nested under `StationRecord::identity`
/// so the persisted shape matches the domain type one-to-one: `None` means
/// unverified because the field itself is absent, never because its four
/// members all happen to be `None`. A flat, field-level encoding would have
/// relied on `is_icy` (`src/http/response.rs`) always setting at least one
/// of `icy-name`/`icy-br` before classifying a response as live — true
/// today, but not a promise this file format should depend on, and M7 §12
/// already plans to widen live classification in a way that could make an
/// all-absent-but-verified identity reachable.
#[derive(Clone, Serialize, Deserialize)]
struct IdentityRecord {
    name: Option<String>,
    genre: Option<String>,
    bitrate_kbps: Option<u32>,
    logo: Option<Url>,
}

/// Only the field every future version is obliged to keep. Read before the
/// full model, mirroring [`crate::persistence::store`]'s version-first
/// decode so that an unsupported version is never misclassified as garbage.
#[derive(Deserialize)]
struct VersionEnvelope {
    schema_version: u32,
}

/// A decoded, validated station list.
#[derive(Clone, Debug)]
pub struct StationSnapshot {
    pub stations: Vec<Station>,
}

/// The outcome of [`StationStore::load`].
pub struct StationLoad {
    pub snapshot: StationSnapshot,
    pub writable: bool,
    pub reason: LoadReason,
}

/// The pure decode step shared by [`StationStore::load`] and
/// [`StationStore::read_snapshot`]: version envelope first, then the full
/// shape, then semantic validation. Touches no filesystem state at all — no
/// read, no quarantine, no write — so what each caller does with a rejection
/// is entirely theirs.
enum DecodeError {
    /// Deliberately carrying only a sanitized, pre-built reason: a station
    /// record can hold an untrusted URL, and this must never echo the
    /// file's raw JSON bytes back into a log or an error message, under
    /// `Debug` as much as `Display` (design doc §7.2).
    Malformed(String),
    UnsupportedVersion(u32),
}

fn decode_stations(bytes: &[u8]) -> Result<StationSnapshot, DecodeError> {
    let envelope =
        serde_json::from_slice::<VersionEnvelope>(bytes).map_err(|source| malformed(&source))?;

    if envelope.schema_version != SCHEMA_VERSION {
        return Err(DecodeError::UnsupportedVersion(envelope.schema_version));
    }

    let file = serde_json::from_slice::<StationFile>(bytes).map_err(|source| malformed(&source))?;

    validate_records(file.stations).map(|stations| StationSnapshot { stations })
}

/// §4's semantic validation: every `slug` matches its pattern and is
/// unique, every `url` parses as `http(s)`, and every `media` round-trips
/// through `MediaId`'s own parser. Any violation makes the whole file
/// malformed — there is no way to tell which record was corrupted.
fn validate_records(records: Vec<StationRecord>) -> Result<Vec<Station>, DecodeError> {
    let mut seen_slugs = BTreeSet::new();
    let mut stations = Vec::with_capacity(records.len());

    for record in records {
        validate_station_slug(&record.slug)
            .map_err(|_| DecodeError::Malformed("a slug is not valid".to_string()))?;
        if !seen_slugs.insert(record.slug.clone()) {
            return Err(DecodeError::Malformed(
                "a slug is used by more than one station".to_string(),
            ));
        }

        // The DTO already deserialized `url` as a `Url`, so any scheme
        // parses; `NormalizedUrl::parse` re-checks it is `http(s)` with a
        // host. The stored field stays the plain `Url` the DTO carries, but
        // the normalized form is also what `media` must derive from below.
        let normalized = NormalizedUrl::parse(record.url.as_str())
            .map_err(|_| DecodeError::Malformed("a url is not http(s)".to_string()))?;

        let media = record
            .media
            .parse::<MediaId>()
            .map_err(|_| DecodeError::Malformed("a media identifier is not valid".to_string()))?;

        // §6: `media` is a derived field, not an independent one. A
        // hand-edited `url` whose `media` no longer matches it must be
        // quarantined like any other malformed file — silently trusting the
        // stale `media` would let a later Enter enqueue the new URL under
        // the old identity's slug, drawing no tick, so a second Enter would
        // add a duplicate.
        if media != MediaId::RemoteUrl(normalized) {
            return Err(DecodeError::Malformed(
                "a media identifier does not match its url".to_string(),
            ));
        }

        let identity = record.identity.map(|identity| StationIdentity {
            name: identity.name,
            genre: identity.genre,
            bitrate_kbps: identity.bitrate_kbps,
            logo: identity.logo,
        });

        stations.push(Station {
            slug: record.slug,
            url: record.url,
            media,
            identity,
            added_at: record.added_at,
            probed_at: record.probed_at,
        });
    }

    Ok(stations)
}

/// Sanitizes a `serde_json::Error` into a category, deliberately dropping its
/// `Display` text: it can quote the file's raw content verbatim, and a
/// station record can carry an untrusted URL.
fn malformed(source: &serde_json::Error) -> DecodeError {
    use serde_json::error::Category;

    let category = match source.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    DecodeError::Malformed(format!(
        "stations file is malformed ({category} error at line {}, column {})",
        source.line(),
        source.column()
    ))
}

/// Converts a validated domain [`Station`] back into the on-disk DTO.
fn to_record(station: &Station) -> StationRecord {
    let identity = station.identity.as_ref().map(|identity| IdentityRecord {
        name: identity.name.clone(),
        genre: identity.genre.clone(),
        bitrate_kbps: identity.bitrate_kbps,
        logo: identity.logo.clone(),
    });
    StationRecord {
        slug: station.slug.clone(),
        url: station.url.clone(),
        media: station.media.to_string(),
        identity,
        added_at: station.added_at,
        probed_at: station.probed_at,
    }
}

pub struct StationStore {
    path: PathBuf,
    clock: Arc<dyn Clock>,
}

impl StationStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self { path, clock }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn now(&self) -> OffsetDateTime {
        self.clock.sample().wall
    }

    /// The non-mutating counterpart to [`Self::load`] (§4): every read-only
    /// command (the Radio tab's listing) calls this instead, so that merely
    /// displaying stations never quarantines or creates anything. A missing
    /// file is the normal "no stations yet" state; anything present and
    /// unreadable, malformed or an unsupported version is a visible error,
    /// so a listing can never mistake "could not be read" for "nothing is
    /// saved".
    pub fn read_snapshot(&self) -> Result<StationSnapshot, FeedError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(StationSnapshot {
                    stations: Vec::new(),
                });
            }
            Err(error) => {
                return Err(FeedError::StationsUnreadable {
                    reason: format!("cannot read stations file: {error}"),
                });
            }
        };

        decode_stations(&bytes).map_err(|error| match error {
            DecodeError::Malformed(reason) => FeedError::StationsUnreadable { reason },
            DecodeError::UnsupportedVersion(found) => FeedError::StationsUnreadable {
                reason: format!(
                    "stations file is schema version {found}, and this build supports {SCHEMA_VERSION}"
                ),
            },
        })
    }

    /// Used by the mutating commands (adding, removing or re-probing a
    /// station) (§4). Unlike [`Self::read_snapshot`], a malformed file is
    /// quarantined so station management is never blocked by a single
    /// corrupt file; an unreadable file or an unsupported schema version is
    /// preserved in place instead, with writing disabled for the session.
    pub fn load(&self) -> StationLoad {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Self::fresh(true, LoadReason::Missing);
            }
            Err(error) => {
                tracing::warn!(
                    path = ?self.path,
                    %error,
                    "cannot read the stations file; leaving it in place and not writing this session"
                );
                return Self::fresh(false, LoadReason::Unreadable);
            }
        };

        match decode_stations(&bytes) {
            Ok(snapshot) => StationLoad {
                snapshot,
                writable: true,
                reason: LoadReason::Loaded,
            },
            Err(DecodeError::UnsupportedVersion(found)) => {
                tracing::warn!(
                    path = ?self.path,
                    found,
                    supported = SCHEMA_VERSION,
                    "unsupported stations schema; preserving the file and not writing this session"
                );
                Self::fresh(false, LoadReason::UnsupportedVersion { found })
            }
            Err(DecodeError::Malformed(reason)) => {
                tracing::warn!(path = ?self.path, reason, "stations file is malformed");
                self.reject_malformed()
            }
        }
    }

    /// Validates the complete snapshot (so a public DTO cannot bypass §4's
    /// invariants by construction alone), serializes it under the current
    /// schema, and replaces the file atomically.
    pub fn save(&self, snapshot: &StationSnapshot) -> Result<(), FeedError> {
        let records: Vec<StationRecord> = snapshot.stations.iter().map(to_record).collect();

        // Re-runs exactly the load-side validation over the records about to
        // be written, so a `StationSnapshot` built by hand (rather than
        // decoded from disk) cannot write invalid data just because it
        // skipped `read_snapshot`/`load`.
        validate_records(records.clone()).map_err(|error| match error {
            DecodeError::Malformed(reason) => FeedError::StationsUnreadable { reason },
            DecodeError::UnsupportedVersion(found) => FeedError::StationsUnreadable {
                reason: format!("unexpected internal schema version {found}"),
            },
        })?;

        let file = StationFile {
            schema_version: SCHEMA_VERSION,
            stations: records,
        };

        let bytes =
            serde_json::to_vec_pretty(&file).map_err(|source| FeedError::StationsUnreadable {
                reason: format!("cannot serialize stations: {source}"),
            })?;

        replace_bytes(&self.path, &bytes)?;
        Ok(())
    }

    /// The cached logo URL for `media`, if a saved station claims it (§8.1).
    /// `None` for an identity no station claims, which is how `active_cover`
    /// keeps today's behaviour for an ordinary remote entry.
    ///
    /// Keyed on `MediaId`, not `Url`: `active_cover` holds a
    /// `QueueSource::RemoteUrl(NormalizedUrl)` and `entry.media()` is the
    /// matching `MediaId::RemoteUrl`, so this compares the normalized
    /// identity both sides already agree on rather than two spellings of a
    /// URL.
    ///
    /// Reads the file. Only ever called when `cover_key` moves, never per
    /// frame — the same contract `podcast_artwork` already lives under.
    ///
    /// Refreshes on add and on re-probe, never mid-playback: this is R4's
    /// one exception (§8.1) — the store is authoritative for a station's
    /// artwork, where R4 elsewhere holds that stored identity never
    /// overrides a live open. A re-probed station shows its new logo on its
    /// next play, not before.
    pub fn logo_for(&self, media: &MediaId) -> Option<Url> {
        self.read_snapshot()
            .ok()?
            .stations
            .into_iter()
            .find(|station| &station.media == media)
            .and_then(|station| station.identity?.logo)
    }

    /// Whether the saved list claims `media` at all. Distinct from
    /// [`logo_for`], which answers only for a station that stored a logo: a
    /// station without one is still a station, and choosing the built-in
    /// cover for a queue entry turns on that difference.
    ///
    /// Reads the file, under the same contract as [`logo_for`] — called when
    /// `cover_key` moves, never per frame.
    ///
    /// [`logo_for`]: Self::logo_for
    pub fn is_station(&self, media: &MediaId) -> bool {
        self.read_snapshot().is_ok_and(|snapshot| {
            snapshot
                .stations
                .iter()
                .any(|station| &station.media == media)
        })
    }

    fn parent(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    fn fresh(writable: bool, reason: LoadReason) -> StationLoad {
        StationLoad {
            snapshot: StationSnapshot {
                stations: Vec::new(),
            },
            writable,
            reason,
        }
    }

    fn reject_malformed(&self) -> StationLoad {
        match self.quarantine() {
            Some(moved_to) => {
                tracing::warn!(path = ?self.path, ?moved_to, "stations file quarantined");
                Self::fresh(true, LoadReason::Quarantined { moved_to })
            }
            None => {
                tracing::warn!(
                    path = ?self.path,
                    "cannot quarantine the stations file; not writing this session"
                );
                Self::fresh(false, LoadReason::QuarantineFailed)
            }
        }
    }

    /// Move the file aside under a timestamped name, never over one that
    /// already exists — the same policy as
    /// [`crate::persistence::store::StateStore`]'s quarantine, including its
    /// 100-candidate bound. Single-user, single-process by design, so the
    /// exists-then-rename window is not a hazard worth more machinery.
    fn quarantine(&self) -> Option<PathBuf> {
        let stamp = stamp(self.clock.sample().wall);
        let dir = self.parent();
        for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
            let name = if suffix == 1 {
                format!("stations.json.rejected-{stamp}")
            } else {
                format!("stations.json.rejected-{stamp}-{suffix}")
            };
            let candidate = dir.join(name);
            if candidate.exists() {
                continue;
            }
            if fs::rename(&self.path, &candidate).is_ok() {
                return Some(candidate);
            }
            return None;
        }
        None
    }
}

/// `20260908T143211Z` — filesystem-safe, no colons.
fn stamp(at: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    )
}
