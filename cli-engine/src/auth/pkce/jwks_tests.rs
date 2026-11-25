//! `JwksVerifier` against real signatures: RSA and ECDSA keys are generated here,
//! tokens are signed with them, and a loopback server publishes the key set.

use std::sync::{Arc, OnceLock};

use super::{
    IdTokenVerifier, JwksVerifier, OidcDiscovery, VerifyError,
    loopback::{LoopbackServer, Reply},
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

/// A loopback key endpoint at `/keys`, with a verifier pointed at it.
struct KeyServer(LoopbackServer);

impl KeyServer {
    async fn start(reply: Reply) -> Self {
        Self(LoopbackServer::keys(reply).await)
    }

    fn url(&self) -> String {
        self.0.keys_url()
    }

    fn hits(&self) -> usize {
        self.0.hits()
    }

    fn body_bytes_sent(&self) -> usize {
        self.0.body_bytes_sent()
    }

    fn last_request_headers(&self) -> String {
        self.0.last_request_headers()
    }

    fn set(&self, reply: Reply) {
        self.0.set("/keys", reply);
    }

    fn verifier(&self) -> JwksVerifier {
        JwksVerifier::new_unchecked(ISSUER, self.url()).expect("verifier")
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

/// A token whose `aud` lists this client among others, with no `azp` naming it as
/// the party the token was issued to, was issued for someone else too.
#[tokio::test]
async fn a_multi_audience_token_must_name_this_client_as_authorized_party() {
    let server = server_with_both_rsa_keys().await;
    let f = fixture();

    let mut claims = good_claims();
    claims["aud"] = json!([AUDIENCE, "another-client"]);
    let token = rs256(&f.rsa_a, "key-a", &claims);
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("authorized party"), "no azp: {reason}");

    claims["azp"] = json!("another-client");
    let token = rs256(&f.rsa_a, "key-a", &claims);
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("authorized party"), "wrong azp: {reason}");

    claims["azp"] = json!(AUDIENCE);
    let token = rs256(&f.rsa_a, "key-a", &claims);
    server
        .verifier()
        .verify(&token, AUDIENCE)
        .await
        .expect("this client is the authorized party");
}

#[tokio::test]
async fn an_azp_naming_another_client_is_rejected_even_with_a_single_audience() {
    let server = server_with_both_rsa_keys().await;
    let mut claims = good_claims();
    claims["azp"] = json!("another-client");
    let token = rs256(&fixture().rsa_a, "key-a", &claims);
    expect_invalid(server.verifier().verify(&token, AUDIENCE).await);

    // The common case: a single audience with an azp that agrees.
    claims["azp"] = json!(AUDIENCE);
    let token = rs256(&fixture().rsa_a, "key-a", &claims);
    server
        .verifier()
        .verify(&token, AUDIENCE)
        .await
        .expect("azp agrees with aud");
}

#[tokio::test]
async fn a_token_without_a_usable_subject_is_rejected() {
    let server = server_with_both_rsa_keys().await;
    for sub in [
        None,
        Some(json!("")),
        Some(json!(42)),
        Some(json!(null)),
        Some(json!(["a"])),
    ] {
        let mut claims = good_claims();
        match &sub {
            Some(value) => claims["sub"] = value.clone(),
            None => {
                claims.as_object_mut().expect("object").remove("sub");
            }
        }
        let token = rs256(&fixture().rsa_a, "key-a", &claims);
        // Rejected either by our subject check or earlier, by the JWT layer that
        // refuses a `sub` which is not a string; the reason text differs, the
        // outcome must not.
        expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    }
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

/// A key set whose only key is key A with `tweak` applied to its JWK.
async fn server_with_tweaked_key(tweak: impl FnOnce(&mut Value)) -> KeyServer {
    let mut jwk = rsa_jwk(&fixture().rsa_a, "key-a");
    tweak(&mut jwk);
    KeyServer::start(Reply::Body(key_set(vec![jwk]))).await
}

#[tokio::test]
async fn a_key_declaring_a_different_algorithm_is_not_used() {
    // The key says RS384; the token is RS256, signed with the right key.
    let server = server_with_tweaked_key(|jwk| jwk["alg"] = json!("RS384")).await;
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("algorithm"), "{reason}");
}

#[tokio::test]
async fn a_key_declaring_the_matching_algorithm_or_none_is_used() {
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let declared = server_with_tweaked_key(|jwk| jwk["alg"] = json!("RS256")).await;
    declared
        .verifier()
        .verify(&token, AUDIENCE)
        .await
        .expect("alg matches");
    // Some providers publish no `alg` at all.
    let silent = server_with_tweaked_key(|jwk| {
        jwk.as_object_mut().expect("object").remove("alg");
    })
    .await;
    silent
        .verifier()
        .verify(&token, AUDIENCE)
        .await
        .expect("no alg is not a restriction");
}

#[tokio::test]
async fn a_key_whose_operations_exclude_verify_is_not_used() {
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    for ops in [json!(["sign"]), json!(["encrypt", "wrapKey"]), json!([])] {
        let server = server_with_tweaked_key(|jwk| {
            jwk.as_object_mut().expect("object").remove("use");
            jwk["key_ops"] = ops.clone();
        })
        .await;
        let reason = expect_invalid(server.verifier().verify(&token, AUDIENCE).await);
        assert!(reason.contains("not a signature key"), "{ops}: {reason}");
    }
}

#[tokio::test]
async fn a_key_whose_operations_include_verify_is_used() {
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let server = server_with_tweaked_key(|jwk| {
        jwk.as_object_mut().expect("object").remove("use");
        jwk["key_ops"] = json!(["sign", "verify"]);
    })
    .await;
    server
        .verifier()
        .verify(&token, AUDIENCE)
        .await
        .expect("verify is permitted");
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

const LIMIT: usize = 256 * 1024;

/// How much of an oversized body the server may have written by the time the
/// client gives up. The kernel's socket buffers absorb a couple of megabytes
/// before the writer blocks on a client that has stopped reading, so this cannot
/// be as tight as the 256 KiB limit; it only has to be far below the tens of
/// megabytes a client that buffered the whole body would have drawn.
const BODY_BOUND: usize = 8 * 1024 * 1024;

#[tokio::test]
async fn an_oversized_declared_length_is_not_drained() {
    let server = KeyServer::start(Reply::Lying {
        declared: 256 * 1024 * 1024,
        actual: 256 * 1024 * 1024,
    })
    .await;
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let reason = expect_unavailable(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("too large"), "{reason}");
    assert!(
        server.body_bytes_sent() < BODY_BOUND,
        "the client must not have drained the body: {} bytes sent",
        server.body_bytes_sent()
    );
}

#[tokio::test]
async fn a_body_without_a_declared_length_is_abandoned_once_it_passes_the_limit() {
    // 64 MiB offered, no content-length: buffering it whole would take all of it.
    let server = KeyServer::start(Reply::Unbounded {
        bytes: 64 * 1024 * 1024,
    })
    .await;
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    let reason = expect_unavailable(server.verifier().verify(&token, AUDIENCE).await);
    assert!(reason.contains("too large"), "{reason}");
    assert!(
        server.body_bytes_sent() < BODY_BOUND,
        "reading must stop near the limit, not after the whole body: {} bytes sent",
        server.body_bytes_sent()
    );
}

#[tokio::test]
async fn a_declared_length_that_understates_the_body_is_still_bounded() {
    let server = KeyServer::start(Reply::Lying {
        declared: 1024,
        actual: 64 * 1024 * 1024,
    })
    .await;
    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    // hyper stops at the declared length, so this is a short, unparseable body:
    // unavailable either way, and never a large read.
    expect_unavailable(server.verifier().verify(&token, AUDIENCE).await);
    assert!(server.body_bytes_sent() < BODY_BOUND);
}

#[tokio::test]
async fn a_key_set_just_under_the_limit_is_still_accepted() {
    let f = fixture();
    let mut keys = vec![rsa_jwk(&f.rsa_a, "key-a")];
    // Pad with unrelated keys to just under the limit. One padding key's size is
    // measured once and the count computed from it, rather than re-serializing the
    // growing set on every pass.
    // Sized by the longest id any pad will have (`pad-9999`), so no pad is bigger
    // than the estimate and the total cannot overshoot the limit.
    let one_pad = ec_jwk("pad-9999").to_string().len() + 1;
    let base = key_set(keys.clone()).len();
    let pads = (LIMIT - 2048 - base) / one_pad;
    assert!(
        pads < 10_000,
        "{pads} pads would outgrow the id width assumed above"
    );
    keys.extend((0..pads).map(|n| ec_jwk(&format!("pad-{n}"))));
    let body = key_set(keys);
    assert!(
        (LIMIT - 4096..LIMIT).contains(&body.len()),
        "the key set should sit just under the limit, got {} bytes",
        body.len()
    );
    let server = KeyServer::start(Reply::Body(body)).await;
    server
        .verifier()
        .verify(&rs256(&f.rsa_a, "key-a", &good_claims()), AUDIENCE)
        .await
        .expect("a large but legitimate key set is accepted");
}

/// The verifier is built first, as during registration, and the process identity
/// is published afterwards, as when a command starts. The request must carry the
/// identity current at request time, and not the attribution headers.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn the_key_request_carries_the_current_user_agent_and_no_attribution_headers() {
    let _guard = crate::transport::client::UA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _restore = crate::transport::client::RestoreDefaultUserAgent;

    let server = server_with_both_rsa_keys().await;
    let verifier = server.verifier();

    crate::transport::set_default_user_agent("published-later/9.9");
    crate::transport::client::set_default_headers(
        [("x-client-session-id".to_owned(), "abc123session".to_owned())].into(),
    );

    let token = rs256(&fixture().rsa_a, "key-a", &good_claims());
    verifier.verify(&token, AUDIENCE).await.expect("valid");

    let head = server.last_request_headers();
    assert!(
        head.contains("user-agent: published-later/9.9"),
        "the identity published after construction must be used:\n{head}"
    );
    assert!(
        !head.contains("x-client-session-id") && !head.contains("abc123session"),
        "attribution headers must not go to the identity provider:\n{head}"
    );
}

#[tokio::test]
async fn a_redirect_from_the_key_endpoint_is_not_followed() {
    let elsewhere = server_with_both_rsa_keys().await;
    let server = KeyServer::start(Reply::Redirect(elsewhere.url())).await;
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

// ---------------------------------------------------------------------------
// A verifier whose issuer and key-set URL come from discovery
// ---------------------------------------------------------------------------

const WELL_KNOWN: &str = "/.well-known/openid-configuration";

/// An identity provider serving discovery and `/keys`, whose issuer is its own
/// base URL, so tokens must carry that as `iss`.
async fn discovered_provider() -> (LoopbackServer, Arc<OidcDiscovery>, String) {
    let f = fixture();
    let server = LoopbackServer::routes([
        (WELL_KNOWN, Reply::Status(500)),
        (
            "/keys",
            Reply::Body(key_set(vec![rsa_jwk(&f.rsa_a, "key-a")])),
        ),
    ])
    .await;
    let base = server.url("");
    server.set(
        WELL_KNOWN,
        Reply::Body(
            json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "jwks_uri": format!("{base}/keys"),
            })
            .to_string(),
        ),
    );
    let discovery = Arc::new(OidcDiscovery::new_loopback(&base).expect("discovery"));
    (server, discovery, base)
}

