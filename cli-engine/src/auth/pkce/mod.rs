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
//!   `<config-base>` is `$XDG_CONFIG_HOME`, `$HOME/Library/Application
//!   Support` (macOS), `$HOME/.config` (other Unix), or `%APPDATA%` (Windows).
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
//! A field the resolved environment leaves empty falls back to the base
//! config passed to
//! [`PkceAuthProvider::new`](crate::auth::pkce::PkceAuthProvider::new) — there
//! is no environment-variable override for OAuth fields.

use std::{collections::HashMap, net::TcpListener, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    Credential, Result,
    auth::AuthProvider,
    auth::CredentialRequest,
    auth::storage::{CredentialKey, CredentialStorage, default_storage, storage_for},
    config::CredentialStore,
    env_config::{EnvConfig, SourceChain, ValueSource},
    error::CliCoreError,
};

mod callback_server;
mod scopes;
#[cfg(test)]
mod tests;

use callback_server::{
    emit_auth_complete_message, emit_browser_login_prompt, pkce_challenge, random_state,
    wait_for_callback,
};
pub use scopes::ScopeHierarchy;
use scopes::{
    StepUp, decode_jwt_claims, ensure_granted, extract_identity, granted_scopes,
    parse_token_response, plan_step_up, union_scopes,
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
        let now = chrono::Utc::now().timestamp();
        self.expires_at - TOKEN_EXPIRY_BUFFER_SECS > now
    }
}

