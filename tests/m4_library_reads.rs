//! `src/library.rs`'s three read-only functions (design doc §6.6): the seam
//! a future TUI (M5) reuses, returning values and touching neither
//! `subscriptions.json` nor the feed cache nor `state.json`.
//!
//! `Rig` (`tests/support/feeds.rs`) is the fixture every M4 test suite
//! imports: a temp root, a fake clock, and the subscription/cache/state
//! stores that share it.

#[path = "support/feeds.rs"]
mod feeds;

use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use serde_json::json;
use tenuto::feed::error::FeedError;
use tenuto::library::{Progress, list_episodes, list_feeds, resolve_episode};
use tenuto::media::source::SourceLocation;
use tenuto::persistence::model::PersistedState;

/// Six items, deliberately out of chronological order and with two missing
/// `pubDate`s, so that stored (document) order is the only order a
/// well-behaved listing can produce (§1.2). Every item but `ep-f` carries a
/// usable enclosure; `ep-f` has identity (a GUID) but no enclosure at all,
/// which is what exercises the `AUDIO` column (§6.2) and `NotPlayable`
/// (§6.4).
const SIX_ITEMS: &[u8] = br#"<rss><channel>
    <item><guid>ep-a</guid><title>A</title>
        <pubDate>Wed, 01 Jan 2020 00:00:00 GMT</pubDate>
        <enclosure url="https://cdn.example.org/a.mp3"/></item>
    <item><guid>ep-b</guid><title>B</title>
        <pubDate>Tue, 01 Jan 2019 00:00:00 GMT</pubDate>
        <enclosure url="https://cdn.example.org/b.mp3"/></item>
    <item><guid>ep-c</guid><title>C</title>
        <enclosure url="https://cdn.example.org/c.mp3"/></item>
    <item><guid>ep-d</guid><title>D</title>
        <pubDate>Sat, 01 Jan 2022 00:00:00 GMT</pubDate>
        <enclosure url="https://cdn.example.org/d.mp3"/></item>
    <item><guid>ep-e</guid><title>E</title>
        <pubDate>Fri, 01 Jan 2021 00:00:00 GMT</pubDate>
        <enclosure url="https://cdn.example.org/e.mp3"/></item>
    <item><guid>ep-f</guid><title>F</title></item>
</channel></rss>"#;

/// Step 1: the estimate-wins/no-audio test, as the brief specifies it
/// verbatim. The `(id)` below is the computed `serde_json::json!` object
/// key — the canonical `MediaId` string — never the literal key `"id"`.
#[test]
fn latest_estimate_and_missing_audio_are_independent() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(
        br#"<rss><channel><item><guid>id</guid><title>Outtakes</title></item></channel></rss>"#,
        "https://example.org/feed",
    )?;
    let cached = rig.cache.read(&sub)?;
    let id = cached.episodes[0].media_id.to_string();
    let json = serde_json::json!({"schema_version": 4, "checkpoints": {
        (id): {"position": {"secs": 100, "nanos": 0}, "estimated": {"secs": 1082, "nanos": 0},
             "completed": false, "touch_seq": 1, "updated_at": "2026-09-11T00:00:00Z"}
    }});
    let state: PersistedState = serde_json::from_value(json)?;
    rig.state.write(&state)?;
    let rows = list_episodes(
        &rig.subs,
        &rig.cache,
        &rig.state.read_snapshot()?,
        "radio-t",
        None,
    )?;
    assert_eq!(
        rows[0].progress,
        Progress::Estimated(std::time::Duration::from_secs(1082))
    );
    assert!(!rows[0].playable);
    assert_eq!(rows[0].index, 1);
    Ok(())
}

