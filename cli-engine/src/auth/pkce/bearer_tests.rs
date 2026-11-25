//! Tests for [`Bearer`]: which stored token becomes the credential, and how
//! expiry and refresh follow that choice.

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    thread,
    time::Duration,
};

use chrono::Utc;
use serde_json::{Value, json};

use super::tests::{MemoryStorage, make_jwt, valid_token};
use super::{Bearer, PkceAuthProvider, StoredToken, test_provider};
use crate::CredentialRequest;
use crate::auth::AuthProvider;
use crate::middleware::CommandMeta;

/// 2100-01-01T00:00:00Z.
const FAR_FUTURE_EXP: i64 = 4_102_444_800;

pub(super) fn id_token_expiring_at(exp: i64) -> String {
    make_jwt(&json!({"exp": exp, "email": "id@example.com", "sub": "subject-1"}))
}

fn id_token_valid_for_an_hour() -> String {
    id_token_expiring_at(Utc::now().timestamp() + 3600)
}

fn id_token_expired() -> String {
    id_token_expiring_at(Utc::now().timestamp() - 3600)
}

/// A stored token whose access token is fine for an hour and whose id_token is
/// `id_token`.
pub(super) fn token_with_id_token(id_token: Option<String>) -> StoredToken {
    let mut token = valid_token("access-token");
    token.id_token = id_token;
    token
}

/// A stored token whose access token has already expired.
pub(super) fn token_with_expired_access(
    id_token: Option<String>,
    refresh_token: Option<&str>,
) -> StoredToken {
    let mut token = token_with_id_token(id_token);
    token.expires_at = Utc::now().timestamp() - 3600;
    token.refresh_token = refresh_token.map(str::to_owned);
    token
}

fn id_token_provider() -> PkceAuthProvider {
    test_provider().with_bearer(Bearer::IdToken)
}

/// Answers the next request with `body` as JSON and returns the server's base URL.
pub(super) fn serve_token_json(body: &Value) -> String {
    const HEAD_END: &[u8] = b"\r\n\r\n";
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base = format!("http://{}/", listener.local_addr().expect("local addr"));
    let body = body.to_string();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut received = Vec::new();
        let mut buf = [0_u8; 4096];
        // Read the whole request, so closing the socket never resets the client
        // while it is still sending the form body.
        loop {
            let read = stream.read(&mut buf).expect("read request");
            assert!(read > 0, "connection closed before the request ended");
            received.extend_from_slice(&buf[..read]);
            let Some(head_end) = received.windows(4).position(|w| w == HEAD_END) else {
                continue;
            };
            let head = String::from_utf8_lossy(&received[..head_end]).to_ascii_lowercase();
            let wanted = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if received.len() >= head_end + HEAD_END.len() + wanted {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).expect("respond");
    });
    base
}

/// A provider whose token endpoint is a one-shot loopback server answering
/// `refresh_response`, backed by in-memory storage seeded with `stored`.
async fn provider_with_refresh_response(
    bearer: Bearer,
    refresh_response: &Value,
    stored: &StoredToken,
) -> (PkceAuthProvider, Arc<MemoryStorage>) {
    let base = serve_token_json(refresh_response);
    let storage = Arc::new(MemoryStorage::default());
    let provider = PkceAuthProvider::new(
        "test",
        "https://example.com/auth",
        format!("{base}token"),
        "client-id",
        &["openid"],
    )
    .with_app_id("app")
    .with_storage(storage.clone())
    .with_bearer(bearer);
    provider
        .save_stored("dev", stored)
        .await
        .expect("seed stored token");
    (provider, storage)
}

#[test]
fn default_bearer_is_the_access_token() {
    assert_eq!(Bearer::default(), Bearer::AccessToken);
    assert_eq!(test_provider().bearer, Bearer::AccessToken);
}

#[test]
fn default_bearer_credential_keeps_the_access_token_and_its_expiry() {
    let token = token_with_id_token(Some(id_token_valid_for_an_hour()));
    let credential = test_provider()
        .bearer_credential("prod", &token)
        .expect("credential");
    assert_eq!(credential.token, "access-token");
    assert_eq!(credential.expires_at, super::rfc3339_secs(token.expires_at));
}

#[test]
fn id_token_bearer_credential_uses_the_id_token_and_its_exp() {
    let id_token = id_token_expiring_at(FAR_FUTURE_EXP);
    let token = token_with_id_token(Some(id_token.clone()));
    let credential = id_token_provider()
        .bearer_credential("prod", &token)
        .expect("credential");
    assert_eq!(credential.token, id_token);
    assert_eq!(credential.expires_at, "2100-01-01T00:00:00Z");
    // Identity still comes from the merged claims, as with the access token.
    assert_eq!(credential.identity, "id@example.com");
    assert_eq!(credential.sub, "subject-1");
}

