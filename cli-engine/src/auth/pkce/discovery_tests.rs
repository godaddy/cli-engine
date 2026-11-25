//! `OidcDiscovery` against a loopback server standing in for the identity provider.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{
    OidcDiscovery,
    discovery::{bare_host, endpoint_allowed},
    loopback::{LoopbackServer, Reply},
};

const WELL_KNOWN: &str = "/.well-known/openid-configuration";

/// The discovery document for a server at `base`, with `tweak` applied.
fn document(base: &str, tweak: impl FnOnce(&mut Value)) -> Reply {
    let mut doc = json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "jwks_uri": format!("{base}/keys"),
    });
    tweak(&mut doc);
    Reply::Body(doc.to_string())
}

/// A server whose discovery document is `tweak`ed, and a discovery pointed at it.
async fn setup(tweak: impl FnOnce(&mut Value)) -> (LoopbackServer, OidcDiscovery) {
    // The server's own address is needed to build its document, so start it with a
    // placeholder and replace the route once the address is known.
    let server = LoopbackServer::routes([(WELL_KNOWN, Reply::Status(500))]).await;
    let base = server.url("");
    server.set(WELL_KNOWN, document(&base, tweak));
    let discovery = OidcDiscovery::new_loopback(&base).expect("discovery");
    (server, discovery)
}

fn problem(result: Result<&super::OidcEndpoints, crate::CliCoreError>) -> String {
    result.expect_err("discovery should fail").to_string()
}

#[tokio::test]
async fn a_valid_document_yields_its_endpoints() {
    let (server, discovery) = setup(|_| {}).await;
    let endpoints = discovery.endpoints().await.expect("endpoints");
    assert_eq!(endpoints.issuer(), server.url(""));
    assert_eq!(endpoints.authorization_endpoint(), server.url("/authorize"));
    assert_eq!(endpoints.token_endpoint(), server.url("/token"));
    assert_eq!(endpoints.jwks_uri(), server.url("/keys"));
}

#[tokio::test]
async fn the_document_is_fetched_once_however_often_it_is_asked_for() {
    let (server, discovery) = setup(|_| {}).await;
    for _ in 0..4 {
        discovery.endpoints().await.expect("endpoints");
    }
    assert_eq!(server.hits_for(WELL_KNOWN), 1);
}

#[tokio::test]
async fn concurrent_callers_share_a_single_fetch() {
    let (server, discovery) = setup(|_| {}).await;
    let discovery = Arc::new(discovery);
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let discovery = discovery.clone();
            tokio::spawn(async move { discovery.endpoints().await.map(|_| ()).is_ok() })
        })
        .collect();
    for task in tasks {
        assert!(task.await.expect("task"));
    }
    assert_eq!(server.hits_for(WELL_KNOWN), 1);
}

#[tokio::test]
async fn a_failed_fetch_is_not_remembered() {
    let server = LoopbackServer::routes([(WELL_KNOWN, Reply::Status(503))]).await;
    let base = server.url("");
    let discovery = OidcDiscovery::new_loopback(&base).expect("discovery");

    let first = problem(discovery.endpoints().await);
    assert!(first.contains("HTTP 503"), "{first}");

    // The provider recovers; the next caller gets the real document.
    server.set(WELL_KNOWN, document(&base, |_| {}));
    discovery.endpoints().await.expect("recovered");
    assert_eq!(server.hits_for(WELL_KNOWN), 2);
}

#[tokio::test]
async fn a_document_naming_another_issuer_is_rejected() {
    let (_server, discovery) =
        setup(|doc| doc["issuer"] = json!("https://someone-else.example.com")).await;
    let message = problem(discovery.endpoints().await);
    assert!(message.contains("issuer does not match"), "{message}");
    assert!(!message.contains("someone-else"), "{message}");
}

#[tokio::test]
async fn a_document_without_an_issuer_is_rejected() {
    let (_server, discovery) = setup(|doc| {
        doc.as_object_mut().expect("object").remove("issuer");
    })
    .await;
    assert!(problem(discovery.endpoints().await).contains("no issuer"));
}

