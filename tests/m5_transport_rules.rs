mod support;

use std::time::Duration;

use support::media;
use tenuto::application::transport::*;
use tenuto::media::id::MediaId;
use tenuto::persistence::model::PersistedState;
use tenuto::queue::{
    DisplayMetadata, IdAllocator, NewQueueEntry, Queue, QueueEntryId, QueueSource,
};
use tenuto::session::{LoadTarget, Session};

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

/// A queue of three with the second entry active, built through Session so
/// the active entry is set the only legal way.
fn with_active() -> (Queue, Vec<QueueEntryId>) {
    use tenuto::clock::{Clock, FakeClock};
    use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
    use tenuto::playback::event::{PlaybackEvent, StartDisposition};
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session
        .enqueue(
            session.state().playing(),
            vec![entry("a"), entry("b"), entry("c")],
        )
        .unwrap_or_else(|error| panic!("fits: {error}"));
    let request = session
        .register_load(LoadTarget::Queue(ids[1]), &media("b"))
        .unwrap_or_else(|error| panic!("registered: {error:?}"));
    session.observe(
        &PlaybackEvent::Loaded {
            session_rev: 1,
            request,
            media: media("b"),
            metadata: Default::default(),
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::ZERO,
            disposition: StartDisposition::Fresh,
        },
        FakeClock::new().sample(),
    );
    (session.state().queue().clone(), ids)
}

fn without_active() -> (Queue, Vec<QueueEntryId>) {
    let mut queue = Queue::default();
    let ids = queue
        .enqueue(
            vec![entry("a"), entry("b"), entry("c")],
            &mut IdAllocator::default(),
        )
        .unwrap_or_else(|error| panic!("fits: {error}"));
    (queue, ids)
}

fn run(
    queue: &Queue,
    selected: Option<QueueEntryId>,
    phase: PlaybackPhase,
    last: Option<QueueEntryId>,
    input: TransportInput,
) -> TransportDecision {
    decide(
        input,
        &TransportSituation {
            queue,
            selected,
            phase,
            last_requested: last,
            live: false,
        },
    )
}

#[test]
fn a_restored_active_entry_loads_on_space_regardless_of_selection() {
    let (queue, ids) = with_active();
    for input in [TransportInput::Space, TransportInput::Play] {
        assert_eq!(
            run(&queue, Some(ids[2]), PlaybackPhase::Unloaded, None, input),
            TransportDecision::Load(ids[1])
        );
    }
    assert_eq!(
        run(
            &queue,
            Some(ids[2]),
            PlaybackPhase::Unloaded,
            None,
            TransportInput::Enter
        ),
        TransportDecision::Load(ids[2])
    );
    for input in [
        TransportInput::Home,
        TransportInput::SeekBy(10),
        TransportInput::SeekBy(-10),
        TransportInput::SeekTo(Duration::from_secs(3)),
    ] {
        assert_eq!(
            run(&queue, Some(ids[2]), PlaybackPhase::Unloaded, None, input),
            TransportDecision::Notice(PLAY_BEFORE_SEEK)
        );
    }
}

#[test]
fn without_an_active_entry_the_default_selection_is_the_first_row() {
    let (queue, ids) = without_active();
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Unloaded,
            None,
            TransportInput::Space
        ),
        TransportDecision::Load(ids[0])
    );
    assert_eq!(
        run(
            &queue,
            Some(ids[2]),
            PlaybackPhase::Unloaded,
            None,
            TransportInput::Play
        ),
        TransportDecision::Load(ids[2])
    );
}

#[test]
fn an_empty_queue_answers_every_transport_key_with_a_notice() {
    let queue = Queue::default();
    for input in [
        TransportInput::Space,
        TransportInput::Play,
        TransportInput::Enter,
        TransportInput::Home,
        TransportInput::SeekBy(10),
    ] {
        assert_eq!(
            run(&queue, None, PlaybackPhase::Unloaded, None, input),
            TransportDecision::Notice(QUEUE_EMPTY)
        );
    }
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Unloaded,
            None,
            TransportInput::Next
        ),
        TransportDecision::Nothing
    );
}

