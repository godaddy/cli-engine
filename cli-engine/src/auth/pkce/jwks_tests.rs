//! `JwksVerifier` against real signatures: RSA and ECDSA keys are generated here,
//! tokens are signed with them, and a loopback server publishes the key set.

use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use aws_lc_rs::{
    hmac,
    rand::SystemRandom,
    rsa::{KeyPair as RsaKeyPair, KeySize},
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _, RSA_PKCS1_SHA256},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

use super::{IdTokenVerifier, JwksVerifier, VerifyError};

const ISSUER: &str = "https://idp.example.com";
const AUDIENCE: &str = "client-id";

fn b64(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Keys generated once for the whole test binary: RSA generation is slow.
struct Fixture {
    rsa_a: RsaKeyPair,
    rsa_b: RsaKeyPair,
    ec: EcdsaKeyPair,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| Fixture {
        rsa_a: RsaKeyPair::generate(KeySize::Rsa2048).expect("rsa key a"),
        rsa_b: RsaKeyPair::generate(KeySize::Rsa2048).expect("rsa key b"),
        ec: EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).expect("ec key"),
    })
}

fn rsa_signature(key: &RsaKeyPair, message: &[u8]) -> Vec<u8> {
    let mut signature = vec![0; key.public_modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message,
        &mut signature,
    )
    .expect("rsa sign");
    signature
}

fn rsa_jwk(key: &RsaKeyPair, kid: &str) -> Value {
    let public = key.public_key();
    json!({
        "kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256",
        "n": b64(public.modulus().big_endian_without_leading_zero()),
        "e": b64(public.exponent().big_endian_without_leading_zero()),
    })
}

fn ec_jwk(kid: &str) -> Value {
    let point = fixture().ec.public_key().as_ref();
    json!({
        "kty": "EC", "crv": "P-256", "kid": kid, "use": "sig", "alg": "ES256",
        "x": b64(&point[1..33]), "y": b64(&point[33..65]),
    })
}

fn key_set(keys: Vec<Value>) -> String {
    json!({ "keys": keys }).to_string()
}

fn good_claims() -> Value {
    json!({
        "iss": ISSUER, "aud": AUDIENCE, "exp": Utc::now().timestamp() + 3600,
        "sub": "subject-1", "email": "user@example.com",
    })
}

/// Assembles `header.payload.signature`, signing with `sign` over the signing input.
fn jwt(header: &Value, claims: &Value, sign: impl FnOnce(&[u8]) -> Vec<u8>) -> String {
    let signing_input = format!("{}.{}", b64(header.to_string()), b64(claims.to_string()));
    let signature = sign(signing_input.as_bytes());
    format!("{signing_input}.{}", b64(signature))
}

fn rs256(key: &RsaKeyPair, kid: &str, claims: &Value) -> String {
    let header = json!({"alg": "RS256", "typ": "JWT", "kid": kid});
    jwt(&header, claims, |message| rsa_signature(key, message))
}

fn es256(kid: &str, claims: &Value) -> String {
    let header = json!({"alg": "ES256", "typ": "JWT", "kid": kid});
    jwt(&header, claims, |message| {
        fixture()
            .ec
            .sign(&SystemRandom::new(), message)
            .expect("ec sign")
            .as_ref()
            .to_vec()
    })
}

/// What the loopback key endpoint answers.
#[derive(Clone)]
enum Reply {
    Body(String),
    Status(u16),
    Redirect(String),
}

/// A loopback key endpoint that counts its requests and can be re-scripted.
struct KeyServer {
    url: String,
    hits: Arc<AtomicUsize>,
    reply: Arc<Mutex<Reply>>,
    task: JoinHandle<()>,
}

impl KeyServer {
    async fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/keys", listener.local_addr().expect("addr"));
        let hits = Arc::new(AtomicUsize::new(0));
        let reply = Arc::new(Mutex::new(reply));
        let task = tokio::spawn({
            let (hits, reply) = (hits.clone(), reply.clone());
            async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let (hits, reply) = (hits.clone(), reply.clone());
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0_u8; 1024];
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(read) => head.extend_from_slice(&buf[..read]),
                            }
                        }
                        hits.fetch_add(1, Ordering::SeqCst);
                        let reply = reply.lock().expect("reply lock").clone();
                        let response = match reply {
                            Reply::Body(body) => format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            ),
                            Reply::Status(code) => format!(
                                "HTTP/1.1 {code} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            ),
                            Reply::Redirect(to) => format!(
                                "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\n\
                                 connection: close\r\n\r\n"
                            ),
                        };
                        // A client that already gave up makes this fail; that is
                        // expected in the tests that abandon a request.
                        drop(stream.write_all(response.as_bytes()).await);
                    });
                }
            }
        });
        Self {
            url,
            hits,
            reply,
            task,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn set(&self, reply: Reply) {
        *self.reply.lock().expect("reply lock") = reply;
    }

    fn verifier(&self) -> JwksVerifier {
        JwksVerifier::new_unchecked(ISSUER, &self.url).expect("verifier")
    }
}

