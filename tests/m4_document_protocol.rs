mod support;

use std::time::Duration;

use support::server::{DocumentReply, Script, TestServer};
use tenuto::http::{
    document::{CacheValidators, DocumentOutcome, DocumentRequest},
    error::{Phase, RedirectRejection, RemoteFailure},
    limits::Limits,
    service::HttpService,
};

fn reply(path: &str, status: u16, headers: Vec<(&str, &str)>, body: &[u8]) -> DocumentReply {
    DocumentReply {
        path: path.to_string(),
        status,
        headers: headers
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.to_vec(),
        conditional: false,
        header_delay: Duration::ZERO,
    }
}

fn conditional_reply(
    path: &str,
    status: u16,
    headers: Vec<(&str, &str)>,
    body: &[u8],
) -> DocumentReply {
    DocumentReply {
        conditional: true,
        ..reply(path, status, headers, body)
    }
}

#[test]
fn permanent_prefix_stops_before_temporary_redirect() -> Result<(), Box<dyn std::error::Error>> {
    let replies = vec![
        DocumentReply {
            path: "/a".into(),
            status: 301,
            headers: vec![("Location".into(), "/b".into())],
            body: vec![],
            conditional: false,
            header_delay: Duration::ZERO,
        },
        DocumentReply {
            path: "/b".into(),
            status: 302,
            headers: vec![("Location".into(), "/c".into())],
            body: vec![],
            conditional: false,
            header_delay: Duration::ZERO,
        },
        DocumentReply {
            path: "/c".into(),
            status: 200,
            headers: vec![],
            body: b"<rss><channel/></rss>".to_vec(),
            conditional: false,
            header_delay: Duration::ZERO,
        },
    ];
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }))?;
    let DocumentOutcome::Fetched {
        final_url,
        permanent_url,
        ..
    } = result
    else {
        panic!("expected 200")
    };
    assert_eq!(final_url.as_str(), server.url("/c"));
    assert_eq!(
        permanent_url.as_ref().map(url::Url::as_str),
        Some(server.url("/b").as_str())
    );
    server.shutdown();
    Ok(())
}

#[test]
fn reversed_chain_freezes_permanent_url_at_the_first_temporary_hop()
-> Result<(), Box<dyn std::error::Error>> {
    // A -302-> B -301-> C: permanent_url must be None (§3.2 table).
    let replies = vec![
        reply("/a", 302, vec![("Location", "/b")], b""),
        reply("/b", 301, vec![("Location", "/c")], b""),
        reply("/c", 200, vec![], b"<rss/>"),
    ];
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }))?;
    let DocumentOutcome::Fetched {
        final_url,
        permanent_url,
        ..
    } = result
    else {
        panic!("expected 200")
    };
    assert_eq!(final_url.as_str(), server.url("/c"));
    assert_eq!(permanent_url, None);
    server.shutdown();
    Ok(())
}

#[test]
fn an_all_permanent_chain_advances_permanent_url_to_the_final_hop()
-> Result<(), Box<dyn std::error::Error>> {
    // A -301-> B -308-> C: permanent_url must be Some(C) (§3.2 table).
    let replies = vec![
        reply("/a", 301, vec![("Location", "/b")], b""),
        reply("/b", 308, vec![("Location", "/c")], b""),
        reply("/c", 200, vec![], b"<rss/>"),
    ];
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }))?;
    let DocumentOutcome::Fetched {
        final_url,
        permanent_url,
        ..
    } = result
    else {
        panic!("expected 200")
    };
    assert_eq!(final_url.as_str(), server.url("/c"));
    assert_eq!(
        permanent_url.as_ref().map(url::Url::as_str),
        Some(server.url("/c").as_str())
    );
    server.shutdown();
    Ok(())
}

