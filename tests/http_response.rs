use tenuto::http::error::{Operation, RangeRejection, RedirectRejection, RemoteFailure};
use tenuto::http::limits::Limits;
use tenuto::http::response::{
    Accepted, ByteRange, Established, Headers, Validator, accept, accept_redirect, if_range_value,
    parse_content_range,
};
use url::Url;

fn headers(pairs: &[(&str, &str)]) -> Headers {
    Headers::from_pairs(pairs)
}

#[allow(clippy::unwrap_used)] // A literal absolute URL always parses.
fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

#[test]
fn a_well_formed_content_range_parses_into_an_inclusive_interval() {
    assert_eq!(
        parse_content_range("bytes 0-1023/8192"),
        Ok(ByteRange {
            first: 0,
            last: 1023,
            total: Some(8192)
        })
    );
    // An unknown total is legal and does not make the response unusable.
    assert_eq!(
        parse_content_range("bytes 512-1023/*"),
        Ok(ByteRange {
            first: 512,
            last: 1023,
            total: None
        })
    );
}

#[test]
fn a_reversed_interval_is_rejected_rather_than_normalized() {
    assert_eq!(
        parse_content_range("bytes 900-100/8192"),
        Err(RangeRejection::ReversedInterval)
    );
}

#[test]
fn garbage_and_non_byte_units_are_malformed() {
    for value in ["", "bytes", "items 0-10/20", "bytes 0-10", "bytes a-b/20"] {
        assert_eq!(
            parse_content_range(value),
            Err(RangeRejection::Malformed),
            "{value:?} must be malformed"
        );
    }
}

#[test]
fn a_206_at_the_requested_start_establishes_ranged_access() {
    let accepted = accept(
        206,
        &headers(&[
            ("content-range", "bytes 0-1023/8192"),
            ("content-length", "1024"),
        ]),
        0,
        true,
        None,
    );
    assert_eq!(
        accepted,
        Ok(Accepted::Ranged {
            range: ByteRange {
                first: 0,
                last: 1023,
                total: Some(8192)
            }
        })
    );
}

#[test]
fn a_206_at_the_wrong_start_fails_instead_of_installing_bytes_at_that_offset() {
    // H7. Silently accepting these bytes at offset 4096 corrupts every media
    // timestamp that follows, with nothing anywhere reporting an error.
    let accepted = accept(
        206,
        &headers(&[("content-range", "bytes 0-1023/8192")]),
        4096,
        false,
        None,
    );
    assert_eq!(
        accepted,
        Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::WrongStart
        })
    );
}

#[test]
fn a_200_is_sequential_data_only_at_the_origin() {
    assert_eq!(
        accept(200, &headers(&[("content-length", "8192")]), 0, true, None),
        Ok(Accepted::Sequential { len: Some(8192) })
    );
    // The same 200 answering a nonzero range never installs those bytes there.
    assert_eq!(
        accept(
            200,
            &headers(&[("content-length", "8192")]),
            4096,
            false,
            None
        ),
        Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::RangeIgnored
        })
    );
}

#[test]
fn a_multipart_range_response_is_refused() {
    assert_eq!(
        accept(
            206,
            &headers(&[("content-type", "multipart/byteranges; boundary=x")]),
            0,
            true,
            None,
        ),
        Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::Multipart
        })
    );
}

#[test]
fn a_body_length_that_contradicts_the_interval_is_rejected() {
    assert_eq!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/8192"),
                ("content-length", "999")
            ]),
            0,
            true,
            None,
        ),
        Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::LengthMismatch
        })
    );
}

#[test]
fn a_total_that_conflicts_with_the_established_one_is_a_resource_change() {
    let established = Established {
        url: url("https://example.com/a.mp3"),
        total: Some(8192),
        validator: Validator {
            strong_etag: Some("\"v1\"".into()),
            weak_etag: None,
            last_modified: None,
        },
    };
    assert_eq!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/9000"),
                ("content-length", "1024")
            ]),
            0,
            false,
            Some(&established),
        ),
        Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::ConflictingTotal
        })
    );
}

