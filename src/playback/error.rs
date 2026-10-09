use std::path::PathBuf;

use crate::media::probe::ProbeError;

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("cannot open media {path:?}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot play {path:?}: {reason}")]
    UnsupportedInput { path: PathBuf, reason: String },
    #[error("cannot decode media")]
    Decode(#[source] symphonia::core::errors::Error),
    #[error("cannot seek to {target:?}")]
    SeekFailed {
        target: std::time::Duration,
        #[source]
        source: symphonia::core::errors::Error,
    },
    #[error("the audio output did not respond within the deadline")]
    Timeout,
    #[error("the operation was cancelled")]
    Cancelled,
    #[error("audio output failure")]
    Output(#[source] cpal::Error),
    #[error("terminal I/O error")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Remote(#[from] crate::http::error::RemoteFailure),
}

/// A probe failure is the playback failure of the same name: fields and
/// source move across unchanged, so text and chain stay what they were.
impl From<ProbeError> for PlaybackError {
    fn from(error: ProbeError) -> Self {
        match error {
            ProbeError::Open { path, source } => Self::Open { path, source },
            ProbeError::UnsupportedInput { path, reason } => {
                Self::UnsupportedInput { path, reason }
            }
            ProbeError::Decode(source) => Self::Decode(source),
            ProbeError::Io(source) => Self::Io(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;
    use std::path::PathBuf;

    use super::PlaybackError;
    use crate::media::probe::ProbeError;

    fn source_text(error: &dyn Error) -> Option<String> {
        error.source().map(ToString::to_string)
    }

    /// M9 foundation cleanup §3.2: a probe failure converts into the
    /// same-named `PlaybackError` variant with its fields and source moved
    /// across, never wrapped, so its text and chain are what they were.
    #[test]
    fn a_probe_error_becomes_the_same_playback_error() {
        let path = PathBuf::from("/music/a.mp3");

        let probe = ProbeError::Open {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::NotFound, "gone"),
        };
        assert_eq!(probe.to_string(), r#"cannot open media "/music/a.mp3""#);
        let converted = PlaybackError::from(probe);
        assert_eq!(converted.to_string(), r#"cannot open media "/music/a.mp3""#);
        assert_eq!(source_text(&converted).as_deref(), Some("gone"));
        let PlaybackError::Open { path: got, source } = &converted else {
            panic!("Open expected: {converted:?}");
        };
        assert_eq!(got, &path);
        assert_eq!(source.kind(), io::ErrorKind::NotFound);

        let probe = ProbeError::UnsupportedInput {
            path: path.clone(),
            reason: "not a regular file".into(),
        };
        let text = r#"cannot play "/music/a.mp3": not a regular file"#;
        assert_eq!(probe.to_string(), text);
        let converted = PlaybackError::from(probe);
        assert_eq!(converted.to_string(), text);
        assert!(source_text(&converted).is_none());
        let PlaybackError::UnsupportedInput { path: got, reason } = &converted else {
            panic!("UnsupportedInput expected: {converted:?}");
        };
        assert_eq!((got, reason.as_str()), (&path, "not a regular file"));

        let probe = ProbeError::Decode(symphonia::core::errors::Error::DecodeError("bad frame"));
        let decode_source = source_text(&probe);
        assert_eq!(probe.to_string(), "cannot decode media");
        let converted = PlaybackError::from(probe);
        assert_eq!(converted.to_string(), "cannot decode media");
        assert!(matches!(converted, PlaybackError::Decode(_)));
        assert_eq!(source_text(&converted), decode_source);
        assert!(decode_source.is_some());

        let probe = ProbeError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "short"));
        assert_eq!(probe.to_string(), "terminal I/O error");
        let converted = PlaybackError::from(probe);
        assert_eq!(converted.to_string(), "terminal I/O error");
        assert_eq!(source_text(&converted).as_deref(), Some("short"));
        let PlaybackError::Io(source) = &converted else {
            panic!("Io expected: {converted:?}");
        };
        assert_eq!(source.kind(), io::ErrorKind::UnexpectedEof);
    }
}