#[test]
fn a_redirect_loop_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let replies = vec![
        reply("/a", 302, vec![("Location", "/b")], b""),
        reply("/b", 302, vec![("Location", "/a")], b""),
    ];
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::Redirect {
            reason: RedirectRejection::Loop,
        }) => {}
        other => panic!("expected Redirect {{ reason: Loop }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_redirect_chain_past_the_hop_cap_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    // Limits::brisk()'s max_redirects stays at the default of 5, so a chain
    // of 7 hops (/h0 -> /h1 -> ... -> /h6 -> 200) must overflow.
    let mut replies: Vec<DocumentReply> = (0..6)
        .map(|i| reply(&format!("/h{i}"), 302, vec![], b""))
        .collect();
    for (i, r) in replies.iter_mut().enumerate() {
        r.headers = vec![("Location".to_string(), format!("/h{}", i + 1))];
    }
    replies.push(reply("/h6", 200, vec![], b"<rss/>"));
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/h0").parse()?,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::Redirect {
            reason: RedirectRejection::TooMany,
        }) => {}
        other => panic!("expected Redirect {{ reason: TooMany }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_303_and_a_307_are_both_followed_as_redirects() -> Result<(), Box<dyn std::error::Error>> {
    let replies = vec![
        reply("/a", 303, vec![("Location", "/b")], b""),
        reply("/b", 307, vec![("Location", "/c")], b""),
        reply("/c", 200, vec![], b"<rss/>"),
    ];
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }))?;
    let DocumentOutcome::Fetched { final_url, .. } = result else {
        panic!("expected 200")
    };
    assert_eq!(final_url.as_str(), server.url("/c"));
    server.shutdown();
    Ok(())
}

#[test]
fn a_redirect_with_no_location_header_is_a_status_failure() -> Result<(), Box<dyn std::error::Error>>
{
    let replies = vec![reply("/a", 302, vec![], b"")];
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::Status { status: 302, .. }) => {}
        other => panic!("expected Status {{ status: 302, .. }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn an_unsupported_redirect_scheme_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let replies = vec![reply(
        "/a",
        302,
        vec![("Location", "ftp://example.com/feed.xml")],
        b"",
    )];
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::Redirect {
            reason: RedirectRejection::UnsupportedScheme,
        }) => {}
        other => panic!("expected Redirect {{ reason: UnsupportedScheme }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

// §3.3 validator scope: A redirects to B, and the request only carries
// validators scoped to B. A must get neither conditional header; B must get
// both, and its matching 304 must succeed.
#[test]
fn a_validator_scoped_to_the_final_hop_is_sent_only_there_and_a_matching_304_succeeds()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![
        reply("/a", 302, vec![("Location", "/b")], b""),
        conditional_reply(
            "/b",
            200,
            vec![
                ("ETag", "\"v1\""),
                ("Last-Modified", "Tue, 01 Jan 2030 00:00:00 GMT"),
            ],
            b"<rss/>",
        ),
    ]));
    let service = HttpService::spawn(Limits::brisk())?;
    let b_url: url::Url = server.url("/b").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: Some(CacheValidators {
                url: b_url.clone(),
                etag: Some("\"v1\"".to_string()),
                last_modified: Some("Tue, 01 Jan 2030 00:00:00 GMT".to_string()),
            }),
        }))?;
    let DocumentOutcome::Unchanged {
        final_url,
        validators,
        ..
    } = result
    else {
        panic!("expected 304 Unchanged, got a Fetched result")
    };
    assert_eq!(final_url, b_url);
    assert_eq!(validators.etag.as_deref(), Some("\"v1\""));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/a");
    assert_eq!(requests[0].header("if-none-match"), None);
    assert_eq!(requests[0].header("if-modified-since"), None);
    assert_eq!(requests[1].path, "/b");
    assert_eq!(requests[1].header("if-none-match"), Some("\"v1\""));
    assert_eq!(
        requests[1].header("if-modified-since"),
        Some("Tue, 01 Jan 2030 00:00:00 GMT")
    );
    server.shutdown();
    Ok(())
}

// The mirror case: validators are scoped to A (the origin), which redirects
// to B. B must receive neither conditional header, and B's own unsolicited
// 304 must fail rather than succeed.
#[test]
fn a_validator_scoped_to_a_redirected_origin_is_dropped_and_bs_304_is_unsolicited()
-> Result<(), Box<dyn std::error::Error>> {
    // A's route is an unconditional 302: a real redirect responds with the
    // same Location whatever conditional header it was sent, which is
    // exactly why forwarding a validator across the hop would be unsound.
    let server = TestServer::start(Script::documents(vec![
        reply("/a", 302, vec![("Location", "/b")], b""),
        reply("/b", 304, vec![], b""),
    ]));
    let service = HttpService::spawn(Limits::brisk())?;
    let a_url: url::Url = server.url("/a").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: a_url.clone(),
            validators: Some(CacheValidators {
                url: a_url,
                etag: Some("\"v1\"".to_string()),
                last_modified: None,
            }),
        }));
    match result {
        Err(RemoteFailure::UnsolicitedNotModified) => {}
        other => panic!("expected UnsolicitedNotModified, got {other:?}"),
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/a");
    assert_eq!(requests[0].header("if-none-match"), Some("\"v1\""));
    // B never receives the validator that was scoped to A's URL.
    assert_eq!(requests[1].path, "/b");
    assert_eq!(requests[1].header("if-none-match"), None);
    assert_eq!(requests[1].header("if-modified-since"), None);
    server.shutdown();
    Ok(())
}

