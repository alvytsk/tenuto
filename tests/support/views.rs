//! `PlayerView` builders shared by the terminal rendering and input suites:
//! three queue rows (a decoded 03:05 track with an artist, a declared one-hour
//! episode with an estimated saved position, and a played entry without a
//! duration) and a now-playing entry to go with them.

#![allow(dead_code)]

use std::time::Duration;

use tenuto::application::transport::PlaybackPhase;
use tenuto::application::view::{
    NowPlaying, PersistenceStatus, PlayerView, QueueRow, SavedHistory,
};
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::playback::state::PlaybackState;
use tenuto::playback::volume::Volume;
use tenuto::queue::{
    DisplayDuration, DurationSource, IdAllocator, NewQueueEntry, Queue, QueueEntryId, QueueSource,
};

/// The ids a fresh queue assigns to three entries; the same on every call.
pub fn ids() -> Vec<QueueEntryId> {
    let mut queue = Queue::default();
    let entries = ["a", "b", "c"]
        .iter()
        .map(|name| {
            let path = AbsolutePath::new(format!("/music/{name}.flac").into())
                .unwrap_or_else(|error| panic!("absolute path: {error}"));
            NewQueueEntry::new(
                MediaId::LocalFile(path.clone()),
                QueueSource::LocalFile(path),
                Default::default(),
            )
            .unwrap_or_else(|error| panic!("queue entry: {error}"))
        })
        .collect();
    queue
        .enqueue(entries, &mut IdAllocator::default())
        .unwrap_or_else(|error| panic!("three entries fit: {error}"))
}

/// The identity `ids()` gave entry `name`.
pub fn media(name: &str) -> MediaId {
    MediaId::LocalFile(
        AbsolutePath::new(format!("/music/{name}.flac").into())
            .unwrap_or_else(|error| panic!("absolute path: {error}")),
    )
}

pub fn decoded(seconds: u64) -> DisplayDuration {
    DisplayDuration {
        value: Duration::from_secs(seconds),
        source: DurationSource::Decoded(Default::default()),
    }
}

pub fn view(phase: PlaybackPhase, now: Option<NowPlaying>) -> PlayerView {
    let ids = ids();
    PlayerView {
        rows: vec![
            QueueRow {
                id: ids[0],
                media: media("a"),
                title: "Morning Tide".into(),
                subtitle: Some("Harbor".into()),
                duration: Some(decoded(185)),
                saved: None,
            },
            QueueRow {
                id: ids[1],
                media: media("b"),
                title: "Long Episode".into(),
                subtitle: None,
                duration: Some(DisplayDuration {
                    value: Duration::from_secs(3600),
                    source: DurationSource::Declared,
                }),
                saved: Some(SavedHistory::Position {
                    at: Duration::from_secs(62),
                    estimated: true,
                }),
            },
            QueueRow {
                id: ids[2],
                media: media("c"),
                title: "Done".into(),
                subtitle: None,
                duration: None,
                saved: Some(SavedHistory::Played),
            },
        ],
        active: now.as_ref().and_then(|now| now.entry),
        now_playing: now,
        phase,
        volume: Volume::new(0.8),
        status: None,
        persistence: PersistenceStatus::Saving,
        last_requested: None,
        live: false,
        reconnecting: false,
    }
}

/// The three-row queue with nothing active.
pub fn sample_view() -> PlayerView {
    view(PlaybackPhase::Unloaded, None)
}

pub fn playing(
    entry: QueueEntryId,
    loaded: bool,
    duration: Option<DisplayDuration>,
    estimated: bool,
) -> NowPlaying {
    NowPlaying {
        entry: Some(entry),
        title: "Morning Tide".into(),
        artist: Some("Harbor".into()),
        album: Some("Coast".into()),
        year: None,
        loaded,
        state: if loaded {
            PlaybackState::Playing
        } else {
            PlaybackState::Idle
        },
        position: Duration::from_secs(62),
        duration,
        estimated_position: estimated,
        degraded: false,
        buffering: false,
        seek: None,
        saved: Some(SavedHistory::Position {
            at: Duration::from_secs(62),
            estimated: false,
        }),
        session_rev: 1,
        load: None,
    }
}