#[test]
fn an_ended_last_entry_replays_on_play_and_restarts_on_home() {
    let (queue, ids) = with_active();
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::Ended,
            None,
            TransportInput::Space
        ),
        TransportDecision::Load(ids[1])
    );
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::Ended,
            None,
            TransportInput::Enter
        ),
        TransportDecision::Load(ids[0])
    );
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::Ended,
            None,
            TransportInput::Home
        ),
        TransportDecision::Restart
    );
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::Ended,
            None,
            TransportInput::SeekBy(10)
        ),
        TransportDecision::Notice(TRACK_ENDED)
    );
}

#[test]
fn loading_never_accumulates_a_seek() {
    let (queue, ids) = with_active();
    for input in [TransportInput::Home, TransportInput::SeekBy(-10)] {
        assert_eq!(
            run(&queue, None, PlaybackPhase::Loading, Some(ids[2]), input),
            TransportDecision::Notice(STILL_LOADING)
        );
    }
}

#[test]
fn playing_uses_engine_semantics() {
    let (queue, ids) = with_active();
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::Space
        ),
        TransportDecision::TogglePause
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::Play
        ),
        TransportDecision::Play
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Paused,
            None,
            TransportInput::SeekBy(10)
        ),
        TransportDecision::SeekBy(10)
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Stopped,
            None,
            TransportInput::Home
        ),
        TransportDecision::Restart
    );
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::Playing,
            None,
            TransportInput::Enter
        ),
        TransportDecision::Load(ids[0])
    );
}

#[test]
fn a_failed_load_retries_the_last_requested_entry_while_it_is_queued() {
    let (queue, ids) = without_active();
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::LoadFailed,
            Some(ids[2]),
            TransportInput::Space
        ),
        TransportDecision::Load(ids[2])
    );
    let mut shorter = queue.clone();
    shorter.remove(ids[2]).expect("known");
    assert_eq!(
        run(
            &shorter,
            Some(ids[1]),
            PlaybackPhase::LoadFailed,
            Some(ids[2]),
            TransportInput::Play
        ),
        TransportDecision::Load(ids[1])
    );
}

#[test]
fn previous_and_next_anchor_on_the_active_entry_and_never_wrap() {
    let (queue, ids) = with_active();
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::Playing,
            None,
            TransportInput::Next
        ),
        TransportDecision::Load(ids[2])
    );
    assert_eq!(
        run(
            &queue,
            Some(ids[2]),
            PlaybackPhase::Unloaded,
            None,
            TransportInput::Previous
        ),
        TransportDecision::Load(ids[0])
    );
    let (queue, ids) = without_active();
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::Unloaded,
            None,
            TransportInput::Previous
        ),
        TransportDecision::Nothing
    );
    assert_eq!(
        run(
            &queue,
            Some(ids[2]),
            PlaybackPhase::Unloaded,
            None,
            TransportInput::Next
        ),
        TransportDecision::Nothing
    );
}

// An empty queue does not freeze a track that is already playing or paused:
// Space, p, Home and seeks still act on the loaded track, and only Enter -
// which would otherwise pick a row that no longer exists - reports the queue
// as empty. Once playback has stopped, the loaded track's adoption is gone
// too, so every transport key reports the empty queue instead.

#[test]
fn an_empty_queue_keeps_engine_semantics_while_playing() {
    let queue = Queue::default();
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::Space
        ),
        TransportDecision::TogglePause
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::Play
        ),
        TransportDecision::Play
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::Home
        ),
        TransportDecision::Restart
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::SeekBy(10)
        ),
        TransportDecision::SeekBy(10)
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::Enter
        ),
        TransportDecision::Notice(QUEUE_EMPTY)
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::Next
        ),
        TransportDecision::Nothing
    );
}

