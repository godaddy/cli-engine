//! Resolution of the OAuth client configuration (client id, endpoints, default
//! scopes) for one environment.

use crate::{
    Result,
    env_config::{EnvConfig, SourceChain, ValueSource},
    environments::Environments,
    error::CliCoreError,
};

/// The effective OAuth values for an environment. No default on
/// `client_id`/`auth_url`/`token_url`: a provider whose base config and
/// resolved environment both leave one of these blank must fail loudly
/// (`EnvConfigError::MissingField`), not silently assemble with an empty
/// endpoint. `scopes` is different — an empty scope list is a normal,
/// supported configuration (see `PkceAuthProvider::effective_scopes`), not
/// a sign of a never-initialized provider.
#[derive(Debug, Clone, Default, EnvConfig)]
pub(super) struct OAuthSection {
    pub(super) client_id: String,
    pub(super) auth_url: String,
    pub(super) token_url: String,
    #[env_config(default = Vec::new())]
    pub(super) scopes: Vec<String>,
}

/// The provider's own configuration, from `PkceAuthProvider::new`.
pub(super) struct Base<'cfg> {
    pub(super) client_id: &'cfg str,
    pub(super) auth_url: &'cfg str,
    pub(super) token_url: &'cfg str,
    pub(super) scopes: &'cfg [String],
}

/// Computes the effective OAuth config for `env` with a SINGLE environment
/// resolution (at most one `environments.toml` read), by assembling an
/// [`OAuthSection`] from a two-tier [`SourceChain`], highest priority first:
///
/// 1. The resolved environment's own TOML value (compiled + file layers).
/// 2. The provider's own base config.
///
/// Token flows call this once and reuse the result so they don't re-read the
/// environments file once per field.
///
/// # Errors
///
/// Returns an error if a present TOML value fails to convert to its field's type
/// (for example a non-array `scopes` value), or if `client_id`/`auth_url`/
/// `token_url` is blank in *both* tiers — a provider whose base config was never
/// given a real value, and whose resolved environment (if any) doesn't supply one
/// either, fails loudly rather than assembling with an empty endpoint.
pub(super) fn effective_oauth(
    environments: Option<&Environments>,
    env: &str,
    base: Base<'_>,
) -> Result<OAuthSection> {
    let env_source = environments.and_then(|environments| match environments.source(env) {
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
        .with("client_id", base.client_id.to_owned())
        .with("auth_url", base.auth_url.to_owned())
        .with("token_url", base.token_url.to_owned())
        .with("scopes", base.scopes.to_vec());

    let mut chain = SourceChain::new();
    if let Some(env_source) = &env_source {
        chain = chain.push(env_source);
    }
    chain = chain.push(&base);

    OAuthSection::assemble(&chain).map_err(CliCoreError::from)
}