#[test]
fn id_token_bearer_credential_rounds_a_fractional_exp_down() {
    let id_token = make_jwt(&json!({"exp": 4_102_444_800.9_f64}));
    let token = token_with_id_token(Some(id_token));
    let credential = id_token_provider()
        .bearer_credential("prod", &token)
        .expect("credential");
    assert_eq!(credential.expires_at, "2100-01-01T00:00:00Z");
}

#[test]
fn id_token_bearer_credential_fails_without_a_usable_id_token() {
    let provider = id_token_provider();
    let unusable = [
        None,
        Some("not-a-jwt".to_owned()),
        Some(make_jwt(&json!({"email": "no-exp@example.com"}))),
        Some(make_jwt(&json!({"exp": "tomorrow"}))),
    ];
    for id_token in unusable {
        let token = token_with_id_token(id_token);
        let err = provider
            .bearer_credential("prod", &token)
            .expect_err("no usable id_token");
        let message = err.to_string();
        assert!(message.contains("no usable id_token"), "{message}");
        assert!(message.contains("openid"), "{message}");
    }
}

/// `exp` values chrono cannot represent. Parsed from text so the `f64` cases
/// keep their exact magnitude.
const OUT_OF_RANGE_EXP: [&str; 7] = [
    "9223372036854775807",  // i64::MAX
    "-9223372036854775808", // i64::MIN
    "1000000000000000000",
    "-1000000000000000000",
    "1e100",
    "-1e100",
    "1.8446744073709552e19", // beyond i64::MAX as an f64
];