#[test]
fn a_changed_strong_validator_fails_as_resource_changed() {
    // H11. The length can be identical; the validator is what settles it.
    let established = Established {
        url: url("https://example.com/a.mp3"),
        total: Some(8192),
        validator: Validator {
            strong_etag: Some("\"v1\"".into()),
            weak_etag: None,
            last_modified: None,
        },
    };
    assert_eq!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/8192"),
                ("content-length", "1024"),
                ("etag", "\"v2\""),
            ]),
            0,
            false,
            Some(&established),
        ),
        Err(RemoteFailure::ResourceChanged)
    );
}

#[test]
fn weak_and_last_modified_validators_are_compared_even_though_they_are_never_sent() {
    // §7: without a strong validator, range access is *best effort* — "compare
    // available length and validator metadata". Best effort forbids claiming an
    // unchanged weak validator proves sameness; it does not license throwing
    // the metadata away, which would let a same-length replacement through
    // silently.
    let weak = Established {
        url: url("https://example.com/a.mp3"),
        total: Some(8192),
        validator: Validator {
            strong_etag: None,
            weak_etag: Some("W/\"v1\"".into()),
            last_modified: None,
        },
    };
    assert_eq!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/8192"),
                ("content-length", "1024"),
                ("etag", "W/\"v2\""),
            ]),
            0,
            false,
            Some(&weak),
        ),
        Err(RemoteFailure::ResourceChanged)
    );

    let dated = Established {
        url: url("https://example.com/a.mp3"),
        total: Some(8192),
        validator: Validator {
            strong_etag: None,
            weak_etag: None,
            last_modified: Some("Tue, 09 Sep 2026 00:00:00 GMT".into()),
        },
    };
    assert_eq!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/8192"),
                ("content-length", "1024"),
                ("last-modified", "Wed, 10 Sep 2026 00:00:00 GMT"),
            ]),
            0,
            false,
            Some(&dated),
        ),
        Err(RemoteFailure::ResourceChanged)
    );

    // An absent validator on either side is no evidence and must not fail.
    assert!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/8192"),
                ("content-length", "1024")
            ]),
            0,
            false,
            Some(&dated),
        )
        .is_ok()
    );
}

#[test]
fn a_reopened_origin_whose_length_contradicts_the_established_total_fails() {
    // §7: "compare available length and validator metadata". A range-less
    // source reopened at byte zero has no Content-Range to check, so the
    // declared length is the only evidence there is — and a server with no
    // validator headers offers nothing else.
    let established = Established {
        url: url("https://example.com/a.mp3"),
        total: Some(8192),
        validator: Validator {
            strong_etag: None,
            weak_etag: None,
            last_modified: None,
        },
    };
    assert_eq!(
        accept(
            200,
            &headers(&[("content-length", "9000")]),
            0,
            true,
            Some(&established)
        ),
        Err(RemoteFailure::ResourceChanged)
    );
    // The same length is not evidence of anything, and must still be accepted:
    // §7 forbids claiming to detect same-length replacement.
    assert_eq!(
        accept(
            200,
            &headers(&[("content-length", "8192")]),
            0,
            true,
            Some(&established)
        ),
        Ok(Accepted::Sequential { len: Some(8192) })
    );
    // And an unknown total on either side is no evidence either way.
    assert!(accept(200, &headers(&[]), 0, true, Some(&established)).is_ok());
}

