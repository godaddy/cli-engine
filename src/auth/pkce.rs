//! OAuth 2.0 PKCE authentication provider.
//!
//! Implements the browser-based Authorization Code + PKCE flow (RFC 7636).
//! Tokens are stored in the system keychain via the `keyring` crate.
//! On headless or WSL environments where a keychain daemon is unavailable, an
//! opt-in file fallback can be enabled with `PkceAuthProvider::with_file_fallback`;
//! tokens are then written as **unencrypted JSON** under
//! `$XDG_CONFIG_HOME/<app>/<provider>/credentials/` (mode `0600` on Unix).
//! Only enable the fallback when the deployment environment lacks a reliable
//! keychain and the security tradeoff is acceptable.
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

    /// Enables a file-based fallback when the system keychain is unavailable
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
        if !is_safe_path_component(app)
            || !is_safe_path_component(&self.name)
            || !is_safe_path_component(env)
        {
            tracing::warn!(
                app,
                name = self.name,
                env,
                "refusing credential path with unsafe component"
            );
            return None;
        }
        let base = config_base_dir()?;
        Some(
            base.join(app)
                .join("credentials")
                .join(format!("{}-{}.json", self.name, env)),
        )
    }

    async fn load_token_from_keychain(&self, env: &str) -> Option<StoredToken> {
        let service = self.keychain_service(env);
        let user = self.keychain_user().to_owned();

        let json_opt = match tokio::task::spawn_blocking({
            let service = service.clone();
            move || keychain_read_blocking(&service, &user)
        })
        .await
        {
            Ok(opt) => opt,
            Err(e) => {
                let reason = if e.is_cancelled() {
                    "cancelled"
                } else {
                    "panicked"
                };
                tracing::warn!(service, error = %e, reason, "keychain read task failed");
                None
            }
        };

        if let Some(json) = json_opt {
            match serde_json::from_str::<StoredToken>(&json) {
                Ok(token) => return Some(token),
                Err(e) => {
                    tracing::warn!(service, error = %e, "keychain token JSON invalid");
                    // Best-effort delete the corrupt entry so subsequent runs
                    // don't repeat the warning and fall through to re-auth.
                    let svc = service.clone();
                    let usr = self.keychain_user().to_owned();
                    drop(
                        tokio::task::spawn_blocking(move || {
                            if let Ok(entry) = keyring::Entry::new(&svc, &usr)
                                && let Err(e) = entry.delete_credential()
                                && !matches!(e, keyring::Error::NoEntry)
                            {
                                tracing::warn!(service = %svc, error = %e, "failed to self-heal corrupt keychain entry");
                            }
                        })
                        .await,
                    );
                }
            }
        }

        if !self.allow_file_fallback {
            return None;
        }
        let path = self.credential_file_path(env)?;
        load_token_from_file(&path).await
    }

    async fn save_token_to_keychain(&self, env: &str, token: &StoredToken) -> Result<()> {
        let json = serde_json::to_string(token).map_err(CliCoreError::from)?;
        let service = self.keychain_service(env);
        let user = self.keychain_user().to_owned();

        let (keychain_saved, json) = match tokio::task::spawn_blocking({
            let service = service.clone();
            move || {
                let saved = keychain_write_blocking(&service, &user, &json);
                (saved, json)
            }
        })
        .await
        {
            Ok(result) => result,
            Err(e) => {
                let reason = if e.is_cancelled() {
                    "cancelled"
                } else {
                    "panicked"
                };
                tracing::warn!(service, error = %e, reason, "keychain write task failed");
                // Re-serialize on task panic/cancel so the file-fallback path still has json.
                let json = serde_json::to_string(token).map_err(CliCoreError::from)?;
                (false, json)
            }
        };

        if keychain_saved {
            // Best-effort: remove any stale file-fallback token now that the
            // keychain is working. Ignore NotFound; the file may never have existed.
            if let Some(path) = self.credential_file_path(env) {
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {
                        tracing::debug!(path = %path.display(), "removed stale file fallback after keychain write");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::debug!(path = %path.display(), error = %e, "could not remove stale file fallback");
                    }
                }
            }
            return Ok(());
        }
        if !self.allow_file_fallback {
            return Err(CliCoreError::message(
                "failed to save token to keychain and file fallback is disabled — \
                 check logs for the underlying error, or ensure your system keychain \
                 (e.g. gnome-keyring, macOS Keychain) is running and unlocked",
            ));
        }
        let path = self
            .credential_file_path(env)
            .ok_or_else(|| CliCoreError::message("could not determine credential file path"))?;
        tokio::task::spawn_blocking({
            let path = path.clone();
            move || write_token_file_blocking(path, json)
        })
        .await
        .map_err(|e| {
            CliCoreError::message(format!(
                "credential file write task {}: {e}",
                if e.is_cancelled() {
                    "cancelled"
                } else {
                    "panicked"
                }
            ))
        })??;
        tracing::debug!(path = %path.display(), "token saved to file fallback");
        Ok(())
    }

    async fn delete_token_from_keychain(&self, env: &str) {
        let service = self.keychain_service(env);
        let user = self.keychain_user().to_owned();
        let service_for_warn = service.clone();
        if let Err(e) =
            tokio::task::spawn_blocking(move || match keyring::Entry::new(&service, &user) {
                Err(e) => {
                    tracing::warn!(service, error = %e, "keychain entry creation failed on delete");
                }
                Ok(entry) => match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(e) => {
                        tracing::warn!(service, error = %e, "keychain delete failed");
                    }
                },
            })
            .await
        {
            let reason = if e.is_cancelled() {
                "cancelled"
            } else {
                "panicked"
            };
            tracing::warn!(service = service_for_warn, error = %e, reason, "keychain delete task failed");
        }
        if let Some(path) = self.credential_file_path(env) {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to delete credential file");
                }
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
        if let Some(token) = self.load_token_from_keychain(env).await {
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
                self.save_token_to_keychain(env, &refreshed).await?;
                self.store_cached_token(env, refreshed.clone()).await;
                return Ok(refreshed);
            }
        }
        let token = self.run_pkce_flow(env).await?;
        self.save_token_to_keychain(env, &token).await?;
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
        let Some(token) = self.load_token_from_keychain(env).await else {
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
        self.delete_token_from_keychain(env).await;
        let mut cache = self.cache.write().await;
        cache.remove(env);
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        // Keyring and file-fallback storage do not support listing; return only
        // the in-memory cache keys as a hint. Tokens that survived a restart via
        // file fallback are not enumerated here.
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

/// Resolves the base config directory from environment variables.
fn config_base_dir() -> Option<std::path::PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            // On Windows prefer APPDATA over HOME/.config: HOME is often set by
            // Git Bash/MSYS shells and would place credentials in a non-standard
            // location. On all other platforms prefer XDG-conventional HOME/.config,
            // falling back to APPDATA as a last resort if HOME is unset.
            #[cfg(windows)]
            {
                std::env::var("APPDATA")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var("HOME")
                            .ok()
                            .filter(|v| !v.is_empty())
                            .map(|h| std::path::PathBuf::from(h).join(".config"))
                    })
            }
            #[cfg(not(windows))]
            {
                std::env::var("HOME")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|h| std::path::PathBuf::from(h).join(".config"))
                    .or_else(|| {
                        std::env::var("APPDATA")
                            .ok()
                            .filter(|v| !v.is_empty())
                            .map(std::path::PathBuf::from)
                    })
            }
        })
        // Reject relative paths: a relative XDG_CONFIG_HOME/APPDATA/HOME would
        // silently place credentials relative to the current working directory.
        .filter(|p| p.is_absolute())
}

