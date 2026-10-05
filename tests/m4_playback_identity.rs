//! §1.6 and §8.5's headline promise, end to end: **what is played is the
//! enclosure, what is checkpointed is the episode.**
//!
//! Every other M4 test asserts that promise on a value — the pair
//! `library::resolve_episode` returns, the `Load` command `resume_commands`
//! builds. This one asserts it on a *file*: a real feed is cached, a real
//! episode is resolved from it, the existing engine plays that episode's
//! enclosure over loopback HTTP through the virtual audio output, and the
//! checkpoint the session reconciles at shutdown is written to a real
//! `state.json`. What that file is keyed on is then read back.
//!
//! Nothing here is a podcast-specific code path. The engine, `Session`, the
//! shutdown reconciliation and `StateStore` are all M1–M3 code, reused
//! unchanged; the only M4 contribution is which `(MediaId, SourceLocation)`
//! pair enters the `Load`. That is exactly what makes the assertion worth
//! making — a `RemoteUrl` key appearing in the file would mean the identity
//! was re-derived somewhere downstream of resolution, and a feed moving its
//! audio to another CDN would then silently lose a listener's position.

mod support;

#[path = "support/feeds.rs"]
mod feeds;

use std::time::Duration;

use support::server::{Script, TestServer};
use tenuto::{
    clock::Clock,
    http::{limits::Limits, service::HttpService},
    library,
    media::id::{MediaId, NormalizedUrl},
    persistence::model::PersistedState,
    playback::{
        command::{PlaybackCommand, ResumeIntent},
        state::PlaybackState,
    },
    session::{LoadTarget, Session},
};

type Fallible = Result<(), Box<dyn std::error::Error>>;

/// The acceptance test for §1.6's "distinct identities" rule.
///
/// The checkpoint written at shutdown is keyed on the podcast episode the
/// cache resolved, and the enclosure URL that was actually fetched — the one
/// a direct `tenuto play <url>` session would have checkpointed — is
/// absent from the same file.
#[test]
fn playback_persists_the_podcast_id_not_the_enclosure_url() -> Fallible {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let enclosure = server.url("/audio.flac");
    let xml = format!(
        "<rss><channel><title>Radio T</title>\
         <item><guid>episode-1</guid><title>First</title>\
         <enclosure url=\"{enclosure}\"/></item></channel></rss>"
    );
    rig.seed(xml.as_bytes(), "https://example.org/feed")?;

    let (media, source) = library::resolve_episode(&rig.subs, &rig.cache, "radio-t", 1)?;
    // The pair leaving resolution already disagrees about identity: only the
    // source is the enclosure. Asserted here so a later failure downstream
    // cannot be mistaken for resolution having handed over the wrong pair.
    assert!(
        matches!(media, MediaId::PodcastEpisode { .. }),
        "resolution must hand over the podcast identity: {media:?}"
    );

    let mut engine = support::TestEngine::start_idle();
    engine
        .handle()
        .set_http(Some(HttpService::spawn(Limits::brisk())?));
    // Registered before the `Load` is sent, so the `Loaded` it produces
    // carries a token this session recognizes (M5 §6).
    let mut session = Session::new(PersistedState::default());
    let request = session
        .register_load(LoadTarget::Detached, &media)
        .map_err(|error| format!("cannot register the load: {error:?}"))?;
    engine.send(PlaybackCommand::Load {
        request,
        media: media.clone(),
        source,
        resume: ResumeIntent::StartAt(Duration::ZERO),
    });
    engine.await_state(PlaybackState::Paused);

    // `await_state` consumes only the state history, never the event inbox,
    // so the `Loaded` that told the session which media is current is still
    // queued here and is handed to `observe` rather than fabricated.
    let mut saw_loaded = false;
    while let Some(event) = engine.try_event() {
        saw_loaded |= matches!(event, tenuto::playback::event::PlaybackEvent::Loaded { .. });
        let _ = session.observe(&event, rig.clock.sample());
    }
    assert!(saw_loaded, "the session never observed the load");

    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_secs(1));
    while let Some(event) = engine.try_event() {
        let _ = session.observe(&event, rig.clock.sample());
    }
    let _ = session.tick(&engine.progress(), rig.clock.sample());

    let report = engine.shutdown_report().ok_or("engine already joined")?;
    rig.state
        .write(&session.reconcile_shutdown(&report, rig.clock.sample()))?;

    let snapshot = rig.state.read_snapshot()?;
    let entry = snapshot
        .entry_for(&media)
        .ok_or("no checkpoint was written for the resolved episode")?;
    assert!(
        entry.position.is_some() || entry.estimated.is_some(),
        "the episode's checkpoint must carry a position: {entry:?}"
    );

    let remote = MediaId::RemoteUrl(NormalizedUrl::parse(&enclosure)?);
    assert!(
        snapshot.entry_for(&remote).is_none(),
        "the enclosure URL must never become a checkpoint key"
    );

    // The same claim once more against the file's own text, so that a future
    // change to `entry_for`'s lookup cannot hide a second entry under a key
    // that merely fails to compare equal.
    let written = std::fs::read_to_string(rig.state.path())?;
    assert!(
        written.contains("podcast:"),
        "the state file must hold a podcast key: {written}"
    );
    assert!(
        !written.contains("remote:"),
        "the state file must hold no remote key: {written}"
    );

    server.shutdown();
    Ok(())
}
