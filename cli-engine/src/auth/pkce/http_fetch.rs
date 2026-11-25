//! A bounded GET of a small JSON document from an identity provider, shared by
//! OIDC discovery and the JWKS verifier so both apply the same limits.

use std::time::Duration;

use serde::de::DeserializeOwned;

/// Total bound on one fetch. There is no separate command-level budget here: the
/// engine's `--timeout` cancels the whole command, and this keeps an unresponsive
/// endpoint from consuming all of it.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on establishing the connection, so an unroutable endpoint fails fast
/// instead of using up the whole fetch budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Builds the client used for these fetches. Redirects are never followed, so a
/// poisoned URL cannot send a request to another host.
///
/// It is a plain client: the user-agent is attached per request (see
/// [`get_json`]), as the provider's token requests do. The engine's shared builder
/// would snapshot the process identity at construction, and these clients are built
/// while the CLI is being assembled, before that identity is published for the run,
/// so it would hold a stale one. It would also carry attribution headers, including
/// a session-correlation hash, to an identity provider that has no use for them.
pub(super) fn client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(FETCH_TIMEOUT)
        .build()
}

/// Why a fetch did not produce a document. Carries no URL or body, since these
/// are shown to users.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FetchError {
    Unreachable,
    Status(u16),
    Unreadable,
    TooLarge,
    Unparseable,
}

impl FetchError {
    /// A sentence fragment about `what` (for example "the key endpoint").
    pub(super) fn describe(self, what: &str) -> String {
        match self {
            Self::Unreachable => format!("{what} could not be reached"),
            Self::Status(code) => format!("{what} answered HTTP {code}"),
            Self::Unreadable => format!("{what}'s response could not be read"),
            Self::TooLarge => format!("{what}'s response is too large"),
            Self::Unparseable => format!("{what}'s response could not be parsed"),
        }
    }
}

/// GETs `url` and parses its JSON body, reading no more than `max_bytes` of it.
///
/// The body is read in pieces and abandoned the moment it passes the limit, so
/// memory stays bounded whatever `content-length` claims, or whether it is sent.
pub(super) async fn get_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
) -> Result<T, FetchError> {
    let mut response = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            crate::transport::client::default_user_agent(),
        )
        .send()
        .await
        .map_err(|_| FetchError::Unreachable)?;
    if !response.status().is_success() {
        return Err(FetchError::Status(response.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| FetchError::Unreadable)? {
        if body.len() + chunk.len() > max_bytes {
            return Err(FetchError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| FetchError::Unparseable)
}
