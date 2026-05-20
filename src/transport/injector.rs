use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::header::{AUTHORIZATION, COOKIE, HeaderName, HeaderValue};
use serde::Deserialize;
use tokio::{
    sync::Mutex,
    time::{Instant, timeout},
};

use crate::{AuthProvider, CliCoreError, Result};

/// Async callback that returns a token for request injection.
pub type TokenFunc =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync>;

#[async_trait::async_trait]
/// Mutates an outbound request with authentication material.
pub trait AuthInjector: Send + Sync + std::fmt::Debug {
    /// Adds auth headers or cookies to `request`.
    async fn inject(&self, request: &mut reqwest::Request) -> Result<()>;
}

/// Injects `Authorization: Bearer <token>`.
#[derive(Clone)]
pub struct BearerTokenInjector {
    token: TokenFunc,
}

impl std::fmt::Debug for BearerTokenInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerTokenInjector")
            .finish_non_exhaustive()
    }
}

impl BearerTokenInjector {
    /// Creates a bearer-token injector from an async token callback.
    #[must_use]
    pub fn new(token: TokenFunc) -> Self {
        Self { token }
    }
}

#[async_trait::async_trait]
impl AuthInjector for BearerTokenInjector {
    async fn inject(&self, request: &mut reqwest::Request) -> Result<()> {
        let token = (self.token)()
            .await
            .map_err(|err| CliCoreError::message(format!("transport: bearer inject: {err}")))?;
        set_header(request, AUTHORIZATION, &format!("Bearer {token}"))
    }
}

/// Appends a named token cookie to the request.
#[derive(Clone)]
pub struct CookieInjector {
    name: String,
    token: TokenFunc,
}

impl std::fmt::Debug for CookieInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieInjector")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl CookieInjector {
    /// Creates a cookie injector from a cookie name and async token callback.
    #[must_use]
    pub fn new(name: impl Into<String>, token: TokenFunc) -> Self {
        Self {
            name: name.into(),
            token,
        }
    }
}

#[async_trait::async_trait]
impl AuthInjector for CookieInjector {
    async fn inject(&self, request: &mut reqwest::Request) -> Result<()> {
        let token = (self.token)()
            .await
            .map_err(|err| CliCoreError::message(format!("transport: cookie inject: {err}")))?;
        let cookie = format!("{}={}", self.name, token);
        append_cookie(request, &cookie)
    }
}

/// Injects HTTP basic auth.
#[derive(Clone, Debug)]
pub struct BasicAuthInjector {
    username: String,
    password: String,
}