impl Drop for KeyServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server_with_both_rsa_keys() -> KeyServer {
    let f = fixture();
    KeyServer::start(Reply::Body(key_set(vec![
        rsa_jwk(&f.rsa_a, "key-a"),
        rsa_jwk(&f.rsa_b, "key-b"),
        ec_jwk("key-ec"),
    ])))
    .await
}

/// The reason of an `Invalid` outcome; fails the test on anything else.
fn expect_invalid(result: Result<super::VerifiedIdToken, VerifyError>) -> String {
    let err = result.expect_err("the token should be rejected");
    assert!(
        matches!(err, VerifyError::Invalid(_)),
        "expected Invalid, got {err:?}"
    );
    err.to_string()
}

/// The reason of an `Unavailable` outcome; fails the test on anything else.
fn expect_unavailable(result: Result<super::VerifiedIdToken, VerifyError>) -> String {
    let err = result.expect_err("verification should not have been possible");
    assert!(
        matches!(err, VerifyError::Unavailable(_)),
        "expected Unavailable, got {err:?}"
    );
    err.to_string()
}

#[tokio::test]
async fn a_valid_rs256_token_is_verified_and_its_claims_returned() {
    let server = server_with_both_rsa_keys().await;
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let verified = server
        .verifier()
        .verify(&token, AUDIENCE)
        .await
        .expect("valid");
    assert_eq!(verified.claims()["email"], "user@example.com");
    assert_eq!(verified.claims()["sub"], "subject-1");
}

#[tokio::test]
async fn a_valid_es256_token_is_verified() {
    let server = server_with_both_rsa_keys().await;
    let token = es256("key-ec", &good_claims());
    server
        .verifier()
        .verify(&token, AUDIENCE)
        .await
        .expect("valid");
}

#[tokio::test]
async fn a_signature_from_a_different_key_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    // Names key A in the header but is signed with key B.
    let token = rs256(&fixture().rsa_b, "key-a", &good_claims());
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("signature"), "{reason}");
}

#[tokio::test]
async fn a_tampered_payload_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
    let mut forged = good_claims();
    forged["email"] = json!("attacker@evil.test");
    parts[1] = b64(forged.to_string());
    let reason = expect_invalid(server.verifier().verify(&parts.join("."), AUDIENCE).await);
    assert!(reason.contains("signature"), "{reason}");
}

#[tokio::test]
async fn the_wrong_issuer_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    let mut claims = good_claims();
    claims["iss"] = json!("https://evil.example.com");
    let token = rs256(&fixture().rsa_a, "key-a", &claims);
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("issuer"), "{reason}");
}

#[tokio::test]
async fn the_wrong_audience_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let reason = expect_invalid(server.verifier().verify(&token, "someone-else").await);
    assert!(reason.contains("audience"), "{reason}");
}

#[tokio::test]
async fn an_expired_token_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    let mut claims = good_claims();
    claims["exp"] = json!(Utc::now().timestamp() - 3600);
    let token = rs256(&fixture().rsa_a, "key-a", &claims);
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("expired"), "{reason}");
}

#[tokio::test]
async fn a_token_missing_a_required_claim_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    for claim in ["exp", "iss", "aud"] {
        let mut claims = good_claims();
        claims.as_object_mut().expect("object").remove(claim);
        let token = rs256(&fixture().rsa_a, "key-a", &claims);
        let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
        assert!(
            reason.contains("required claim") || reason.contains("could not be verified"),
            "missing {claim}: {reason}"
        );
    }
}

#[tokio::test]
async fn an_hmac_token_signed_with_the_public_key_is_rejected_before_any_key_is_fetched() {
    // The classic algorithm-confusion attack: sign HS256 using the published
    // public key material as the shared secret.
    let server = server_with_both_rsa_keys().await;
    let secret = fixture()
        .rsa_a
        .public_key()
        .modulus()
        .big_endian_without_leading_zero()
        .to_vec();
    let key = hmac::Key::new(hmac::HMAC_SHA256, &secret);
    let header = json!({"alg": "HS256", "typ": "JWT", "kid": "key-a"});
    let token = jwt(&header, &good_claims(), |message| {
        hmac::sign(&key, message).as_ref().to_vec()
    });
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("algorithm"), "{reason}");
    assert_eq!(server.hits(), 0, "an unacceptable algorithm needs no keys");
}

