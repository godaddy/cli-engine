//! Shared `reqwest` client construction.
//!
//! Code that needs a plain [`reqwest::Client`] — progenitor-generated API
//! clients, hand-rolled multipart or streaming uploads, anything that cannot go
//! through [`super::HttpClient`] — builds it from [`reqwest_client_builder`] so
//! outbound policy (user-agent, timeouts, and future correlation headers) is
//! defined once in the engine instead of per call site.

use super::{client_identity_snapshot, timeouts::TimeoutPolicy};

/// Returns a [`reqwest::ClientBuilder`] preconfigured with the engine's
/// outbound policy.
///
/// - `User-Agent` is the process-wide default (see
///   [`set_default_user_agent`](super::set_default_user_agent)), and the
///   headers published by client attribution (see
///   [`AttributionConfig`](crate::transport::AttributionConfig)) are sent on
///   every request, so attribution reaches every client built here. A later
///   `.default_headers(..)` on the returned builder adds to them; on a name
///   clash the later value wins.
/// - Both are captured when this function is called, as one snapshot. Call it
///   from a command handler (after the `execute*` entrypoints have published
///   the identity), not during module registration, which runs earlier.
/// - The default [`TimeoutPolicy`] (5s connect, 30s read) bounds hangs. Each
///   call returns an independent builder, so a caller with several situations
///   (an API client, a long upload) calls [`reqwest_client_builder_with`] once
///   per situation with the policy that fits, and they all share the identity.
///   There is deliberately no total-request timeout: callers that want one set
///   `.timeout(..)` on the returned builder.
///
/// The builder is returned unbuilt so callers can layer their own settings on
/// top (default headers, redirect policy, a per-client user-agent override).
/// Later settings win.
pub fn reqwest_client_builder() -> reqwest::ClientBuilder {
    reqwest_client_builder_with(TimeoutPolicy::default())
}