#[test]
fn an_absent_validators_record_sends_no_conditional_headers_and_a_200_is_fetched()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![reply("/a", 200, vec![], b"<rss/>")]));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }))?;
    let DocumentOutcome::Fetched { bytes, .. } = result else {
        panic!("expected a body")
    };
    assert_eq!(bytes, b"<rss/>");
    let requests = server.requests();
    assert_eq!(requests[0].header("if-none-match"), None);
    assert_eq!(requests[0].header("if-modified-since"), None);
    server.shutdown();
    Ok(())
}

#[test]
fn a_weak_etag_is_preserved_exactly_across_a_304_merge() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![conditional_reply(
        "/a",
        200,
        vec![("ETag", "W/\"v1\"")],
        b"<rss/>",
    )]));
    let service = HttpService::spawn(Limits::brisk())?;
    let url: url::Url = server.url("/a").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: url.clone(),
            validators: Some(CacheValidators {
                url,
                etag: Some("W/\"v1\"".to_string()),
                last_modified: None,
            }),
        }))?;
    let DocumentOutcome::Unchanged { validators, .. } = result else {
        panic!("expected 304 Unchanged")
    };
    assert_eq!(validators.etag.as_deref(), Some("W/\"v1\""));
    let requests = server.requests();
    assert_eq!(requests[0].header("if-none-match"), Some("W/\"v1\""));
    server.shutdown();
    Ok(())
}

#[test]
fn a_last_modified_only_validator_drives_a_304_via_if_modified_since()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![conditional_reply(
        "/a",
        200,
        vec![("Last-Modified", "Mon, 01 Jan 2024 00:00:00 GMT")],
        b"<rss/>",
    )]));
    let service = HttpService::spawn(Limits::brisk())?;
    let url: url::Url = server.url("/a").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: url.clone(),
            validators: Some(CacheValidators {
                url,
                etag: None,
                last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string()),
            }),
        }))?;
    match result {
        DocumentOutcome::Unchanged { .. } => {}
        other => panic!("expected 304 Unchanged, got {other:?}"),
    }
    let requests = server.requests();
    assert_eq!(requests[0].header("if-none-match"), None);
    assert_eq!(
        requests[0].header("if-modified-since"),
        Some("Mon, 01 Jan 2024 00:00:00 GMT")
    );
    server.shutdown();
    Ok(())
}

#[test]
fn a_304_merges_a_freshly_supplied_etag_into_the_cached_validator_record()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![conditional_reply(
        "/a",
        200,
        vec![
            ("ETag", "\"old\""),
            ("Last-Modified", "Mon, 01 Jan 2024 00:00:00 GMT"),
        ],
        b"<rss/>",
    )]));
    let service = HttpService::spawn(Limits::brisk())?;
    let url: url::Url = server.url("/a").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: url.clone(),
            validators: Some(CacheValidators {
                url,
                etag: Some("\"old\"".to_string()),
                last_modified: Some("Sun, 01 Jan 2023 00:00:00 GMT".to_string()),
            }),
        }))?;
    let DocumentOutcome::Unchanged { validators, .. } = result else {
        panic!("expected 304 Unchanged")
    };
    // The 304 itself carried both headers, so both are merged in, replacing
    // the stale last_modified the cache held (§3.3).
    assert_eq!(validators.etag.as_deref(), Some("\"old\""));
    assert_eq!(
        validators.last_modified.as_deref(),
        Some("Mon, 01 Jan 2024 00:00:00 GMT")
    );
    server.shutdown();
    Ok(())
}

#[test]
fn a_permanent_redirect_ending_in_a_matching_304_reports_the_permanent_url()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![
        reply("/a", 301, vec![("Location", "/b")], b""),
        conditional_reply("/b", 200, vec![("ETag", "\"v1\"")], b"<rss/>"),
    ]));
    let service = HttpService::spawn(Limits::brisk())?;
    let b_url: url::Url = server.url("/b").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: Some(CacheValidators {
                url: b_url.clone(),
                etag: Some("\"v1\"".to_string()),
                last_modified: None,
            }),
        }))?;
    let DocumentOutcome::Unchanged {
        final_url,
        permanent_url,
        ..
    } = result
    else {
        panic!("expected 304 Unchanged")
    };
    assert_eq!(final_url, b_url);
    assert_eq!(permanent_url.as_ref(), Some(&b_url));
    server.shutdown();
    Ok(())
}

