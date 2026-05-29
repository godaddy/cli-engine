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
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{Credential, Result, auth::AuthProvider, error::CliCoreError};

const REDIRECT_PORT_DEFAULT: u16 = 7443;
const TOKEN_EXPIRY_BUFFER_SECS: i64 = 30;

/// Stored token with expiry tracking.
///
/// Token fields are zeroized on drop to limit in-memory exposure.
#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct StoredToken {
    access_token: String,
    expires_at: i64,
    refresh_token: Option<String>,
}

impl std::fmt::Debug for StoredToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredToken")
            .field("access_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field(
                "refresh_token",
                if self.refresh_token.is_some() {
                    &"Some([redacted])"
                } else {
                    &"None"
                },
            )
            .finish()
    }
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
    redirect_uri: Option<String>,
    app_id: String,
    env_prefix: String,
    allow_file_fallback: bool,
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
            redirect_uri: None,
            app_id: String::new(),
            env_prefix,
            allow_file_fallback: false,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Sets the local redirect server port (default: 7443).
    #[must_use]
    pub fn with_redirect_port(mut self, port: u16) -> Self {
        self.redirect_port = port;
        self
    }

    /// Overrides the redirect URI sent to the authorization server.
    ///
    /// By default the redirect URI is `http://127.0.0.1:{port}/callback`. Use
    /// this when the OAuth client is allowlisted with a different URI, such as
    /// `http://localhost:{port}/callback`. The local listener always binds to
    /// `127.0.0.1` regardless of what is set here.
    #[must_use]
    pub fn with_redirect_uri(mut self, uri: impl Into<String>) -> Self {
        self.redirect_uri = Some(uri.into());
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

    /// Enables an encrypted-file fallback when the system keychain is unavailable
    /// (e.g. headless Linux / WSL without a running secret-service daemon).
    ///
    /// Disabled by default. The original TypeScript CLI had no file fallback;
    /// enable only when you have confirmed the deployment environment lacks a
    /// reliable keychain and you accept unencrypted credentials on disk.
    #[must_use]
    pub fn with_file_fallback(mut self, enabled: bool) -> Self {
        self.allow_file_fallback = enabled;
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

    fn effective_redirect_uri(&self) -> String {
        self.redirect_uri
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}/callback", self.redirect_port))
    }

    /// Parses the effective redirect URI and returns `(bind_port, callback_path)`.
    fn parse_redirect_uri(&self) -> Result<(u16, String)> {
        let uri_str = self.effective_redirect_uri();
        let parsed = url::Url::parse(&uri_str)
            .map_err(|e| CliCoreError::message(format!("invalid redirect URI '{uri_str}': {e}")))?;
        let port = parsed
            .port()
            .or_else(|| parsed.port_or_known_default())
            .ok_or_else(|| {
                CliCoreError::message(format!("redirect URI '{uri_str}' has no port"))
            })?;
        let path = parsed.path().to_owned();
        Ok((port, path))
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

    /// Returns the path to the fallback credential file for this provider/env.
    ///
    /// Used when the system keychain is unavailable (e.g. WSL, headless Linux).
    fn credential_file_path(&self, env: &str) -> Option<std::path::PathBuf> {
        let app = if self.app_id.is_empty() {
            &self.name
        } else {
            &self.app_id
        };
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|_| {
                std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
            })
            .ok()?;
        Some(
            base.join(app)
                .join("credentials")
                .join(format!("{}-{}.json", self.name, env)),
        )
    }

    fn load_token_from_keychain(&self, env: &str) -> Option<StoredToken> {
        let service = self.keychain_service(env);
        match keyring::Entry::new(&service, self.keychain_user()) {
            Err(e) => {
                tracing::warn!(service, error = %e, "keychain entry creation failed");
            }
            Ok(entry) => match entry.get_password() {
                Err(e) => {
                    tracing::warn!(service, error = %e, "keychain read failed");
                }
                Ok(json) => match serde_json::from_str(&json) {
                    Err(e) => {
                        tracing::warn!(service, error = %e, "keychain token JSON invalid");
                    }
                    Ok(token) => return Some(token),
                },
            },
        }
        if !self.allow_file_fallback {
            return None;
        }
        let path = self.credential_file_path(env)?;
        let json = std::fs::read_to_string(&path).ok()?;
        tracing::debug!(path = %path.display(), "loaded token from file fallback");
        serde_json::from_str(&json).ok()
    }

    fn save_token_to_keychain(&self, env: &str, token: &StoredToken) -> Result<()> {
        let json = serde_json::to_string(token).map_err(CliCoreError::from)?;
        let service = self.keychain_service(env);
        match keyring::Entry::new(&service, self.keychain_user()) {
            Err(e) => {
                tracing::warn!(service, error = %e, "keychain entry creation failed");
            }
            Ok(entry) => match entry.set_password(&json) {
                Err(e) => {
                    tracing::warn!(service, error = %e, "keychain write failed");
                }
                Ok(()) => {
                    tracing::debug!(service, "token saved to keychain");
                    return Ok(());
                }
            },
        }
        if !self.allow_file_fallback {
            return Err(CliCoreError::message(
                "keychain is unavailable and file fallback is disabled — \
                 ensure your system keychain (e.g. gnome-keyring, macOS Keychain) \
                 is running and unlocked",
            ));
        }
        let path = self
            .credential_file_path(env)
            .ok_or_else(|| CliCoreError::message("could not determine credential file path"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CliCoreError::message(format!("failed to create credential directory: {e}"))
            })?;
        }
        std::fs::write(&path, &json).map_err(|e| {
            CliCoreError::message(format!(
                "failed to write credentials to {}: {e}",
                path.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
        }
        tracing::debug!(path = %path.display(), "token saved to file fallback");
        Ok(())
    }

    fn delete_token_from_keychain(&self, env: &str) {
        if let Ok(entry) = keyring::Entry::new(&self.keychain_service(env), self.keychain_user()) {
            drop(entry.delete_credential());
        }
        if self.allow_file_fallback {
            if let Some(path) = self.credential_file_path(env) {
                drop(std::fs::remove_file(path));
            }
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
            if let Some(refresh_token) = token.refresh_token.as_deref()
                && let Ok(mut refreshed) = self.refresh_access_token(refresh_token).await
            {
                if refreshed.refresh_token.is_none() {
                    refreshed.refresh_token = Some(refresh_token.to_owned());
                }
                self.save_token_to_keychain(env, &refreshed)?;
                self.store_cached_token(env, refreshed.clone()).await;
                return Ok(refreshed);
            }
        }
        let token = self.run_pkce_flow(env).await?;
        self.save_token_to_keychain(env, &token)?;
        self.store_cached_token(env, token.clone()).await;
        Ok(token)
    }

    async fn run_pkce_flow(&self, env: &str) -> Result<StoredToken> {
        let (code_verifier, code_challenge) = pkce_challenge();
        let state = random_state();
        let client_id = self.effective_client_id();
        let auth_url = self.effective_auth_url();
        let redirect_uri = self.effective_redirect_uri();
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

        let (bind_port, callback_path) = self.parse_redirect_uri()?;

        // Start the local callback server before opening the browser so the
        // redirect lands as soon as the user approves.
        let listener =
            TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], bind_port))).map_err(|err| {
                CliCoreError::message(format!(
                    "failed to bind callback server on port {bind_port}: {err}"
                ))
            })?;

        tracing::info!("Opening browser for authentication…");
        tracing::info!("If the browser does not open, visit:\n  {url}");
        drop(open::that(url.as_str()));

        let code =
            wait_for_callback(listener, &state, &callback_path, Duration::from_secs(120)).await?;
        self.exchange_code_for_token(&code, &code_verifier, env)
            .await
    }

    async fn exchange_code_for_token(
        &self,
        code: &str,
        code_verifier: &str,
        env: &str,
    ) -> Result<StoredToken> {
        let redirect_uri = self.effective_redirect_uri();
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
            token: token.access_token.clone(),
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
            token: token.access_token.clone(),
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
    let bytes: [u8; 32] = rand::rng().random();
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);
    (verifier, challenge)
}

