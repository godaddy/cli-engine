//! OpenID Connect discovery: resolving a provider's authorization, token and key-set
//! endpoints from its published `.well-known/openid-configuration` document.
//!
//! The document is fetched over TLS from the issuer, but nothing in it vouches for
//! where it points: a poisoned or misconfigured document could send authorization
//! requests, code exchanges, refresh tokens or the key-set fetch to another host.
//! So every endpoint is checked against an allowed host set before it is used, and
//! the document must name the issuer it was fetched for (OpenID Connect Discovery
//! 1.0 section 4.3).

use std::{
    collections::BTreeSet,
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::Deserialize;
use tokio::sync::{Mutex, OnceCell};

use super::http_fetch;
use crate::{Result, error::CliCoreError};

/// Largest discovery document accepted. Real ones are a few kilobytes.
const DISCOVERY_MAX_BYTES: usize = 128 * 1024;

const WELL_KNOWN_PATH: &str = ".well-known/openid-configuration";

/// The endpoints a discovery document published, after they passed the checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcEndpoints {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

impl OidcEndpoints {
    /// The issuer the document was fetched for, as configured.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Where the browser is sent to authorize.
    #[must_use]
    pub fn authorization_endpoint(&self) -> &str {
        &self.authorization_endpoint
    }

    /// Where authorization codes and refresh tokens are exchanged.
    #[must_use]
    pub fn token_endpoint(&self) -> &str {
        &self.token_endpoint
    }

    /// Where the issuer's signing keys are published.
    #[must_use]
    pub fn jwks_uri(&self) -> &str {
        &self.jwks_uri
    }
}

/// What the document is parsed into. Every field is optional here so that a missing
/// one can be reported by name instead of as an opaque parse failure.
#[derive(Debug, Deserialize)]
struct Document {
    issuer: Option<String>,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    jwks_uri: Option<String>,
}

/// Resolves and remembers one issuer's endpoints.
///
/// Share one [`OidcDiscovery`] (it is meant to be held in an [`Arc`](std::sync::Arc))
/// between [`PkceAuthProvider::with_oidc_discovery`](super::PkceAuthProvider::with_oidc_discovery)
/// and, with the `id-token-verify` feature, `JwksVerifier::from_discovery`, so the
/// document is fetched once per process however many things need it. A successful
/// fetch is kept for the life of the value; a failed one is not, so the next
/// caller tries again.
///
/// Every endpoint must be an `https` URL, with no port or credentials, on an allowed
/// host. The issuer's own host is always allowed; add others (some providers serve
/// their endpoints from a different domain) with
/// [`with_allowed_hosts`](Self::with_allowed_hosts). The document must also name the
/// issuer it was fetched for, exactly as configured. Redirects are never followed, so
/// a poisoned URL cannot send a fetch elsewhere.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use cli_engine::auth::pkce::{OidcDiscovery, PkceAuthProvider};
///
/// let discovery = Arc::new(OidcDiscovery::new("https://idp.example.com")?);
/// // The endpoints come from discovery, so the ones given here are not used.
/// let provider = PkceAuthProvider::new("idp", "", "", "my-client-id", &["openid"])
///     .with_oidc_discovery(discovery);
/// # let _ = provider;
/// # Ok::<(), cli_engine::CliCoreError>(())
/// ```
#[derive(Debug)]
pub struct OidcDiscovery {
    issuer: String,
    allowed_hosts: BTreeSet<String>,
    client: reqwest::Client,
    endpoints: OnceCell<OidcEndpoints>,
    /// Serializes fetch attempts, so only one runs at a time.
    attempt_lock: Mutex<()>,
    /// How many attempts have failed. A caller that was waiting for the lock while
    /// this changed was waiting on an attempt that has since failed, and shares its
    /// outcome (`last_failure`) instead of starting another.
    failed_attempts: AtomicU64,
    /// The message of the most recent failed attempt.
    last_failure: StdMutex<String>,
    /// Lets tests serve the endpoints from a plain-HTTP loopback server. Not a
    /// field outside tests, so nothing can switch the scheme check off.
    #[cfg(test)]
    loopback_http: bool,
}

impl OidcDiscovery {
    /// Creates discovery for `issuer`.
    ///
    /// # Errors
    ///
    /// Returns an error when `issuer` is not an `https` URL, or has a port,
    /// credentials, a query or a fragment.
    pub fn new(issuer: impl Into<String>) -> Result<Self> {
        let issuer = issuer.into();
        let host = issuer_host(&issuer).ok_or_else(|| {
            CliCoreError::message(
                "OIDC discovery: the issuer must be an https URL without a port, credentials, \
                 query or fragment",
            )
        })?;
        Self::build(issuer, host, false)
    }

    /// Like [`new`](Self::new) without the URL checks, accepting a plain-HTTP
    /// loopback issuer and endpoints, so tests can use a local server.
    #[cfg(test)]
    pub(super) fn new_loopback(issuer: impl Into<String>) -> Result<Self> {
        let issuer = issuer.into();
        let host = url::Url::parse(&issuer)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_default();
        Self::build(issuer, host, true)
    }

    // `loopback_http` is only read in tests; the parameter keeps one constructor.
    #[cfg_attr(not(test), allow(unused_variables))]
    fn build(issuer: String, host: String, loopback_http: bool) -> Result<Self> {
        let client = http_fetch::client().map_err(|err| {
            CliCoreError::message(format!(
                "OIDC discovery: could not build an HTTP client: {err}"
            ))
        })?;
        Ok(Self {
            issuer,
            allowed_hosts: BTreeSet::from([host]),
            client,
            endpoints: OnceCell::new(),
            attempt_lock: Mutex::new(()),
            failed_attempts: AtomicU64::new(0),
            last_failure: StdMutex::new(String::new()),
            #[cfg(test)]
            loopback_http,
        })
    }

    /// Also allows endpoints on `hosts`, in addition to the issuer's own host.
    ///
    /// Each must be a bare host name (letters, digits, `-` and `.`): no scheme, port,
    /// path, credentials or wildcard. Matching is exact, so `accounts.example.com`
    /// does not allow `login.accounts.example.com`.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first entry that is not a bare host name.
    pub fn with_allowed_hosts(mut self, hosts: &[impl AsRef<str>]) -> Result<Self> {
        for host in hosts {
            let host = host.as_ref();
            let normalized = bare_host(host).ok_or_else(|| {
                CliCoreError::message(format!(
                    "OIDC discovery: {host:?} is not a bare host name (no scheme, port, path, \
                     credentials or wildcard)"
                ))
            })?;
            self.allowed_hosts.insert(normalized);
        }
        Ok(self)
    }

    /// The issuer this discovery is for, as configured.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Resolves the endpoints, fetching the discovery document the first time.
    ///
    /// # Errors
    ///
    /// Returns an error when the document cannot be fetched or parsed, names a
    /// different issuer, lacks an endpoint, or has an endpoint that fails the checks
    /// described on [`OidcDiscovery`]. The message names the rule and never echoes
    /// the document.
    pub async fn endpoints(&self) -> Result<&OidcEndpoints> {
        if let Some(endpoints) = self.endpoints.get() {
            return Ok(endpoints);
        }
        // Read before waiting: if this changes while this caller queues for the
        // lock, an attempt it was waiting behind has failed.
        let seen = self.failed_attempts.load(Ordering::SeqCst);
        let _attempt = self.attempt_lock.lock().await;
        if let Some(endpoints) = self.endpoints.get() {
            return Ok(endpoints);
        }
        if self.failed_attempts.load(Ordering::SeqCst) != seen {
            // Share that attempt's failure. Retrying here would turn one outage
            // into a request, and a timeout, per waiting caller, one after another.
            // A caller arriving after the failure sees the new count and tries again.
            let message = self
                .last_failure
                .lock()
                .map(|failure| failure.clone())
                .unwrap_or_default();
            return Err(CliCoreError::message(message));
        }
        match self.fetch_and_check().await {
            Ok(endpoints) => {
                // Under the lock and checked empty above, so this cannot lose a race;
                // `get_or_init` would only be needed if it could.
                drop(self.endpoints.set(endpoints));
                self.endpoints.get().ok_or_else(|| {
                    CliCoreError::message("OIDC discovery: the endpoints were not retained")
                })
            }
            Err(err) => {
                if let Ok(mut failure) = self.last_failure.lock() {
                    *failure = err.to_string();
                }
                self.failed_attempts.fetch_add(1, Ordering::SeqCst);
                Err(err)
            }
        }
    }

    /// The URL of the discovery document: the issuer with
    /// `/.well-known/openid-configuration` appended, whatever path the issuer has.
    pub(super) fn discovery_url(&self) -> String {
        format!("{}/{WELL_KNOWN_PATH}", self.issuer.trim_end_matches('/'))
    }

    async fn fetch_and_check(&self) -> Result<OidcEndpoints> {
        let document: Document =
            http_fetch::get_json(&self.client, &self.discovery_url(), DISCOVERY_MAX_BYTES)
                .await
                .map_err(|err| {
                    CliCoreError::message(format!(
                        "OIDC discovery: {}",
                        err.describe("the discovery endpoint")
                    ))
                })?;
        self.check(document)
    }

    fn check(&self, document: Document) -> Result<OidcEndpoints> {
        let problem = |what: &str| CliCoreError::message(format!("OIDC discovery: {what}"));
        let issuer = document
            .issuer
            .ok_or_else(|| problem("the document has no issuer"))?;
        if issuer != self.issuer {
            return Err(problem(
                "the document's issuer does not match the configured issuer",
            ));
        }
        Ok(OidcEndpoints {
            issuer,
            authorization_endpoint: self.endpoint(
                document.authorization_endpoint,
                "authorization_endpoint",
                "authorization",
            )?,
            token_endpoint: self.endpoint(document.token_endpoint, "token_endpoint", "token")?,
            jwks_uri: self.endpoint(document.jwks_uri, "jwks_uri", "key-set")?,
        })
    }

    /// One endpoint from the document, if it is present and acceptable.
    fn endpoint(&self, value: Option<String>, field: &str, label: &str) -> Result<String> {
        let problem = |what: String| CliCoreError::message(format!("OIDC discovery: {what}"));
        let value = value.ok_or_else(|| problem(format!("the document has no {field}")))?;
        if endpoint_allowed(&value, &self.allowed_hosts, self.allows_loopback_http()) {
            Ok(value)
        } else {
            Err(problem(format!(
                "the {label} endpoint must be an https URL on an allowed host, without a port, \
                 credentials or fragment"
            )))
        }
    }

    fn allows_loopback_http(&self) -> bool {
        #[cfg(test)]
        {
            self.loopback_http
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

/// The host of `issuer` when it is `https` with no port, credentials, query or
/// fragment: what OpenID Connect allows an issuer identifier to be.
fn issuer_host(issuer: &str) -> Option<String> {
    let parsed = url::Url::parse(issuer).ok()?;
    let plain = parsed.scheme() == "https"
        && parsed.port().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    plain.then(|| parsed.host_str().map(str::to_ascii_lowercase))?
}

/// `host` as a lowercase bare host name, or `None` if it is anything else.
pub(super) fn bare_host(host: &str) -> Option<String> {
    let lower = host.trim().to_ascii_lowercase();
    let simple = !lower.is_empty()
        && lower
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    // Round-trips through the URL parser, which rewrites some spellings (`127.1`
    // becomes `127.0.0.1`); one that changes is not the host it looks like.
    let round_trips = url::Url::parse(&format!("https://{lower}/"))
        .ok()
        .is_some_and(|url| url.host_str() == Some(lower.as_str()));
    (simple && round_trips).then_some(lower)
}

/// Whether `url` may be used as an endpoint: `https`, no port, no credentials, no
/// fragment, on one of `allowed` (matched exactly, case-insensitively).
///
/// `loopback_http` additionally admits plain HTTP to `127.0.0.1` on any port, for
/// tests; production always passes `false`.
pub(super) fn endpoint_allowed(url: &str, allowed: &BTreeSet<String>, loopback_http: bool) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return false;
    }
    let secure = parsed.scheme() == "https" && parsed.port().is_none();
    let loopback =
        loopback_http && parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1");
    (secure || loopback) && parsed.host_str().is_some_and(|host| allowed.contains(host))
}
