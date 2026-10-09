//! Shared `reqwest` client construction.
//!
//! Code that needs a plain [`reqwest::Client`] — progenitor-generated API
//! clients, hand-rolled multipart or streaming uploads, anything that cannot go
//! through [`super::HttpClient`] — builds it from [`reqwest_client_builder`] so
//! outbound policy (user-agent, timeouts, and future correlation headers) is
//! defined once in the engine instead of per call site.

use std::time::Duration;

use super::client_identity_snapshot;

/// Connect timeout applied to every client from [`reqwest_client_builder`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Idle read timeout applied to every client from [`reqwest_client_builder`].
///
/// This bounds the wait for each read (first response byte included), not the
/// whole request, so a large streaming download that keeps making progress is
/// never cut off.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Returns a [`reqwest::ClientBuilder`] preconfigured with the engine's
/// outbound policy.
///
/// - `User-Agent` is the process-wide default (see
///   [`set_default_user_agent`](super::set_default_user_agent)), read when this
///   function is called.
/// - The process-wide default headers (see
///   [`set_default_headers`](super::set_default_headers)) are sent on every
///   request, so attribution reaches every client built here. A later
///   `.default_headers(..)` on the returned builder adds to them; on a name
///   clash the later value wins.
/// - [`DEFAULT_CONNECT_TIMEOUT`] and [`DEFAULT_READ_TIMEOUT`] bound hangs.
///   There is deliberately no total-request timeout: callers that want one
///   (for example a short API call) set `.timeout(..)` on the returned builder,
///   and transfer-heavy callers keep the default.
///
/// The builder is returned unbuilt so callers can layer their own settings on
/// top (default headers, redirect policy, a per-client user-agent override).
/// Later settings win.
pub fn reqwest_client_builder() -> reqwest::ClientBuilder {
    let (user_agent, headers) = client_identity_snapshot();
    reqwest::Client::builder()
        .user_agent(user_agent)
        .default_headers(header_map(&headers))
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .read_timeout(DEFAULT_READ_TIMEOUT)
}

/// Converts default headers to a `HeaderMap`. Published defaults are already
/// validated; an entry that still fails to convert is skipped rather than
/// failing client construction.
fn header_map(headers: &std::collections::BTreeMap<String, String>) -> reqwest::header::HeaderMap {
    headers
        .iter()
        .filter_map(|(name, value)| {
            Some((
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).ok()?,
                reqwest::header::HeaderValue::from_str(value).ok()?,
            ))
        })
        .collect()
}

