//! The shared tail of the two OAuth token-endpoint calls (code exchange and
//! refresh): send, check the status, parse.

use super::{StoredToken, scopes::parse_token_response};
use crate::{Result, error::CliCoreError};

/// Builds the client used for token-endpoint requests.
///
/// Redirects are never followed. A token request carries the authorization code and
/// PKCE verifier, or the refresh token, and `reqwest` re-sends a 307 or 308 with its
/// body to wherever it points. The endpoint was validated (under discovery, against
/// an allowed host list), but a redirect target never was, so following one would
/// hand those secrets to a host that passed no check. A redirect is reported as the
/// failed status it is.
pub(super) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // Fails only when the TLS backend cannot initialize, which is also what
        // makes `Client::new()`, used here before, panic. There is no safe
        // fallback: a default client follows redirects.
        .expect("the TLS backend failed to initialize")
}

/// Which token-endpoint call is about to be made, and so which endpoints it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Needs {
    /// An interactive login: the authorization and the token endpoints.
    Login,
    /// A refresh: the token endpoint only.
    Refresh,
}

/// Wording for the two ways a token-endpoint call can fail, which differ between
/// the code exchange and the refresh.
#[derive(Clone, Copy)]
pub(super) struct Wording {
    /// Prefixes a transport failure, e.g. `token request failed`.
    pub(super) send_failed: &'static str,
    /// Names the endpoint in a non-success status, e.g. `token endpoint`.
    pub(super) endpoint: &'static str,
}

pub(super) const EXCHANGE: Wording = Wording {
    send_failed: "token request failed",
    endpoint: "token endpoint",
};

pub(super) const REFRESH: Wording = Wording {
    send_failed: "token refresh failed",
    endpoint: "refresh endpoint",
};

/// Sends `request` and parses the token response, recording `scopes` as the
/// granted scopes when the server does not echo them.
pub(super) async fn fetch_tokens(
    request: reqwest::RequestBuilder,
    wording: Wording,
    scopes: &[String],
) -> Result<StoredToken> {
    let response = request
        .send()
        .await
        .map_err(|err| CliCoreError::message(format!("{}: {err}", wording.send_failed)))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(CliCoreError::message(format!(
            "{} returned {status}: {body}",
            wording.endpoint
        )));
    }
    parse_token_response(response, scopes).await
}
