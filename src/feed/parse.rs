//! Feed bytes to parse-layer values (§4).
//!
//! [`parse_feed`] is subscription-independent on purpose: it takes bytes and
//! the URL they were retrieved from, and knows nothing about `FeedId`s, caches
//! or the network. That is what makes it testable against fixtures alone.
//!
//! Decoding is an explicit layer, never `NsReader` over raw bytes (§4.1):
//!
//! ```text
//! bytes -> quick_xml::encoding::DecodingReader -> NsReader<DecodingReader<&[u8]>>
//! ```
//!
//! `DecodingReader` transcodes with `decode_to_utf8_without_replacement`, so a
//! malformed byte becomes [`FeedError::Encoding`] rather than U+FFFD. A
//! silently mangled title is worse than a refusal that names its cause.
//!
//! RSS 2.0 and Atom 1.0 are two mappings over **one** walker (§4.3). The
//! document element picks the mapping once, and from there the format lives in
//! the `Context` the open path resolves to: an RSS arm wants an RSS context
//! and the empty namespace, an Atom arm wants an Atom context and the Atom
//! namespace URI. Prefixes never enter into it, and no element can be mistaken
//! for a field of the other format.

use std::io::ErrorKind;
use std::time::Duration;

use quick_xml::XmlVersion;
use quick_xml::encoding::DecodingReader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesDecl, BytesRef, BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::reader::NsReader;
use time::OffsetDateTime;
use time::format_description::well_known::{Rfc2822, Rfc3339};
use url::Url;

use super::error::FeedError;
use super::model::{Enclosure, ParsedFeed, ParsedItem};

/// The Atom 1.0 namespace. Every Atom element is matched by this URI plus a
/// local name, never by a prefix, so `<feed>`, `<a:feed>` and `<atom:feed>`
/// are the same document element and an unqualified `<title>` inside an Atom
/// feed is not an Atom title at all (§4.2, §4.3).
const ATOM_NS: &str = "http://www.w3.org/2005/Atom";
/// The `itunes` namespace, the one extension this parser reads rather than
/// ignores (§4.3).
const ITUNES_NS: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";
/// The namespace the `xml` prefix is permanently bound to, so `xml:base`
/// is recognized by its expanded name rather than by a raw prefix (§4.4).
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// `quick_xml::encoding::DecodingReader`'s lookahead window, in **source**
/// bytes, minus any byte-order mark it strips.
///
/// `DecodingReader` reads this many bytes up front precisely so that
/// `set_encoding` can still be called after the declaration has been parsed;
/// once that buffer drains, `set_encoding` *asserts*. The value is a private
/// `PREFIX_CAP` constant in quick-xml 0.42 and is not exposed, so
/// [`declaration_fits_prefix`] is guarded by a characterization test —
/// `a_declaration_longer_than_the_decoder_window_is_refused_not_panicked` in
/// `tests/m4_feed_parse.rs` walks declaration lengths across the boundary —
/// rather than by trust.
const PREFIX_CAP: u64 = 64;

/// UTF-8 byte-order mark. `DecodingReader` strips it from its lookahead
/// window, shortening the window by exactly this many bytes.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The deepest an open-element stack is allowed to get before [`Walker::open`]
/// refuses to push another [`Frame`].
///
/// [`Limits::document_bytes`](crate::http::limits::Limits::document_bytes) bounds the
/// *input*, not the *work* the walker does per byte, and the two are not
/// proportional: `<a><a><a>…` costs three source bytes per nested element,
/// so an 8 MiB document of nothing else opens roughly 2.8 million frames.
/// Each `Frame` is on the order of 150–160 bytes plus a heap allocation for
/// `local`, so that document would hold a `Vec<Frame>` north of half a
/// gigabyte — with a transient peak near double that while the vector grows
/// — before any typed error could fire. The parse is iterative rather than
/// recursive, so unbounded depth costs heap, not stack: a slow OOM kill
/// instead of a refusal. No legitimate RSS or Atom document nests past
/// single digits, so 256 is generous headroom, not a tight fit.
const MAX_NESTING_DEPTH: usize = 256;

/// The outcome of parsing one feed document.
///
/// `skipped` counts **parse-stage** item rejection only — an item whose
/// identity could not be decoded. Identityless and duplicate items are the
/// binding layer's concern, and a date that will not parse is neither (§4.6).
#[derive(Clone, Debug, PartialEq)]
pub struct ParseReport {
    pub feed: ParsedFeed,
    pub skipped: usize,
    pub warnings: Vec<ParseWarning>,
}

/// One non-fatal observation about a document.
///
/// `item` is the **1-based ordinal of the `<item>` or `<entry>` element in
/// document order**, counted before anything is skipped, or `None` for a
/// feed-level observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseWarning {
    pub item: Option<usize>,
    pub kind: WarningKind,
}

