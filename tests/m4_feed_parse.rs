//! §4.1-§4.8: RSS 2.0 and Atom 1.0 parsing through strict decoding and
//! namespace-aware events.
//!
//! Encoded variants are built here rather than checked in as bytes. A fixture
//! file that claims to be Latin-1 while sitting on disk as UTF-8 proves
//! nothing; `utf16(...)` and `splice(...)` below produce the actual bytes.
//! See `tests/fixtures/feeds/README.md`.

use std::time::Duration;

use tenuto::feed::error::FeedError;
use tenuto::feed::parse::{ParseWarning, WarningKind, parse_feed};
use url::Url;

const RSS2_MINIMAL: &[u8] = include_bytes!("fixtures/feeds/rss2-minimal.xml");
const DECL_WITHOUT_ENCODING: &[u8] = include_bytes!("fixtures/feeds/decl-without-encoding.xml");
const ATOM_MINIMAL: &[u8] = include_bytes!("fixtures/feeds/atom-minimal.xml");
const ATOM_TITLE_TYPES: &[u8] = include_bytes!("fixtures/feeds/atom-title-types.xml");
const ATOM_LINK_NO_REL: &[u8] = include_bytes!("fixtures/feeds/atom-link-no-rel.xml");
const XML_BASE_RELATIVE: &[u8] = include_bytes!("fixtures/feeds/xml-base-relative.xml");
const CDATA_TITLE: &[u8] = include_bytes!("fixtures/feeds/cdata-title.xml");
const CYRILLIC_TITLE: &[u8] = include_bytes!("fixtures/feeds/cyrillic-title.xml");
const UNKNOWN_ENTITY_IN_TITLE: &[u8] = include_bytes!("fixtures/feeds/unknown-entity-in-title.xml");
const UNKNOWN_ENTITY_IN_GUID: &[u8] = include_bytes!("fixtures/feeds/unknown-entity-in-guid.xml");
const ITUNES_DURATION_FORMS: &[u8] = include_bytes!("fixtures/feeds/itunes-duration-forms.xml");
const ENCLOSURE_MALFORMED_WITH_GUID: &[u8] =
    include_bytes!("fixtures/feeds/enclosure-malformed-with-guid.xml");
const ENCLOSURE_SCHEME_UNSUPPORTED: &[u8] =
    include_bytes!("fixtures/feeds/enclosure-scheme-unsupported.xml");
const MULTIPLE_ENCLOSURES: &[u8] = include_bytes!("fixtures/feeds/multiple-enclosures.xml");
const RSS1_RDF: &[u8] = include_bytes!("fixtures/feeds/rss1-rdf.xml");
const TRUNCATED_XML: &[u8] = include_bytes!("fixtures/feeds/truncated-xml.xml");

fn feed_url() -> Url {
    #[allow(clippy::unwrap_used)]
    Url::parse("https://example.org/feed.xml").unwrap()
}

/// The live drift the 2026-11-01 review caught: an enclosure was checked
/// for scheme only, so a URL with embedded credentials from an untrusted
/// feed was admitted at parse and only failed later, at play. The
/// public-source bar (scheme, host, no credentials) holds at parse now.
#[test]
fn an_enclosure_with_embedded_credentials_or_no_host_is_dropped()
-> Result<(), Box<dyn std::error::Error>> {
    let body = "        <item><title>cred</title><guid>g-cred</guid>\
        <enclosure url=\"https://user:pw@example.org/a.mp3\" type=\"audio/mpeg\" length=\"1\"/></item>\
        <item><title>clean</title><guid>g-clean</guid>\
        <enclosure url=\"https://clean.example.org/a.mp3\" type=\"audio/mpeg\" length=\"1\"/></item>";
    let report = parse_feed(rss(body).as_bytes(), &feed_url())?;
    assert!(
        report.feed.items[0].enclosure.is_none(),
        "a credential-carrying enclosure is dropped"
    );
    assert!(
        report.feed.items[1].enclosure.is_some(),
        "a clean enclosure still parses"
    );
    Ok(())
}

/// Wraps a channel body in a minimal RSS 2.0 document.
fn rss(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<rss version=\"2.0\" \
         xmlns:i=\"http://www.itunes.com/dtds/podcast-1.0.dtd\" \
         xmlns:x=\"urn:example:extension\">\n<channel>{body}</channel>\n</rss>\n"
    )
}

/// Wraps a feed body in a minimal Atom 1.0 document.
///
/// The namespace is bound to the prefix `a` here and left as the default
/// namespace in the checked-in Atom fixtures, so both spellings are exercised
/// and neither can be what the mapping actually keys on.
fn atom(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<a:feed xmlns:a=\"http://www.w3.org/2005/Atom\" \
         xmlns:i=\"http://www.itunes.com/dtds/podcast-1.0.dtd\" \
         xmlns:x=\"urn:example:extension\">\n{body}\n</a:feed>\n"
    )
}

