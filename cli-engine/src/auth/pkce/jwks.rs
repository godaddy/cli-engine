//! A ready-made [`IdTokenVerifier`] that checks an id_token against the issuer's
//! published JSON Web Key Set.

use std::time::Duration;

use async_trait::async_trait;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    errors::ErrorKind,
    jwk::{Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use super::verify::{IdTokenVerifier, VerifiedIdToken, VerifyError};

/// Per-fetch bound on the key-set request. There is no separate command-level
/// budget here: the engine's `--timeout` cancels the whole command, and this
/// keeps an unresponsive key endpoint from consuming all of it.
const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on establishing the connection, so an unroutable key endpoint fails fast
/// instead of using up the whole fetch budget.
const JWKS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest key set accepted, so a hostile or broken endpoint cannot make the CLI
/// buffer an unbounded response: the body is read incrementally and abandoned once
/// it passes this. Real key sets are a few kilobytes.
const JWKS_MAX_BYTES: usize = 256 * 1024;

/// Asymmetric algorithms only. HMAC is excluded on purpose: with a verifier that
/// accepts it, an attacker can sign a token using the *public* key as the shared
/// secret. `EdDSA` is excluded because identity providers do not use it for
/// id_tokens and the extra surface buys nothing here.
const ALLOWED_ALGORITHMS: [Algorithm; 8] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

/// Verifies id_tokens against a JSON Web Key Set.
///
/// For each token it checks the signature against the key named by the token's
/// `kid`, and the `iss` (issuer), `aud` (audience) and `exp` (expiry) claims. It
/// also applies the OpenID Connect checks a generic JWT validator omits: `sub` must
/// be a non-empty string, and `azp` (authorized party), when present or when `aud`
/// lists several clients, must be this client. The
/// accepted signature algorithms are the asymmetric RSA, RSA-PSS and ECDSA ones;
/// HMAC is never accepted.
///
/// The key set is fetched once per verifier and kept. It is fetched again, at
/// most once per verification, when a token names a `kid` that the kept set does
/// not contain, which is how a key rotation reaches a long-running process. A
/// failure to fetch is reported as [`VerifyError::Unavailable`], not as a bad
/// token.
///
/// The key-set URL must be `https` on the same host as the issuer, with no port or
/// credentials, and redirects are never followed, so a poisoned URL cannot send
/// the fetch elsewhere.
///
/// Requires the `id-token-verify` feature, and a `jsonwebtoken` signature
/// backend that the program enables itself: exactly one of `jsonwebtoken`'s
/// `rust_crypto` or `aws_lc_rs` features.
///
/// This crate does not pick one on purpose. `jsonwebtoken` keeps its backend in a
/// process-wide global, and it panics, on the first token it decodes anywhere in
/// the process, when no backend is enabled or when both are. A backend chosen
/// here would therefore break any program that already uses the other. If the
/// program enables none, the first verification panics with a message from
/// `jsonwebtoken` saying so.
///
/// # Examples
///
/// With `jsonwebtoken = { version = "11", default-features = false, features = ["rust_crypto"] }`
/// (or `aws_lc_rs`) in the program's own `Cargo.toml`:
///
/// ```
/// use std::sync::Arc;
/// use cli_engine::auth::pkce::{JwksVerifier, PkceAuthProvider};
///
/// let verifier = JwksVerifier::new(
///     "https://idp.example.com",
///     "https://idp.example.com/oauth2/v1/keys",
/// )
/// .expect("keys are on the issuer's host");
/// let provider = PkceAuthProvider::new(
///     "idp",
///     "https://idp.example.com/oauth2/v1/authorize",
///     "https://idp.example.com/oauth2/v1/token",
///     "my-client-id",
///     &["openid", "email"],
/// )
/// .with_id_token_verifier(Arc::new(verifier));
/// # let _ = provider;
/// ```
#[derive(Debug)]
pub struct JwksVerifier {
    issuer: String,
    jwks_uri: String,
    client: reqwest::Client,
    keys: Mutex<Option<JwkSet>>,
}

impl JwksVerifier {
    /// Creates a verifier for tokens issued by `issuer`, whose keys are published
    /// at `jwks_uri`.
    ///
    /// # Errors
    ///
    /// Returns an error when `jwks_uri` is not an `https` URL on the issuer's
    /// host without a port or credentials, or when `issuer` is not such a URL.
    pub fn new(issuer: impl Into<String>, jwks_uri: impl Into<String>) -> crate::Result<Self> {
        let (issuer, jwks_uri) = (issuer.into(), jwks_uri.into());
        let invalid = |what: &str| {
            crate::error::CliCoreError::message(format!(
                "id_token verifier: {what} must be an https URL without a port or credentials, \
                 and the key set must be on the issuer's host"
            ))
        };
        let issuer_host = https_host(&issuer).ok_or_else(|| invalid("the issuer"))?;
        let jwks_host = https_host(&jwks_uri).ok_or_else(|| invalid("the key-set URL"))?;
        if issuer_host != jwks_host {
            return Err(invalid("the key-set URL"));
        }
        Self::build(issuer, jwks_uri)
    }

    /// Like [`new`](Self::new) without the URL checks, so tests can point at a
    /// loopback server.
    #[cfg(test)]
    pub(super) fn new_unchecked(
        issuer: impl Into<String>,
        jwks_uri: impl Into<String>,
    ) -> crate::Result<Self> {
        Self::build(issuer.into(), jwks_uri.into())
    }

    fn build(issuer: String, jwks_uri: String) -> crate::Result<Self> {
        // A plain client: the user-agent is attached per request (see `fetch`), as
        // the provider's token requests do. `reqwest_client_builder` would snapshot
        // the process identity here, and a verifier is built during registration,
        // before the identity is published, so it would hold a stale one. It would
        // also carry attribution headers, including a session-correlation hash,
        // to an identity provider that has no use for them.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(JWKS_CONNECT_TIMEOUT)
            .timeout(JWKS_FETCH_TIMEOUT)
            .build()
            .map_err(|err| {
                crate::error::CliCoreError::message(format!(
                    "id_token verifier: could not build an HTTP client: {err}"
                ))
            })?;
        Ok(Self {
            issuer,
            jwks_uri,
            client,
            keys: Mutex::new(None),
        })
    }

    /// Returns the key for `kid`, fetching the key set when it is not held yet or
    /// does not contain `kid`.
    async fn key_for(&self, kid: &str) -> Result<Jwk, VerifyError> {
        let mut held = self.keys.lock().await;
        if let Some(jwk) = held.as_ref().and_then(|set| set.find(kid)) {
            return Ok(jwk.clone());
        }
        let fresh = self.fetch().await?;
        let found = fresh.find(kid).cloned();
        *held = Some(fresh);
        found.ok_or_else(|| {
            VerifyError::Invalid("no published signing key matches the token's `kid`".to_owned())
        })
    }

    async fn fetch(&self) -> Result<JwkSet, VerifyError> {
        let unavailable = |what: &str| VerifyError::Unavailable(format!("signing keys: {what}"));
        let mut response = self
            .client
            .get(&self.jwks_uri)
            .header(
                reqwest::header::USER_AGENT,
                crate::transport::client::default_user_agent(),
            )
            .send()
            .await
            .map_err(|_| unavailable("the key endpoint could not be reached"))?;
        if !response.status().is_success() {
            return Err(unavailable(&format!(
                "the key endpoint answered HTTP {}",
                response.status().as_u16()
            )));
        }
        let too_large = || unavailable("the key endpoint's response is too large");
        // Read in pieces and abandoned the moment it passes the limit, so memory
        // stays bounded whatever `content-length` claims, or whether it is sent.
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| unavailable("the key endpoint's response could not be read"))?
        {
            if body.len() + chunk.len() > JWKS_MAX_BYTES {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| unavailable("the key set could not be parsed"))
    }
}

/// The host of `url` when it is `https` with no port, user or password.
fn https_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let plain = parsed.scheme() == "https"
        && parsed.port().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none();
    plain.then(|| parsed.host_str().map(str::to_ascii_lowercase))?
}

