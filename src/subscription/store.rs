//! `subscriptions.json`: the durable, user-authored list of feeds (design
//! doc §5.1). Reads and recovery are kept apart, exactly as
//! [`crate::persistence::store::StateStore`] separates them for checkpoints
//! (§5.5):
//!
//! - [`SubscriptionStore::read_snapshot`] is for every read-only command. It
//!   never renames and never writes; a missing file yields an empty set and
//!   anything else unreadable is a visible `Err`.
//! - [`SubscriptionStore::load`] is for the mutating commands (`subscribe`,
//!   `unsubscribe`, `refresh`). It applies the shared versioned-file policy:
//!   unreadable or an unsupported schema preserve the file and disable
//!   writing for the session; malformed quarantines it.
//!
//! Unlike `state.json` there is no entry cap and no eviction (§5.1): this is
//! user-authored data, and silently dropping a subscription to respect a
//! limit would be data loss.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::clock::Clock;
use crate::feed::error::FeedError;
use crate::media::id::NormalizedUrl;
use crate::persistence::json_file::{self, JsonFile, Load, Malformed};
use crate::persistence::store::LoadReason;

use super::model::{Subscription, validate_feed_id, validate_slug};

/// The only schema version this build writes or accepts (§5.1).
const SCHEMA_VERSION: u32 = 1;

const VERSIONS: RangeInclusive<u32> = SCHEMA_VERSION..=SCHEMA_VERSION;

/// How every reason this store reports names its file.
const FILE: &str = "subscriptions file";

/// The on-disk envelope (§5.1's JSON shape).
#[derive(Serialize, Deserialize)]
struct SubscriptionFile {
    schema_version: u32,
    subscriptions: Vec<SubscriptionRecord>,
}

/// One record inside `subscriptions.json`. Deliberately a plain DTO,
/// separate from [`Subscription`]: deserializing straight into the domain
/// type would let a `feed_id` or `slug` skip §5.6's validation.
#[derive(Clone, Serialize, Deserialize)]
struct SubscriptionRecord {
    feed_id: String,
    slug: String,
    title: Option<String>,
    fetch_url: Url,
    #[serde(with = "time::serde::rfc3339")]
    added_at: OffsetDateTime,
}

/// A decoded, validated subscription list.
#[derive(Clone, Debug)]
pub struct SubscriptionSnapshot {
    pub subscriptions: Vec<Subscription>,
}

/// The outcome of [`SubscriptionStore::load`].
pub struct SubscriptionLoad {
    pub snapshot: SubscriptionSnapshot,
    pub writable: bool,
    pub reason: LoadReason,
}

/// The full shape, then §5.6's semantic validation, once the shared
/// lifecycle has checked the version. Touches no filesystem.
fn decode(_version: u32, bytes: &[u8]) -> Result<SubscriptionSnapshot, Malformed> {
    let file = serde_json::from_slice::<SubscriptionFile>(bytes)
        .map_err(|source| json_file::malformed(&source))?;
    validate_records(file.subscriptions).map(|subscriptions| SubscriptionSnapshot { subscriptions })
}

fn empty() -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        subscriptions: Vec::new(),
    }
}

