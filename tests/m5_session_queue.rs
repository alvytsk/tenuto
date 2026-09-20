mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use support::media;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::PersistenceError;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::persistence::writer::{StateSink, Urgency, WriterHandle};
use tenuto::playback::command::{LoadRequestId, ResumeIntent};
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::queue::{
    DisplayMetadata, MAX_PLAYLIST_ENTRIES, NewQueueEntry, QueueError, QueueSource,
};
use tenuto::resume::ResumeCandidate;
use tenuto::session::{Action, DisplayUpdate, LoadTarget, Session};

// entry(), loaded(), progress(), playing() helpers: copied from tests/m5_session_adoption.rs.

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else {
        unreachable!()
    };
    NewQueueEntry::new(
        media(name),
        QueueSource::LocalFile(path),
        DisplayMetadata::default(),
    )
    .unwrap_or_else(|error| panic!("a literal entry must be valid: {error}"))
}

fn loaded(request: LoadRequestId, rev: u64, name: &str) -> PlaybackEvent {
    PlaybackEvent::Loaded {
        session_rev: rev,
        request,
        media: media(name),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position: Duration::ZERO,
        disposition: StartDisposition::Fresh,
    }
}

fn progress(rev: u64, name: &str, secs: u64, load: Option<LoadRequestId>) -> Progress {
    Progress {
        session_rev: rev,
        media: Some(media(name)),
        position: Duration::from_secs(secs),
        quality: PositionQuality::Exact,
        provenance: PositionProvenance::Established,
        buffering: false,
        load,
    }
}

fn playing(rev: u64) -> PlaybackEvent {
    PlaybackEvent::StateChanged {
        session_rev: rev,
        state: PlaybackState::Playing,
        request: None,
    }
}

#[test]
fn an_accepted_enqueue_submits_the_queue_and_a_rejected_one_submits_nothing() {
    let mut session = Session::new(PersistedState::default());
    let (_, action) = session.enqueue(vec![entry("a")]).expect("fits");
    let Action::Submit { state, .. } = action else {
        panic!("must submit")
    };
    assert_eq!(state.queue().len(), 1);
    let too_many = (0..MAX_PLAYLIST_ENTRIES)
        .map(|i| entry(&format!("t{i}")))
        .collect();
    assert!(matches!(
        session.enqueue(too_many),
        Err(QueueError::Capacity { .. })
    ));
    assert_eq!(session.state().queue().len(), 1);
}

#[test]
fn removing_the_active_entry_captures_it_stops_and_selects_the_successor() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("b")]).expect("fits");
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());

    let removal = session
        .remove_entry(ids[0], &progress(1, "a", 42, Some(request)), clock.sample())
        .expect("known");
    assert!(removal.stop_playback);
    assert_eq!(removal.selection, Some(ids[1]));
    assert_eq!(session.state().queue().active(), None);
    assert_eq!(session.adopted(), None);
    assert_eq!(
        session
            .state()
            .entry_for(&media("a"))
            .and_then(|e| e.position),
        Some(Duration::from_secs(42))
    );
    assert_eq!(session.state().current_media(), Some(&media("a")));
    assert_eq!(
        session.state().queue().active(),
        None,
        "selection never activates"
    );
}

#[test]
fn removing_a_nonplaying_entry_leaves_playback_alone() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("b")]).expect("fits");
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());
    let removal = session
        .remove_entry(ids[1], &progress(1, "a", 3, Some(request)), clock.sample())
        .expect("known");
    assert!(!removal.stop_playback);
    assert_eq!(session.state().queue().active(), Some(ids[0]));
}

#[test]
fn clearing_stops_and_keeps_listening_history() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("b")]).expect("fits");
    let request = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .expect("registered");
    session.observe(&loaded(request, 1, "b"), clock.sample());
    session.observe(&playing(1), clock.sample());
    let removal = session.clear_queue(&progress(1, "b", 9, Some(request)), clock.sample());
    assert!(removal.stop_playback);
    assert!(session.state().queue().is_empty());
    assert!(session.state().entry_for(&media("b")).is_some());
}