/// The explicit reasons a token is not accepted. They name the rule, never a claim
/// value or the token, because reasons are shown to users.
fn invalid(kind: &ErrorKind) -> VerifyError {
    let reason = match kind {
        ErrorKind::InvalidSignature => "the signature is not valid for the published key",
        ErrorKind::ExpiredSignature => "the token has expired",
        ErrorKind::InvalidIssuer => "the issuer is not the expected one",
        ErrorKind::InvalidAudience => "the audience is not this client",
        ErrorKind::MissingRequiredClaim(_) => "a required claim is missing",
        ErrorKind::InvalidAlgorithm | ErrorKind::MissingAlgorithm => {
            "the signature algorithm is not accepted"
        }
        _ => "the token could not be verified",
    };
    VerifyError::Invalid(reason.to_owned())
}

#[async_trait]
impl IdTokenVerifier for JwksVerifier {
    async fn verify(&self, id_token: &str, audience: &str) -> Result<VerifiedIdToken, VerifyError> {
        let header = decode_header(id_token)
            .map_err(|_| VerifyError::Invalid("the token header is malformed".to_owned()))?;
        if !ALLOWED_ALGORITHMS.contains(&header.alg) {
            return Err(VerifyError::Invalid(
                "the signature algorithm is not accepted".to_owned(),
            ));
        }
        let kid = header
            .kid
            .ok_or_else(|| VerifyError::Invalid("the token names no signing key".to_owned()))?;
        let jwk = self.key_for(&kid).await?;
        check_key_restrictions(&jwk, header.alg)?;
        let key = DecodingKey::from_jwk(&jwk)
            .map_err(|_| VerifyError::Invalid("the published key is not usable".to_owned()))?;

        let mut validation = Validation::new(header.alg);
        validation.algorithms = vec![header.alg];
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        let verified = decode::<Map<String, Value>>(id_token, &key, &validation)
            .map_err(|err| invalid(err.kind()))?;
        check_oidc_claims(&verified.claims, audience)?;
        Ok(VerifiedIdToken::new(verified.claims))
    }
}

