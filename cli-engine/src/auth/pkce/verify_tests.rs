//! How [`PkceAuthProvider`] behaves around an [`IdTokenVerifier`]: what it trusts,
//! what it stores, and how it tells a rejected token from a check that could not
//! run. The verifier is a scripted fake, so none of this depends on real crypto.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Map, Value, json};

use super::bearer_tests::{id_token_expiring_at, token_with_expired_access, token_with_id_token};
use super::tests::{MemoryStorage, make_jwt, valid_token};
use super::{
    Bearer, IdTokenVerifier, PkceAuthProvider, StoredToken, VerifiedIdToken, VerifyError,
    test_provider,
};
use crate::CredentialRequest;
use crate::auth::AuthProvider;
use crate::auth::storage::{CredentialKey, CredentialStorage};
use crate::middleware::CommandMeta;

type Outcome = Result<Map<String, Value>, VerifyError>;

/// An [`IdTokenVerifier`] that answers from a script and records what it saw.
#[derive(Debug, Default)]
struct ScriptedVerifier {
    script: Mutex<VecDeque<Outcome>>,
    seen: Mutex<Vec<(String, String)>>,
}

impl ScriptedVerifier {
    fn new(script: Vec<Outcome>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            seen: Mutex::default(),
        })
    }

    fn accepting(email: &str) -> Arc<Self> {
        Self::new(vec![Ok(verified_claims(email))])
    }

    fn calls(&self) -> usize {
        self.seen.lock().expect("seen lock").len()
    }

    fn audiences(&self) -> Vec<String> {
        let seen = self.seen.lock().expect("seen lock");
        seen.iter().map(|(_, audience)| audience.clone()).collect()
    }
}

#[async_trait]
impl IdTokenVerifier for ScriptedVerifier {
    async fn verify(&self, id_token: &str, audience: &str) -> Result<VerifiedIdToken, VerifyError> {
        self.seen
            .lock()
            .expect("seen lock")
            .push((id_token.to_owned(), audience.to_owned()));
        let next = self.script.lock().expect("script lock").pop_front();
        next.unwrap_or_else(|| Err(VerifyError::Invalid("script exhausted".to_owned())))
            .map(VerifiedIdToken::new)
    }
}

fn verified_claims(email: &str) -> Map<String, Value> {
    match json!({"email": email, "sub": "verified-sub"}) {
        Value::Object(claims) => claims,
        _ => unreachable!("a JSON object literal"),
    }
}

fn rejected(reason: &str) -> Outcome {
    Err(VerifyError::Invalid(reason.to_owned()))
}

fn unavailable() -> Outcome {
    Err(VerifyError::Unavailable("keys unreachable".to_owned()))
}

fn id_token_expired() -> String {
    id_token_expiring_at(Utc::now().timestamp() - 60)
}

fn id_token_for_an_hour() -> String {
    id_token_expiring_at(Utc::now().timestamp() + 3600)
}

fn provider_with(verifier: &Arc<ScriptedVerifier>) -> (PkceAuthProvider, Arc<MemoryStorage>) {
    let storage = Arc::new(MemoryStorage::default());
    let provider = test_provider()
        .with_app_id("app")
        .with_storage(storage.clone())
        .with_id_token_verifier(verifier.clone());
    (provider, storage)
}

/// A token endpoint that answers one request, plus a provider using it.
fn provider_with_token_endpoint(
    verifier: &Arc<ScriptedVerifier>,
    response: &Value,
) -> (PkceAuthProvider, Arc<MemoryStorage>) {
    let base = super::bearer_tests::serve_token_json(response);
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
    .with_id_token_verifier(verifier.clone());
    (provider, storage)
}

fn stored_key() -> CredentialKey<'static> {
    CredentialKey::new("app", "test", "dev")
}

/// A stored token with a valid access token and a valid id_token.
fn good_stored_token() -> StoredToken {
    token_with_id_token(Some(id_token_for_an_hour()))
}

