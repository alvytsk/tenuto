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
