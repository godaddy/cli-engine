use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

use bytes::Bytes;
use reqwest::{Method, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::time;

use super::{AuthInjector, Error};
use crate::{CliCoreError, Result};

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

impl HttpClient {
    /// Creates a client builder.
    #[must_use]
    pub fn builder(base_url: impl Into<String>, auth: Arc<dyn AuthInjector>) -> HttpClientBuilder {
        HttpClientBuilder::new(base_url, auth)
    }

    /// Creates a client with default settings.
    #[must_use]
    pub fn new(base_url: impl Into<String>, auth: Arc<dyn AuthInjector>) -> Self {
        HttpClientBuilder::new(base_url, auth).build()
    }

    /// Sends GET and decodes a JSON response.
    pub async fn get<T: Default + DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.do_json(Method::GET, path, Option::<&()>::None).await
    }

    /// Sends GET and checks only for success.
    pub async fn get_without_response(&self, path: &str) -> Result<()> {
        self.do_empty(Method::GET, path, Option::<&()>::None).await
    }

    /// Sends POST with a JSON body and decodes a JSON response.
    pub async fn post<B: Serialize, T: Default + DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.do_json(Method::POST, path, Some(body)).await
    }

    /// Sends POST with a JSON body and checks only for success.
    pub async fn post_without_response<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        self.do_empty(Method::POST, path, Some(body)).await
    }

    /// Sends PUT with a JSON body and decodes a JSON response.
    pub async fn put<B: Serialize, T: Default + DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.do_json(Method::PUT, path, Some(body)).await
    }

    /// Sends PUT with a JSON body and checks only for success.
    pub async fn put_without_response<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        self.do_empty(Method::PUT, path, Some(body)).await
    }

    /// Sends PATCH with a JSON body and decodes a JSON response.
    pub async fn patch<B: Serialize, T: Default + DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.do_json(Method::PATCH, path, Some(body)).await
    }

    /// Sends PATCH with a JSON body and checks only for success.
    pub async fn patch_without_response<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        self.do_empty(Method::PATCH, path, Some(body)).await
    }

    /// Sends DELETE and checks for success.
    pub async fn delete(&self, path: &str) -> Result<()> {
        self.do_empty(Method::DELETE, path, Option::<&()>::None)
            .await
    }

    /// Sends DELETE with a JSON body and checks for success.
    pub async fn delete_with_body<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        self.do_empty(Method::DELETE, path, Some(body)).await
    }

    /// Sends GET and returns decoded JSON plus the ETag header.
    pub async fn get_etag<T: Default + DeserializeOwned>(&self, path: &str) -> Result<(T, String)> {
        let response = self.send_get_status_only_retry(path).await?;
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let value = self.decode_json_response(response, "GET", path).await?;
        Ok((value, etag))
    }

    /// Sends GET and returns only the ETag header after checking success.
    pub async fn get_etag_without_response(&self, path: &str) -> Result<String> {
        let response = self.send_get_status_only_retry(path).await?;
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        self.ensure_success_response(response, "GET", path).await?;
        Ok(etag)
    }

    /// Sends PUT with `If-Match` and decodes a JSON response.
    pub async fn put_if_match<B: Serialize, T: Default + DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        etag: &str,
    ) -> Result<T> {
        let response = self.send_put_if_match(path, body, etag).await?;
        self.decode_json_response(response, "PUT", path).await
    }

    /// Sends PUT with `If-Match` and checks only for success.
    pub async fn put_if_match_without_response<B: Serialize>(
        &self,
        path: &str,
        body: &B,
        etag: &str,
    ) -> Result<()> {
        let response = self.send_put_if_match(path, body, etag).await?;
        self.ensure_success_response(response, "PUT", path).await
    }

    /// Streams a raw GET response body into a writer.
    pub async fn get_raw(&self, path: &str, writer: &mut dyn Write) -> Result<()> {
        let response = self.send_get_raw_status_only_retry(path).await?;
        let (status, bytes) = self
            .read_and_log_response(response, "GET", path, false)
            .await?;
        if status.is_client_error() || status.is_server_error() {
            return Err(
                parse_error_body(status, &String::from_utf8_lossy(&bytes), "GET", path).into(),
            );
        }
        writer.write_all(&bytes)?;
        Ok(())
    }

    /// Sends GET and returns the raw response body as bytes.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let response = self.send_get_raw_status_only_retry(path).await?;
        let (status, bytes) = self
            .read_and_log_response(response, "GET", path, false)
            .await?;
        if status.is_client_error() || status.is_server_error() {
            return Err(
                parse_error_body(status, &String::from_utf8_lossy(&bytes), "GET", path).into(),
            );
        }
        Ok(bytes.to_vec())
    }

    /// Sends POST and streams the raw response body into a writer.
    pub async fn post_raw<B: Serialize>(
        &self,
        path: &str,
        body: Option<&B>,
        writer: &mut dyn Write,
    ) -> Result<()> {
        let response = self.send_post_raw_once(path, body).await?;
        let (status, bytes) = self
            .read_and_log_response(response, "POST", path, false)
            .await?;
        if status.is_client_error() || status.is_server_error() {
            return Err(
                parse_error_body(status, &String::from_utf8_lossy(&bytes), "POST", path).into(),
            );
        }
        writer.write_all(&bytes)?;
        Ok(())
    }

    /// Sends a raw-body request and decodes a JSON response.
    pub async fn do_raw<T: Default + DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        body: impl Into<Vec<u8>>,
    ) -> Result<T> {
        self.do_raw_optional_body(method, path, content_type, Some(body.into()))
            .await
    }

    /// Sends an optional raw-body request and decodes a JSON response.
    pub async fn do_raw_optional_body<T: Default + DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        body: Option<Vec<u8>>,
    ) -> Result<T> {
        let method_text = method.as_str().to_owned();
        let response = self.send_raw_once(method, path, content_type, body).await?;
        self.decode_json_response(response, &method_text, path)
            .await
    }

    /// Sends a raw-body request and checks only for success.
    pub async fn do_raw_without_response(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        body: impl Into<Vec<u8>>,
    ) -> Result<()> {
        self.do_raw_optional_body_without_response(method, path, content_type, Some(body.into()))
            .await
    }

    /// Sends an optional raw-body request and checks only for success.
    pub async fn do_raw_optional_body_without_response(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        body: Option<Vec<u8>>,
    ) -> Result<()> {
        let method_text = method.as_str().to_owned();
        let response = self.send_raw_once(method, path, content_type, body).await?;
        self.ensure_success_response(response, &method_text, path)
            .await
    }

    /// Sends a multipart file upload and decodes a JSON response.
    pub async fn post_multipart<T: Default + DeserializeOwned>(
        &self,
        path: &str,
        field_name: &str,
        file_path: &Path,
    ) -> Result<T> {
        self.post_multipart_with_fields(path, field_name, file_path, &BTreeMap::new())
            .await
    }

    /// Sends a multipart file upload and checks only for success.
    pub async fn post_multipart_without_response(
        &self,
        path: &str,
        field_name: &str,
        file_path: &Path,
    ) -> Result<()> {
        self.post_multipart_with_fields_without_response(
            path,
            field_name,
            file_path,
            &BTreeMap::new(),
        )
        .await
    }

    /// Sends a multipart file upload with fields and decodes a JSON response.
    pub async fn post_multipart_with_fields<T: Default + DeserializeOwned>(
        &self,
        path: &str,
        file_field: &str,
        file_path: &Path,
        fields: &BTreeMap<String, String>,
    ) -> Result<T> {
        let form = self.multipart_form(file_field, file_path, fields).await?;
        self.send_multipart(path, form).await
    }

    async fn multipart_form(
        &self,
        file_field: &str,
        file_path: &Path,
        fields: &BTreeMap<String, String>,
    ) -> Result<reqwest::multipart::Form> {
        let mut form = reqwest::multipart::Form::new();
        for (key, value) in fields {
            form = form.text(key.clone(), value.clone());
        }
        let file_name = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_owned();
        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(|err| CliCoreError::message(format!("transport: open file: {err}")))?;
        let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
        form = form.part(file_field.to_owned(), part);
        Ok(form)
    }

    /// Sends a multipart file upload with fields and checks only for success.
    pub async fn post_multipart_with_fields_without_response(
        &self,
        path: &str,
        file_field: &str,
        file_path: &Path,
        fields: &BTreeMap<String, String>,
    ) -> Result<()> {
        let form = self.multipart_form(file_field, file_path, fields).await?;
        self.send_multipart_without_response(path, form).await
    }

    /// Sends multipart form fields without a file and decodes a JSON response.
    pub async fn post_multipart_fields<T: Default + DeserializeOwned>(
        &self,
        path: &str,
        fields: &BTreeMap<String, String>,
    ) -> Result<T> {
        let mut form = reqwest::multipart::Form::new();
        for (key, value) in fields {
            form = form.text(key.clone(), value.clone());
        }
        self.send_multipart(path, form).await
    }

    /// Sends multipart form fields without a file and checks only for success.
    pub async fn post_multipart_fields_without_response(
        &self,
        path: &str,
        fields: &BTreeMap<String, String>,
    ) -> Result<()> {
        let mut form = reqwest::multipart::Form::new();
        for (key, value) in fields {
            form = form.text(key.clone(), value.clone());
        }
        self.send_multipart_without_response(path, form).await
    }

    /// Sends a GraphQL request and decodes the `data` envelope into a value.
    pub async fn post_graphql<T: DeserializeOwned + Default>(
        &self,
        path: &str,
        query: &str,
        variables: BTreeMap<String, Value>,
    ) -> Result<T> {
        self.post_graphql_optional_variables(path, query, Some(variables))
            .await
    }

    /// Sends a GraphQL request with optional variables and decodes `data`.
    pub async fn post_graphql_optional_variables<T: DeserializeOwned + Default>(
        &self,
        path: &str,
        query: &str,
        variables: Option<BTreeMap<String, Value>>,
    ) -> Result<T> {
        let mut result = T::default();
        self.post_graphql_optional_variables_into(path, query, variables, &mut result)
            .await?;
        Ok(result)
    }

    /// Sends a GraphQL request and checks only for GraphQL/HTTP success.
    pub async fn post_graphql_without_response(
        &self,
        path: &str,
        query: &str,
        variables: BTreeMap<String, Value>,
    ) -> Result<()> {
        self.post_graphql_optional_variables_without_response(path, query, Some(variables))
            .await
    }

    /// Sends a GraphQL request with optional variables and checks only for success.
    pub async fn post_graphql_optional_variables_without_response(
        &self,
        path: &str,
        query: &str,
        variables: Option<BTreeMap<String, Value>>,
    ) -> Result<()> {
        self.post_graphql_response_envelope(path, query, variables)
            .await?;
        Ok(())
    }

    /// Sends a GraphQL request and decodes `data` into an existing value.
    pub async fn post_graphql_into<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &str,
        variables: BTreeMap<String, Value>,
        result: &mut T,
    ) -> Result<()> {
        self.post_graphql_optional_variables_into(path, query, Some(variables), result)
            .await
    }

    /// Sends a GraphQL request with optional variables and decodes into an existing value.
    pub async fn post_graphql_optional_variables_into<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &str,
        variables: Option<BTreeMap<String, Value>>,
        result: &mut T,
    ) -> Result<()> {
        let envelope = self
            .post_graphql_response_envelope(path, query, variables)
            .await?;
        if let Some(data) = envelope.data
            && !data.is_null()
        {
            *result = serde_json::from_value(data).map_err(|err| {
                CliCoreError::message(format!("transport: decode graphql data: {err}"))
            })?;
        }
        Ok(())
    }

    async fn do_json<B: Serialize, T: Default + DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        let method_text = method.as_str().to_owned();
        let response = self.send_with_retry(method, path, body).await?;
        self.decode_json_response(response, &method_text, path)
            .await
    }

    async fn post_graphql_response_envelope(
        &self,
        path: &str,
        query: &str,
        variables: Option<BTreeMap<String, Value>>,
    ) -> Result<GraphQlEnvelope> {
        #[derive(Serialize)]
        struct Request<'query> {
            query: &'query str,
            variables: Option<BTreeMap<String, Value>>,
        }

        let envelope: GraphQlEnvelope = self.post(path, &Request { query, variables }).await?;
        if !envelope.errors.is_empty() {
            let message = envelope
                .errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(CliCoreError::message(format!("graphql: {message}")));
        }
        Ok(envelope)
    }

    async fn send_put_if_match<B: Serialize>(
        &self,
        path: &str,
        body: &B,
        etag: &str,
    ) -> Result<reqwest::Response> {
        let mut request = self
            .build_request(Method::PUT, path, Some(body))?
            .header(header::IF_MATCH, etag)
            .build()
            .map_err(|err| CliCoreError::message(format!("transport: create request: {err}")))?;
        self.inject_auth(&mut request).await?;
        self.log_request(&request);
        self.base
            .execute(request)
            .await
            .map_err(|err| CliCoreError::message(format!("transport: PUT {path}: {err}")))
    }

    async fn send_multipart<T: Default + DeserializeOwned>(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> Result<T> {
        let response = self.send_multipart_response(path, form).await?;
        self.decode_json_response(response, "POST", path).await
    }

    async fn send_multipart_without_response(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> Result<()> {
        let response = self.send_multipart_response(path, form).await?;
        self.ensure_success_response(response, "POST", path).await
    }

    async fn send_multipart_response(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = self
            .base
            .post(url)
            .header(header::USER_AGENT, self.user_agent.clone())
            .multipart(form);
        for (key, value) in &self.default_headers {
            builder = builder.header(key, value);
        }
        let mut request = builder
            .build()
            .map_err(|err| CliCoreError::message(format!("transport: create request: {err}")))?;
        self.inject_auth(&mut request).await?;
        self.log_request(&request);
        self.base
            .execute(request)
            .await
            .map_err(|err| CliCoreError::message(format!("transport: POST {path}: {err}")))
    }

    async fn do_empty<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<()> {
        let method_text = method.as_str().to_owned();
        let response = self.send_with_retry(method, path, body).await?;
        self.ensure_success_response(response, &method_text, path)
            .await
    }

    async fn send_raw_once(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url, path);
        let method_text = method.as_str().to_owned();
        let mut builder = self
            .base
            .request(method, url)
            .header(header::USER_AGENT, self.user_agent.clone());
        if let Some(body) = body {
            builder = builder.body(body);
        }
        if !content_type.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        for (key, value) in &self.default_headers {
            builder = builder.header(key, value);
        }
        let mut request = builder
            .build()
            .map_err(|err| CliCoreError::message(format!("transport: create request: {err}")))?;
        self.inject_auth(&mut request).await?;
        self.log_request(&request);
        self.base
            .execute(request)
            .await
            .map_err(|err| CliCoreError::message(format!("transport: {method_text} {path}: {err}")))
    }

    async fn send_get_raw_status_only_retry(&self, path: &str) -> Result<reqwest::Response> {
        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let backoff = BASE_BACKOFF * 2_u32.pow(u32::try_from(attempt - 1).unwrap_or(0));
                time::sleep(backoff).await;
            }

            match self.send_get_raw_once(path).await {
                Ok(response) => {
                    let status = response.status();
                    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        self.log_response("GET", path, status, response.headers(), None, None);
                        last_err = Some(CliCoreError::message(format!(
                            "transport: GET {}: status {}",
                            path,
                            status.as_u16()
                        )));
                        continue;
                    }
                    return Ok(response);
                }
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or_else(|| CliCoreError::message("transport: retry failed")))
    }

    async fn send_get_raw_once(&self, path: &str) -> Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = self
            .base
            .get(url)
            .header(header::USER_AGENT, self.user_agent.clone());
        for (key, value) in &self.default_headers {
            builder = builder.header(key, value);
        }
        let mut request = builder
            .build()
            .map_err(|err| CliCoreError::message(format!("transport: create request: {err}")))?;
        self.inject_auth(&mut request).await?;
        self.log_request(&request);
        self.base
            .execute(request)
            .await
            .map_err(|err| CliCoreError::message(format!("transport: GET {path}: {err}")))
    }

    async fn send_post_raw_once<B: Serialize>(
        &self,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response> {
        let mut request = self
            .build_request(Method::POST, path, body)?
            .build()
            .map_err(|err| CliCoreError::message(format!("transport: create request: {err}")))?;
        self.inject_auth(&mut request).await?;
        self.log_request(&request);
        self.base
            .execute(request)
            .await
            .map_err(|err| CliCoreError::message(format!("transport: POST {path}: {err}")))
    }

    async fn send_with_retry<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response> {
        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let backoff = BASE_BACKOFF * 2_u32.pow(u32::try_from(attempt - 1).unwrap_or(0));
                self.log_debug(
                    "retrying request",
                    [
                        ("attempt", (attempt + 1).to_string()),
                        ("backoff", format!("{backoff:?}")),
                    ],
                );
                time::sleep(backoff).await;
            }

            match self.send_once(method.clone(), path, body).await {
                Ok(response) => {
                    if retryable_status(method.clone(), response.status()) {
                        last_err = Some(
                            self.retryable_status_error(response, method.as_str(), path)
                                .await,
                        );
                        continue;
                    }
                    return Ok(response);
                }
                Err(err) if is_idempotent(&method) => {
                    last_err = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_err.unwrap_or_else(|| CliCoreError::message("transport: retry failed")))
    }

    async fn send_get_status_only_retry(&self, path: &str) -> Result<reqwest::Response> {
        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let backoff = BASE_BACKOFF * 2_u32.pow(u32::try_from(attempt - 1).unwrap_or(0));
                self.log_debug(
                    "retrying request",
                    [
                        ("attempt", (attempt + 1).to_string()),
                        ("backoff", format!("{backoff:?}")),
                    ],
                );
                time::sleep(backoff).await;
            }

            match self.send_once(Method::GET, path, Option::<&()>::None).await {
                Ok(response) => {
                    let status = response.status();
                    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        self.log_response("GET", path, status, response.headers(), None, None);
                        last_err = Some(CliCoreError::message(format!(
                            "transport: GET {}: status {}",
                            path,
                            status.as_u16()
                        )));
                        continue;
                    }
                    return Ok(response);
                }
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or_else(|| CliCoreError::message("transport: retry failed")))
    }

    async fn send_once<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response> {
        let mut request = self
            .build_request(method.clone(), path, body)?
            .build()
            .map_err(|err| CliCoreError::message(format!("transport: create request: {err}")))?;
        self.inject_auth(&mut request).await?;
        let method_text = method.as_str().to_owned();
        self.log_request(&request);
        self.base
            .execute(request)
            .await
            .map_err(|err| CliCoreError::message(format!("transport: {method_text} {path}: {err}")))
    }

    fn build_request<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::RequestBuilder> {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = self
            .base
            .request(method, url)
            .header(header::USER_AGENT, self.user_agent.clone());
        if let Some(body) = body {
            let body = serde_json::to_vec(body)
                .map_err(|err| CliCoreError::message(format!("transport: marshal body: {err}")))?;
            builder = builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        for (key, value) in &self.default_headers {
            builder = builder.header(key, value);
        }
        Ok(builder)
    }

    fn log_debug(
        &self,
        message: &'static str,
        fields: impl IntoIterator<Item = (&'static str, String)>,
    ) {
        if !self.logger.enabled() {
            return;
        }
        self.logger.debug(&TransportLogEvent {
            message,
            fields: fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
            headers: None,
            body: None,
        });
    }

    /// Emits an `http request` event capturing the built request's headers and
    /// in-memory body. Streaming bodies (e.g. multipart) report no body.
    ///
    /// Skips capture entirely when the logger is disabled, so the non-debug path
    /// does not clone headers or copy request bodies.
    fn log_request(&self, request: &reqwest::Request) {
        if !self.logger.enabled() {
            return;
        }
        self.logger.debug(&TransportLogEvent {
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

    /// Emits an `http response` event. When `body` is `None`, `body_bytes`
    /// records the payload size instead (used for raw/byte-download paths so
    /// large responses are not buffered into the log).
    fn log_response(
        &self,
        method: &str,
        path: &str,
        status: StatusCode,
        headers: &header::HeaderMap,
        body: Option<&[u8]>,
        body_bytes: Option<usize>,
    ) {
        if !self.logger.enabled() {
            return;
        }
        let mut fields = BTreeMap::from([
            ("status".to_owned(), status.as_u16().to_string()),
            ("method".to_owned(), method.to_owned()),
            ("url".to_owned(), format!("{}{}", self.base_url, path)),
        ]);
        if let Some(len) = body_bytes {
            fields.insert("body_bytes".to_owned(), len.to_string());
        }
        self.logger.debug(&TransportLogEvent {
            message: "http response",
            fields,
            headers: Some(header_pairs(headers)),
            body: body.map(<[u8]>::to_vec),
        });
    }

    /// Reads a response body once, emits the `http response` event, and returns
    /// the status and buffered bytes. `include_body` controls whether the body
    /// is attached to the log or only its size is reported.
    ///
    /// Returns the body as [`Bytes`] (a cheap clone of the buffer `reqwest`
    /// already owns) so callers decode without an extra copy. When the logger is
    /// disabled, response headers are not cloned and no event is built.
    async fn read_and_log_response(
        &self,
        response: reqwest::Response,
        method: &str,
        path: &str,
        include_body: bool,
    ) -> Result<(StatusCode, Bytes)> {
        let status = response.status();
        let logging = self.logger.enabled();
        let headers = logging.then(|| response.headers().clone());
        let body = response.bytes().await.map_err(|err| {
            CliCoreError::message(format!("transport: read response body: {err}"))
        })?;
        if let Some(headers) = headers {
            if include_body {
                self.log_response(method, path, status, &headers, Some(&body), None);
            } else {
                self.log_response(method, path, status, &headers, None, Some(body.len()));
            }
        }
        Ok((status, body))
    }

    async fn inject_auth(&self, request: &mut reqwest::Request) -> Result<()> {
        self.auth
            .inject(request)
            .await
            .map_err(|err| CliCoreError::message(format!("transport: auth inject: {err}")))
    }

    async fn decode_json_response<T: Default + DeserializeOwned>(
        &self,
        response: reqwest::Response,
        method: &str,
        path: &str,
    ) -> Result<T> {
        let (status, body) = self
            .read_and_log_response(response, method, path, true)
            .await?;
        if status.is_client_error() || status.is_server_error() {
            return Err(
                parse_error_body(status, &String::from_utf8_lossy(&body), method, path).into(),
            );
        }
        if status == StatusCode::NO_CONTENT {
            return Ok(T::default());
        }
        if body.trim_ascii() == b"null" {
            return Ok(T::default());
        }
        serde_json::from_slice::<T>(&body)
            .map_err(|err| CliCoreError::message(format!("transport: decode response: {err}")))
    }

    async fn ensure_success_response(
        &self,
        response: reqwest::Response,
        method: &str,
        path: &str,
    ) -> Result<()> {
        // A `*_without_response` call discards the body, so the body is only
        // needed to build an error message or to feed the logger. When neither
        // applies (non-error status, logging disabled), skip buffering it —
        // matching the pre-logging behavior of not reading success bodies.
        let is_error = response.status().is_client_error() || response.status().is_server_error();
        if !is_error && !self.logger.enabled() {
            return Ok(());
        }
        let (status, body) = self
            .read_and_log_response(response, method, path, true)
            .await?;
        if status.is_client_error() || status.is_server_error() {
            return Err(
                parse_error_body(status, &String::from_utf8_lossy(&body), method, path).into(),
            );
        }
        Ok(())
    }

    async fn retryable_status_error(
        &self,
        response: reqwest::Response,
        method: &str,
        path: &str,
    ) -> CliCoreError {
        let status = response.status();
        let headers = self.logger.enabled().then(|| response.headers().clone());
        match response.bytes().await {
            Ok(body) => {
                if let Some(headers) = &headers {
                    self.log_response(method, path, status, headers, Some(&body), None);
                }
                CliCoreError::message(format!(
                    "transport: {method} {path}: status {}: {}",
                    status.as_u16(),
                    String::from_utf8_lossy(&body)
                ))
            }
            Err(err) => {
                if let Some(headers) = &headers {
                    self.log_response(method, path, status, headers, None, None);
                }
                CliCoreError::message(format!(
                    "transport: {method} {path}: status {} (body read failed: {err})",
                    status.as_u16()
                ))
            }
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

fn retryable_status(method: Method, status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || (status.is_server_error() && is_idempotent(&method))
}

fn is_idempotent(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::DELETE)
}