fn claims_for(issuer: &str) -> Value {
    let mut claims = good_claims();
    claims["iss"] = json!(issuer);
    claims
}

#[tokio::test]
async fn a_verifier_built_from_discovery_verifies_against_the_discovered_issuer_and_keys() {
    let (server, discovery, base) = discovered_provider().await;
    let verifier = JwksVerifier::from_discovery(discovery).expect("verifier");

    let token = rs256(&fixture().rsa_a, "key-a", &claims_for(&base));
    let verified = verifier.verify(&token, AUDIENCE).await.expect("valid");
    assert_eq!(verified.claims()["email"], "user@example.com");
    assert_eq!(server.hits_for("/keys"), 1);
    assert_eq!(server.hits_for(WELL_KNOWN), 1);
}

#[tokio::test]
async fn a_discovered_verifier_rejects_a_token_from_another_issuer() {
    let (_server, discovery, _base) = discovered_provider().await;
    let verifier = JwksVerifier::from_discovery(discovery).expect("verifier");
    let token = rs256(
        &fixture().rsa_a,
        "key-a",
        &claims_for("https://evil.example.com"),
    );
    let reason = expect_invalid(verifier.verify(&token, AUDIENCE).await);
    assert!(reason.contains("issuer"), "{reason}");
}

#[tokio::test]
async fn a_verifier_and_a_provider_sharing_discovery_fetch_the_document_once() {
    let (server, discovery, base) = discovered_provider().await;
    let verifier = JwksVerifier::from_discovery(discovery.clone()).expect("verifier");

    // The provider needs the endpoints first, as it would to log in.
    let provider = super::PkceAuthProvider::new("test", "", "", "client-id", &["openid"])
        .with_oidc_discovery(discovery);
    provider.resolved_oauth("dev").await.expect("endpoints");
    verifier
        .verify(
            &rs256(&fixture().rsa_a, "key-a", &claims_for(&base)),
            AUDIENCE,
        )
        .await
        .expect("valid");

    assert_eq!(server.hits_for(WELL_KNOWN), 1, "one fetch for both");
}

