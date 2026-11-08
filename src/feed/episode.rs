//! Binding parsed feed items to subscription-scoped episode identities
//! (§2.4).
//!
//! [`bind_feed`] is the join `tests/m4_feed_parse.rs`'s pure parser
//! deliberately does not perform: it takes the `FeedId` only the
//! subscription layer holds, plus a [`ParseReport`], and produces real
//! [`crate::media::id::MediaId::PodcastEpisode`] identities. It calls
//! [`crate::media::id::EpisodeKey::resolve`] exactly as M0 wrote it —
//! unmodified — and is otherwise infallible: every failure mode it can
//! encounter is an item-level skip-and-warn, never an `Err`.

use std::collections::BTreeSet;

use url::Url;

use crate::media::Episode;
use crate::media::id::{EpisodeKey, FeedId, MediaId};
use crate::media::source::SourceLocation;

use super::parse::{ParseReport, ParseWarning, WarningKind};

/// One feed document after its items have been bound to a subscription's
/// identity.
///
/// `skipped` is [`ParseReport::skipped`] (parse-stage identity failures)
/// plus this stage's own rejections — an item with no identity at all, or
/// one whose resolved key duplicates an earlier item in the same feed —
/// each item counted exactly once. `warnings` likewise carries the parse
/// stage's warnings forward and appends this stage's own.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundFeed {
    pub title: Option<String>,
    pub site_link: Option<Url>,
    pub image: Option<Url>,
    pub items: Vec<BoundItem>,
    pub skipped: usize,
    pub warnings: Vec<ParseWarning>,
}

/// One bound episode, plus the enclosure metadata that is a claim rather
/// than evidence and so never reaches [`Episode`] itself (§2.1): a declared
/// length and MIME type, kept beside the episode for a later debug log or
/// warning rather than inside it.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundItem {
    pub episode: Episode,
    /// The item's own artwork URL, when the feed named one for it.
    pub image: Option<Url>,
    pub enclosure_length: Option<u64>,
    pub enclosure_mime: Option<String>,
}

/// Whether an enclosure URL is a usable playback source: the public-source
/// bar (`src/media`, one implementation for the whole crate) - http(s) with
/// a host and no embedded credentials (§4.5).
///
/// `parse_feed` already enforces this before an `Enclosure` ever reaches a
/// `ParsedItem`, but it is checked again here: a caller can build a
/// `ParsedItem` directly, without going through the parser, and `resolve`
/// trusts whatever URL it is handed as the enclosure without questioning
/// its scheme.
fn usable_enclosure_url(url: &Url) -> bool {
    crate::media::is_public_source_url(url)
}

/// Binds one parsed feed document to `feed_id`, producing real
/// `MediaId::PodcastEpisode` identities (§2.4).
///
/// Items are walked in the parser's document order and never re-sorted.
/// For each item: the GUID is used verbatim; the enclosure is used only if
/// it passes `usable_enclosure_url`, and `None` otherwise, so an
/// unsupported scheme can never poison resolution past the GUID; the link
/// is the last resort. An item that resolves to no identity at all is
/// skipped with [`WarningKind::MissingIdentity`]. An item whose resolved
/// key was already seen in this feed is skipped with
/// [`WarningKind::DuplicateIdentity`] — the first occurrence in document
/// order wins (§4.7). An item with identity but no usable enclosure is kept
/// with `source: None` (§2.4).
///
/// A warning this stage raises names the item by
/// [`crate::feed::model::ParsedItem::ordinal`] —
/// the same **pre-skip** document position the parser itself uses for its
/// own warnings (§7.2's "the ordinal plus the category is enough to find the
/// item" applies identically to both stages) — never by where the item
/// lands in `feed.items`, which counts only items that survived parsing.
pub fn bind_feed(feed_id: &FeedId, parsed: ParseReport) -> BoundFeed {
    let ParseReport {
        feed,
        mut skipped,
        mut warnings,
    } = parsed;

    let mut items = Vec::with_capacity(feed.items.len());
    let mut seen = BTreeSet::new();

    for item in feed.items {
        let ordinal = item.ordinal;
        let enclosure = item
            .enclosure
            .filter(|enclosure| usable_enclosure_url(&enclosure.url));

        let key = match EpisodeKey::resolve(
            item.guid.as_deref(),
            enclosure.as_ref().map(|e| &e.url),
            item.link.as_ref(),
        ) {
            Ok(key) => key,
            Err(_) => {
                skipped += 1;
                warnings.push(ParseWarning {
                    item: Some(ordinal),
                    kind: WarningKind::MissingIdentity,
                });
                continue;
            }
        };
        if !seen.insert(key.clone()) {
            skipped += 1;
            warnings.push(ParseWarning {
                item: Some(ordinal),
                kind: WarningKind::DuplicateIdentity,
            });
            continue;
        }
        let id = MediaId::PodcastEpisode {
            feed: feed_id.clone(),
            episode: key,
        };

        let source = enclosure
            .as_ref()
            .map(|enclosure| SourceLocation::Http(enclosure.url.clone()));
        let (enclosure_length, enclosure_mime) = match enclosure {
            Some(enclosure) => (enclosure.length, enclosure.mime_type),
            None => (None, None),
        };

        items.push(BoundItem {
            image: item.image,
            episode: Episode {
                id,
                source,
                title: item.title,
                published: item.published,
                declared_duration: item.declared_duration,
            },
            enclosure_length,
            enclosure_mime,
        });
    }

    BoundFeed {
        title: feed.title,
        site_link: feed.site_link,
        image: feed.image,
        items,
        skipped,
        warnings,
    }
}
