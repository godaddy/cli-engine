use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

use reqwest::{StatusCode, header};
use serde_json::Value;

use super::{AuthInjector, Error};

mod methods;

const MAX_RETRIES: usize = 3;
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const BUILTIN_DEFAULT_USER_AGENT: &str = "cli/dev";
static DEFAULT_USER_AGENT: OnceLock<RwLock<String>> = OnceLock::new();

/// Sets the process-wide default user-agent for outbound requests.
///
/// Applies to subsequently created [`HttpClient`] values (those that do not set
/// their own via [`HttpClientBuilder::user_agent`]) and to the engine's other
/// outbound token traffic that reads this default — the PKCE provider's
/// token/refresh requests and the client-credentials injector. A per-client
/// user-agent still overrides it for that client.
pub fn set_default_user_agent(user_agent: impl Into<String>) {
    let lock =
        DEFAULT_USER_AGENT.get_or_init(|| RwLock::new(BUILTIN_DEFAULT_USER_AGENT.to_owned()));
    if let Ok(mut current) = lock.write() {
        *current = user_agent.into();
    }
}

/// Returns the process-wide default user-agent set via
/// [`set_default_user_agent`], or the builtin default when none was set.
///
/// Used by [`HttpClientBuilder`] and by the engine's OAuth token requests so
/// that all outbound traffic carries the same user-agent.
pub(crate) fn default_user_agent() -> String {
    DEFAULT_USER_AGENT
        .get_or_init(|| RwLock::new(BUILTIN_DEFAULT_USER_AGENT.to_owned()))
        .read()
        .map_or_else(
            |_| BUILTIN_DEFAULT_USER_AGENT.to_owned(),
            |value| value.clone(),
        )
}

/// Serializes unit tests that mutate the process-wide default user-agent so
/// they cannot observe one another's writes. Integration tests in
/// `tests/foundation.rs` run in a separate binary and use their own lock.
#[cfg(test)]
pub(crate) static UA_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Restores the process-wide default user-agent to the builtin on drop, so a
/// panicking assertion in a test that mutates it cannot leak the value into
/// later tests in this binary. Declare it after acquiring [`UA_TEST_LOCK`] so
/// the reset runs while the lock is still held.
#[cfg(test)]
pub(crate) struct RestoreDefaultUserAgent;

#[cfg(test)]
impl Drop for RestoreDefaultUserAgent {
    fn drop(&mut self) {
        set_default_user_agent(BUILTIN_DEFAULT_USER_AGENT);
    }
}

static DEFAULT_TRANSPORT_LOGGER: OnceLock<RwLock<Arc<dyn TransportLogger>>> = OnceLock::new();

fn default_transport_logger_lock() -> &'static RwLock<Arc<dyn TransportLogger>> {
    DEFAULT_TRANSPORT_LOGGER.get_or_init(|| RwLock::new(Arc::new(NoopTransportLogger)))
}

/// Sets the process-wide default transport logger for outbound HTTP traffic.
///
/// Applies to subsequently created [`HttpClient`] values (those that do not set
/// their own via [`HttpClientBuilder::logger`]) and to the free
/// [`super::debug_log_reqwest_request`] / [`super::debug_log_reqwest_response`]
/// helpers used by code that talks to `reqwest` directly.
///
/// The CLI installs a logger from this setter when `--debug` selects the
/// `transport` component, so command handlers get request/response diagnostics
/// without any per-command wiring. A per-client logger still overrides it for
/// that client.
pub fn set_default_transport_logger(logger: Arc<dyn TransportLogger>) {
    // Recover from a poisoned lock (a panic while a writer held it) instead of
    // silently doing nothing, which would leave a stale logger installed and
    // make `--debug transport` appear ineffective.
    let mut current = default_transport_logger_lock()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *current = logger;
}

