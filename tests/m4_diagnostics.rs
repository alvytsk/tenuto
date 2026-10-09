#![cfg(target_os = "linux")]

//! §7.2's redaction rule, audited through the code paths that actually
//! construct these errors.
//!
//! Nothing here hand-builds a `FeedError` out of already-clean text. Every
//! case drives a real operation — `library::subscribe` against a real
//! loopback server, `library::refresh` against a real cache, a real
//! `state.json` on disk — and then reads whatever error that operation chose
//! to return. An error that was safe only because the test wrote its fields
//! would prove nothing about the constructor, which is the thing that has to
//! stay safe.
//!
//! Three separate policies are under audit here, and they are deliberately
//! not the same policy:
//!
//! * **Transport URLs are secret.** A signed query and embedded userinfo are
//!   where a bearer token hides, so every URL reaching a message has passed
//!   through [`tenuto::telemetry::redact_url`] first — under `Debug` as
//!   much as `Display`, because `main.rs` logs `?error`.
//! * **File content is never quoted.** A `serde_json::Error`'s own `Display`
//!   can echo the offending bytes, and a checkpoint key or a cached
//!   `media_id` is untrusted text that may itself be a URL. Both decoders
//!   reduce it to a category plus line and column.
//! * **Titles, explicit aliases and declaration labels are user-visible
//!   content.** They are what the listener asked to see, or what the feed
//!   claimed about itself, and hiding them would make `unknown feed: …`,
//!   `episode 2 "…" has no audio` and `unsupported feed encoding: …`
//!   useless. They are escaped at the formatting boundary
//!   (`telemetry::displayable`) rather than redacted, and this file asserts
//!   they survive — otherwise "no secret in the message" could be passed by
//!   a message that says nothing at all. None of the three is ever a URL.

#[path = "support/process.rs"]
mod process;
mod support;

#[path = "support/feeds.rs"]
mod feeds;

use support::server::{Script, TestServer};
use tenuto::{
    feed::error::FeedError,
    http::{
        document::CacheValidators,
        error::{RedirectRejection, RemoteFailure},
        limits::Limits,
        service::HttpService,
    },
    library::{self, RefreshOutcome},
    persistence::PersistenceError,
};

type Fallible = Result<(), Box<dyn std::error::Error>>;

/// A URL carrying both shapes of secret §11 keeps out of diagnostics: a
/// bearer-ish query parameter and embedded userinfo.
const SIGNED: &str = "https://user:secret@example.org/feed?token=SECRETVALUE";

/// The same marker inside text that does not parse as a URL at all. The
/// unparseable text may itself be the secret, so it is replaced wholesale
/// rather than echoed.
const MALFORMED: &str = "ht tp:/ /example.org/feed?token=SECRETVALUE";

/// The audit's core assertion, verbatim from the task brief.
fn assert_no_transport_secret(error: &(impl std::fmt::Display + std::fmt::Debug)) {
    for text in [format!("{error}"), format!("{error:?}")] {
        assert!(!text.contains("SECRETVALUE"), "query leaked: {text}");
        assert!(!text.contains("user:secret"), "credentials leaked: {text}");
    }
}

/// The same assertion applied to every error in a chain, not only the one on
/// top. `main.rs` prints `{error}` and logs `?error`, and `FeedError`'s
/// `Remote` and `Persistence` arms are `#[error(transparent)]` — so a nested
/// source that leaked would reach both without the outer `Display` ever
/// showing it.
fn assert_chain_is_clean(error: &dyn std::error::Error) {
    let mut current = Some(error);
    let mut depth = 0;
    while let Some(link) = current {
        assert!(
            !format!("{link}").contains("SECRETVALUE")
                && !format!("{link:?}").contains("SECRETVALUE"),
            "query leaked at source depth {depth}: {link} / {link:?}"
        );
        assert!(
            !format!("{link}").contains("user:secret")
                && !format!("{link:?}").contains("user:secret"),
            "credentials leaked at source depth {depth}: {link} / {link:?}"
        );
        current = link.source();
        depth += 1;
    }
}

fn both(error: &FeedError) {
    assert_no_transport_secret(error);
    assert_chain_is_clean(error);
}

fn text(error: &FeedError) -> String {
    format!("{error} :: {error:?}")
}

// --- Input rejection --------------------------------------------------