#[test]
fn an_empty_queue_keeps_engine_semantics_while_paused() {
    let queue = Queue::default();
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Paused,
            None,
            TransportInput::Space
        ),
        TransportDecision::TogglePause
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Paused,
            None,
            TransportInput::Play
        ),
        TransportDecision::Play
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Paused,
            None,
            TransportInput::Home
        ),
        TransportDecision::Restart
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Paused,
            None,
            TransportInput::SeekBy(10)
        ),
        TransportDecision::SeekBy(10)
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Paused,
            None,
            TransportInput::Enter
        ),
        TransportDecision::Notice(QUEUE_EMPTY)
    );
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Paused,
            None,
            TransportInput::Next
        ),
        TransportDecision::Nothing
    );
}

#[test]
fn an_empty_queue_while_stopped_still_notices() {
    let queue = Queue::default();
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Stopped,
            None,
            TransportInput::Space
        ),
        TransportDecision::Notice(QUEUE_EMPTY)
    );
}

// A selected row that has since been removed is ignored; the default
// selection falls back to the first row, so a stale id never produces a
// load that a later queue registration would reject.

#[test]
fn a_stale_selection_falls_back_to_the_first_row() {
    let (mut queue, ids) = without_active();
    queue.remove(ids[0]).expect("known");
    assert_eq!(
        run(
            &queue,
            Some(ids[0]),
            PlaybackPhase::Unloaded,
            None,
            TransportInput::Space
        ),
        TransportDecision::Load(ids[1])
    );
}

// An ended track with no active queue entry replays from the selection, the
// same as before anything loaded.

#[test]
fn ended_with_no_active_entry_loads_the_selection_on_space() {
    let (queue, ids) = without_active();
    assert_eq!(
        run(
            &queue,
            Some(ids[1]),
            PlaybackPhase::Ended,
            None,
            TransportInput::Space
        ),
        TransportDecision::Load(ids[1])
    );
}

// While loading, Previous/Next anchor on the last requested entry only if it
// is still queued; otherwise on the active entry, then the selection.

#[test]
fn loading_anchors_on_active_when_last_requested_is_gone() {
    // with_active(): a, b(active), c. Removing a leaves b at index 0 and c
    // at index 1, so a Next from the active entry (not the stale
    // last_requested) reaches c.
    let (queue, ids) = with_active();
    let mut shorter = queue.clone();
    shorter.remove(ids[0]).expect("known");
    assert_eq!(
        run(
            &shorter,
            None,
            PlaybackPhase::Loading,
            Some(ids[0]),
            TransportInput::Next
        ),
        TransportDecision::Load(ids[2])
    );
}

// With no active entry, Previous/Next during playback, pause or stop do
// nothing.

#[test]
fn playing_with_no_active_entry_does_not_navigate() {
    let (queue, _ids) = without_active();
    assert_eq!(
        run(
            &queue,
            None,
            PlaybackPhase::Playing,
            None,
            TransportInput::Next
        ),
        TransportDecision::Nothing
    );
}

// M7 §3.4/§6.3: a live entry is controlled like a playing one, but it has no
// timeline to move around in.

fn live(queue: &Queue, phase: PlaybackPhase, input: TransportInput) -> TransportDecision {
    decide(
        input,
        &TransportSituation {
            queue,
            selected: None,
            phase,
            last_requested: None,
            live: true,
        },
    )
}

#[test]
fn a_live_entry_answers_every_seek_and_home_with_a_notice() {
    let (queue, _) = with_active();
    for phase in [
        PlaybackPhase::Playing,
        PlaybackPhase::Reconnecting,
        PlaybackPhase::Paused,
        PlaybackPhase::Stopped,
    ] {
        for input in [
            TransportInput::SeekBy(10),
            TransportInput::SeekTo(Duration::from_secs(5)),
            TransportInput::Home,
        ] {
            assert_eq!(
                live(&queue, phase, input),
                TransportDecision::Notice(LIVE_NO_SEEK),
                "{phase:?} {input:?}"
            );
        }
    }
}

#[test]
fn reconnecting_is_controlled_like_playing() {
    let (queue, _) = with_active();
    assert_eq!(
        live(&queue, PlaybackPhase::Reconnecting, TransportInput::Space),
        TransportDecision::TogglePause
    );
    assert_eq!(
        live(&queue, PlaybackPhase::Reconnecting, TransportInput::Play),
        TransportDecision::Play
    );
}