#[test]
fn an_interval_that_cannot_fit_inside_its_total_is_rejected() {
    // `bytes 0-99/10` describes 100 bytes of a 10-byte object. Accepting it
    // installs bytes past the end of the recording at offsets nothing owns.
    assert_eq!(
        parse_content_range("bytes 0-99/10"),
        Err(RangeRejection::IntervalPastTotal)
    );
    assert_eq!(
        parse_content_range("bytes 10-10/10"),
        Err(RangeRejection::IntervalPastTotal)
    );
    // The last legal byte of a 10-byte object is 9.
    assert_eq!(
        parse_content_range("bytes 9-9/10"),
        Ok(ByteRange {
            first: 9,
            last: 9,
            total: Some(10)
        })
    );
}

#[test]
fn an_interval_length_that_overflows_is_malformed_rather_than_wrapping() {
    // In release mode `last - first + 1` wraps to zero here, turning a hostile
    // header into a silently empty interval.
    assert_eq!(
        parse_content_range("bytes 0-18446744073709551615/*"),
        Err(RangeRejection::Malformed)
    );
}

#[test]
fn a_weak_etag_is_never_sent_as_if_range() {
    // RFC 9110 §13.1.5: If-Range takes a strong validator only. Sending a weak
    // one asks the server a question it is entitled to answer wrongly.
    let weak = Validator {
        strong_etag: None,
        weak_etag: Some("W/\"v1\"".into()),
        last_modified: Some("Tue, 09 Sep 2026 00:00:00 GMT".into()),
    };
    assert_eq!(if_range_value(&weak), None);
    let strong = Validator {
        strong_etag: Some("\"v1\"".into()),
        weak_etag: None,
        last_modified: None,
    };
    assert_eq!(if_range_value(&strong), Some("\"v1\"".to_string()));
}

#[test]
fn a_weak_etag_header_is_not_stored_as_a_strong_one() {
    let validator = tenuto::http::response::validator_from(&headers(&[("etag", "W/\"v1\"")]));
    assert_eq!(validator.strong_etag, None);
    // Kept, though: it is comparable even when it is not sendable.
    assert_eq!(validator.weak_etag.as_deref(), Some("W/\"v1\""));
}

#[test]
fn a_non_identity_encoding_is_refused_so_offsets_stay_media_bytes() {
    assert_eq!(
        accept(
            200,
            &headers(&[("content-encoding", "gzip")]),
            0,
            true,
            None
        ),
        Err(RemoteFailure::NonIdentityEncoding {
            encoding: "gzip".into()
        })
    );
    // `identity` spelled out explicitly is fine, as is its absence.
    assert!(
        accept(
            200,
            &headers(&[("content-encoding", "identity")]),
            0,
            true,
            None
        )
        .is_ok()
    );
    assert!(accept(200, &headers(&[]), 0, true, None).is_ok());
}

#[test]
fn a_416_is_reported_as_unsatisfiable_rather_than_as_completion() {
    // H7/H8: a 416 is never track completion.
    assert_eq!(
        accept(416, &headers(&[]), 4096, false, None),
        Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::Unsatisfiable
        })
    );
}

#[test]
fn ordinary_status_failures_carry_the_status() {
    for status in [401u16, 403, 404, 500, 503] {
        assert!(matches!(
            accept(status, &headers(&[]), 0, true, None),
            Err(RemoteFailure::Status { status: got, .. }) if got == status
        ));
    }
}