#[test]
fn identity_comes_from_verified_claims_not_from_the_raw_tokens() {
    let verifier = ScriptedVerifier::new(vec![]);
    let (provider, _storage) = provider_with(&verifier);
    let mut token = valid_token(&make_jwt(&json!({"email": "forged-access@evil.test"})));
    token.id_token = Some(make_jwt(
        &json!({"email": "forged-id@evil.test", "sub": "forged"}),
    ));
    token.verified_claims = Some(verified_claims("real@example.com"));

    let credential = provider.build_credential("dev", &token);
    assert_eq!(credential.identity, "real@example.com");
    assert_eq!(credential.sub, "verified-sub");
}

#[test]
fn an_unverified_token_names_no_identity_when_a_verifier_is_configured() {
    let verifier = ScriptedVerifier::new(vec![]);
    let (provider, _storage) = provider_with(&verifier);
    let mut token = valid_token(&make_jwt(&json!({"email": "forged-access@evil.test"})));
    token.id_token = Some(make_jwt(
        &json!({"email": "forged-id@evil.test", "sub": "forged"}),
    ));

    let credential = provider.build_credential("dev", &token);
    assert_eq!(credential.identity, "");
    assert_eq!(credential.sub, "");
}

#[test]
fn without_a_verifier_identity_still_comes_from_the_raw_claims() {
    let mut token = valid_token("opaque");
    token.id_token = Some(make_jwt(&json!({"email": "id@example.com", "sub": "s"})));
    let credential = test_provider().build_credential("dev", &token);
    assert_eq!(credential.identity, "id@example.com");
}

#[tokio::test]
async fn a_stored_token_is_verified_before_it_is_reused_and_then_cached() {
    let verifier = ScriptedVerifier::accepting("real@example.com");
    let (provider, _storage) = provider_with(&verifier);
    let stored = good_stored_token();
    provider.save_stored("dev", &stored).await.expect("seed");

    let first = provider
        .existing_token("dev")
        .await
        .expect("no error")
        .expect("reused");
    assert!(first.verified_claims.is_some());
    assert_eq!(verifier.calls(), 1);
    assert_eq!(verifier.audiences(), vec!["client-id".to_owned()]);

    // Served from the in-process cache: the verifier is not asked again.
    let second = provider
        .existing_token("dev")
        .await
        .expect("no error")
        .expect("cached");
    assert!(second.verified_claims.is_some());
    assert_eq!(verifier.calls(), 1);
}

/// The access token outlives the id_token the identity was vouched for with.
fn access_valid_but_id_token_expired() -> StoredToken {
    token_with_id_token(Some(id_token_expired()))
}

#[tokio::test]
async fn a_cached_identity_does_not_outlive_its_id_token() {
    let verifier = ScriptedVerifier::new(vec![]);
    let (provider, _storage) = provider_with(&verifier);
    let mut token = access_valid_but_id_token_expired();
    token.verified_claims = Some(verified_claims("real@example.com"));
    provider.store_cached_token("dev", token).await;

    assert!(
        provider.cached_token("dev").await.is_none(),
        "a verified identity must not be served past its id_token's expiry"
    );
}

#[tokio::test]
async fn a_stored_token_with_an_expired_id_token_is_not_reused_even_if_the_verifier_would_accept() {
    // The verifier accepts everything; reuse must still be refused on expiry.
    let verifier = ScriptedVerifier::accepting("real@example.com");
    let (provider, _storage) = provider_with(&verifier);
    provider
        .save_stored("dev", &access_valid_but_id_token_expired())
        .await
        .expect("seed");

    let reused = provider.existing_token("dev").await.expect("no error");
    assert!(
        reused.is_none(),
        "an expired id_token needs a refresh or login"
    );
    assert_eq!(verifier.calls(), 0, "no point verifying an expired token");
}

#[tokio::test]
async fn without_a_verifier_an_expired_id_token_does_not_block_reuse() {
    // Unchanged behaviour: the access token alone decides when nothing relies on
    // the id_token's authenticity.
    let storage = Arc::new(MemoryStorage::default());
    let provider = test_provider()
        .with_app_id("app")
        .with_storage(storage.clone());
    provider
        .store_cached_token("dev", access_valid_but_id_token_expired())
        .await;
    assert!(provider.cached_token("dev").await.is_some());
}

