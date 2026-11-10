//! M13 §1.1: the repeat policy on `Session`'s advance decision, the MPRIS
//! `LoopStatus` mapping in both directions, and the additive persistence
//! roundtrip.

mod support;

use std::time::Duration;

use support::media;
use tenuto::application::mpris::{Change, Published, Request, command};
use tenuto::application::runtime::AppCommand;
use tenuto::application::transport::PlaybackPhase;
use tenuto::application::view::{PersistenceStatus, PlayerView};
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::media::provenance::PositionProvenance;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::command::LoadRequestId;
use tenuto::playback::event::{PlaybackEvent, StartDisposition};
use tenuto::playlist::RepeatMode;
use tenuto::queue::{NewQueueEntry, QueueEntryId, QueueSource};
use tenuto::resume::{ResumeDecision, ResumeIntent, StoredProgress, decide_resume, resume_intent};
use tenuto::session::{Action, Advance, LoadTarget, Session};
use tenuto::volume::Volume;

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else {
        unreachable!()
    };
    NewQueueEntry::new(
        media(name),
        QueueSource::LocalFile(path),
        Default::default(),
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

fn ended(session: &mut Session, rev: u64) {
    session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: rev,
            position: Duration::from_secs(180),
            provenance: PositionProvenance::Established,
        },
        FakeClock::new().sample(),
    );
}

fn adopt(session: &mut Session, id: QueueEntryId, name: &str, rev: u64) -> LoadRequestId {
    let request = session
        .register_load(LoadTarget::Queue(id), &media(name))
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    session.observe(&loaded(request, rev, name), FakeClock::new().sample());
    request
}

fn advance(session: &mut Session) -> Option<Advance> {
    session.take_advance()
}

fn set_repeat_expect_submit(session: &mut Session, mode: RepeatMode) {
    assert!(
        matches!(session.set_repeat(mode), Action::Submit { .. }),
        "a repeat change persists"
    );
    assert_eq!(session.state().repeat(), mode);
}

/// Playlist A (the default, playing) holds a1 a2 a3, with a1 adopted.
struct Three {
    session: Session,
    ids: Vec<QueueEntryId>,
}