#[tokio::test]
async fn a_document_missing_an_endpoint_is_rejected_by_name() {
    for field in ["authorization_endpoint", "token_endpoint", "jwks_uri"] {
        let (_server, discovery) = setup(|doc| {
            doc.as_object_mut().expect("object").remove(field);
        })
        .await;
        let message = problem(discovery.endpoints().await);
        assert!(message.contains(field), "{field}: {message}");
    }
}

#[tokio::test]
async fn an_endpoint_that_is_not_a_string_is_rejected() {
    for bad in [json!(42), json!(null), json!(["https://x"]), json!({})] {
        let (_server, discovery) = setup(|doc| doc["token_endpoint"] = bad.clone()).await;
        assert!(discovery.endpoints().await.is_err(), "{bad}");
    }
}

#[tokio::test]
async fn an_endpoint_on_another_host_is_rejected_whichever_endpoint_it_is() {
    for field in ["authorization_endpoint", "token_endpoint", "jwks_uri"] {
        let (_server, discovery) =
            setup(|doc| doc[field] = json!("https://evil.example.net/x")).await;
        let message = problem(discovery.endpoints().await);
        assert!(message.contains("allowed host"), "{field}: {message}");
        assert!(!message.contains("evil.example.net"), "{field}: {message}");
    }
}

/// Every reason an endpoint can be refused is named in the message, so an operator
/// with a rejected document can tell what to fix.
#[tokio::test]
async fn the_rejection_message_names_every_rule_an_endpoint_can_fail() {
    let (_server, discovery) =
        setup(|doc| doc["token_endpoint"] = json!("https://idp.example.com/token#frag")).await;
    let message = problem(discovery.endpoints().await);
    for rule in ["https", "allowed host", "port", "credentials", "fragment"] {
        assert!(
            message.contains(rule),
            "message should name {rule:?}: {message}"
        );
    }
    assert!(!message.contains("#frag"), "{message}");
}

#[tokio::test]
async fn an_unparseable_or_oversized_document_is_an_error_that_never_echoes_it() {
    let server =
        LoopbackServer::routes([(WELL_KNOWN, Reply::Body("not json at all".into()))]).await;
    let discovery = OidcDiscovery::new_loopback(server.url("")).expect("discovery");
    let message = problem(discovery.endpoints().await);
    assert!(message.contains("could not be parsed"), "{message}");
    assert!(!message.contains("not json at all"), "{message}");

    let huge = LoopbackServer::routes([(
        WELL_KNOWN,
        Reply::Unbounded {
            bytes: 64 * 1024 * 1024,
        },
    )])
    .await;
    let discovery = OidcDiscovery::new_loopback(huge.url("")).expect("discovery");
    let message = problem(discovery.endpoints().await);
    assert!(message.contains("too large"), "{message}");
    assert!(
        huge.body_bytes_sent() < 8 * 1024 * 1024,
        "reading must stop near the limit: {} bytes sent",
        huge.body_bytes_sent()
    );
}

#[tokio::test]
async fn an_unreachable_discovery_endpoint_is_an_error() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", closed.local_addr().expect("addr"));
    drop(closed);
    let discovery = OidcDiscovery::new_loopback(base).expect("discovery");
    assert!(problem(discovery.endpoints().await).contains("could not be reached"));
}

#[tokio::test]
async fn a_redirect_from_the_discovery_endpoint_is_not_followed() {
    let elsewhere = LoopbackServer::routes([(WELL_KNOWN, Reply::Body("{}".into()))]).await;
    let server =
        LoopbackServer::routes([(WELL_KNOWN, Reply::Redirect(elsewhere.url(WELL_KNOWN)))]).await;
    let discovery = OidcDiscovery::new_loopback(server.url("")).expect("discovery");
    let message = problem(discovery.endpoints().await);
    assert!(message.contains("HTTP 302"), "{message}");
    assert_eq!(
        elsewhere.hits(),
        0,
        "the redirect target must never be asked"
    );
}