/// Generates a random OAuth state parameter.
fn random_state() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Waits for the OAuth callback on the given listener, validates state and path.
///
/// Accepts connections in a loop so that stray connections (port scanners,
/// browser preflight requests) do not consume the single callback attempt.
/// Uses async I/O so the future is properly cancelled on Ctrl+C.
async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    expected_path: &str,
    timeout: Duration,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    listener
        .set_nonblocking(true)
        .map_err(|err| CliCoreError::message(format!("callback server setup failed: {err}")))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|err| CliCoreError::message(format!("callback server setup failed: {err}")))?;

    let expected_state = expected_state.to_owned();
    let expected_path = expected_path.to_owned();
    let result = tokio::time::timeout(timeout, async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => {
                    // Back off before retrying so a persistent accept failure
                    // (e.g. file-descriptor exhaustion) cannot spin the CPU until
                    // the timeout fires. The sleep is an await point, so Ctrl+C
                    // still cancels the flow promptly.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let mut buf = vec![0_u8; 4096];
            let n = match stream.read(&mut buf).await {
                Ok(0) | Err(_) => continue,
                Ok(n) => n,
            };
            let request = String::from_utf8_lossy(&buf[..n]);

            if extract_request_path(&request).as_deref() != Some(expected_path.as_str()) {
                drop(
                    stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await,
                );
                continue;
            }

            let code = extract_query_param(&request, "code");
            let state = extract_query_param(&request, "state");

            let html_response = if state.as_deref() == Some(&expected_state) && code.is_some() {
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                 <html><body>Authentication successful. You may close this window.</body></html>"
            } else {
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n\
                 <html><body>Authentication failed. Please try again.</body></html>"
            };
            drop(stream.write_all(html_response.as_bytes()).await);

            if state.as_deref() == Some(expected_state.as_str()) {
                return code
                    .ok_or_else(|| CliCoreError::message("no authorization code in callback"));
            }
        }
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(CliCoreError::message(
            "timed out waiting for OAuth callback",
        )),
    }
}

