use std::time::Duration;
use tenuto::media::{
    Episode,
    id::{EpisodeKey, FeedId, MediaId, NormalizedUrl},
    metadata::MediaMetadata,
    source::SourceLocation,
};
use tenuto::resume::PlaybackCheckpoint;
use time::OffsetDateTime;
use url::Url;

#[test]
fn fetch_url_stays_separate_from_identity_and_unplayable_items_exist() {
    let fetch = Url::parse("HTTPS://CDN.EXAMPLE.COM:443/audio?z=%2f&a=1&z=%2F#part").unwrap();
    let parsed_spelling = fetch.as_str().to_owned();
    let feed = FeedId::new("subscription-1".into()).unwrap();
    let id = MediaId::PodcastEpisode {
        feed: feed.clone(),
        episode: EpisodeKey::resolve(Some("guid"), Some(&fetch), None).unwrap(),
    };
    let episode = Episode {
        id: id.clone(),
        source: Some(SourceLocation::Http(fetch.clone())),
        title: None,
        published: None,
        declared_duration: None,
    };
    let SourceLocation::Http(actual) = episode.source.unwrap() else {
        panic!("expected HTTP source")
    };
    assert_eq!(actual.as_str(), parsed_spelling);
    assert_eq!(actual.query(), Some("z=%2f&a=1&z=%2F"));
    assert_eq!(actual.fragment(), Some("part"));
    assert_eq!(
        NormalizedUrl::parse(fetch.as_str()).unwrap().as_str(),
        "https://cdn.example.com/audio?z=%2f&a=1&z=%2F"
    );
    let redirected = Url::parse("https://new.example.com/changed?signature=new").unwrap();
    let same_id = MediaId::PodcastEpisode {
        feed,
        episode: EpisodeKey::resolve(Some("guid"), Some(&redirected), None).unwrap(),
    };
    assert_eq!(same_id, id);
    assert!(
        Episode {
            id,
            source: None,
            title: None,
            published: None,
            declared_duration: None,
        }
        .source
        .is_none()
    );
    assert_eq!(
        MediaMetadata {
            title: None,
            artist: None,
            album: None,
            year: None,
            duration: None,
            duration_provenance: tenuto::media::provenance::PositionProvenance::Established,
            front_cover: None,
        }
        .duration,
        None
    );
}

#[test]
fn checkpoint_round_trip_preserves_position_and_rfc3339_timestamp() {
    let media = MediaId::RemoteUrl(NormalizedUrl::parse("https://example.com/audio").unwrap());
    let checkpoint = PlaybackCheckpoint {
        media,
        position: Duration::new(3_597, 123_000_000),
        updated_at: OffsetDateTime::UNIX_EPOCH,
    };
    let json = serde_json::to_value(&checkpoint).unwrap();
    assert_eq!(json["updated_at"], "1970-01-01T00:00:00Z");
    assert!(json["media"].is_string());
    let restored: PlaybackCheckpoint = serde_json::from_value(json).unwrap();
    assert_eq!(restored, checkpoint);
}