/// §6.7's preflight runs before any request, so this is the one rejection a
/// signed URL reaches without the transport ever seeing it. The host and
/// path survive, because a diagnostic that named nothing would be unusable.
#[test]
fn a_signed_subscribe_url_is_refused_without_echoing_it() -> Fallible {
    let rig = feeds::Rig::new()?;
    let service = HttpService::spawn(Limits::brisk())?;

    let error = match service.handle().block_on(library::subscribe(
        &service, &rig.subs, &rig.cache, SIGNED, None,
    )) {
        Err(error) => error,
        Ok(outcome) => return Err(format!("a signed URL must be refused, got {outcome:?}").into()),
    };
    both(&error);
    let rendered = text(&error);
    assert!(
        rendered.contains("example.org"),
        "the host must survive redaction: {rendered}"
    );
    assert!(
        rendered.contains("credentials"),
        "the reason must name the credentials: {rendered}"
    );
    assert!(
        matches!(
            error,
            FeedError::Remote(RemoteFailure::InvalidSource { .. })
        ),
        "{error:?}"
    );

    // Nothing was fetched, so nothing was written either.
    assert!(!rig.subs.path().exists());
    Ok(())
}

/// Text that does not parse cannot be partially redacted — there is no query
/// component to strip — so it is replaced entirely.
#[test]
fn an_unparseable_subscribe_url_is_replaced_rather_than_quoted() -> Fallible {
    let rig = feeds::Rig::new()?;
    let service = HttpService::spawn(Limits::brisk())?;

    let error = match service.handle().block_on(library::subscribe(
        &service, &rig.subs, &rig.cache, MALFORMED, None,
    )) {
        Err(error) => error,
        Ok(outcome) => {
            return Err(format!("a malformed URL must be refused, got {outcome:?}").into());
        }
    };
    both(&error);
    assert!(
        text(&error).contains("<unparseable URL>"),
        "{}",
        text(&error)
    );
    Ok(())
}

// --- Redirect rejection ------------------------------------------------

/// §3.2: a rejected `Location` is reported by *category*, never by target.
/// The header value here is entirely attacker-controlled — a feed's server
/// chooses it — so it never reaches a message at all, which is a stronger
/// guarantee than redacting it would be.
#[test]
fn a_refused_redirect_reports_its_reason_and_never_its_location() -> Fallible {
    let cases = [
        (
            "ftp://user:secret@example.org/feed?token=SECRETVALUE",
            RedirectRejection::UnsupportedScheme,
        ),
        // RFC 3986: a location opening with ':' has a malformed scheme.
        (":SECRETVALUE", RedirectRejection::InvalidLocation),
    ];

    for (location, expected) in cases {
        let rig = feeds::Rig::new()?;
        let server = TestServer::start(Script::serving(b"<rss/>".to_vec()).redirect_to(location));
        let service = HttpService::spawn(Limits::brisk())?;
        let url = server.url("/feed");

        let result = service.handle().block_on(library::subscribe(
            &service, &rig.subs, &rig.cache, &url, None,
        ));
        server.shutdown();

        let error = match result {
            Err(error) => error,
            Ok(outcome) => {
                return Err(format!("{location} must be refused, got {outcome:?}").into());
            }
        };
        both(&error);
        match &error {
            FeedError::Remote(RemoteFailure::Redirect { reason }) => {
                assert_eq!(*reason, expected, "{location}");
            }
            other => return Err(format!("{location}: expected a Redirect, got {other:?}").into()),
        }
    }
    Ok(())
}

// --- Transport failure -------------------------------------------------

