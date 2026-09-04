use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde_json::{Value, json};

use super::callback_server::{extract_query_param, extract_request_path};
use super::scopes::{
    ScopeHierarchy, StepUp, decode_jwt_claims, ensure_granted, extract_identity, granted_scopes,
    plan_step_up, scopes_from_jwt, union_scopes,
};
use super::{DEFAULT_IDENTITY_CLAIMS, PkceAuthProvider, StoredToken, TOKEN_EXPIRY_BUFFER_SECS};
use crate::CredentialRequest;
use crate::auth::AuthProvider;
use crate::auth::storage::{CredentialKey, CredentialStorage};
use crate::config::CredentialStore;
use std::collections::HashMap;

fn test_provider() -> PkceAuthProvider {
    PkceAuthProvider::new(
        "test",
        "https://example.com/auth",
        "https://example.com/token",
        "client-id",
        &["openid"],
    )
}

fn valid_token(access_token: &str) -> StoredToken {
    StoredToken {
        access_token: access_token.to_owned(),
        expires_at: Utc::now().timestamp() + 3600,
        refresh_token: None,
        scopes: Vec::new(),
    }
}

fn token_with_scopes(access_token: &str, scopes: &[&str]) -> StoredToken {
    // No struct-update from `valid_token`: StoredToken is `Drop`
    // (ZeroizeOnDrop), so fields cannot be moved out of another instance.
    StoredToken {
        access_token: access_token.to_owned(),
        expires_at: Utc::now().timestamp() + 3600,
        refresh_token: None,
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn expired_token() -> StoredToken {
    StoredToken {
        access_token: "old-token".to_owned(),
        // Older than the expiry buffer so is_valid() returns false.
        expires_at: Utc::now().timestamp() - TOKEN_EXPIRY_BUFFER_SECS - 1,
        refresh_token: None,
        scopes: Vec::new(),
    }
}

fn envs_for_test() -> Arc<crate::environments::Environments> {
    use crate::environments::{EnvTable, Environments};
    Arc::new(
        Environments::new("prod").with_environment(
            "prod",
            EnvTable::new()
                .with("client_id", "prod-client")
                .with("auth_url", "https://prod.example.com/auth")
                .with("token_url", "https://prod.example.com/token")
                .with("scopes", vec!["openid", "prod.read"]),
        ),
    )
}

/// A provider wired to an [`Environments`](crate::environments::Environments)
/// resolver sources its per-env OAuth config (client id, endpoints, scopes)
/// from the resolved environment, making the environment the single source
/// of truth.
#[test]
fn environment_wired_provider_sources_oauth_from_resolver() {
    let provider = PkceAuthProvider::new(
        "godaddy",
        "https://base/auth",
        "https://base/token",
        "base-client",
        &["openid"],
    )
    .with_environments(envs_for_test());
    let oauth = provider.effective_oauth("prod").expect("assembles");
    assert_eq!(oauth.client_id, "prod-client");
    assert_eq!(oauth.auth_url, "https://prod.example.com/auth");
    assert_eq!(oauth.token_url, "https://prod.example.com/token");
    assert_eq!(
        oauth.scopes,
        vec!["openid".to_owned(), "prod.read".to_owned()]
    );
}

/// A resolved environment's `scopes = []` is treated as absent, not as a
/// deliberate "no scopes" override — it falls through to the provider's
/// real base scopes, the same way a blank `client_id`/`auth_url` string
/// would. An OAuth flow with zero scopes is never what a wired
/// environment actually means; it's the same kind of unset-placeholder
/// case blank-string collapsing already exists to catch.
#[test]
fn environment_with_empty_scopes_falls_back_to_base_scopes() {
    use crate::environments::{EnvTable, Environments};

    let environments = Arc::new(
        Environments::new("prod").with_environment(
            "prod",
            EnvTable::new()
                .with("client_id", "prod-client")
                .with("scopes", Vec::<String>::new()),
        ),
    );
    let provider = PkceAuthProvider::new(
        "godaddy",
        "https://base/auth",
        "https://base/token",
        "base-client",
        &["openid", "base.read"],
    )
    .with_environments(environments);

    let oauth = provider.effective_oauth("prod").expect("assembles");
    assert_eq!(
        oauth.client_id, "prod-client",
        "the environment's own value still wins"
    );
    assert_eq!(
        oauth.scopes,
        vec!["openid".to_owned(), "base.read".to_owned()],
        "an empty scopes array from the environment must defer to the base config's real scopes"
    );
}

/// A provider with no environment resolver falls back to the base client id,
/// endpoints, and scopes for every env.
#[test]
fn non_wired_provider_uses_base_config() {
    let provider = PkceAuthProvider::new(
        "godaddy",
        "https://base/auth",
        "https://base/token",
        "base-client",
        &["openid"],
    );
    let oauth = provider.effective_oauth("anything").expect("assembles");
    assert_eq!(oauth.client_id, "base-client");
    assert_eq!(oauth.scopes, vec!["openid".to_owned()]);
}

/// OAuth token traffic must carry the engine's configured default
/// user-agent so it is attributed consistently with all other outbound
/// calls (some upstream WAFs reject requests without a User-Agent).
#[test]
fn token_request_carries_default_user_agent() {
    let _guard = crate::transport::client::UA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _restore = crate::transport::client::RestoreDefaultUserAgent;
    crate::transport::set_default_user_agent("ua-probe/7.7");
    let provider = test_provider().with_token_timeout(Duration::from_secs(12));
    let request = provider
        .token_request(
            "https://example.com/token",
            &[("grant_type", "refresh_token")],
        )
        .build()
        .expect("token request should build");
    let header = request
        .headers()
        .get(reqwest::header::USER_AGENT)
        .expect("token request should set a user-agent");
    assert_eq!(header, "ua-probe/7.7");
    assert_eq!(request.timeout(), Some(&Duration::from_secs(12)));
}

/// OAuth token requests must not hang indefinitely: the provider applies a
/// 30s timeout by default.
#[test]
fn default_token_timeout_is_thirty_seconds() {
    assert_eq!(test_provider().token_timeout, Duration::from_secs(30));
}

/// The default token timeout can be overridden per provider.
#[test]
fn with_token_timeout_overrides_default() {
    let provider = test_provider().with_token_timeout(Duration::from_secs(5));
    assert_eq!(provider.token_timeout, Duration::from_secs(5));
}

/// store_cached_token + cached_token round-trip: the mechanism used by
/// the persistence fix must reliably write and read tokens from the cache.
#[tokio::test]
async fn cache_stores_and_retrieves_valid_token() {
    let provider = test_provider();
    let token = valid_token("access-abc");

    provider.store_cached_token("dev", token.clone()).await;

    let cached = provider.cached_token("dev").await;
    assert!(cached.is_some(), "expected cached token to be present");
    assert_eq!(
        cached.expect("token must be present").access_token,
        "access-abc"
    );
}

/// Expired tokens must not be returned from the cache; the caller would
/// then proceed to the keychain or PKCE flow.
#[tokio::test]
async fn cached_token_ignores_expired_tokens() {
    let provider = test_provider();
    provider.store_cached_token("dev", expired_token()).await;

    assert!(
        provider.cached_token("dev").await.is_none(),
        "expired token should not be returned from cache"
    );
}

#[test]
fn scopes_from_jwt_parses_scope_claim() {
    let token = make_jwt(&json!({ "scope": "a b c" }));
    assert_eq!(scopes_from_jwt(&token), vec!["a", "b", "c"]);
}

#[test]
fn scopes_from_jwt_parses_scp_and_array_claims() {
    // Azure-style `scp` array.
    let scp = make_jwt(&json!({ "scp": ["a", "b"] }));
    assert_eq!(scopes_from_jwt(&scp), vec!["a", "b"]);
    // `scope` encoded as an array.
    let array = make_jwt(&json!({ "scope": ["a", "b c"] }));
    assert_eq!(scopes_from_jwt(&array), vec!["a", "b", "c"]);
    // Empty `scope` falls through to `scp`.
    let mixed = make_jwt(&json!({ "scope": "", "scp": ["x"] }));
    assert_eq!(scopes_from_jwt(&mixed), vec!["x"]);
}

#[test]
fn granted_scopes_uses_recorded_scopes_for_opaque_token() {
    // An opaque (non-JWT) token carries no readable claim, so coverage comes
    // from the scopes recorded when it was obtained.
    let token = token_with_scopes("opaque-token", &["a", "b"]);
    assert_eq!(granted_scopes(&token), vec!["a", "b"]);
}

#[test]
fn ensure_granted_rejects_a_token_missing_required_scopes() {
    let required = vec!["a".to_owned(), "b".to_owned()];
    let hierarchy = ScopeHierarchy::new();
    // JWT that exposes only `a` → `b` is detectably not granted.
    let jwt = valid_token(&make_jwt(&json!({ "scope": "a" })));
    let err = ensure_granted("dev", &jwt, &required, &hierarchy).expect_err("b is not granted");
    assert!(
        err.to_string().contains("did not grant required scope(s)"),
        "{err}"
    );
    assert!(err.to_string().contains('b'), "{err}");

    // A token granting both passes.
    let ok = valid_token(&make_jwt(&json!({ "scope": "a b" })));
    ensure_granted("dev", &ok, &required, &hierarchy).expect("both granted");
    // Recorded scopes (opaque token) also satisfy the check.
    let opaque = token_with_scopes("opaque", &["a", "b"]);
    ensure_granted("dev", &opaque, &required, &hierarchy).expect("recorded scopes granted");
}

#[test]
fn ensure_granted_accepts_hierarchy_covered_grant() {
    // The IdP literally grants only `admin`; the hierarchy says that
    // covers `read`, so the exact-string-missing scope should not error.
    let required = vec!["read".to_owned()];
    let hierarchy = ScopeHierarchy::new().with_implication("admin", &["read"]);
    let jwt = valid_token(&make_jwt(&json!({ "scope": "admin" })));
    ensure_granted("dev", &jwt, &required, &hierarchy).expect("admin implies read");
}

#[test]
fn plan_step_up_covers_or_reauthenticates() {
    let granted = vec!["base".to_owned(), "read".to_owned()];
    let read = vec!["read".to_owned()];
    let write = vec!["write".to_owned()];
    let hierarchy = ScopeHierarchy::new();

    // Already covered (decision needs only granted vs required).
    assert_eq!(plan_step_up(&granted, &read, &hierarchy), StepUp::Covered);
    // Missing → reauthenticate, with no interactivity gate: step-up now
    // mirrors the no-token path and acquires the scope via a fresh login
    // rather than failing when stdio is not a TTY. The caller builds the
    // union (defaults ∪ granted ∪ required) only on this path.
    assert_eq!(
        plan_step_up(&granted, &write, &hierarchy),
        StepUp::Reauthenticate
    );
    // The union itself (defaults ∪ granted ∪ required) is covered by
    // union_scopes' own test.
}

#[test]
fn plan_step_up_covers_via_hierarchy() {
    let granted = vec!["admin".to_owned()];
    let read = vec!["read".to_owned()];
    let hierarchy = ScopeHierarchy::new().with_implication("admin", &["read"]);

    assert_eq!(plan_step_up(&granted, &read, &hierarchy), StepUp::Covered);
}

#[test]
fn scope_hierarchy_covers_transitively() {
    let hierarchy = ScopeHierarchy::new()
        .with_implication("a", &["b"])
        .with_implication("b", &["c"]);

    assert!(hierarchy.covers(&["a".to_owned()], "c"));
}

#[test]
fn scope_hierarchy_ignores_cycles() {
    let hierarchy = ScopeHierarchy::new()
        .with_implication("a", &["b"])
        .with_implication("b", &["a"]);

    // Terminates instead of looping, and still resolves correctly.
    assert!(hierarchy.covers(&["a".to_owned()], "b"));
    assert!(!hierarchy.covers(&["a".to_owned()], "z"));
}

#[test]
fn scope_hierarchy_defaults_to_exact_match() {
    let hierarchy = ScopeHierarchy::new();

    assert!(hierarchy.covers(&["read".to_owned()], "read"));
    assert!(!hierarchy.covers(&["admin".to_owned()], "read"));
}

/// An opaque cached token whose recorded scopes cover the requirement is
/// returned without starting a flow — proving coverage no longer depends on
/// a readable JWT scope claim.
#[tokio::test]
async fn get_credential_for_uses_recorded_scopes_for_opaque_token() {
    let provider = test_provider();
    provider
        .store_cached_token("dev", token_with_scopes("opaque-token", &["read", "write"]))
        .await;

    let meta = crate::middleware::CommandMeta {
        scopes: vec!["read".to_owned()],
        ..crate::middleware::CommandMeta::default()
    };
    let req = CredentialRequest::new("dev", "app:list", "read", &meta);
    let credential = provider
        .get_credential_for(&req)
        .await
        .expect("recorded scopes cover the requirement");
    assert_eq!(credential.token, "opaque-token");
}

#[test]
fn union_scopes_dedupes_and_preserves_order() {
    let defaults = vec!["a".to_owned(), "b".to_owned()];
    let granted = vec!["b".to_owned(), "c".to_owned()];
    let required = vec!["c".to_owned(), "d".to_owned()];
    assert_eq!(
        union_scopes(&defaults, &granted, &required),
        vec!["a", "b", "c", "d"]
    );
}

#[test]
fn scopes_from_jwt_empty_for_opaque_or_missing() {
    assert!(scopes_from_jwt("opaque-token").is_empty());
    let no_scope = make_jwt(&json!({ "sub": "user" }));
    assert!(scopes_from_jwt(&no_scope).is_empty());
}

/// When the cached token's JWT already covers the required scopes,
/// `get_credential_for` must return it without starting a PKCE flow.
#[tokio::test]
async fn get_credential_for_uses_cached_token_when_scopes_covered() {
    let provider = test_provider();
    let token = valid_token(&make_jwt(&json!({
        "scope": "apps.app-registry:read apps.app-registry:write",
        "sub": "user-1",
    })));
    provider.store_cached_token("dev", token).await;

    let mut meta = crate::middleware::CommandMeta::default();
    meta.set_scopes(vec!["apps.app-registry:read".to_owned()]);
    let req = CredentialRequest {
        env: "dev",
        command: "app:list",
        tier: "read",
        meta: &meta,
    };
    let credential = provider
        .get_credential_for(&req)
        .await
        .expect("cached token covers required scopes");
    assert_eq!(credential.sub, "user-1");
}

/// With no required scopes, `get_credential_for` behaves like
/// `get_credential` and returns the cached token unchanged.
#[tokio::test]
async fn get_credential_for_no_scopes_returns_cached() {
    let provider = test_provider();
    provider
        .store_cached_token("dev", valid_token("opaque"))
        .await;
    let meta = crate::middleware::CommandMeta::default();
    let req = CredentialRequest {
        env: "dev",
        command: "app:list",
        tier: "read",
        meta: &meta,
    };
    let credential = provider
        .get_credential_for(&req)
        .await
        .expect("no scopes required");
    assert_eq!(credential.token, "opaque");
}

#[test]
fn redirect_uri_default_uses_127_0_0_1_and_redirect_port() {
    let provider = test_provider().with_redirect_port(9000);
    assert_eq!(
        provider.effective_redirect_uri(),
        "http://127.0.0.1:9000/callback"
    );
}

#[test]
fn with_redirect_uri_overrides_default() {
    let provider = test_provider().with_redirect_uri("http://localhost:8080/auth/callback");
    assert_eq!(
        provider.effective_redirect_uri(),
        "http://localhost:8080/auth/callback"
    );
}

#[test]
fn parse_redirect_uri_extracts_port_and_path_from_default() {
    let provider = test_provider().with_redirect_port(9000);
    let (port, path) = provider.parse_redirect_uri().expect("valid URI");
    assert_eq!(port, 9000);
    assert_eq!(path, "/callback");
}

#[test]
fn parse_redirect_uri_extracts_port_and_path_from_custom_uri() {
    let provider = test_provider().with_redirect_uri("http://localhost:8080/auth/callback");
    let (port, path) = provider.parse_redirect_uri().expect("valid URI");
    assert_eq!(port, 8080);
    assert_eq!(path, "/auth/callback");
}

#[test]
fn with_redirect_uri_does_not_affect_listener_host() {
    // The port is derived from the URI, but the listener always binds to
    // 127.0.0.1 — this test confirms the URI host does not change that.
    let provider = test_provider().with_redirect_uri("http://localhost:7777/callback");
    let (port, _) = provider.parse_redirect_uri().expect("valid URI");
    assert_eq!(port, 7777);
    // Caller uses 127.0.0.1 for bind regardless; SocketAddr construction
    // is in run_pkce_flow and is not repeated here.
}

#[test]
fn extract_request_path_strips_query_string() {
    assert_eq!(
        extract_request_path("GET /auth/callback?code=abc&state=xyz HTTP/1.1\r\n"),
        Some("/auth/callback".to_owned()),
    );
}

#[test]
fn extract_request_path_handles_no_query_string() {
    assert_eq!(
        extract_request_path("GET /callback HTTP/1.1\r\n"),
        Some("/callback".to_owned()),
    );
}

#[test]
fn extract_query_param_skips_malformed_pairs() {
    let request = "GET /callback?foo&code=abc123&state=xyz HTTP/1.1\r\nHost: localhost\r\n";
    assert_eq!(
        extract_query_param(request, "code"),
        Some("abc123".to_owned()),
    );
    assert_eq!(
        extract_query_param(request, "state"),
        Some("xyz".to_owned()),
    );
}

#[test]
fn extract_query_param_decodes_percent_encoding() {
    let request = "GET /callback?code=a%20b%2Bc&state=ok HTTP/1.1\r\n";
    assert_eq!(
        extract_query_param(request, "code"),
        Some("a b+c".to_owned()),
    );
}

/// resolve_token must return a pre-seeded in-memory token without
/// triggering the PKCE browser flow (which would require a port and browser).
/// This also exercises the cache-hit path that follows token persistence.
#[tokio::test]
async fn resolve_token_returns_cached_token_without_pkce_flow() {
    let provider = test_provider();
    provider
        .store_cached_token("dev", valid_token("cached-token"))
        .await;

    let resolved = provider
        .resolve_token("dev")
        .await
        .expect("resolve from cache");
    assert_eq!(resolved.access_token, "cached-token");
}

/// list_environments returns only in-memory cache keys; tokens written to
/// disk via file fallback during a previous session are not enumerated.
#[tokio::test]
async fn list_environments_returns_only_cached_keys() {
    let provider = test_provider();
    provider.store_cached_token("dev", valid_token("t1")).await;
    provider.store_cached_token("prod", valid_token("t2")).await;

    let mut envs = provider.list_environments().await.expect("list");
    envs.sort();
    assert_eq!(envs, ["dev", "prod"]);
}

/// A provider with no cache entries returns an empty list, regardless of
/// what credential files may exist on disk from a previous session.
#[tokio::test]
async fn list_environments_returns_empty_without_cache() {
    let provider = test_provider();
    let envs = provider.list_environments().await.expect("list");
    assert!(envs.is_empty(), "expected empty list for a fresh provider");
}

/// Builds an unsigned-looking JWT (`header.payload.signature`) whose payload
/// is the given claims object, base64url-encoded without padding.
fn make_jwt(claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("serialize claims"));
    format!("{header}.{payload}.signature")
}

#[test]
fn decode_jwt_claims_extracts_payload() {
    let token = make_jwt(&json!({"email": "user@example.com", "sub": "abc123"}));
    let claims = decode_jwt_claims(&token).expect("claims decode");
    assert_eq!(
        claims.get("email").and_then(Value::as_str),
        Some("user@example.com")
    );
    assert_eq!(claims.get("sub").and_then(Value::as_str), Some("abc123"));
}

#[test]
fn decode_jwt_claims_returns_none_for_non_jwt() {
    assert!(decode_jwt_claims("opaque-access-token").is_none());
    assert!(decode_jwt_claims("only.two").is_none());
    // Valid structure but the payload is not valid base64/JSON.
    assert!(decode_jwt_claims("aaa.!!!.bbb").is_none());
}

#[test]
fn extract_identity_honors_priority_and_skips_empty() {
    let priority: Vec<String> = DEFAULT_IDENTITY_CLAIMS
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    // `email` is empty, so the next non-empty claim (`preferred_username`) wins.
    let claims = serde_json::from_value(json!({
        "email": "",
        "preferred_username": "jdoe",
        "name": "Jane Doe",
    }))
    .expect("claims map");
    assert_eq!(extract_identity(&claims, &priority), "jdoe");

    // No matching claim yields an empty identity.
    let empty = serde_json::from_value(json!({"unrelated": "x"})).expect("claims map");
    assert_eq!(extract_identity(&empty, &priority), "");
}

#[test]
fn build_credential_populates_identity_and_sub() {
    let provider = test_provider();
    let token = valid_token(&make_jwt(&json!({
        "email": "user@example.com",
        "sub": "subject-1",
    })));
    let credential = provider.build_credential("prod", &token);
    assert_eq!(credential.identity, "user@example.com");
    assert_eq!(credential.sub, "subject-1");
    assert_eq!(credential.env, "prod");
    assert_eq!(credential.provider, "test");
}

#[test]
fn build_credential_populates_scopes_from_stored_token() {
    let provider = test_provider();
    let mut token = valid_token(&make_jwt(&json!({"sub": "subject-1"})));
    token.scopes = vec!["a".to_owned(), "b".to_owned()];
    let credential = provider.build_credential("prod", &token);
    assert_eq!(credential.scopes, vec!["a", "b"]);
}

#[test]
fn build_credential_sets_refreshable_when_refresh_token_present() {
    let provider = test_provider();
    let mut token = valid_token(&make_jwt(&json!({"sub": "subject-1"})));
    token.refresh_token = Some("a-refresh-token".to_owned());
    let credential = provider.build_credential("prod", &token);
    assert!(credential.refreshable);
}

#[test]
fn build_credential_leaves_refreshable_false_without_refresh_token() {
    let provider = test_provider();
    let token = valid_token(&make_jwt(&json!({"sub": "subject-1"})));
    let credential = provider.build_credential("prod", &token);
    assert!(!credential.refreshable);
}

#[test]
fn build_credential_leaves_identity_blank_for_opaque_token() {
    let provider = test_provider();
    let token = valid_token("opaque-token");
    let credential = provider.build_credential("prod", &token);
    assert_eq!(credential.identity, "");
    assert_eq!(credential.sub, "");
}

#[test]
fn with_identity_claims_overrides_selection() {
    let provider = test_provider().with_identity_claims(&["custom_user"]);
    let token = valid_token(&make_jwt(&json!({
        "email": "ignored@example.com",
        "custom_user": "picked",
    })));
    let credential = provider.build_credential("prod", &token);
    assert_eq!(credential.identity, "picked");
}

/// In-memory [`CredentialStorage`] double: lets us assert the provider
/// delegates load/save/delete without any real keychain or filesystem.
#[derive(Debug, Default)]
struct MemoryStorage {
    entries: std::sync::Mutex<HashMap<String, String>>,
}

impl MemoryStorage {
    fn entry_key(key: &CredentialKey<'_>) -> String {
        format!("{}/{}/{}", key.app_id, key.provider, key.env)
    }
}

#[async_trait]
impl CredentialStorage for MemoryStorage {
    async fn load(&self, key: &CredentialKey<'_>) -> Option<String> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::entry_key(key))
            .cloned()
    }

    async fn save(&self, key: &CredentialKey<'_>, value: &str) -> crate::Result<()> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(Self::entry_key(key), value.to_owned());
        Ok(())
    }

    async fn delete(&self, key: &CredentialKey<'_>) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&Self::entry_key(key));
    }
}

#[test]
#[allow(deprecated)]
fn with_file_fallback_maps_to_store_modes() {
    assert_eq!(
        test_provider().with_file_fallback(true).store_mode,
        Some(CredentialStore::Auto)
    );
    assert_eq!(
        test_provider().with_file_fallback(false).store_mode,
        Some(CredentialStore::Keyring)
    );
}

#[test]
fn builders_record_storage_selection() {
    assert_eq!(
        test_provider()
            .with_credential_store(CredentialStore::File)
            .store_mode,
        Some(CredentialStore::File)
    );
    let provider = test_provider().with_storage(Arc::new(MemoryStorage::default()));
    assert!(provider.storage_override.is_some());
}

#[tokio::test]
async fn provider_delegates_to_injected_storage() {
    let mem = Arc::new(MemoryStorage::default());
    let provider = test_provider().with_app_id("app").with_storage(mem.clone());

    // No entry yet: status reports not-logged-in.
    assert!(provider.status("dev").await.is_err());

    // Saving routes through the injected store.
    provider
        .save_stored("dev", &valid_token("tok"))
        .await
        .expect("save");
    let key = CredentialKey::new("app", "test", "dev");
    assert!(mem.load(&key).await.is_some(), "token reached the store");

    // And status reads it back.
    let cred = provider.status("dev").await.expect("status");
    assert_eq!(cred.token, "tok");

    // Logout clears it from the store.
    provider.logout("dev").await.expect("logout");
    assert!(mem.load(&key).await.is_none(), "token removed on logout");
}

#[tokio::test]
async fn corrupt_stored_blob_self_heals() {
    let mem = Arc::new(MemoryStorage::default());
    let key = CredentialKey::new("app", "test", "dev");
    mem.save(&key, "not-valid-json").await.expect("seed");

    let provider = test_provider().with_app_id("app").with_storage(mem.clone());
    assert!(provider.load_stored("dev").await.is_none());
    assert!(
        mem.load(&key).await.is_none(),
        "corrupt blob should be deleted (self-heal)"
    );
}

#[tokio::test]
// The guard is intentionally held across awaits to serialize env mutation.
#[allow(clippy::await_holding_lock)]
async fn file_store_round_trips_without_keyring() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Hold the shared lock + env guard across the awaits.
    let _lock = crate::config::test_env::lock();
    let _env = crate::config::test_env::EnvVarGuard::set("XDG_CONFIG_HOME", Some(dir.path()));

    let provider = test_provider()
        .with_app_id("app")
        .with_credential_store(CredentialStore::File);
    assert!(provider.status("dev").await.is_err());
    provider
        .save_stored("dev", &valid_token("filetok"))
        .await
        .expect("save");
    let cred = provider.status("dev").await.expect("status");
    assert_eq!(cred.token, "filetok");
}

