use crate::error::DomainError;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::{Component, Path, PathBuf},
    str::FromStr,
};
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AbsolutePath(String);

impl AbsolutePath {
    pub fn new(path: PathBuf) -> Result<Self, DomainError> {
        let invalid = |reason| DomainError::InvalidPath {
            path: path.clone(),
            reason,
        };
        let raw = path
            .to_str()
            .ok_or_else(|| invalid("non-UTF-8 paths are unsupported"))?;
        if !path.is_absolute() {
            return Err(invalid("path must be absolute"));
        }
        let prefix_len = match path.components().next() {
            Some(Component::Prefix(prefix)) => prefix.as_os_str().len(),
            _ => 0,
        };
        let rooted = raw
            .get(prefix_len..)
            .ok_or_else(|| invalid("unsupported path prefix"))?;
        let tail = rooted
            .strip_prefix(std::path::is_separator)
            .ok_or_else(|| invalid("path must include a root separator"))?;
        if !tail.is_empty()
            && tail
                .split(std::path::is_separator)
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(invalid("path contains empty, . or .. components"));
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NormalizedUrl(String);

impl NormalizedUrl {
    pub fn parse(input: &str) -> Result<Self, DomainError> {
        let mut url = Url::parse(input).map_err(|source| DomainError::InvalidUrl {
            input: input.into(),
            source,
        })?;
        // The full public-source bar, one check: http(s) with a host, and no
        // embedded credentials. The credentials ban is the point of doing
        // this here rather than at every caller - the foundation type must
        // not be the one place a `user:password@` URL could slip into a
        // `MediaId`.
        if !super::is_public_source_url(&url) {
            return Err(DomainError::UnsupportedUrl {
                input: input.into(),
            });
        }
        url.set_fragment(None);
        Ok(Self(url.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FeedId(String);

impl FeedId {
    pub fn new(value: String) -> Result<Self, DomainError> {
        if value.is_empty() {
            return Err(DomainError::EmptyFeedId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EpisodeKey(EpisodeIdentity);

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum EpisodeIdentity {
    Guid(String),
    Url(NormalizedUrl),
}

impl EpisodeKey {
    pub fn resolve(
        guid: Option<&str>,
        enclosure: Option<&Url>,
        link: Option<&Url>,
    ) -> Result<Self, DomainError> {
        if let Some(guid) = guid.filter(|value| !value.is_empty()) {
            return Ok(Self(EpisodeIdentity::Guid(guid.into())));
        }
        let url = enclosure
            .or(link)
            .ok_or(DomainError::MissingEpisodeIdentity)?;
        Ok(Self(EpisodeIdentity::Url(NormalizedUrl::parse(
            url.as_str(),
        )?)))
    }

    fn canonical(&self) -> String {
        match &self.0 {
            EpisodeIdentity::Guid(guid) => format!("guid:{guid}"),
            EpisodeIdentity::Url(url) => format!("url:{}", url.as_str()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum MediaId {
    LocalFile(AbsolutePath),
    PodcastEpisode { feed: FeedId, episode: EpisodeKey },
    RemoteUrl(NormalizedUrl),
}

const ID_ESCAPE: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'\\').add(b'%');
const PODCAST_ESCAPE: &AsciiSet = &ID_ESCAPE.add(b'/');

fn escape(value: &str, set: &'static AsciiSet) -> String {
    utf8_percent_encode(value, set).to_string()
}

impl fmt::Display for MediaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalFile(path) => write!(f, "local:{}", escape(path.as_str(), ID_ESCAPE)),
            Self::RemoteUrl(url) => write!(f, "remote:{}", escape(url.as_str(), ID_ESCAPE)),
            Self::PodcastEpisode { feed, episode } => write!(
                f,
                "podcast:{}/{}",
                escape(feed.as_str(), PODCAST_ESCAPE),
                escape(&episode.canonical(), PODCAST_ESCAPE)
            ),
        }
    }
}

impl From<MediaId> for String {
    fn from(value: MediaId) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for MediaId {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl FromStr for MediaId {
    type Err = DomainError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = |reason| DomainError::InvalidMediaId {
            input: input.into(),
            reason,
        };
        let decode = |part: &str| -> Result<String, DomainError> {
            percent_decode_str(part)
                .decode_utf8()
                .map(|value| value.into_owned())
                .map_err(|_| invalid("component is not UTF-8"))
        };
        let (kind, body) = input
            .split_once(':')
            .ok_or_else(|| invalid("missing kind separator"))?;
        let id = match kind {
            "local" => Self::LocalFile(AbsolutePath::new(PathBuf::from(decode(body)?))?),
            "remote" => Self::RemoteUrl(NormalizedUrl::parse(&decode(body)?)?),
            "podcast" => {
                let (feed, key) = body
                    .split_once('/')
                    .ok_or_else(|| invalid("missing episode separator"))?;
                let key = decode(key)?;
                let episode = if let Some(guid) = key.strip_prefix("guid:") {
                    if guid.is_empty() {
                        return Err(invalid("empty GUID"));
                    }
                    EpisodeKey(EpisodeIdentity::Guid(guid.into()))
                } else if let Some(url) = key.strip_prefix("url:") {
                    EpisodeKey(EpisodeIdentity::Url(NormalizedUrl::parse(url)?))
                } else {
                    return Err(invalid("unknown episode key kind"));
                };
                Self::PodcastEpisode {
                    feed: FeedId::new(decode(feed)?)?,
                    episode,
                }
            }
            _ => return Err(invalid("unknown media kind")),
        };
        if id.to_string() != input {
            return Err(invalid("noncanonical representation"));
        }
        Ok(id)
    }
}

impl MediaId {
    /// The subscription this id belongs to, or `None` for a `LocalFile` or
    /// `RemoteUrl` (§2.2). Lets callers reach the feed without a duplicate
    /// `key` field on `Episode`.
    pub fn feed(&self) -> Option<&FeedId> {
        match self {
            Self::PodcastEpisode { feed, .. } => Some(feed),
            _ => None,
        }
    }

    /// The episode identity within its feed, or `None` for a `LocalFile` or
    /// `RemoteUrl` (§2.2).
    pub fn episode_key(&self) -> Option<&EpisodeKey> {
        match self {
            Self::PodcastEpisode { episode, .. } => Some(episode),
            _ => None,
        }
    }
}
