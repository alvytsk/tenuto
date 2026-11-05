//! M12: what snapshot sharing bought. Ignored by default — they measure,
//! they do not assert. Run them by hand and copy the output into
//! docs/m12-acceptance.md:
//!
//!   cargo test --release --test m12_snapshot_size -- --ignored --nocapture
//!
//! The fixture mirrors `tests/m8_snapshot_size.rs`'s shape — the same mix
//! of `MediaId` kinds, 8 playlists, 4,096 entries, 512 checkpoints — but is
//! self-contained by design, so the m8 file and its record stay historical.
//! `a_full_snapshot_clone_costs_this_much` re-times what m8's record
//! measured at 2.19–2.29 ms; `a_checkpoint_touch_costs_this_much` times the
//! cost m12 moved onto the 5-second capture: a checkpoint `record` beside a
//! retained snapshot, and without one.

use std::time::{Duration, Instant};

use tenuto::media::id::{AbsolutePath, EpisodeKey, FeedId, MediaId, NormalizedUrl};
use tenuto::media::provenance::PositionProvenance;
use tenuto::persistence::model::PersistedState;
use tenuto::queue::{
    DisplayDuration, DisplayMetadata, DurationSource, MAX_PLAYLIST_ENTRIES, NewQueueEntry,
    QueueSource,
};
use tenuto::resume::PlaybackCheckpoint;
use tenuto::session::Session;
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Local,
    Url,
    Podcast,
}

/// Roughly 80% local files, 10% plain URLs, 10% podcast episodes — a
/// defensible music-player-plus-podcasts mix. Chosen so that `step_by(8)`
/// (the checkpoint sampling in `build_state`) does not land on the same
/// residue every time and so silently skip a kind.
fn kind_of(i: usize) -> Kind {
    match i % 10 {
        0 => Kind::Url,
        8 => Kind::Podcast,
        _ => Kind::Local,
    }
}

/// Most entries carry a duration, split between the two `DurationSource`
/// variants; a few (every 13th) have none, as an unread or un-probed file
/// would.
fn duration_for(i: usize) -> Option<DisplayDuration> {
    if i.is_multiple_of(13) {
        return None;
    }
    let value = Duration::from_secs(120 + (i % 5400) as u64);
    let source = if i.is_multiple_of(2) {
        DurationSource::Declared
    } else {
        DurationSource::Decoded(PositionProvenance::Established)
    };
    Some(DisplayDuration { value, source })
}

/// A path, tags and a duration of the length a real library has.
fn local_entry(i: usize) -> (MediaId, NewQueueEntry) {
    let path = AbsolutePath::new(
        format!(
            "/home/listener/Music/Various Artists/Some Fairly Long Album Title (Deluxe Edition) [2019]/{:02} - A Track Title of Ordinary Length {i}.flac",
            i % 20 + 1
        )
        .into(),
    )
    .unwrap_or_else(|error| panic!("absolute: {error}"));
    let media = MediaId::LocalFile(path.clone());
    let entry = NewQueueEntry::new(
        media.clone(),
        QueueSource::LocalFile(path),
        DisplayMetadata {
            title: Some(format!("A Track Title of Ordinary Length {i}")),
            artist: Some("An Artist With a Reasonable Name".into()),
            album: Some("Some Fairly Long Album Title (Deluxe Edition)".into()),
            year: Some("2019".into()),
            duration: duration_for(i),
        },
    )
    .unwrap_or_else(|error| panic!("valid: {error}"));
    (media, entry)
}

/// A realistic CDN-style URL: a path and a query string, not just a bare
/// host.
fn url_entry(i: usize) -> (MediaId, NewQueueEntry) {
    let raw = format!("https://cdn.example.net/streams/live-cut-{i}.mp3?token=abc123&cache={i}");
    let url = NormalizedUrl::parse(&raw).unwrap_or_else(|error| panic!("normalized url: {error}"));
    let media = MediaId::RemoteUrl(url.clone());
    let entry = NewQueueEntry::new(
        media.clone(),
        QueueSource::RemoteUrl(url),
        DisplayMetadata {
            title: Some(format!("Live Cut {i}")),
            artist: Some("An Independent Broadcaster".into()),
            album: None,
            year: None,
            duration: duration_for(i),
        },
    )
    .unwrap_or_else(|error| panic!("valid: {error}"));
    (media, entry)
}

/// A podcast episode as the runtime actually builds one: a `MediaId` keyed
/// by feed and episode, a `QueueSource::Podcast` fallback enclosure URL.
fn podcast_entry(i: usize) -> (MediaId, NewQueueEntry) {
    let feed = FeedId::new(format!("{i:032x}")).unwrap_or_else(|error| panic!("feed id: {error}"));
    let episode = EpisodeKey::resolve(Some(&format!("guid-{i}")), None, None)
        .unwrap_or_else(|error| panic!("episode key: {error}"));
    let media = MediaId::PodcastEpisode { feed, episode };
    let fallback: Url = format!("https://podcasts.example.org/audio/episode-{i}.mp3")
        .parse()
        .unwrap_or_else(|error| panic!("fallback url: {error}"));
    let entry = NewQueueEntry::new(
        media.clone(),
        QueueSource::Podcast { fallback },
        DisplayMetadata {
            title: Some(format!("Episode {i}: A Fairly Long Podcast Episode Title")),
            artist: Some("A Podcast Network".into()),
            album: Some("A Long-Running Interview Show".into()),
            year: Some("2024".into()),
            duration: duration_for(i),
        },
    )
    .unwrap_or_else(|error| panic!("valid: {error}"));
    (media, entry)
}