/// The effective OAuth values for an environment. No default on
/// `client_id`/`auth_url`/`token_url`: a provider whose base config and
/// resolved environment both leave one of these blank must fail loudly
/// (`EnvConfigError::MissingField`), not silently assemble with an empty
/// endpoint. `scopes` is different — an empty scope list is a normal,
/// supported configuration (see [`PkceAuthProvider::effective_scopes`]), not
/// a sign of a never-initialized provider.
#[derive(Debug, Clone, Default, EnvConfig)]
struct OAuthSection {
    client_id: String,
    auth_url: String,
    token_url: String,
    #[env_config(default = Vec::new())]
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
    /// the resolved environment instead of the base config passed to
    /// [`PkceAuthProvider::new`]. Looked up by the `env` passed to
    /// [`AuthProvider::get_credential`].
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
    /// Scope implication relationships from [`PkceAuthProvider::with_scope_hierarchy`].
    /// Empty by default, which preserves exact-string scope matching.
    scope_hierarchy: ScopeHierarchy,
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
        Self {
            name: name.into(),
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
            storage_override: None,
            store_mode: None,
            storage: tokio::sync::OnceCell::new(),
            identity_claims: DEFAULT_IDENTITY_CLAIMS
                .iter()
                .map(|claim| (*claim).to_owned())
                .collect(),
            cache: Arc::new(RwLock::new(HashMap::new())),
            scope_hierarchy: ScopeHierarchy::new(),
        }
    }

    /// Sources per-environment OAuth config from a shared
    /// [`Environments`](crate::environments::Environments).
    ///
    /// Given an `env`, every OAuth-driven method on this provider resolves its
    /// OAuth config from two tiers, highest priority first: the resolved
    /// environment's own TOML value, then this provider's base configuration
    /// from [`PkceAuthProvider::new`]. There is no environment-variable
    /// override for either tier. Prefer wiring an
    /// [`Environments`](crate::environments::Environments) over relying on
    /// the base `client_id`/`auth_url`/`token_url` when the consumer registers
    /// environments via
    /// [`CliConfig::with_environments`](crate::CliConfig::with_environments) —
    /// it's the single-source-of-truth path.
    ///
    /// A field absent from the resolved environment falls through to the
    /// base config, so a partial environment can override only the client id
    /// while inheriting the provider's base endpoints.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use cli_engine::{
    ///     auth::pkce::PkceAuthProvider,
    ///     environments::{EnvTable, Environments},
    /// };
    ///
    /// let environments = Arc::new(
    ///     Environments::new("prod").with_environment(
    ///         "dev",
    ///         EnvTable::new()
    ///             .with("client_id", "dev-client-id")
    ///             .with("auth_url", "https://api.dev-godaddy.com/v2/oauth2/authorize")
    ///             .with("token_url", "https://api.dev-godaddy.com/v2/oauth2/token"),
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

    /// Declares scope implication relationships (for example, a granted
    /// `admin` scope covering a required `read` scope) so step-up only
    /// re-authenticates when the current token genuinely lacks a required
    /// scope.
    ///
    /// Empty by default, which preserves exact-string scope matching.
    #[must_use]
    pub fn with_scope_hierarchy(mut self, hierarchy: ScopeHierarchy) -> Self {
        self.scope_hierarchy = hierarchy;
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
            scopes: granted_scopes(token),
            refreshable: token.refresh_token.is_some(),
            ..Credential::default()
        }
    }

    /// Computes the effective OAuth config for `env` with a SINGLE environment
    /// resolution (at most one `environments.toml` read), by assembling an
    /// [`OAuthSection`] from a two-tier [`SourceChain`], highest priority
    /// first:
    ///
    /// 1. The resolved environment's own TOML value (compiled + file layers).
    /// 2. This provider's own base config, from [`PkceAuthProvider::new`].
    ///
    /// Token flows call this once and reuse the result so they don't re-read the
    /// environments file once per field.
    ///
    /// # Errors
    ///
    /// Returns an error if a present TOML value fails to convert to its
    /// field's type (for example a non-array `scopes` value), or if
    /// `client_id`/`auth_url`/`token_url` is blank in *both* tiers — a
    /// provider whose base config was never given a real value, and whose
    /// resolved environment (if any) doesn't supply one either, fails loudly
    /// rather than assembling with an empty endpoint.
    fn effective_oauth(&self, env: &str) -> Result<OAuthSection> {
        let env_source =
            self.environments
                .as_ref()
                .and_then(|environments| match environments.source(env) {
                    Ok(source) => Some(source),
                    Err(err) => {
                        tracing::debug!(
                            env,
                            error = %err,
                            "environment resolve failed; falling back to base OAuth config"
                        );
                        None
                    }
                });
        let base = ValueSource::new()
            .with("client_id", self.client_id.clone())
            .with("auth_url", self.auth_url.clone())
            .with("token_url", self.token_url.clone())
            .with("scopes", self.scopes.clone());

        let mut chain = SourceChain::new();
        if let Some(env_source) = &env_source {
            chain = chain.push(env_source);
        }
        chain = chain.push(&base);

        OAuthSection::assemble(&chain).map_err(CliCoreError::from)
    }

    /// Default scopes for `env`: the resolved environment's scopes when
    /// non-empty, otherwise the provider's base scopes.
    ///
    /// # Errors
    ///
    /// See [`effective_oauth`](Self::effective_oauth).
    fn effective_scopes(&self, env: &str) -> Result<Vec<String>> {
        Ok(self.effective_oauth(env)?.scopes)
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
        let scopes = self.effective_scopes(env)?;
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
        let oauth = self.effective_oauth(env)?;
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
        let listener = TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], bind_port)))
            .map_err(|err| {
            CliCoreError::message(format!(
                "failed to bind callback server on port {bind_port}: {err}"
            ))
        })?;

        emit_browser_login_prompt(&url);
        drop(open::that(url.as_str()));

        let code =
            wait_for_callback(listener, &state, &callback_path, Duration::from_secs(120)).await?;
        let token = self
            .exchange_code_for_token(&oauth, &code, &code_verifier, scopes)
            .await?;
        emit_auth_complete_message();
        Ok(token)
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
        oauth: &OAuthSection,
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
        let oauth = self.effective_oauth(env)?;
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

        // Look for a usable token WITHOUT launching a flow, so we can pick the
        // scope set for a single login rather than authenticating twice (e.g.
        // `auth login --scope X` logs out first; resolving defaults and then
        // stepping up would open the browser twice).
        if let Some(token) = self.existing_token(env).await? {
            // Decide based on what the token grants (JWT claim plus the scopes it
            // was obtained with).
            let granted = granted_scopes(&token);
            match plan_step_up(&granted, required, &self.scope_hierarchy) {
                StepUp::Covered => return Ok(self.build_credential(env, &token)),
                // Step-up is re-consent: the authorization server has no silent
                // scope-expansion grant, so acquire the missing scopes with a
                // fresh login — the same browser flow the no-token path runs
                // below, rather than failing when stdio is not a TTY. Resolve
                // per-env defaults only now (off the cached-token hot path) and
                // request defaults ∪ already-granted ∪ required so step-up never
                // drops previously-acquired scopes.
                StepUp::Reauthenticate => {
                    let union = union_scopes(&self.effective_scopes(env)?, &granted, required);
                    let token = self.reauthenticate(env, &union).await?;
                    ensure_granted(env, &token, required, &self.scope_hierarchy)?;
                    return Ok(self.build_credential(env, &token));
                }
            }
        }

        // No usable token: authenticate once, requesting defaults ∪ required.
        let union = union_scopes(&self.effective_scopes(env)?, &[], required);
        let token = self.reauthenticate(env, &union).await?;
        ensure_granted(env, &token, required, &self.scope_hierarchy)?;
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
