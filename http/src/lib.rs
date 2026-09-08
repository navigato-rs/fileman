//! Experimental blocking HTTPS for Fileman and Starcom. Not enabled in either
//! application by default. Run requests on a worker, never inside a panic hook.
#![forbid(unsafe_code)]
mod provider;

use rustls::pki_types;
use std::{error, fmt, sync, time};

/// Bounds apply to the complete request and buffered response, not each read.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub timeout: time::Duration,
    pub response_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: time::Duration::from_secs(15),
            response_bytes: 1024 * 1024,
        }
    }
}

pub struct Client {
    agent: ureq::Agent,
    response_bytes: u64,
}
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum Error {
    InvalidUrl,
    InvalidLimits,
    TrustStore,
    RequestTooLarge,
    ResponseTooLarge,
    Redirect,
    Transport(ureq::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidUrl => "an HTTPS URL without credentials or fragments is required",
            Self::InvalidLimits => "invalid HTTPS request limits",
            Self::TrustStore => "no usable trust roots; TLS verification remains enabled",
            Self::RequestTooLarge => "HTTPS request body exceeds 64 KiB",
            Self::ResponseTooLarge => "HTTPS response exceeds its byte limit",
            Self::Redirect => "HTTPS redirects are not followed",
            Self::Transport(_) => "HTTPS request failed",
        })
    }
}
impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match *self {
            Self::Transport(ref error) => Some(error),
            _ => None,
        }
    }
}
impl From<ureq::Error> for Error {
    fn from(error: ureq::Error) -> Self {
        Self::Transport(error)
    }
}

impl Client {
    /// Read OS certificates; verification itself remains in Rustls/WebPKI.
    /// No fallback to a different TLS provider or an empty/insecure root store.
    pub fn from_system_roots(limits: Limits) -> Result<Self, Error> {
        let roots = rustls_native_certs::load_native_certs();
        if !roots.errors.is_empty() {
            return Err(Error::TrustStore);
        }
        Self::with_roots(roots.certs, limits)
    }
    /// Explicit trust for test fixtures and managed deployments. Replaces system
    /// roots rather than silently extending them. DER certificates only.
    pub fn with_roots(
        roots: Vec<pki_types::CertificateDer<'static>>,
        limits: Limits,
    ) -> Result<Self, Error> {
        if limits.timeout.is_zero()
            || limits.timeout > time::Duration::from_secs(60)
            || limits.response_bytes == 0
            || limits.response_bytes > 8 * 1024 * 1024
        {
            return Err(Error::InvalidLimits);
        }
        if roots.is_empty() {
            return Err(Error::TrustStore);
        }
        let mut store = rustls::RootCertStore::empty();
        for root in &roots {
            store.add(root.clone()).map_err(|_| Error::TrustStore)?;
        }
        let certs = roots
            .into_iter()
            .map(|root| ureq::tls::Certificate::from_der(&root).to_owned());
        let agent = ureq::Agent::config_builder()
            .https_only(true)
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .max_idle_connections(0)
            .proxy(None)
            .timeout_global(Some(limits.timeout))
            .timeout_connect(Some(limits.timeout))
            .timeout_resolve(Some(limits.timeout))
            .max_response_header_size(16 * 1024)
            .user_agent("navigato-tls-evaluation/0.1")
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .unversioned_rustls_crypto_provider(sync::Arc::new(provider::provider()))
                    .root_certs(certs.into())
                    .build(),
            )
            .build()
            .new_agent();
        Ok(Self {
            agent,
            response_bytes: limits.response_bytes,
        })
    }
    pub fn get(&self, url: &str) -> Result<Response, Error> {
        validate_url(url)?;
        self.finish(self.agent.get(url).call()?)
    }
    /// One bounded submission. No redirects, credentials, cookies or automatic
    /// retry queue. The caller owns consent and interpretation of HTTP status.
    pub fn post(&self, url: &str, content_type: &str, body: &[u8]) -> Result<Response, Error> {
        validate_url(url)?;
        if body.len() > 64 * 1024 {
            return Err(Error::RequestTooLarge);
        }
        self.finish(
            self.agent
                .post(url)
                .header("Content-Type", content_type)
                .send(body)?,
        )
    }
    fn finish(&self, mut response: ureq::http::Response<ureq::Body>) -> Result<Response, Error> {
        if response.status().is_redirection() {
            return Err(Error::Redirect);
        }
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(self.response_bytes + 1)
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => Error::ResponseTooLarge,
                other => Error::Transport(other),
            })?;
        if body.len() as u64 > self.response_bytes {
            return Err(Error::ResponseTooLarge);
        }
        Ok(Response { status, body })
    }
}
fn validate_url(url: &str) -> Result<(), Error> {
    if url.len() > 4096 || url.contains('#') || url.chars().any(char::is_control) {
        return Err(Error::InvalidUrl);
    }
    let uri: ureq::http::Uri = url.parse().map_err(|_| Error::InvalidUrl)?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none_or(str::is_empty)
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
    {
        return Err(Error::InvalidUrl);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
