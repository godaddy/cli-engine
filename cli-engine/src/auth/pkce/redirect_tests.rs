use super::test_provider;

#[test]
fn redirect_url_template_default_uses_127_0_0_1_and_fixed_port() {
    let provider = test_provider().with_redirect_port(9000);
    let (url, requested_port) = provider.redirect_url_template().expect("valid template");
    assert_eq!(url.as_str(), "http://127.0.0.1:9000/callback");
    assert_eq!(requested_port, 9000);
    assert_eq!(url.path(), "/callback");
}

#[test]
fn redirect_url_template_uses_explicit_uri_override() {
    let provider = test_provider().with_redirect_uri("http://localhost:8080/auth/callback");
    let (url, requested_port) = provider.redirect_url_template().expect("valid template");
    assert_eq!(url.as_str(), "http://localhost:8080/auth/callback");
    assert_eq!(requested_port, 8080);
    assert_eq!(url.path(), "/auth/callback");
}

#[test]
fn redirect_url_template_ephemeral_requests_port_zero() {
    let provider = test_provider().with_ephemeral_redirect_port();
    let (url, requested_port) = provider.redirect_url_template().expect("valid template");
    assert_eq!(requested_port, 0);
    assert_eq!(url.as_str(), "http://127.0.0.1:0/callback");
}

/// The explicit `redirect_uri` override wins over an ephemeral-port request,
/// matching today's precedence over a fixed port — a config-selection
/// decision, so this needs no real bind.
#[test]
fn redirect_url_template_explicit_uri_overrides_ephemeral_mode() {
    let provider = test_provider()
        .with_ephemeral_redirect_port()
        .with_redirect_uri("http://localhost:8080/callback");
    let (url, requested_port) = provider.redirect_url_template().expect("valid template");
    assert_eq!(url.as_str(), "http://localhost:8080/callback");
    assert_eq!(requested_port, 8080);
}

#[test]
fn ephemeral_redirect_port_binds_os_assigned_port() {
    let provider = test_provider().with_ephemeral_redirect_port();
    let (listener, redirect_uri, callback_path) = provider
        .bind_callback_listener()
        .expect("ephemeral bind should succeed");
    let actual_port = listener
        .local_addr()
        .expect("listener has a local addr")
        .port();
    assert_ne!(actual_port, 0, "OS must assign a real port");
    assert_eq!(
        redirect_uri,
        format!("http://127.0.0.1:{actual_port}/callback")
    );
    assert_eq!(callback_path, "/callback");
}

/// A `with_redirect_uri` override whose bound port doesn't need to change
/// must come back byte-for-byte identical to what the caller configured —
/// not re-serialized from the parsed `Url`. Re-serializing a path-less URI
/// silently appends `/` (`url::Url` normalizes an empty path), which would
/// rewrite a previously-working, exactly-matched `redirect_uri` into one an
/// OAuth server no longer recognizes.
///
/// Uses an uncommon high port rather than something like 8080/3000/5000 —
/// those are common local dev-server defaults, and this test does perform a
/// real (if brief) bind, so picking a port nothing else is plausibly already
/// using matters here in exactly the way the module's own port-collision
/// history (the hardcoded 7443 default) already demonstrated.
#[test]
fn bind_callback_listener_preserves_path_less_override_verbatim() {
    let provider = test_provider().with_redirect_uri("http://localhost:48231");
    let (_listener, redirect_uri, _) = provider
        .bind_callback_listener()
        .expect("listener should bind");
    assert_eq!(redirect_uri, "http://localhost:48231");
}

/// Binding a `Fixed` port that's already occupied must surface a clear
/// error, not panic, hang, or silently succeed on some other port.
///
/// Deliberately keeps the occupying listener alive for the whole test,
/// rather than probing a free port and dropping it right before the real
/// bind: that drop-then-rebind pattern is a time-of-check/time-of-use race
/// — another process can grab the "freed" port in the gap, making the test
/// intermittently fail despite the port having been free a moment earlier.
/// Holding the port for the test's duration has no such gap. (The
/// success-path round-trip — a `Fixed`/`Ephemeral` real bind reporting back
/// the port it actually used — is already covered by
/// `ephemeral_redirect_port_binds_os_assigned_port`, since both modes share
/// the same bind-then-read-back code path; this test's job is the error
/// path that was previously untested.)
#[test]
fn fixed_redirect_port_bind_failure_is_a_clear_error() {
    let occupying_listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("OS should hand back a free port");
    let occupied_port = occupying_listener
        .local_addr()
        .expect("bound listener has a local addr")
        .port();

    let provider = test_provider().with_redirect_port(occupied_port);
    let err = provider
        .bind_callback_listener()
        .expect_err("binding an already-occupied port must fail, not silently succeed");
    let message = format!("{err}");
    assert!(
        message.contains(&occupied_port.to_string()),
        "error should name the port that failed to bind: {message}"
    );

    drop(occupying_listener);
}

/// The redirect URI may advertise "localhost" (RFC 8252 recommends it over a
/// literal IP), but the actual TCP bind always targets 127.0.0.1 regardless
/// — and an explicit `0` port in the override URI still gets a real port
/// substituted in.
#[test]
fn bind_callback_listener_binds_127_0_0_1_even_with_localhost_uri() {
    let provider = test_provider().with_redirect_uri("http://localhost:0/callback");
    let (listener, redirect_uri, callback_path) = provider
        .bind_callback_listener()
        .expect("listener should bind");
    assert_eq!(
        listener.local_addr().expect("addr").ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert!(redirect_uri.starts_with("http://localhost:"));
    assert!(!redirect_uri.ends_with(":0/callback"));
    assert_eq!(callback_path, "/callback");
}
