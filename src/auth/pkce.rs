//! OAuth 2.0 PKCE authentication provider.
//!
//! Implements the browser-based Authorization Code + PKCE flow (RFC 7636).
//! Tokens are persisted through a pluggable [`CredentialStorage`] backend
//! (see [`crate::auth::storage`]) rather than a hard-wired keychain. By default
//! the backend is resolved from configuration — the `--credential-store` flag,
//! the `${PREFIX}_CREDENTIAL_STORE` env var, the engine config file, or the
//! `keyring` default — so an operator can disable the system keychain on
//! environments where it is unavailable (headless Linux, WSL) without code
//! changes. The three modes are:
//!
//! - `Keyring` (default): system keychain only.
//! - `Auto`: keychain with a transparent unencrypted-file fallback when the
//!   keychain backend is unavailable.
//! - `File`: never contact the keychain; store unencrypted JSON under
//!   `<config-base>/<app>/credentials/<provider>-<env>.json`, where
//!   `<config-base>` is `$XDG_CONFIG_HOME`, `$HOME/.config`, or `%APPDATA%`.
//!
//! See [`CredentialStore`](crate::config::CredentialStore). A backend can also be
//! injected directly with
//! [`PkceAuthProvider::with_storage`](crate::auth::pkce::PkceAuthProvider::with_storage)
//! or forced with
//! [`PkceAuthProvider::with_credential_store`](crate::auth::pkce::PkceAuthProvider::with_credential_store).
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
//! For per-environment OAuth config (different client id or endpoints per env),
//! wire the provider to a shared
//! [`Environments`](crate::environments::Environments) with
//! [`PkceAuthProvider::with_environments`](crate::auth::pkce::PkceAuthProvider::with_environments);
//! the resolved environment then drives the OAuth config for the active `env`.
//!
//! Without a wired resolver (or for a field the resolved environment leaves
//! empty), endpoints and client ID can still be overridden via environment
//! variables:
//! - `<PREFIX>_OAUTH_CLIENT_ID`
//! - `<PREFIX>_OAUTH_AUTH_URL`
//! - `<PREFIX>_OAUTH_TOKEN_URL`
//!
//! where `<PREFIX>` is the provider name uppercased and with `-` replaced by `_`.

use std::{
    collections::HashMap,
    io::{IsTerminal, Write},
    net::{SocketAddr, TcpListener},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    Credential, Result,
    auth::AuthProvider,
    auth::CredentialRequest,
    auth::storage::{CredentialKey, CredentialStorage, default_storage, storage_for},
    config::CredentialStore,
    error::CliCoreError,
};

const REDIRECT_PORT_DEFAULT: u16 = 7443;
const TOKEN_EXPIRY_BUFFER_SECS: i64 = 30;
/// Default timeout applied to OAuth token-endpoint requests (exchange/refresh)
/// so a stalled token server cannot hang the CLI indefinitely.
const TOKEN_REQUEST_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);

/// Stored token with expiry tracking.
///
/// Token fields are zeroized on drop to limit in-memory exposure.
#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct StoredToken {
    access_token: String,
    expires_at: i64,
    refresh_token: Option<String>,
    /// Scopes the token was obtained with (granted by the authorization server,
    /// or the requested set when the server does not echo `scope`). Lets scope
    /// coverage work for opaque access tokens and IdPs that do not expose scopes
    /// in the access token itself. Not secret, so excluded from zeroization.
    ///
    /// `#[serde(default)]` keeps tokens written before this field was added
    /// loadable from the keychain (they decode with an empty set, falling back to
    /// the JWT `scope`/`scp` claim as before).
    #[serde(default)]
    #[zeroize(skip)]
    scopes: Vec<String>,
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
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl StoredToken {
    fn is_valid(&self) -> bool {
        let now = Utc::now().timestamp();
        self.expires_at - TOKEN_EXPIRY_BUFFER_SECS > now
    }
}

/// The effective OAuth values for an environment, computed from a single
/// environment resolution. A token flow resolves this once and reuses it across
/// the authorize URL, code exchange, and refresh, rather than re-reading and
/// re-parsing `environments.toml` once per field.
struct EffectiveOAuth {
    client_id: String,
    auth_url: String,
    token_url: String,
    scopes: Vec<String>,
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
    /// Optional environment resolver; when set, per-env OAuth config comes from
    /// the resolved environment instead of the base config / legacy env override.
    /// Looked up by the `env` passed to [`AuthProvider::get_credential`].
    environments: Option<Arc<crate::environments::Environments>>,
    redirect_port: u16,
    redirect_uri: Option<String>,
    /// Timeout applied to token-endpoint requests (exchange and refresh).
    token_timeout: Duration,
    /// Shared HTTP client for token-endpoint traffic, built once and reused by
    /// exchange and refresh so connections and TLS configuration are pooled
    /// rather than rebuilt per request. The user-agent and timeout are applied
    /// per request (not baked into the client) so they reflect the value
    /// published at execution time, not at provider construction.
    client: reqwest::Client,
    app_id: String,
    env_prefix: String,
    /// Explicit storage backend injected via [`PkceAuthProvider::with_storage`].
    /// Wins over `store_mode` and the config-driven default.
    storage_override: Option<Arc<dyn CredentialStorage>>,
    /// Explicit storage mode from [`PkceAuthProvider::with_credential_store`].
    /// Forces a built-in backend, bypassing flag/env/config resolution.
    store_mode: Option<CredentialStore>,
    /// Lazily-resolved storage backend. Built on first use so `--schema` /
    /// `--dry-run` (which never resolve a credential) touch no keychain/config.
    storage: tokio::sync::OnceCell<Arc<dyn CredentialStorage>>,
    /// Prioritized JWT claim names used to derive `Credential.identity` from the
    /// decoded access-token payload. First non-empty string claim wins.
    identity_claims: Vec<String>,
    /// In-process token cache keyed by env.
    cache: Arc<RwLock<HashMap<String, StoredToken>>>,
}

