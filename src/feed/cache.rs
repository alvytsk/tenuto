//! The per-feed episode cache (design doc §5.2): `$XDG_CACHE_HOME` (or
//! platform equivalent) `/tenuto/feeds/<feed_id>.json`, one file per feed,
//! keyed by [`FeedId`] and never by slug — a rename cannot orphan it.
//!
//! It stores the **parsed** result, not raw XML, so `episodes` never
//! re-parses on every listing, and it carries a [`PARSER_VERSION`] because a
//! later parser fix would not reach already-cached data. Validators and the
//! per-check timestamps live here too, replaced in the same atomic write as
//! the episodes, so a validator can never describe a snapshot other than the
//! one on display.
//!
//! This module is disposable: nothing here ever touches the network, and
//! [`CacheStore::read`] never mkdirs, renames or deletes. [`CacheStore::save`]
//! is the only writer, and it validates the value it is given exactly as
//! [`CacheStore::read`] validates what it decodes, so a `CachedFeed` built by
//! hand — not decoded from disk — cannot bypass §5.6's invariants merely by
//! skipping a read.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::feed::episode::BoundFeed;
use crate::feed::error::FeedError;
use crate::http::document::CacheValidators;
use crate::media::Episode;
use crate::media::id::{FeedId, MediaId};
use crate::media::source::SourceLocation;
use crate::persistence::PersistenceError;
use crate::persistence::atomic::replace_bytes;
use crate::subscription::model::{Subscription, validate_feed_id};

/// The only schema version this build writes or accepts.
pub const CACHE_SCHEMA_VERSION: u32 = 1;

/// The parser version stamped onto every cache entry this build writes. A
/// mismatch means the file was written by a parser this build no longer
/// trusts to have produced it — distinct from an unsupported schema, and
/// recovered by an unconditional refetch rather than a quarantine (§5.4).
pub const PARSER_VERSION: u32 = 2;

/// One cached episode: [`Self::episode`]'s exact inputs, plus the enclosure
/// metadata that is a claim rather than evidence (§2.1) and so never reaches
/// [`Episode`] itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedEpisode {
    pub media_id: MediaId,
    pub enclosure_url: Option<Url>,
    pub enclosure_length: Option<u64>,
    pub enclosure_mime: Option<String>,
    pub title: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub published: Option<OffsetDateTime>,
    pub declared_duration_secs: Option<u64>,
    /// The episode's own `itunes:image`. Defaulted so a cache written before
    /// this field existed still decodes; it fills in on the next refresh.
    #[serde(default)]
    pub image: Option<Url>,
}

/// The on-disk shape of one feed's cache entry (§5.2's JSON shape,
/// reproduced field-for-field).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedFeed {
    pub schema_version: u32,
    pub parser_version: u32,
    pub feed_id: String,
    pub fetched_from: Url,
    #[serde(with = "time::serde::rfc3339")]
    pub last_refreshed_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_fetched_at: OffsetDateTime,
    pub validators: CacheValidators,
    pub title: Option<String>,
    pub site_link: Option<Url>,
    /// The feed-level `itunes:image`; defaulted like [`CachedEpisode::image`].
    #[serde(default)]
    pub image: Option<Url>,
    pub skipped_items: usize,
    pub episodes: Vec<CachedEpisode>,
}

impl CachedEpisode {
    /// Reconstructs the domain [`Episode`] this entry was built from, without
    /// re-resolving identity from `enclosure_url` — the identity already
    /// lives in `media_id`, exactly as [`crate::feed::episode::bind_feed`]
    /// produced it.
    pub fn episode(&self) -> Episode {
        Episode {
            id: self.media_id.clone(),
            source: self.enclosure_url.clone().map(SourceLocation::Http),
            title: self.title.clone(),
            published: self.published,
            declared_duration: self
                .declared_duration_secs
                .map(std::time::Duration::from_secs),
        }
    }
}

impl CachedFeed {
    /// Flattens a freshly bound [`BoundFeed`] into the cache's JSON shape.
    /// `final_url` is where the representation being cached was actually
    /// retrieved (§5.2) — not necessarily `id`'s subscription's `fetch_url` —
    /// and `now` stamps both `last_refreshed_at` and `last_fetched_at`, since
    /// this always describes a freshly parsed 200.
    pub fn from_bound(
        id: &FeedId,
        feed: BoundFeed,
        final_url: Url,
        validators: CacheValidators,
        now: OffsetDateTime,
    ) -> Self {
        let episodes = feed
            .items
            .into_iter()
            .map(|item| {
                let enclosure_url = match &item.episode.source {
                    Some(SourceLocation::Http(url)) => Some(url.clone()),
                    _ => None,
                };
                CachedEpisode {
                    media_id: item.episode.id,
                    enclosure_url,
                    enclosure_length: item.enclosure_length,
                    enclosure_mime: item.enclosure_mime,
                    title: item.episode.title,
                    published: item.episode.published,
                    declared_duration_secs: item
                        .episode
                        .declared_duration
                        .map(|duration| duration.as_secs()),
                    image: item.image,
                }
            })
            .collect();

        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            parser_version: PARSER_VERSION,
            feed_id: id.as_str().to_string(),
            fetched_from: final_url,
            last_refreshed_at: now,
            last_fetched_at: now,
            validators,
            title: feed.title,
            site_link: feed.site_link,
            image: feed.image,
            skipped_items: feed.skipped,
            episodes,
        }
    }
}