/// Refuses a published key whose own declarations rule out this use, so the
/// issuer's restrictions bind us as well as its signing.
///
/// - `use`, when present, must be `sig`.
/// - `key_ops`, when present, must include `verify`. An empty list permits nothing.
/// - `alg`, when present, must be the algorithm the token used: a key published
///   for RS384 is not one an RS256 token may be checked with.
///
/// A key that declares none of these is unrestricted, which is how several
/// providers publish theirs.
fn check_key_restrictions(jwk: &Jwk, token_alg: Algorithm) -> Result<(), VerifyError> {
    let not_for_signatures = |reason: &str| Err(VerifyError::Invalid(reason.to_owned()));
    if matches!(&jwk.common.public_key_use, Some(usage) if *usage != PublicKeyUse::Signature) {
        return not_for_signatures("the published key is not a signature key");
    }
    if matches!(&jwk.common.key_operations, Some(ops) if !ops.contains(&KeyOperations::Verify)) {
        return not_for_signatures("the published key is not a signature key");
    }
    if matches!(jwk.common.key_algorithm, Some(declared) if declared != KeyAlgorithm::from(token_alg))
    {
        return not_for_signatures("the signature algorithm is not accepted");
    }
    Ok(())
}

/// The OpenID Connect Core 3.1.3.7 checks that a generic JWT validator does not
/// make, applied to claims whose signature, issuer, audience membership and expiry
/// have already been checked.
///
/// - `sub` must be a non-empty string: it is the identity, and without one the
///   token authenticates nobody.
/// - `azp`, when present, must be this client. A token whose `aud` lists several
///   clients was issued to one of them, and `azp` names which; a token naming
///   another is not ours to accept even though our id is in its audience.
/// - With several audiences, `azp` must be present and be this client. The spec
///   leaves that a SHOULD; it is required here because accepting such a token
///   without it would take any client that merely appears in `aud` as the holder.
fn check_oidc_claims(claims: &Map<String, Value>, audience: &str) -> Result<(), VerifyError> {
    let has_subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .is_some_and(|sub| !sub.is_empty());
    if !has_subject {
        return Err(VerifyError::Invalid(
            "the token has no usable subject".to_owned(),
        ));
    }
    let several_audiences = claims
        .get("aud")
        .and_then(Value::as_array)
        .is_some_and(|audiences| audiences.len() > 1);
    let wrong_party = match claims.get("azp") {
        None => several_audiences,
        Some(azp) => azp.as_str() != Some(audience),
    };
    if wrong_party {
        return Err(VerifyError::Invalid(
            "this client is not the token's authorized party".to_owned(),
        ));
    }
    Ok(())
}