/// What a [`ParseWarning`] is about.
///
/// These variants deliberately carry **no text**: no GUID, no URL, no title.
/// Redaction has to hold under `Debug` as well as `Display` (§7.2), and the
/// cheapest way to guarantee that is to give the payload nowhere to hide a
/// URL in the first place. The ordinal plus the category is enough to find
/// the item in the feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarningKind {
    /// An identity field contained an entity reference this parser cannot
    /// resolve (§4.8): for an item- or entry-level field the item is
    /// rejected, and for a feed-level field (`item: None`) the field is
    /// merely dropped and the document stands.
    UnknownIdentityEntity,
    /// A publication date would not parse. The item stays, with
    /// `published: None` (§4.6).
    InvalidDate,
    /// An enclosure's URL would not resolve, or its scheme is not HTTP(S),
    /// so that enclosure was discarded (§4.5).
    InvalidEnclosure,
    /// The item carried more than one enclosure — RSS `<enclosure>` or Atom
    /// `link[rel=enclosure]` — and the first usable one in document order won,
    /// so this many were ignored (§4.7).
    ExtraEnclosures { ignored: usize },
    /// The binding stage found no GUID, no usable enclosure and no link, so
    /// the item has no identity and was skipped (§2.4). Never assigned by
    /// the parser itself — only by [`crate::feed::episode::bind_feed`].
    MissingIdentity,
    /// The binding stage resolved this item to the same episode key as an
    /// earlier item in the same feed; the first occurrence won and this one
    /// was skipped (§4.7). Never assigned by the parser itself — only by
    /// [`crate::feed::episode::bind_feed`].
    DuplicateIdentity,
}

/// Parses one feed document into parse-layer values.
///
/// `retrieval_url` is the final URL after redirects, and is the base every
/// relative URL falls back to when no `xml:base` is in scope (§4.4).
///
/// # Errors
///
/// Feed-level faults only, and §4.6's list of them is exhaustive: XML that
/// will not parse, a decoding failure, an unsupported document element, or a
/// missing `<channel>`. Item faults are counted in
/// [`ParseReport::skipped`] instead, and a feed-level field that will not
/// decode is merely absent.
pub fn parse_feed(bytes: &[u8], retrieval_url: &Url) -> Result<ParseReport, FeedError> {
    let mut reader = NsReader::from_reader(DecodingReader::new(bytes));
    // quick-xml 0.42 defaults are already what §4 asks for: no whitespace
    // trimming, end names checked, a dangling `&` refused. Nothing to set.
    let mut buf = Vec::new();
    let mut walker = Walker::new(retrieval_url);
    let mut at_start = true;

    loop {
        buf.clear();
        let event = match reader.read_event_into(&mut buf) {
            Ok(event) => event,
            Err(error) => return Err(map_xml_error(&error, reader.error_position())),
        };

        if at_start {
            at_start = false;
            if let Event::Decl(declaration) = &event {
                walker.version = apply_declaration(&mut reader, declaration, bytes)?;
                continue;
            }
        }

        match event {
            Event::Start(ref element) => walker.open(&reader, element)?,
            Event::Empty(ref element) => {
                walker.open(&reader, element)?;
                walker.close()?;
            }
            Event::End(_) => walker.close()?,
            Event::Text(ref text) => walker.push_text(&text.xml_content(walker.version)),
            Event::CData(ref data) => {
                // CDATA is already literal. Unescaping it a second time would
                // turn a title that legitimately reads `&amp;` into `&`.
                walker.push_text(data);
            }
            Event::GeneralRef(ref reference) => walker.push_reference(reference),
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) => {}
            // A DTD is never fetched and its entity declarations are never
            // expanded, so entities it declares stay unknown (§4.8).
            Event::DocType(_) => {}
            Event::Eof => break,
        }
    }

    walker.finish()
}

/// Makes an XML declaration's raw `encoding` label safe to place in
/// [`FeedError::UnsupportedEncoding`].
///
/// `BytesDecl::encoding()` returns the attribute's bytes decoded but
/// otherwise unvalidated — quick-xml does not enforce XML 1.0's `Char`
/// production here, so a feed can put a control character, including an
/// ANSI escape (ESC, 0x1B), into this string. That string reaches a
/// terminal or a log line from more than one place — `main.rs` prints
/// `{error}`, `commands.rs` prints it in a refresh summary, and `tracing`
/// records it — and none of those call sites goes through a shared escaping
/// boundary. Sanitizing has to happen here, at construction, rather than at
/// any one of those formatters.
///
/// ASCII graphic characters are the only ones let through: it is a
/// declaration label, not free text, so there is nothing lost by refusing
/// whitespace, control characters and non-ASCII here that a genuine
/// character-set name would ever need. The length cap is generous for a
/// label and stops a pathological declaration from ballooning the message.
fn sanitize_declaration_label(label: &str) -> String {
    label
        .chars()
        .filter(char::is_ascii_graphic)
        .take(64)
        .collect()
}