/// `transport_detail` is the one place in the HTTP layer where a *live*
/// request URL reaches human-readable text: it appends `error.url()` to
/// reqwest's own message. Both of its branches are driven here from a
/// subscription whose `fetch_url` genuinely carries a signed query.
///
/// * A connection refused on loopback — the server is started and shut down
///   so the port is real and closed — produces a reqwest error that *does*
///   carry the URL, which is the branch that has to redact.
/// * An `ETag` containing a raw CR/LF, which `HeaderValue` refuses and
///   reqwest defers to `build()`, produces one that carries none. That is the
///   same injection `m4_document_protocol.rs` already uses, and it covers the
///   `None` branch — a message that names nothing at all is also safe.
///
/// Each iteration asserts the branch it is *for*, not merely that the result
/// is safe. Both servers are shut down before the refresh, so a
/// connection-refused error would satisfy `Transport { .. }` in either
/// iteration; without the `!contains("127.0.0.1")` below, a reqwest change
/// that stopped rejecting the CR/LF header would silently run the redacting
/// branch twice and leave the `None` branch unaudited while the suite stayed
/// green.
#[test]
fn a_transport_failure_names_the_host_and_redacts_the_query() -> Fallible {
    let mut renderings = Vec::new();
    for bad_validator in [false, true] {
        let rig = feeds::Rig::new()?;
        let server = TestServer::start(Script::serving(b"<rss/>".to_vec()));
        let signed = format!("{}?token=SECRETVALUE", server.url("/feed"));
        let subscription = rig.seed(
            b"<rss><channel><title>T</title><item><guid>a</guid></item></channel></rss>",
            &signed,
        )?;
        if bad_validator {
            let mut cached = rig.cache.read(&subscription)?;
            cached.validators = CacheValidators {
                url: subscription.fetch_url.clone(),
                etag: Some("bad\r\nvalue".to_string()),
                last_modified: None,
            };
            rig.cache.save(&subscription, &cached)?;
        }
        // Closed before the refresh either way: the refused connection is
        // this case's fault, and the builder case must not reach a live
        // server behind the header it cannot encode.
        server.shutdown();

        let service = HttpService::spawn(Limits::brisk())?;
        let outcome = service.handle().block_on(library::refresh(
            &service,
            &rig.subs,
            &rig.cache,
            &subscription.slug,
        ))?;

        let error = match outcome {
            RefreshOutcome::Failed { error, .. } => error,
            other => return Err(format!("expected a failed refresh, got {other:?}").into()),
        };
        both(&error);
        let rendered = text(&error);
        assert!(
            matches!(error, FeedError::Remote(RemoteFailure::Transport { .. })),
            "{error:?}"
        );
        if bad_validator {
            // `build()` failed before a request existed, so reqwest's error
            // carries no URL and `transport_detail` has nothing to append.
            // This is what distinguishes this iteration from the other one:
            // a connection-refused error would name the host here.
            assert!(
                !rendered.contains("127.0.0.1"),
                "the builder branch must carry no URL at all: {rendered}"
            );
        } else {
            assert!(
                rendered.contains("127.0.0.1"),
                "the host must survive redaction: {rendered}"
            );
            assert!(
                !rendered.contains("?token"),
                "the query must not survive: {rendered}"
            );
        }
        renderings.push(rendered);
    }

    // Belt and braces: the two iterations must have produced genuinely
    // different messages, so neither can be the other one run twice.
    assert_ne!(
        renderings[0], renderings[1],
        "both transport branches rendered identically: {renderings:?}"
    );
    Ok(())
}

// --- Deserialization ---------------------------------------------------

/// §5.6/§7.2: a cached `media_id` is untrusted text that may be a URL, and
/// `serde_json`'s own `Display` quotes exactly the token that failed — here
/// wrapping a `DomainError` that quotes it a second time. The cache decoder
/// reduces the whole thing to a category and a position, so the message says
/// *where* the file is broken without repeating *what* it holds.
///
/// Both refusal tiers are exercised, because they are reached by different
/// code: the first identity never parses at all (serde's path), the second
/// parses perfectly and is then refused by §5.6's semantic validation for
/// belonging to no feed.
#[test]
fn a_cache_whose_media_id_is_refused_reports_where_not_what() -> Fallible {
    let cases = [
        ("unparseable", "remote:not a url SECRETVALUE", true),
        (
            "foreign",
            "remote:https://user:secret@example.org/a.mp3?token=SECRETVALUE",
            false,
        ),
    ];

    for (label, media_id, from_serde) in cases {
        let rig = feeds::Rig::new()?;
        let subscription = rig.seed(
            b"<rss><channel><title>T</title><item><guid>a</guid></item></channel></rss>",
            "https://example.org/feed",
        )?;
        let path = rig.cache.path_for(&subscription.feed_id)?;

        let mut entry: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        entry["episodes"][0]["media_id"] = serde_json::json!(media_id);
        std::fs::write(&path, serde_json::to_vec_pretty(&entry)?)?;

        let state = rig.state.read_snapshot()?;
        let error = match library::list_episodes(&rig.subs, &rig.cache, &state, "radio-t", None) {
            Err(error) => error,
            Ok(rows) => return Err(format!("{label} must be refused, got {rows:?}").into()),
        };
        both(&error);
        let rendered = text(&error);
        assert!(rendered.contains("corrupt cache"), "{label}: {rendered}");
        assert!(
            rendered.contains("run tenuto refresh radio-t"),
            "{label}: the recovery must be named: {rendered}"
        );
        if from_serde {
            assert!(
                rendered.contains("error at line"),
                "{label}: the position must survive: {rendered}"
            );
        } else {
            assert!(
                rendered.contains("not a podcast episode for this feed"),
                "{label}: {rendered}"
            );
        }
    }
    Ok(())
}