/// Every branch of §6.2's precedence table, plus stored (document) order:
/// six checkpoint shapes, one per item, none of which participate in a sort
/// by publication date. "Entry with neither" (`ep-e`) is distinct from "no
/// entry at all" (`ep-f`) and must not collapse into it.
#[test]
fn progress_precedence_and_stored_order_are_preserved() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(SIX_ITEMS, "https://example.org/feed")?;
    let cached = rig.cache.read(&sub)?;
    assert_eq!(cached.episodes.len(), 6, "all six items retain identity");

    let id_of = |guid: &str| -> Result<String, Box<dyn std::error::Error>> {
        cached
            .episodes
            .iter()
            .find(|episode| episode.media_id.to_string().contains(guid))
            .map(|episode| episode.media_id.to_string())
            .ok_or_else(|| format!("no cached episode for {guid}").into())
    };

    let json = json!({"schema_version": 4, "checkpoints": {
        (id_of("ep-a")?): {"position": {"secs": 100, "nanos": 0}, "estimated": {"secs": 200, "nanos": 0},
             "completed": true, "touch_seq": 1, "updated_at": "2026-09-11T00:00:00Z"},
        (id_of("ep-b")?): {"position": {"secs": 50, "nanos": 0}, "estimated": {"secs": 222, "nanos": 0},
             "completed": false, "touch_seq": 2, "updated_at": "2026-09-11T00:00:00Z"},
        (id_of("ep-c")?): {"position": {"secs": 75, "nanos": 0}, "completed": false,
             "touch_seq": 3, "updated_at": "2026-09-11T00:00:00Z"},
        (id_of("ep-d")?): {"estimated": {"secs": 300, "nanos": 0}, "completed": false,
             "touch_seq": 4, "updated_at": "2026-09-11T00:00:00Z"},
        (id_of("ep-e")?): {"completed": false, "touch_seq": 5, "updated_at": "2026-09-11T00:00:00Z"}
    }});
    let state: PersistedState = serde_json::from_value(json)?;
    rig.state.write(&state)?;

    let rows = list_episodes(
        &rig.subs,
        &rig.cache,
        &rig.state.read_snapshot()?,
        "radio-t",
        None,
    )?;
    assert_eq!(rows.len(), 6);

    // Stored order, contiguous 1-based indices, no re-sort by pubDate.
    let titles: Vec<&str> = rows
        .iter()
        .map(|row| row.title.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(titles, vec!["A", "B", "C", "D", "E", "F"]);
    assert_eq!(
        rows.iter().map(|row| row.index).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );

    assert_eq!(
        rows[0].progress,
        Progress::Played,
        "completed wins over an estimate"
    );
    assert_eq!(
        rows[1].progress,
        Progress::Estimated(Duration::from_secs(222)),
        "an estimate wins over an established position"
    );
    assert_eq!(
        rows[2].progress,
        Progress::Established(Duration::from_secs(75)),
        "no estimate falls back to the established position"
    );
    assert_eq!(
        rows[3].progress,
        Progress::Estimated(Duration::from_secs(300)),
        "an estimate-only entry is still an estimate"
    );
    assert_eq!(
        rows[4].progress,
        Progress::Unknown,
        "an entry with neither field is 'position unknown', not 'no entry'"
    );
    assert_eq!(
        rows[5].progress,
        Progress::None,
        "no checkpoint entry at all is a bare dash"
    );

    // Playability is a separate column: ep-f has identity but no enclosure.
    assert_eq!(
        rows.iter().map(|row| row.playable).collect::<Vec<_>>(),
        vec![true, true, true, true, true, false]
    );
    Ok(())
}

/// `-n 2` truncates without renumbering, and `resolve_episode`'s 1-based
/// selection lands on the same identity the listing showed at that index
/// (§1.2, §6.3).
#[test]
fn limit_truncates_and_selection_matches_the_listed_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    rig.seed(SIX_ITEMS, "https://example.org/feed")?;

    let limit = NonZeroUsize::new(2).ok_or("2 must be nonzero")?;
    let rows = list_episodes(
        &rig.subs,
        &rig.cache,
        &rig.state.read_snapshot()?,
        "radio-t",
        Some(limit),
    )?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].index, 1);
    assert_eq!(rows[1].index, 2);

    let full = list_episodes(
        &rig.subs,
        &rig.cache,
        &rig.state.read_snapshot()?,
        "radio-t",
        None,
    )?;
    let (media, source) = resolve_episode(&rig.subs, &rig.cache, "radio-t", 2)?;
    assert_eq!(media, full[1].media);
    assert_eq!(
        source,
        SourceLocation::Http("https://cdn.example.org/b.mp3".parse()?)
    );
    Ok(())
}

/// An index whose retained episode has no usable enclosure fails typed and
/// early — before any playback resource is built (§6.4). Zero and
/// past-the-end indices are `IndexOutOfRange`, carrying the retained count.
#[test]
fn unplayable_and_out_of_range_indices_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    rig.seed(SIX_ITEMS, "https://example.org/feed")?;

    match resolve_episode(&rig.subs, &rig.cache, "radio-t", 6) {
        Err(FeedError::NotPlayable { slug, index, title }) => {
            assert_eq!(slug, "radio-t");
            assert_eq!(index, 6);
            assert_eq!(title, "F");
        }
        other => return Err(format!("expected NotPlayable, got {other:?}").into()),
    }

    match resolve_episode(&rig.subs, &rig.cache, "radio-t", 0) {
        Err(FeedError::IndexOutOfRange {
            slug,
            index,
            retained,
        }) => {
            assert_eq!(slug, "radio-t");
            assert_eq!(index, 0);
            assert_eq!(retained, 6);
        }
        other => return Err(format!("expected IndexOutOfRange, got {other:?}").into()),
    }

    match resolve_episode(&rig.subs, &rig.cache, "radio-t", 7) {
        Err(FeedError::IndexOutOfRange {
            slug,
            index,
            retained,
        }) => {
            assert_eq!(slug, "radio-t");
            assert_eq!(index, 7);
            assert_eq!(retained, 6);
        }
        other => return Err(format!("expected IndexOutOfRange, got {other:?}").into()),
    }
    Ok(())
}

