//! HTTP transport helpers for command implementations.
//!
//! [`crate::transport::client::HttpClient`] wraps `reqwest` with the
//! conventions CLI commands usually need: auth injection, default headers,
//! user-agent handling, structured HTTP errors, idempotent retries, raw
//! response helpers, multipart helpers, ETag helpers, and GraphQL envelope
//! decoding.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::DetailedError;

/// HTTP client implementation.
pub mod client;
/// Request auth injectors.
pub mod injector;

pub use client::{
    HttpClient, HttpClientBuilder, NoopTransportLogger, TransportLogEvent, TransportLogger,
    set_default_user_agent,
};
pub use injector::{
    ApiKeyInjector, AuthInjector, BasicAuthInjector, BearerTokenInjector,
    ClientCredentialsInjector, CookieInjector, NoopInjector, ProviderBearerInjector, TokenFunc,
};

/// Structured HTTP error decoded from a backend response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    /// Error code. Backend errors are normalized to `HTTP_<status>`.
    pub code: String,
    /// Human-readable backend or transport error message.
    pub message: String,
    /// Optional backend system id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system: String,
    /// Optional request id returned by the backend.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_id: String,
}

impl DetailedError for Error {
    fn error_code(&self) -> Cow<'static, str> {
        Cow::Owned(self.code.clone())
    }

    fn error_system(&self) -> Option<Cow<'static, str>> {
        (!self.system.is_empty()).then(|| Cow::Owned(self.system.clone()))
    }

    fn error_request_id(&self) -> Option<Cow<'static, str>> {
        (!self.request_id.is_empty()).then(|| Cow::Owned(self.request_id.clone()))
    }
}