/// Honors the XML declaration's `encoding` attribute, if any (§4.1).
///
/// The declaration is inspected with [`BytesDecl::encoding`] rather than
/// [`BytesDecl::encoder`] because `encoder` chains `encoding()` -> `.ok()` ->
/// `Encoding::for_label` and collapses three different situations into one
/// `None`, which would send a perfectly valid `<?xml version="1.0"?>` down
/// the unsupported-encoding path. Once `encoding()` has told the four cases
/// apart, `encoder()` is what resolves the label — that keeps `encoding_rs`
/// out of this crate's direct dependencies.
fn apply_declaration(
    reader: &mut NsReader<DecodingReader<&[u8]>>,
    declaration: &BytesDecl<'_>,
    bytes: &[u8],
) -> Result<XmlVersion, FeedError> {
    let version = match declaration.version() {
        Ok(version) => match version.as_ref() {
            "1.0" => XmlVersion::Explicit1_0,
            "1.1" => XmlVersion::Explicit1_1,
            _ => return Err(malformed("XML declaration names an unknown version")),
        },
        Err(_) => return Err(malformed("XML declaration has no leading version")),
    };
    if let Some(Err(_)) = declaration.standalone() {
        return Err(malformed("invalid XML declaration"));
    }

    match declaration.encoding() {
        // No `encoding` attribute: keep whatever was detected from the BOM or
        // the declaration's byte pattern. Not an error.
        None => {}
        Some(Err(_)) => return Err(malformed("invalid XML declaration")),
        Some(Ok(label)) => {
            let label = label.into_owned();
            let Some(encoding) = declaration.encoder() else {
                return Err(FeedError::UnsupportedEncoding {
                    label: sanitize_declaration_label(&label),
                });
            };
            // Already decoding as the declared encoding — either it was
            // detected, or the label is a synonym for it. `set_encoding` would
            // be a no-op, and calling it is the one thing that could assert.
            if !encodings_agree(reader.get_ref().encoding().name(), encoding.name()) {
                if !declaration_fits_prefix(reader, bytes) {
                    return Err(malformed(
                        "XML declaration is too long for its encoding to be applied",
                    ));
                }
                reader.get_mut().set_encoding(encoding);
            }
        }
    }
    Ok(version)
}

/// Whether the declared label already describes how the stream is being
/// decoded, so that no switch is needed.
///
/// Names match for the ordinary cases. The exception is UTF-16: XML 1.0
/// §4.3.3 requires a UTF-16 entity to carry a byte-order mark and lets the
/// mark, not the label, settle the byte order, while `Encoding::for_label`
/// answers the bare label `UTF-16` with UTF-16LE. A big-endian document that
/// honestly declares `encoding="UTF-16"` is correct XML, and must not be
/// refused for disagreeing with a label that never carried the byte order in
/// the first place.
fn encodings_agree(detected: &str, declared: &str) -> bool {
    detected == declared || (detected.starts_with("UTF-16") && declared.starts_with("UTF-16"))
}

/// Whether `set_encoding` can still be called safely.
///
/// `DecodingReader` drains its lookahead window the moment the parser asks for
/// a byte past it, and `set_encoding` asserts once that has happened. Feed
/// bytes decide how long a declaration is, so this is checked rather than
/// assumed — a padded declaration must produce a typed error, never a panic.
///
/// Two conditions have to hold, and both are observable from outside the
/// crate:
///
/// * the stream is being decoded as UTF-8, so one decoded byte is one source
///   byte and `buffer_position()` counts source bytes directly. For UTF-16 the
///   window holds only half as many characters as bytes, and the position
///   would understate how far the parser has read.
/// * the declaration ended within the window, whose size shrinks by the length
///   of any BOM the reader stripped.
fn declaration_fits_prefix(reader: &NsReader<DecodingReader<&[u8]>>, bytes: &[u8]) -> bool {
    if reader.get_ref().encoding().name() != "UTF-8" {
        return false;
    }
    let stripped = if bytes.starts_with(&UTF8_BOM) {
        UTF8_BOM.len() as u64
    } else {
        0
    };
    reader.buffer_position() <= PREFIX_CAP - stripped
}

/// One open element. Owned, because the namespace resolver's view of a name
/// dies the moment the reader advances.
struct Frame {
    namespace: Option<String>,
    local: String,
    /// The `xml:base` in scope for this element, already resolved. `None`
    /// means nothing overrode the retrieval URL.
    base: Option<Url>,
    text: String,
    /// Whether text inside this element is worth keeping. Ignored extensions
    /// buffer nothing, so a huge one costs no memory.
    collect: bool,
    /// Whether the accumulated text contains an entity this parser could not
    /// resolve, and so cannot serve as an identity (§4.8).
    unknown_entity: bool,
}

impl Frame {
    fn is(&self, namespace: Option<&str>, local: &str) -> bool {
        self.namespace.as_deref() == namespace && self.local == local
    }
}

/// Which mapping the document element selected (§4.2). Settled once, at the
/// document element, and never re-derived from a descendant.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Rss,
    Atom,
}

/// Where the walker is in the feed tree, derived from the whole open path
/// rather than from depth alone, and qualified by format. An extension
/// element with a matching local name therefore cannot substitute for a feed
/// field — its parent chain does not match — and neither can an unqualified
/// `<title>` inside an Atom `<feed>`, because the RSS arms of every match
/// below require both an RSS context *and* the empty namespace.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
    /// Nothing is open yet; the next element is the document element.
    Document,
    /// Inside `rss`.
    RssRoot,
    /// Inside `rss/channel`.
    RssChannel,
    /// Inside `rss/channel/item`.
    RssItem,
    /// Inside Atom `feed`, which carries feed-level fields directly.
    AtomFeed,
    /// Inside Atom `feed/entry`.
    AtomEntry,
    /// Anywhere else, including everything below an unrecognized element.
    Ignored,
}

fn context(format: Option<Format>, stack: &[Frame]) -> Context {
    // No document element yet, so nothing is open and nothing is decided.
    let Some(format) = format else {
        return Context::Document;
    };
    match (format, stack) {
        (_, []) => Context::Document,
        // The document element's own name was settled by dispatch, so the root
        // frame needs no second look here.
        (Format::Rss, [_]) => Context::RssRoot,
        (Format::Rss, [_, channel]) if channel.is(None, "channel") => Context::RssChannel,
        (Format::Rss, [_, channel, item])
            if channel.is(None, "channel") && item.is(None, "item") =>
        {
            Context::RssItem
        }
        (Format::Atom, [_]) => Context::AtomFeed,
        (Format::Atom, [_, entry]) if entry.is(Some(ATOM_NS), "entry") => Context::AtomEntry,
        _ => Context::Ignored,
    }
}