/// A missing `subscriptions.json` means zero subscriptions, so a slug that
/// cannot exist is `UnknownSlug`, never a storage fault (§6.3) — for both
/// `list_episodes` and `resolve_episode`.
#[test]
fn a_missing_subscriptions_file_yields_unknown_slug_not_a_storage_fault()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;

    match list_episodes(
        &rig.subs,
        &rig.cache,
        &rig.state.read_snapshot()?,
        "nope",
        None,
    ) {
        Err(FeedError::UnknownSlug { slug }) => assert_eq!(slug, "nope"),
        other => return Err(format!("expected UnknownSlug, got {other:?}").into()),
    }
    match resolve_episode(&rig.subs, &rig.cache, "nope", 1) {
        Err(FeedError::UnknownSlug { slug }) => assert_eq!(slug, "nope"),
        other => return Err(format!("expected UnknownSlug, got {other:?}").into()),
    }
    assert_eq!(list_feeds(&rig.subs, &rig.cache)?.len(), 0);
    Ok(())
}

/// A subscribed feed with no cache entry yet is `CacheMissing` for episode
/// operations, but a zero-success *row* — not an error — for `feeds`, since
/// `feeds` must stay usable the moment a feed is subscribed and before its
/// first refresh (§6.4).
#[test]
fn missing_cache_is_an_error_for_episodes_but_a_row_for_feeds()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(SIX_ITEMS, "https://example.org/feed")?;
    rig.cache.remove(&sub.feed_id)?;

    match list_episodes(
        &rig.subs,
        &rig.cache,
        &rig.state.read_snapshot()?,
        "radio-t",
        None,
    ) {
        Err(FeedError::CacheMissing { slug }) => assert_eq!(slug, "radio-t"),
        other => return Err(format!("expected CacheMissing, got {other:?}").into()),
    }
    match resolve_episode(&rig.subs, &rig.cache, "radio-t", 1) {
        Err(FeedError::CacheMissing { slug }) => assert_eq!(slug, "radio-t"),
        other => return Err(format!("expected CacheMissing, got {other:?}").into()),
    }

    let summaries = list_feeds(&rig.subs, &rig.cache)?;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].slug, "radio-t");
    assert_eq!(summaries[0].episodes, None);
    assert_eq!(summaries[0].last_refreshed_at, None);
    // The durable subscription title is used, not anything from the cache.
    assert_eq!(summaries[0].title, sub.title);
    Ok(())
}

/// `list_feeds` propagates a corrupt cache rather than swallowing it into a
/// missing-cache row: corruption and "never refreshed" must stay visibly
/// distinct outcomes.
#[test]
fn list_feeds_propagates_a_corrupt_cache_rather_than_a_missing_row()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(SIX_ITEMS, "https://example.org/feed")?;
    let cache_path = rig.cache.path_for(&sub.feed_id)?;
    std::fs::write(&cache_path, b"not json")?;

    match list_feeds(&rig.subs, &rig.cache) {
        Err(FeedError::CacheCorrupt { slug, .. }) => assert_eq!(slug, "radio-t"),
        other => return Err(format!("expected CacheCorrupt, got {other:?}").into()),
    }
    Ok(())
}

/// Neither read-only function ever mutates the temp tree: every file's
/// bytes and modified time (never access time — no assertion here touches
/// it) are identical before and after both a successful and a failing call.
#[test]
fn neither_listing_mutates_any_file_on_success_or_failure() -> Result<(), Box<dyn std::error::Error>>
{
    let rig = feeds::Rig::new()?;
    rig.seed(SIX_ITEMS, "https://example.org/feed")?;

    let before = snapshot_tree(rig.root.path())?;

    let state = rig.state.read_snapshot()?;
    let _ = list_feeds(&rig.subs, &rig.cache)?;
    let _ = list_episodes(&rig.subs, &rig.cache, &state, "radio-t", None)?;
    let _ = resolve_episode(&rig.subs, &rig.cache, "radio-t", 1)?;
    // Failing calls too: unknown slug, out-of-range index, unplayable index.
    let _ = list_episodes(&rig.subs, &rig.cache, &state, "no-such-slug", None);
    let _ = resolve_episode(&rig.subs, &rig.cache, "radio-t", 0);
    let _ = resolve_episode(&rig.subs, &rig.cache, "radio-t", 6);

    let after = snapshot_tree(rig.root.path())?;
    assert_eq!(
        before, after,
        "a read-only call must leave every file untouched"
    );
    Ok(())
}

/// One file's `(path relative to the snapshot root, bytes, modified time)`.
type FileSnapshot = (String, Vec<u8>, std::time::SystemTime);

/// Recursively snapshots every regular file under `root` as a
/// [`FileSnapshot`], sorted by relative path. Deliberately excludes access
/// time: this build never asserts on it, since merely opening a file for
/// `assert_eq!`'s own later reads would otherwise make such an assertion
/// self-defeating.
fn snapshot_tree(root: &Path) -> Result<Vec<FileSnapshot>, Box<dyn std::error::Error>> {
    fn walk(
        dir: &Path,
        root: &Path,
        out: &mut Vec<FileSnapshot>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out)?;
            } else {
                let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
                let bytes = std::fs::read(&path)?;
                let modified = std::fs::metadata(&path)?.modified()?;
                out.push((relative, bytes, modified));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
