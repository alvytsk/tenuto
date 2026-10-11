//! `stations.json`: the durable, user-authored list of saved radio stations
//! (design doc §4). Reads and recovery are kept apart, exactly as
//! [`crate::subscription::store::SubscriptionStore`] separates them:
//!
//! - [`StationStore::read_snapshot`] is for every read-only command. It
//!   never renames and never writes; a missing file yields an empty set and
//!   anything else unreadable is a visible `Err`.
//! - [`StationStore::load`] is for the mutating commands (adding, removing
//!   or re-probing a station). It applies the shared versioned-file
//!   policy: unreadable or an unsupported schema preserve the file and
//!   disable writing for the session; malformed quarantines it.
//!
//! Like `subscriptions.json` there is no entry cap and no eviction: this is
//! user-authored data, and silently dropping a station to respect a limit
//! would be data loss.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::clock::Clock;
use crate::feed::error::FeedError;
use crate::http::source::StationIdentity;
use crate::media::id::{MediaId, NormalizedUrl};
use crate::persistence::json_file::{self, JsonFile, Load, Malformed};
use crate::persistence::store::LoadReason;

use super::model::{Station, validate_station_slug};

/// The only schema version this build writes or accepts (§4).
const SCHEMA_VERSION: u32 = 1;

const VERSIONS: RangeInclusive<u32> = SCHEMA_VERSION..=SCHEMA_VERSION;

/// How every reason this store reports names its file.
const FILE: &str = "stations file";

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

/// The full shape, then §4's semantic validation, once the shared
/// lifecycle has checked the version. Touches no filesystem.
fn decode(_version: u32, bytes: &[u8]) -> Result<StationSnapshot, Malformed> {
    let file = serde_json::from_slice::<StationFile>(bytes)
        .map_err(|source| json_file::malformed(&source))?;
    validate_records(file.stations).map(|stations| StationSnapshot { stations })
}

fn empty() -> StationSnapshot {
    StationSnapshot {
        stations: Vec::new(),
    }
}

/// §4's semantic validation: every `slug` matches its pattern and is
/// unique, every `url` parses as `http(s)`, and every `media` round-trips
/// through `MediaId`'s own parser. Any violation makes the whole file
/// malformed — there is no way to tell which record was corrupted.
fn validate_records(records: Vec<StationRecord>) -> Result<Vec<Station>, Malformed> {
    let mut seen_slugs = BTreeSet::new();
    let mut stations = Vec::with_capacity(records.len());

    for record in records {
        validate_station_slug(&record.slug)
            .map_err(|_| Malformed::Invalid("a slug is not valid"))?;
        if !seen_slugs.insert(record.slug.clone()) {
            return Err(Malformed::Invalid(
                "a slug is used by more than one station",
            ));
        }

        // The DTO already deserialized `url` as a `Url`, so any scheme
        // parses; `NormalizedUrl::parse` re-checks it is `http(s)` with a
        // host. The stored field stays the plain `Url` the DTO carries, but
        // the normalized form is also what `media` must derive from below.
        let normalized = NormalizedUrl::parse(record.url.as_str())
            .map_err(|_| Malformed::Invalid("a url is not http(s)"))?;

        let media = record
            .media
            .parse::<MediaId>()
            .map_err(|_| Malformed::Invalid("a media identifier is not valid"))?;

        // §6: `media` is a derived field, not an independent one. A
        // hand-edited `url` whose `media` no longer matches it must be
        // quarantined like any other malformed file — silently trusting the
        // stale `media` would let a later Enter enqueue the new URL under
        // the old identity's slug, drawing no tick, so a second Enter would
        // add a duplicate.
        if media != MediaId::RemoteUrl(normalized) {
            return Err(Malformed::Invalid(
                "a media identifier does not match its url",
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
    file: JsonFile,
}

impl StationStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self {
            file: JsonFile::new(path, clock),
        }
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn now(&self) -> OffsetDateTime {
        self.file.now()
    }

    /// The non-mutating counterpart to [`Self::load`] (§4): every read-only
    /// command (the Radio tab's listing) calls this instead, so that merely
    /// displaying stations never quarantines or creates anything. A missing
    /// file is the normal "no stations yet" state; anything present and
    /// unreadable, malformed or an unsupported version is a visible error,
    /// so a listing can never mistake "could not be read" for "nothing is
    /// saved".
    pub fn read_snapshot(&self) -> Result<StationSnapshot, FeedError> {
        match self.file.read(VERSIONS, decode) {
            Ok(snapshot) => Ok(snapshot.unwrap_or_else(empty)),
            Err(rejected) => Err(FeedError::StationsUnreadable {
                reason: rejected.reason(FILE, SCHEMA_VERSION),
            }),
        }
    }

    /// Used by the mutating commands (adding, removing or re-probing a
    /// station) (§4). Unlike [`Self::read_snapshot`], a malformed file is
    /// quarantined so station management is never blocked by a single
    /// corrupt file; an unreadable file or an unsupported schema version is
    /// preserved in place instead, with writing disabled for the session.
    pub fn load(&self) -> StationLoad {
        let Load {
            found,
            writable,
            reason,
        } = self.file.load(VERSIONS, decode);
        StationLoad {
            snapshot: found.map_or_else(empty, |found| found.value),
            writable,
            reason,
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
        validate_records(records.clone()).map_err(|malformed| FeedError::StationsUnreadable {
            reason: malformed.reason(FILE),
        })?;

        self.file.save(&StationFile {
            schema_version: SCHEMA_VERSION,
            stations: records,
        })?;
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
}