#[test]
fn redirects_are_bounded_checked_and_never_downgraded() {
    let limits = Limits::default();
    let from = url("https://a.example/x.mp3");

    assert_eq!(
        accept_redirect(&from, "https://b.example/y.mp3", 1, &[], &limits),
        Ok(url("https://b.example/y.mp3"))
    );
    // Relative locations resolve against the current URL.
    assert_eq!(
        accept_redirect(&from, "/z.mp3", 1, &[], &limits),
        Ok(url("https://a.example/z.mp3"))
    );
    assert_eq!(
        accept_redirect(&from, "http://b.example/y.mp3", 1, &[], &limits),
        Err(RemoteFailure::Redirect {
            reason: RedirectRejection::Downgrade
        })
    );
    assert_eq!(
        accept_redirect(&from, "ftp://b.example/y.mp3", 1, &[], &limits),
        Err(RemoteFailure::Redirect {
            reason: RedirectRejection::UnsupportedScheme
        })
    );
    assert_eq!(
        accept_redirect(&from, "::::", 1, &[], &limits),
        Err(RemoteFailure::Redirect {
            reason: RedirectRejection::InvalidLocation
        })
    );
    assert_eq!(
        accept_redirect(&from, "https://b.example/y.mp3", 6, &[], &limits),
        Err(RemoteFailure::Redirect {
            reason: RedirectRejection::TooMany
        })
    );
    assert_eq!(
        accept_redirect(
            &from,
            "https://a.example/x.mp3",
            2,
            &[url("https://a.example/x.mp3")],
            &limits
        ),
        Err(RemoteFailure::Redirect {
            reason: RedirectRejection::Loop
        })
    );
}

#[test]
fn an_http_origin_may_redirect_to_http() {
    // Only *downgrade* is refused. A plain-HTTP source that was already plain
    // HTTP loses nothing by staying there.
    let limits = Limits::default();
    assert_eq!(
        accept_redirect(
            &url("http://a.example/x.mp3"),
            "http://b.example/y.mp3",
            1,
            &[],
            &limits
        ),
        Ok(url("http://b.example/y.mp3"))
    );
}

fn icy(extra: &[(&str, &str)]) -> Headers {
    let mut pairs = vec![("icy-name", "Test Radio"), ("icy-br", "128")];
    pairs.extend_from_slice(extra);
    Headers::from_pairs(&pairs)
}

#[test]
fn an_icy_200_at_origin_is_live_whatever_length_it_declares() {
    assert_eq!(accept(200, &icy(&[]), 0, true, None), Ok(Accepted::Live));
    assert_eq!(
        accept(200, &icy(&[("content-length", "1000")]), 0, true, None),
        Ok(Accepted::Live)
    );
}

#[test]
fn an_icy_206_from_zero_is_live_and_from_anywhere_else_is_a_wrong_start() {
    let ranged = icy(&[("content-range", "bytes 0-999/1000")]);
    assert_eq!(accept(206, &ranged, 0, true, None), Ok(Accepted::Live));
    let later = icy(&[("content-range", "bytes 5-999/1000")]);
    assert_eq!(
        accept(206, &later, 5, false, None),
        Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::WrongStart
        })
    );
}

#[test]
fn metadata_framing_is_refused_until_it_can_be_demultiplexed() {
    assert_eq!(
        accept(200, &icy(&[("icy-metaint", "16000")]), 0, true, None),
        Err(RemoteFailure::IcyFramingUnsupported)
    );
}

#[test]
fn a_zero_or_unparseable_metaint_means_no_framing_and_the_station_is_live() {
    // StreamGuys answers every client that did not ask for metadata with
    // `icy-metaint: 0`: the header is present, the interval is not. Only a
    // positive interval means the body is interleaved.
    for value in ["0", "00", "junk", ""] {
        assert_eq!(
            accept(200, &icy(&[("icy-metaint", value)]), 0, true, None),
            Ok(Accepted::Live),
            "icy-metaint: {value:?}"
        );
    }
}

#[test]
fn an_hls_playlist_is_refused_before_any_probe() {
    let headers = Headers::from_pairs(&[("content-type", "application/vnd.apple.mpegurl")]);
    assert_eq!(
        accept(200, &headers, 0, true, None),
        Err(RemoteFailure::UnsupportedLiveMedia)
    );
}

#[test]
fn an_error_status_is_never_live_whatever_icy_headers_it_carries() {
    for status in [404u16, 429, 503] {
        assert_eq!(
            accept(status, &icy(&[]), 0, true, None),
            Err(RemoteFailure::Status {
                status,
                operation: Operation::Open
            })
        );
    }
}