#[tokio::test]
async fn an_unsigned_token_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    let header = json!({"alg": "none", "typ": "JWT", "kid": "key-a"});
    let token = jwt(&header, &good_claims(), |_| Vec::new());
    expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn malformed_tokens_are_rejected() {
    let server = server_with_both_rsa_keys().await;
    for token in ["", "not-a-jwt", "a.b.c", "a.b"] {
        expect_invalid(server.verifier().verify(token, AUDIENCE).await);
    }
}

#[tokio::test]
async fn a_token_that_names_no_key_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    let header = json!({"alg": "RS256", "typ": "JWT"});
    let token = jwt(&header, &good_claims(), |message| {
        rsa_signature(&fixture().rsa_a, message)
    });
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("names no signing key"), "{reason}");
}

#[tokio::test]
async fn a_key_that_is_not_for_signatures_is_rejected() {
    let f = fixture();
    let mut jwk = rsa_jwk(&f.rsa_a, "key-a");
    jwk["use"] = json!("enc");
    let server = KeyServer::start(Reply::Body(key_set(vec![jwk]))).await;
    let token = rs256(&f.rsa_a, "key-a", &good_claims());
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("not a signature key"), "{reason}");
}

#[tokio::test]
async fn keys_are_fetched_once_and_kept_across_verifications() {
    let server = server_with_both_rsa_keys().await;
    let verifier = server.verifier();
    for _ in 0..3 {
        let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
        verifier.verify(&token, AUDIENCE).await.expect("valid");
    }
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn an_unknown_key_id_refetches_once_and_then_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    let verifier = server.verifier();
    let token = rs256(&fixture().rsa_a, "key-zzz", &good_claims());

    let reason = expect_invalid(verifier.verify(&token, AUDIENCE).await);
    assert!(reason.contains("`kid`"), "{reason}");
    assert_eq!(server.hits(), 1);

    expect_invalid(verifier.verify(&token, AUDIENCE).await);
    assert_eq!(server.hits(), 2, "one refetch per verification, no more");
}

#[tokio::test]
async fn a_rotated_in_key_is_picked_up_without_a_restart() {
    let f = fixture();
    let server = KeyServer::start(Reply::Body(key_set(vec![rsa_jwk(&f.rsa_a, "key-a")]))).await;
    let verifier = server.verifier();
    verifier
        .verify(&rs256(&f.rsa_a, "key-a", &good_claims()), AUDIENCE)
        .await
        .expect("old key");

    server.set(Reply::Body(key_set(vec![
        rsa_jwk(&f.rsa_a, "key-a"),
        rsa_jwk(&f.rsa_b, "key-b"),
    ])));
    verifier
        .verify(&rs256(&f.rsa_b, "key-b", &good_claims()), AUDIENCE)
        .await
        .expect("new key");
    assert_eq!(server.hits(), 2);
}

#[tokio::test]
async fn a_key_endpoint_that_cannot_be_reached_is_unavailable_not_invalid() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/keys", closed.local_addr().expect("addr"));
    drop(closed);
    let verifier = JwksVerifier::new_unchecked(ISSUER, url).expect("verifier");
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let reason = expect_unavailable(verifier.verify(&token, AUDIENCE).await);
    assert!(reason.contains("could not be reached"), "{reason}");
}