/// One item under construction.
#[derive(Default)]
struct ItemBuilder {
    item: ParsedItem,
    /// Set when an identity field could not be decoded; the item is counted
    /// as skipped instead of retained.
    rejected: bool,
    /// Every enclosure seen — RSS `<enclosure>` or Atom `link[rel=enclosure]`
    /// — usable or not (§4.7).
    enclosure_count: usize,
    /// Atom `published` and `updated`, kept as text until the entry closes.
    /// Atom's precedence is by element, not by document order: `published`
    /// wins even when `updated` was written first, and a `published` that will
    /// not parse yields `None` rather than falling through to `updated`
    /// (§4.3, §4.6).
    published_text: Option<String>,
    updated_text: Option<String>,
}

/// The attributes an RSS `<enclosure>` and an Atom `<link>` have in common,
/// gathered before either is interpreted.
///
/// `href_undecodable` is kept apart from an absent `href`: a URL that carries
/// an entity this parser will not resolve is an identity that cannot be
/// decoded consistently, which is a stronger fault than no URL at all (§4.8).
#[derive(Default)]
struct LinkCandidate {
    href: Option<String>,
    href_undecodable: bool,
    length: Option<String>,
    mime_type: Option<String>,
}

impl LinkCandidate {
    fn set_href(&mut self, value: Option<String>) {
        match value {
            Some(value) => self.href = Some(value),
            None => self.href_undecodable = true,
        }
    }
}

struct Walker<'a> {
    retrieval: &'a Url,
    version: XmlVersion,
    /// `None` until the document element names a mapping (§4.2).
    format: Option<Format>,
    stack: Vec<Frame>,
    /// The stack index of an open Atom `title` of type `xhtml`, whose markup
    /// is dropped and whose descendant text nodes are concatenated (§4.8).
    ///
    /// Descendant text is appended straight to that one frame rather than
    /// folded up level by level as each element closes. Folding would copy the
    /// accumulated text once per level, which a feed controls the depth of.
    xhtml_title: Option<usize>,
    feed: ParsedFeed,
    item: Option<ItemBuilder>,
    ordinal: usize,
    warnings: Vec<ParseWarning>,
    skipped: usize,
    saw_channel: bool,
    root_closed: bool,
}

impl<'a> Walker<'a> {
    fn new(retrieval: &'a Url) -> Self {
        Self {
            retrieval,
            version: XmlVersion::Implicit1_0,
            format: None,
            stack: Vec::new(),
            xhtml_title: None,
            feed: ParsedFeed::default(),
            item: None,
            ordinal: 0,
            warnings: Vec::new(),
            skipped: 0,
            saw_channel: false,
            root_closed: false,
        }
    }

    /// The `xml:base` in scope, falling back to the retrieval URL (§4.4).
    fn base(&self) -> &Url {
        self.stack
            .iter()
            .rev()
            .find_map(|frame| frame.base.as_ref())
            .unwrap_or(self.retrieval)
    }

    fn warn(&mut self, kind: WarningKind) {
        let item = self.item.as_ref().map(|_| self.ordinal);
        self.warnings.push(ParseWarning { item, kind });
    }

    fn open(
        &mut self,
        reader: &NsReader<DecodingReader<&[u8]>>,
        element: &BytesStart<'_>,
    ) -> Result<(), FeedError> {
        if self.stack.len() >= MAX_NESTING_DEPTH {
            return Err(malformed("document is nested too deeply"));
        }
        let (namespace, local) = expanded(reader, element.name())?;
        let parent = context(self.format, &self.stack);

        if parent == Context::Document {
            if self.root_closed {
                return Err(malformed("content after the document element"));
            }
            // §4.2 dispatches on the *expanded* name. Everything else, RSS 1.0
            // included, is refused by name rather than half-parsed.
            self.format = Some(match (namespace.as_deref(), local.as_str()) {
                (None, "rss") => Format::Rss,
                (Some(ATOM_NS), "feed") => Format::Atom,
                _ => return Err(FeedError::UnsupportedFormat),
            });
        }

        let base = self.element_base(reader, element)?;

        match (parent, namespace.as_deref(), local.as_str()) {
            (Context::RssRoot, None, "channel") => self.saw_channel = true,
            (Context::RssChannel, None, "item") | (Context::AtomFeed, Some(ATOM_NS), "entry") => {
                self.ordinal += 1;
                self.item = Some(ItemBuilder {
                    item: ParsedItem {
                        ordinal: self.ordinal,
                        ..ParsedItem::default()
                    },
                    ..ItemBuilder::default()
                });
            }
            (Context::RssItem, None, "enclosure") => {
                // Attributes resolve against the base in scope *here*, which
                // includes this element's own `xml:base`.
                let effective = base.as_ref().unwrap_or_else(|| self.base()).clone();
                self.read_enclosure(element, &effective)?;
            }
            (Context::AtomFeed | Context::AtomEntry, Some(ATOM_NS), "link") => {
                let effective = base.as_ref().unwrap_or_else(|| self.base()).clone();
                self.read_atom_link(element, &effective, parent == Context::AtomEntry)?;
            }
            (
                Context::RssChannel | Context::AtomFeed | Context::RssItem | Context::AtomEntry,
                Some(ITUNES_NS),
                "image",
            ) => {
                let effective = base.as_ref().unwrap_or_else(|| self.base()).clone();
                self.read_itunes_image(element, &effective, parent)?;
            }
            _ => {}
        }

        // Inside an xhtml title every element is markup to be dropped, and its
        // text belongs to the title's own frame, so nothing below it collects
        // for itself (§4.8).
        let collect = self.xhtml_title.is_none()
            && matches!(
                (parent, namespace.as_deref(), local.as_str()),
                (Context::RssChannel, None, "title" | "link")
                    | (
                        Context::RssItem,
                        None,
                        "guid" | "title" | "link" | "pubDate"
                    )
                    | (Context::AtomFeed, Some(ATOM_NS), "title")
                    | (
                        Context::AtomEntry,
                        Some(ATOM_NS),
                        "id" | "title" | "published" | "updated"
                    )
                    | (
                        Context::RssItem | Context::AtomEntry,
                        Some(ITUNES_NS),
                        "duration"
                    )
            );
        // `title` is the one element whose gathering depends on an attribute.
        if collect
            && matches!(
                (parent, namespace.as_deref(), local.as_str()),
                (
                    Context::AtomFeed | Context::AtomEntry,
                    Some(ATOM_NS),
                    "title"
                )
            )
            && is_xhtml_title(element, self.version)
        {
            self.xhtml_title = Some(self.stack.len());
        }

        self.stack.push(Frame {
            namespace,
            local,
            base,
            text: String::new(),
            collect,
            unknown_entity: false,
        });
        Ok(())
    }

