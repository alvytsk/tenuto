mod support;

use std::time::Duration;

use support::media;
use tenuto::media::id::{EpisodeKey, FeedId, MediaId, NormalizedUrl};
use tenuto::persistence::model::PersistedState;
use tenuto::playback::checkpoint::PlaybackCheckpoint;
use tenuto::playback::command::ResumeIntent;
use tenuto::session::Session;
use time::OffsetDateTime;

/// A state whose `media` has an established 90-second checkpoint.
fn remembered(media: &MediaId) -> Session {
    let mut state = PersistedState::default();
    state.record(
        &PlaybackCheckpoint {
            media: media.clone(),
            position: Duration::from_secs(90),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        },
        false,
    );
    Session::new(state)
}

fn remote_media(url: &str) -> MediaId {
    MediaId::RemoteUrl(
        NormalizedUrl::parse(url).unwrap_or_else(|error| panic!("a literal URL: {error}")),
    )
}

fn episode_media() -> MediaId {
    MediaId::PodcastEpisode {
        feed: FeedId::new("0123456789abcdef0123456789abcdef".into())
            .unwrap_or_else(|error| panic!("a literal feed ID: {error}")),
        episode: EpisodeKey::resolve(Some("guid-1"), None, None)
            .unwrap_or_else(|error| panic!("a literal key: {error}")),
    }
}

#[test]
fn a_local_file_starts_from_zero_even_with_a_checkpoint() {
    let session = remembered(&media("song"));
    assert_eq!(
        session.resume_intent(&media("song")),
        ResumeIntent::StartAt(Duration::ZERO)
    );
    assert!(
        session.state().entry_for(&media("song")).is_some(),
        "the checkpoint is still kept"
    );
}

#[test]
fn a_plain_url_starts_from_zero_too() {
    let url = remote_media("https://example.test/track.mp3");
    assert_eq!(
        remembered(&url).resume_intent(&url),
        ResumeIntent::StartAt(Duration::ZERO)
    );
}

#[test]
fn a_podcast_episode_still_resumes() {
    let episode = episode_media();
    assert_ne!(
        remembered(&episode).resume_intent(&episode),
        ResumeIntent::StartAt(Duration::ZERO)
    );
}