#[tokio::test]
async fn an_error_status_a_bad_body_and_an_oversized_body_are_unavailable() {
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let huge = format!(r#"{{"keys":[],"pad":"{}"}}"#, "x".repeat(300 * 1024));
    let cases = [
        (Reply::Status(500), "HTTP 500"),
        (Reply::Status(404), "HTTP 404"),
        (Reply::Body("not json".to_owned()), "could not be parsed"),
        (Reply::Body(huge), "too large"),
    ];
    for (reply, expected) in cases {
        let server = KeyServer::start(reply).await;
        let reason = expect_unavailable(server.verifier().verify(&token, AUDIENCE).await);
        assert!(reason.contains(expected), "{expected}: {reason}");
    }
}

#[tokio::test]
async fn a_redirect_from_the_key_endpoint_is_not_followed() {
    let elsewhere = server_with_both_rsa_keys().await;
    let server = KeyServer::start(Reply::Redirect(elsewhere.url.clone())).await;
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let reason = expect_unavailable(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("HTTP 302"), "{reason}");
    assert_eq!(
        elsewhere.hits(),
        0,
        "the redirect target must never be asked"
    );
}

#[tokio::test]
async fn unavailable_and_invalid_reasons_never_contain_the_token() {
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let server = KeyServer::start(Reply::Status(503)).await;
    let reason = expect_unavailable(server.verifier().verify(&token, AUDIENCE).await);
    assert!(!reason.contains(&token), "{reason}");

    let server = server_with_both_rsa_keys().await;
    let mut claims = good_claims();
    claims["iss"] = json!("https://evil.example.com");
    let reason = expect_invalid(
        server
            .verifier()
            .verify(&rs256(&fixture().rsa_a, "key-a", &claims), AUDIENCE)
            .await,
    );
    assert!(!reason.contains("evil.example.com"), "{reason}");
    assert!(!reason.contains(&token), "{reason}");
}

#[test]
fn only_https_key_urls_on_the_issuers_host_are_accepted() {
    let accepted = [
        ("https://idp.example.com", "https://idp.example.com/keys"),
        (
            "https://idp.example.com/oauth2/default",
            "https://idp.example.com/oauth2/default/v1/keys",
        ),
        ("https://IDP.example.com", "https://idp.example.com/keys"),
        (
            "https://idp.example.com",
            "https://idp.example.com:443/keys",
        ),
    ];
    for (issuer, jwks) in accepted {
        assert!(JwksVerifier::new(issuer, jwks).is_ok(), "{issuer} {jwks}");
    }
    let rejected = [
        ("https://idp.example.com", "http://idp.example.com/keys"),
        ("https://idp.example.com", "https://evil.example.com/keys"),
        (
            "https://idp.example.com",
            "https://idp.example.com.evil.test/keys",
        ),
        (
            "https://idp.example.com",
            "https://idp.example.com:8443/keys",
        ),
        (
            "https://idp.example.com",
            "https://user:pw@idp.example.com/keys",
        ),
        (
            "https://idp.example.com",
            "https://user@idp.example.com/keys",
        ),
        ("https://idp.example.com", "/keys"),
        ("https://idp.example.com", "not a url"),
        ("http://idp.example.com", "https://idp.example.com/keys"),
        ("idp.example.com", "https://idp.example.com/keys"),
    ];
    for (issuer, jwks) in rejected {
        assert!(JwksVerifier::new(issuer, jwks).is_err(), "{issuer} {jwks}");
    }
}

#[test]
fn a_rejected_url_error_names_the_rule_and_not_the_url() {
    let err = JwksVerifier::new(
        "https://idp.example.com",
        "https://user:hunter2@idp.example.com/k",
    )
    .expect_err("credentials in the url");
    let message = err.to_string();
    assert!(
        message.contains("https URL without a port or credentials"),
        "{message}"
    );
    assert!(!message.contains("hunter2"), "{message}");
}

#[tokio::test]
async fn a_verifier_plugs_into_a_provider_end_to_end() {
    use crate::auth::AuthProvider;

    let f = fixture();
    let server = KeyServer::start(Reply::Body(key_set(vec![rsa_jwk(&f.rsa_a, "key-a")]))).await;
    let storage = Arc::new(super::tests::MemoryStorage::default());
    let provider = super::test_provider()
        .with_app_id("app")
        .with_storage(storage)
        .with_id_token_verifier(Arc::new(server.verifier()));

    // A stored token whose id_token is genuinely signed names its real identity...
    let genuine = rs256(&f.rsa_a, "key-a", &good_claims());
    let mut stored = super::tests::valid_token("opaque-access-token");
    stored.id_token = Some(genuine);
    provider.save_stored("dev", &stored).await.expect("seed");
    let credential = provider.status("dev").await.expect("verified");
    assert_eq!(credential.identity, "user@example.com");

    // ...and one an attacker edited in storage is refused outright.
    let mut forged_claims = good_claims();
    forged_claims["email"] = json!("attacker@evil.test");
    let forged = rs256(&f.rsa_b, "key-a", &forged_claims);
    stored.id_token = Some(forged);
    provider.save_stored("dev", &stored).await.expect("seed");
    let err = provider.status("dev").await.expect_err("forged");
    assert!(err.to_string().contains("failed verification"), "{err}");
}
