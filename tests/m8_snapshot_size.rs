//! M8 §12: what a full 4,096-entry state costs. Ignored by default — it
//! measures, it does not assert. Run it by hand and copy the output into
//! docs/m8-acceptance.md:
//!
//!   cargo test --release --test m8_snapshot_size -- --ignored --nocapture
//!
//! Spec §12 asks for "representative paths, URLs and tags": the fixture
//! below is a mix of all three `MediaId` kinds (mostly local files, a
//! minority of plain URLs, a minority of podcast episodes), and the
//! checkpoint map draws from all three too. The atomic write is measured
//! twice: once on `/tmp` (typically `tmpfs`, kept only as a labelled
//! comparison) and once under this crate's own `target/` directory, which
//! sits on whatever real filesystem the crate itself is built on — the same
//! kind of disk `$XDG_STATE_HOME/state.json` lives on, unlike `/tmp`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenuto::clock::FakeClock;
use tenuto::media::id::{AbsolutePath, EpisodeKey, FeedId, MediaId, NormalizedUrl};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::playback::provenance::PositionProvenance;
use tenuto::queue::{
    DisplayDuration, DisplayMetadata, DurationSource, MAX_PLAYLIST_ENTRIES, NewQueueEntry,
    QueueSource,
};
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
/// (the checkpoint sampling below) does not land on the same residue every
/// time and so silently skip a kind.
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

/// The filesystem a path lives on, read from `/proc/mounts` by longest
/// matching mount-point prefix. Best-effort and Linux-only: this is a
/// measurement aid, not production code, so "unknown" is an acceptable
/// answer rather than a reason to add a crate for it.
fn fs_type_of(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let canonical = canonical.to_string_lossy();
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return "unknown".into();
    };
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let Some(_device) = fields.next() else {
            continue;
        };
        let Some(mount_point) = fields.next() else {
            continue;
        };
        let Some(fstype) = fields.next() else {
            continue;
        };
        if canonical.starts_with(mount_point) {
            let len = mount_point.len();
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, fstype.to_string()));
            }
        }
    }
    best.map_or_else(|| "unknown".into(), |(_, fstype)| fstype)
}

#[test]
#[ignore = "a measurement, run by hand; see the module comment"]
fn a_full_snapshot_costs_this_much() {
    let mut session = Session::new(PersistedState::default());
    let mut playlists = vec![session.state().playing()];
    for i in 1..8 {
        playlists.push(
            session
                .create_playlist(&format!("Playlist {i}"))
                .expect("room")
                .0,
        );
    }
    let mut medias = Vec::new();
    let (mut local_count, mut url_count, mut podcast_count) = (0usize, 0usize, 0usize);
    for i in 0..MAX_PLAYLIST_ENTRIES {
        match kind_of(i) {
            Kind::Local => local_count += 1,
            Kind::Url => url_count += 1,
            Kind::Podcast => podcast_count += 1,
        }
    }
    for (index, chunk) in (0..MAX_PLAYLIST_ENTRIES)
        .collect::<Vec<_>>()
        .chunks(MAX_PLAYLIST_ENTRIES / 8)
        .enumerate()
    {
        let (ms, entries): (Vec<_>, Vec<_>) = chunk.iter().map(|i| representative(*i)).unzip();
        medias.extend(ms);
        session.enqueue(playlists[index], entries).expect("fits");
    }

    // A realistic checkpoint map, drawn from all three media kinds: every
    // eighth track has been played, with a varied position, timestamp and
    // completion flag rather than one constant. Every fourth (the design
    // conversation's original figure) would be 1,024 checkpoints, but
    // `persistence::model::MAX_ENTRIES` (the checkpoint cap, currently 512)
    // evicts before that many fit, so this steps by 8 to land exactly on
    // what the cap allows without triggering an eviction.
    let mut state = session.state().clone();
    let (mut checkpoint_local, mut checkpoint_url, mut checkpoint_podcast) =
        (0usize, 0usize, 0usize);
    for (n, media) in medias.iter().step_by(8).enumerate() {
        let position = Duration::from_secs(15 + ((n * 37) % 5400) as u64);
        let updated_at =
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(n as i64 * 3600 + 1);
        let completed = n % 3 == 0;
        state.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position,
                updated_at,
            },
            completed,
        );
        match media {
            MediaId::LocalFile(_) => checkpoint_local += 1,
            MediaId::RemoteUrl(_) => checkpoint_url += 1,
            MediaId::PodcastEpisode { .. } => checkpoint_podcast += 1,
        }
    }
    assert_eq!(state.total_entries(), MAX_PLAYLIST_ENTRIES);

    let started = Instant::now();
    let cloned = state.clone();
    let clone_time = started.elapsed();

    let started = Instant::now();
    let bytes = serde_json::to_vec_pretty(&cloned).expect("serializes");
    let serialize_time = started.elapsed();

    // Comparison only: `/tmp` is typically `tmpfs`, which never touches a
    // physical block device, so its `fsync` is close to free.
    let tmp_dir = tempfile::tempdir().expect("tmp tempdir");
    let tmp_store = StateStore::new(
        tmp_dir.path().join("state.json"),
        Arc::new(FakeClock::new()),
    );
    let started = Instant::now();
    tmp_store.write(&cloned).expect("writes to tmpfs");
    let tmpfs_write_time = started.elapsed();
    let tmpfs_fstype = fs_type_of(tmp_dir.path());

    // The headline number: the real filesystem this crate itself sits on,
    // the same kind of disk `$XDG_STATE_HOME/state.json` lives on.
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&target_dir).expect("target dir exists");
    let disk_dir = tempfile::tempdir_in(&target_dir).expect("on-disk tempdir");
    let disk_store = StateStore::new(
        disk_dir.path().join("state.json"),
        Arc::new(FakeClock::new()),
    );
    let started = Instant::now();
    disk_store.write(&cloned).expect("writes on disk");
    let disk_write_time = started.elapsed();
    let disk_fstype = fs_type_of(disk_dir.path());

    println!("entries                        {}", state.total_entries());
    println!(
        "entries by kind                local={local_count} url={url_count} podcast={podcast_count}"
    );
    println!("checkpoints                    {}", state.len());
    println!(
        "checkpoints by kind            local={checkpoint_local} url={checkpoint_url} podcast={checkpoint_podcast}"
    );
    println!("bytes on disk                  {}", bytes.len());
    println!("clone (app thread)             {clone_time:?}");
    println!("serialize                      {serialize_time:?}");
    println!(
        "atomic write, tmpfs (/tmp)     {tmpfs_write_time:?}  fstype={tmpfs_fstype}  (comparison only)"
    );
    println!(
        "atomic write, on-disk (target) {disk_write_time:?}  fstype={disk_fstype}  path={}",
        disk_dir.path().display()
    );
}