fn three() -> Three {
    let mut session = Session::new(PersistedState::default());
    let a = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(a, vec![entry("a1"), entry("a2"), entry("a3")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    adopt(&mut session, ids[0], "a1", 1);
    Three { session, ids }
}

#[test]
fn repeat_off_advances_and_stops_at_the_end() {
    let mut three = three();
    ended(&mut three.session, 2);
    assert_eq!(
        advance(&mut three.session),
        Some(Advance::Next(three.ids[1]))
    );
    adopt(&mut three.session, three.ids[1], "a2", 3);
    ended(&mut three.session, 4);
    assert_eq!(
        advance(&mut three.session),
        Some(Advance::Next(three.ids[2]))
    );
    adopt(&mut three.session, three.ids[2], "a3", 5);
    ended(&mut three.session, 6);
    assert_eq!(advance(&mut three.session), Some(Advance::EndOfQueue));
}

#[test]
fn repeat_one_replays_the_ended_entry() {
    let mut three = three();
    set_repeat_expect_submit(&mut three.session, RepeatMode::One);
    ended(&mut three.session, 2);
    assert_eq!(
        advance(&mut three.session),
        Some(Advance::Next(three.ids[0]))
    );
}

#[test]
fn repeat_all_wraps_to_the_first_entry_in_playback_order() {
    let mut three = three();
    three.session.set_repeat(RepeatMode::All);
    adopt(&mut three.session, three.ids[2], "a3", 2);
    ended(&mut three.session, 3);
    assert_eq!(
        advance(&mut three.session),
        Some(Advance::Next(three.ids[0]))
    );
}

#[test]
fn repeat_all_wraps_a_single_entry_playlist_onto_itself() {
    let mut session = Session::new(PersistedState::default());
    let a = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(a, vec![entry("only")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    session.set_repeat(RepeatMode::All);
    adopt(&mut session, ids[0], "only", 1);
    ended(&mut session, 2);
    assert_eq!(advance(&mut session), Some(Advance::Next(ids[0])));
}

#[test]
fn repeat_all_keeps_the_current_shuffled_order_across_the_seam() {
    let mut session = Session::new(PersistedState::default());
    let a = session.state().playlists().playing();
    let (ids, _) = session
        .enqueue(a, vec![entry("a1"), entry("a2"), entry("a3")])
        .unwrap_or_else(|error| panic!("fits: {error}"));
    // A fixed seed, so the order is the same on both sides of the wrap.
    session.set_shuffle(a, Some(0xC0_FF_EE)).expect("shuffled");
    let order = session
        .state()
        .playlists()
        .playlist(a)
        .expect("playing")
        .playback_order();
    let last = *order.last().expect("non-empty");
    let first = order[0];
    let last_name = ["a1", "a2", "a3"][ids
        .iter()
        .position(|id| *id == last)
        .expect("from this batch")];
    session.set_repeat(RepeatMode::All);
    adopt(&mut session, last, last_name, 1);
    ended(&mut session, 2);
    assert_eq!(advance(&mut session), Some(Advance::Next(first)));
    assert_eq!(
        session
            .state()
            .playlists()
            .playlist(a)
            .expect("playing")
            .playback_order(),
        order,
        "the wrap never reshuffles"
    );
}

#[test]
fn repeat_one_of_a_completed_episode_resolves_to_a_start_at_zero() {
    // M13 §1.1's subtle rule: under repeat-one a podcast episode restarts
    // rather than resuming, because the ended entry is completed and D1
    // replays a completed entry from zero — pinned at the `resume` layer
    // the worker resolves against.
    let stored = StoredProgress {
        position: Some(Duration::from_secs(120)),
        estimated: None,
        completed: true,
    };
    let episode = episode_media();
    let ResumeIntent::Candidate(candidate) = resume_intent(&episode, Some(stored)) else {
        panic!("a completed episode resolves to a candidate");
    };
    let decision = decide_resume(Some(candidate), None);
    assert_eq!(decision, ResumeDecision::Completed);
    assert_eq!(decision.start_at(), Duration::ZERO);
}

fn episode_media() -> MediaId {
    let feed = match tenuto::media::id::FeedId::new("0123456789abcdef0123456789abcdef".to_string())
    {
        Ok(feed) => feed,
        Err(error) => panic!("a valid FeedId: {error:?}"),
    };
    let episode = match tenuto::media::id::EpisodeKey::resolve(Some("guid:x"), None, None) {
        Ok(episode) => episode,
        Err(error) => panic!("a resolvable key: {error:?}"),
    };
    MediaId::PodcastEpisode { feed, episode }
}

#[test]
fn repeat_mode_survives_a_save_and_load() {
    let mut session = Session::new(PersistedState::default());
    let _ = session.set_repeat(RepeatMode::One);
    let json = serde_json::to_string(session.state()).expect("serializes");
    assert!(
        json.contains("\"repeat_mode\":\"one\""),
        "the additive field is written: {json}"
    );
    let back: PersistedState = serde_json::from_str(&json).expect("round trips");
    assert_eq!(back.repeat(), RepeatMode::One);
}

#[test]
fn a_state_file_without_the_field_loads_with_repeat_off() {
    let mut session = Session::new(PersistedState::default());
    let _ = session.set_repeat(RepeatMode::All);
    let json = serde_json::to_string(session.state()).expect("serializes");
    let mut stripped: serde_json::Value = serde_json::from_str(&json).expect("parses");
    stripped
        .as_object_mut()
        .expect("a top-level object")
        .remove("repeat_mode");
    let back: PersistedState =
        serde_json::from_value(stripped).expect("an older file shape still loads");
    assert_eq!(back.repeat(), RepeatMode::Off);
}

#[test]
fn an_unknown_repeat_spelling_defaults_to_off() {
    let mut session = Session::new(PersistedState::default());
    let _ = session.set_repeat(RepeatMode::All);
    let json = serde_json::to_string(session.state()).expect("serializes");
    let poisoned: serde_json::Value =
        serde_json::from_str(&json.replace("\"all\"", "\"sometimes\"")).expect("parses");
    let back: PersistedState = serde_json::from_value(poisoned).expect("loads leniently");
    assert_eq!(back.repeat(), RepeatMode::Off);
}

fn view_with(repeat: RepeatMode) -> PlayerView {
    let viewed = PersistedState::default().playlists().playing();
    PlayerView {
        rows: Default::default(),
        tabs: Vec::new(),
        viewed,
        active: None,
        now_playing: None,
        phase: PlaybackPhase::Stopped,
        volume: Volume::FULL,
        repeat,
        status: None,
        persistence: PersistenceStatus::Saving,
        last_requested: None,
        live: false,
        reconnecting: false,
    }
}

#[test]
fn mpris_loop_status_requests_map_to_set_repeat() {
    for mode in [RepeatMode::One, RepeatMode::All] {
        let view = view_with(RepeatMode::Off);
        assert!(
            matches!(
                command(Request::SetLoopStatus(mode), &view),
                Some(AppCommand::SetRepeat(got)) if got == mode
            ),
            "{mode:?} maps while different from the view"
        );
    }
    // Setting the mode the view already has is a no-op, like SetVolume.
    let view = view_with(RepeatMode::All);
    assert!(
        command(Request::SetLoopStatus(RepeatMode::All), &view).is_none(),
        "the same mode is a no-op"
    );
}

#[test]
fn a_loop_status_change_publishes_and_unchanged_stays_quiet() {
    let mut published = Published::default();
    let now = || FakeClock::new().sample().monotonic;
    let changes = published.apply_at(snap_with(RepeatMode::Off), now());
    assert!(changes.contains(&Change::LoopStatus), "the first publish");
    let changes = published.apply_at(snap_with(RepeatMode::All), now());
    assert!(changes.contains(&Change::LoopStatus), "off → all publishes");
    let changes = published.apply_at(snap_with(RepeatMode::All), now());
    assert!(
        !changes.contains(&Change::LoopStatus),
        "unchanged stays quiet"
    );
}

fn snap_with(repeat: RepeatMode) -> tenuto::application::mpris::Snapshot {
    tenuto::application::mpris::Snapshot {
        track: None,
        status: tenuto::application::mpris::Status::Playing,
        volume: 1.0,
        position: Duration::ZERO,
        cover: None,
        shuffle: false,
        repeat,
        sought: false,
    }
}