/// Only the fields every future version is obliged to keep. Read before the
/// full shape, mirroring [`crate::subscription::store`]'s and
/// [`crate::persistence::store`]'s version-first decode, so that an
/// unsupported schema or a stale parser is never misclassified as generic
/// corruption.
#[derive(Deserialize)]
struct VersionEnvelope {
    schema_version: u32,
    parser_version: u32,
}

/// The pure decode/validate step shared by [`CacheStore::read`] and
/// [`CacheStore::save`]. Touches no filesystem state.
enum CacheDecodeError {
    /// Deliberately carrying only a sanitized, pre-built reason — never a
    /// raw `serde_json::Error` message or an offending value, both of which
    /// could quote an untrusted URL back out (design doc §7.2).
    Malformed(String),
    UnsupportedSchema,
    ParserMismatch {
        found: u32,
        expected: u32,
    },
}

fn check_versions(schema_version: u32, parser_version: u32) -> Result<(), CacheDecodeError> {
    if schema_version != CACHE_SCHEMA_VERSION {
        return Err(CacheDecodeError::UnsupportedSchema);
    }
    if parser_version != PARSER_VERSION {
        return Err(CacheDecodeError::ParserMismatch {
            found: parser_version,
            expected: PARSER_VERSION,
        });
    }
    Ok(())
}

/// Whether a URL clears the public-source bar (`src/media`, one
/// implementation for the whole crate) - the same usability bar §4.5 and
/// [`crate::feed::episode::bind_feed`] hold enclosures to.
fn is_http_url(url: &Url) -> bool {
    crate::media::is_public_source_url(url)
}

/// §5.6's semantic validation, extended by this task's additional checks:
/// the envelope's own `feed_id` is a valid `FeedId` equal to `id`; every
/// episode's `media_id` is a `MediaId::PodcastEpisode` whose `feed()` equals
/// `id`; no two episodes share a `media_id`; `fetched_from` and the
/// validator URL are both HTTP(S); every present `enclosure_url` is a usable
/// HTTP(S) URL. A valid identity with no enclosure remains legal.
fn validate_cache(feed: &CachedFeed, id: &FeedId) -> Result<(), CacheDecodeError> {
    check_versions(feed.schema_version, feed.parser_version)?;

    // `validate_feed_id`'s `Err` is a deliberate placeholder (R4): caught and
    // re-mapped here, never propagated with `?`.
    let feed_id = validate_feed_id(&feed.feed_id)
        .map_err(|_| CacheDecodeError::Malformed("cache feed id is not valid".to_string()))?;
    if feed_id != *id {
        return Err(CacheDecodeError::Malformed(
            "cache feed id does not match the requested feed".to_string(),
        ));
    }

    if !is_http_url(&feed.fetched_from) {
        return Err(CacheDecodeError::Malformed(
            "fetched_from is not http(s)".to_string(),
        ));
    }
    if !is_http_url(&feed.validators.url) {
        return Err(CacheDecodeError::Malformed(
            "validator url is not http(s)".to_string(),
        ));
    }

    let mut seen = BTreeSet::new();
    for episode in &feed.episodes {
        if episode.media_id.feed() != Some(&feed_id) {
            return Err(CacheDecodeError::Malformed(
                "an episode id is not a podcast episode for this feed".to_string(),
            ));
        }
        if !seen.insert(episode.media_id.to_string()) {
            return Err(CacheDecodeError::Malformed(
                "an episode key is used by more than one entry".to_string(),
            ));
        }
        if let Some(url) = &episode.enclosure_url
            && !is_http_url(url)
        {
            return Err(CacheDecodeError::Malformed(
                "an enclosure url is not http(s)".to_string(),
            ));
        }
    }

    Ok(())
}

/// Decodes and validates a cache file's bytes for `id`: the version envelope
/// first (so an unsupported schema or a stale parser is diagnosed before the
/// full shape is ever parsed), then the full shape, then §5.6's semantic
/// validation.
fn decode_cache(bytes: &[u8], id: &FeedId) -> Result<CachedFeed, CacheDecodeError> {
    let envelope =
        serde_json::from_slice::<VersionEnvelope>(bytes).map_err(|source| malformed(&source))?;
    check_versions(envelope.schema_version, envelope.parser_version)?;

    let feed = serde_json::from_slice::<CachedFeed>(bytes).map_err(|source| malformed(&source))?;
    validate_cache(&feed, id)?;
    Ok(feed)
}

/// Sanitizes a `serde_json::Error` into a category plus line/column,
/// deliberately dropping its `Display` text and any source: an episode's
/// `media_id` or `enclosure_url` is untrusted and may carry a URL.
fn malformed(source: &serde_json::Error) -> CacheDecodeError {
    use serde_json::error::Category;

    let category = match source.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    CacheDecodeError::Malformed(format!(
        "cache file is malformed ({category} error at line {}, column {})",
        source.line(),
        source.column()
    ))
}

