//! OAuth 2.0 PKCE authentication provider.
//!
//! Implements the browser-based Authorization Code + PKCE flow (RFC 7636).
//! Tokens are stored in the system keychain via the `keyring` crate.
//!
//! # Setup
//!
//! ```no_run
//! use std::sync::Arc;
//! use cli_engine::{CliConfig, auth::pkce::PkceAuthProvider};
//!
//! let provider = Arc::new(PkceAuthProvider::new(
//!     "my-provider",
//!     "https://auth.example.com/oauth/authorize",
//!     "https://auth.example.com/oauth/token",
//!     "my-client-id",
//!     &["openid", "profile"],
//! ));
//!
//! let config = CliConfig::new("mycli", "My CLI", "mycli")
//!     .with_default_auth_provider("my-provider")
//!     .with_auth_provider(provider);
//! ```
//!
//! Override endpoints and client ID via environment variables:
//! - `<PREFIX>_OAUTH_CLIENT_ID`
//! - `<PREFIX>_OAUTH_AUTH_URL`
//! - `<PREFIX>_OAUTH_TOKEN_URL`
//!
//! where `<PREFIX>` is the provider name uppercased and with `-` replaced by `_`.

use std::{
    collections::HashMap,
    net::{SocketAddr, TcpListener},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::{
    Credential, Result,
    auth::AuthProvider,
    error::CliCoreError,
};

const REDIRECT_PORT_DEFAULT: u16 = 7443;
const TOKEN_EXPIRY_BUFFER_SECS: i64 = 30;

/// Stored token with expiry tracking.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    expires_at: i64,
    refresh_token: Option<String>,
}

impl StoredToken {
    fn is_valid(&self) -> bool {
        let now = Utc::now().timestamp();
        self.expires_at - TOKEN_EXPIRY_BUFFER_SECS > now
    }
}

/// OAuth 2.0 PKCE authentication provider.
///
/// Stores one token per `(env, provider)` pair in the system keychain.
/// The keychain service name is `<app_id>/<provider>/<env>`.
#[derive(Debug)]
pub struct PkceAuthProvider {
    name: String,
    auth_url: String,
    token_url: String,
    client_id: String,
    scopes: Vec<String>,
    redirect_port: u16,
    app_id: String,
    env_prefix: String,
    /// In-process token cache keyed by env.
    cache: Arc<RwLock<HashMap<String, StoredToken>>>,
}

