use std::collections::{HashMap, HashSet};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::StoredToken;
use crate::{Result, error::CliCoreError};

#[derive(Debug, Deserialize)]
pub(super) struct TokenResponse {
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
pub(super) fn decode_jwt_claims(token: &str) -> Option<Map<String, Value>> {
    // A JWT is `header.payload.signature`; the payload is the middle segment,
    // base64url-encoded without padding.
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Returns `defaults ∪ granted ∪ required`, order-preserving and de-duplicated.
pub(super) fn union_scopes(
    defaults: &[String],
    granted: &[String],
    required: &[String],
) -> Vec<String> {
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
pub(super) fn scopes_from_jwt(token: &str) -> Vec<String> {
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

/// Scope implication relationships for an identity provider whose scopes
/// nest (for example, a granted `write` scope also covering `read`).
///
/// Attach one to a [`PkceAuthProvider`](super::PkceAuthProvider) with
/// [`with_scope_hierarchy`](super::PkceAuthProvider::with_scope_hierarchy) so
/// scope coverage checks stop treating scopes as opaque, unrelated strings.
/// Empty by default, which falls back to exact-string matching — today's
/// behavior.
#[derive(Debug, Default, Clone)]
pub struct ScopeHierarchy {
    implies: HashMap<String, Vec<String>>,
}

impl ScopeHierarchy {
    /// Creates an empty hierarchy, equivalent to exact-string scope matching.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares that holding `scope` also satisfies every scope in `implied`.
    ///
    /// Implications compose transitively: if `admin` implies `write` and
    /// `write` implies `read`, a granted `admin` scope covers a required
    /// `read` scope.
    #[must_use]
    pub fn with_implication(
        mut self,
        scope: impl Into<String>,
        implied: &[impl AsRef<str>],
    ) -> Self {
        self.implies
            .entry(scope.into())
            .or_default()
            .extend(implied.iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// True if `required` is present in `granted` verbatim, or transitively
    /// implied by something in `granted`.
    pub(super) fn covers(&self, granted: &[String], required: &str) -> bool {
        let mut queue: Vec<&str> = granted.iter().map(String::as_str).collect();
        let mut visited: HashSet<&str> = HashSet::new();
        while let Some(scope) = queue.pop() {
            if scope == required {
                return true;
            }
            if !visited.insert(scope) {
                continue;
            }
            if let Some(implied) = self.implies.get(scope) {
                queue.extend(implied.iter().map(String::as_str));
            }
        }
        false
    }
}

/// All scopes an access token is known to carry: the JWT `scope`/`scp` claim
/// plus the scopes recorded when the token was obtained. The recorded scopes
/// make coverage work for opaque tokens and IdPs that omit scopes from the
/// access token.
pub(super) fn granted_scopes(token: &StoredToken) -> Vec<String> {
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
pub(super) enum StepUp {
    /// The token already covers every required scope.
    Covered,
    /// Re-authenticate to acquire the missing scopes. The caller builds the
    /// requested set (defaults ∪ granted ∪ required) only on this path, so
    /// resolving per-env default scopes stays off the cached-token hot path.
    Reauthenticate,
}

/// Decides the step-up action from what the token grants versus what the command
/// requires. Deliberately does NOT take the per-env default scopes: the coverage
/// decision needs only `granted`/`required`, so the caller can avoid resolving
/// defaults (potential `environments.toml` I/O) when a cached token already
/// covers the requirement.
pub(super) fn plan_step_up(
    granted: &[String],
    required: &[String],
    hierarchy: &ScopeHierarchy,
) -> StepUp {
    let covered = required
        .iter()
        .all(|scope| hierarchy.covers(granted, scope.as_str()));
    if covered {
        StepUp::Covered
    } else {
        StepUp::Reauthenticate
    }
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
pub(super) fn ensure_granted(
    env: &str,
    token: &StoredToken,
    required: &[String],
    hierarchy: &ScopeHierarchy,
) -> Result<()> {
    let granted = granted_scopes(token);
    let missing: Vec<String> = required
        .iter()
        .filter(|scope| !hierarchy.covers(&granted, scope.as_str()))
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

/// Returns the first claim value that is a non-empty string, in priority order.
pub(super) fn extract_identity(claims: &Map<String, Value>, priority: &[String]) -> String {
    priority
        .iter()
        .filter_map(|name| claims.get(name).and_then(Value::as_str))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

pub(super) async fn parse_token_response(
    response: reqwest::Response,
    requested_scopes: &[String],
) -> Result<StoredToken> {
    let body: TokenResponse = response
        .json()
        .await
        .map_err(|err| CliCoreError::message(format!("failed to parse token response: {err}")))?;
    let expires_in = body.expires_in.unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;
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