#[test]
fn advancing_into_partial_and_completed_entries_uses_the_resume_policy() {
    let file = serde_json::json!({ "schema_version": 3, "volume": 1.0, "checkpoints": {
        "local:/music/half.flac": { "position": { "secs": 40, "nanos": 0 }, "completed": false, "touch_seq": 1, "updated_at": "2026-09-14T10:00:00Z" },
        "local:/music/done.flac": { "position": { "secs": 90, "nanos": 0 }, "completed": true, "touch_seq": 2, "updated_at": "2026-09-14T10:00:00Z" } } });
    let session = Session::new(serde_json::from_value(file).expect("valid"));
    assert_eq!(
        session.resume_intent(&media("half")),
        ResumeIntent::Candidate(ResumeCandidate {
            position: Duration::from_secs(40),
            completed: false
        })
    );
    assert!(matches!(
        session.resume_intent(&media("done")),
        ResumeIntent::Candidate(ResumeCandidate {
            completed: true,
            ..
        })
    ));
    assert_eq!(
        session.resume_intent(&media("new")),
        ResumeIntent::StartAt(Duration::ZERO)
    );
}

#[test]
fn a_queued_track_can_lose_its_history_to_eviction_and_stays_queued() {
    let mut checkpoints = serde_json::Map::new();
    for i in 0..512 {
        checkpoints.insert(
            format!("local:/music/old{i}.flac"),
            serde_json::json!({
                "position": { "secs": 5, "nanos": 0 }, "completed": false, "touch_seq": i + 1, "updated_at": "2026-09-14T10:00:00Z" }),
        );
    }
    let file = serde_json::json!({ "schema_version": 3, "volume": 1.0, "checkpoints": checkpoints,
        "queue": [{ "id": 1, "media": "local:/music/old0.flac", "source": { "kind": "local", "path": "/music/old0.flac" } },
                  { "id": 2, "media": "local:/music/fresh.flac", "source": { "kind": "local", "path": "/music/fresh.flac" } }] });
    let clock = FakeClock::new();
    let mut session = Session::new(serde_json::from_value(file).expect("valid"));
    let ids: Vec<_> = session
        .state()
        .queue()
        .entries()
        .iter()
        .map(|e| e.id())
        .collect();
    let request = session
        .register_load(LoadTarget::Queue(ids[1]), &media("fresh"))
        .expect("registered");
    session.observe(&loaded(request, 1, "fresh"), clock.sample());
    session.observe(&playing(1), clock.sample());
    clock.advance(Duration::from_secs(6));
    let _ = session.tick(&progress(1, "fresh", 3, Some(request)), clock.sample());
    assert!(
        session.state().entry_for(&media("old0")).is_none(),
        "oldest history evicted"
    );
    assert_eq!(
        session.state().queue().len(),
        2,
        "queue membership does not pin history"
    );
    assert_eq!(
        session.resume_intent(&media("old0")),
        ResumeIntent::StartAt(Duration::ZERO)
    );
}

#[test]
fn volume_and_display_updates_submit_through_the_session() {
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("a")]).expect("fits");
    assert!(matches!(
        session.set_volume(tenuto::playback::volume::Volume::new(0.4)),
        Action::Submit { .. }
    ));
    let update = DisplayUpdate {
        title: Some("Title".into()),
        artist: Some("Artist".into()),
        album: None,
        year: None,
        duration: None,
    };
    assert!(matches!(
        session.update_display(&media("a"), update),
        Action::Submit { .. }
    ));
    for id in ids {
        assert_eq!(
            session
                .state()
                .queue()
                .get(id)
                .and_then(|e| e.display().title.as_deref()),
            Some("Title")
        );
    }
}

#[test]
fn an_identical_display_update_submits_nothing_and_leaves_state_unchanged() {
    let mut session = Session::new(PersistedState::default());
    session.enqueue(vec![entry("a")]).expect("fits");
    let update = DisplayUpdate {
        title: Some("Title".into()),
        artist: Some("Artist".into()),
        album: None,
        year: None,
        duration: None,
    };
    assert!(matches!(
        session.update_display(&media("a"), update.clone()),
        Action::Submit { .. }
    ));
    let before = session.state().clone();
    assert!(matches!(
        session.update_display(&media("a"), update),
        Action::None
    ));
    assert_eq!(
        session.state().queue().entries(),
        before.queue().entries(),
        "a repeated, unchanged update must not touch state"
    );
}