/// The same rule on the other file. A checkpoint map key *is* a media
/// identity, so a `state.json` that cannot be decoded is exactly the case
/// where quoting the input would publish one.
#[test]
fn state_deserialization_never_quotes_the_checkpoint_key() -> Fallible {
    let rig = feeds::Rig::new()?;
    let path = rig.state.path().to_path_buf();
    std::fs::create_dir_all(path.parent().ok_or("state.json has a parent")?)?;
    std::fs::write(
        &path,
        br#"{"schema_version":2,"volume":1.0,"checkpoints":{
             "remote:not a url SECRETVALUE":
             {"position":{"secs":1,"nanos":0},"completed":false,"touch_seq":1,
              "updated_at":"2026-09-11T09:14:00Z"}}}"#,
    )?;

    let error = match rig.state.read_snapshot() {
        Err(error) => error,
        Ok(_) => return Err("a broken checkpoint key must be refused".into()),
    };
    assert_no_transport_secret(&error);
    assert_chain_is_clean(&error);
    assert!(
        matches!(error, PersistenceError::Deserialize { .. }),
        "{error:?}"
    );

    // And once more after `FeedError`'s transparent wrapper, which is how
    // `tenuto episodes` actually surfaces it.
    let wrapped = FeedError::from(error);
    both(&wrapped);
    assert!(text(&wrapped).contains("malformed"), "{}", text(&wrapped));
    Ok(())
}

/// `subscriptions.json` is durable user data and its records hold URLs, so
/// its decoder follows the same rule — both for a syntax fault and for a
/// record that parses as JSON and then fails §5.6's validation.
#[test]
fn an_unreadable_subscriptions_file_is_never_quoted_back() -> Fallible {
    let syntactically_broken = br#"{"schema_version":1,"subscriptions":[ this is not json
        "https://user:secret@example.org/feed?token=SECRETVALUE" ]}"#
        .to_vec();
    let semantically_invalid = br#"{"schema_version":1,"subscriptions":[{
        "feed_id":"0123456789abcdef0123456789abcdef","slug":"radio-t","title":null,
        "fetch_url":"ftp://user:secret@example.org/feed?token=SECRETVALUE",
        "added_at":"2026-09-11T09:14:00Z"}]}"#
        .to_vec();

    for (label, bytes) in [
        ("syntax", syntactically_broken),
        ("validation", semantically_invalid),
    ] {
        let rig = feeds::Rig::new()?;
        let path = rig.subs.path().to_path_buf();
        std::fs::create_dir_all(path.parent().ok_or("subscriptions.json has a parent")?)?;
        std::fs::write(&path, &bytes)?;

        let error = match library::list_feeds(&rig.subs, &rig.cache) {
            Err(error) => error,
            Ok(feeds) => return Err(format!("{label} must be refused, got {feeds:?}").into()),
        };
        both(&error);
        assert!(
            matches!(error, FeedError::SubscriptionsUnreadable { .. }),
            "{label}: {error:?}"
        );
        // A read-only listing neither quarantines nor rewrites (§5.1).
        assert_eq!(std::fs::read(&path)?, bytes, "{label}");
    }
    Ok(())
}

// --- Content is not transport ------------------------------------------