#[tokio::test]
async fn a_login_whose_id_token_has_no_usable_expiry_stores_nothing() {
    let verifier = ScriptedVerifier::accepting("real@example.com");
    let (provider, storage) = provider_with(&verifier);
    let mut token = valid_token("access");
    token.id_token = Some(make_jwt(&json!({"sub": "no-exp"})));

    let err = provider
        .accept_login("dev", token)
        .await
        .expect_err("no exp");
    assert!(err.to_string().contains("`exp`"), "{err}");
    assert!(storage.load(&stored_key()).await.is_none());
}

#[tokio::test]
async fn a_refresh_returning_an_already_expired_id_token_is_discarded() {
    let verifier = ScriptedVerifier::accepting("real@example.com");
    let (provider, storage) = provider_with_token_endpoint(
        &verifier,
        &json!({"access_token": "new", "expires_in": 3600, "id_token": id_token_expired()}),
    );
    let stored = token_with_expired_access(Some(id_token_for_an_hour()), Some("refresh-1"));
    provider.save_stored("dev", &stored).await.expect("seed");
    let before = storage.load(&stored_key()).await;

    assert!(
        provider
            .existing_token("dev")
            .await
            .expect("no error")
            .is_none()
    );
    assert_eq!(storage.load(&stored_key()).await, before);
}

#[tokio::test]
async fn a_rejected_stored_token_is_not_reused_or_cached() {
    let verifier = ScriptedVerifier::new(vec![rejected("bad signature")]);
    let (provider, _storage) = provider_with(&verifier);
    provider
        .save_stored("dev", &good_stored_token())
        .await
        .expect("seed");

    let reused = provider.existing_token("dev").await.expect("no error");
    assert!(reused.is_none(), "a rejected token needs a new login");
    assert!(provider.cached_token("dev").await.is_none());
}

#[tokio::test]
async fn a_check_that_could_not_run_is_an_error_not_a_reason_to_log_in() {
    let verifier = ScriptedVerifier::new(vec![unavailable()]);
    let (provider, storage) = provider_with(&verifier);
    let stored = good_stored_token();
    provider.save_stored("dev", &stored).await.expect("seed");
    let before = storage.load(&stored_key()).await;

    let err = provider
        .existing_token("dev")
        .await
        .expect_err("verification could not run");
    assert!(err.to_string().contains("could not verify"), "{err}");
    assert_eq!(
        storage.load(&stored_key()).await,
        before,
        "the stored credential must be left alone"
    );
}

#[tokio::test]
async fn get_credential_does_not_start_a_login_when_verification_is_unavailable() {
    // Hold the redirect port so a login attempt would fail at once instead of
    // opening a browser and waiting for a callback.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = held.local_addr().expect("addr").port();
    let verifier = ScriptedVerifier::new(vec![unavailable()]);
    let (provider, _storage) = provider_with(&verifier);
    let provider = provider.with_redirect_port(port);
    provider
        .save_stored("dev", &good_stored_token())
        .await
        .expect("seed");

    let err = provider
        .get_credential("dev", "", "")
        .await
        .expect_err("verification could not run");
    assert!(err.to_string().contains("could not verify"), "{err}");
    drop(held);
}

#[tokio::test]
async fn a_stored_blob_cannot_vouch_for_itself() {
    let verifier = ScriptedVerifier::new(vec![]);
    let (provider, storage) = provider_with(&verifier);
    let mut blob: Value =
        serde_json::to_value(good_stored_token()).expect("a stored token is serializable");
    blob["verified_claims"] = json!({"email": "attacker@evil.test", "sub": "attacker"});
    storage
        .save(&stored_key(), &blob.to_string())
        .await
        .expect("seed");

    let loaded = provider.load_stored("dev").await.expect("loads");
    assert!(
        loaded.verified_claims.is_none(),
        "only a verifier in this process may set verified claims"
    );
    let credential = provider.build_credential("dev", &loaded);
    assert_eq!(credential.identity, "");
}

