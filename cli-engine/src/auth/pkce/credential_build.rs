//! How a stored token becomes the [`Credential`] a command receives.
//!
//! Free functions rather than methods: they read the provider's settings (the bearer,
//! the verifier, the identity claims), and the lint against multiple inherent impls
//! keeps the provider's methods in one place.

use serde_json::Value;

use super::{
    PkceAuthProvider, StoredToken, decode_jwt_claims, extract_identity, granted_scopes,
    rfc3339_secs,
};
use crate::{Credential, Result};

/// Builds the [`Credential`] handed to callers: [`build_credential`]
/// with the token and expiry this provider's [`Bearer`] selects.
///
/// # Errors
///
/// Returns an error when the id_token is the bearer and `token` has no usable one.
pub(super) fn bearer_credential(
    provider: &PkceAuthProvider,
    env: &str,
    token: &StoredToken,
) -> Result<Credential> {
    let (bearer_token, expires_at) = token.require_bearer(provider.bearer, env)?;
    let mut credential = build_credential(provider, env, token);
    bearer_token.clone_into(&mut credential.token);
    credential.expires_at = rfc3339_secs(expires_at);
    Ok(credential)
}

/// Builds a [`Credential`] from a stored token, deriving `identity` and `sub`
/// from the access-token JWT claims when present.
pub(super) fn build_credential(
    provider: &PkceAuthProvider,
    env: &str,
    token: &StoredToken,
) -> Credential {
    // The id_token (see `StoredToken::id_token`) is the OIDC-standard home
    // for profile claims; merge it over the access token's claims, letting
    // its values win on a shared key, so `identity`/`sub` reflect whichever
    // token actually carries them.
    let claims = if provider.verifier.is_some() {
        // Only authenticated claims, and none when the token was not verified:
        // an unauthenticated one must not name the identity.
        token.verified_claims.clone().unwrap_or_default()
    } else {
        let mut claims = decode_jwt_claims(&token.access_token).unwrap_or_default();
        if let Some(id_claims) = token.id_token.as_deref().and_then(decode_jwt_claims) {
            claims.extend(id_claims);
        }
        claims
    };
    let identity = extract_identity(&claims, &provider.identity_claims);
    let sub = claims
        .get("sub")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Credential {
        token: token.access_token.clone(),
        env: env.to_owned(),
        provider: provider.name.clone(),
        expires_at: rfc3339_secs(token.expires_at),
        identity,
        sub,
        scopes: granted_scopes(token),
        refreshable: token.refresh_token.is_some(),
        ..Credential::default()
    }
}