    /// This element's own `xml:base`, resolved against the base already in
    /// scope. `None` when the element does not set one, or sets one that will
    /// not resolve — in which case the enclosing base simply stays in force.
    fn element_base(
        &self,
        reader: &NsReader<DecodingReader<&[u8]>>,
        element: &BytesStart<'_>,
    ) -> Result<Option<Url>, FeedError> {
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|_| malformed("invalid attribute"))?;
            // An unprefixed attribute is in no namespace by definition, so
            // only a prefixed one is worth expanding.
            if !attribute.key.as_ref().contains(':') {
                continue;
            }
            let (namespace, local) = expanded_attribute(reader, attribute.key)?;
            if namespace.as_deref() == Some(XML_NS) && local == "base" {
                let Some(value) = normalized(&attribute, self.version) else {
                    return Ok(None);
                };
                return Ok(resolved_url(Some(self.base()), &value));
            }
        }
        Ok(None)
    }

    /// `itunes:image href`, kept only when it resolves to an http(s) URL
    /// with a host — the same bar an enclosure is held to. The first one at
    /// each level wins; an item's own image is stored on the item and the
    /// feed's on the feed, so the caller can prefer the former.
    fn read_itunes_image(
        &mut self,
        element: &BytesStart<'_>,
        base: &Url,
        parent: Context,
    ) -> Result<(), FeedError> {
        let mut href = None;
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|_| malformed("invalid attribute"))?;
            if attribute.key.as_ref() == "href" {
                href = normalized(&attribute, self.version);
            }
        }
        let image = href
            .and_then(|value| resolved_url(Some(base), &value))
            .filter(crate::media::is_public_source_url);
        match parent {
            Context::RssItem | Context::AtomEntry => {
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.item.image, image);
                }
            }
            _ => set_once(&mut self.feed.image, image),
        }
        Ok(())
    }

    fn read_enclosure(&mut self, element: &BytesStart<'_>, base: &Url) -> Result<(), FeedError> {
        let mut candidate = LinkCandidate::default();
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|_| malformed("invalid attribute"))?;
            // `enclosure`'s attributes are unprefixed, and an unprefixed
            // attribute is in no namespace: an extension cannot smuggle a
            // `foo:url` in here.
            let key = attribute.key.as_ref();
            if key.contains(':') {
                continue;
            }
            let value = normalized(&attribute, self.version);
            match key {
                "url" => candidate.set_href(value),
                "length" => candidate.length = value,
                "type" => candidate.mime_type = value,
                _ => {}
            }
        }
        self.push_enclosure(candidate, base);
        Ok(())
    }

    /// One Atom `link`. `rel` defaults to `alternate` (RFC 4287 §4.2.7.2),
    /// applied before any selection, so a bare `<link href="..."/>` is the
    /// site link or the item link exactly as a spelled-out one would be
    /// (§4.3).
    fn read_atom_link(
        &mut self,
        element: &BytesStart<'_>,
        base: &Url,
        in_entry: bool,
    ) -> Result<(), FeedError> {
        let mut candidate = LinkCandidate::default();
        let mut rel = None;
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|_| malformed("invalid attribute"))?;
            let key = attribute.key.as_ref();
            if key.contains(':') {
                continue;
            }
            let value = normalized(&attribute, self.version);
            match key {
                "rel" => rel = value,
                "href" => candidate.set_href(value),
                "length" => candidate.length = value,
                "type" => candidate.mime_type = value,
                _ => {}
            }
        }

        match rel.as_deref().map_or("alternate", str::trim) {
            "enclosure" if in_entry => self.push_enclosure(candidate, base),
            "alternate" => self.push_alternate(candidate, base, in_entry),
            // `self`, `related`, `via`, a hub, an unregistered relation: not a
            // field this parser maps, so not a field whose href has to decode.
            _ => {}
        }
        Ok(())
    }

    /// Records an enclosure candidate, whatever element it came from.
    fn push_enclosure(&mut self, candidate: LinkCandidate, base: &Url) {
        let Some(builder) = self.item.as_mut() else {
            return;
        };
        builder.enclosure_count += 1;
        if candidate.href_undecodable {
            // An enclosure URL is an identity fallback, and an identity that
            // cannot be decoded consistently is not an identity (§4.8). This
            // runs *before* the "already have one" return: §4.8's "every href"
            // is unqualified, and a rule whose outcome depended on which
            // enclosure the feed happened to list first would be a rule about
            // line order rather than about identity.
            builder.rejected = true;
            self.warn(WarningKind::UnknownIdentityEntity);
            return;
        }
        if builder.item.enclosure.is_some() {
            // Already have one; §4.7 keeps the first and counts the rest.
            return;
        }
        let url = candidate
            .href
            .as_deref()
            .and_then(|href| resolved_url(Some(base), href))
            .filter(crate::media::is_public_source_url);
        match url {
            Some(url) => {
                let length = candidate
                    .length
                    .and_then(|raw| raw.trim().parse::<u64>().ok());
                let mime_type = candidate
                    .mime_type
                    .map(|raw| raw.trim().to_owned())
                    .filter(|raw| !raw.is_empty());
                builder.item.enclosure = Some(Enclosure {
                    url,
                    length,
                    mime_type,
                });
            }
            // A bad `length` never reaches here: it yields `None`, not a
            // discarded enclosure.
            None => self.warn(WarningKind::InvalidEnclosure),
        }
    }

    /// Records an Atom `alternate` link: the entry's link, or the feed's site
    /// link. First in document order wins.
    fn push_alternate(&mut self, candidate: LinkCandidate, base: &Url, in_entry: bool) {
        // Checked before anything asks whether the field is already filled, so
        // that good-then-bad and bad-then-good reach the same outcome.
        if candidate.href_undecodable {
            if in_entry {
                self.reject_identity();
            } else {
                self.warn_feed_identity();
            }
            return;
        }
        let url = candidate
            .href
            .as_deref()
            .and_then(|href| resolved_url(Some(base), href));
        if in_entry {
            if let Some(builder) = self.item.as_mut() {
                set_once(&mut builder.item.link, url);
            }
        } else {
            set_once(&mut self.feed.site_link, url);
        }
    }

    /// The frame character content belongs to: the innermost open element,
    /// unless an Atom `xhtml` title is gathering its descendants' text (§4.8).
    fn text_frame(&mut self) -> Option<&mut Frame> {
        match self.xhtml_title {
            Some(index) => self.stack.get_mut(index),
            None => self.stack.last_mut(),
        }
    }

    fn push_text(&mut self, text: &str) {
        if let Some(frame) = self.text_frame()
            && frame.collect
        {
            frame.text.push_str(text);
        }
    }

    fn push_reference(&mut self, reference: &BytesRef<'_>) {
        let resolved = if reference.is_char_ref() {
            reference
                .resolve_char_ref()
                .ok()
                .flatten()
                .map(String::from)
        } else {
            resolve_predefined_entity(reference).map(String::from)
        };
        let Some(frame) = self.text_frame() else {
            return;
        };
        match resolved {
            Some(text) => {
                if frame.collect {
                    frame.text.push_str(&text);
                }
            }
            None => {
                // Unknown-entity passthrough is confined to display fields: the
                // literal text stays, and the flag stops an identity field from
                // ever being built out of it (§4.8).
                frame.unknown_entity = true;
                if frame.collect {
                    frame.text.push('&');
                    frame.text.push_str(reference);
                    frame.text.push(';');
                }
            }
        }
    }

    fn close(&mut self) -> Result<(), FeedError> {
        let Some(frame) = self.stack.pop() else {
            return Err(malformed("end tag without a matching start tag"));
        };
        // Inside an Atom `xhtml` title the markup is dropped and only the text
        // nodes survive, already gathered on the title's own frame (§4.8).
        // Whitespace at a markup boundary is one of those text nodes, so it
        // survives too.
        if let Some(index) = self.xhtml_title {
            if index < self.stack.len() {
                return Ok(());
            }
            // This *is* the title, so the gathering is over and the frame goes
            // on to be mapped like any other.
            self.xhtml_title = None;
        }
        let Frame {
            namespace,
            local,
            base,
            text,
            unknown_entity,
            ..
        } = frame;
        let parent = context(self.format, &self.stack);
        if parent == Context::Document {
            self.root_closed = true;
        }

        match (parent, namespace.as_deref(), local.as_str()) {
            (Context::RssChannel, None, "title") | (Context::AtomFeed, Some(ATOM_NS), "title") => {
                set_once(&mut self.feed.title, trimmed(&text));
            }
            (Context::RssChannel, None, "link") => {
                if unknown_entity {
                    self.warn_feed_identity();
                } else {
                    let base = base.clone().unwrap_or_else(|| self.base().clone());
                    set_once(&mut self.feed.site_link, resolved_url(Some(&base), &text));
                }
            }
            (Context::RssChannel, None, "item") | (Context::AtomFeed, Some(ATOM_NS), "entry") => {
                self.finish_item();
            }
            (Context::RssItem, None, "guid") | (Context::AtomEntry, Some(ATOM_NS), "id") => {
                if unknown_entity {
                    self.reject_identity();
                } else if let Some(builder) = self.item.as_mut() {
                    // Stored exactly as assembled and decoded. Never trimmed:
                    // the surrounding whitespace is part of what the feed
                    // said, and identity may not be quietly rewritten.
                    set_once(&mut builder.item.guid, Some(text));
                }
            }
            (Context::RssItem, None, "title") | (Context::AtomEntry, Some(ATOM_NS), "title") => {
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.item.title, trimmed(&text));
                }
            }
            (Context::RssItem, None, "link") => {
                if unknown_entity {
                    self.reject_identity();
                } else {
                    let base = base.clone().unwrap_or_else(|| self.base().clone());
                    let link = resolved_url(Some(&base), &text);
                    if let Some(builder) = self.item.as_mut() {
                        set_once(&mut builder.item.link, link);
                    }
                }
            }
            (Context::RssItem, None, "pubDate") => {
                let parsed = parse_rfc2822(&text);
                let missing = self
                    .item
                    .as_ref()
                    .is_some_and(|builder| builder.item.published.is_none());
                if parsed.is_none() && missing && !text.trim().is_empty() {
                    self.warn(WarningKind::InvalidDate);
                }
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.item.published, parsed);
                }
            }
            // Atom's two timestamps are resolved together when the entry
            // closes, because `published` outranks `updated` whichever came
            // first in the document.
            (Context::AtomEntry, Some(ATOM_NS), "published") => {
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.published_text, Some(text));
                }
            }
            (Context::AtomEntry, Some(ATOM_NS), "updated") => {
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.updated_text, Some(text));
                }
            }
            (Context::RssItem | Context::AtomEntry, Some(ITUNES_NS), "duration") => {
                if let Some(builder) = self.item.as_mut() {
                    set_once(&mut builder.item.declared_duration, parse_duration(&text));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn reject_identity(&mut self) {
        if let Some(builder) = self.item.as_mut() {
            builder.rejected = true;
        }
        self.warn(WarningKind::UnknownIdentityEntity);
    }

    /// A **feed-level** href that will not decode: the field goes absent and
    /// the document stands.
    ///
    /// §4.6's list of feed-level faults is exhaustive and does not include
    /// this, and §4.8's remedy — fail the item — has no item to fail here.
    /// Nor should it invent one: a site link is identity for nothing. It never
    /// reaches `EpisodeKey::resolve`, so refusing to decode it costs the
    /// homepage URL and nothing else, and losing every episode over a
    /// homepage URL is not a trade §4.6 asks for.
    fn warn_feed_identity(&mut self) {
        self.warnings.push(ParseWarning {
            item: None,
            kind: WarningKind::UnknownIdentityEntity,
        });
    }

    fn finish_item(&mut self) {
        let Some(mut builder) = self.item.take() else {
            return;
        };
        // `published`, else `updated` (§4.3). A `published` that will not parse
        // stays `None` — it is still the element that answers the question, so
        // `updated` does not get a second turn.
        if let Some(text) = builder
            .published_text
            .as_deref()
            .or(builder.updated_text.as_deref())
        {
            let parsed = parse_rfc3339(text);
            if parsed.is_none() && !text.trim().is_empty() {
                self.warnings.push(ParseWarning {
                    item: Some(self.ordinal),
                    kind: WarningKind::InvalidDate,
                });
            }
            builder.item.published = parsed;
        }
        if builder.enclosure_count > 1 {
            self.warnings.push(ParseWarning {
                item: Some(self.ordinal),
                kind: WarningKind::ExtraEnclosures {
                    ignored: builder.enclosure_count - 1,
                },
            });
        }
        if builder.rejected {
            tracing::warn!(
                item = self.ordinal,
                "skipping feed item: identity contains an entity that cannot be resolved"
            );
            self.skipped += 1;
            return;
        }
        self.feed.items.push(builder.item);
    }

    fn finish(self) -> Result<ParseReport, FeedError> {
        if !self.stack.is_empty() {
            return Err(malformed("document ended with elements still open"));
        }
        // RSS keeps its items one level down, so an `<rss>` with no
        // `<channel>` is a document this parser has nothing to say about.
        // Atom's `<feed>` *is* the container, and an empty one is a valid,
        // itemless feed (§4.2, §4.6).
        let complete = match self.format {
            Some(Format::Rss) => self.saw_channel,
            Some(Format::Atom) => true,
            None => false,
        };
        if !self.root_closed || !complete {
            return Err(FeedError::UnsupportedFormat);
        }
        Ok(ParseReport {
            feed: self.feed,
            skipped: self.skipped,
            warnings: self.warnings,
        })
    }
}

/// Expands an element name, owning the result before the reader advances.
fn expanded(
    reader: &NsReader<DecodingReader<&[u8]>>,
    name: QName<'_>,
) -> Result<(Option<String>, String), FeedError> {
    let (resolved, local) = reader.resolver().resolve_element(name);
    Ok((own_namespace(resolved)?, local.as_ref().to_owned()))
}

/// Expands an attribute name. Unprefixed attributes are in no namespace,
/// which is why this cannot reuse [`expanded`].
fn expanded_attribute(
    reader: &NsReader<DecodingReader<&[u8]>>,
    name: QName<'_>,
) -> Result<(Option<String>, String), FeedError> {
    let (resolved, local) = reader.resolver().resolve_attribute(name);
    Ok((own_namespace(resolved)?, local.as_ref().to_owned()))
}

fn own_namespace(resolved: ResolveResult<'_>) -> Result<Option<String>, FeedError> {
    match resolved {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(namespace) => Ok(Some(namespace.as_ref().to_owned())),
        ResolveResult::Unknown(_) => Err(malformed("undeclared namespace prefix")),
    }
}

/// An attribute value with predefined entities expanded. `None` when it
/// contains a reference this parser will not resolve — custom DTD entities
/// are never expanded (§4.8).
fn normalized(attribute: &Attribute<'_>, version: XmlVersion) -> Option<String> {
    attribute
        .normalized_value_with(version, 1, resolve_predefined_entity)
        .ok()
        .map(|value| value.into_owned())
}

fn set_once<T>(slot: &mut Option<T>, value: Option<T>) {
    if slot.is_none() {
        *slot = value;
    }
}

fn trimmed(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// RFC 2822's obsolete-zone production (§4.3) defines `UT` and `GMT` but never
/// `UTC`, so a strict parser refuses an otherwise well-formed timestamp that
/// spells the zero offset that way. Real feeds spell it that way constantly —
/// Radio-T spells every `pubDate` `… UTC`, which blanked the PUBLISHED column
/// for the whole feed — so §4.3 accepts the spelling rather than the column.
///
/// The rewrite is deliberately narrow: only a trailing zone token that is
/// exactly `UTC` becomes the `+0000` it means, and nothing else about the
/// timestamp is relaxed. `UTCC` and `UTC+1` stay refused, because a zone this
/// parser cannot name is a zone it should not guess at.
fn parse_rfc2822(value: &str) -> Option<OffsetDateTime> {
    let value = value.trim();
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc2822) {
        return Some(parsed);
    }
    let (head, zone) = value.rsplit_once(char::is_whitespace)?;
    if !zone.eq_ignore_ascii_case("UTC") {
        return None;
    }
    OffsetDateTime::parse(&format!("{head} +0000"), &Rfc2822).ok()
}

fn parse_rfc3339(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value.trim(), &Rfc3339).ok()
}