/// Default prioritized claim names for deriving a human-readable identity.
const DEFAULT_IDENTITY_CLAIMS: &[&str] =
    &["email", "preferred_username", "username", "name", "sub"];

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
            environments: None,
            redirect_port: REDIRECT_PORT_DEFAULT,
            redirect_uri: None,
            token_timeout: TOKEN_REQUEST_TIMEOUT_DEFAULT,
            client: reqwest::Client::new(),
            app_id: String::new(),
            env_prefix,
            storage_override: None,
            store_mode: None,
            storage: tokio::sync::OnceCell::new(),
            identity_claims: DEFAULT_IDENTITY_CLAIMS
                .iter()
                .map(|claim| (*claim).to_owned())
                .collect(),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Sources per-environment OAuth config from a shared
    /// [`Environments`](crate::environments::Environments).
    ///
    /// Given an `env`, the provider resolves the environment and uses its
    /// [`OAuthConfig`](crate::environments::OAuthConfig). This is the
    /// single-source-of-truth path; prefer it over the base
    /// `client_id`/`auth_url`/`token_url` when the consumer registers
    /// environments via
    /// [`CliConfig::with_environments`](crate::CliConfig::with_environments).
    ///
    /// Precedence per OAuth field, for the resolved env: the resolved
    /// environment's value when non-empty; otherwise the legacy
    /// provider-prefixed env var (`<PREFIX>_OAUTH_CLIENT_ID`, `_AUTH_URL`,
    /// `_TOKEN_URL`); otherwise the base configuration supplied to
    /// [`PkceAuthProvider::new`]. An empty field on the resolved environment
    /// falls through, so a partial environment can override only the client id
    /// while inheriting the provider's base endpoints.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use cli_engine::{
    ///     auth::pkce::PkceAuthProvider,
    ///     environments::{EnvironmentDef, Environments},
    /// };
    ///
    /// let environments = Arc::new(
    ///     Environments::new("prod").with_environment(
    ///         "dev",
    ///         EnvironmentDef::new()
    ///             .with_client_id("dev-client-id")
    ///             .with_auth_url("https://api.dev-godaddy.com/v2/oauth2/authorize")
    ///             .with_token_url("https://api.dev-godaddy.com/v2/oauth2/token"),
    ///     ),
    /// );
    ///
    /// let provider = PkceAuthProvider::new(
    ///     "godaddy",
    ///     "https://api.godaddy.com/v2/oauth2/authorize",
    ///     "https://api.godaddy.com/v2/oauth2/token",
    ///     "prod-client-id",
    ///     &["openid", "profile"],
    /// )
    /// .with_environments(environments);
    /// # let _ = provider;
    /// ```
    #[must_use]
    pub fn with_environments(
        mut self,
        environments: Arc<crate::environments::Environments>,
    ) -> Self {
        self.environments = Some(environments);
        self
    }

    /// Sets the local redirect server port (default: 7443).
    #[must_use]
    pub fn with_redirect_port(mut self, port: u16) -> Self {
        self.redirect_port = port;
        self
    }

    /// Sets the timeout applied to token-endpoint requests (authorization-code
    /// exchange and refresh).
    ///
    /// Defaults to 30 seconds. This bounds only the HTTP token requests; the
    /// interactive browser/callback wait has its own separate timeout.
    #[must_use]
    pub fn with_token_timeout(mut self, timeout: Duration) -> Self {
        self.token_timeout = timeout;
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

    /// Injects a custom credential storage backend.
    ///
    /// Takes precedence over [`with_credential_store`](Self::with_credential_store)
    /// and the config-driven default. Use this to plug in a bespoke
    /// [`CredentialStorage`] (for example an in-memory store in tests, or a
    /// remote secret manager).
    #[must_use]
    pub fn with_storage(mut self, storage: Arc<dyn CredentialStorage>) -> Self {
        self.storage_override = Some(storage);
        self
    }

    /// Forces a built-in credential storage mode, bypassing the
    /// flag/env/config resolution.
    ///
    /// Use [`CredentialStore::File`] to skip the system keychain entirely (the
    /// escape hatch for headless Linux / WSL), [`CredentialStore::Auto`] for a
    /// keychain-with-file-fallback, or [`CredentialStore::Keyring`] for
    /// keychain-only. When unset, the mode is resolved per
    /// [`crate::config::resolve_credential_store`].
    #[must_use]
    pub fn with_credential_store(mut self, mode: CredentialStore) -> Self {
        self.store_mode = Some(mode);
        self
    }

    /// Enables a file-based fallback when the system keychain is unavailable
    /// (e.g. headless Linux / WSL without a running secret-service daemon).
    ///
    /// `true` maps to [`CredentialStore::Auto`] and `false` to
    /// [`CredentialStore::Keyring`].
    #[must_use]
    #[deprecated(
        since = "0.3.0",
        note = "use with_credential_store(CredentialStore::Auto) or (CredentialStore::Keyring)"
    )]
    pub fn with_file_fallback(self, enabled: bool) -> Self {
        self.with_credential_store(if enabled {
            CredentialStore::Auto
        } else {
            CredentialStore::Keyring
        })
    }

    /// Overrides the prioritized JWT claim names used to derive
    /// [`Credential::identity`](crate::Credential) from the decoded access-token
    /// payload.
    ///
    /// The first claim whose value is a non-empty string wins. The default order
    /// is `email`, `preferred_username`, `username`, `name`, `sub`. Use this when
    /// the identity provider exposes the human identity under a non-standard
    /// claim name.
    #[must_use]
    pub fn with_identity_claims(mut self, claims: &[impl AsRef<str>]) -> Self {
        self.identity_claims = claims.iter().map(|c| c.as_ref().to_owned()).collect();
        self
    }

    /// Builds a [`Credential`] from a stored token, deriving `identity` and `sub`
    /// from the access-token JWT claims when present.
    fn build_credential(&self, env: &str, token: &StoredToken) -> Credential {
        let claims = decode_jwt_claims(&token.access_token);
        let identity = claims
            .as_ref()
            .map(|claims| extract_identity(claims, &self.identity_claims))
            .unwrap_or_default();
        let sub = claims
            .as_ref()
            .and_then(|claims| claims.get("sub"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Credential {
            token: token.access_token.clone(),
            env: env.to_owned(),
            provider: self.name.clone(),
            expires_at: chrono::DateTime::from_timestamp(token.expires_at, 0)
                .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_default(),
            identity,
            sub,
            ..Credential::default()
        }
    }

    /// Resolves the [`OAuthConfig`](crate::environments::OAuthConfig) for `env`
    /// from the wired [`Environments`](crate::environments::Environments), if
    /// any. Returns `None` when no resolver is wired, the env does not resolve,
    /// or the resolved environment carries no OAuth slice.
    fn resolved_oauth(&self, env: &str) -> Option<crate::environments::OAuthConfig> {
        let envs = self.environments.as_ref()?;
        match envs.resolve(env) {
            Ok(resolved) => resolved.oauth,
            Err(e) => {
                tracing::debug!(
                    env,
                    error = %e,
                    "environment resolve failed; falling back to base OAuth config"
                );
                None
            }
        }
    }

    /// Computes the effective OAuth config for `env` with a SINGLE environment
    /// resolution (at most one `environments.toml` read), then applies the
    /// resolved → `<PREFIX>_OAUTH_*` env var → base precedence to each field.
    ///
    /// Token flows call this once and reuse the result so they don't re-read the
    /// environments file once per field.
    fn effective_oauth(&self, env: &str) -> EffectiveOAuth {
        let resolved = self.resolved_oauth(env);
        // Resolved value when non-empty, else the provider-prefixed env var, else base.
        let field = |resolved_value: Option<&String>, var_suffix: &str, base: &str| -> String {
            if let Some(value) = resolved_value
                && !value.is_empty()
            {
                return value.clone();
            }
            std::env::var(format!("{}_OAUTH_{var_suffix}", self.env_prefix))
                .unwrap_or_else(|_| base.to_owned())
        };
        let scopes = match &resolved {
            Some(oauth) if !oauth.scopes.is_empty() => oauth.scopes.clone(),
            _ => self.scopes.clone(),
        };
        EffectiveOAuth {
            client_id: field(
                resolved.as_ref().map(|o| &o.client_id),
                "CLIENT_ID",
                &self.client_id,
            ),
            auth_url: field(
                resolved.as_ref().map(|o| &o.auth_url),
                "AUTH_URL",
                &self.auth_url,
            ),
            token_url: field(
                resolved.as_ref().map(|o| &o.token_url),
                "TOKEN_URL",
                &self.token_url,
            ),
            scopes,
        }
    }

    /// Default scopes for `env`: the resolved environment's scopes when
    /// non-empty, otherwise the provider's base scopes.
    fn effective_scopes(&self, env: &str) -> Vec<String> {
        self.effective_oauth(env).scopes
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

    /// Builds the storage key for this provider and `env`.
    fn credential_key<'key>(&'key self, env: &'key str) -> CredentialKey<'key> {
        CredentialKey::new(&self.app_id, &self.name, env)
    }

    /// Returns the credential storage backend, resolving and caching it on first
    /// use. Precedence: an injected [`with_storage`](Self::with_storage) backend,
    /// then a forced [`with_credential_store`](Self::with_credential_store) mode,
    /// then the config-driven [`default_storage`].
    ///
    /// Resolution is lazy so paths that never resolve a credential (`--schema`,
    /// `--dry-run`) build no storage and touch neither the keychain nor config.
    async fn storage(&self) -> &Arc<dyn CredentialStorage> {
        self.storage
            .get_or_init(async || {
                if let Some(storage) = &self.storage_override {
                    storage.clone()
                } else if let Some(mode) = self.store_mode {
                    storage_for(mode)
                } else {
                    default_storage(&self.app_id)
                }
            })
            .await
    }

    /// Loads and deserializes the stored token for `env`, if present.
    ///
    /// On a corrupt/undecodable blob, best-effort deletes it (self-heal) and
    /// returns `None` so the caller re-authenticates rather than looping on the
    /// bad entry.
    async fn load_stored(&self, env: &str) -> Option<StoredToken> {
        let key = self.credential_key(env);
        let raw = self.storage().await.load(&key).await?;
        match serde_json::from_str::<StoredToken>(&raw) {
            Ok(token) => Some(token),
            Err(e) => {
                tracing::warn!(env, error = %e, "stored token JSON invalid; clearing");
                self.storage().await.delete(&key).await;
                None
            }
        }
    }

    /// Serializes and persists `token` for `env` via the storage backend.
    async fn save_stored(&self, env: &str, token: &StoredToken) -> Result<()> {
        let json = serde_json::to_string(token).map_err(CliCoreError::from)?;
        let key = self.credential_key(env);
        self.storage().await.save(&key, &json).await
    }

    /// Removes any stored token for `env` via the storage backend.
    async fn delete_stored(&self, env: &str) {
        let key = self.credential_key(env);
        self.storage().await.delete(&key).await;
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
        if let Some(token) = self.existing_token(env).await? {
            return Ok(token);
        }
        let scopes = self.effective_scopes(env);
        self.reauthenticate(env, &scopes).await
    }

    /// Returns a usable token from the in-memory cache, keychain, or a refresh —
    /// **without** launching an interactive PKCE flow. `None` means the caller
    /// must authenticate. Keeping this flow-free lets `get_credential_for` decide
    /// the scope set for a single login instead of authenticating twice.
    async fn existing_token(&self, env: &str) -> Result<Option<StoredToken>> {
        if let Some(token) = self.cached_token(env).await {
            return Ok(Some(token));
        }
        if let Some(token) = self.load_stored(env).await {
            if token.is_valid() {
                self.store_cached_token(env, token.clone()).await;
                return Ok(Some(token));
            }
            if let Some(refresh_token) = token.refresh_token.as_deref()
                && let Ok(mut refreshed) = self
                    .refresh_access_token(env, refresh_token, &token.scopes)
                    .await
            {
                if refreshed.refresh_token.is_none() {
                    refreshed.refresh_token = Some(refresh_token.to_owned());
                }
                self.save_stored(env, &refreshed).await?;
                self.store_cached_token(env, refreshed.clone()).await;
                return Ok(Some(refreshed));
            }
        }
        Ok(None)
    }

    /// Runs a fresh interactive PKCE flow requesting exactly `scopes`, replacing
    /// any stored token for `env`.
    async fn reauthenticate(&self, env: &str, scopes: &[String]) -> Result<StoredToken> {
        let token = self.run_pkce_flow_with(env, scopes).await?;
        // Persist first — the keychain write overwrites the existing entry for
        // this env — and only update the in-memory cache after a successful
        // save. This avoids destroying a still-valid token if the save fails
        // (e.g. keychain unavailable and file fallback disabled).
        self.save_stored(env, &token).await?;
        self.store_cached_token(env, token.clone()).await;
        Ok(token)
    }

    /// Runs the browser PKCE flow requesting exactly `scopes` (used both for the
    /// default login and for scope step-up, which requests a wider union).
    async fn run_pkce_flow_with(&self, env: &str, scopes: &[String]) -> Result<StoredToken> {
        let (code_verifier, code_challenge) = pkce_challenge();
        let state = random_state();
        // Resolve the OAuth config once for this whole flow (authorize + exchange).
        let oauth = self.effective_oauth(env);
        let redirect_uri = self.effective_redirect_uri();
        let scope = scopes.join(" ");

        let auth_params = [
            ("response_type", "code"),
            ("client_id", &oauth.client_id),
            ("redirect_uri", &redirect_uri),
            ("scope", &scope),
            ("state", &state),
            ("code_challenge", &code_challenge),
            ("code_challenge_method", "S256"),
        ];
        let url = url::Url::parse_with_params(&oauth.auth_url, &auth_params)
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

        emit_browser_login_prompt(&url);
        drop(open::that(url.as_str()));

        let code =
            wait_for_callback(listener, &state, &callback_path, Duration::from_secs(120)).await?;
        self.exchange_code_for_token(&oauth, &code, &code_verifier, scopes)
            .await
    }

    /// Builds a POST to an OAuth token endpoint on the provider's shared client.
    ///
    /// Token traffic does not go through [`HttpClient`](crate::transport::HttpClient)
    /// — that client is built for authenticated, JSON-bodied backend calls,
    /// whereas the token endpoint is unauthenticated and form-encoded. The
    /// user-agent and timeout are attached here per request (read at call time)
    /// so every outbound call, including credential acquisition and refresh, is
    /// attributed consistently and bounded.
    fn token_request(&self, token_url: &str, params: &[(&str, &str)]) -> reqwest::RequestBuilder {
        self.client
            .post(token_url)
            .header(
                reqwest::header::USER_AGENT,
                crate::transport::client::default_user_agent(),
            )
            .timeout(self.token_timeout)
            .form(params)
    }

    async fn exchange_code_for_token(
        &self,
        oauth: &EffectiveOAuth,
        code: &str,
        code_verifier: &str,
        requested_scopes: &[String],
    ) -> Result<StoredToken> {
        let redirect_uri = self.effective_redirect_uri();

        let params = [
            ("grant_type", "authorization_code"),
            ("client_id", &oauth.client_id),
            ("redirect_uri", &redirect_uri),
            ("code", code),
            ("code_verifier", code_verifier),
        ];
        let response = self
            .token_request(&oauth.token_url, &params)
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

        parse_token_response(response, requested_scopes).await
    }

    async fn refresh_access_token(
        &self,
        env: &str,
        refresh_token: &str,
        prior_scopes: &[String],
    ) -> Result<StoredToken> {
        let oauth = self.effective_oauth(env);
        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", &oauth.client_id),
            ("refresh_token", refresh_token),
        ];
        let response = self
            .token_request(&oauth.token_url, &params)
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

        parse_token_response(response, prior_scopes).await
    }
}

#[async_trait]
impl AuthProvider for PkceAuthProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        let token = self.resolve_token(env).await?;
        Ok(self.build_credential(env, &token))
    }

    async fn get_credential_for(&self, req: &CredentialRequest<'_>) -> Result<Credential> {
        let env = req.env;
        let required = &req.meta.scopes;
        let defaults = self.effective_scopes(env);

        // Look for a usable token WITHOUT launching a flow, so we can pick the
        // scope set for a single login rather than authenticating twice (e.g.
        // `auth login --scope X` logs out first; resolving defaults and then
        // stepping up would open the browser twice).
        if let Some(token) = self.existing_token(env).await? {
            // Decide based on what the token grants (JWT claim plus the scopes it
            // was obtained with). Step-up means re-consent: the authorization
            // server has no silent scope-expansion grant, so in non-interactive
            // contexts we fail fast rather than hang on the callback timeout.
            let granted = granted_scopes(&token);
            match plan_step_up(&defaults, &granted, required, session_is_interactive()) {
                StepUp::Covered => return Ok(self.build_credential(env, &token)),
                StepUp::MissingNonInteractive(missing) => {
                    return Err(missing_scope_error(env, &missing));
                }
                // Union (defaults ∪ already-granted ∪ required) so step-up never
                // drops scopes acquired by an earlier login or step-up.
                StepUp::Reauthenticate(union) => {
                    let token = self.reauthenticate(env, &union).await?;
                    ensure_granted(env, &token, required)?;
                    return Ok(self.build_credential(env, &token));
                }
            }
        }

        // No usable token: authenticate once, requesting defaults ∪ required.
        let union = union_scopes(&defaults, &[], required);
        let token = self.reauthenticate(env, &union).await?;
        ensure_granted(env, &token, required)?;
        Ok(self.build_credential(env, &token))
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        let Some(token) = self.load_stored(env).await else {
            return Err(CliCoreError::message(format!(
                "not logged in for environment {env:?}"
            )));
        };
        Ok(self.build_credential(env, &token))
    }

    async fn logout(&self, env: &str) -> Result<()> {
        self.delete_stored(env).await;
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

fn emit_browser_login_prompt(url: &url::Url) {
    let mut stderr = std::io::stderr().lock();
    drop(writeln!(stderr, "Opening browser for authentication…"));
    drop(writeln!(
        stderr,
        "If the browser does not open, visit:\n  {url}"
    ));
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
    /// Space-delimited scopes the server actually granted, when it echoes them.
    scope: Option<String>,
}

/// Decodes the claims (payload) segment of a JWT **without verifying the
/// signature**.
///
/// The returned claims are used to display a human-readable identity in
/// `auth status` and audit logs, and (via [`scopes_from_jwt`]) to decide whether
/// scope step-up needs a fresh login. These are convenience/optimization reads,
/// **not** trust or authorization decisions — the authorization server remains
/// the source of truth for granted scopes — so signature verification is
/// intentionally skipped. Opaque (non-JWT) tokens and any decode/parse failure
/// yield `None`, leaving the identity blank (and treating scopes as absent, which
/// just forces a re-auth).
fn decode_jwt_claims(token: &str) -> Option<Map<String, Value>> {
    // A JWT is `header.payload.signature`; the payload is the middle segment,
    // base64url-encoded without padding.
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Returns `defaults ∪ granted ∪ required`, order-preserving and de-duplicated.
fn union_scopes(defaults: &[String], granted: &[String], required: &[String]) -> Vec<String> {
    let mut union = defaults.to_vec();
    for scope in granted.iter().chain(required.iter()) {
        if !union.contains(scope) {
            union.push(scope.clone());
        }
    }
    union
}

/// Reads the granted scopes from a JWT access token.
///
/// OAuth uses a space-delimited `scope` string (RFC), but some IdPs (e.g. Azure
/// AD) use `scp`, and either may be encoded as a JSON array — so all of those
/// forms are accepted. Returns an empty list for opaque (non-JWT) tokens or
/// tokens without a recognized scope claim; coverage then falls back to the
/// scopes recorded on the [`StoredToken`] (see [`granted_scopes`]).
fn scopes_from_jwt(token: &str) -> Vec<String> {
    let Some(claims) = decode_jwt_claims(token) else {
        return Vec::new();
    };
    for key in ["scope", "scp"] {
        if let Some(value) = claims.get(key) {
            let scopes = scopes_from_claim(value);
            if !scopes.is_empty() {
                return scopes;
            }
        }
    }
    Vec::new()
}

/// Parses a scope claim that may be a space-delimited string or a JSON array of
/// (possibly space-delimited) strings.
fn scopes_from_claim(value: &Value) -> Vec<String> {
    match value {
        Value::String(scope) => scope.split_whitespace().map(str::to_owned).collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(str::split_whitespace)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// All scopes an access token is known to carry: the JWT `scope`/`scp` claim
/// plus the scopes recorded when the token was obtained. The recorded scopes
/// make coverage work for opaque tokens and IdPs that omit scopes from the
/// access token.
fn granted_scopes(token: &StoredToken) -> Vec<String> {
    let mut scopes = scopes_from_jwt(&token.access_token);
    for scope in &token.scopes {
        if !scopes.contains(scope) {
            scopes.push(scope.clone());
        }
    }
    scopes
}

/// The action scope step-up should take for a token, given what it already
/// grants and what the command requires. Pure so the decision is unit-testable
/// without real TTY detection or a browser flow.
#[derive(Debug, PartialEq, Eq)]
enum StepUp {
    /// The token already covers every required scope.
    Covered,
    /// Re-authenticate requesting this scope set (defaults ∪ granted ∪ required).
    Reauthenticate(Vec<String>),
    /// Scopes are missing but the session is non-interactive, so step-up cannot
    /// prompt; carries the missing scopes for the error message.
    MissingNonInteractive(Vec<String>),
}

fn plan_step_up(
    defaults: &[String],
    granted: &[String],
    required: &[String],
    interactive: bool,
) -> StepUp {
    let missing: Vec<String> = required
        .iter()
        .filter(|scope| !granted.iter().any(|have| have == *scope))
        .cloned()
        .collect();
    if missing.is_empty() {
        StepUp::Covered
    } else if interactive {
        StepUp::Reauthenticate(union_scopes(defaults, granted, required))
    } else {
        StepUp::MissingNonInteractive(missing)
    }
}

/// Treats the session as interactive if any stdio stream is a TTY, so
/// redirecting one (e.g. capturing stderr) does not block a user who can still
/// complete the browser flow.
fn session_is_interactive() -> bool {
    std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
        || std::io::stderr().is_terminal()
}

/// Confirms a freshly (re)authenticated token actually grants `required`.
///
/// Re-consent does not guarantee the authorization server grants every requested
/// scope (it may decline by policy). When the difference is detectable — the
/// token is a JWT exposing its scopes, or the token response echoed a narrower
/// `scope` — return a clear error instead of handing back an under-scoped token
/// that the API would later reject with a 403, and instead of re-prompting in a
/// loop the server will keep refusing. (For opaque tokens whose grant the server
/// does not echo, the recorded scopes equal what was requested, so an undetected
/// decline still surfaces downstream as a 403.)
fn ensure_granted(env: &str, token: &StoredToken, required: &[String]) -> Result<()> {
    let granted = granted_scopes(token);
    let missing: Vec<String> = required
        .iter()
        .filter(|scope| !granted.iter().any(|have| have == *scope))
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(CliCoreError::message(format!(
            "authorization server did not grant required scope(s) for {env:?}: {}",
            missing.join(", ")
        )))
    }
}

/// Error returned when required scopes are missing and step-up cannot prompt.
fn missing_scope_error(env: &str, missing: &[String]) -> CliCoreError {
    let display = missing.join(", ");
    let hint = missing
        .iter()
        .map(|scope| format!("--scope {scope}"))
        .collect::<Vec<_>>()
        .join(" ");
    CliCoreError::message(format!(
        "access token for {env:?} is missing required scope(s): {display}; \
         run `auth login --env {env} {hint}` in an interactive terminal"
    ))
}

/// Returns the first claim value that is a non-empty string, in priority order.
fn extract_identity(claims: &Map<String, Value>, priority: &[String]) -> String {
    priority
        .iter()
        .filter_map(|name| claims.get(name).and_then(Value::as_str))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

async fn parse_token_response(
    response: reqwest::Response,
    requested_scopes: &[String],
) -> Result<StoredToken> {
    let body: TokenResponse = response
        .json()
        .await
        .map_err(|err| CliCoreError::message(format!("failed to parse token response: {err}")))?;
    let expires_in = body.expires_in.unwrap_or(3600);
    let expires_at = Utc::now().timestamp() + expires_in;
    // Record what the token grants: the server's echoed `scope` when present,
    // otherwise the scopes we asked for. This is the coverage signal for opaque
    // tokens, which carry no readable scope claim.
    let scopes = body
        .scope
        .as_deref()
        .map(|scope| {
            scope
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|scopes| !scopes.is_empty())
        .unwrap_or_else(|| requested_scopes.to_vec());
    Ok(StoredToken {
        access_token: body.access_token,
        expires_at,
        refresh_token: body.refresh_token,
        scopes,
    })
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use serde_json::json;

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
            scopes: Vec::new(),
        }
    }

    fn token_with_scopes(access_token: &str, scopes: &[&str]) -> StoredToken {
        // No struct-update from `valid_token`: StoredToken is `Drop`
        // (ZeroizeOnDrop), so fields cannot be moved out of another instance.
        StoredToken {
            access_token: access_token.to_owned(),
            expires_at: Utc::now().timestamp() + 3600,
            refresh_token: None,
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn expired_token() -> StoredToken {
        StoredToken {
            access_token: "old-token".to_owned(),
            // Older than the expiry buffer so is_valid() returns false.
            expires_at: Utc::now().timestamp() - TOKEN_EXPIRY_BUFFER_SECS - 1,
            refresh_token: None,
            scopes: Vec::new(),
        }
    }

    fn envs_for_test() -> Arc<crate::environments::Environments> {
        use crate::environments::{EnvironmentDef, Environments};
        Arc::new(
            Environments::new("prod").with_environment(
                "prod",
                EnvironmentDef::new()
                    .with_client_id("prod-client")
                    .with_auth_url("https://prod.example.com/auth")
                    .with_token_url("https://prod.example.com/token")
                    .with_scopes(&["openid", "prod.read"]),
            ),
        )
    }

    /// A provider wired to an [`Environments`](crate::environments::Environments)
    /// resolver sources its per-env OAuth config (client id, endpoints, scopes)
    /// from the resolved environment, making the environment the single source
    /// of truth.
    #[test]
    fn environment_wired_provider_sources_oauth_from_resolver() {
        let provider = PkceAuthProvider::new(
            "godaddy",
            "https://base/auth",
            "https://base/token",
            "base-client",
            &["openid"],
        )
        .with_environments(envs_for_test());
        let oauth = provider.effective_oauth("prod");
        assert_eq!(oauth.client_id, "prod-client");
        assert_eq!(oauth.auth_url, "https://prod.example.com/auth");
        assert_eq!(oauth.token_url, "https://prod.example.com/token");
        assert_eq!(
            oauth.scopes,
            vec!["openid".to_owned(), "prod.read".to_owned()]
        );
    }

    /// A provider with no environment resolver falls back to the base client id,
    /// endpoints, and scopes for every env.
    #[test]
    fn non_wired_provider_uses_base_config() {
        let provider = PkceAuthProvider::new(
            "godaddy",
            "https://base/auth",
            "https://base/token",
            "base-client",
            &["openid"],
        );
        let oauth = provider.effective_oauth("anything");
        assert_eq!(oauth.client_id, "base-client");
        assert_eq!(oauth.scopes, vec!["openid".to_owned()]);
    }

    /// OAuth token traffic must carry the engine's configured default
    /// user-agent so it is attributed consistently with all other outbound
    /// calls (some upstream WAFs reject requests without a User-Agent).
    #[test]
    fn token_request_carries_default_user_agent() {
        let _guard = crate::transport::client::UA_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = crate::transport::client::RestoreDefaultUserAgent;
        crate::transport::set_default_user_agent("ua-probe/7.7");
        let provider = test_provider().with_token_timeout(Duration::from_secs(12));
        let request = provider
            .token_request(
                "https://example.com/token",
                &[("grant_type", "refresh_token")],
            )
            .build()
            .expect("token request should build");
        let header = request
            .headers()
            .get(reqwest::header::USER_AGENT)
            .expect("token request should set a user-agent");
        assert_eq!(header, "ua-probe/7.7");
        assert_eq!(request.timeout(), Some(&Duration::from_secs(12)));
    }

    /// OAuth token requests must not hang indefinitely: the provider applies a
    /// 30s timeout by default.
    #[test]
    fn default_token_timeout_is_thirty_seconds() {
        assert_eq!(test_provider().token_timeout, Duration::from_secs(30));
    }

    /// The default token timeout can be overridden per provider.
    #[test]
    fn with_token_timeout_overrides_default() {
        let provider = test_provider().with_token_timeout(Duration::from_secs(5));
        assert_eq!(provider.token_timeout, Duration::from_secs(5));
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
    fn scopes_from_jwt_parses_scope_claim() {
        let token = make_jwt(&json!({ "scope": "a b c" }));
        assert_eq!(scopes_from_jwt(&token), vec!["a", "b", "c"]);
    }

    #[test]
    fn scopes_from_jwt_parses_scp_and_array_claims() {
        // Azure-style `scp` array.
        let scp = make_jwt(&json!({ "scp": ["a", "b"] }));
        assert_eq!(scopes_from_jwt(&scp), vec!["a", "b"]);
        // `scope` encoded as an array.
        let array = make_jwt(&json!({ "scope": ["a", "b c"] }));
        assert_eq!(scopes_from_jwt(&array), vec!["a", "b", "c"]);
        // Empty `scope` falls through to `scp`.
        let mixed = make_jwt(&json!({ "scope": "", "scp": ["x"] }));
        assert_eq!(scopes_from_jwt(&mixed), vec!["x"]);
    }

    #[test]
    fn granted_scopes_uses_recorded_scopes_for_opaque_token() {
        // An opaque (non-JWT) token carries no readable claim, so coverage comes
        // from the scopes recorded when it was obtained.
        let token = token_with_scopes("opaque-token", &["a", "b"]);
        assert_eq!(granted_scopes(&token), vec!["a", "b"]);
    }

    #[test]
    fn ensure_granted_rejects_a_token_missing_required_scopes() {
        let required = vec!["a".to_owned(), "b".to_owned()];
        // JWT that exposes only `a` → `b` is detectably not granted.
        let jwt = valid_token(&make_jwt(&json!({ "scope": "a" })));
        let err = ensure_granted("dev", &jwt, &required).expect_err("b is not granted");
        assert!(
            err.to_string().contains("did not grant required scope(s)"),
            "{err}"
        );
        assert!(err.to_string().contains('b'), "{err}");

        // A token granting both passes.
        let ok = valid_token(&make_jwt(&json!({ "scope": "a b" })));
        ensure_granted("dev", &ok, &required).expect("both granted");
        // Recorded scopes (opaque token) also satisfy the check.
        let opaque = token_with_scopes("opaque", &["a", "b"]);
        ensure_granted("dev", &opaque, &required).expect("recorded scopes granted");
    }

    #[test]
    fn plan_step_up_covers_reauths_and_fails_non_interactive() {
        let defaults = vec!["base".to_owned()];
        let granted = vec!["base".to_owned(), "read".to_owned()];
        let read = vec!["read".to_owned()];
        let write = vec!["write".to_owned()];

        // Already covered.
        assert_eq!(
            plan_step_up(&defaults, &granted, &read, true),
            StepUp::Covered
        );
        // Missing + interactive → reauth requesting the union.
        assert_eq!(
            plan_step_up(&defaults, &granted, &write, true),
            StepUp::Reauthenticate(vec![
                "base".to_owned(),
                "read".to_owned(),
                "write".to_owned()
            ])
        );
        // Missing + non-interactive → fail fast, carrying the missing scopes.
        assert_eq!(
            plan_step_up(&defaults, &granted, &write, false),
            StepUp::MissingNonInteractive(vec!["write".to_owned()])
        );
    }

    /// An opaque cached token whose recorded scopes cover the requirement is
    /// returned without starting a flow — proving coverage no longer depends on
    /// a readable JWT scope claim.
    #[tokio::test]
    async fn get_credential_for_uses_recorded_scopes_for_opaque_token() {
        let provider = test_provider();
        provider
            .store_cached_token("dev", token_with_scopes("opaque-token", &["read", "write"]))
            .await;

        let meta = crate::middleware::CommandMeta {
            scopes: vec!["read".to_owned()],
            ..crate::middleware::CommandMeta::default()
        };
        let req = CredentialRequest::new("dev", "app:list", "read", &meta);
        let credential = provider
            .get_credential_for(&req)
            .await
            .expect("recorded scopes cover the requirement");
        assert_eq!(credential.token, "opaque-token");
    }

    #[test]
    fn union_scopes_dedupes_and_preserves_order() {
        let defaults = vec!["a".to_owned(), "b".to_owned()];
        let granted = vec!["b".to_owned(), "c".to_owned()];
        let required = vec!["c".to_owned(), "d".to_owned()];
        assert_eq!(
            union_scopes(&defaults, &granted, &required),
            vec!["a", "b", "c", "d"]
        );
    }

    #[test]
    fn scopes_from_jwt_empty_for_opaque_or_missing() {
        assert!(scopes_from_jwt("opaque-token").is_empty());
        let no_scope = make_jwt(&json!({ "sub": "user" }));
        assert!(scopes_from_jwt(&no_scope).is_empty());
    }

    /// When the cached token's JWT already covers the required scopes,
    /// `get_credential_for` must return it without starting a PKCE flow.
    #[tokio::test]
    async fn get_credential_for_uses_cached_token_when_scopes_covered() {
        let provider = test_provider();
        let token = valid_token(&make_jwt(&json!({
            "scope": "apps.app-registry:read apps.app-registry:write",
            "sub": "user-1",
        })));
        provider.store_cached_token("dev", token).await;

        let mut meta = crate::middleware::CommandMeta::default();
        meta.set_scopes(vec!["apps.app-registry:read".to_owned()]);
        let req = CredentialRequest {
            env: "dev",
            command: "app:list",
            tier: "read",
            meta: &meta,
        };
        let credential = provider
            .get_credential_for(&req)
            .await
            .expect("cached token covers required scopes");
        assert_eq!(credential.sub, "user-1");
    }

    /// With no required scopes, `get_credential_for` behaves like
    /// `get_credential` and returns the cached token unchanged.
    #[tokio::test]
    async fn get_credential_for_no_scopes_returns_cached() {
        let provider = test_provider();
        provider
            .store_cached_token("dev", valid_token("opaque"))
            .await;
        let meta = crate::middleware::CommandMeta::default();
        let req = CredentialRequest {
            env: "dev",
            command: "app:list",
            tier: "read",
            meta: &meta,
        };
        let credential = provider
            .get_credential_for(&req)
            .await
            .expect("no scopes required");
        assert_eq!(credential.token, "opaque");
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

    /// Builds an unsigned-looking JWT (`header.payload.signature`) whose payload
    /// is the given claims object, base64url-encoded without padding.
    fn make_jwt(claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("serialize claims"));
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn decode_jwt_claims_extracts_payload() {
        let token = make_jwt(&json!({"email": "user@example.com", "sub": "abc123"}));
        let claims = decode_jwt_claims(&token).expect("claims decode");
        assert_eq!(
            claims.get("email").and_then(Value::as_str),
            Some("user@example.com")
        );
        assert_eq!(claims.get("sub").and_then(Value::as_str), Some("abc123"));
    }

    #[test]
    fn decode_jwt_claims_returns_none_for_non_jwt() {
        assert!(decode_jwt_claims("opaque-access-token").is_none());
        assert!(decode_jwt_claims("only.two").is_none());
        // Valid structure but the payload is not valid base64/JSON.
        assert!(decode_jwt_claims("aaa.!!!.bbb").is_none());
    }

    #[test]
    fn extract_identity_honors_priority_and_skips_empty() {
        let priority: Vec<String> = DEFAULT_IDENTITY_CLAIMS
            .iter()
            .map(|c| (*c).to_owned())
            .collect();
        // `email` is empty, so the next non-empty claim (`preferred_username`) wins.
        let claims = serde_json::from_value(json!({
            "email": "",
            "preferred_username": "jdoe",
            "name": "Jane Doe",
        }))
        .expect("claims map");
        assert_eq!(extract_identity(&claims, &priority), "jdoe");

        // No matching claim yields an empty identity.
        let empty = serde_json::from_value(json!({"unrelated": "x"})).expect("claims map");
        assert_eq!(extract_identity(&empty, &priority), "");
    }

    #[test]
    fn build_credential_populates_identity_and_sub() {
        let provider = test_provider();
        let token = valid_token(&make_jwt(&json!({
            "email": "user@example.com",
            "sub": "subject-1",
        })));
        let credential = provider.build_credential("prod", &token);
        assert_eq!(credential.identity, "user@example.com");
        assert_eq!(credential.sub, "subject-1");
        assert_eq!(credential.env, "prod");
        assert_eq!(credential.provider, "test");
    }

    #[test]
    fn build_credential_leaves_identity_blank_for_opaque_token() {
        let provider = test_provider();
        let token = valid_token("opaque-token");
        let credential = provider.build_credential("prod", &token);
        assert_eq!(credential.identity, "");
        assert_eq!(credential.sub, "");
    }

    #[test]
    fn with_identity_claims_overrides_selection() {
        let provider = test_provider().with_identity_claims(&["custom_user"]);
        let token = valid_token(&make_jwt(&json!({
            "email": "ignored@example.com",
            "custom_user": "picked",
        })));
        let credential = provider.build_credential("prod", &token);
        assert_eq!(credential.identity, "picked");
    }

    /// In-memory [`CredentialStorage`] double: lets us assert the provider
    /// delegates load/save/delete without any real keychain or filesystem.
    #[derive(Debug, Default)]
    struct MemoryStorage {
        entries: std::sync::Mutex<HashMap<String, String>>,
    }

    impl MemoryStorage {
        fn entry_key(key: &CredentialKey<'_>) -> String {
            format!("{}/{}/{}", key.app_id, key.provider, key.env)
        }
    }

    #[async_trait]
    impl CredentialStorage for MemoryStorage {
        async fn load(&self, key: &CredentialKey<'_>) -> Option<String> {
            self.entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&Self::entry_key(key))
                .cloned()
        }

        async fn save(&self, key: &CredentialKey<'_>, value: &str) -> Result<()> {
            self.entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(Self::entry_key(key), value.to_owned());
            Ok(())
        }

        async fn delete(&self, key: &CredentialKey<'_>) {
            self.entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&Self::entry_key(key));
        }
    }

    #[test]
    #[allow(deprecated)]
    fn with_file_fallback_maps_to_store_modes() {
        assert_eq!(
            test_provider().with_file_fallback(true).store_mode,
            Some(CredentialStore::Auto)
        );
        assert_eq!(
            test_provider().with_file_fallback(false).store_mode,
            Some(CredentialStore::Keyring)
        );
    }

    #[test]
    fn builders_record_storage_selection() {
        assert_eq!(
            test_provider()
                .with_credential_store(CredentialStore::File)
                .store_mode,
            Some(CredentialStore::File)
        );
        let provider = test_provider().with_storage(Arc::new(MemoryStorage::default()));
        assert!(provider.storage_override.is_some());
    }

    #[tokio::test]
    async fn provider_delegates_to_injected_storage() {
        let mem = Arc::new(MemoryStorage::default());
        let provider = test_provider().with_app_id("app").with_storage(mem.clone());

        // No entry yet: status reports not-logged-in.
        assert!(provider.status("dev").await.is_err());

        // Saving routes through the injected store.
        provider
            .save_stored("dev", &valid_token("tok"))
            .await
            .expect("save");
        let key = CredentialKey::new("app", "test", "dev");
        assert!(mem.load(&key).await.is_some(), "token reached the store");

        // And status reads it back.
        let cred = provider.status("dev").await.expect("status");
        assert_eq!(cred.token, "tok");

        // Logout clears it from the store.
        provider.logout("dev").await.expect("logout");
        assert!(mem.load(&key).await.is_none(), "token removed on logout");
    }

    #[tokio::test]
    async fn corrupt_stored_blob_self_heals() {
        let mem = Arc::new(MemoryStorage::default());
        let key = CredentialKey::new("app", "test", "dev");
        mem.save(&key, "not-valid-json").await.expect("seed");

        let provider = test_provider().with_app_id("app").with_storage(mem.clone());
        assert!(provider.load_stored("dev").await.is_none());
        assert!(
            mem.load(&key).await.is_none(),
            "corrupt blob should be deleted (self-heal)"
        );
    }

    #[tokio::test]
    // The guard is intentionally held across awaits to serialize env mutation.
    #[allow(clippy::await_holding_lock)]
    async fn file_store_round_trips_without_keyring() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Hold the shared lock + env guard across the awaits.
        let _lock = crate::config::test_env::lock();
        let _env = crate::config::test_env::EnvVarGuard::set("XDG_CONFIG_HOME", Some(dir.path()));

        let provider = test_provider()
            .with_app_id("app")
            .with_credential_store(CredentialStore::File);
        assert!(provider.status("dev").await.is_err());
        provider
            .save_stored("dev", &valid_token("filetok"))
            .await
            .expect("save");
        let cred = provider.status("dev").await.expect("status");
        assert_eq!(cred.token, "filetok");
    }

    /// Serializes env-var access and removes the vars on drop (matching the
    /// convention in `src/environments.rs` tests). Vars persist process-wide, so
    /// concurrent tests would otherwise see each other's writes.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard removing an env var on drop, even on panic, while ENV_LOCK held.
    struct EnvGuard(&'static str);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: the test holds ENV_LOCK for the guard's lifetime.
            unsafe { std::env::remove_var(self.0) }
        }
    }

    /// The fix: a provider wired to a shared `Arc<Environments>` whose
    /// `environments.toml` file layer defines `prod` with a different `client_id`
    /// resolves the FILE's client id. This proves the provider's file layer
    /// resolves — the shared, app_id-stamped instance reaches the provider rather
    /// than an unstamped copy whose file path is `None`.
    #[test]
    fn wired_provider_resolves_client_id_from_environments_file() {
        use crate::environments::{EnvironmentDef, Environments};

        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[prod]
client_id = "file-prod-client"
"#,
        )
        .expect("write environments.toml");

        let environments = Arc::new(
            Environments::new("prod")
                .with_app_id("x")
                .with_environment(
                    "prod",
                    EnvironmentDef::new().with_client_id("compiled-prod-client"),
                )
                .with_config_file_path_override(file),
        );

        let provider = PkceAuthProvider::new(
            "godaddy",
            "https://base/auth",
            "https://base/token",
            "base-client",
            &["openid"],
        )
        .with_environments(environments);

        // The file overrides the compiled client id, which itself overrides the
        // provider's base — proving the wired provider reads the file layer.
        assert_eq!(
            provider.effective_oauth("prod").client_id,
            "file-prod-client"
        );
    }

    /// Legacy escape hatch: a NON-wired provider still honors the provider-prefixed
    /// `<PREFIX>_OAUTH_CLIENT_ID` env var, and a wired provider whose resolved env
    /// yields a non-empty client id takes precedence over that legacy var.
    #[test]
    fn legacy_oauth_client_id_env_var_overrides_base_but_yields_to_wired_env() {
        use crate::environments::{EnvironmentDef, Environments};

        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `<PREFIX>` = provider name uppercased, '-' -> '_'. Provider is "test".
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit.
        unsafe { std::env::set_var("TEST_OAUTH_CLIENT_ID", "legacy-client") };
        let _guard = EnvGuard("TEST_OAUTH_CLIENT_ID");

        // Non-wired provider: the legacy var overrides the base client id.
        let bare = test_provider();
        assert_eq!(bare.effective_oauth("prod").client_id, "legacy-client");

        // Wired provider whose resolved env carries a non-empty client id: the
        // resolved environment wins over the legacy var.
        let environments = Arc::new(
            Environments::new("prod")
                .with_app_id("x")
                .with_environment("prod", EnvironmentDef::new().with_client_id("env-client")),
        );
        let wired = test_provider().with_environments(environments);
        assert_eq!(wired.effective_oauth("prod").client_id, "env-client");
    }
}