#[tokio::test]
async fn a_discovery_failure_is_unavailable_and_never_a_verdict_on_the_token() {
    let server = LoopbackServer::routes([(WELL_KNOWN, Reply::Status(503))]).await;
    let discovery = Arc::new(OidcDiscovery::new_loopback(server.url("")).expect("discovery"));
    let verifier = JwksVerifier::from_discovery(discovery).expect("verifier");

    let token = rs256(&fixture().rsa_a, "key-a", &claims_for(&server.url("")));
    let reason = expect_unavailable(verifier.verify(&token, AUDIENCE).await);
    assert!(reason.contains("discovery"), "{reason}");
    assert!(!reason.contains(&token), "{reason}");
}

#[tokio::test]
async fn a_poisoned_key_set_url_in_the_document_is_never_fetched() {
    let evil = LoopbackServer::routes([("/keys", Reply::Status(500))]).await;
    let server = LoopbackServer::routes([(WELL_KNOWN, Reply::Status(500))]).await;
    let base = server.url("");
    server.set(
        WELL_KNOWN,
        Reply::Body(
            json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "jwks_uri": "https://evil.example.net/keys",
            })
            .to_string(),
        ),
    );
    let discovery = Arc::new(OidcDiscovery::new_loopback(&base).expect("discovery"));
    let verifier = JwksVerifier::from_discovery(discovery).expect("verifier");

    let token = rs256(&fixture().rsa_a, "key-a", &claims_for(&base));
    expect_unavailable(verifier.verify(&token, AUDIENCE).await);
    assert_eq!(evil.hits(), 0);
}

#[tokio::test]
async fn nothing_is_fetched_for_a_token_that_needs_no_keys() {
    let (server, discovery, _base) = discovered_provider().await;
    let verifier = JwksVerifier::from_discovery(discovery).expect("verifier");
    expect_invalid(verifier.verify("not-a-jwt", AUDIENCE).await);
    assert_eq!(
        server.hits(),
        0,
        "a malformed token is rejected before any fetch"
    );
}
