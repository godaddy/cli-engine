//! The token set persisted for one `(app, provider, env)`, and the choice of which
//! of its tokens becomes the bearer credential.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::{TOKEN_EXPIRY_BUFFER_SECS, scopes::decode_jwt_claims};
use crate::{Result, error::CliCoreError};

/// Which token a [`PkceAuthProvider`](super::PkceAuthProvider) puts in
/// [`Credential::token`](crate::Credential::token).
///
/// Most resource servers expect the OAuth access token, which is the default.
/// Some validate the OIDC id_token instead, and [`Bearer::IdToken`] serves those.
///
/// With [`Bearer::IdToken`]:
///
/// - `Credential::token` is the id_token and `Credential::expires_at` is its
///   `exp` claim. Like every other claim this provider decodes, `exp` is read
///   without verifying the signature; the resource server decides whether the
///   token is acceptable.
/// - A login must return an id_token carrying `exp` (request the `openid`
///   scope). One that does not fails, and nothing is stored, rather than
///   handing out the access token.
/// - Whether a stored token can be reused is judged by the id_token's expiry,
///   not the access token's. An expired one is renewed with the refresh token
///   only when the refresh response carries a fresh id_token. The id_token from
///   before the refresh is never carried forward, so a refresh without one is
///   discarded and the next command logs in interactively.
/// - A stored token with no usable id_token (for example one written before this
///   option was enabled) counts as not logged in: commands log in again and
///   `auth status` reports an error for it.
///
/// # Examples
///
/// ```
/// use cli_engine::auth::pkce::{Bearer, PkceAuthProvider};
///
/// let provider = PkceAuthProvider::new(
///     "okta",
///     "https://idp.example.com/oauth2/v1/authorize",
///     "https://idp.example.com/oauth2/v1/token",
///     "my-client-id",
///     &["openid", "email"],
/// )
/// .with_bearer(Bearer::IdToken);
/// # let _ = provider;
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Bearer {
    /// The OAuth access token. This is the default.
    #[default]
    AccessToken,
    /// The OIDC id_token. Requires the `openid` scope.
    IdToken,
}

/// Stored token with expiry tracking.
///
/// Token fields are zeroized on drop to limit in-memory exposure.
#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(super) struct StoredToken {
    pub(super) access_token: String,
    pub(super) expires_at: i64,
    pub(super) refresh_token: Option<String>,
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
    pub(super) scopes: Vec<String>,
    /// OIDC ID token, present when `openid` is among the granted scopes. Many
    /// IdPs (GoDaddy's included) leave profile claims (`email`/`name`/etc.) off
    /// the access token even when `openid`/`profile` are granted — those claims
    /// live here instead. [`PkceAuthProvider::build_credential`] merges this
    /// token's claims over the access token's (winning on overlap) when
    /// deriving `Credential.identity`/`sub`, and [`Bearer::IdToken`] makes it the
    /// credential's token.
    ///
    /// `#[serde(default)]` keeps tokens cached before this field was added
    /// loadable (they decode with `None`, falling back to the access token's
    /// own claims exactly as before). Not `#[zeroize(skip)]`: like
    /// `access_token`/`refresh_token`, it can carry sensitive claims.
    #[serde(default)]
    pub(super) id_token: Option<String>,
    /// The id_token's claims as authenticated by the provider's
    /// [`IdTokenVerifier`](super::IdTokenVerifier), when one is configured.
    ///
    /// Held in memory only. `#[serde(skip)]` keeps it out of what is written to
    /// storage and, just as important, out of what is read back: the stored copy
    /// is writable by anything running as the user, so a value found there must
    /// never count as verified. Only a verifier in this process can set it. Not
    /// secret, so excluded from zeroization.
    #[serde(skip)]
    #[zeroize(skip)]
    pub(super) verified_claims: Option<Map<String, Value>>,
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
            .field(
                "id_token",
                if self.id_token.is_some() {
                    &"Some([redacted])"
                } else {
                    &"None"
                },
            )
            .field("scopes", &self.scopes)
            .field("verified", &self.verified_claims.is_some())
            .finish()
    }
}

impl StoredToken {
    fn is_valid(&self) -> bool {
        outlives_expiry_buffer(self.expires_at)
    }

    /// Whether the token `bearer` selects can be handed out without a refresh.
    pub(super) fn is_valid_for(&self, bearer: Bearer) -> bool {
        match bearer {
            Bearer::AccessToken => self.is_valid(),
            Bearer::IdToken => self.id_token_is_fresh(),
        }
    }

    /// Whether the id_token carries a usable `exp` that is not about to pass.
    ///
    /// Under an [`IdTokenVerifier`](super::IdTokenVerifier) this bounds how long
    /// the identity it vouched for may be reused, whichever token is the bearer:
    /// an id_token commonly expires before the access token does.
    pub(super) fn id_token_is_fresh(&self) -> bool {
        self.id_token_expiry().is_some_and(outlives_expiry_buffer)
    }

    /// The token `bearer` selects, and when it expires (Unix seconds).
    ///
    /// # Errors
    ///
    /// Returns an error when `bearer` is [`Bearer::IdToken`] and there is no
    /// id_token, it does not decode, or it carries no usable `exp` claim (absent,
    /// not a number, or outside the representable date range).
    pub(super) fn require_bearer(&self, bearer: Bearer, env: &str) -> Result<(&str, i64)> {
        let selected = match bearer {
            Bearer::AccessToken => Some((self.access_token.as_str(), self.expires_at)),
            Bearer::IdToken => self.id_token.as_deref().zip(self.id_token_expiry()),
        };
        selected.ok_or_else(|| {
            CliCoreError::message(format!(
                "no usable id_token for environment {env:?}: this provider uses the id_token \
                 as its bearer credential, so the login must return one with an `exp` claim \
                 (request the `openid` scope)"
            ))
        })
    }

    /// The id_token's `exp` claim in Unix seconds, or `None` when it is absent,
    /// not a number, or outside the range [`chrono`] can represent.
    ///
    /// The range check matters because the id_token is untrusted input: an
    /// unrepresentable `exp` would otherwise saturate the cast to an extreme
    /// `i64`, which either overflows the expiry arithmetic or renders as an empty
    /// (and so seemingly non-expiring) `Credential::expires_at`. RFC 7519 allows a
    /// fractional `NumericDate`, so one is rounded down rather than rejected.
    fn id_token_expiry(&self) -> Option<i64> {
        let claims = decode_jwt_claims(self.id_token.as_deref()?)?;
        let exp = claims.get("exp")?;
        let seconds = exp.as_i64().or_else(|| {
            exp.as_f64()
                .filter(|seconds| seconds.is_finite())
                .map(|seconds| seconds.floor() as i64)
        })?;
        chrono::DateTime::from_timestamp(seconds, 0).map(|_| seconds)
    }
}

fn outlives_expiry_buffer(expires_at: i64) -> bool {
    expires_at.saturating_sub(TOKEN_EXPIRY_BUFFER_SECS) > chrono::Utc::now().timestamp()
}
