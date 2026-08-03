//! Auth provider abstraction and built-in auth helpers.
//!
//! Consumer CLIs normally register one or more [`AuthProvider`] implementations
//! with [`crate::CliConfig`]. Middleware then resolves credentials before
//! business logic runs and passes a [`Credential`] to command handlers.
//!
//! The module also contains an [`crate::auth::exec::ExecProvider`] for provider
//! binaries that speak the JSON stdin/stdout contract.
//!
//! When the `pkce-auth` feature is enabled, `pkce::PkceAuthProvider` adds a
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
/// Pluggable credential storage backends (keychain, file, auto).
pub mod storage;

use async_trait::async_trait;

pub use commands::{
    AuthLoginResult, AuthStatusEntry, auth_command_group, login_and_build,
    login_and_build_with_scopes, logout_result, status_result, to_status_entry,
};
pub use credential::{CACHE_TTL, Credential};
pub use dispatcher::{Dispatcher, SingleProvider, StatusEntry};
pub use exec::{
    ACTION_AUTHENTICATE, ACTION_LIST_ENVIRONMENTS, ACTION_LIST_REALMS, ACTION_LOGOUT,
    ACTION_STATUS, AuthnRequest, EnvironmentsResponse, ExecProvider,
};
#[cfg(feature = "pkce-auth")]
pub use storage::{AutoStorage, KeyringStorage};
pub use storage::{CredentialKey, CredentialStorage, FileStorage, default_storage, storage_for};

use crate::Result;
use crate::middleware::CommandMeta;

/// Everything an [`AuthProvider`] may inspect about the command requesting a
/// credential.
///
/// This bundles the routing fields passed to [`AuthProvider::get_credential`]
/// (`env`, colon command path, and tier) together with the command's
/// [`CommandMeta`], so a provider can read richer metadata — for example an
/// OAuth provider reading [`CommandMeta::scopes`] to decide whether the cached
/// token is sufficient. Providers that do not need metadata can ignore it.
///
/// Marked `#[non_exhaustive]` because the framework constructs it (providers only
/// read it) and more request fields may be added over time; build one with
/// [`CredentialRequest::new`] rather than a struct literal so adding a field is
/// not a breaking change for downstream crates.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct CredentialRequest<'req> {
    /// Target environment name.
    pub env: &'req str,
    /// Colon-separated command path, for example `project:list`.
    pub command: &'req str,
    /// Risk tier as a string, for example `read` or `mutate`.
    pub tier: &'req str,
    /// Metadata for the command requesting the credential.
    pub meta: &'req CommandMeta,
}

impl<'req> CredentialRequest<'req> {
    /// Creates a request from the routing fields and command metadata.
    #[must_use]
    pub fn new(
        env: &'req str,
        command: &'req str,
        tier: &'req str,
        meta: &'req CommandMeta,
    ) -> Self {
        Self {
            env,
            command,
            tier,
            meta,
        }
    }
}

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

    /// Returns a credential for a command, given its full [`CredentialRequest`].
    ///
    /// The default implementation ignores the metadata and delegates to
    /// [`get_credential`](AuthProvider::get_credential). Providers that act on
    /// command metadata — such as an OAuth provider performing scope step-up
    /// from [`CommandMeta::scopes`] — override this. The framework calls this
    /// method (not `get_credential`) when resolving credentials, so an override
    /// receives the command's metadata.
    async fn get_credential_for(&self, req: &CredentialRequest<'_>) -> Result<Credential> {
        self.get_credential(req.env, req.command, req.tier).await
    }

    /// Returns cached credential status for one environment.
    async fn status(&self, env: &str) -> Result<Credential>;

    /// Clears cached credentials for one environment.
    async fn logout(&self, env: &str) -> Result<()>;

    /// Lists environments with cached credentials.
    async fn list_environments(&self) -> Result<Vec<String>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_request_new_sets_all_fields() {
        let meta = CommandMeta::default();
        let req = CredentialRequest::new("dev", "app:list", "read", &meta);
        assert_eq!(req.env, "dev");
        assert_eq!(req.command, "app:list");
        assert_eq!(req.tier, "read");
        // `Copy` is preserved (using `req` after copying it must compile).
        let copy = req;
        assert_eq!(copy.env, req.env);
    }
}
