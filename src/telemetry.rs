use crate::error::TelemetryError;
use tracing_subscriber::EnvFilter;
use url::Url;

pub fn subscriber(filter: &str) -> Result<impl tracing::Subscriber + Send + Sync, TelemetryError> {
    Ok(tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter)?)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .finish())
}

pub fn init() -> Result<(), TelemetryError> {
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "tenuto=info".into(),
        Err(error) => return Err(TelemetryError::Environment(error)),
    };
    tracing::subscriber::set_global_default(subscriber(&filter)?)?;
    Ok(())
}

/// Scheme, host, port and path only.
///
/// Query and userinfo are the two places a bearer secret hides, and §11 keeps
/// both out of normal diagnostics. An input that does not parse is reported as
/// a placeholder rather than echoed: the unparseable text may itself be the
/// secret.
pub fn redact_url(input: &str) -> String {
    let Ok(mut url) = Url::parse(input) else {
        return "<unparseable URL>".to_string();
    };
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

/// Feed-supplied text reaching a terminal, made safe **at the formatting
/// boundary only**: the cached title and the identity derived from it are
/// untouched, so nothing here changes what a later refresh compares against.
/// A newline would break the row layout and an escape sequence would reach
/// the terminal, so every character that can do either becomes a visible
/// escape; all the rest — Cyrillic, CJK, emoji — pass through exactly as
/// stored, since transliterating a title would make it someone else's title.
pub(crate) fn displayable(text: &str) -> String {
    if !text.chars().any(needs_escape) {
        return text.to_string();
    }
    text.chars()
        .map(|value| match value {
            '\n' => r"\n".to_string(),
            '\r' => r"\r".to_string(),
            '\t' => r"\t".to_string(),
            other if needs_escape(other) => format!("\\u{{{:x}}}", other as u32),
            other => other.to_string(),
        })
        .collect()
}

/// What a title may not carry into a row.
///
/// `char::is_control` alone is not the whole set: U+2028 LINE SEPARATOR and
/// U+2029 PARAGRAPH SEPARATOR are line breaks that it does not classify as
/// control characters, and the bidi overrides U+202A–U+202E can reorder
/// everything rendered after them — including the columns beside the title
/// — without being line breaks at all. Neither are the bidi **isolates**
/// U+2066–U+2069 (LRI/RLI/FSI/PDI): they reorder the same way the overrides
/// do, and are the half of the Trojan Source technique that survives in
/// modern Unicode, since isolates were added specifically so an override
/// could not leak its reordering past its own text — the isolate itself
/// still reorders whatever it wraps. U+200E/U+200F (LRM/RLM) and U+061C
/// (ALM) are direction marks rather than reordering ranges, but they are
/// invisible and feed-controlled, so they are escaped alongside the rest
/// for the same reason. A feed title is untrusted input on its way to a
/// terminal, so every one of these groups is escaped rather than displayed.
fn needs_escape(value: char) -> bool {
    value.is_control()
        || matches!(
            value,
            '\u{200e}'
                | '\u{200f}'
                | '\u{061c}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}
