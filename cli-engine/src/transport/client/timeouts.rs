//! Timeout policy for outbound HTTP clients.

use std::time::Duration;

/// Default connect timeout of [`TimeoutPolicy::default`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default read timeout of [`TimeoutPolicy::default`].
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Connect and read timeouts for an HTTP client. `None` disables a timeout.
///
/// - **Connect** bounds establishing the connection and cannot affect a
///   transfer that is already under way.
/// - **Read** bounds each wait for data from the server and resets after every
///   successful read, so a large download that keeps making progress is never
///   cut off. Note that for the *response headers* the wait starts when the
///   request starts and is not reset by upload progress: a request whose
///   response only arrives after a long upload needs a policy with no read
///   timeout (see [`TimeoutPolicy::uploads`]).
///
/// There is deliberately no total-request timeout here; callers that want one
/// set `.timeout(..)` on a `reqwest` builder or request.
///
/// The default is a 5s connect timeout and a 30s read timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeoutPolicy {
    connect: Option<Duration>,
    read: Option<Duration>,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            connect: Some(DEFAULT_CONNECT_TIMEOUT),
            read: Some(DEFAULT_READ_TIMEOUT),
        }
    }
}

impl TimeoutPolicy {
    /// A policy with neither timeout.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            connect: None,
            read: None,
        }
    }

    /// The policy for uploads: the default connect timeout and no read
    /// timeout, because the server may legitimately respond only after the
    /// whole body has been sent.
    #[must_use]
    pub const fn uploads() -> Self {
        Self {
            connect: Some(DEFAULT_CONNECT_TIMEOUT),
            read: None,
        }
    }

    /// Sets the connect timeout; `None` disables it.
    #[must_use]
    pub const fn with_connect(mut self, connect: Option<Duration>) -> Self {
        self.connect = connect;
        self
    }

    /// Sets the read timeout; `None` disables it.
    #[must_use]
    pub const fn with_read(mut self, read: Option<Duration>) -> Self {
        self.read = read;
        self
    }

    /// The connect timeout, if any.
    #[must_use]
    pub const fn connect(&self) -> Option<Duration> {
        self.connect
    }

    /// The read timeout, if any.
    #[must_use]
    pub const fn read(&self) -> Option<Duration> {
        self.read
    }

    /// Applies this policy to a `reqwest` builder, leaving a disabled timeout
    /// unset.
    pub fn apply(self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        if let Some(connect) = self.connect {
            builder = builder.connect_timeout(connect);
        }
        if let Some(read) = self.read {
            builder = builder.read_timeout(read);
        }
        builder
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::transport::{
        HttpClient, HttpClientBuilder, NoopInjector,
        client::{
            RestoreDefaultUserAgent, UA_TEST_LOCK, reqwest_client_builder_with,
            set_client_identity, test_support::serve_once_after,
        },
    };

    const SHORT: Duration = Duration::from_millis(100);
    const SLOW_RESPONSE: Duration = Duration::from_millis(500);

    fn client(
        url: &str,
        configure: impl FnOnce(HttpClientBuilder) -> HttpClientBuilder,
    ) -> HttpClient {
        configure(HttpClientBuilder::new(
            url.trim_end_matches('/'),
            Arc::new(NoopInjector),
        ))
        .build()
    }

    fn upload_file() -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), b"file-bytes").expect("write upload");
        file
    }

    #[test]
    fn default_is_five_second_connect_and_thirty_second_read() {
        let policy = TimeoutPolicy::default();

        assert_eq!(policy.connect(), Some(Duration::from_secs(5)));
        assert_eq!(policy.read(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn unbounded_disables_both() {
        let policy = TimeoutPolicy::unbounded();

        assert_eq!((policy.connect(), policy.read()), (None, None));
    }

    #[test]
    fn uploads_keeps_connect_and_drops_read() {
        let policy = TimeoutPolicy::uploads();

        assert_eq!(policy.connect(), Some(DEFAULT_CONNECT_TIMEOUT));
        assert_eq!(policy.read(), None);
    }

    #[test]
    fn setters_replace_or_disable_each_timeout_independently() {
        let policy = TimeoutPolicy::default()
            .with_connect(Some(Duration::from_secs(1)))
            .with_read(None);

        assert_eq!(policy.connect(), Some(Duration::from_secs(1)));
        assert_eq!(policy.read(), None);
    }

    #[tokio::test]
    async fn factory_client_with_no_read_timeout_waits_for_a_slow_response() {
        let (url, server) = serve_once_after(SLOW_RESPONSE);
        let http = reqwest_client_builder_with(TimeoutPolicy::default().with_read(None))
            .build()
            .expect("client builds");

        http.get(&url).send().await.expect("slow response arrives");

        server.join().expect("server thread");
    }

    /// Asserts `outcome` failed because the read timeout fired: the error came
    /// back well before the slow response could have, so it cannot be the
    /// response arriving. (`reqwest` keeps the timeout in the error's source
    /// chain, which `HttpClient` flattens away, so the message can't say so.)
    fn assert_timed_out_early<T: std::fmt::Debug>(
        outcome: crate::Result<T>,
        started: std::time::Instant,
    ) {
        let error = outcome.expect_err("a response slower than the read timeout fails");
        assert!(
            error.to_string().starts_with("transport: POST /"),
            "{error}"
        );
        assert!(
            started.elapsed() < SLOW_RESPONSE - SHORT,
            "failed only after {:?}, so it was not the read timeout",
            started.elapsed()
        );
    }

    /// POST is used because `HttpClient` retries idempotent methods on network
    /// errors, which would blur the timing.
    #[tokio::test]
    async fn ordinary_requests_honor_the_configured_read_timeout() {
        let (url, server) = serve_once_after(SLOW_RESPONSE);
        let http = client(&url, |b| {
            b.timeouts(TimeoutPolicy::default().with_read(Some(SHORT)))
        });
        let started = std::time::Instant::now();

        let outcome = http
            .post_without_response("/", &serde_json::json!({"a": 1}))
            .await;

        assert_timed_out_early(outcome, started);
        server.join().expect("server thread");
    }

    /// A situation that needs different timeouts gets its own client; here an
    /// upload to a server that responds only after a long wait.
    #[tokio::test]
    async fn a_client_built_with_the_uploads_policy_is_not_cut_off() {
        let upload = upload_file();
        let (url, server) = serve_once_after(SLOW_RESPONSE);
        let http = client(&url, |b| b.timeouts(TimeoutPolicy::uploads()));

        http.post_multipart_without_response("/", "file", upload.path())
            .await
            .expect("no read timeout, so the slow response is awaited");

        server.join().expect("server thread");
    }

    /// There is no hidden exemption: a multipart request obeys its client's
    /// policy like any other.
    #[tokio::test]
    async fn multipart_uploads_obey_their_clients_policy() {
        let upload = upload_file();
        let (url, server) = serve_once_after(SLOW_RESPONSE);
        let http = client(&url, |b| {
            b.timeouts(TimeoutPolicy::default().with_read(Some(SHORT)))
        });
        let started = std::time::Instant::now();

        let outcome = http
            .post_multipart_without_response("/", "file", upload.path())
            .await;

        assert_timed_out_early(outcome, started);
        server.join().expect("server thread");
    }

    /// Several situations, several policies, one identity: nothing about the
    /// user-agent or headers is configured per client.
    #[tokio::test]
    async fn clients_for_different_situations_share_identity_but_not_timeouts() {
        let (api, upload) = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            set_client_identity(
                "shared/1".to_owned(),
                [("x-shared".to_owned(), "yes".to_owned())].into(),
            );
            let (api_url, api_server) = serve_once_after(SLOW_RESPONSE);
            let (upload_url, upload_server) = serve_once_after(SLOW_RESPONSE);
            let api = (
                client(&api_url, |b| {
                    b.timeouts(TimeoutPolicy::default().with_read(Some(SHORT)))
                }),
                api_server,
            );
            let upload = (
                client(&upload_url, |b| b.timeouts(TimeoutPolicy::uploads())),
                upload_server,
            );
            (api, upload)
        };
        let started = std::time::Instant::now();

        let api_outcome = api
            .0
            .post_without_response("/", &serde_json::json!({"a": 1}))
            .await;
        // Judge the first request before the second one's wait is added.
        assert_timed_out_early(api_outcome, started);

        upload
            .0
            .post_without_response("/", &serde_json::json!({"a": 1}))
            .await
            .expect("the upload-situation client waits for the response");
        for head in [
            api.1.join().expect("api server"),
            upload.1.join().expect("upload server"),
        ] {
            let head = head.to_lowercase();
            assert!(head.contains("user-agent: shared/1"), "{head}");
            assert!(head.contains("x-shared: yes"), "{head}");
        }
    }

    /// The same holds for plain `reqwest` consumers: each call mints an
    /// independent builder with its own timeouts and the shared identity.
    #[tokio::test]
    async fn factory_builders_per_situation_share_identity_but_not_timeouts() {
        let (short, patient) = {
            let _guard = UA_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _restore = RestoreDefaultUserAgent;
            set_client_identity(
                "shared/2".to_owned(),
                [("x-shared".to_owned(), "yes".to_owned())].into(),
            );
            (
                reqwest_client_builder_with(TimeoutPolicy::default().with_read(Some(SHORT)))
                    .build()
                    .expect("short client"),
                reqwest_client_builder_with(TimeoutPolicy::unbounded())
                    .build()
                    .expect("patient client"),
            )
        };
        let (short_url, short_server) = serve_once_after(SLOW_RESPONSE);
        let (patient_url, patient_server) = serve_once_after(SLOW_RESPONSE);

        let short_error = short
            .get(&short_url)
            .send()
            .await
            .expect_err("slower than its read timeout");
        patient
            .get(&patient_url)
            .send()
            .await
            .expect("this builder has no read timeout");

        assert!(short_error.is_timeout(), "{short_error}");
        for head in [
            short_server.join().expect("short server"),
            patient_server.join().expect("patient server"),
        ] {
            let head = head.to_lowercase();
            assert!(head.contains("user-agent: shared/2"), "{head}");
            assert!(head.contains("x-shared: yes"), "{head}");
        }
    }
}