/// The counter-assertion that keeps the redaction rule honest. A title and
/// an explicitly requested alias are the listener's own content; a message
/// that hid them would be safe and useless. `SECRETVALUE` here is *not* a
/// secret — it is a string a feed put in a title — and it survives, while
/// the enclosure URL beside it still does not exist in any message.
#[test]
fn titles_and_explicit_aliases_are_content_rather_than_transport() -> Fallible {
    let rig = feeds::Rig::new()?;
    rig.seed(
        b"<rss><channel><title>T</title>\
          <item><guid>a</guid><title>Episode SECRETVALUE</title></item></channel></rss>",
        "https://example.org/feed",
    )?;

    let error = match library::resolve_episode(&rig.subs, &rig.cache, "radio-t", 1) {
        Err(error) => error,
        Ok(pair) => {
            return Err(format!("an item with no enclosure is not playable: {pair:?}").into());
        }
    };
    match &error {
        FeedError::NotPlayable { title, .. } => assert_eq!(title, "Episode SECRETVALUE"),
        other => return Err(format!("expected NotPlayable, got {other:?}").into()),
    }
    assert!(
        format!("{error}").contains("Episode SECRETVALUE"),
        "a title must reach the listener: {error}"
    );

    // The alias half: an explicitly requested slug is echoed exactly, since
    // the listener typed it and a rejection that renamed it would be
    // unactionable.
    let service = HttpService::spawn(Limits::brisk())?;
    let taken = match service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        "https://example.org/other",
        Some("radio-t"),
    )) {
        Err(error) => error,
        Ok(outcome) => return Err(format!("a taken slug must be refused, got {outcome:?}").into()),
    };
    assert!(
        matches!(taken, FeedError::SlugTaken { ref slug } if slug == "radio-t"),
        "{taken:?}"
    );

    let invalid = match service.handle().block_on(library::subscribe(
        &service,
        &rig.subs,
        &rig.cache,
        "https://example.org/other",
        Some("Радио Т"),
    )) {
        Err(error) => error,
        Ok(outcome) => {
            return Err(format!("an invalid slug must be refused, got {outcome:?}").into());
        }
    };
    assert!(
        format!("{invalid}").contains("Радио Т"),
        "the requested alias must be echoed: {invalid}"
    );
    Ok(())
}

/// The third kind of echoed content, and the one most easily mistaken for a
/// transport value because it arrives from the network.
///
/// An `encoding` label reaches `UnsupportedEncoding` **because**
/// `BytesDecl::encoder()` did not recognize it — the lookup is what proves
/// the value is unknown, not what constrains it — so what is echoed is
/// arbitrary text the feed wrote, exactly like a title. It is safe for the
/// same reason a title is: it is not a URL, it carries no credential, and a
/// message that hid it would leave the listener unable to see what their feed
/// actually claimed. The marker here is a string in a declaration, not a
/// secret, and it survives on purpose.
///
/// §8.5's stated scope — URLs, credentials, query strings — says nothing
/// about control characters, and that silence used to be a gap: quick-xml
/// does not enforce XML 1.0's `Char` production on this attribute, so
/// nothing stopped a feed from putting an ANSI escape (ESC, 0x1B) into the
/// label and having it printed raw to a terminal via `main.rs`'s `{error}`,
/// to `commands.rs`'s refresh summary, and to `tracing`'s log line. Because
/// none of those three call sites shares a formatting boundary,
/// `feed::parse::sanitize_declaration_label` filters the label to ASCII
/// graphic characters and bounds its length **at construction**, before it
/// is ever placed in the field, rather than at any one display site — see
/// [`a_control_character_in_the_encoding_label_never_reaches_a_terminal`]
/// below.
#[test]
fn an_unsupported_encoding_label_is_echoed_exactly_as_the_feed_wrote_it() -> Fallible {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::serving(
        b"<?xml version=\"1.0\" encoding=\"x-SECRETVALUE\"?>\
          <rss version=\"2.0\"><channel><title>T</title></channel></rss>"
            .to_vec(),
    ));
    let service = HttpService::spawn(Limits::brisk())?;
    let url = server.url("/feed");

    let result = service.handle().block_on(library::subscribe(
        &service, &rig.subs, &rig.cache, &url, None,
    ));
    server.shutdown();

    let error = match result {
        Err(error) => error,
        Ok(outcome) => {
            return Err(format!("an unknown encoding must be refused, got {outcome:?}").into());
        }
    };
    match &error {
        FeedError::UnsupportedEncoding { label } => assert_eq!(label, "x-SECRETVALUE"),
        other => return Err(format!("expected UnsupportedEncoding, got {other:?}").into()),
    }
    assert!(
        format!("{error}").contains("x-SECRETVALUE"),
        "the declared label must reach the listener: {error}"
    );
    // Content, not transport: nothing of the *request* leaked alongside it.
    assert!(
        !format!("{error}").contains("127.0.0.1"),
        "an encoding failure must not carry the request URL: {error}"
    );
    Ok(())
}

