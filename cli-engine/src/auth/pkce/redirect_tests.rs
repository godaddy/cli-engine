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

/// `RedirectPort::Fixed` goes through the same bind + `local_addr()`
/// read-back as `Ephemeral` — this is a no-op echo for a concretely
/// requested port. Discovers a free port via a probe OS-assigned bind
/// (dropped immediately) rather than hardcoding one, so this test can't
/// collide with a port already in use on the machine or by another test —
/// exactly the class of collision we hit with the hardcoded 7443 default.
#[test]
fn fixed_redirect_port_round_trips_through_real_bind() {
    let free_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("OS should hand back a free port")
        .local_addr()
        .expect("bound listener has a local addr")
        .port();
    // Probe listener dropped here, releasing the port before the real bind.

    let provider = test_provider().with_redirect_port(free_port);
    let (listener, redirect_uri, _) = provider
        .bind_callback_listener()
        .expect("fixed port should still be free");
    assert_eq!(listener.local_addr().expect("addr").port(), free_port);
    assert_eq!(
        redirect_uri,
        format!("http://127.0.0.1:{free_port}/callback")
    );
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