/// Turns a [`CacheDecodeError`] into the `FeedError` a caller sees, naming
/// the subscription by its slug — the same presentation identity every other
/// `FeedError` variant in this family uses.
fn to_feed_error(error: CacheDecodeError, slug: &str) -> FeedError {
    match error {
        CacheDecodeError::Malformed(detail) => FeedError::CacheCorrupt {
            slug: slug.to_string(),
            detail,
        },
        // A static diagnostic, deliberately: unlike a parser mismatch, an
        // unsupported schema has no dedicated typed field to carry the
        // found version, and this detail string never varies with input.
        CacheDecodeError::UnsupportedSchema => FeedError::CacheCorrupt {
            slug: slug.to_string(),
            detail: "unsupported cache schema version".to_string(),
        },
        CacheDecodeError::ParserMismatch { found, expected } => FeedError::CacheParserMismatch {
            slug: slug.to_string(),
            found,
            expected,
        },
    }
}

/// The per-feed episode cache directory: one JSON file per subscribed feed.
pub struct CacheStore {
    feeds_dir: PathBuf,
}

impl CacheStore {
    pub fn new(feeds_dir: PathBuf) -> Self {
        Self { feeds_dir }
    }

    /// The path a feed's cache entry lives at, validating `id` first (R4):
    /// [`validate_feed_id`]'s `Err` is a placeholder that must never escape
    /// as `SubscriptionsUnreadable`, so it is caught and remapped to
    /// `CacheCorrupt` here, before any path is built from the id. This
    /// guards against a `FeedId` that reached this store without having
    /// passed through subscription-load validation — `FeedId::new` itself
    /// only rejects an empty string, not a traversal-shaped one, and
    /// `MediaId`'s own decode path (`src/media/id.rs`) builds `FeedId`
    /// values that never pass `validate_feed_id` either. Unreachable today
    /// only by call-site discipline, not structurally, so both fields are
    /// **static**, exactly like the unsupported-schema diagnostic below: an
    /// id that already failed validation is precisely the id least safe to
    /// echo back — `slug` is the one field every `FeedError` `Display`
    /// prints as a bare, trusted identifier (`CacheCorrupt`'s message
    /// suggests `run tenuto refresh {slug}`), so a traversal-shaped or
    /// otherwise malformed id must never reach it, under `Debug` as much as
    /// `Display` (§7.2).
    pub fn path_for(&self, id: &FeedId) -> Result<PathBuf, FeedError> {
        validate_feed_id(id.as_str()).map_err(|_| FeedError::CacheCorrupt {
            slug: "<invalid feed id>".to_string(),
            detail: "feed id is not valid".to_string(),
        })?;
        Ok(self.feeds_dir.join(format!("{}.json", id.as_str())))
    }

    /// Reads and validates `subscription`'s cache entry. Never creates the
    /// cache directory, never renames and never deletes anything — a missing
    /// file is `CacheMissing`, the normal "never refreshed" state, and every
    /// other filesystem failure is a `PersistenceError::Io` naming the
    /// attempted operation.
    pub fn read(&self, subscription: &Subscription) -> Result<CachedFeed, FeedError> {
        let path = self.path_for(&subscription.feed_id)?;

        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(FeedError::CacheMissing {
                    slug: subscription.slug.clone(),
                });
            }
            Err(source) => {
                return Err(FeedError::Persistence(PersistenceError::Io {
                    path,
                    op: "read",
                    source,
                }));
            }
        };

        decode_cache(&bytes, &subscription.feed_id)
            .map_err(|error| to_feed_error(error, &subscription.slug))
    }

    /// Validates `feed` exactly as [`Self::read`] validates what it decodes
    /// — so a `CachedFeed` built by hand cannot bypass §5.6's invariants by
    /// skipping a read — serializes it, and replaces the file atomically via
    /// [`replace_bytes`]. A validation failure never reaches the filesystem:
    /// the previous cache entry, if any, is left exactly as it was.
    pub fn save(&self, subscription: &Subscription, feed: &CachedFeed) -> Result<(), FeedError> {
        let path = self.path_for(&subscription.feed_id)?;

        validate_cache(feed, &subscription.feed_id)
            .map_err(|error| to_feed_error(error, &subscription.slug))?;

        let bytes = serde_json::to_vec_pretty(feed).map_err(|source| FeedError::CacheCorrupt {
            slug: subscription.slug.clone(),
            detail: format!("cannot serialize cache: {source}"),
        })?;

        replace_bytes(&path, &bytes)?;
        Ok(())
    }

    /// Removes a feed's cache entry, treating an already-missing file as
    /// success — `episodes` and `refresh` alike must be able to drop a stale
    /// cache without caring whether one was ever written.
    pub fn remove(&self, id: &FeedId) -> Result<(), FeedError> {
        let path = self.path_for(id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(FeedError::Persistence(PersistenceError::Io {
                path,
                op: "remove",
                source,
            })),
        }
    }
}