/// Like [`reqwest_client_builder`], with an explicit [`TimeoutPolicy`] in place
/// of the default one.
///
/// A policy can disable a timeout (`None`), which `reqwest` itself cannot undo
/// once set, so choose the policy here rather than trying to remove a timeout
/// from the builder afterwards.
pub fn reqwest_client_builder_with(timeouts: TimeoutPolicy) -> reqwest::ClientBuilder {
    let (user_agent, headers) = client_identity_snapshot();
    // The user-agent goes last: `reqwest` stores it as a default header, so
    // applying it after the header map keeps a `user-agent` entry in that map
    // from replacing the identity, matching how `HttpClient` ranks them.
    timeouts
        .apply(reqwest::Client::builder())
        .default_headers(header_map(&headers))
        .user_agent(user_agent)
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

/// Builds a client carrying only `timeouts`, with no process identity.
///
/// [`super::HttpClient`] applies its user-agent and default headers per
/// request from the snapshot its builder captured, so its base clients must not
/// read the process-wide identity a second time. Falls back to a bare client if
/// the TLS backend fails to initialize.
pub(super) fn build_client(timeouts: TimeoutPolicy) -> reqwest::Client {
    timeouts
        .apply(reqwest::Client::builder())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, time::Duration};

    use super::*;
    use crate::transport::client::{
        RestoreDefaultUserAgent, UA_TEST_LOCK, test_support::serve_once,
    };

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
        let client = build_with_defaults("probe/1", &[("x-client-session-id", "abc123")], |b| b);
        let (url, server) = serve_once();

        client.get(&url).send().await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("x-client-session-id: abc123"), "{head}");
    }

    /// Generated clients add `Authorization` via `.default_headers(..)` on this
    /// builder; that must add to, not replace, the process defaults.
    #[tokio::test]
    async fn later_default_headers_add_to_process_defaults() {
        let client = build_with_defaults("probe/1", &[("x-client-session-id", "abc123")], |b| {
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
        assert!(head.contains("x-client-session-id: abc123"), "{head}");
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
                [("x-client-session-id", "process"), ("x-process-only", "1")]
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
            );
            crate::transport::HttpClientBuilder::new(
                url.trim_end_matches('/'),
                std::sync::Arc::new(crate::transport::NoopInjector),
            )
            .default_headers(
                [("x-client-session-id", "client".to_owned())]
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
            )
            .build()
        };

        client.get_bytes("/").await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("x-client-session-id: client"), "{head}");
        assert!(!head.contains("x-client-session-id: process"), "{head}");
        assert!(head.contains("x-process-only: 1"), "{head}");
    }

    /// A default header never duplicates or overrides a header the request
    /// itself sets: `Content-Type` and `User-Agent` are request-owned.
    #[tokio::test]
    async fn default_headers_do_not_duplicate_request_owned_headers() {
        let (url, server) = serve_once();
        let client = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            super::super::set_default_headers(
                [
                    ("content-type".to_owned(), "text/plain".to_owned()),
                    ("user-agent".to_owned(), "sneaky/0".to_owned()),
                    ("x-extra".to_owned(), "1".to_owned()),
                ]
                .into(),
            );
            crate::transport::HttpClientBuilder::new(
                url.trim_end_matches('/'),
                std::sync::Arc::new(crate::transport::NoopInjector),
            )
            .user_agent("real/1")
            .build()
        };

        client
            .post_without_response("/", &serde_json::json!({"a": 1}))
            .await
            .expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert_eq!(head.matches("content-type:").count(), 1, "{head}");
        assert!(head.contains("content-type: application/json"), "{head}");
        assert_eq!(head.matches("user-agent:").count(), 1, "{head}");
        assert!(head.contains("user-agent: real/1"), "{head}");
        assert!(head.contains("x-extra: 1"), "{head}");
    }

    /// Replacing the user-agent invalidates the published pair, so it must not
    /// leave a previous execution's attribution headers behind.
    #[test]
    fn setting_the_user_agent_clears_previously_published_headers() {
        let _guard = UA_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = RestoreDefaultUserAgent;
        super::super::set_client_identity(
            "cli-a/1".to_owned(),
            [(
                "x-client-session-id".to_owned(),
                "a-session-hash".to_owned(),
            )]
            .into(),
        );

        super::super::set_default_user_agent("cli-b/2");

        let (user_agent, headers) = client_identity_snapshot();
        assert_eq!(user_agent, "cli-b/2");
        assert!(headers.is_empty(), "stale headers survived: {headers:?}");
    }

    /// A `user-agent` entry among the published headers must not replace the
    /// identity user-agent on clients from the factory.
    #[tokio::test]
    async fn header_map_cannot_override_the_identity_user_agent() {
        let client = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            super::super::set_client_identity(
                "identity/1".to_owned(),
                [("user-agent".to_owned(), "sneaky/0".to_owned())].into(),
            );
            reqwest_client_builder().build().expect("client builds")
        };
        let (url, server) = serve_once();

        client.get(&url).send().await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert_eq!(head.matches("user-agent:").count(), 1, "{head}");
        assert!(head.contains("user-agent: identity/1"), "{head}");
    }

    /// An explicit per-request precondition must survive a conflicting client
    /// default: `If-Match: *` would otherwise discard the caller's ETag and
    /// permit a lost update.
    #[tokio::test]
    async fn own_default_cannot_replace_an_explicit_if_match() {
        let (url, server) = serve_once();
        let client = crate::transport::HttpClientBuilder::new(
            url.trim_end_matches('/'),
            std::sync::Arc::new(crate::transport::NoopInjector),
        )
        .default_headers([("If-Match".to_owned(), "*".to_owned())].into())
        .build();

        client
            .put_if_match_without_response("/", &serde_json::json!({"a": 1}), "\"v7\"")
            .await
            .expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert_eq!(head.matches("if-match:").count(), 1, "{head}");
        assert!(head.contains("if-match: \"v7\""), "{head}");
    }

    /// A client's own default `Content-Type` must not replace a multipart
    /// request's generated `Content-Type`: that header carries the boundary
    /// the server needs to parse the body.
    #[tokio::test]
    async fn own_default_content_type_does_not_strip_the_multipart_boundary() {
        let upload = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(upload.path(), b"file-bytes").expect("write upload");
        let (url, server) = serve_once();
        let client = crate::transport::HttpClientBuilder::new(
            url.trim_end_matches('/'),
            std::sync::Arc::new(crate::transport::NoopInjector),
        )
        .default_headers(
            [(
                "Content-Type".to_owned(),
                "application/vnd.test+json".to_owned(),
            )]
            .into(),
        )
        .build();

        client
            .post_multipart_without_response("/", "file", upload.path())
            .await
            .expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert_eq!(head.matches("content-type:").count(), 1, "{head}");
        assert!(
            head.contains("content-type: multipart/form-data; boundary="),
            "{head}"
        );
    }

    /// The client's own default `Content-Type` replaces the JSON one, exactly
    /// once (a client opting into a vendor media type), while a process-wide
    /// default of the same name does not.
    #[tokio::test]
    async fn own_default_content_type_replaces_json_once() {
        let (url, server) = serve_once();
        let client = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            super::super::set_default_headers(
                [("content-type".to_owned(), "text/plain".to_owned())].into(),
            );
            crate::transport::HttpClientBuilder::new(
                url.trim_end_matches('/'),
                std::sync::Arc::new(crate::transport::NoopInjector),
            )
            .default_headers(
                [(
                    "Content-Type".to_owned(),
                    "application/vnd.test+json".to_owned(),
                )]
                .into(),
            )
            .build()
        };

        client
            .post_without_response("/", &serde_json::json!({"a": 1}))
            .await
            .expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert_eq!(head.matches("content-type:").count(), 1, "{head}");
        assert!(
            head.contains("content-type: application/vnd.test+json"),
            "{head}"
        );
    }

    /// A client's `X-Client-Session-Id` replaces a process default `x-client-session-id`
    /// (header names are case-insensitive); only the client's value is sent.
    #[tokio::test]
    async fn http_client_header_override_is_case_insensitive() {
        let (url, server) = serve_once();
        let client = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            super::super::set_default_headers(
                [("x-client-session-id".to_owned(), "process".to_owned())].into(),
            );
            crate::transport::HttpClientBuilder::new(
                url.trim_end_matches('/'),
                std::sync::Arc::new(crate::transport::NoopInjector),
            )
            .default_headers([("X-Client-Session-Id".to_owned(), "client".to_owned())].into())
            .build()
        };

        client.get_bytes("/").await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert_eq!(head.matches("x-client-session-id:").count(), 1, "{head}");
        assert!(head.contains("x-client-session-id: client"), "{head}");
    }

    /// `HttpClient` uses the identity captured when its builder was created;
    /// publishing a different identity before `build()` must not leak into it.
    #[tokio::test]
    async fn http_client_does_not_resnapshot_identity_at_build_time() {
        let (url, server) = serve_once();
        let client = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            super::super::set_client_identity(
                "app-a/1".to_owned(),
                [("x-app".to_owned(), "a".to_owned())].into(),
            );
            let builder = crate::transport::HttpClientBuilder::new(
                url.trim_end_matches('/'),
                std::sync::Arc::new(crate::transport::NoopInjector),
            );
            super::super::set_client_identity(
                "app-b/2".to_owned(),
                [("x-app-b".to_owned(), "b".to_owned())].into(),
            );
            builder.build()
        };

        client.get_bytes("/").await.expect("request succeeds");

        let head = server.join().expect("server thread").to_lowercase();
        assert!(head.contains("user-agent: app-a/1"), "{head}");
        assert!(head.contains("x-app: a"), "{head}");
        assert!(!head.contains("app-b"), "{head}");
        assert!(!head.contains("x-app-b"), "{head}");
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
        let client = reqwest_client_builder_with(
            TimeoutPolicy::default().with_read(Some(Duration::from_millis(100))),
        )
        .build()
        .expect("client builds");

        let error = client.get(&url).send().await.expect_err("must time out");

        assert!(error.is_timeout(), "{error}");
        server.join().expect("server thread");
    }
}
