use std::{collections::HashMap, path::PathBuf};
use tenuto::media::id::{AbsolutePath, EpisodeKey, FeedId, MediaId, NormalizedUrl};

#[allow(clippy::unwrap_used)] // Fallible construction of fixed test fixtures.
fn identities() -> Vec<MediaId> {
    let path = if cfg!(windows) {
        "C:/audio/a #?:%.mp3"
    } else {
        "/audio/a #?:%.mp3"
    };
    let mut ids = vec![
        MediaId::LocalFile(AbsolutePath::new(PathBuf::from(path)).unwrap()),
        MediaId::RemoteUrl(NormalizedUrl::parse("https://example.com:443/a?q=%2f&x=1#f").unwrap()),
    ];
    let enclosure = url::Url::parse("https://example.com/audio?sig=%2F#part").unwrap();
    ids.push(MediaId::PodcastEpisode {
        feed: FeedId::new("subscription-1".into()).unwrap(),
        episode: EpisodeKey::resolve(None, Some(&enclosure), None).unwrap(),
    });
    for value in [
        "#",
        "/",
        ":",
        "?",
        " ",
        "%2F",
        "тест/🎵",
        "url:https://example.com/x",
        "guid:x",
        "https://feed.example/a b?q=1#frag",
    ] {
        ids.push(MediaId::PodcastEpisode {
            feed: FeedId::new(value.into()).unwrap(),
            episode: EpisodeKey::resolve(Some(value), None, None).unwrap(),
        });
    }
    ids
}

#[test]
fn canonical_strings_and_json_map_keys_round_trip() {
    assert_eq!(
        MediaId::RemoteUrl(
            NormalizedUrl::parse("https://cdn.radio-t.com/rt_podcast900.mp3").unwrap()
        )
        .to_string(),
        "remote:https://cdn.radio-t.com/rt_podcast900.mp3"
    );
    for id in identities() {
        let encoded = id.to_string();
        assert_eq!(encoded.parse::<MediaId>().unwrap(), id);
        let map = HashMap::from([(id.clone(), 42_u64)]);
        let json = serde_json::to_string(&map).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[&encoded], 42);
        assert_eq!(
            serde_json::from_str::<HashMap<MediaId, u64>>(&json).unwrap(),
            map
        );
        assert_eq!(serde_json::to_value(&id).unwrap(), encoded);
    }
}

#[test]
fn delimiters_do_not_collide() {
    let make = |feed: &str, guid: &str| MediaId::PodcastEpisode {
        feed: FeedId::new(feed.into()).unwrap(),
        episode: EpisodeKey::resolve(Some(guid), None, None).unwrap(),
    };
    assert_ne!(make("a/b", "c").to_string(), make("a", "b/c").to_string());
    assert_eq!(
        make("a/b", "# :?%").to_string(),
        "podcast:a%2Fb/guid:#%20:?%25"
    );
}

#[test]
fn parser_rejects_invalid_and_noncanonical_forms() {
    for input in [
        "",
        "unknown:a",
        "local:relative",
        "remote:%FF",
        "remote:%GG",
        "podcast:a",
        "podcast:/guid%3Ax",
        "podcast:a/guid%3A",
        "podcast:a/other%3Ax",
        "podcast:a/guid:x/extra",
        "podcast:%61/guid:x",
        "podcast:a/guid:%GG",
        "podcast:a/guid:%2f",
    ] {
        assert!(input.parse::<MediaId>().is_err(), "{input}");
        assert!(serde_json::from_value::<MediaId>(serde_json::json!(input)).is_err());
    }
}

/// The full public-source bar lives in the foundation type itself: a
/// `user:password@` URL must never be carried into a `MediaId` because a
/// caller skipped the transport's own source check.
#[test]
fn normalization_refuses_embedded_credentials() {
    for input in [
        "https://user:pw@example.org/audio.mp3",
        "https://user@example.org/audio.mp3",
        "http://example.org:80@evil.example/audio.mp3",
    ] {
        assert!(
            NormalizedUrl::parse(input).is_err(),
            "{input} must be refused"
        );
    }
    assert!(NormalizedUrl::parse("https://example.org/audio.mp3").is_ok());
}
