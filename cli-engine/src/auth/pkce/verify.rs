//! Signature verification of the OIDC id_tokens a
//! [`PkceAuthProvider`](super::PkceAuthProvider) receives.
//!
//! Without a verifier the provider decodes id_token claims without checking
//! anything about them, which is fine for display but not for a decision such as
//! authorization. [`IdTokenVerifier`] is the hook for a consumer that needs the
//! claims to be authentic; `JwksVerifier` (feature `id-token-verify`) is a ready-made
//! implementation.

use async_trait::async_trait;
use serde_json::{Map, Value};

use super::stored_token::StoredToken;
use crate::{Result, error::CliCoreError};

/// The claims of an id_token that a verifier has authenticated.
#[derive(Clone, PartialEq)]
pub struct VerifiedIdToken {
    claims: Map<String, Value>,
}

impl VerifiedIdToken {
    /// Wraps claims that the caller has already authenticated.
    #[must_use]
    pub fn new(claims: Map<String, Value>) -> Self {
        Self { claims }
    }

    /// The authenticated claims.
    #[must_use]
    pub fn claims(&self) -> &Map<String, Value> {
        &self.claims
    }
}

impl std::fmt::Debug for VerifiedIdToken {
    /// Lists claim names only: values such as an email address are personal data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedIdToken")
            .field("claims", &self.claims.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Why an [`IdTokenVerifier`] did not return claims.
///
/// The two cases are handled differently: a rejected token is not trusted and
/// the provider falls back to a refresh or a new login, whereas a verification
/// that could not run must not be answered by starting a login, which would need
/// the same keys and could overwrite a credential that is still good.
///
/// Reasons are shown to users and logged, so they must never contain the token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// The id_token is not acceptable: bad signature, wrong issuer or audience,
    /// expired, unknown signing key, unsupported algorithm, or malformed.
    #[error("{0}")]
    Invalid(String),
    /// Verification could not be carried out, for example because the signing
    /// keys could not be fetched. Says nothing about the token itself.
    #[error("{0}")]
    Unavailable(String),
}

/// Authenticates an id_token before a
/// [`PkceAuthProvider`](super::PkceAuthProvider) trusts anything in it.
///
/// Register one with
/// [`PkceAuthProvider::with_id_token_verifier`](super::PkceAuthProvider::with_id_token_verifier).
/// It then runs on every id_token the provider handles: at login, after a refresh,
/// and when a stored credential is read back, which is the case that matters most
/// because the stored copy is writable by anything running as the user.
///
/// With a verifier configured the provider:
///
/// - takes `identity` and `sub` from the claims the verifier returns, never from
///   the unauthenticated access token or from the stored copy;
/// - requires an id_token, so the `openid` scope must be requested;
/// - never persists or reuses a token the verifier rejected.
// async_trait's expansion attaches a bare `#[must_use]` to each generated
// method, duplicating the one already implied by `Result`.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait IdTokenVerifier: Send + Sync + std::fmt::Debug {
    /// Verifies `id_token`, which was issued to `audience` (the OAuth client id),
    /// and returns its authenticated claims.
    ///
    /// # Errors
    ///
    /// [`VerifyError::Invalid`] when the token must not be trusted, and
    /// [`VerifyError::Unavailable`] when no decision could be made.
    async fn verify(
        &self,
        id_token: &str,
        audience: &str,
    ) -> std::result::Result<VerifiedIdToken, VerifyError>;
}

/// The outcome of checking a token, short of "could not check" (an `Err`).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verification {
    /// Authenticated. The verified claims are now on the token.
    Verified,
    /// The token is not to be trusted; the string says why.
    Rejected(String),
}

/// Runs `verifier` over `token`'s id_token and, when it passes, attaches the
/// verified claims to `token` in memory.
///
/// # Errors
///
/// Returns an error only when verification could not run
/// ([`VerifyError::Unavailable`]).
pub(super) async fn verify_token(
    verifier: &dyn IdTokenVerifier,
    audience: &str,
    token: &mut StoredToken,
) -> Result<Verification> {
    let Some(id_token) = token.id_token.as_deref() else {
        return Ok(Verification::Rejected(
            "there is no id_token to verify (request the `openid` scope)".to_owned(),
        ));
    };
    match verifier.verify(id_token, audience).await {
        Ok(verified) => {
            token.verified_claims = Some(verified.claims);
            Ok(Verification::Verified)
        }
        Err(VerifyError::Invalid(reason)) => Ok(Verification::Rejected(reason)),
        Err(VerifyError::Unavailable(reason)) => Err(CliCoreError::message(format!(
            "could not verify the id_token: {reason}"
        ))),
    }
}