/// UTF-16 bytes, with or without a byte-order mark.
fn utf16(xml: &str, little_endian: bool, bom: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    if bom {
        bytes.extend_from_slice(if little_endian {
            &[0xff, 0xfe]
        } else {
            &[0xfe, 0xff]
        });
    }
    for unit in xml.encode_utf16() {
        if little_endian {
            bytes.extend_from_slice(&unit.to_le_bytes());
        } else {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
    }
    bytes
}

/// Splices raw bytes into an ASCII document at the `@` placeholder, which is
/// how a document gets a byte no UTF-8 decoder will accept.
fn splice(ascii: &str, raw: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut parts = ascii.split('@');
    #[allow(clippy::unwrap_used)]
    bytes.extend_from_slice(parts.next().unwrap().as_bytes());
    for part in parts {
        bytes.extend_from_slice(raw);
        bytes.extend_from_slice(part.as_bytes());
    }
    bytes
}

// ---------------------------------------------------------------- structure

#[test]
fn rss_keeps_decoded_guid_whitespace() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(RSS2_MINIMAL, &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Radio & Friends"));
    assert_eq!(
        report.feed.items[0].guid.as_deref(),
        Some("  https://EXAMPLE.org/p?a=1&b=2  ")
    );
    assert_eq!(
        report.feed.items[0]
            .enclosure
            .as_ref()
            .map(|enclosure| enclosure.url.as_str()),
        Some("https://example.org/audio/one.mp3")
    );
    assert_eq!(
        report.feed.items[0].declared_duration,
        Some(Duration::from_secs(6120))
    );
    Ok(())
}

#[test]
fn rss_reports_channel_metadata_and_enclosure_details() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(RSS2_MINIMAL, &feed_url())?;
    assert_eq!(
        report.feed.site_link.as_ref().map(Url::as_str),
        Some("https://example.org/")
    );
    let enclosure = report.feed.items[0]
        .enclosure
        .as_ref()
        .ok_or("expected an enclosure")?;
    assert_eq!(enclosure.length, Some(42));
    assert_eq!(enclosure.mime_type.as_deref(), Some("audio/mpeg"));
    assert_eq!(report.feed.items[0].title.as_deref(), Some("Opening"));
    // 2026-09-06T18:00:00Z, the `pubDate` the fixture declares.
    assert_eq!(
        report.feed.items[0].published,
        Some(time::OffsetDateTime::from_unix_timestamp(1_788_717_600)?)
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn document_order_is_preserved_against_publication_dates() -> Result<(), Box<dyn std::error::Error>>
{
    let report = parse_feed(
        rss(concat!(
            "<item><guid>a</guid><pubDate>Sun, 06 Sep 2026 18:00:00 GMT</pubDate></item>",
            "<item><guid>b</guid><pubDate>Sat, 05 Sep 2026 18:00:00 GMT</pubDate></item>",
            "<item><guid>c</guid><pubDate>Mon, 07 Sep 2026 18:00:00 GMT</pubDate></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    let guids: Vec<_> = report
        .feed
        .items
        .iter()
        .filter_map(|item| item.guid.as_deref())
        .collect();
    assert_eq!(guids, ["a", "b", "c"]);
    Ok(())
}

#[test]
fn extension_elements_never_substitute_for_rss_fields() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<x:title>Extension channel title</x:title>",
            "<x:wrapper><title>Nested title</title><item><guid>nested</guid></item></x:wrapper>",
            "<title>Real channel title</title>",
            "<item><x:guid>extension guid</x:guid><guid>real guid</guid>",
            "<x:enclosure url=\"https://example.org/wrong.mp3\"/></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.title.as_deref(), Some("Real channel title"));
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("real guid"));
    assert_eq!(report.feed.items[0].enclosure, None);
    Ok(())
}

#[test]
fn deeply_nested_extensions_do_not_exhaust_the_stack() -> Result<(), Box<dyn std::error::Error>> {
    // This used to nest 20,000 levels deep, back when the walker had no
    // depth cap at all: the point was that an iterative walker pays no
    // per-level stack cost, however deep a feed nests. `Walker::open` now
    // enforces `MAX_NESTING_DEPTH` (256) for an unrelated reason — a `Frame`
    // is heap-allocated, so unbounded depth is unbounded *heap*, not a stack
    // overflow — and 20,000 would simply hit that cap and return
    // `FeedError::Malformed` instead of exercising this test's point. 200
    // stays comfortably under the cap (rss + channel + x:deep + 200 x:n +
    // the buried title is 204) while still nesting far deeper than any real
    // feed does. The cap itself, and that it is a typed error rather than
    // unbounded memory growth, is
    // `a_document_nested_past_the_depth_cap_is_refused_not_exhausted` below.
    const DEPTH: usize = 200;
    let body = format!(
        "<x:deep>{}<title>buried</title>{}</x:deep><title>Real</title><item><guid>g</guid></item>",
        "<x:n>".repeat(DEPTH),
        "</x:n>".repeat(DEPTH),
    );
    let report = parse_feed(rss(&body).as_bytes(), &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Real"));
    assert_eq!(report.feed.items.len(), 1);
    Ok(())
}

#[test]
fn a_document_nested_past_the_depth_cap_is_refused_not_exhausted()
-> Result<(), Box<dyn std::error::Error>> {
    // One past what `deeply_nested_extensions_do_not_exhaust_the_stack`
    // proves is fine: rss + channel + 300 `x:n` clears `MAX_NESTING_DEPTH`
    // (256), so the walker must refuse this with a typed error long before
    // it would ever need to allocate 300 frames, let alone the millions an
    // attacker-sized document would ask for.
    const DEPTH: usize = 300;
    let body = format!("{}{}", "<x:n>".repeat(DEPTH), "</x:n>".repeat(DEPTH));
    match parse_feed(rss(&body).as_bytes(), &feed_url()) {
        Err(FeedError::Malformed { detail }) => {
            assert!(detail.contains("nested"), "{detail}");
        }
        other => return Err(format!("expected a nesting-depth Malformed, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn cdata_is_not_unescaped_a_second_time() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss("<title><![CDATA[Tom &amp; Jerry]]></title><item><guid><![CDATA[a&b]]></guid></item>")
            .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.title.as_deref(), Some("Tom &amp; Jerry"));
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("a&b"));
    Ok(())
}

// -------------------------------------------------------------------- Atom

#[test]
fn atom_uses_nested_base_and_exact_id() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(ATOM_MINIMAL, &"https://example.org/feeds/show.xml".parse()?)?;
    let item = &report.feed.items[0];
    assert_eq!(item.guid.as_deref(), Some("  stable&id  "));
    assert_eq!(item.title.as_deref(), Some("Hello world"));
    assert_eq!(
        item.enclosure.as_ref().map(|e| e.url.as_str()),
        Some("https://example.org/podcasts/season/one.mp3")
    );
    Ok(())
}

#[test]
fn atom_maps_feed_title_site_link_and_enclosure_details() -> Result<(), Box<dyn std::error::Error>>
{
    let report = parse_feed(ATOM_MINIMAL, &"https://example.org/feeds/show.xml".parse()?)?;
    assert_eq!(report.feed.title.as_deref(), Some("Example"));
    // `<a:link href="./"/>` carries no `rel`, which is `alternate` (RFC 4287
    // §4.2.7.2) and so the site link.
    assert_eq!(
        report.feed.site_link.as_ref().map(Url::as_str),
        Some("https://example.org/podcasts/")
    );
    let enclosure = report.feed.items[0]
        .enclosure
        .as_ref()
        .ok_or("expected an enclosure")?;
    assert_eq!(enclosure.length, Some(42));
    assert_eq!(enclosure.mime_type.as_deref(), Some("audio/mpeg"));
    // 2026-09-06T18:00:00Z, the RFC 3339 `published` the fixture declares.
    assert_eq!(
        report.feed.items[0].published,
        Some(time::OffsetDateTime::from_unix_timestamp(1_788_717_600)?)
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn atom_namespace_aliases_are_immaterial() -> Result<(), Box<dyn std::error::Error>> {
    // The same feed three ways. Nothing in the mapping may key on a prefix.
    let documents = [
        "<a:feed xmlns:a=\"http://www.w3.org/2005/Atom\"><a:title>T</a:title>\
         <a:entry><a:id>one</a:id></a:entry></a:feed>",
        "<atom:feed xmlns:atom=\"http://www.w3.org/2005/Atom\"><atom:title>T</atom:title>\
         <atom:entry><atom:id>one</atom:id></atom:entry></atom:feed>",
        "<feed xmlns=\"http://www.w3.org/2005/Atom\"><title>T</title>\
         <entry><id>one</id></entry></feed>",
    ];
    for document in documents {
        let report = parse_feed(document.as_bytes(), &feed_url())?;
        assert_eq!(report.feed.title.as_deref(), Some("T"), "{document}");
        assert_eq!(report.feed.items.len(), 1, "{document}");
        assert_eq!(
            report.feed.items[0].guid.as_deref(),
            Some("one"),
            "{document}"
        );
    }
    Ok(())
}

#[test]
fn atom_title_types_decide_how_much_markup_survives() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(ATOM_TITLE_TYPES, &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Plain & simple <b>"));
    let titles: Vec<_> = report
        .feed
        .items
        .iter()
        .map(|item| item.title.as_deref())
        .collect();
    assert_eq!(
        titles,
        [
            // Absent `type` is `text`: character content, references decoded.
            Some("No type at all & fine"),
            // `html`: decoded, and its markup stays literal because reading it
            // properly would need an HTML parser (§4.8).
            Some("Part <b>one</b> & two"),
            // `xhtml`: descendant text concatenated, markup dropped, and the
            // whitespace at each markup boundary kept.
            Some("Part one & two"),
        ]
    );
    Ok(())
}

#[test]
fn an_atom_link_without_rel_is_an_alternate() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(ATOM_LINK_NO_REL, &feed_url())?;
    assert_eq!(
        report.feed.site_link.as_ref().map(Url::as_str),
        Some("https://example.org/site/")
    );
    // Neither entry has an `id` or an enclosure, so this rel-less link is the
    // only identity the binding layer will be given (§4.3).
    assert!(
        report
            .feed
            .items
            .iter()
            .all(|item| item.guid.is_none() && item.enclosure.is_none())
    );
    let links: Vec<_> = report
        .feed
        .items
        .iter()
        .map(|item| item.link.as_ref().map(Url::as_str))
        .collect();
    assert_eq!(
        links,
        [
            Some("https://example.org/episodes/1"),
            // A `related` link is not an alternate, and the first alternate
            // wins over the later explicit one.
            Some("https://example.org/episodes/2"),
        ]
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn atom_prefers_published_over_updated_whatever_the_order() -> Result<(), Box<dyn std::error::Error>>
{
    let report = parse_feed(
        atom(concat!(
            "<a:entry><a:id>both</a:id>",
            "<a:updated>2026-09-07T18:00:00Z</a:updated>",
            "<a:published>2026-09-06T18:00:00Z</a:published></a:entry>",
            "<a:entry><a:id>updated-only</a:id>",
            "<a:updated>2026-09-07T18:00:00Z</a:updated></a:entry>",
            "<a:entry><a:id>neither</a:id></a:entry>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    let published: Vec<_> = report
        .feed
        .items
        .iter()
        .map(|item| item.published)
        .collect();
    assert_eq!(
        published,
        [
            Some(time::OffsetDateTime::from_unix_timestamp(1_788_717_600)?),
            Some(time::OffsetDateTime::from_unix_timestamp(1_788_804_000)?),
            None,
        ]
    );
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn a_feed_level_updated_is_never_an_entry_time() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        atom(concat!(
            "<a:updated>2026-09-07T18:00:00Z</a:updated>",
            "<a:entry><a:id>dateless</a:id></a:entry>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items[0].published, None);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn a_malformed_atom_date_keeps_the_item_and_never_falls_back()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        atom(concat!(
            // A `published` that will not parse answers the question badly
            // rather than handing it to `updated`.
            "<a:entry><a:id>bad-published</a:id>",
            "<a:published>Sun, 06 Sep 2026 18:00:00 GMT</a:published>",
            "<a:updated>2026-09-07T18:00:00Z</a:updated></a:entry>",
            "<a:entry><a:id>bad-updated</a:id>",
            "<a:updated>yesterday-ish</a:updated></a:entry>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items.len(), 2);
    assert!(
        report
            .feed
            .items
            .iter()
            .all(|item| item.published.is_none())
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(
        report.warnings,
        vec![
            ParseWarning {
                item: Some(1),
                kind: WarningKind::InvalidDate
            },
            ParseWarning {
                item: Some(2),
                kind: WarningKind::InvalidDate
            },
        ]
    );
    Ok(())
}

#[test]
fn an_unknown_entity_in_an_atom_identity_skips_only_that_entry()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        atom(concat!(
            "<a:entry><a:id>kept</a:id><a:title>Ten&nbsp;minutes</a:title></a:entry>",
            "<a:entry><a:id>a&nbsp;b</a:id></a:entry>",
            "<a:entry><a:id>bad-enclosure</a:id>",
            "<a:link rel=\"enclosure\" href=\"https://example.org/&nbsp;.mp3\"/></a:entry>",
            "<a:entry><a:link href=\"https://example.org/&nbsp;\"/></a:entry>",
            "<a:entry><a:id>after</a:id></a:entry>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    let guids: Vec<_> = report
        .feed
        .items
        .iter()
        .filter_map(|item| item.guid.as_deref())
        .collect();
    assert_eq!(guids, ["kept", "after"]);
    assert_eq!(
        report.feed.items[0].title.as_deref(),
        Some("Ten&nbsp;minutes")
    );
    assert_eq!(report.skipped, 3);
    assert_eq!(
        report.warnings,
        vec![
            ParseWarning {
                item: Some(2),
                kind: WarningKind::UnknownIdentityEntity
            },
            ParseWarning {
                item: Some(3),
                kind: WarningKind::UnknownIdentityEntity
            },
            ParseWarning {
                item: Some(4),
                kind: WarningKind::UnknownIdentityEntity
            },
        ]
    );
    Ok(())
}

#[test]
fn foreign_titles_and_ids_cannot_replace_atom_fields() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        atom(concat!(
            "<title>Unqualified feed title</title>",
            "<x:title>Extension feed title</x:title>",
            "<a:title>Real feed title</a:title>",
            "<x:wrapper><a:title>Buried</a:title>",
            "<a:entry><a:id>buried</a:id></a:entry></x:wrapper>",
            "<a:entry><id>unqualified</id><x:id>extension</x:id><a:id>real id</a:id>",
            "<link rel=\"enclosure\" href=\"https://example.org/wrong.mp3\"/></a:entry>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.title.as_deref(), Some("Real feed title"));
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("real id"));
    assert_eq!(report.feed.items[0].enclosure, None);
    assert_eq!(report.skipped, 0);
    Ok(())
}

#[test]
fn a_deeply_nested_xhtml_title_does_not_exhaust_the_stack() -> Result<(), Box<dyn std::error::Error>>
{
    // Markup a feed controls the depth of, with text at every level. A walker
    // that recursed, or that folded each level's text into its parent as it
    // unwound, would pay for that depth; this one does not.
    //
    // This used to nest 20,000 `x:b` levels, before `Walker::open` gained
    // `MAX_NESTING_DEPTH` (256, for the unrelated reason that an unbounded
    // `Vec<Frame>` is unbounded heap — see
    // `a_document_nested_past_the_depth_cap_is_refused_not_exhausted` in
    // `m4_feed_parse.rs`). 200 keeps a:feed + a:entry + a:title + x:div +
    // 200 x:b at 204, under the cap, while still nesting far deeper than any
    // real Atom title does.
    const DEPTH: usize = 200;
    let report = parse_feed(
        atom(&format!(
            "<a:entry><a:id>deep</a:id><a:title type=\"xhtml\">\
             <x:div>{}{}</x:div></a:title></a:entry>",
            "<x:b>a".repeat(DEPTH),
            "</x:b>".repeat(DEPTH),
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(
        report.feed.items[0].title.as_deref(),
        Some(&*"a".repeat(DEPTH))
    );
    Ok(())
}

#[test]
fn a_second_document_element_is_refused_whatever_it_is() -> Result<(), Box<dyn std::error::Error>> {
    let first = atom("<a:entry><a:id>a</a:id></a:entry>");
    let documents = [
        format!("{first}<a:feed xmlns:a=\"http://www.w3.org/2005/Atom\"/>"),
        format!("{first}<rss version=\"2.0\"/>"),
    ];
    for document in documents {
        assert!(
            matches!(
                parse_feed(document.as_bytes(), &feed_url()),
                Err(FeedError::Malformed { .. })
            ),
            "expected Malformed for {document}"
        );
    }
    Ok(())
}

#[test]
fn a_feed_level_href_that_will_not_decode_is_dropped_not_fatal()
-> Result<(), Box<dyn std::error::Error>> {
    // A site link is identity for nothing — it never reaches
    // `EpisodeKey::resolve` — so §4.8's remedy of failing the item has nothing
    // to fail, and §4.6's exhaustive list of feed-level faults does not include
    // this. The field goes absent, the warning says so, and every episode in
    // the feed survives a bad homepage URL.
    const BAD: &str = "https://example.org/&nbsp;";
    const GOOD: &str = "https://example.org/site/";
    let cases = [
        (
            rss(&format!("<link>{BAD}</link><item><guid>a</guid></item>")),
            None,
        ),
        (
            atom(&format!(
                "<a:link href=\"{BAD}\"/><a:entry><a:id>a</a:id></a:entry>"
            )),
            None,
        ),
        // And with a usable link beside it, in both orders: the good one wins
        // and the warning is raised exactly once either way.
        (
            rss(&format!(
                "<link>{BAD}</link><link>{GOOD}</link><item><guid>a</guid></item>"
            )),
            Some(GOOD),
        ),
        (
            rss(&format!(
                "<link>{GOOD}</link><link>{BAD}</link><item><guid>a</guid></item>"
            )),
            Some(GOOD),
        ),
        (
            atom(&format!(
                "<a:link href=\"{BAD}\"/><a:link href=\"{GOOD}\"/>\
                 <a:entry><a:id>a</a:id></a:entry>"
            )),
            Some(GOOD),
        ),
        (
            atom(&format!(
                "<a:link href=\"{GOOD}\"/><a:link href=\"{BAD}\"/>\
                 <a:entry><a:id>a</a:id></a:entry>"
            )),
            Some(GOOD),
        ),
    ];
    for (document, site_link) in cases {
        let report = parse_feed(document.as_bytes(), &feed_url())?;
        assert_eq!(report.feed.items.len(), 1, "{document}");
        assert_eq!(
            report.feed.items[0].guid.as_deref(),
            Some("a"),
            "{document}"
        );
        assert_eq!(
            report.feed.site_link.as_ref().map(Url::as_str),
            site_link,
            "{document}"
        );
        assert_eq!(report.skipped, 0, "{document}");
        assert_eq!(
            report.warnings,
            vec![ParseWarning {
                item: None,
                kind: WarningKind::UnknownIdentityEntity
            }],
            "{document}"
        );
    }
    Ok(())
}

#[test]
fn an_undecodable_enclosure_href_rejects_the_item_whatever_the_order()
-> Result<(), Box<dyn std::error::Error>> {
    // The same semantic input twice, in both formats. §4.8's "every href" is
    // unqualified, so an outcome that flipped with line order would be wrong
    // whichever way it flipped.
    const BAD: &str = "https://example.org/&nbsp;.mp3";
    const GOOD: &str = "https://example.org/good.mp3";
    let documents = [
        rss(&format!(
            "<item><guid>a</guid><enclosure url=\"{BAD}\"/><enclosure url=\"{GOOD}\"/></item>"
        )),
        rss(&format!(
            "<item><guid>a</guid><enclosure url=\"{GOOD}\"/><enclosure url=\"{BAD}\"/></item>"
        )),
        atom(&format!(
            "<a:entry><a:id>a</a:id><a:link rel=\"enclosure\" href=\"{BAD}\"/>\
             <a:link rel=\"enclosure\" href=\"{GOOD}\"/></a:entry>"
        )),
        atom(&format!(
            "<a:entry><a:id>a</a:id><a:link rel=\"enclosure\" href=\"{GOOD}\"/>\
             <a:link rel=\"enclosure\" href=\"{BAD}\"/></a:entry>"
        )),
    ];
    for document in documents {
        let report = parse_feed(document.as_bytes(), &feed_url())?;
        assert_eq!(report.feed.items, vec![], "{document}");
        assert_eq!(report.skipped, 1, "{document}");
        assert_eq!(
            report.warnings,
            vec![
                ParseWarning {
                    item: Some(1),
                    kind: WarningKind::UnknownIdentityEntity
                },
                ParseWarning {
                    item: Some(1),
                    kind: WarningKind::ExtraEnclosures { ignored: 1 }
                },
            ],
            "{document}"
        );
    }
    Ok(())
}

#[test]
fn an_undecodable_item_link_rejects_the_item_whatever_the_order()
-> Result<(), Box<dyn std::error::Error>> {
    const BAD: &str = "https://example.org/&nbsp;";
    const GOOD: &str = "https://example.org/good";
    let documents = [
        rss(&format!(
            "<item><guid>a</guid><link>{BAD}</link><link>{GOOD}</link></item>"
        )),
        rss(&format!(
            "<item><guid>a</guid><link>{GOOD}</link><link>{BAD}</link></item>"
        )),
        atom(&format!(
            "<a:entry><a:id>a</a:id><a:link href=\"{BAD}\"/>\
             <a:link href=\"{GOOD}\"/></a:entry>"
        )),
        atom(&format!(
            "<a:entry><a:id>a</a:id><a:link href=\"{GOOD}\"/>\
             <a:link href=\"{BAD}\"/></a:entry>"
        )),
    ];
    for document in documents {
        let report = parse_feed(document.as_bytes(), &feed_url())?;
        assert_eq!(report.feed.items, vec![], "{document}");
        assert_eq!(report.skipped, 1, "{document}");
        assert_eq!(
            report.warnings,
            vec![ParseWarning {
                item: Some(1),
                kind: WarningKind::UnknownIdentityEntity
            }],
            "{document}"
        );
    }
    Ok(())
}

#[test]
fn an_empty_but_valid_feed_parses_to_no_items() -> Result<(), Box<dyn std::error::Error>> {
    let documents = [
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel/></rss>",
        // Before Atom was mapped this document was `UnsupportedFormat`; it is
        // now a valid feed that simply has nothing in it.
        "<?xml version=\"1.0\"?><feed xmlns=\"http://www.w3.org/2005/Atom\">\
         <title>T</title></feed>",
    ];
    for document in documents {
        let report = parse_feed(document.as_bytes(), &feed_url())?;
        assert_eq!(report.feed.items, vec![], "{document}");
        assert_eq!(report.skipped, 0, "{document}");
        assert_eq!(report.warnings, vec![], "{document}");
    }
    Ok(())
}

#[test]
fn items_outside_their_container_are_ignored() -> Result<(), Box<dyn std::error::Error>> {
    let document = concat!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\">",
        "<item><guid>above the channel</guid></item>",
        "<channel><title>T</title>",
        "<wrapper><item><guid>buried</guid></item></wrapper>",
        "<item><guid>real</guid></item></channel>",
        "<item><guid>after the channel</guid></item></rss>",
    );
    let report = parse_feed(document.as_bytes(), &feed_url())?;
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("real"));

    let report = parse_feed(
        atom(concat!(
            "<a:entry><a:id>real</a:id></a:entry>",
            "<x:wrapper><a:entry><a:id>buried</a:id></a:entry></x:wrapper>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("real"));
    assert_eq!(report.skipped, 0);
    Ok(())
}

// --------------------------------------------------------- the fixture matrix

#[test]
fn xml_base_nests_and_restores_on_pop() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        XML_BASE_RELATIVE,
        &"https://example.org/feeds/show.xml".parse()?,
    )?;
    assert_eq!(
        report.feed.site_link.as_ref().map(Url::as_str),
        Some("https://example.org/media/season-1/index.html")
    );
    let urls: Vec<_> = report
        .feed
        .items
        .iter()
        .map(|item| item.enclosure.as_ref().map(|e| e.url.as_str()))
        .collect();
    assert_eq!(
        urls,
        [
            // Both the document element's base and the channel's and the
            // item's, in order.
            Some("https://example.org/media/season-1/bonus/one.mp3"),
            // The sibling item sets no base of its own, so the channel's is
            // back in force.
            Some("https://example.org/media/season-1/two.mp3"),
        ]
    );
    Ok(())
}

#[test]
fn cdata_titles_are_shown_exactly_as_written() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(CDATA_TITLE, &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("A &amp; B"));
    // CDATA stays literal while a reference standing beside it still decodes,
    // which is what unescaping the assembled field a second time would break.
    assert_eq!(
        report.feed.items[0].title.as_deref(),
        Some("Part &amp; parcel & more")
    );
    Ok(())
}

#[test]
fn an_unknown_entity_survives_literally_in_a_title() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(UNKNOWN_ENTITY_IN_TITLE, &feed_url())?;
    assert_eq!(
        report.feed.title.as_deref(),
        Some("Radio&nbsp;Show & Friends")
    );
    assert_eq!(
        report.feed.items[0].title.as_deref(),
        Some("Ten&nbsp;minutes & counting")
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn an_unknown_entity_in_a_guid_skips_only_that_item() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(UNKNOWN_ENTITY_IN_GUID, &feed_url())?;
    let guids: Vec<_> = report
        .feed
        .items
        .iter()
        .filter_map(|item| item.guid.as_deref())
        .collect();
    assert_eq!(guids, ["before", "after"]);
    assert_eq!(report.skipped, 1);
    assert_eq!(
        report.warnings,
        vec![ParseWarning {
            item: Some(2),
            kind: WarningKind::UnknownIdentityEntity
        }]
    );
    Ok(())
}

#[test]
fn the_duration_fixture_covers_three_forms_and_three_refusals()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(ITUNES_DURATION_FORMS, &feed_url())?;
    let durations: Vec<_> = report
        .feed
        .items
        .iter()
        .map(|item| item.declared_duration)
        .collect();
    let same = Some(Duration::from_secs(3723));
    // `1:02:03`, `62:03` and `3723` are the same length; `1:60`, a negative
    // and an overflow are not lengths at all.
    assert_eq!(durations, [same, same, same, None, None, None]);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn a_malformed_enclosure_leaves_the_guid_intact() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(ENCLOSURE_MALFORMED_WITH_GUID, &feed_url())?;
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(
        report.feed.items[0].guid.as_deref(),
        Some("urn:example:kept")
    );
    assert_eq!(report.feed.items[0].enclosure, None);
    assert_eq!(report.skipped, 0);
    assert_eq!(
        report.warnings,
        vec![ParseWarning {
            item: Some(1),
            kind: WarningKind::InvalidEnclosure
        }]
    );
    Ok(())
}

#[test]
fn unsupported_enclosure_schemes_never_become_playback_sources()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(ENCLOSURE_SCHEME_UNSUPPORTED, &feed_url())?;
    let urls: Vec<_> = report
        .feed
        .items
        .iter()
        .map(|item| item.enclosure.as_ref().map(|e| e.url.as_str()))
        .collect();
    // file, data and ftp in order, then the one scheme that is a source.
    assert_eq!(
        urls,
        [None, None, None, Some("https://example.org/four.mp3")]
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(
        report.warnings,
        vec![
            ParseWarning {
                item: Some(1),
                kind: WarningKind::InvalidEnclosure
            },
            ParseWarning {
                item: Some(2),
                kind: WarningKind::InvalidEnclosure
            },
            ParseWarning {
                item: Some(3),
                kind: WarningKind::InvalidEnclosure
            },
        ]
    );
    Ok(())
}

#[test]
fn the_multiple_enclosure_fixture_keeps_the_first_usable_one()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(MULTIPLE_ENCLOSURES, &feed_url())?;
    let enclosure = report.feed.items[0]
        .enclosure
        .as_ref()
        .ok_or("expected an enclosure")?;
    assert_eq!(enclosure.url.as_str(), "https://example.org/chosen.mp3");
    assert_eq!(enclosure.length, Some(7));
    assert_eq!(
        report.warnings,
        vec![
            ParseWarning {
                item: Some(1),
                kind: WarningKind::InvalidEnclosure
            },
            // The gopher one was never usable; the two after the winner were.
            ParseWarning {
                item: Some(1),
                kind: WarningKind::ExtraEnclosures { ignored: 3 }
            },
        ]
    );
    Ok(())
}

#[test]
fn the_rss1_rdf_fixture_is_unsupported() {
    assert!(matches!(
        parse_feed(RSS1_RDF, &feed_url()),
        Err(FeedError::UnsupportedFormat)
    ));
}

#[test]
fn the_truncated_fixture_fails_the_whole_document() {
    // Its first item is complete and would otherwise be usable. A document
    // that ends mid-element is still a feed-level fault (§4.6).
    assert!(matches!(
        parse_feed(TRUNCATED_XML, &feed_url()),
        Err(FeedError::Malformed { .. })
    ));
}

#[test]
fn a_cyrillic_title_is_preserved_verbatim() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(CYRILLIC_TITLE, &feed_url())?;
    // Verbatim: no transliteration, no case folding, no normalization. Turning
    // a title like this into an ASCII slug is the subscription layer's problem
    // (§2.5), and nothing the parser is allowed to anticipate.
    assert_eq!(
        report.feed.title.as_deref(),
        Some("Радио «Континуо» — сезон 1")
    );
    assert_eq!(
        report.feed.items[0].title.as_deref(),
        Some("Пролог: тишина и шум")
    );
    assert_eq!(
        report.feed.items[0].guid.as_deref(),
        Some("urn:example:пролог")
    );
    Ok(())
}

// ---------------------------------------------------------- URLs, durations

#[test]
fn enclosures_resolve_against_the_innermost_xml_base() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item xml:base=\"https://cdn.example.net/season/\">",
            "<guid>a</guid><enclosure url=\"one.mp3\"/></item>",
            "<item xml:base=\"https://cdn.example.net/season/\">",
            "<guid>b</guid><enclosure xml:base=\"https://other.example.net/x/\" url=\"two.mp3\"/>",
            "<link>three.html</link></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(
        report.feed.items[0]
            .enclosure
            .as_ref()
            .map(|enclosure| enclosure.url.as_str()),
        Some("https://cdn.example.net/season/one.mp3")
    );
    assert_eq!(
        report.feed.items[1]
            .enclosure
            .as_ref()
            .map(|enclosure| enclosure.url.as_str()),
        Some("https://other.example.net/x/two.mp3")
    );
    assert_eq!(
        report.feed.items[1].link.as_ref().map(Url::as_str),
        Some("https://cdn.example.net/season/three.html")
    );
    Ok(())
}

#[test]
fn unusable_enclosures_are_discarded_but_the_item_stays() -> Result<(), Box<dyn std::error::Error>>
{
    let report = parse_feed(
        rss(concat!(
            "<item><guid>a</guid><enclosure url=\"ftp://example.org/one.mp3\"/></item>",
            "<item><guid>b</guid><enclosure url=\"http://[bad\"/></item>",
            "<item><guid>c</guid></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items.len(), 3);
    assert!(
        report
            .feed
            .items
            .iter()
            .all(|item| item.enclosure.is_none())
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(
        report.warnings,
        vec![
            ParseWarning {
                item: Some(1),
                kind: WarningKind::InvalidEnclosure
            },
            ParseWarning {
                item: Some(2),
                kind: WarningKind::InvalidEnclosure
            },
        ]
    );
    Ok(())
}

#[test]
fn a_bad_enclosure_length_yields_none_not_a_rejection() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item><guid>a</guid>",
            "<enclosure url=\"https://example.org/one.mp3\" length=\"-3\" type=\" \"/></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    let enclosure = report.feed.items[0]
        .enclosure
        .as_ref()
        .ok_or("expected the enclosure to survive a bad length")?;
    assert_eq!(enclosure.length, None);
    assert_eq!(enclosure.mime_type, None);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn the_first_usable_enclosure_wins_and_the_rest_are_counted()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item><guid>a</guid>",
            "<enclosure url=\"gopher://example.org/skipped.mp3\"/>",
            "<enclosure url=\"https://example.org/chosen.mp3\"/>",
            "<enclosure url=\"https://example.org/extra.mp3\"/></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(
        report.feed.items[0]
            .enclosure
            .as_ref()
            .map(|enclosure| enclosure.url.as_str()),
        Some("https://example.org/chosen.mp3")
    );
    assert_eq!(
        report.warnings,
        vec![
            ParseWarning {
                item: Some(1),
                kind: WarningKind::InvalidEnclosure
            },
            ParseWarning {
                item: Some(1),
                kind: WarningKind::ExtraEnclosures { ignored: 2 }
            },
        ]
    );
    Ok(())
}

#[test]
fn itunes_duration_accepts_three_forms_and_refuses_the_rest()
-> Result<(), Box<dyn std::error::Error>> {
    let accepted = [
        ("90", 90u64),
        ("0", 0),
        ("7:30", 450),
        ("90:00", 5400),
        ("1:42:00", 6120),
        ("  1:00:00  ", 3600),
        ("00:00:59", 59),
    ];
    let refused = [
        "",
        "-5",
        "1.5",
        "1:2:3:4",
        "1::2",
        ":30",
        "30:",
        "1:60",
        "1:60:00",
        "1:00:60",
        "one",
        "18446744073709551615:00",
        "1 : 2",
    ];

    let body: String = accepted
        .iter()
        .map(|(raw, _)| raw)
        .chain(refused.iter())
        .enumerate()
        .map(|(index, raw)| {
            format!("<item><guid>{index}</guid><i:duration>{raw}</i:duration></item>")
        })
        .collect();
    let report = parse_feed(rss(&body).as_bytes(), &feed_url())?;

    for (index, (raw, seconds)) in accepted.iter().enumerate() {
        assert_eq!(
            report.feed.items[index].declared_duration,
            Some(Duration::from_secs(*seconds)),
            "duration {raw:?} should parse"
        );
    }
    for (offset, raw) in refused.iter().enumerate() {
        let index = accepted.len() + offset;
        assert_eq!(
            report.feed.items[index].declared_duration, None,
            "duration {raw:?} should be refused"
        );
    }
    Ok(())
}

#[test]
fn an_unparsable_date_keeps_the_item_and_never_counts_as_skipped()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss("<item><guid>a</guid><pubDate>yesterday-ish</pubDate></item>").as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(report.feed.items[0].published, None);
    assert_eq!(report.skipped, 0);
    assert_eq!(
        report.warnings,
        vec![ParseWarning {
            item: Some(1),
            kind: WarningKind::InvalidDate
        }]
    );
    Ok(())
}

/// Real podcast feeds routinely spell the zero offset `UTC`, which RFC 2822
/// does not define — its obsolete-zone production allows `UT` and `GMT` but
/// never `UTC`, so a strict parser rejects the whole timestamp. Radio-T spells
/// every one of its `pubDate`s that way, which blanked the PUBLISHED column
/// for an entire feed. §4.3 accepts the spelling rather than the column.
#[test]
fn the_utc_zone_spelling_is_accepted_even_though_rfc_2822_omits_it()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss("<item><guid>a</guid><pubDate>Sat, 12 Sep 2026 18:21:23 UTC</pubDate></item>")
            .as_bytes(),
        &feed_url(),
    )?;
    let published = report.feed.items[0]
        .published
        .ok_or("expected a timestamp")?;
    assert_eq!(published.unix_timestamp(), 1_789_237_283);
    assert_eq!(published.offset(), time::UtcOffset::UTC);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

/// The spellings RFC 2822 does define keep working, and a genuinely malformed
/// zone is still refused — the widening is one spelling, not a loosened parser.
#[test]
fn the_conformant_zone_spellings_are_unaffected() -> Result<(), Box<dyn std::error::Error>> {
    for (zone, offset_hours) in [("GMT", 0), ("UT", 0), ("+0300", 3), ("EST", -5)] {
        let feed = format!(
            "<item><guid>a</guid><pubDate>Sat, 12 Sep 2026 18:21:23 {zone}</pubDate></item>"
        );
        let report = parse_feed(rss(&feed).as_bytes(), &feed_url())?;
        let published = report.feed.items[0]
            .published
            .ok_or_else(|| format!("{zone} should parse"))?;
        assert_eq!(published.offset().whole_hours(), offset_hours, "{zone}");
    }
    for zone in ["UTC+1", "NOTAZONE", "UTCC"] {
        let feed = format!(
            "<item><guid>a</guid><pubDate>Sat, 12 Sep 2026 18:21:23 {zone}</pubDate></item>"
        );
        let report = parse_feed(rss(&feed).as_bytes(), &feed_url())?;
        assert_eq!(
            report.feed.items[0].published, None,
            "{zone} must stay refused"
        );
    }
    Ok(())
}

// -------------------------------------------------------------- identity

#[test]
fn an_unknown_entity_is_literal_in_a_title_and_fatal_in_an_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item><guid>kept</guid><title>Ten&nbsp;minutes</title></item>",
            "<item><guid>a&nbsp;b</guid><title>dropped</title></item>",
            "<item><guid>c</guid><link>https://example.org/&nbsp;</link></item>",
            "<item><guid>d</guid><enclosure url=\"https://example.org/&nbsp;.mp3\"/></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.items.len(), 1);
    assert_eq!(
        report.feed.items[0].title.as_deref(),
        Some("Ten&nbsp;minutes")
    );
    assert_eq!(report.skipped, 3);
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| warning.kind == WarningKind::UnknownIdentityEntity),
        "{:?}",
        report.warnings
    );
    Ok(())
}

#[test]
fn numeric_character_references_resolve_everywhere() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss("<title>caf&#233;</title><item><guid>a&#x26;b</guid></item>").as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(report.feed.title.as_deref(), Some("café"));
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("a&b"));
    assert_eq!(report.skipped, 0);
    Ok(())
}

#[test]
fn a_doctype_entity_declaration_is_never_expanded() -> Result<(), Box<dyn std::error::Error>> {
    let document = concat!(
        "<?xml version=\"1.0\"?>\n",
        "<!DOCTYPE rss [<!ENTITY secret \"leaked\">]>\n",
        "<rss version=\"2.0\"><channel><title>T&secret;</title>",
        "<item><guid>a</guid></item></channel></rss>",
    );
    let report = parse_feed(document.as_bytes(), &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("T&secret;"));
    Ok(())
}

#[test]
fn warning_payloads_carry_no_urls_or_guids_under_debug() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(concat!(
            "<item><guid>urn:secret-guid:1</guid>",
            "<enclosure url=\"gopher://secret.example.net/a.mp3\"/>",
            "<enclosure url=\"https://secret.example.net/b.mp3\"/>",
            "<pubDate>not-a-date</pubDate></item>",
            "<item><guid>urn:secret-guid:&nbsp;</guid></item>",
        ))
        .as_bytes(),
        &feed_url(),
    )?;
    let rendered = format!("{:?}", report.warnings);
    for leak in [
        "secret",
        "gopher",
        "https",
        "urn:",
        "example.net",
        "not-a-date",
    ] {
        assert!(
            !rendered.contains(leak),
            "warning debug output leaked {leak:?}: {rendered}"
        );
    }
    assert_eq!(report.warnings.len(), 4);
    Ok(())
}

// ------------------------------------------------------------- feed faults

#[test]
fn unsupported_document_elements_are_refused_by_name() {
    let cases: [&str; 5] = [
        // RSS 1.0, refused by name rather than half-parsed (§4.2).
        "<?xml version=\"1.0\"?><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
         xmlns=\"http://purl.org/rss/1.0/\"><channel><title>T</title></channel></rdf:RDF>",
        // Atom 0.3: the right local name in the wrong namespace. Only Atom 1.0
        // is mapped, and the URI is what says which is which.
        "<?xml version=\"1.0\"?><feed xmlns=\"http://purl.org/atom/ns#\" version=\"0.3\">\
         <title>T</title></feed>",
        // An unqualified `feed` is in no namespace, so it is not Atom either.
        "<?xml version=\"1.0\"?><feed><title>T</title></feed>",
        // An `rss` with no `channel` has nowhere to keep items (§4.6).
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><title>no channel</title></rss>",
        "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\"><body/></html>",
    ];
    for document in cases {
        assert!(
            matches!(
                parse_feed(document.as_bytes(), &feed_url()),
                Err(FeedError::UnsupportedFormat)
            ),
            "expected UnsupportedFormat for {document}"
        );
    }
}

#[test]
fn an_incomplete_document_is_malformed_even_after_a_good_item() {
    let cases = [
        // Truncated mid-document.
        rss("<item><guid>a</guid></item>").replace("</channel>\n</rss>\n", ""),
        // A complete item, then a suffix that never closes.
        rss("<item><guid>a</guid></item>").replace("</rss>\n", "<x:trailing>"),
        // Malformed XML inside an element this parser otherwise ignores.
        rss("<item><guid>a</guid></item><x:junk></x:mismatch>"),
        // A second document element after the first one closed.
        format!(
            "{}<rss version=\"2.0\"/>",
            rss("<item><guid>a</guid></item>")
        ),
    ];
    for document in cases {
        assert!(
            matches!(
                parse_feed(document.as_bytes(), &feed_url()),
                Err(FeedError::Malformed { .. })
            ),
            "expected Malformed for {document}"
        );
    }
}

#[test]
fn a_malformed_error_never_quotes_the_document() -> Result<(), Box<dyn std::error::Error>> {
    let document = rss("<item><guid>urn:secret-guid:1</guid></item>").replace("</rss>\n", "");
    let Err(error) = parse_feed(document.as_bytes(), &feed_url()) else {
        return Err("expected the truncated document to fail".into());
    };
    let rendered = format!("{error} / {error:?}");
    for leak in ["secret", "example.org", "guid"] {
        assert!(
            !rendered.contains(leak),
            "error output leaked {leak:?}: {rendered}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- encoding

#[test]
fn utf16_bom_is_decoded_strictly() -> Result<(), Box<dyn std::error::Error>> {
    let xml = "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>";
    let mut bytes = vec![0xff, 0xfe];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let report = parse_feed(&bytes, &"https://example.org/feed".parse()?)?;
    assert_eq!(report.feed.title.as_deref(), Some("Радио"));
    Ok(())
}

#[test]
fn every_utf16_framing_decodes_to_the_same_title() -> Result<(), Box<dyn std::error::Error>> {
    let undeclared = "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>";
    // The declaration is far longer than the reader's lookahead window once
    // encoded as UTF-16; the BOM (or the byte pattern) settles the encoding,
    // so nothing has to be switched.
    let declared = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><rss><channel><title>Радио</title></channel></rss>";
    let cases = [
        utf16(undeclared, true, true),
        utf16(undeclared, false, true),
        utf16(declared, true, false),
        utf16(declared, false, true),
    ];
    for bytes in cases {
        let report = parse_feed(&bytes, &feed_url())?;
        assert_eq!(report.feed.title.as_deref(), Some("Радио"));
    }
    Ok(())
}

#[test]
fn a_utf8_bom_is_stripped_with_and_without_a_declared_encoding()
-> Result<(), Box<dyn std::error::Error>> {
    let bodies = [
        "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>",
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><rss><channel><title>Радио</title></channel></rss>",
    ];
    for body in bodies {
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice(body.as_bytes());
        let report = parse_feed(&bytes, &feed_url())?;
        assert_eq!(report.feed.title.as_deref(), Some("Радио"));
    }
    Ok(())
}

#[test]
fn a_declared_single_byte_encoding_is_honored() -> Result<(), Box<dyn std::error::Error>> {
    let document = splice(
        "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\
         <rss version=\"2.0\"><channel><title>Caf@</title>\
         <item><guid>a</guid></item></channel></rss>",
        b"\xe9",
    );
    let report = parse_feed(&document, &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Café"));
    Ok(())
}

#[test]
fn malformed_bytes_are_refused_rather_than_replaced() -> Result<(), Box<dyn std::error::Error>> {
    // Once inside the reader's lookahead window, once well past it, so both
    // the prefix decode and the main decode path are exercised.
    let near = splice(
        "<?xml version=\"1.0\"?><rss><channel><title>@</title></channel></rss>",
        b"\xff",
    );
    let far = splice(
        &rss("<link>https://example.org/</link><item><guid>a</guid><title>@</title></item>"),
        b"\xff",
    );
    for document in [near, far] {
        match parse_feed(&document, &feed_url()) {
            Err(FeedError::Encoding) => {}
            other => return Err(format!("expected FeedError::Encoding, got {other:?}").into()),
        }
    }
    Ok(())
}

#[test]
fn nothing_is_ever_replaced_with_the_replacement_character()
-> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        &utf16(
            "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>",
            true,
            true,
        ),
        &feed_url(),
    )?;
    let title = report.feed.title.unwrap_or_default();
    assert!(!title.contains('\u{fffd}'), "{title:?}");
    Ok(())
}

#[test]
fn an_absent_encoding_attribute_is_not_an_error() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(DECL_WITHOUT_ENCODING, &feed_url())?;
    assert_eq!(report.feed.title.as_deref(), Some("Радио «Континуо»"));
    assert_eq!(report.feed.items[0].title.as_deref(), Some("Пролог"));
    assert_eq!(report.skipped, 0);
    assert_eq!(report.warnings, vec![]);
    Ok(())
}

#[test]
fn an_unrecognized_encoding_label_names_itself() -> Result<(), Box<dyn std::error::Error>> {
    let document = "<?xml version=\"1.0\" encoding=\"x-nonesuch\"?>\
                    <rss version=\"2.0\"><channel><title>T</title></channel></rss>";
    match parse_feed(document.as_bytes(), &feed_url()) {
        Err(FeedError::UnsupportedEncoding { label }) => assert_eq!(label, "x-nonesuch"),
        other => return Err(format!("expected UnsupportedEncoding, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_malformed_declaration_is_malformed_not_unsupported() -> Result<(), Box<dyn std::error::Error>>
{
    let cases = [
        // Unquoted encoding value: `encoding()` reports an attribute error.
        "<?xml version=\"1.0\" encoding=utf-8?>\
         <rss version=\"2.0\"><channel><title>T</title></channel></rss>",
        // No version at all.
        "<?xml encoding=\"utf-8\"?>\
         <rss version=\"2.0\"><channel><title>T</title></channel></rss>",
        // A version this parser does not know.
        "<?xml version=\"2.0\"?>\
         <rss version=\"2.0\"><channel><title>T</title></channel></rss>",
    ];
    for document in cases {
        match parse_feed(document.as_bytes(), &feed_url()) {
            Err(FeedError::Malformed { .. }) => {}
            other => return Err(format!("expected Malformed for {document}, got {other:?}").into()),
        }
    }
    Ok(())
}

/// Characterizes the bound this parser depends on rather than trusting it.
///
/// `DecodingReader` buffers 64 source bytes so that `set_encoding` can still
/// be called after the declaration has been parsed, and *asserts* once that
/// buffer drains. How long a declaration is, is feed-controlled, so the
/// parser checks the bound rather than assuming it. Past 64 bytes the
/// document is refused with a typed error and never a panic — as
/// `FeedError::Encoding` when the undecodable bytes are reached first, and as
/// `FeedError::Malformed` when the body is plain ASCII and the refused
/// encoding switch is the only thing wrong.
#[test]
fn a_declaration_longer_than_the_decoder_window_is_refused_not_panicked()
-> Result<(), Box<dyn std::error::Error>> {
    let latin1_body = "<rss version=\"2.0\"><channel><title>Caf@</title>\
                       <item><guid>a</guid></item></channel></rss>";
    let ascii_body = "<rss version=\"2.0\"><channel><title>Cafe</title>\
                      <item><guid>a</guid></item></channel></rss>";
    let shortest = "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>";
    assert_eq!(shortest.len(), 43);

    for padding in 0..=24 {
        let declaration =
            shortest.replace(" encoding=", &format!("{} encoding=", " ".repeat(padding)));
        let length = declaration.len();
        assert_eq!(length, 43 + padding);
        let within_window = length <= 64;

        let latin1 = splice(&format!("{declaration}{latin1_body}"), b"\xe9");
        match parse_feed(&latin1, &feed_url()) {
            Ok(report) if within_window => {
                assert_eq!(report.feed.title.as_deref(), Some("Café"));
            }
            Err(FeedError::Encoding) if !within_window => {}
            other => {
                return Err(
                    format!("{length}-byte declaration, Latin-1 body: got {other:?}").into(),
                );
            }
        }

        let ascii = format!("{declaration}{ascii_body}");
        match parse_feed(ascii.as_bytes(), &feed_url()) {
            Ok(report) if within_window => {
                assert_eq!(report.feed.title.as_deref(), Some("Cafe"));
            }
            Err(FeedError::Malformed { .. }) if !within_window => {}
            other => {
                return Err(format!("{length}-byte declaration, ASCII body: got {other:?}").into());
            }
        }
    }
    Ok(())
}

/// `itunes:image href` names the cover at feed level and, overriding it, at
/// item level. A relative href resolves against the base in scope; an href
/// that is not http(s) is dropped rather than kept as an unusable URL.
#[test]
fn itunes_image_is_read_at_feed_and_item_level() -> Result<(), Box<dyn std::error::Error>> {
    let report = parse_feed(
        rss(r#"<i:image href="covers/cover.png"/>
            <item><guid>a</guid><i:image href="https://cdn.example.org/a.jpg"/></item>
            <item><guid>b</guid></item>
            <item><guid>c</guid><i:image href="ftp://cdn.example.org/c.jpg"/></item>"#)
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(
        report.feed.image,
        Some(Url::parse("https://example.org/covers/cover.png")?)
    );
    let images: Vec<_> = report
        .feed
        .items
        .iter()
        .map(|item| item.image.clone())
        .collect();
    assert_eq!(
        images,
        vec![
            Some(Url::parse("https://cdn.example.org/a.jpg")?),
            None,
            None
        ]
    );

    let report = parse_feed(
        atom(
            r#"<i:image href="https://example.org/feed.png"/>
            <a:entry><a:id>a</a:id><i:image href="https://example.org/a.png"/></a:entry>"#,
        )
        .as_bytes(),
        &feed_url(),
    )?;
    assert_eq!(
        report.feed.image,
        Some(Url::parse("https://example.org/feed.png")?)
    );
    assert_eq!(
        report.feed.items[0].image,
        Some(Url::parse("https://example.org/a.png")?)
    );
    Ok(())
}