#[test]
fn a_display_update_that_repeats_one_field_and_changes_another_submits_and_keeps_the_repeated_field()
 {
    let mut session = Session::new(PersistedState::default());
    session.enqueue(vec![entry("a")]).expect("fits");
    session.update_display(
        &media("a"),
        DisplayUpdate {
            title: Some("Title".into()),
            artist: Some("Artist".into()),
            album: None,
            year: None,
            duration: None,
        },
    );
    let action = session.update_display(
        &media("a"),
        DisplayUpdate {
            title: Some("Title".into()),
            artist: Some("New Artist".into()),
            album: None,
            year: None,
            duration: None,
        },
    );
    assert!(matches!(action, Action::Submit { .. }));
    let entry = session
        .state()
        .queue()
        .entries()
        .first()
        .expect("one entry");
    assert_eq!(entry.display().title.as_deref(), Some("Title"));
    assert_eq!(entry.display().artist.as_deref(), Some("New Artist"));
}

/// `volume_and_display_updates_submit_through_the_session` already leaves
/// `album` and `duration` as `None` throughout, which shows a `None` field
/// staying absent — but not that a `None` field leaves an *existing* value
/// alone. This test covers that case directly: `album`, once set, survives a
/// second update that carries `None` for it.
#[test]
fn a_none_field_in_a_display_update_never_blanks_an_existing_value() {
    let mut session = Session::new(PersistedState::default());
    session.enqueue(vec![entry("a")]).expect("fits");
    session.update_display(
        &media("a"),
        DisplayUpdate {
            title: None,
            artist: None,
            album: Some("Album".into()),
            year: None,
            duration: None,
        },
    );
    let action = session.update_display(
        &media("a"),
        DisplayUpdate {
            title: Some("Title".into()),
            artist: None,
            album: None,
            year: None,
            duration: None,
        },
    );
    assert!(matches!(action, Action::Submit { .. }));
    let entry = session
        .state()
        .queue()
        .entries()
        .first()
        .expect("one entry");
    assert_eq!(entry.display().title.as_deref(), Some("Title"));
    assert_eq!(
        entry.display().album.as_deref(),
        Some("Album"),
        "a None field must not blank an existing value"
    );
}

/// A sink slow enough that snapshots queue up behind it.
struct SlowSink {
    inner: StateStore,
    seen: Arc<Mutex<usize>>,
}

impl StateSink for SlowSink {
    fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        std::thread::sleep(Duration::from_millis(150));
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        self.inner.write(state)
    }
}

#[test]
fn queue_and_checkpoint_writes_interleave_into_one_latest_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new());
    let store = StateStore::new(dir.path().join("state.json"), clock.clone());
    let seen = Arc::new(Mutex::new(0));
    let mut writer = WriterHandle::spawn(
        Box::new(SlowSink {
            inner: store,
            seen: seen.clone(),
        }),
        clock.clone(),
    );
    let mut session = Session::new(PersistedState::default());

    let (ids, action) = session.enqueue(vec![entry("a"), entry("b")]).expect("fits");
    if let Action::Submit { state, .. } = action {
        writer.submit(state, Urgency::Forced);
    }
    let request = session
        .register_load(LoadTarget::Queue(ids[0]), &media("a"))
        .expect("registered");
    if let Action::Submit { state, urgency } =
        session.observe(&loaded(request, 1, "a"), clock.sample())
    {
        writer.submit(state, urgency);
    }
    session.observe(&playing(1), clock.sample());
    clock.advance(Duration::from_secs(6));
    if let Action::Submit { state, urgency } =
        session.tick(&progress(1, "a", 17, Some(request)), clock.sample())
    {
        writer.submit(state, urgency);
    }
    let (_, action) = session.enqueue(vec![entry("c")]).expect("fits");
    if let Action::Submit { state, .. } = action {
        writer.submit(state, Urgency::Forced);
    }
    assert!(matches!(
        writer.shutdown(),
        tenuto::persistence::writer::ShutdownOutcome::Written
    ));

    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).expect("read"))
            .expect("json");
    assert_eq!(
        written["playlists"][0]["entries"].as_array().map(Vec::len),
        Some(3),
        "latest queue"
    );
    assert_eq!(written["playlists"][0]["active_entry"], ids[0].get());
    assert_eq!(
        written["checkpoints"]["local:/music/a.flac"]["position"]["secs"], 17,
        "latest checkpoint"
    );
}