impl PkceAuthProvider {
    /// Creates a new PKCE provider.
    ///
    /// - `name`: Provider registration name (e.g. `"primary"`)
    /// - `auth_url`: Authorization endpoint
    /// - `token_url`: Token endpoint
    /// - `client_id`: OAuth client ID
    /// - `scopes`: Default OAuth scopes
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        auth_url: impl Into<String>,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        scopes: &[impl AsRef<str>],
    ) -> Self {
        let name = name.into();
        let env_prefix = name.to_uppercase().replace('-', "_");
        Self {
            name,
            auth_url: auth_url.into(),
            token_url: token_url.into(),
            client_id: client_id.into(),
            scopes: scopes.iter().map(|s| s.as_ref().to_owned()).collect(),
            redirect_port: REDIRECT_PORT_DEFAULT,
            app_id: String::new(),
            env_prefix,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Sets the local redirect server port (default: 7443).
    #[must_use]
    pub fn with_redirect_port(mut self, port: u16) -> Self {
        self.redirect_port = port;
        self
    }

    /// Sets the application id used as the keychain service prefix.
    #[must_use]
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = app_id.into();
        self
    }

    /// Adds extra scopes beyond the default set.
    #[must_use]
    pub fn with_extra_scopes(mut self, scopes: &[impl AsRef<str>]) -> Self {
        self.scopes
            .extend(scopes.iter().map(|s| s.as_ref().to_owned()));
        self
    }

    fn effective_client_id(&self) -> String {
        let key = format!("{}_OAUTH_CLIENT_ID", self.env_prefix);
        std::env::var(&key).unwrap_or_else(|_| self.client_id.clone())
    }

    fn effective_auth_url(&self) -> String {
        let key = format!("{}_OAUTH_AUTH_URL", self.env_prefix);
        std::env::var(&key).unwrap_or_else(|_| self.auth_url.clone())
    }

    fn effective_token_url(&self) -> String {
        let key = format!("{}_OAUTH_TOKEN_URL", self.env_prefix);
        std::env::var(&key).unwrap_or_else(|_| self.token_url.clone())
    }

    fn keychain_service(&self, env: &str) -> String {
        if self.app_id.is_empty() {
            format!("{}/{}", self.name, env)
        } else {
            format!("{}/{}/{}", self.app_id, self.name, env)
        }
    }

    fn keychain_user(&self) -> &str {
        "token"
    }

    fn load_token_from_keychain(&self, env: &str) -> Option<StoredToken> {
        let entry = keyring::Entry::new(&self.keychain_service(env), self.keychain_user()).ok()?;
        let json = entry.get_password().ok()?;
        serde_json::from_str(&json).ok()
    }

    fn save_token_to_keychain(&self, env: &str, token: &StoredToken) -> Result<()> {
        let entry =
            keyring::Entry::new(&self.keychain_service(env), self.keychain_user()).map_err(
                |err| CliCoreError::message(format!("keychain access failed: {err}")),
            )?;
        let json = serde_json::to_string(token).map_err(CliCoreError::from)?;
        entry
            .set_password(&json)
            .map_err(|err| CliCoreError::message(format!("keychain write failed: {err}")))?;
        Ok(())
    }

    fn delete_token_from_keychain(&self, env: &str) {
        if let Ok(entry) = keyring::Entry::new(&self.keychain_service(env), self.keychain_user()) {
            drop(entry.delete_credential());
        }
    }

    async fn cached_token(&self, env: &str) -> Option<StoredToken> {
        let cache = self.cache.read().await;
        cache.get(env).filter(|t| t.is_valid()).cloned()
    }

    async fn store_cached_token(&self, env: &str, token: StoredToken) {
        let mut cache = self.cache.write().await;
        cache.insert(env.to_owned(), token);
    }

    async fn resolve_token(&self, env: &str) -> Result<StoredToken> {
        if let Some(token) = self.cached_token(env).await {
            return Ok(token);
        }
        if let Some(token) = self.load_token_from_keychain(env) {
            if token.is_valid() {
                self.store_cached_token(env, token.clone()).await;
                return Ok(token);
            }
            if let Some(refresh_token) = &token.refresh_token.clone()
                && let Ok(refreshed) = self.refresh_access_token(refresh_token).await
            {
                self.save_token_to_keychain(env, &refreshed)?;
                self.store_cached_token(env, refreshed.clone()).await;
                return Ok(refreshed);
            }
        }
        self.run_pkce_flow(env).await
    }

    async fn run_pkce_flow(&self, env: &str) -> Result<StoredToken> {
        let (code_verifier, code_challenge) = pkce_challenge();
        let state = random_state();
        let client_id = self.effective_client_id();
        let auth_url = self.effective_auth_url();
        let redirect_uri = format!("http://127.0.0.1:{}/callback", self.redirect_port);
        let scope = self.scopes.join(" ");

        let auth_params = [
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", &redirect_uri),
            ("scope", &scope),
            ("state", &state),
            ("code_challenge", &code_challenge),
            ("code_challenge_method", "S256"),
        ];
        let url = url::Url::parse_with_params(&auth_url, &auth_params)
            .map_err(|err| CliCoreError::message(format!("invalid auth URL: {err}")))?;

        // Start the local callback server before opening the browser so the
        // redirect lands as soon as the user approves.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], self.redirect_port)))
            .map_err(|err| {
                CliCoreError::message(format!(
                    "failed to bind callback server on port {}: {err}",
                    self.redirect_port
                ))
            })?;

        tracing::info!("Opening browser for authentication…");
        tracing::info!("If the browser does not open, visit:\n  {url}");
        drop(open::that(url.as_str()));

        let code = wait_for_callback(listener, &state, Duration::from_secs(120)).await?;
        self.exchange_code_for_token(&code, &code_verifier, env).await
    }

    async fn exchange_code_for_token(
        &self,
        code: &str,
        code_verifier: &str,
        env: &str,
    ) -> Result<StoredToken> {
        let redirect_uri = format!("http://127.0.0.1:{}/callback", self.redirect_port);
        let client_id = self.effective_client_id();
        let token_url = self.effective_token_url();

        let params = [
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("redirect_uri", &redirect_uri),
            ("code", code),
            ("code_verifier", code_verifier),
        ];
        let response = reqwest::Client::new()
            .post(&token_url)
            .form(&params)
            .send()
            .await
            .map_err(|err| CliCoreError::message(format!("token request failed: {err}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CliCoreError::message(format!(
                "token endpoint returned {status}: {body}"
            )));
        }

        parse_token_response(response, env).await
    }

    async fn refresh_access_token(&self, refresh_token: &str) -> Result<StoredToken> {
        let client_id = self.effective_client_id();
        let token_url = self.effective_token_url();
        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", &client_id),
            ("refresh_token", refresh_token),
        ];
        let response = reqwest::Client::new()
            .post(&token_url)
            .form(&params)
            .send()
            .await
            .map_err(|err| CliCoreError::message(format!("token refresh failed: {err}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CliCoreError::message(format!(
                "refresh endpoint returned {status}: {body}"
            )));
        }

        parse_token_response(response, "").await
    }
}

