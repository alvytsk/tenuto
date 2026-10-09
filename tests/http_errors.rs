use tenuto::http::error::{Operation, RangeRejection, RemoteFailure};
use tenuto::telemetry::redact_url;

#[test]
fn a_signed_query_never_reaches_a_diagnostic() {
    let url = "https://cdn.example.com/ep/42.mp3?token=SECRET&Expires=99";
    let redacted = redact_url(url);
    assert!(!redacted.contains("SECRET"), "token leaked: {redacted}");
    assert!(!redacted.contains("Expires"), "query leaked: {redacted}");
    assert!(
        redacted.contains("cdn.example.com") && redacted.contains("/ep/42.mp3"),
        "redaction must stay legible: {redacted}"
    );
}

#[test]
fn userinfo_never_reaches_a_diagnostic() {
    let redacted = redact_url("https://alice:hunter2@example.com/a.mp3");
    assert!(!redacted.contains("hunter2"), "password leaked: {redacted}");
    assert!(!redacted.contains("alice"), "username leaked: {redacted}");
    assert!(redacted.contains("example.com"), "host lost: {redacted}");
}

#[test]
fn an_unparseable_url_redacts_to_a_placeholder_rather_than_itself() {
    // The input may itself be the secret. Echoing it back on the failure path
    // is exactly the leak this function exists to prevent.
    let redacted = redact_url("not a url?token=SECRET");
    assert!(!redacted.contains("SECRET"), "leaked: {redacted}");
}

#[test]
fn every_category_the_spec_names_has_a_distinct_variant() {
    // §11's list, so a later refactor cannot quietly collapse two categories
    // into one and lose the distinction the status line depends on.
    let categories = [
        RemoteFailure::InvalidSource {
            input: redact_url("https://x/y"),
            reason: "no host",
        },
        RemoteFailure::Status {
            status: 503,
            operation: Operation::Open,
        },
        RemoteFailure::Redirect {
            reason: tenuto::http::error::RedirectRejection::TooMany,
        },
        RemoteFailure::Timeout {
            phase: tenuto::http::error::Phase::Headers,
        },
        RemoteFailure::InvalidRange {
            reason: RangeRejection::WrongStart,
        },
        RemoteFailure::ResourceChanged,
        RemoteFailure::TruncatedBody { missing: 17 },
        RemoteFailure::ProbeLimitExceeded { limit: 8 << 20 },
        RemoteFailure::ContinuityUndetermined,
        RemoteFailure::UnsupportedLiveMedia,
        RemoteFailure::SeekUnavailable,
        RemoteFailure::NonIdentityEncoding {
            encoding: "gzip".into(),
        },
        RemoteFailure::Transport {
            operation: Operation::Read,
            detail: "reset".into(),
        },
        RemoteFailure::Cancelled,
    ];
    for (i, a) in categories.iter().enumerate() {
        for b in categories.iter().skip(i + 1) {
            assert_ne!(a, b, "two §11 categories are the same value");
        }
        assert!(!a.to_string().is_empty(), "{a:?} renders empty");
    }
    assert_eq!(categories.len(), 14);
}

#[test]
fn only_failures_that_say_nothing_about_the_location_are_retryable() {
    use tenuto::http::error::{Operation, Phase, RedirectRejection, RemoteFailure};
    let status = |status| RemoteFailure::Status {
        status,
        operation: Operation::Open,
    };
    for failure in [
        RemoteFailure::LiveEnded,
        RemoteFailure::Timeout {
            phase: Phase::Stall,
        },
        RemoteFailure::Transport {
            operation: Operation::Read,
            detail: "reset".into(),
        },
        status(429),
        status(500),
        status(503),
    ] {
        assert!(failure.is_retryable(), "{failure:?}");
    }
    for failure in [
        status(401),
        status(403),
        status(404),
        status(410),
        RemoteFailure::Cancelled,
        RemoteFailure::ResourceChanged,
        RemoteFailure::IcyFramingUnsupported,
        RemoteFailure::UnsupportedLiveMedia,
        RemoteFailure::ContinuityUndetermined,
        RemoteFailure::Redirect {
            reason: RedirectRejection::Loop,
        },
        RemoteFailure::TruncatedBody { missing: 1 },
    ] {
        assert!(!failure.is_retryable(), "{failure:?}");
    }
}
