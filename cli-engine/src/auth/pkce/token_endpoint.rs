//! The shared tail of the two OAuth token-endpoint calls (code exchange and
//! refresh): send, check the status, parse.

use super::{StoredToken, scopes::parse_token_response};
use crate::{Result, error::CliCoreError};

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