#[tokio::test]
// The guard serializes the process-wide identity across the awaits that follow.
#[allow(clippy::await_holding_lock)]
async fn the_request_carries_the_current_user_agent_and_no_attribution_headers() {
    let _guard = crate::transport::client::UA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _restore = crate::transport::client::RestoreDefaultUserAgent;

    let (server, discovery) = setup(|_| {}).await;
    crate::transport::set_default_user_agent("published-later/9.9");
    crate::transport::client::set_default_headers(
        [("x-client-session-id".to_owned(), "abc123session".to_owned())].into(),
    );
    discovery.endpoints().await.expect("endpoints");

    let head = server.last_request_headers();
    assert!(head.contains("user-agent: published-later/9.9"), "{head}");
    assert!(!head.contains("abc123session"), "{head}");
}

#[test]
fn the_discovery_url_is_the_issuer_plus_the_well_known_path() {
    for (issuer, expected) in [
        (
            "https://idp.example.com",
            "https://idp.example.com/.well-known/openid-configuration",
        ),
        (
            "https://idp.example.com/",
            "https://idp.example.com/.well-known/openid-configuration",
        ),
        (
            "https://idp.example.com/oauth2/default",
            "https://idp.example.com/oauth2/default/.well-known/openid-configuration",
        ),
    ] {
        let discovery = OidcDiscovery::new(issuer).expect("discovery");
        assert_eq!(discovery.discovery_url(), expected, "{issuer}");
    }
}