/// §5.6's semantic validation: every `feed_id` matches the `FeedId` pattern
/// and is unique, every `slug` matches its pattern and is unique, and every
/// `fetch_url` parses as `http(s)`. Any violation makes the whole file
/// malformed — there is no way to tell which record was corrupted — and an
/// id is validated into a `FeedId` before anything else touches it, so a
/// traversal-shaped id never reaches a path built from it.
fn validate_records(records: Vec<SubscriptionRecord>) -> Result<Vec<Subscription>, Malformed> {
    let mut seen_ids = BTreeSet::new();
    let mut seen_slugs = BTreeSet::new();
    let mut subscriptions = Vec::with_capacity(records.len());

    for record in records {
        // `validate_feed_id`'s `Err` is a deliberate placeholder (R4, see
        // `subscription::model`'s doc comment on the function): it must be
        // caught and re-mapped here, never propagated with `?`, since this
        // decode step's own `Malformed` is what carries the real meaning for
        // subscriptions.json.
        let feed_id = validate_feed_id(&record.feed_id)
            .map_err(|_| Malformed::Invalid("a feed id is not valid"))?;
        if !seen_ids.insert(feed_id.as_str().to_string()) {
            return Err(Malformed::Invalid(
                "a feed id is used by more than one subscription",
            ));
        }

        validate_slug(&record.slug).map_err(|_| Malformed::Invalid("a slug is not valid"))?;
        if !seen_slugs.insert(record.slug.clone()) {
            return Err(Malformed::Invalid(
                "a slug is used by more than one subscription",
            ));
        }

        // The DTO already deserialized `fetch_url` as a `Url`, so any scheme
        // parses; `NormalizedUrl::parse` re-checks it is `http(s)` with a
        // host. Only its acceptance is used here — the stored field stays
        // the plain `Url` the DTO carries.
        NormalizedUrl::parse(record.fetch_url.as_str())
            .map_err(|_| Malformed::Invalid("a fetch_url is not http(s)"))?;

        subscriptions.push(Subscription {
            feed_id,
            slug: record.slug,
            title: record.title,
            fetch_url: record.fetch_url,
            added_at: record.added_at,
        });
    }

    Ok(subscriptions)
}

/// Converts a validated domain [`Subscription`] back into the on-disk DTO.
fn to_record(subscription: &Subscription) -> SubscriptionRecord {
    SubscriptionRecord {
        feed_id: subscription.feed_id.as_str().to_string(),
        slug: subscription.slug.clone(),
        title: subscription.title.clone(),
        fetch_url: subscription.fetch_url.clone(),
        added_at: subscription.added_at,
    }
}

pub struct SubscriptionStore {
    file: JsonFile,
}

impl SubscriptionStore {
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

    /// The non-mutating counterpart to [`Self::load`] (§5.1, §5.5): every
    /// read-only command (`feeds`, `episodes`) calls this instead, so that
    /// merely displaying subscriptions never quarantines or creates
    /// anything. A missing file is the normal "no subscriptions yet" state;
    /// anything present and unreadable, malformed or an unsupported version
    /// is a visible error, so a listing can never mistake "could not be
    /// read" for "nothing is subscribed".
    pub fn read_snapshot(&self) -> Result<SubscriptionSnapshot, FeedError> {
        match self.file.read(VERSIONS, decode) {
            Ok(snapshot) => Ok(snapshot.unwrap_or_else(empty)),
            Err(rejected) => Err(FeedError::SubscriptionsUnreadable {
                reason: rejected.reason(FILE, SCHEMA_VERSION),
            }),
        }
    }

    /// Used by the mutating commands (`subscribe`, `unsubscribe`, `refresh`)
    /// (§5.1). Unlike [`Self::read_snapshot`], a malformed file is
    /// quarantined so subscription management is never blocked by a single
    /// corrupt file; an unreadable file or an unsupported schema version is
    /// preserved in place instead, with writing disabled for the session.
    pub fn load(&self) -> SubscriptionLoad {
        let Load {
            found,
            writable,
            reason,
        } = self.file.load(VERSIONS, decode);
        SubscriptionLoad {
            snapshot: found.map_or_else(empty, |found| found.value),
            writable,
            reason,
        }
    }

    /// Validates the complete snapshot (so a public DTO cannot bypass §5.6's
    /// invariants by construction alone), serializes it under the current
    /// schema, and replaces the file atomically. Synchronous on the calling
    /// thread, not through the writer thread: no playback path runs while
    /// `subscribe` or `refresh` executes (§5.1).
    pub fn save(&self, snapshot: &SubscriptionSnapshot) -> Result<(), FeedError> {
        let records: Vec<SubscriptionRecord> =
            snapshot.subscriptions.iter().map(to_record).collect();

        // Re-runs exactly the load-side validation over the records about to
        // be written, so a `SubscriptionSnapshot` built by hand (rather than
        // decoded from disk) cannot write invalid data just because it
        // skipped `read_snapshot`/`load`.
        validate_records(records.clone()).map_err(|malformed| {
            FeedError::SubscriptionsUnreadable {
                reason: malformed.reason(FILE),
            }
        })?;

        self.file.save(&SubscriptionFile {
            schema_version: SCHEMA_VERSION,
            subscriptions: records,
        })?;
        Ok(())
    }
}