#[tokio::test]
async fn an_unverified_cache_entry_is_not_served() {
    let verifier = ScriptedVerifier::new(vec![]);
    let (provider, _storage) = provider_with(&verifier);
    provider
        .store_cached_token("dev", good_stored_token())
        .await;
    assert!(provider.cached_token("dev").await.is_none());
}

#[tokio::test]
async fn verified_claims_are_never_written_to_storage() {
    let verifier = ScriptedVerifier::new(vec![]);
    let (provider, storage) = provider_with(&verifier);
    let mut token = good_stored_token();
    token.verified_claims = Some(verified_claims("real@example.com"));
    provider.save_stored("dev", &token).await.expect("save");

    let raw = storage.load(&stored_key()).await.expect("stored");
    assert!(!raw.contains("verified_claims"), "{raw}");
    assert!(!raw.contains("real@example.com"), "{raw}");
}

#[tokio::test]
async fn a_refresh_without_an_id_token_does_not_reuse_the_old_one() {
    let verifier = ScriptedVerifier::new(vec![]);
    let (provider, storage) = provider_with_token_endpoint(
        &verifier,
        &json!({"access_token": "new-access", "expires_in": 3600}),
    );
    let stored = token_with_expired_access(Some(id_token_for_an_hour()), Some("refresh-1"));
    provider.save_stored("dev", &stored).await.expect("seed");
    let before = storage.load(&stored_key()).await;

    let reused = provider.existing_token("dev").await.expect("no error");
    assert!(reused.is_none(), "there is nothing verifiable to hand out");
    assert_eq!(
        verifier.calls(),
        0,
        "the old id_token must not be re-checked"
    );
    assert_eq!(storage.load(&stored_key()).await, before);
}

#[tokio::test]
async fn a_refresh_whose_id_token_is_rejected_is_discarded() {
    let verifier = ScriptedVerifier::new(vec![rejected("bad signature")]);
    let fresh = id_token_for_an_hour();
    let (provider, storage) = provider_with_token_endpoint(
        &verifier,
        &json!({"access_token": "new-access", "expires_in": 3600, "id_token": fresh}),
    );
    let stored = token_with_expired_access(Some(id_token_for_an_hour()), Some("refresh-1"));
    provider.save_stored("dev", &stored).await.expect("seed");
    let before = storage.load(&stored_key()).await;

    assert!(
        provider
            .existing_token("dev")
            .await
            .expect("no error")
            .is_none()
    );
    assert_eq!(storage.load(&stored_key()).await, before);
    assert!(provider.cached_token("dev").await.is_none());
}

#[tokio::test]
async fn a_refresh_whose_id_token_verifies_is_stored_and_used() {
    let verifier = ScriptedVerifier::accepting("real@example.com");
    let fresh = id_token_for_an_hour();
    let (provider, _storage) = provider_with_token_endpoint(
        &verifier,
        &json!({"access_token": "new-access", "expires_in": 3600, "id_token": fresh}),
    );
    let stored = token_with_expired_access(Some(id_token_for_an_hour()), Some("refresh-1"));
    provider.save_stored("dev", &stored).await.expect("seed");

    let token = provider
        .existing_token("dev")
        .await
        .expect("no error")
        .expect("refreshed");
    assert_eq!(token.id_token.as_deref(), Some(fresh.as_str()));
    assert_eq!(
        provider.build_credential("dev", &token).identity,
        "real@example.com"
    );
}

#[tokio::test]
async fn a_rejected_login_stores_nothing() {
    let verifier = ScriptedVerifier::new(vec![rejected("wrong audience")]);
    let (provider, storage) = provider_with(&verifier);

    let err = provider
        .accept_login("dev", good_stored_token())
        .await
        .expect_err("rejected");
    assert!(err.to_string().contains("nothing was stored"), "{err}");
    assert!(err.to_string().contains("wrong audience"), "{err}");
    assert!(storage.load(&stored_key()).await.is_none());
    assert!(provider.cached_token("dev").await.is_none());
}

#[tokio::test]
async fn a_login_whose_check_could_not_run_stores_nothing() {
    let verifier = ScriptedVerifier::new(vec![unavailable()]);
    let (provider, storage) = provider_with(&verifier);

    let err = provider
        .accept_login("dev", good_stored_token())
        .await
        .expect_err("unavailable");
    assert!(err.to_string().contains("could not verify"), "{err}");
    assert!(storage.load(&stored_key()).await.is_none());
}