#[test]
fn an_issuer_must_be_a_plain_https_url() {
    for ok in [
        "https://idp.example.com",
        "https://idp.example.com/oauth2/default",
        "https://IDP.example.com",
    ] {
        assert!(OidcDiscovery::new(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://idp.example.com",
        "https://idp.example.com:8443",
        "https://user:pw@idp.example.com",
        "https://user@idp.example.com",
        "https://idp.example.com?x=1",
        "https://idp.example.com#frag",
        "idp.example.com",
        "/relative",
        "",
    ] {
        let err = OidcDiscovery::new(bad).expect_err(bad).to_string();
        assert!(err.contains("https URL"), "{bad}: {err}");
        assert!(!err.contains("pw"), "{bad}: {err}");
    }
}

#[test]
fn allowed_hosts_must_be_bare_host_names() {
    let base = || OidcDiscovery::new("https://idp.example.com").expect("discovery");
    assert!(
        base()
            .with_allowed_hosts(&["accounts.example.com", "KEYS.example.net"])
            .is_ok()
    );
    for bad in [
        "",
        "https://accounts.example.com",
        "accounts.example.com:443",
        "accounts.example.com/path",
        "user@accounts.example.com",
        "*.example.com",
        "accounts example.com",
        "127.1",
    ] {
        assert!(base().with_allowed_hosts(&[bad]).is_err(), "{bad:?}");
    }
    assert_eq!(
        bare_host("  Accounts.Example.COM ").as_deref(),
        Some("accounts.example.com")
    );
}

#[test]
fn matching_is_exact_not_a_suffix_or_prefix() {
    let allowed = ["idp.example.com".to_owned()].into();
    for ok in [
        "https://idp.example.com/x",
        "https://IDP.example.com/x",
        "https://idp.example.com/a/b?c=d",
    ] {
        assert!(endpoint_allowed(ok, &allowed, false), "{ok}");
    }
    for bad in [
        "https://evilidp.example.com/x",
        "https://idp.example.com.evil.test/x",
        "https://sub.idp.example.com/x",
        "https://example.com/x",
        "https://idp.example.com@evil.test/x",
        "https://evil.test/https://idp.example.com/x",
        "http://idp.example.com/x",
        "https://idp.example.com:8443/x",
        "https://idp.example.com:0/x",
        "https://user:pw@idp.example.com/x",
        "https://idp.example.com/x#frag",
        "//idp.example.com/x",
        "not a url",
        "",
    ] {
        assert!(!endpoint_allowed(bad, &allowed, false), "{bad}");
    }
}

/// The URL parser drops a port that is the scheme's default, so `:443` on an `https`
/// endpoint is the same URL as one without it. It is accepted because it cannot
/// point the request anywhere else; every port that is not the default is not.
#[test]
fn an_explicit_default_port_is_the_same_endpoint_and_is_accepted() {
    let allowed = ["idp.example.com".to_owned()].into();
    assert!(endpoint_allowed(
        "https://idp.example.com:443/x",
        &allowed,
        false
    ));
}

#[test]
fn plain_http_is_never_accepted_outside_the_loopback_test_switch() {
    let allowed = ["127.0.0.1".to_owned()].into();
    assert!(!endpoint_allowed("http://127.0.0.1/x", &allowed, false));
    assert!(endpoint_allowed("http://127.0.0.1:1234/x", &allowed, true));
    // The switch admits loopback and nothing else.
    assert!(!endpoint_allowed("http://localhost/x", &allowed, true));
    assert!(!endpoint_allowed("http://evil.test/x", &allowed, true));
}

#[tokio::test]
async fn extra_allowed_hosts_admit_endpoints_the_issuer_host_would_not() {
    // A provider serving its endpoints from a second domain.
    let server = LoopbackServer::routes([(WELL_KNOWN, Reply::Status(500))]).await;
    let base = server.url("");
    server.set(
        WELL_KNOWN,
        document(&base, |doc| {
            doc["jwks_uri"] = json!("https://keys.example.net/keys")
        }),
    );

    let strict = OidcDiscovery::new_loopback(&base).expect("discovery");
    assert!(strict.endpoints().await.is_err(), "not allowed by default");

    let relaxed = OidcDiscovery::new_loopback(&base)
        .expect("discovery")
        .with_allowed_hosts(&["keys.example.net"])
        .expect("bare host");
    assert_eq!(
        relaxed.endpoints().await.expect("allowed").jwks_uri(),
        "https://keys.example.net/keys"
    );
}

// ---------------------------------------------------------------------------
// The provider's use of discovery
// ---------------------------------------------------------------------------

use super::{
    PkceAuthProvider,
    tests::{MemoryStorage, valid_token},
};
use crate::auth::{AuthProvider, storage::CredentialStorage};
use crate::environments::{EnvTable, Environments};

/// A provider whose endpoints come from `discovery`, with deliberately empty ones
/// passed to `new`, plus the storage behind it.
fn discovery_provider(discovery: OidcDiscovery) -> (PkceAuthProvider, Arc<MemoryStorage>) {
    let storage = Arc::new(MemoryStorage::default());
    let provider = PkceAuthProvider::new("test", "", "", "client-id", &["openid"])
        .with_app_id("app")
        .with_storage(storage.clone())
        .with_oidc_discovery(Arc::new(discovery));
    (provider, storage)
}

#[tokio::test]
async fn the_provider_takes_its_endpoints_from_the_discovery_document() {
    let (server, discovery) = setup(|_| {}).await;
    let (provider, _storage) = discovery_provider(discovery);

    let oauth = provider.resolved_oauth("dev").await.expect("resolved");
    assert_eq!(oauth.auth_url, server.url("/authorize"));
    assert_eq!(oauth.token_url, server.url("/token"));
    assert_eq!(oauth.client_id, "client-id");
    assert_eq!(server.hits_for(WELL_KNOWN), 1);
}

#[tokio::test]
async fn without_discovery_a_blank_endpoint_is_still_an_error() {
    let provider = PkceAuthProvider::new("test", "", "", "client-id", &["openid"]);
    let err = provider
        .effective_oauth("dev")
        .expect_err("no endpoint and no discovery");
    assert!(err.to_string().contains("auth_url"), "{err}");

    let blank_token_url =
        PkceAuthProvider::new("test", "https://x/auth", "", "client-id", &["openid"]);
    let err = blank_token_url
        .effective_oauth("dev")
        .expect_err("no token endpoint and no discovery");
    assert!(err.to_string().contains("token_url"), "{err}");
}

#[tokio::test]
async fn endpoints_given_to_new_are_ignored_under_discovery() {
    let (server, discovery) = setup(|_| {}).await;
    let storage = Arc::new(MemoryStorage::default());
    let provider = PkceAuthProvider::new(
        "test",
        "https://configured.example.com/auth",
        "https://configured.example.com/token",
        "client-id",
        &["openid"],
    )
    .with_storage(storage)
    .with_oidc_discovery(Arc::new(discovery));

    let oauth = provider.resolved_oauth("dev").await.expect("resolved");
    assert_eq!(oauth.auth_url, server.url("/authorize"));
    assert_eq!(oauth.token_url, server.url("/token"));
}

#[tokio::test]
async fn an_endpoint_set_for_the_environment_wins_over_discovery() {
    let (server, discovery) = setup(|_| {}).await;
    let environments = Arc::new(Environments::new("dev").with_environment(
        "dev",
        EnvTable::new().with("token_url", "https://override.example.com/token"),
    ));
    let provider = PkceAuthProvider::new("test", "", "", "client-id", &["openid"])
        .with_environments(environments)
        .with_oidc_discovery(Arc::new(discovery));

    let oauth = provider.resolved_oauth("dev").await.expect("resolved");
    assert_eq!(oauth.token_url, "https://override.example.com/token");
    assert_eq!(oauth.auth_url, server.url("/authorize"));
}

#[tokio::test]
async fn nothing_is_fetched_when_both_endpoints_are_set_for_the_environment() {
    let (server, discovery) = setup(|_| {}).await;
    let environments = Arc::new(
        Environments::new("dev").with_environment(
            "dev",
            EnvTable::new()
                .with("auth_url", "https://override.example.com/auth")
                .with("token_url", "https://override.example.com/token"),
        ),
    );
    let provider = PkceAuthProvider::new("test", "", "", "client-id", &["openid"])
        .with_environments(environments)
        .with_oidc_discovery(Arc::new(discovery));

    provider.resolved_oauth("dev").await.expect("resolved");
    assert_eq!(server.hits(), 0, "nothing needed the document");
}

#[tokio::test]
async fn reusing_a_valid_stored_credential_never_fetches_the_document() {
    let (server, discovery) = setup(|_| {}).await;
    let (provider, _storage) = discovery_provider(discovery);
    provider
        .save_stored("dev", &valid_token("stored-access"))
        .await
        .expect("seed");

    let credential = provider
        .get_credential("dev", "", "")
        .await
        .expect("reused");
    assert_eq!(credential.token, "stored-access");
    assert_eq!(server.hits(), 0, "a valid credential needs no endpoint");
}

#[tokio::test]
async fn status_and_logout_never_fetch_the_document() {
    let (server, discovery) = setup(|_| {}).await;
    let (provider, _storage) = discovery_provider(discovery);
    provider
        .save_stored("dev", &valid_token("stored-access"))
        .await
        .expect("seed");

    provider.status("dev").await.expect("status");
    provider.logout("dev").await.expect("logout");
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn a_refresh_uses_the_discovered_token_endpoint() {
    let server = LoopbackServer::routes([
        (WELL_KNOWN, Reply::Status(500)),
        (
            "/token",
            Reply::Body(json!({"access_token": "refreshed", "expires_in": 3600}).to_string()),
        ),
    ])
    .await;
    let base = server.url("");
    server.set(WELL_KNOWN, document(&base, |_| {}));
    let (provider, _storage) = discovery_provider(OidcDiscovery::new_loopback(&base).expect("d"));

    let mut stale = valid_token("old");
    stale.expires_at = chrono::Utc::now().timestamp() - 3600;
    stale.refresh_token = Some("refresh-1".to_owned());
    provider.save_stored("dev", &stale).await.expect("seed");

    let credential = provider
        .get_credential("dev", "", "")
        .await
        .expect("refreshed through the discovered endpoint");
    assert_eq!(credential.token, "refreshed");
    assert_eq!(server.hits_for("/token"), 1);
    assert_eq!(server.hits_for(WELL_KNOWN), 1);
}

#[tokio::test]
async fn a_failed_discovery_stops_a_login_before_anything_is_opened_or_stored() {
    // Hold the redirect port so a login that got past discovery would fail at the
    // bind and the test would notice; discovery must fail first.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = held.local_addr().expect("addr").port();
    let server = LoopbackServer::routes([(WELL_KNOWN, Reply::Status(503))]).await;
    let (provider, storage) =
        discovery_provider(OidcDiscovery::new_loopback(server.url("")).expect("discovery"));
    let provider = provider.with_redirect_port(port);

    let err = provider
        .get_credential("dev", "", "")
        .await
        .expect_err("discovery failed");
    let message = err.to_string();
    assert!(message.contains("OIDC discovery"), "{message}");
    assert!(
        !message.contains("failed to bind"),
        "must fail before binding the listener: {message}"
    );
    assert!(
        storage
            .load(&crate::auth::storage::CredentialKey::new(
                "app", "test", "dev"
            ))
            .await
            .is_none()
    );
    drop(held);
}

#[tokio::test]
async fn a_poisoned_discovery_document_never_reaches_an_authorization_request() {
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = held.local_addr().expect("addr").port();
    let (_server, discovery) =
        setup(|doc| doc["authorization_endpoint"] = json!("https://evil.example.net/auth")).await;
    let (provider, _storage) = discovery_provider(discovery);
    let provider = provider.with_redirect_port(port);

    let err = provider
        .get_credential("dev", "", "")
        .await
        .expect_err("the document names a foreign host");
    assert!(err.to_string().contains("allowed host"), "{err}");
    drop(held);
}

#[tokio::test]
async fn a_refresh_whose_discovery_fails_falls_back_to_a_login_without_leaking_the_token() {
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = held.local_addr().expect("addr").port();
    let server = LoopbackServer::routes([(WELL_KNOWN, Reply::Status(503))]).await;
    let (provider, _storage) =
        discovery_provider(OidcDiscovery::new_loopback(server.url("")).expect("discovery"));
    let provider = provider.with_redirect_port(port);

    let mut stale = valid_token("old");
    stale.expires_at = chrono::Utc::now().timestamp() - 3600;
    stale.refresh_token = Some("secret-refresh-token".to_owned());
    provider.save_stored("dev", &stale).await.expect("seed");

    let err = provider
        .get_credential("dev", "", "")
        .await
        .expect_err("cannot refresh or log in");
    assert!(!err.to_string().contains("secret-refresh-token"), "{err}");
    assert_eq!(
        server.hits_for("/token"),
        0,
        "the refresh token must not be sent anywhere"
    );
}

/// The statuses a redirect can have. 307 and 308 preserve the method and body, so a
/// client that follows them re-sends the secret; 302 is turned into a body-less GET
/// by `reqwest` but is still a request to a host that never passed discovery.
const REDIRECT_STATUSES: [u16; 3] = [302, 307, 308];

/// An allowed token endpoint that answers a refresh with a redirect to a host that
/// never passed discovery. A client that follows a 307 or 308 re-sends the POST,
/// body and all, so the refresh token would go to the redirect target.
#[tokio::test]
async fn a_refresh_is_never_forwarded_by_a_redirect_from_the_token_endpoint() {
    for status in REDIRECT_STATUSES {
        let elsewhere = LoopbackServer::routes([("/token", Reply::Status(500))]).await;
        let server = LoopbackServer::routes([
            (WELL_KNOWN, Reply::Status(500)),
            (
                "/token",
                Reply::RedirectWith {
                    status,
                    to: elsewhere.url("/token"),
                },
            ),
        ])
        .await;
        let base = server.url("");
        server.set(WELL_KNOWN, document(&base, |_| {}));
        let (provider, _storage) =
            discovery_provider(OidcDiscovery::new_loopback(&base).expect("d"));

        let mut stale = valid_token("old");
        stale.expires_at = chrono::Utc::now().timestamp() - 3600;
        stale.refresh_token = Some("secret-refresh-token".to_owned());
        provider.save_stored("dev", &stale).await.expect("seed");

        // The refresh cannot complete, so the provider falls back to a login, which
        // cannot start here (the redirect port is held); either way it must fail.
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let provider = provider.with_redirect_port(held.local_addr().expect("addr").port());
        provider
            .get_credential("dev", "", "")
            .await
            .expect_err("a redirected refresh must not succeed");

        assert_eq!(
            server.hits_for("/token"),
            1,
            "{status}: the endpoint was asked"
        );
        assert_eq!(
            elsewhere.hits(),
            0,
            "{status}: the redirect target must never receive the refresh token"
        );
        drop(held);
    }
}

#[tokio::test]
async fn a_code_exchange_is_never_forwarded_by_a_redirect_from_the_token_endpoint() {
    for status in REDIRECT_STATUSES {
        let elsewhere = LoopbackServer::routes([("/token", Reply::Status(500))]).await;
        let server = LoopbackServer::routes([(
            "/token",
            Reply::RedirectWith {
                status,
                to: elsewhere.url("/token"),
            },
        )])
        .await;
        let provider = PkceAuthProvider::new(
            "test",
            "https://example.com/auth",
            server.url("/token"),
            "client-id",
            &["openid"],
        );
        let oauth = provider.effective_oauth("dev").expect("oauth");

        provider
            .exchange_code_for_token(
                &oauth,
                "the-code",
                "the-verifier",
                "http://127.0.0.1/cb",
                &[],
            )
            .await
            .expect_err("a redirected exchange must not succeed");

        assert_eq!(server.hits_for("/token"), 1, "{status}");
        assert_eq!(
            elsewhere.hits(),
            0,
            "{status}: the redirect target must never receive the code or verifier"
        );
    }
}

/// With only the token endpoint overridden for the environment, a refresh needs
/// nothing from discovery, so a discovery outage must not stop it.
#[tokio::test]
async fn a_token_only_override_refreshes_without_consulting_discovery() {
    let token_server = LoopbackServer::routes([(
        "/token",
        Reply::Body(json!({"access_token": "refreshed", "expires_in": 3600}).to_string()),
    )])
    .await;
    let discovery_server = LoopbackServer::routes([(WELL_KNOWN, Reply::Status(503))]).await;
    let environments = Arc::new(Environments::new("dev").with_environment(
        "dev",
        EnvTable::new().with("token_url", token_server.url("/token")),
    ));
    let storage = Arc::new(MemoryStorage::default());
    let provider = PkceAuthProvider::new("test", "", "", "client-id", &["openid"])
        .with_app_id("app")
        .with_storage(storage)
        .with_environments(environments)
        .with_oidc_discovery(Arc::new(
            OidcDiscovery::new_loopback(discovery_server.url("")).expect("discovery"),
        ));

    let mut stale = valid_token("old");
    stale.expires_at = chrono::Utc::now().timestamp() - 3600;
    stale.refresh_token = Some("refresh-1".to_owned());
    provider.save_stored("dev", &stale).await.expect("seed");

    let credential = provider
        .get_credential("dev", "", "")
        .await
        .expect("refreshed without discovery");
    assert_eq!(credential.token, "refreshed");
    assert_eq!(
        discovery_server.hits(),
        0,
        "a refresh does not need the document"
    );
    assert_eq!(token_server.hits_for("/token"), 1);
}

/// Login does need the authorization endpoint, so the same override still consults
/// discovery for it.
#[tokio::test]
async fn a_token_only_override_still_discovers_the_authorization_endpoint_for_login() {
    let (server, discovery) = setup(|_| {}).await;
    let environments = Arc::new(Environments::new("dev").with_environment(
        "dev",
        EnvTable::new().with("token_url", "https://override.example.com/token"),
    ));
    let provider = PkceAuthProvider::new("test", "", "", "client-id", &["openid"])
        .with_environments(environments)
        .with_oidc_discovery(Arc::new(discovery));

    let oauth = provider.resolved_oauth("dev").await.expect("resolved");
    assert_eq!(oauth.auth_url, server.url("/authorize"));
    assert_eq!(oauth.token_url, "https://override.example.com/token");
}