fn representative(i: usize) -> (MediaId, NewQueueEntry) {
    match kind_of(i) {
        Kind::Local => local_entry(i),
        Kind::Url => url_entry(i),
        Kind::Podcast => podcast_entry(i),
    }
}

/// A checkpoint with varied position and timestamp rather than one constant;
/// `n` only keeps the values distinct between calls.
fn checkpoint_for(media: &MediaId, n: usize) -> PlaybackCheckpoint {
    PlaybackCheckpoint {
        media: media.clone(),
        position: Duration::from_secs(15 + ((n * 37) % 5400) as u64),
        updated_at: time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(n as i64 * 3600 + 1),
    }
}

/// The m8-shaped state, built through `Session`'s public surface: 8
/// playlists, 4,096 mixed entries, then a checkpoint map filled by sampling
/// every eighth media. That step lands on exactly 512 —
/// `persistence::model::MAX_ENTRIES`, the checkpoint cap — so the map sits
/// at the cap without an eviction ever firing, and the eviction case below
/// is a measurement call of its own. Returns the state and every media in
/// enqueue order, so the callers can pick map members (indices ≡ 0 mod 8)
/// and non-members for a fresh identity.
fn build_state() -> (PersistedState, Vec<MediaId>) {
    let mut session = Session::new(PersistedState::default());
    let mut playlists = vec![session.state().playlists().playing()];
    for i in 1..8 {
        playlists.push(
            session
                .create_playlist(&format!("Playlist {i}"))
                .unwrap_or_else(|error| panic!("room: {error}"))
                .0,
        );
    }
    let mut medias = Vec::new();
    for (index, chunk) in (0..MAX_PLAYLIST_ENTRIES)
        .collect::<Vec<_>>()
        .chunks(MAX_PLAYLIST_ENTRIES / 8)
        .enumerate()
    {
        let (ms, entries): (Vec<_>, Vec<_>) = chunk.iter().map(|i| representative(*i)).unzip();
        medias.extend(ms);
        session
            .enqueue(playlists[index], entries)
            .unwrap_or_else(|error| panic!("fits: {error}"));
    }

    let mut state = session.state().clone();
    for (n, media) in medias.iter().step_by(8).enumerate() {
        state.record(&checkpoint_for(media, n), n % 3 == 0);
    }
    (state, medias)
}

/// The headline number: what cloning the whole state costs the application
/// thread when it hands the writer a snapshot. m8's record measured
/// 2.19–2.29 ms for the same fixture; with sharing it is expected to be
/// refcount updates only.
#[test]
#[ignore = "a measurement, run by hand; see the module comment"]
fn a_full_snapshot_clone_costs_this_much() {
    let (state, _medias) = build_state();

    let started = Instant::now();
    let cloned = state.clone();
    let clone_time = started.elapsed();

    let bytes = serde_json::to_vec_pretty(&cloned).expect("serializes");

    println!(
        "entries                        {}",
        state.playlists().total_entries()
    );
    println!("checkpoints                    {}", state.len());
    println!("bytes on disk                  {}", bytes.len());
    println!("clone (app thread)             {clone_time:?}");
}

/// The cost m12 moved onto the 5-second capture: one checkpoint `record`
/// each, under a retained snapshot and without one. While a snapshot is
/// retained, the first touch copies the checkpoint map on write, and later
/// touches mutate the split map in place; the fresh identity is the
/// capacity worst case — 512 held, evict + insert. Once the retained
/// snapshot is dropped, a touch mutates the unshared map in place, which is
/// the steady state the player actually lives in.
#[test]
#[ignore = "a measurement, run by hand; see the module comment"]
fn a_checkpoint_touch_costs_this_much() {
    let (mut state, medias) = build_state();

    let retained = state.clone();
    let started = Instant::now();
    state.record(&checkpoint_for(&medias[0], 10_001), false);
    let steady_state = started.elapsed();

    let started = Instant::now();
    state.record(&checkpoint_for(&medias[8], 10_002), false);
    let still_retained = started.elapsed();

    // `medias[1]` is enqueued but has no checkpoint: a fresh identity, so
    // the full map is at the cap and this pays evict + insert.
    let started = Instant::now();
    state.record(&checkpoint_for(&medias[1], 10_003), false);
    let evict_insert = started.elapsed();

    drop(retained);
    // `medias[24]` must be an entry the eviction above could not have
    // taken: eviction removes the lowest-`touch_seq` entry, and this one
    // was recorded after the touched pair and the victim. Replacing it is
    // then an update of an existing key on an unshared map — the in-place
    // case, no copy.
    let started = Instant::now();
    state.record(&checkpoint_for(&medias[24], 10_004), false);
    let in_place = started.elapsed();

    println!("touch, snapshot retained (steady state)  {steady_state:?}");
    println!("touch again, still retained              {still_retained:?}");
    println!("insert with eviction, retained           {evict_insert:?}");
    println!("touch, nothing retained (in place)       {in_place:?}");
}