/// The gap §8.5 is silent on: a declaration label is content, but content
/// that can carry a raw ANSI escape is not safe to print unfiltered, and
/// nothing in `BytesDecl::encoding()` rules that out. This drives a real
/// `subscribe` against a declaration carrying an ESC byte (0x1B) and checks
/// both `Display` and `Debug` — redaction has to hold under both — for the
/// raw control byte.
#[test]
fn a_control_character_in_the_encoding_label_never_reaches_a_terminal() -> Fallible {
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::serving(
        b"<?xml version=\"1.0\" encoding=\"x-\x1b[31mPWNED\"?>\
          <rss version=\"2.0\"><channel><title>T</title></channel></rss>"
            .to_vec(),
    ));
    let service = HttpService::spawn(Limits::brisk())?;
    let url = server.url("/feed");

    let result = service.handle().block_on(library::subscribe(
        &service, &rig.subs, &rig.cache, &url, None,
    ));
    server.shutdown();

    let error = match result {
        Err(error) => error,
        Ok(outcome) => {
            return Err(format!("an unknown encoding must be refused, got {outcome:?}").into());
        }
    };
    assert!(
        matches!(error, FeedError::UnsupportedEncoding { .. }),
        "{error:?}"
    );
    assert!(
        !format!("{error}").contains('\x1b'),
        "an ESC byte must never reach Display: {error:?}"
    );
    assert!(
        !format!("{error:?}").contains('\x1b'),
        "an ESC byte must never reach Debug: {error:?}"
    );
    // The rest of the label is still visible: sanitizing is not redacting.
    assert!(
        format!("{error}").contains("PWNED"),
        "the printable remainder of the label must still reach the listener: {error}"
    );
    Ok(())
}

// --- The variant table -------------------------------------------------

