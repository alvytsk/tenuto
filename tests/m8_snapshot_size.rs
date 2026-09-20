//! M8 §12: what a full 4,096-entry state costs. Ignored by default — it
//! measures, it does not assert. Run it by hand and copy the output into
//! docs/m8-acceptance.md:
//!
//!   cargo test --release --test m8_snapshot_size -- --ignored --nocapture

use std::sync::Arc;
use std::time::Instant;

use tenuto::clock::FakeClock;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::queue::{DisplayMetadata, MAX_PLAYLIST_ENTRIES, NewQueueEntry, QueueSource};
use tenuto::session::Session;

/// A path, tags and a duration of the length a real library has.
fn representative(i: usize) -> (MediaId, NewQueueEntry) {
    let path = AbsolutePath::new(
        format!("/home/listener/Music/Various Artists/Some Fairly Long Album Title (Deluxe Edition) [2019]/{:02} - A Track Title of Ordinary Length {i}.flac", i % 20 + 1).into(),
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
            duration: None,
        },
    )
    .unwrap_or_else(|error| panic!("valid: {error}"));
    (media, entry)
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
    for (index, chunk) in (0..MAX_PLAYLIST_ENTRIES)
        .collect::<Vec<_>>()
        .chunks(MAX_PLAYLIST_ENTRIES / 8)
        .enumerate()
    {
        let (ms, entries): (Vec<_>, Vec<_>) = chunk.iter().map(|i| representative(*i)).unzip();
        medias.extend(ms);
        session.enqueue(playlists[index], entries).expect("fits");
    }
    // A realistic checkpoint map: every eighth track has been played. Every
    // fourth (the design conversation's original figure) would be 1,024
    // checkpoints, but `persistence::model::MAX_ENTRIES` (the checkpoint
    // cap, currently 512) evicts before that many fit, so this steps by 8 to
    // land exactly on what the cap allows without triggering an eviction.
    let mut state = session.state().clone();
    for media in medias.iter().step_by(8) {
        state.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: std::time::Duration::from_secs(95),
                updated_at: time::OffsetDateTime::UNIX_EPOCH,
            },
            true,
        );
    }
    assert_eq!(state.total_entries(), MAX_PLAYLIST_ENTRIES);

    let started = Instant::now();
    let cloned = state.clone();
    let clone_time = started.elapsed();

    let started = Instant::now();
    let bytes = serde_json::to_vec_pretty(&cloned).expect("serializes");
    let serialize_time = started.elapsed();

    let dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::new(dir.path().join("state.json"), Arc::new(FakeClock::new()));
    let started = Instant::now();
    store.write(&cloned).expect("writes");
    let write_time = started.elapsed();

    println!("entries            {}", state.total_entries());
    println!("checkpoints        {}", state.len());
    println!("bytes on disk      {}", bytes.len());
    println!("clone (app thread) {clone_time:?}");
    println!("serialize          {serialize_time:?}");
    println!(
        "atomic write       {write_time:?}  (serialize + write-temp-file + fsync + rename + best-effort parent fsync, as StateStore::write does it)"
    );
}
