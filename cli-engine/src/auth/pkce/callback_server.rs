use std::{io::Write, net::TcpListener, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::{Result, error::CliCoreError};

pub(super) fn emit_browser_login_prompt(url: &url::Url) {
    let mut stderr = std::io::stderr().lock();
    drop(writeln!(stderr, "Opening browser for authentication…"));
    drop(writeln!(
        stderr,
        "If the browser does not open, visit:\n  {url}"
    ));
}

// Printed once the OAuth token is in hand, so a caller who goes on to run a
// long-running command (e.g. a purchase) doesn't mistake that work for the
// browser flow still being pending. See DEVEX-892 / GDDEVPLAT-64.
pub(super) fn emit_auth_complete_message() {
    let mut stderr = std::io::stderr().lock();
    drop(writeln!(stderr, "Authentication complete."));
}

/// Generates a PKCE code verifier and SHA-256 code challenge.
pub(super) fn pkce_challenge() -> (String, String) {
    let bytes: [u8; 32] = rand::rng().random();
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);
    (verifier, challenge)
}

/// Generates a random OAuth state parameter.
pub(super) fn random_state() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Waits for the OAuth callback on the given listener, validates state and path.
///
/// Accepts connections in a loop so that stray connections (port scanners,
/// browser preflight requests) do not consume the single callback attempt.
/// Uses async I/O so the future is properly cancelled on Ctrl+C.
pub(super) async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    expected_path: &str,
    timeout: Duration,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    listener
        .set_nonblocking(true)
        .map_err(|err| CliCoreError::message(format!("callback server setup failed: {err}")))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|err| CliCoreError::message(format!("callback server setup failed: {err}")))?;

    let expected_state = expected_state.to_owned();
    let expected_path = expected_path.to_owned();
    let result = tokio::time::timeout(timeout, async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => {
                    // Back off before retrying so a persistent accept failure
                    // (e.g. file-descriptor exhaustion) cannot spin the CPU until
                    // the timeout fires. The sleep is an await point, so Ctrl+C
                    // still cancels the flow promptly.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let mut buf = vec![0_u8; 4096];
            let n = match stream.read(&mut buf).await {
                Ok(0) | Err(_) => continue,
                Ok(n) => n,
            };
            let request = String::from_utf8_lossy(&buf[..n]);

            if extract_request_path(&request).as_deref() != Some(expected_path.as_str()) {
                drop(
                    stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await,
                );
                continue;
            }

            let code = extract_query_param(&request, "code");
            let state = extract_query_param(&request, "state");

            let html_response = if state.as_deref() == Some(&expected_state) && code.is_some() {
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                 <html><body>Authentication successful. You may close this window.</body></html>"
            } else {
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n\
                 <html><body>Authentication failed. Please try again.</body></html>"
            };
            drop(stream.write_all(html_response.as_bytes()).await);

            if state.as_deref() == Some(expected_state.as_str()) {
                return code
                    .ok_or_else(|| CliCoreError::message("no authorization code in callback"));
            }
        }
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(CliCoreError::message(
            "timed out waiting for OAuth callback",
        )),
    }
}

/// Extracts the path component from an HTTP request line (without query string).
pub(super) fn extract_request_path(request: &str) -> Option<String> {
    let line = request.lines().next()?;
    let path_with_query = line.split_whitespace().nth(1)?;
    Some(
        path_with_query
            .split_once('?')
            .map_or(path_with_query, |(p, _)| p)
            .to_owned(),
    )
}

/// Extracts a query parameter value from an HTTP request line.
pub(super) fn extract_query_param(request: &str, name: &str) -> Option<String> {
    let line = request.lines().next()?;
    let path = line.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}