/// Returns the process-wide default transport logger set via
/// [`set_default_transport_logger`], or a [`NoopTransportLogger`] when none was
/// set.
#[must_use]
pub fn default_transport_logger() -> Arc<dyn TransportLogger> {
    default_transport_logger_lock()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Logs a `reqwest::Request` to the process-wide default transport logger.
///
/// This is the bridge for code that talks to `reqwest` directly — bare clients
/// or progenitor-generated clients that cannot use [`HttpClient`] — so a single
/// `--debug`-controlled trace can still cover them. Captures the request method,
/// URL, headers, and in-memory body. Pairs with [`debug_log_reqwest_response`].
/// It is a no-op (no header clone or body copy) unless an enabled logger has
/// been installed via [`set_default_transport_logger`].
pub fn debug_log_reqwest_request(request: &reqwest::Request) {
    let logger = default_transport_logger();
    if !logger.enabled() {
        return;
    }
    logger.debug(&TransportLogEvent {
        message: "http request",
        fields: BTreeMap::from([
            ("method".to_owned(), request.method().as_str().to_owned()),
            ("url".to_owned(), request.url().as_str().to_owned()),
        ]),
        headers: Some(header_pairs(request.headers())),
        body: request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map(<[u8]>::to_vec),
    });
}

/// Logs an HTTP response (status, headers, body) to the process-wide default
/// transport logger.
///
/// Companion to [`debug_log_reqwest_request`] for `reqwest`-direct call sites.
/// The caller passes the already-read response body. It is a no-op (no header
/// clone or body copy) unless an enabled logger has been installed via
/// [`set_default_transport_logger`].
pub fn debug_log_reqwest_response(status: StatusCode, headers: &header::HeaderMap, body: &[u8]) {
    let logger = default_transport_logger();
    if !logger.enabled() {
        return;
    }
    logger.debug(&TransportLogEvent {
        message: "http response",
        fields: BTreeMap::from([("status".to_owned(), status.as_u16().to_string())]),
        headers: Some(header_pairs(headers)),
        body: Some(body.to_vec()),
    });
}

#[derive(serde::Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Default, serde::Deserialize)]
struct GraphQlEnvelope {
    data: Option<Value>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

/// Structured debug event emitted by [`TransportLogger`].
///
/// `message` and `fields` are the stable breadcrumb surface (method, url,
/// status, retry attempt). `headers` and `body` carry the raw, un-redacted
/// request or response payload when one is available; loggers that print these
/// (such as [`StderrTransportLogger`](super::StderrTransportLogger)) are
/// responsible for redacting sensitive headers.
#[derive(Clone, Debug, Default)]
pub struct TransportLogEvent {
    /// Event name such as `http request` or `retrying request`.
    pub message: &'static str,
    /// Stable event fields.
    pub fields: BTreeMap<String, String>,
    /// Raw header name/value pairs for the request or response, when known.
    pub headers: Option<Vec<(String, String)>>,
    /// Raw request or response body bytes, when captured. Streaming and
    /// byte-download responses omit this and report a `body_bytes` field
    /// instead to avoid buffering large payloads into the log.
    pub body: Option<Vec<u8>>,
}

/// Debug logger interface for transport events.
pub trait TransportLogger: Send + Sync + std::fmt::Debug {
    /// Records one debug event.
    fn debug(&self, event: &TransportLogEvent);

    /// Whether this logger records anything.
    ///
    /// Defaults to `true`. The transport checks this before capturing request
    /// and response headers/bodies, so a logger that returns `false` (such as
    /// [`NoopTransportLogger`]) keeps the common non-debug path free of those
    /// clones.
    fn enabled(&self) -> bool {
        true
    }
}

/// Logger that intentionally drops transport events.
#[derive(Clone, Debug, Default)]
pub struct NoopTransportLogger;

impl TransportLogger for NoopTransportLogger {
    fn debug(&self, _event: &TransportLogEvent) {}

    fn enabled(&self) -> bool {
        false
    }
}

/// Authenticated HTTP client for CLI command implementations.
///
/// The client covers the transport behavior command authors usually need: auth
/// injection, JSON request/response helpers, structured HTTP errors,
/// idempotent retries, ETag helpers, raw streaming helpers, multipart helpers,
/// and GraphQL envelope decoding.
#[derive(Clone, Debug)]
pub struct HttpClient {
    base: reqwest::Client,
    base_url: String,
    auth: Arc<dyn AuthInjector>,
    user_agent: String,
    default_headers: BTreeMap<String, String>,
    logger: Arc<dyn TransportLogger>,
}

/// Builder for [`HttpClient`].
#[derive(Clone, Debug)]
pub struct HttpClientBuilder {
    base_url: String,
    auth: Arc<dyn AuthInjector>,
    user_agent: String,
    default_headers: BTreeMap<String, String>,
    logger: Arc<dyn TransportLogger>,
}

impl HttpClientBuilder {
    /// Creates a builder with a base URL and auth injector.
    #[must_use]
    pub fn new(base_url: impl Into<String>, auth: Arc<dyn AuthInjector>) -> Self {
        Self {
            base_url: base_url.into(),
            auth,
            user_agent: default_user_agent(),
            default_headers: BTreeMap::new(),
            logger: default_transport_logger(),
        }
    }

    /// Sets the user-agent for this client.
    #[must_use]
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    /// Alias for [`HttpClientBuilder::user_agent`] for migration readability.
    #[must_use]
    pub fn with_user_agent(self, user_agent: impl Into<String>) -> Self {
        self.user_agent(user_agent)
    }

    /// Sets headers sent on every request.
    #[must_use]
    pub fn default_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.default_headers = headers;
        self
    }

    /// Alias for [`HttpClientBuilder::default_headers`] for migration readability.
    #[must_use]
    pub fn with_default_headers(self, headers: BTreeMap<String, String>) -> Self {
        self.default_headers(headers)
    }

    /// Sets the transport debug logger.
    #[must_use]
    pub fn logger(mut self, logger: Arc<dyn TransportLogger>) -> Self {
        self.logger = logger;
        self
    }

    /// Alias for [`HttpClientBuilder::logger`] for migration readability.
    #[must_use]
    pub fn with_logger(self, logger: Arc<dyn TransportLogger>) -> Self {
        self.logger(logger)
    }

    /// Builds the client.
    #[must_use]
    pub fn build(self) -> HttpClient {
        HttpClient {
            base: reqwest::Client::new(),
            base_url: self.base_url,
            auth: self.auth,
            user_agent: self.user_agent,
            default_headers: self.default_headers,
            logger: self.logger,
        }
    }
}

/// Converts a `reqwest` header map into owned name/value pairs for logging.
///
/// Header values that are not valid UTF-8 are rendered as a byte-count
/// placeholder rather than dropped, so the trace still shows the header exists.
fn header_pairs(headers: &header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value.to_str().map_or_else(
                |_| format!("<{} non-utf8 bytes>", value.as_bytes().len()),
                str::to_owned,
            );
            (name.as_str().to_owned(), value)
        })
        .collect()
}

/// Converts a non-success HTTP response into the shared transport error shape.
///
/// If the response body already contains an API-style error document, the
/// service message is preserved and the HTTP status is normalized into the
/// error code. Otherwise the method, path, status, and response body are folded
/// into a readable fallback message.
pub async fn parse_error_response(response: reqwest::Response, method: &str, path: &str) -> Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    parse_error_body(status, &body, method, path)
}

fn parse_error_body(status: StatusCode, body: &str, method: &str, path: &str) -> Error {
    if let Ok(mut api_error) = serde_json::from_str::<Error>(body)
        && !api_error.message.is_empty()
    {
        api_error.code = format!("HTTP_{}", status.as_u16());
        return api_error;
    }
    Error {
        code: format!("HTTP_{}", status.as_u16()),
        message: format!("{} {}: {} {}", method, path, status.as_u16(), body),
        system: String::new(),
        request_id: String::new(),
    }
}

fn retryable_status(method: reqwest::Method, status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || (status.is_server_error() && is_idempotent(&method))
}

fn is_idempotent(method: &reqwest::Method) -> bool {
    matches!(
        *method,
        reqwest::Method::GET | reqwest::Method::HEAD | reqwest::Method::DELETE
    )
}