/// Whether an Atom `title` declares `type="xhtml"`, the one value that changes
/// how the title's text is gathered.
///
/// An absent `type` is `text` (RFC 4287 §3.1.1), and `text` and `html` are
/// both simply the element's character content — `html`'s markup stays literal
/// because decoding it properly needs an HTML parser this project is not
/// adding (§4.8). Anything unrecognized is treated as `text`, which is the
/// reading that shows the feed's own characters rather than inventing
/// structure for them.
fn is_xhtml_title(element: &BytesStart<'_>, version: XmlVersion) -> bool {
    element.attributes().flatten().any(|attribute| {
        attribute.key.as_ref() == "type"
            && normalized(&attribute, version).is_some_and(|value| value.trim() == "xhtml")
    })
}

/// Resolves `value` against `base`, or parses it as absolute when there is no
/// base. A URL that will not resolve is absent, not fatal (§4.4).
fn resolved_url(base: Option<&Url>, value: &str) -> Option<Url> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match base {
        Some(base) => base.join(value).ok(),
        None => Url::parse(value).ok(),
    }
}

/// `itunes:duration` as `HH:MM:SS`, `MM:SS`, or bare seconds (§4.3).
///
/// Anything else is `None`: negatives, decimals, empty parts, four parts, a
/// seconds or minutes field that has a colon in front of it and is 60 or more,
/// and anything that overflows. Minutes in `MM:SS` may exceed 59 — a
/// 90-minute episode written `90:00` is a real thing feeds do.
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let mut parts = [0u64; 3];
    let mut count = 0usize;
    for part in value.split(':') {
        if count == parts.len() {
            return None;
        }
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        parts[count] = part.parse::<u64>().ok()?;
        count += 1;
    }

    let seconds = match count {
        0 => return None,
        1 => parts[0],
        2 => {
            if parts[1] >= 60 {
                return None;
            }
            parts[0].checked_mul(60)?.checked_add(parts[1])?
        }
        _ => {
            if parts[1] >= 60 || parts[2] >= 60 {
                return None;
            }
            parts[0]
                .checked_mul(3600)?
                .checked_add(parts[1].checked_mul(60)?)?
                .checked_add(parts[2])?
        }
    };
    Some(Duration::from_secs(seconds))
}

fn malformed(detail: &str) -> FeedError {
    FeedError::Malformed {
        detail: detail.to_owned(),
    }
}

/// Maps a quick-xml failure onto [`FeedError`].
///
/// The detail is a category and a byte offset, never a slice of the document:
/// a parse error is exactly the situation in which the bytes at fault are
/// most likely to be something a log should not repeat.
fn map_xml_error(error: &quick_xml::Error, position: u64) -> FeedError {
    let category = match error {
        quick_xml::Error::Encoding(_) => return FeedError::Encoding,
        quick_xml::Error::Io(io) if io.kind() == ErrorKind::InvalidData => {
            return FeedError::Encoding;
        }
        quick_xml::Error::Io(_) => "could not read the document",
        quick_xml::Error::Syntax(_) => "XML syntax error",
        quick_xml::Error::IllFormed(_) => "ill-formed XML",
        quick_xml::Error::InvalidAttr(_) => "invalid attribute",
        quick_xml::Error::Escape(_) => "invalid character or entity reference",
        quick_xml::Error::Namespace(_) => "invalid namespace declaration",
    };
    FeedError::Malformed {
        detail: format!("{category} at byte {position}"),
    }
}