/// Builds a client from [`reqwest_client_builder`], falling back to a bare
/// client if the TLS backend fails to initialize.
pub(super) fn build_default_client() -> reqwest::Client {
    reqwest_client_builder()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use super::*;
    use crate::transport::client::{RestoreDefaultUserAgent, UA_TEST_LOCK};

    /// Serves one request on loopback and returns the raw request head.
    fn serve_once() -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}/", listener.local_addr().expect("local addr"));
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 4096];
            let read = stream.read(&mut buf).expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .expect("write response");
            String::from_utf8_lossy(&buf[..read]).into_owned()
        });
        (url, handle)
    }

    /// Builds a client while the process-default user-agent is `default_ua`.
    ///
    /// The user-agent is captured when the builder is created, so the global
    /// lock is released before any `.await` in the caller.
    fn build_with_default_ua(
        default_ua: &str,
        configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> reqwest::Client {
        let _guard = UA_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = RestoreDefaultUserAgent;
        super::super::set_default_user_agent(default_ua);
        configure(reqwest_client_builder())
            .build()
            .expect("client builds")
    }

    /// Builds a client while the process defaults are `ua` and `headers`.
    fn build_with_defaults(
        ua: &str,
        headers: &[(&str, &str)],
        configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> reqwest::Client {
        let _guard = UA_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = RestoreDefaultUserAgent;
        super::super::set_default_user_agent(ua);
        super::super::set_default_headers(
            headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        );
        configure(reqwest_client_builder())
            .build()
            .expect("client builds")
    }

    #[tokio::test]
    async fn default_headers_reach_the_wire() {
        let client = build_with_defaults("probe/1", &[("x-client-session", "abc123")], |b| b);
        let (url, server) = serve_once();

        client.get(&url).send().await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("x-client-session: abc123"), "{head}");
    }

    /// Generated clients add `Authorization` via `.default_headers(..)` on this
    /// builder; that must add to, not replace, the process defaults.
    #[tokio::test]
    async fn later_default_headers_add_to_process_defaults() {
        let client = build_with_defaults("probe/1", &[("x-client-session", "abc123")], |b| {
            let mut extra = reqwest::header::HeaderMap::new();
            extra.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_static("Bearer tok"),
            );
            b.default_headers(extra)
        });
        let (url, server) = serve_once();

        client.get(&url).send().await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("x-client-session: abc123"), "{head}");
        assert!(head.contains("authorization: bearer tok"), "{head}");
    }

    /// `HttpClient` layers its own headers over the process defaults: both are
    /// sent, and the client's value wins on a name clash.
    #[tokio::test]
    async fn http_client_merges_process_defaults_under_its_own_headers() {
        let (url, server) = serve_once();
        let client = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            super::super::set_default_headers(
                [("x-client-session", "process"), ("x-process-only", "1")]
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
            );
            crate::transport::HttpClientBuilder::new(
                url.trim_end_matches('/'),
                std::sync::Arc::new(crate::transport::NoopInjector),
            )
            .default_headers(
                [("x-client-session", "client".to_owned())]
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
            )
            .build()
        };

        client.get_bytes("/").await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("x-client-session: client"), "{head}");
        assert!(!head.contains("x-client-session: process"), "{head}");
        assert!(head.contains("x-process-only: 1"), "{head}");
    }

    /// The user-agent and default headers are published and read as a pair, so
    /// a concurrent reader can never see one publish's user-agent with
    /// another's headers.
    #[test]
    fn identity_pair_is_never_torn_across_concurrent_publishes() {
        let _guard = UA_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = RestoreDefaultUserAgent;
        let writers: Vec<_> = (0..4)
            .map(|id| {
                std::thread::spawn(move || {
                    for _ in 0..2_000 {
                        let tag = format!("app-{id}");
                        super::super::set_client_identity(
                            tag.clone(),
                            std::collections::BTreeMap::from([("x-app".to_owned(), tag)]),
                        );
                    }
                })
            })
            .collect();
        let reader = std::thread::spawn(|| {
            for _ in 0..20_000 {
                let (user_agent, headers) = client_identity_snapshot();
                if let Some(app) = headers.get("x-app") {
                    assert_eq!(&user_agent, app, "torn identity: {user_agent} / {app}");
                }
            }
        });

        for writer in writers {
            writer.join().expect("writer thread");
        }
        reader.join().expect("reader saw only consistent pairs");
    }

    #[test]
    fn set_default_headers_drops_invalid_entries_at_publish_time() {
        let _guard = UA_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = RestoreDefaultUserAgent;

        super::super::set_default_headers(
            [
                ("not a header", "x"),
                ("x-bad-value", "line\nbreak"),
                ("x-ok", "1"),
            ]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        );

        let kept = super::super::default_headers();
        assert_eq!(kept.keys().collect::<Vec<_>>(), vec!["x-ok"]);
    }

    /// An invalid process default must not make `HttpClient` requests fail at
    /// request-construction time.
    #[tokio::test]
    async fn http_client_requests_survive_an_invalid_process_default_header() {
        let (url, server) = serve_once();
        let client = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            super::super::set_default_headers(
                [("not a header", "x"), ("x-ok", "1")]
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
            );
            crate::transport::HttpClientBuilder::new(
                url.trim_end_matches('/'),
                std::sync::Arc::new(crate::transport::NoopInjector),
            )
            .build()
        };

        client
            .get_bytes("/")
            .await
            .expect("request is built despite the invalid default");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("x-ok: 1"), "{head}");
    }

    #[tokio::test]
    async fn invalid_default_header_is_skipped_not_fatal() {
        let client = build_with_defaults("probe/1", &[("not a header", "x"), ("x-ok", "1")], |b| b);
        let (url, server) = serve_once();

        client.get(&url).send().await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("x-ok: 1"), "{head}");
    }

    #[tokio::test]
    async fn builder_sends_the_process_default_user_agent() {
        let client = build_with_default_ua("factory-probe/1.2", |builder| builder);
        let (url, server) = serve_once();

        client.get(&url).send().await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("user-agent: factory-probe/1.2"), "{head}");
    }

    #[tokio::test]
    async fn caller_can_override_the_user_agent() {
        let client = build_with_default_ua("factory-probe/1.2", |builder| {
            builder.user_agent("override/9")
        });
        let (url, server) = serve_once();

        client.get(&url).send().await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("user-agent: override/9"), "{head}");
    }

    #[tokio::test]
    async fn stalled_server_hits_the_read_timeout_not_a_hang() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}/", listener.local_addr().expect("local addr"));
        // Accept and never respond; keep the socket open until the client gives up.
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            std::thread::sleep(Duration::from_millis(400));
            drop(stream);
        });
        let client = reqwest_client_builder()
            .read_timeout(Duration::from_millis(100))
            .build()
            .expect("client builds");

        let error = client.get(&url).send().await.expect_err("must time out");

        assert!(error.is_timeout(), "{error}");
        server.join().expect("server thread");
    }
}