#[tokio::test]
async fn a_login_without_an_id_token_is_rejected_when_a_verifier_is_configured() {
    let verifier = ScriptedVerifier::new(vec![]);
    let (provider, storage) = provider_with(&verifier);

    let err = provider
        .accept_login("dev", token_with_id_token(None))
        .await
        .expect_err("no id_token");
    assert!(err.to_string().contains("openid"), "{err}");
    assert_eq!(verifier.calls(), 0);
    assert!(storage.load(&stored_key()).await.is_none());
}

#[tokio::test]
async fn an_accepted_login_is_stored_and_carries_verified_identity() {
    let verifier = ScriptedVerifier::accepting("real@example.com");
    let (provider, storage) = provider_with(&verifier);

    let token = provider
        .accept_login("dev", good_stored_token())
        .await
        .expect("accepted");
    assert_eq!(
        provider.build_credential("dev", &token).identity,
        "real@example.com"
    );
    assert!(storage.load(&stored_key()).await.is_some());
    assert!(provider.cached_token("dev").await.is_some());
}

#[tokio::test]
async fn without_a_verifier_a_login_is_accepted_as_before() {
    let storage = Arc::new(MemoryStorage::default());
    let provider = test_provider()
        .with_app_id("app")
        .with_storage(storage.clone());

    let token = provider
        .accept_login("dev", token_with_id_token(None))
        .await
        .expect("no verifier, no extra requirements");
    assert!(token.verified_claims.is_none());
    assert!(storage.load(&stored_key()).await.is_some());
}

#[tokio::test]
async fn status_reports_a_rejected_stored_token_as_an_error() {
    let verifier = ScriptedVerifier::new(vec![rejected("the token has expired")]);
    let (provider, _storage) = provider_with(&verifier);
    provider
        .save_stored("dev", &good_stored_token())
        .await
        .expect("seed");

    let err = provider.status("dev").await.expect_err("rejected");
    let message = err.to_string();
    assert!(message.contains("failed verification"), "{message}");
    assert!(message.contains("log in again"), "{message}");
}

#[tokio::test]
async fn status_reports_verified_identity() {
    let verifier = ScriptedVerifier::accepting("real@example.com");
    let (provider, _storage) = provider_with(&verifier);
    let mut stored = good_stored_token();
    stored.id_token = Some(make_jwt(&json!({
        "exp": Utc::now().timestamp() + 3600,
        "email": "forged@evil.test",
    })));
    provider.save_stored("dev", &stored).await.expect("seed");

    let credential = provider.status("dev").await.expect("verified");
    assert_eq!(credential.identity, "real@example.com");
}

#[tokio::test]
async fn id_token_bearer_with_a_verifier_serves_the_verified_token() {
    let verifier = ScriptedVerifier::accepting("real@example.com");
    let (provider, _storage) = provider_with(&verifier);
    let provider = provider.with_bearer(Bearer::IdToken);
    let id_token = id_token_for_an_hour();
    provider
        .save_stored("dev", &token_with_id_token(Some(id_token.clone())))
        .await
        .expect("seed");

    let meta = CommandMeta::default();
    let request = CredentialRequest::new("dev", "app:list", "read", &meta);
    let credential = provider.get_credential_for(&request).await.expect("cred");
    assert_eq!(credential.token, id_token);
    assert_eq!(credential.identity, "real@example.com");
}

#[test]
fn verified_id_token_debug_lists_claim_names_not_values() {
    let verified = VerifiedIdToken::new(verified_claims("secret@example.com"));
    let shown = format!("{verified:?}");
    assert!(shown.contains("email"), "{shown}");
    assert!(!shown.contains("secret@example.com"), "{shown}");
}

#[test]
fn stored_token_debug_never_prints_verified_values() {
    let mut token = good_stored_token();
    token.verified_claims = Some(verified_claims("secret@example.com"));
    let shown = format!("{token:?}");
    assert!(!shown.contains("secret@example.com"), "{shown}");
}