/// Reads a token JSON string from the system keychain. Sync; call inside `spawn_blocking`.
fn keychain_read_blocking(service: &str, user: &str) -> Option<String> {
    match keyring::Entry::new(service, user) {
        Err(e) => {
            tracing::warn!(service, error = %e, "keychain entry creation failed");
            None
        }
        Ok(entry) => match entry.get_password() {
            Err(keyring::Error::NoEntry) => {
                tracing::debug!(service, "no stored token in keychain");
                None
            }
            Err(e) => {
                tracing::warn!(service, error = %e, "keychain read failed");
                None
            }
            Ok(json) => Some(json),
        },
    }
}

/// Writes a token JSON string to the system keychain. Sync; call inside `spawn_blocking`.
fn keychain_write_blocking(service: &str, user: &str, json: &str) -> bool {
    match keyring::Entry::new(service, user) {
        Err(e) => {
            tracing::warn!(service, error = %e, "keychain entry creation failed");
            false
        }
        Ok(entry) => match entry.set_password(json) {
            Err(e) => {
                tracing::warn!(service, error = %e, "keychain write failed");
                false
            }
            Ok(()) => {
                tracing::debug!(service, "token saved to keychain");
                true
            }
        },
    }
}

/// Reads and parses a [`StoredToken`] from the file fallback path.
async fn load_token_from_file(path: &std::path::Path) -> Option<StoredToken> {
    let json = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "file fallback read failed");
            return None;
        }
    };
    match serde_json::from_str(&json) {
        Ok(token) => {
            tracing::debug!(path = %path.display(), "loaded token from file fallback");
            Some(token)
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "file fallback token JSON invalid");
            // Best-effort delete: a permanently corrupt file causes repeated
            // warnings and PKCE flows on every run until manually removed.
            tokio::fs::remove_file(path).await.ok();
            None
        }
    }
}