#[test]
fn a_200_with_missing_validator_headers_resets_cached_validators_to_none()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![reply("/a", 200, vec![], b"<rss/>")]));
    let service = HttpService::spawn(Limits::brisk())?;
    let url: url::Url = server.url("/a").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: url.clone(),
            validators: Some(CacheValidators {
                url,
                etag: Some("\"stale\"".to_string()),
                last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string()),
            }),
        }))?;
    let DocumentOutcome::Fetched { validators, .. } = result else {
        panic!("expected a body")
    };
    assert_eq!(validators.etag, None);
    assert_eq!(validators.last_modified, None);
    server.shutdown();
    Ok(())
}

#[test]
fn an_empty_validators_record_sends_no_conditional_header_and_reaches_a_200()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![reply("/a", 200, vec![], b"<rss/>")]));
    let service = HttpService::spawn(Limits::brisk())?;
    let url: url::Url = server.url("/a").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: url.clone(),
            validators: Some(CacheValidators {
                url,
                etag: None,
                last_modified: None,
            }),
        }))?;
    match result {
        DocumentOutcome::Fetched { .. } => {}
        other => panic!("expected a Fetched 200, got {other:?}"),
    }
    let requests = server.requests();
    assert_eq!(requests[0].header("if-none-match"), None);
    assert_eq!(requests[0].header("if-modified-since"), None);
    server.shutdown();
    Ok(())
}

// The overall `open` deadline bounds the whole multi-hop operation and is
// never reset per hop: two 60ms header delays exceed a 90ms open budget even
// though each individual hop stays under the 100ms headers deadline.
#[test]
fn the_open_deadline_bounds_the_whole_redirect_chain_not_each_hop()
-> Result<(), Box<dyn std::error::Error>> {
    let replies = vec![
        DocumentReply {
            header_delay: Duration::from_millis(60),
            ..reply("/a", 302, vec![("Location", "/b")], b"")
        },
        DocumentReply {
            header_delay: Duration::from_millis(60),
            ..reply("/b", 200, vec![], b"<rss/>")
        },
    ];
    let server = TestServer::start(Script::documents(replies));
    let limits = Limits {
        headers: Duration::from_millis(100),
        open: Duration::from_millis(90),
        ..Limits::brisk()
    };
    let service = HttpService::spawn(limits)?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::Timeout { phase: Phase::Open }) => {}
        other => panic!("expected Timeout {{ phase: Phase::Open }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_hop_timeout_that_lands_with_the_open_deadline_reports_open()
-> Result<(), Box<dyn std::error::Error>> {
    // Equal budgets wake both timers together, which is what a late runner
    // (the macOS leg) does to the test above: the expiry is still the whole
    // operation's, whichever timer the runtime polled first.
    let replies = vec![DocumentReply {
        header_delay: Duration::from_millis(300),
        ..reply("/a", 200, vec![], b"<rss/>")
    }];
    let server = TestServer::start(Script::documents(replies));
    let limits = Limits {
        headers: Duration::from_millis(100),
        open: Duration::from_millis(100),
        ..Limits::brisk()
    };
    let service = HttpService::spawn(limits)?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: server.url("/a").parse()?,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::Timeout { phase: Phase::Open }) => {}
        other => panic!("expected Timeout {{ phase: Phase::Open }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

// An invalid cached header value (here, one containing a raw CR/LF, which
// `HeaderValue` refuses) must fail safely while building the request rather
// than panicking — reqwest defers the conversion error to `.build()`.
#[test]
fn an_invalid_cached_header_value_fails_safely_rather_than_panicking()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::documents(vec![reply("/a", 200, vec![], b"<rss/>")]));
    let service = HttpService::spawn(Limits::brisk())?;
    let url: url::Url = server.url("/a").parse()?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: url.clone(),
            validators: Some(CacheValidators {
                url,
                etag: Some("bad\r\nvalue".to_string()),
                last_modified: None,
            }),
        }));
    match result {
        Err(RemoteFailure::Transport { .. }) => {}
        other => panic!("expected a Transport failure, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

// The existing pure downgrade test (`tests/http_response.rs`) already
// exercises `accept_redirect`'s HTTPS -> HTTP refusal directly, which is why
// this suite has no TLS-backed downgrade test of its own (per the brief).
