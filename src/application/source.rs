//! Turns the `play` command's positional argument into the
//! `(MediaId, SourceLocation)` pair the rest of the program plays from —
//! resolved on the calling thread, before anything with a network or a
//! worker exists.

use std::path::Path;

use url::Url;

use crate::http::error::RemoteFailure;
use crate::media::id::{AbsolutePath, MediaId, NormalizedUrl};
use crate::media::source::SourceLocation;
use crate::playback::error::PlaybackError;
use crate::telemetry::redact_url;

/// §5's disambiguation. An explicit http/https scheme is a URL; everything
/// else keeps existing path behaviour, so `./https:weird` remains an
/// unambiguous local spelling.
pub fn resolve_source(input: &str) -> Result<(MediaId, SourceLocation), PlaybackError> {
    if is_url_spelling(input) {
        return resolve_url(input);
    }
    resolve_path(Path::new(input))
}

/// The local-file half of [`resolve_source`], for a caller that already
/// holds a path and must not have it reinterpreted as a URL spelling.
pub fn resolve_path(path: &Path) -> Result<(MediaId, SourceLocation), PlaybackError> {
    let path = path.to_path_buf();
    let canonical = path.canonicalize().map_err(|source| PlaybackError::Open {
        path: path.clone(),
        source,
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|source| PlaybackError::Open {
        path: canonical.clone(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(PlaybackError::UnsupportedInput {
            path: canonical,
            reason: "not a regular file".into(),
        });
    }
    let absolute =
        AbsolutePath::new(canonical.clone()).map_err(|error| PlaybackError::UnsupportedInput {
            path: path.clone(),
            reason: error.to_string(),
        })?;
    Ok((
        MediaId::LocalFile(absolute),
        SourceLocation::LocalPath(canonical),
    ))
}

/// Only an explicit prefix counts, ASCII case-insensitively: `./https:weird`
/// does not start with either spelling, so it is unaffected, and there is no
/// looser check anywhere else that could make it one.
pub fn is_url_spelling(input: &str) -> bool {
    let starts_with_ci = |prefix: &str| {
        input
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    starts_with_ci("http://") || starts_with_ci("https://")
}

fn resolve_url(input: &str) -> Result<(MediaId, SourceLocation), PlaybackError> {
    // Ruling 5: every `RemoteFailure` built from user input here carries an
    // already-redacted URL — the raw text may itself be the secret (a
    // malformed URL that embedded a token, say), so it is never echoed back.
    let invalid = |reason: &'static str| -> PlaybackError {
        RemoteFailure::InvalidSource {
            input: redact_url(input),
            reason,
        }
        .into()
    };
    let url = Url::parse(input).map_err(|_| invalid("not a valid URL"))?;
    // §5: no implicit credential feature. Rejected here, before identity is
    // ever built from it, rather than left for the fetch to refuse later.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("URLs with embedded credentials are not supported"));
    }
    let normalized =
        NormalizedUrl::parse(input).map_err(|_| invalid("expected http(s) with a host"))?;
    // The parsed `Url` is kept separately as the fetch target — its query
    // stays whole, and a redirect changes it without ever touching identity.
    Ok((MediaId::RemoteUrl(normalized), SourceLocation::Http(url)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_http_or_https_prefix_is_a_url() {
        assert!(is_url_spelling("http://example.com/a.mp3"));
        assert!(is_url_spelling("https://example.com/a.mp3"));
        // ASCII case-insensitive (§5).
        assert!(is_url_spelling("HTTP://example.com/a.mp3"));
        assert!(is_url_spelling("HtTpS://example.com/a.mp3"));
        // §5: `./https:weird` is an unambiguous local spelling - the scheme
        // has to be a genuine prefix, not merely present anywhere.
        assert!(!is_url_spelling("./https:not-a-url"));
        assert!(!is_url_spelling("https:not-a-url"));
        assert!(!is_url_spelling("/music/http://weird.flac"));
        assert!(!is_url_spelling(""));
    }

    #[test]
    fn a_malformed_url_is_reported_as_invalid_source_not_a_missing_file() {
        let error = match resolve_source("https://") {
            Err(error) => error,
            Ok(_) => panic!("an empty host must not resolve"),
        };
        assert!(
            matches!(
                error,
                PlaybackError::Remote(RemoteFailure::InvalidSource { .. })
            ),
            "{error}"
        );
        assert!(error.to_string().contains("URL"), "{error}");
    }

    #[test]
    fn a_url_with_embedded_credentials_is_rejected_and_the_password_never_appears() {
        let error = match resolve_source("https://alice:hunter2@example.com/a.mp3") {
            Err(error) => error,
            Ok(_) => panic!("credentials must be refused"),
        };
        let message = error.to_string();
        assert!(message.contains("credentials"), "{message}");
        assert!(
            !message.contains("hunter2"),
            "the password leaked: {message}"
        );
    }

    #[test]
    fn a_valid_remote_url_resolves_to_a_remote_media_id_with_the_query_kept_for_fetching() {
        let (media, location) = match resolve_source("https://example.com/a.flac?token=secret") {
            Ok(resolved) => resolved,
            Err(error) => panic!("a well-formed URL must resolve: {error}"),
        };
        assert!(matches!(media, MediaId::RemoteUrl(_)));
        match location {
            SourceLocation::Http(url) => {
                assert_eq!(
                    url.query(),
                    Some("token=secret"),
                    "the fetch URL keeps the query"
                );
            }
            other => panic!("expected an HTTP source location: {other:?}"),
        }
    }

    #[test]
    fn a_local_spelling_that_looks_like_a_url_is_never_parsed_as_one() {
        // §5: `./https:...` stays a path. It will not canonicalize (there is
        // no such file), but the error must be a path error, not a URL one.
        let error = match resolve_source("./https:not-a-url") {
            Err(error) => error,
            Ok(_) => panic!("a nonexistent path must not resolve"),
        };
        assert!(matches!(error, PlaybackError::Open { .. }), "{error}");
        assert!(
            error.to_string().contains("https:not-a-url"),
            "expected the literal path in the diagnostic: {error}"
        );
    }
}