fn id_token_with_raw_exp(raw_exp: &str) -> String {
    let claims: Value =
        serde_json::from_str(&format!(r#"{{"exp": {raw_exp}, "sub": "subject-1"}}"#))
            .expect("claims parse");
    make_jwt(&claims)
}

#[test]
fn out_of_range_exp_is_rejected_instead_of_saturating() {
    let provider = id_token_provider();
    for raw_exp in OUT_OF_RANGE_EXP {
        let token = token_with_id_token(Some(id_token_with_raw_exp(raw_exp)));
        assert!(
            !token.is_valid_for(Bearer::IdToken),
            "exp {raw_exp} must not count as a valid (or panic) expiry"
        );
        let err = provider
            .bearer_credential("prod", &token)
            .expect_err("unrepresentable exp");
        assert!(
            err.to_string().contains("no usable id_token"),
            "exp {raw_exp}: {err}"
        );
        // The same check gates a fresh login before anything is stored.
        assert!(
            token.require_bearer(Bearer::IdToken, "dev").is_err(),
            "exp {raw_exp} must fail the post-login check"
        );
    }
}

#[test]
fn expiry_check_does_not_overflow_on_extreme_values() {
    // The access-token path has no range check of its own (the value comes from
    // the token response), so the shared expiry arithmetic must not overflow.
    for expires_at in [i64::MIN, i64::MIN + 1, i64::MAX] {
        let mut token = valid_token("access-token");
        token.expires_at = expires_at;
        let _ = token.is_valid_for(Bearer::AccessToken);
    }
}

#[test]
fn id_token_bearer_error_never_echoes_a_token() {
    let id_token = make_jwt(&json!({"email": "secret@example.com"}));
    let token = token_with_id_token(Some(id_token.clone()));
    let message = id_token_provider()
        .bearer_credential("prod", &token)
        .expect_err("no exp")
        .to_string();
    assert!(!message.contains(&id_token), "{message}");
    assert!(!message.contains("access-token"), "{message}");
}

#[test]
fn validity_follows_the_selected_token() {
    let fresh_id = token_with_expired_access(Some(id_token_valid_for_an_hour()), None);
    assert!(fresh_id.is_valid_for(Bearer::IdToken));
    assert!(!fresh_id.is_valid_for(Bearer::AccessToken));

    let stale_id = token_with_id_token(Some(id_token_expired()));
    assert!(stale_id.is_valid_for(Bearer::AccessToken));
    assert!(!stale_id.is_valid_for(Bearer::IdToken));

    assert!(!token_with_id_token(None).is_valid_for(Bearer::IdToken));
}

#[test]
fn id_token_inside_the_expiry_buffer_is_not_valid() {
    let almost_expired = id_token_expiring_at(Utc::now().timestamp() + 5);
    let token = token_with_id_token(Some(almost_expired));
    assert!(!token.is_valid_for(Bearer::IdToken));
}

#[tokio::test]
async fn expired_access_token_is_reused_while_the_id_token_is_valid() {
    // No refresh and no network: the id_token alone decides reuse.
    let provider = id_token_provider().with_storage(Arc::new(MemoryStorage::default()));
    let stored = token_with_expired_access(Some(id_token_valid_for_an_hour()), None);
    provider.save_stored("dev", &stored).await.expect("seed");

    let token = provider
        .existing_token("dev")
        .await
        .expect("no error")
        .expect("reused");
    assert_eq!(token.id_token, stored.id_token);
}

#[tokio::test]
async fn expired_id_token_without_a_refresh_token_needs_a_login() {
    let provider = id_token_provider().with_storage(Arc::new(MemoryStorage::default()));
    let stored = token_with_id_token(Some(id_token_expired()));
    provider.save_stored("dev", &stored).await.expect("seed");

    let reused = provider.existing_token("dev").await.expect("no error");
    assert!(
        reused.is_none(),
        "a valid access token must not stand in for an expired id_token"
    );
}

#[tokio::test]
async fn cached_token_with_an_expired_id_token_is_not_served() {
    let provider = id_token_provider();
    provider
        .store_cached_token("dev", token_with_id_token(Some(id_token_expired())))
        .await;
    assert!(provider.cached_token("dev").await.is_none());
}

#[tokio::test]
async fn refresh_without_an_id_token_never_carries_the_old_one_forward() {
    let stored = token_with_expired_access(Some(id_token_expired()), Some("refresh-1"));
    let (provider, _storage) = provider_with_refresh_response(
        Bearer::IdToken,
        &json!({"access_token": "new-access", "expires_in": 3600}),
        &stored,
    )
    .await;

    let reused = provider.existing_token("dev").await.expect("no error");
    assert!(reused.is_none(), "must fall back to a login");

    // The refresh result was discarded: the previous entry is untouched, and the
    // in-process cache holds nothing for this environment.
    let on_disk = provider.load_stored("dev").await.expect("still stored");
    assert_eq!(on_disk.access_token, "access-token");
    assert_eq!(on_disk.id_token, stored.id_token);
    assert!(provider.cached_token("dev").await.is_none());
}

#[tokio::test]
async fn refresh_with_a_fresh_id_token_is_used_and_stored() {
    let fresh = id_token_expiring_at(FAR_FUTURE_EXP);
    let stored = token_with_expired_access(Some(id_token_expired()), Some("refresh-1"));
    let (provider, _storage) = provider_with_refresh_response(
        Bearer::IdToken,
        &json!({"access_token": "new-access", "expires_in": 3600, "id_token": fresh}),
        &stored,
    )
    .await;

    let token = provider
        .existing_token("dev")
        .await
        .expect("no error")
        .expect("refreshed");
    assert_eq!(token.id_token.as_deref(), Some(fresh.as_str()));
    // The server sent no new refresh token, so the old one is kept for next time.
    assert_eq!(token.refresh_token.as_deref(), Some("refresh-1"));

    let credential = provider.status("dev").await.expect("status");
    assert_eq!(credential.token, fresh);
    assert_eq!(credential.expires_at, "2100-01-01T00:00:00Z");
}

#[tokio::test]
async fn access_token_bearer_still_carries_the_old_id_token_across_a_refresh() {
    let old_id_token = id_token_expired();
    let stored = token_with_expired_access(Some(old_id_token.clone()), Some("refresh-1"));
    let (provider, _storage) = provider_with_refresh_response(
        Bearer::AccessToken,
        &json!({"access_token": "new-access", "expires_in": 3600}),
        &stored,
    )
    .await;

    let token = provider
        .existing_token("dev")
        .await
        .expect("no error")
        .expect("refreshed");
    assert_eq!(token.access_token, "new-access");
    // Unchanged default behavior: identity claims keep coming from the earlier
    // id_token when a refresh response omits one.
    assert_eq!(token.id_token.as_deref(), Some(old_id_token.as_str()));
}

#[tokio::test]
async fn get_credential_for_returns_the_id_token_for_a_covered_cached_token() {
    let provider = id_token_provider();
    let id_token = id_token_expiring_at(FAR_FUTURE_EXP);
    provider
        .store_cached_token("dev", token_with_id_token(Some(id_token.clone())))
        .await;

    let meta = CommandMeta::default();
    let request = CredentialRequest::new("dev", "app:list", "read", &meta);
    let credential = provider.get_credential_for(&request).await.expect("cred");
    assert_eq!(credential.token, id_token);
}

#[tokio::test]
async fn status_reports_an_error_for_a_stored_token_with_no_id_token() {
    let provider = id_token_provider().with_storage(Arc::new(MemoryStorage::default()));
    provider
        .save_stored("dev", &token_with_id_token(None))
        .await
        .expect("seed");
    let err = provider.status("dev").await.expect_err("no id_token");
    assert!(err.to_string().contains("no usable id_token"), "{err}");
}