/// Writes `json` to `path` via a uniquely-named temp file then renames it into place.
/// On Unix the rename is atomic. On Windows it is best-effort (`MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING`): it replaces an existing destination but is not
/// crash-atomic. Sync; call inside `spawn_blocking`.
fn write_token_file_blocking(path: std::path::PathBuf, json: String) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliCoreError::message(format!("failed to create credential directory: {e}"))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            {
                // The credential file itself is always 0o600; failing to
                // restrict the parent directory is a defence-in-depth miss,
                // not a confidentiality breach.
                tracing::debug!(
                    path = %parent.display(),
                    error = %e,
                    "could not restrict credential directory permissions"
                );
            }
        }
    }
    let rand_id = rand::random::<u32>();
    let tmp_path = path.with_file_name(format!(
        "{}.{rand_id:08x}.tmp",
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("cred"),
    ));
    write_token_tmp(&tmp_path, &json)?;
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        std::fs::remove_file(&tmp_path).ok();
        return Err(CliCoreError::message(format!(
            "failed to finalize credential file {}: {e}",
            path.display()
        )));
    }
    Ok(())
}

/// Opens `tmp_path` with `O_CREAT|O_EXCL` and writes `json`.
/// Sets mode `0o600` on Unix so credentials are never world-readable.
fn write_token_tmp(tmp_path: &std::path::Path, json: &str) -> Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(tmp_path).map_err(|e| {
        CliCoreError::message(format!(
            "failed to write credentials to {}: {e}",
            tmp_path.display()
        ))
    })?;
    file.write_all(json.as_bytes()).map_err(|e| {
        CliCoreError::message(format!(
            "failed to write credentials to {}: {e}",
            tmp_path.display()
        ))
    })
}

/// Returns true only when `s` is a single, non-traversal path component that is
/// valid on all supported platforms.
///
/// Rejects:
/// - empty strings, `.`, and `..`
/// - strings containing `/` or `\` (path separators on any platform)
/// - Windows-forbidden filename characters: `:  * ? " < > |`
/// - ASCII control characters (bytes 0x00–0x1F) and the DEL character (0x7F)
/// - leading or trailing space (leading space is invisible in directory listings)
/// - trailing `.` (valid on Unix but rejected by Windows)
/// - Windows reserved device names (`CON`, `NUL`, `COM1`, etc.) with or without extension
fn is_safe_path_component(s: &str) -> bool {
    // '/' is listed explicitly because Path::components() silently strips trailing
    // slashes — "prod/" parses as a single Normal("prod") component and would
    // otherwise pass the components check below.
    const FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    if s.contains(FORBIDDEN) || s.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return false;
    }
    if s.starts_with(' ') || s.ends_with('.') || s.ends_with(' ') {
        return false;
    }
    // Windows treats these device names as special regardless of extension,
    // e.g. opening "NUL.json" writes to the null device, not a file.
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
        "LPT9",
    ];
    let stem = std::path::Path::new(s)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(s);
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return false;
    }
    let mut components = std::path::Path::new(s).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
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
// set_var/remove_var are unsafe in Rust 2024 edition. The XDG_MUTEX in this
// module serialises all access so usage here is data-race-free.
#[allow(unsafe_code)]
mod tests {
    use super::*;

