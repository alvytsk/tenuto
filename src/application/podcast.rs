//! Resolves which URL to play for a queued podcast episode (M5 plan, Task
//! 10): the local subscription and feed-cache files only, never the
//! network and never an implicit refresh. A queued episode carries its
//! persisted identity ([`MediaId::PodcastEpisode`]) plus a saved fallback
//! enclosure URL; this module decides between the cache's current
//! enclosure and that fallback, by identity alone — never by title, index
//! or date, since a feed can be reordered or partially retained between
//! queueing and playback.

use url::Url;

use crate::feed::cache::CacheStore;
use crate::feed::error::FeedError;
use crate::media::id::MediaId;
use crate::media::source::SourceLocation;
use crate::subscription::store::SubscriptionStore;

/// Shown when a queued episode falls back to its saved enclosure URL rather
/// than the cache's current one.
pub const SAVED_SOURCE_NOTICE: &str = "Using saved episode source";

/// The artwork URL for a queued podcast episode, from the cache alone: the
/// episode's own `itunes:image`, else the feed's. `None` for a non-podcast,
/// an unsubscribed feed, a missing or unreadable cache, or an episode the
/// cache no longer lists — every one of those is simply "no cover".
pub fn podcast_artwork(
    subs: &SubscriptionStore,
    cache: &CacheStore,
    media: &MediaId,
) -> Option<Url> {
    let feed = media.feed()?;
    let snapshot = subs.read_snapshot().ok()?;
    let subscription = snapshot
        .subscriptions
        .iter()
        .find(|subscription| &subscription.feed_id == feed)?;
    let cached = cache.read(subscription).ok()?;
    let episode = cached
        .episodes
        .iter()
        .find(|episode| &episode.media_id == media)?;
    episode.image.clone().or(cached.image)
}

/// The podcast an episode belongs to: the subscription's title, else the
/// cached feed's. `None` for a non-podcast or when neither is readable.
pub fn podcast_name(
    subs: &SubscriptionStore,
    cache: &CacheStore,
    media: &MediaId,
) -> Option<String> {
    let feed = media.feed()?;
    let snapshot = subs.read_snapshot().ok()?;
    let subscription = snapshot
        .subscriptions
        .iter()
        .find(|subscription| &subscription.feed_id == feed)?;
    subscription
        .title
        .clone()
        .or_else(|| cache.read(subscription).ok()?.title)
}

/// The outcome of [`resolve_podcast`].
#[derive(Clone, Debug, PartialEq)]
pub enum PodcastResolution {
    /// The cache still lists this exact episode with an enclosure.
    Current {
        location: SourceLocation,
        /// `Some` only when the cache's enclosure differs from the saved
        /// fallback passed in — the caller's cue to persist a refreshed
        /// queue entry.
        refreshed_fallback: Option<Url>,
    },
    /// Subscription, cache file or episode absent: play the saved URL.
    Saved { location: SourceLocation },
}

#[derive(Debug, thiserror::Error)]
pub enum PodcastResolveError {
    #[error("not a podcast episode")]
    NotPodcast,
    #[error("this episode no longer has an audio enclosure")]
    NotPlayable,
    #[error(transparent)]
    Library(#[from] FeedError),
}

/// Resolves a queued podcast episode's playable source using only
/// `subscriptions.json` and the feed cache. `fallback` is the enclosure URL
/// saved on the queue entry itself, used whenever the cache can no longer
/// speak to this exact episode: an absent subscription, an absent or
/// corrupt cache file (a genuinely missing cache aside — see below), or an
/// episode the cache no longer lists.
///
/// A missing cache file ([`FeedError::CacheMissing`]) is absence, and
/// resolves to [`PodcastResolution::Saved`]; every other [`FeedError`] from
/// reading the subscriptions or the cache (corrupt, parser-mismatched,
/// unreadable) is a resolution error, not absence, and is returned as
/// [`PodcastResolveError::Library`].
pub fn resolve_podcast(
    subs: &SubscriptionStore,
    cache: &CacheStore,
    media: &MediaId,
    fallback: &Url,
) -> Result<PodcastResolution, PodcastResolveError> {
    let feed = media.feed().ok_or(PodcastResolveError::NotPodcast)?;
    let saved = || PodcastResolution::Saved {
        location: SourceLocation::Http(fallback.clone()),
    };

    let snapshot = subs.read_snapshot()?;
    let Some(subscription) = snapshot
        .subscriptions
        .iter()
        .find(|subscription| &subscription.feed_id == feed)
    else {
        return Ok(saved());
    };

    let cached = match cache.read(subscription) {
        Ok(cached) => cached,
        Err(FeedError::CacheMissing { .. }) => return Ok(saved()),
        Err(error) => return Err(PodcastResolveError::Library(error)),
    };

    let Some(cached_episode) = cached
        .episodes
        .iter()
        .find(|episode| &episode.media_id == media)
    else {
        return Ok(saved());
    };

    match &cached_episode.enclosure_url {
        Some(url) => Ok(PodcastResolution::Current {
            location: SourceLocation::Http(url.clone()),
            refreshed_fallback: if url == fallback {
                None
            } else {
                Some(url.clone())
            },
        }),
        None => Err(PodcastResolveError::NotPlayable),
    }
}