/// Which constructor supplies each [`FeedError`] variant's context, and why
/// that context is safe. Exhaustive by construction: the match below has no
/// wildcard arm, so a new variant cannot be added without a decision being
/// recorded here.
///
/// | Variant | Context | Built by | Why it is safe |
/// |---|---|---|---|
/// | `Encoding` | none | `feed::parse` | No payload at all. |
/// | `UnsupportedEncoding` | `label` | `feed::parse` | Feed-controlled **content**, classified with titles rather than with transport. `src/feed/parse.rs` echoes the label precisely *because* `BytesDecl::encoder()` returned `None` for it, so the value is arbitrary text the feed wrote — but unlike a title, it never reaches `telemetry::displayable`'s formatting-boundary escaping, since `main.rs` and `tracing` print it directly. §8.5's scope (URLs, credentials, query strings) says nothing about control characters, and quick-xml does not enforce XML 1.0's `Char` production on this attribute, so the label could otherwise carry a raw ANSI escape. `sanitize_declaration_label` filters it to ASCII graphic characters and bounds its length *at construction*, before the field exists, so it is safe both because it is a declaration label rather than a URL and because it cannot contain a control byte, and useful because the listener still sees what was claimed. |
/// | `UnsupportedFormat` | none | `feed::parse` | No payload at all. |
/// | `Malformed` | `detail` | `feed::parse`, `commands` | A fixed phrase or a `quick-xml` position; never a document excerpt (`m4_feed_parse.rs` asserts this). |
/// | `NotPlayable` | `slug`, `index`, `title` | `library::resolve_episode` | A validated slug, an integer, and a title — content the listener asked to see, escaped at the formatting boundary. |
/// | `UnknownSlug` | `slug` | `library::find_subscription` | The slug the listener typed. |
/// | `IndexOutOfRange` | `slug`, `index`, `retained` | `library::resolve_episode` | Integers and a slug. |
/// | `CacheMissing` | `slug` | `feed::cache::CacheStore::read` | A slug. |
/// | `CacheCorrupt` | `slug`, `detail` | `feed::cache::malformed` | A serde *category* plus line and column; the error's own `Display`, which quotes the file, is dropped. |
/// | `CacheParserMismatch` | `slug`, versions | `feed::cache::check_versions` | Integers from the envelope. |
/// | `SubscriptionsUnreadable` | `reason` | `subscription::store`, `library::load_mutating` | A fixed phrase, a quarantine path this process chose, or the same category/line/column reduction. |
/// | `StationsUnreadable` | `reason` | `station::store` | A fixed phrase, a quarantine path this process chose, or the same category/line/column reduction — the same shape as `SubscriptionsUnreadable`, never the file's raw bytes. |
/// | `SubscriptionsBusy` | — | `library::lock_subscriptions` | Nothing: a fixed phrase. |
/// | `InvalidSlug` | `slug` | `subscription::model::validate_slug` | The alias the listener typed. |
/// | `SlugTaken` | `slug` | `library::subscribe` | A stored, already-validated slug. |
/// | `AlreadySubscribed` | `slug` | `library::subscribe` | A stored, already-validated slug — deliberately not the URL that matched. |
/// | `BatchIncomplete` | counts | `application::feed_ops::finish_refresh_batch` | Integers. |
/// | `Remote` | transparent | `http` | Every URL-bearing variant holds `redact_url` output, never a live `Url`. |
/// | `Persistence` | transparent | `persistence` | Paths this process chose; `Deserialize` carries no `#[source]`, so serde's quoting cannot reach it. |
#[test]
fn every_feed_error_variant_has_a_recorded_safe_context() {
    fn context(error: &FeedError) -> &'static str {
        match error {
            FeedError::Encoding | FeedError::UnsupportedFormat => "none",
            FeedError::UnsupportedEncoding { .. } => "the label the feed declared",
            FeedError::Malformed { .. } => "a fixed phrase or a parser position",
            FeedError::NotPlayable { .. } => "a slug, an index and a title",
            FeedError::UnknownSlug { .. } | FeedError::InvalidSlug { .. } => "the typed slug",
            FeedError::IndexOutOfRange { .. } => "a slug and two integers",
            FeedError::CacheMissing { .. } => "a slug",
            FeedError::CacheCorrupt { .. } => "a slug and a serde category plus position",
            FeedError::CacheParserMismatch { .. } => "a slug and two versions",
            FeedError::SubscriptionsUnreadable { .. } => "a fixed phrase or a chosen path",
            FeedError::StationsUnreadable { .. } => "a fixed phrase or a chosen path",
            FeedError::SubscriptionsBusy => "none",
            FeedError::SlugTaken { .. } | FeedError::AlreadySubscribed { .. } => "a stored slug",
            FeedError::BatchIncomplete { .. } => "two integers",
            FeedError::Remote(_) => "already-redacted transport text",
            FeedError::Persistence(_) => "a chosen path and a serde category",
        }
    }

    // The wildcard-free `match` above is the guard that actually holds: a new
    // `FeedError` variant fails to compile until it is classified there. The
    // list below is what makes each classification *run*, and checks every
    // rendering for a marker none of them carries.
    let samples = [
        FeedError::Encoding,
        FeedError::UnsupportedEncoding {
            label: "x-nonesuch".into(),
        },
        FeedError::UnsupportedFormat,
        FeedError::Malformed {
            detail: "syntax error at 1:1".into(),
        },
        FeedError::NotPlayable {
            slug: "radio-t".into(),
            index: 2,
            title: "(untitled)".into(),
        },
        FeedError::UnknownSlug {
            slug: "nosuch".into(),
        },
        FeedError::IndexOutOfRange {
            slug: "radio-t".into(),
            index: 99,
            retained: 2,
        },
        FeedError::CacheMissing {
            slug: "radio-t".into(),
        },
        FeedError::CacheCorrupt {
            slug: "radio-t".into(),
            detail: "cache file is malformed (data error at line 1, column 2)".into(),
        },
        FeedError::CacheParserMismatch {
            slug: "radio-t".into(),
            found: 99,
            expected: 1,
        },
        FeedError::SubscriptionsUnreadable {
            reason: "subscriptions file could not be read".into(),
        },
        FeedError::StationsUnreadable {
            reason: "stations file could not be read".into(),
        },
        FeedError::SubscriptionsBusy,
        FeedError::InvalidSlug {
            slug: "Радио Т".into(),
        },
        FeedError::SlugTaken {
            slug: "radio-t".into(),
        },
        FeedError::AlreadySubscribed {
            slug: "radio-t".into(),
        },
        FeedError::BatchIncomplete {
            failed: 1,
            total: 2,
        },
        FeedError::Remote(RemoteFailure::ContinuityUndetermined),
        FeedError::Persistence(PersistenceError::NoStateDirectory),
    ];
    for sample in &samples {
        assert!(!context(sample).is_empty());
        assert_no_transport_secret(sample);
    }
}