    /// Serialises access to XDG_CONFIG_HOME (and restores it) so env-var tests
    /// cannot race each other when the test runner spawns multiple threads.
    static XDG_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that restores an env var when dropped, including on panic.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: XDG_MUTEX is held for the duration of this guard's lifetime,
            // ensuring no other thread modifies these variables concurrently.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn with_xdg_config_home<F: FnOnce()>(value: &std::path::Path, f: F) {
        let _lock = XDG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: same as EnvVarGuard::drop — mutex held for the duration.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", value) };
        let _restore = EnvVarGuard {
            key: "XDG_CONFIG_HOME",
            prev,
        };
        f();
    }

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

    #[test]
    fn credential_file_path_uses_xdg_config_home() {
        let dir = std::env::temp_dir().join("cli-engine-test-xdg-pkce");
        with_xdg_config_home(&dir, || {
            let path = test_provider().credential_file_path("prod");
            assert_eq!(
                path,
                Some(dir.join("test").join("credentials").join("test-prod.json"))
            );
        });
    }

    #[test]
    fn credential_file_path_with_app_id_uses_app_id_as_dir() {
        let dir = std::env::temp_dir().join("cli-engine-test-xdg-pkce-appid");
        with_xdg_config_home(&dir, || {
            let path = test_provider()
                .with_app_id("myapp")
                .credential_file_path("prod");
            assert_eq!(
                path,
                Some(dir.join("myapp").join("credentials").join("test-prod.json"))
            );
        });
    }

    #[test]
    fn credential_file_path_rejects_traversal_in_env() {
        let dir = std::env::temp_dir().join("cli-engine-test-xdg-traversal");
        with_xdg_config_home(&dir, || {
            assert_eq!(
                test_provider().credential_file_path("../../etc/passwd"),
                None
            );
            assert_eq!(test_provider().credential_file_path("dev/subdir"), None);
            assert_eq!(test_provider().credential_file_path("dev\\subdir"), None);
            assert_eq!(test_provider().credential_file_path(".."), None);
        });
    }

    #[test]
    fn is_safe_path_component_rejects_windows_reserved_names() {
        for name in &[
            "CON", "con", "NUL", "nul", "COM1", "LPT9", "CON.txt", "NUL.json",
        ] {
            assert!(
                !is_safe_path_component(name),
                "{name:?} should be rejected as a Windows reserved name"
            );
        }
    }

    #[test]
    fn is_safe_path_component_rejects_empty_string() {
        assert!(!is_safe_path_component(""));
    }

    #[test]
    fn is_safe_path_component_rejects_leading_space_and_del() {
        assert!(
            !is_safe_path_component(" prod"),
            "leading space should be rejected"
        );
        assert!(
            !is_safe_path_component("prod\x7f"),
            "DEL byte should be rejected"
        );
    }

    #[test]
    fn is_safe_path_component_rejects_trailing_dot_and_space() {
        assert!(!is_safe_path_component("prod."));
        assert!(!is_safe_path_component("prod "));
    }

    #[test]
    fn is_safe_path_component_accepts_normal_values() {
        for name in &["dev", "prod", "staging", "my-app", "my_app", "app.v2"] {
            assert!(is_safe_path_component(name), "{name:?} should be accepted");
        }
    }

    #[test]
    fn credential_file_path_rejects_relative_base_dir() {
        let _lock = XDG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: mutex held for the duration.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", ".") };
        let _restore = EnvVarGuard {
            key: "XDG_CONFIG_HOME",
            prev,
        };
        assert_eq!(
            test_provider().credential_file_path("prod"),
            None,
            "relative XDG_CONFIG_HOME should be rejected"
        );
    }

    #[tokio::test]
    async fn file_fallback_round_trip_write_then_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test-prod.json");
        let token = valid_token("file-token");
        let json = serde_json::to_string(&token).expect("serialize");

        write_token_file_blocking(path.clone(), json).expect("write");

        let loaded = load_token_from_file(&path).await;
        assert_eq!(loaded.expect("token present").access_token, "file-token");
    }

    #[tokio::test]
    async fn file_fallback_invalid_json_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not-valid-json").expect("write");

        assert!(
            load_token_from_file(&path).await.is_none(),
            "invalid JSON should return None"
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

    /// list_environments returns only in-memory cache keys; tokens written to
    /// disk via file fallback during a previous session are not enumerated.
    #[tokio::test]
    async fn list_environments_returns_only_cached_keys() {
        let provider = test_provider();
        provider.store_cached_token("dev", valid_token("t1")).await;
        provider.store_cached_token("prod", valid_token("t2")).await;

        let mut envs = provider.list_environments().await.expect("list");
        envs.sort();
        assert_eq!(envs, ["dev", "prod"]);
    }

    /// A provider with no cache entries returns an empty list, regardless of
    /// what credential files may exist on disk from a previous session.
    #[tokio::test]
    async fn list_environments_returns_empty_without_cache() {
        let provider = test_provider();
        let envs = provider.list_environments().await.expect("list");
        assert!(envs.is_empty(), "expected empty list for a fresh provider");
    }
}