impl BasicAuthInjector {
    /// Creates a basic-auth injector.
    #[must_use]
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

#[async_trait::async_trait]
impl AuthInjector for BasicAuthInjector {
    async fn inject(&self, request: &mut reqwest::Request) -> Result<()> {
        let encoded = STANDARD.encode(format!("{}:{}", self.username, self.password));
        set_header(request, AUTHORIZATION, &format!("Basic {encoded}"))
    }
}

/// Injects an `x-api-key` header.
#[derive(Clone, Debug)]
pub struct ApiKeyInjector {
    key: String,
}

impl ApiKeyInjector {
    /// Creates an API-key injector.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }
}

#[async_trait::async_trait]
impl AuthInjector for ApiKeyInjector {
    async fn inject(&self, request: &mut reqwest::Request) -> Result<()> {
        set_header(request, HeaderName::from_static("x-api-key"), &self.key)
    }
}

/// Auth injector that leaves requests unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopInjector;

#[async_trait::async_trait]
impl AuthInjector for NoopInjector {
    async fn inject(&self, _request: &mut reqwest::Request) -> Result<()> {
        Ok(())
    }
}

/// Resolves a credential from an auth provider and injects its token as bearer auth.
#[derive(Clone, Debug)]
pub struct ProviderBearerInjector {
    provider: Arc<dyn AuthProvider>,
    env: String,
    token: Arc<Mutex<Option<String>>>,
}

impl ProviderBearerInjector {
    /// Creates a provider-backed bearer injector for one environment.
    #[must_use]
    pub fn new(provider: Arc<dyn AuthProvider>, env: impl Into<String>) -> Self {
        Self {
            provider,
            env: env.into(),
            token: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait::async_trait]
impl AuthInjector for ProviderBearerInjector {
    async fn inject(&self, request: &mut reqwest::Request) -> Result<()> {
        let mut cached = self.token.lock().await;
        if cached.as_deref().is_none_or(str::is_empty) {
            let credential = self
                .provider
                .get_credential(&self.env, "", "")
                .await
                .map_err(|err| {
                    CliCoreError::message(format!("transport: provider bearer: {err}"))
                })?;
            *cached = Some(credential.token);
        }
        let Some(token) = cached.as_ref() else {
            return Err(CliCoreError::message(
                "transport: provider bearer: empty token cache",
            ));
        };
        set_header(request, AUTHORIZATION, &format!("Bearer {token}"))
    }
}

/// OAuth2 client-credentials injector with in-memory token caching.
#[derive(Clone, Debug)]
pub struct ClientCredentialsInjector {
    token_url: String,
    client_id: String,
    client_secret: String,
    scopes: String,
    client: reqwest::Client,
    token: Arc<Mutex<Option<CachedToken>>>,
}

#[derive(Clone, Debug)]
struct CachedToken {
    token: String,
    expiry: Instant,
}

impl ClientCredentialsInjector {
    /// Creates a client-credentials injector.
    #[must_use]
    pub fn new(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        scopes: impl Into<String>,
    ) -> Self {
        Self {
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scopes: scopes.into(),
            client: reqwest::Client::new(),
            token: Arc::new(Mutex::new(None)),
        }
    }

    async fn get_token(&self) -> Result<String> {
        let mut cached = self.token.lock().await;
        if let Some(token) = cached.as_ref()
            && !token.token.is_empty()
            && Instant::now() < token.expiry
        {
            return Ok(token.token.clone());
        }

        let mut form = BTreeMap::from([
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
        ]);
        if !self.scopes.is_empty() {
            form.insert("scope", self.scopes.as_str());
        }

        let response = timeout(
            Duration::from_secs(30),
            self.client
                .post(&self.token_url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .form(&form)
                .send(),
        )
        .await
        .map_err(|_| CliCoreError::message("token request: timed out"))?
        .map_err(|err| CliCoreError::message(format!("token request: {err}")))?;

        if response.status() != reqwest::StatusCode::OK {
            return Err(CliCoreError::message(format!(
                "token request: status {}",
                response.status().as_u16()
            )));
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            #[serde(default)]
            access_token: String,
            #[serde(default)]
            expires_in: i64,
        }

        let token_response = response
            .json::<TokenResponse>()
            .await
            .map_err(|err| CliCoreError::message(format!("decode token response: {err}")))?;

        let expiry = if token_response.expires_in > 30 {
            Instant::now() + Duration::from_secs((token_response.expires_in - 30) as u64)
        } else {
            Instant::now()
        };
        *cached = Some(CachedToken {
            token: token_response.access_token.clone(),
            expiry,
        });
        Ok(token_response.access_token)
    }
}

#[async_trait::async_trait]
impl AuthInjector for ClientCredentialsInjector {
    async fn inject(&self, request: &mut reqwest::Request) -> Result<()> {
        let token = self.get_token().await.map_err(|err| {
            CliCoreError::message(format!("transport: client credentials inject: {err}"))
        })?;
        set_header(request, AUTHORIZATION, &format!("Bearer {token}"))
    }
}

fn set_header(request: &mut reqwest::Request, name: HeaderName, value: &str) -> Result<()> {
    let value = HeaderValue::from_str(value)
        .map_err(|err| CliCoreError::message(format!("transport: invalid header value: {err}")))?;
    request.headers_mut().insert(name, value);
    Ok(())
}

fn append_cookie(request: &mut reqwest::Request, cookie: &str) -> Result<()> {
    let value = match request.headers().get(COOKIE) {
        Some(existing) => {
            let existing = existing.to_str().map_err(|err| {
                CliCoreError::message(format!("transport: invalid header value: {err}"))
            })?;
            format!("{existing}; {cookie}")
        }
        None => cookie.to_owned(),
    };
    set_header(request, COOKIE, &value)
}
