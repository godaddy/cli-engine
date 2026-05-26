//! Auth provider abstraction and built-in auth helpers.
//!
//! Consumer CLIs normally register one or more [`AuthProvider`] implementations
//! with [`crate::CliConfig`]. Middleware then resolves credentials before
//! business logic runs and passes a [`Credential`] to command handlers.
//!
//! The module also contains an [`crate::auth::exec::ExecProvider`] for provider
//! binaries that speak the JSON stdin/stdout contract.
//!
//! When the `pkce-auth` feature is enabled, [`pkce::PkceAuthProvider`] adds a
//! built-in browser-based OAuth 2.0 PKCE flow with system keychain storage.

/// Built-in `auth login`, `auth status`, and `auth logout` command helpers.
pub mod commands;
mod credential;
mod dispatcher;
/// External process auth provider implementation.
pub mod exec;
/// OAuth 2.0 PKCE auth provider (requires the `pkce-auth` feature).
#[cfg(feature = "pkce-auth")]
pub mod pkce;

use async_trait::async_trait;

pub use commands::{
    AuthLoginResult, AuthStatusEntry, auth_command_group, login_and_build, logout_result,
    status_result, to_status_entry,
};
pub use credential::{CACHE_TTL, Credential};
pub use dispatcher::{Dispatcher, SingleProvider, StatusEntry};
pub use exec::{
    ACTION_AUTHENTICATE, ACTION_LIST_ENVIRONMENTS, ACTION_LIST_REALMS, ACTION_LOGOUT,
    ACTION_STATUS, AuthnRequest, EnvironmentsResponse, ExecProvider,
};

use crate::Result;

#[async_trait]
/// Named auth provider used by middleware and transport injectors.
///
/// Implementations own their credential cache strategy. The framework only
/// routes calls and passes command context (`env`, colon command path, and tier).
pub trait AuthProvider: Send + Sync + std::fmt::Debug {
    /// Stable provider registration name, for example `primary` or `oauth`.
    fn name(&self) -> &str;

    /// Returns a credential for `env`, `command`, and `tier`.
    async fn get_credential(&self, env: &str, command: &str, tier: &str) -> Result<Credential>;

    /// Returns cached credential status for one environment.
    async fn status(&self, env: &str) -> Result<Credential>;

    /// Clears cached credentials for one environment.
    async fn logout(&self, env: &str) -> Result<()>;

    /// Lists environments with cached credentials.
    async fn list_environments(&self) -> Result<Vec<String>>;
}