/// Extracts the path component from an HTTP request line (without query string).
fn extract_request_path(request: &str) -> Option<String> {
    let line = request.lines().next()?;
    let path_with_query = line.split_whitespace().nth(1)?;
    Some(
        path_with_query
            .split_once('?')
            .map_or(path_with_query, |(p, _)| p)
            .to_owned(),
    )
}

/// Extracts a query parameter value from an HTTP request line.
fn extract_query_param(request: &str, name: &str) -> Option<String> {
    let line = request.lines().next()?;
    let path = line.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_provider() -> PkceAuthProvider {
        PkceAuthProvider::new(
            "test",
            "https://example.com/auth",
            "https://example.com/token",
            "client-id",
            &["openid"],
        )
    }

    fn valid_token(access_token: &str) -> StoredToken {
        StoredToken {
            access_token: access_token.to_owned(),
            expires_at: Utc::now().timestamp() + 3600,
            refresh_token: None,
        }
    }

    fn expired_token() -> StoredToken {
        StoredToken {
            access_token: "old-token".to_owned(),
            // Older than the expiry buffer so is_valid() returns false.
            expires_at: Utc::now().timestamp() - TOKEN_EXPIRY_BUFFER_SECS - 1,
            refresh_token: None,
        }
    }

    /// store_cached_token + cached_token round-trip: the mechanism used by
    /// the persistence fix must reliably write and read tokens from the cache.
    #[tokio::test]
    async fn cache_stores_and_retrieves_valid_token() {
        let provider = test_provider();
        let token = valid_token("access-abc");

        provider.store_cached_token("dev", token.clone()).await;

        let cached = provider.cached_token("dev").await;
        assert!(cached.is_some(), "expected cached token to be present");
        assert_eq!(
            cached.expect("token must be present").access_token,
            "access-abc"
        );
    }

    /// Expired tokens must not be returned from the cache; the caller would
    /// then proceed to the keychain or PKCE flow.
    #[tokio::test]
    async fn cached_token_ignores_expired_tokens() {
        let provider = test_provider();
        provider.store_cached_token("dev", expired_token()).await;

        assert!(
            provider.cached_token("dev").await.is_none(),
            "expired token should not be returned from cache"
        );
    }

    #[test]
    fn redirect_uri_default_uses_127_0_0_1_and_redirect_port() {
        let provider = test_provider().with_redirect_port(9000);
        assert_eq!(
            provider.effective_redirect_uri(),
            "http://127.0.0.1:9000/callback"
        );
    }

    #[test]
    fn with_redirect_uri_overrides_default() {
        let provider = test_provider().with_redirect_uri("http://localhost:8080/auth/callback");
        assert_eq!(
            provider.effective_redirect_uri(),
            "http://localhost:8080/auth/callback"
        );
    }

    #[test]
    fn parse_redirect_uri_extracts_port_and_path_from_default() {
        let provider = test_provider().with_redirect_port(9000);
        let (port, path) = provider.parse_redirect_uri().expect("valid URI");
        assert_eq!(port, 9000);
        assert_eq!(path, "/callback");
    }

    #[test]
    fn parse_redirect_uri_extracts_port_and_path_from_custom_uri() {
        let provider = test_provider().with_redirect_uri("http://localhost:8080/auth/callback");
        let (port, path) = provider.parse_redirect_uri().expect("valid URI");
        assert_eq!(port, 8080);
        assert_eq!(path, "/auth/callback");
    }

    #[test]
    fn with_redirect_uri_does_not_affect_listener_host() {
        // The port is derived from the URI, but the listener always binds to
        // 127.0.0.1 — this test confirms the URI host does not change that.
        let provider = test_provider().with_redirect_uri("http://localhost:7777/callback");
        let (port, _) = provider.parse_redirect_uri().expect("valid URI");
        assert_eq!(port, 7777);
        // Caller uses 127.0.0.1 for bind regardless; SocketAddr construction
        // is in run_pkce_flow and is not repeated here.
    }

    #[test]
    fn extract_request_path_strips_query_string() {
        assert_eq!(
            extract_request_path("GET /auth/callback?code=abc&state=xyz HTTP/1.1\r\n"),
            Some("/auth/callback".to_owned()),
        );
    }

    #[test]
    fn extract_request_path_handles_no_query_string() {
        assert_eq!(
            extract_request_path("GET /callback HTTP/1.1\r\n"),
            Some("/callback".to_owned()),
        );
    }

    #[test]
    fn extract_query_param_skips_malformed_pairs() {
        let request = "GET /callback?foo&code=abc123&state=xyz HTTP/1.1\r\nHost: localhost\r\n";
        assert_eq!(
            extract_query_param(request, "code"),
            Some("abc123".to_owned()),
        );
        assert_eq!(
            extract_query_param(request, "state"),
            Some("xyz".to_owned()),
        );
    }

    #[test]
    fn extract_query_param_decodes_percent_encoding() {
        let request = "GET /callback?code=a%20b%2Bc&state=ok HTTP/1.1\r\n";
        assert_eq!(
            extract_query_param(request, "code"),
            Some("a b+c".to_owned()),
        );
    }

    /// resolve_token must return a pre-seeded in-memory token without
    /// triggering the PKCE browser flow (which would require a port and browser).
    /// This also exercises the cache-hit path that follows token persistence.
    #[tokio::test]
    async fn resolve_token_returns_cached_token_without_pkce_flow() {
        let provider = test_provider();
        provider
            .store_cached_token("dev", valid_token("cached-token"))
            .await;

        let resolved = provider
            .resolve_token("dev")
            .await
            .expect("resolve from cache");
        assert_eq!(resolved.access_token, "cached-token");
    }
}