/// The fix: a provider wired to a shared `Arc<Environments>` whose
/// `environments.toml` file layer defines `prod` with a different `client_id`
/// resolves the FILE's client id. This proves the provider's file layer
/// resolves — the shared, app_id-stamped instance reaches the provider rather
/// than an unstamped copy whose file path is `None`.
#[test]
fn wired_provider_resolves_client_id_from_environments_file() {
    use crate::environments::{EnvTable, Environments};

    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("environments.toml");
    std::fs::write(
        &file,
        r#"
[prod]
client_id = "file-prod-client"
"#,
    )
    .expect("write environments.toml");

    let environments = Arc::new(
        Environments::new("prod")
            .with_app_id("x")
            .with_environment(
                "prod",
                EnvTable::new().with("client_id", "compiled-prod-client"),
            )
            .with_config_file_path_override(file),
    );

    let provider = PkceAuthProvider::new(
        "godaddy",
        "https://base/auth",
        "https://base/token",
        "base-client",
        &["openid"],
    )
    .with_environments(environments);

    // The file overrides the compiled client id, which itself overrides the
    // provider's base — proving the wired provider reads the file layer.
    assert_eq!(
        provider
            .effective_oauth("prod")
            .expect("assembles")
            .client_id,
        "file-prod-client"
    );
}

/// A wired provider's resolved environment overrides the base config's
/// client id.
#[test]
fn wired_provider_resolved_env_overrides_base_client_id() {
    use crate::environments::{EnvTable, Environments};

    let environments = Arc::new(
        Environments::new("prod")
            .with_app_id("x")
            .with_environment("prod", EnvTable::new().with("client_id", "env-client")),
    );
    let wired = test_provider().with_environments(environments);
    assert_eq!(
        wired.effective_oauth("prod").expect("assembles").client_id,
        "env-client"
    );
}

/// `client_id`/`auth_url`/`token_url` have no default: a provider whose
/// base config was never given a real client id, and with no environment
/// wired to supply one either, must fail loudly rather than assemble with
/// an empty client id.
#[test]
fn effective_oauth_rejects_a_never_initialized_client_id() {
    let provider = PkceAuthProvider::new(
        "test",
        "https://example.com/auth",
        "https://example.com/token",
        "",
        &["openid"],
    );
    let err = provider
        .effective_oauth("prod")
        .expect_err("a blank client_id with no other tier to supply one must be an error");
    assert!(err.to_string().contains("client_id"));
}