// --- Tracing -----------------------------------------------------------

/// `RUST_LOG=tenuto=debug` is the level a listener is told to raise when
/// something goes wrong, so it is also the level a leak would surface at.
///
/// Linux-gated because it drives the real binary through XDG directories;
/// `ProjectDirs` resolves elsewhere on macOS and Windows, where these
/// variables mean nothing.
#[cfg(target_os = "linux")]
#[test]
fn debug_logging_records_no_document_request_or_validator() -> Fallible {
    let root = tempfile::tempdir()?;
    let media = TestServer::start(Script::from_fixture("sine-5s.flac"));
    // Every untrusted field carries the marker: the enclosure URL, the
    // item's identity, the item's title, and the channel title a slug would
    // be derived from.
    let xml = format!(
        "<rss><channel><title>Radio SECRETVALUE</title>\
         <item><guid>guid-SECRETVALUE</guid><title>Episode SECRETVALUE</title>\
         <enclosure url=\"{}?token=SECRETVALUE\" type=\"video/mp4\" length=\"12\"/>\
         </item>\
         <item><guid>guid-two-SECRETVALUE</guid></item></channel></rss>",
        media.url("/audio.flac")
    );
    let feed = TestServer::start(Script::documents(vec![support::server::DocumentReply {
        path: "/feed".to_string(),
        status: 200,
        headers: vec![
            ("ETag".to_string(), "\"v1-SECRETVALUE\"".to_string()),
            (
                "Content-Type".to_string(),
                "application/rss+xml".to_string(),
            ),
        ],
        body: xml.into_bytes(),
        conditional: true,
        header_delay: std::time::Duration::ZERO,
    }]));

    let run = |args: &[&str]| -> std::io::Result<std::process::Output> {
        process::command_in(root.path())
            .args(args)
            .env("RUST_LOG", "tenuto=debug")
            .output()
    };

    let mut logs = String::new();
    let feed_url = feed.url("/feed");
    for args in [
        ["subscribe", feed_url.as_str(), "--as", "radio-t"].as_slice(),
        ["refresh", "radio-t"].as_slice(),
        ["episodes", "radio-t"].as_slice(),
        ["play", "radio-t", "1", "--probe-only"].as_slice(),
        ["feeds"].as_slice(),
    ] {
        let output = run(args)?;
        logs.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    feed.shutdown();
    media.shutdown();

    assert!(
        !logs.is_empty(),
        "debug logging produced nothing to audit; the filter did not take effect"
    );
    assert!(
        logs.contains("resolved episode enclosure diagnostics"),
        "the debug-level feed diagnostics never ran: {logs}"
    );
    assert!(
        !logs.contains("SECRETVALUE"),
        "an untrusted feed field reached the log: {logs}"
    );

    // No whole record is ever logged: a `ParsedItem`, `BoundItem`,
    // `CachedFeed`, `CachedEpisode`, `DocumentRequest` or `CacheValidators`
    // printed under `Debug` would carry a GUID, a title and a URL at once.
    for forbidden in [
        "ParsedItem",
        "BoundItem",
        "CachedFeed",
        "CachedEpisode",
        "DocumentRequest",
        "CacheValidators",
        "Subscription {",
        "etag:",
    ] {
        assert!(
            !logs.contains(forbidden),
            "{forbidden} was logged in full: {logs}"
        );
    }

    // The diagnostics that *are* meant to be there: a declared type that is
    // not audio is a claim worth reporting, and reporting it names only the
    // two already-safe enclosure fields.
    assert!(
        logs.contains("declared enclosure type is not audio/*"),
        "the non-audio warning never fired: {logs}"
    );
    Ok(())
}
