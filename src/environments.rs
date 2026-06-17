//! First-class environment definitions and layered resolution.
//!
//! An [`Environments`] value holds compiled-in environment definitions and,
//! optionally, an `environments.toml` file plus `<ENV>_*` env-var overrides.
//! Resolving a name merges those layers (later wins) into an [`Environment`].

use std::collections::BTreeMap;

/// Standard OAuth slice of an environment, consumed by `PkceAuthProvider`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OAuthConfig {
    /// OAuth client id.
    pub client_id: String,
    /// Authorization endpoint.
    pub auth_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Default scopes.
    pub scopes: Vec<String>,
}

/// A fully-resolved environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// Environment name (e.g. `prod`).
    pub name: String,
    /// OAuth configuration, present when the environment participates in OAuth.
    pub oauth: Option<OAuthConfig>,
    /// App-specific fields (for example `api_url`).
    pub extra: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_config_defaults_are_empty() {
        let c = OAuthConfig::default();
        assert!(c.client_id.is_empty() && c.scopes.is_empty());
    }
}