#[async_trait]
impl AuthProvider for PkceAuthProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        let token = self.resolve_token(env).await?;
        Ok(Credential {
            token: token.access_token,
            env: env.to_owned(),
            provider: self.name.clone(),
            expires_at: chrono::DateTime::from_timestamp(token.expires_at, 0)
                .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_default(),
            ..Credential::default()
        })
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        let Some(token) = self.load_token_from_keychain(env) else {
            return Err(CliCoreError::message(format!(
                "not logged in for environment {env:?}"
            )));
        };
        Ok(Credential {
            token: token.access_token,
            env: env.to_owned(),
            provider: self.name.clone(),
            expires_at: chrono::DateTime::from_timestamp(token.expires_at, 0)
                .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_default(),
            ..Credential::default()
        })
    }

    async fn logout(&self, env: &str) -> Result<()> {
        self.delete_token_from_keychain(env);
        let mut cache = self.cache.write().await;
        cache.remove(env);
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        // Keyring doesn't support listing; return cache keys as a hint.
        let cache = self.cache.read().await;
        Ok(cache.keys().cloned().collect())
    }
}

/// Generates a PKCE code verifier and SHA-256 code challenge.
fn pkce_challenge() -> (String, String) {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);
    (verifier, challenge)
}

/// Generates a random OAuth state parameter.
fn random_state() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Waits for the OAuth callback on the given listener, validates state.
async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    use std::io::{Read, Write};

    let expected_state = expected_state.to_owned();
    let result = tokio::time::timeout(timeout, async move {
        tokio::task::spawn_blocking(move || {
            let (mut stream, _) = listener.accept().map_err(|err| {
                CliCoreError::message(format!("callback server accept failed: {err}"))
            })?;
            let mut buf = [0_u8; 4096];
            let n = stream
                .read(&mut buf)
                .map_err(|err| CliCoreError::message(format!("callback read failed: {err}")))?;
            let request = String::from_utf8_lossy(&buf[..n]);

            let code = extract_query_param(&request, "code");
            let state = extract_query_param(&request, "state");

            let html_response = if state.as_deref() == Some(&expected_state) && code.is_some() {
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                 <html><body>Authentication successful. You may close this window.</body></html>"
            } else {
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n\
                 <html><body>Authentication failed. Please try again.</body></html>"
            };
            drop(stream.write_all(html_response.as_bytes()));

            if state.as_deref() != Some(expected_state.as_str()) {
                return Err(CliCoreError::message("OAuth state mismatch — possible CSRF"));
            }
            code.ok_or_else(|| CliCoreError::message("no authorization code in callback"))
        })
        .await
        .map_err(|err| CliCoreError::message(format!("callback task failed: {err}")))?
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(CliCoreError::message("timed out waiting for OAuth callback")),
    }
}

/// Extracts a query parameter value from an HTTP request line.
fn extract_query_param(request: &str, name: &str) -> Option<String> {
    let line = request.lines().next()?;
    let path = line.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            return Some(url_decode(value));
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let h1 = chars.next().and_then(|c| c.to_digit(16));
            let h2 = chars.next().and_then(|c| c.to_digit(16));
            if let (Some(h1), Some(h2)) = (h1, h2) {
                #[allow(clippy::cast_possible_truncation)]
                let byte = ((h1 << 4) | h2) as u8;
                result.push(byte as char);
                continue;
            }
        }
        result.push(ch);
    }
    result
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
}

async fn parse_token_response(response: reqwest::Response, _env: &str) -> Result<StoredToken> {
    let body: TokenResponse = response
        .json()
        .await
        .map_err(|err| CliCoreError::message(format!("failed to parse token response: {err}")))?;
    let expires_in = body.expires_in.unwrap_or(3600);
    let expires_at = Utc::now().timestamp() + expires_in;
    Ok(StoredToken {
        access_token: body.access_token,
        expires_at,
        refresh_token: body.refresh_token,
    })
}
