//! Redirect-URI and loopback-listener selection for the PKCE login flow.
//!
//! Split out of `mod.rs` so the provider module stays under the file-size cap.
//! [`PkceAuthProvider`](super::PkceAuthProvider)'s `with_redirect_port`,
//! `with_ephemeral_redirect_port` and `with_redirect_uri` builders choose the
//! inputs; these functions turn them into a bound listener and a redirect URI.

use std::net::TcpListener;

use crate::{Result, error::CliCoreError};

pub(super) const REDIRECT_PORT_DEFAULT: u16 = 7443;

/// How the local callback listener's port is chosen. Private: callers select
/// a variant through `PkceAuthProvider::with_redirect_port` or
/// `PkceAuthProvider::with_ephemeral_redirect_port`, not by naming this type
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RedirectPort {
    /// Bind exactly this port; `TcpListener::bind` fails if it's taken.
    Fixed(u16),
    /// Bind port `0` and let the OS assign a free loopback port (RFC 8252
    /// §7.3 loopback interface redirection). Opt-in — see
    /// `PkceAuthProvider::with_ephemeral_redirect_port`.
    Ephemeral,
}

/// Decides the redirect-URI template and the port to request from
/// `TcpListener::bind`, without touching a socket.
///
/// Precedence, highest first: an explicit `redirect_uri` override, then the
/// configured [`RedirectPort`]. The returned port is `0` for
/// [`RedirectPort::Ephemeral`] — this function alone cannot know the real port;
/// only binding and reading it back (see [`bind_callback_listener`]) can.
pub(super) fn redirect_url_template(
    redirect_uri: Option<&str>,
    redirect_port: RedirectPort,
) -> Result<(url::Url, u16)> {
    if let Some(uri) = redirect_uri {
        let parsed = url::Url::parse(uri)
            .map_err(|e| CliCoreError::message(format!("invalid redirect URI '{uri}': {e}")))?;
        let port = parsed
            .port()
            .or_else(|| parsed.port_or_known_default())
            .ok_or_else(|| CliCoreError::message(format!("redirect URI '{uri}' has no port")))?;
        return Ok((parsed, port));
    }
    let requested_port = match redirect_port {
        RedirectPort::Fixed(port) => port,
        RedirectPort::Ephemeral => 0,
    };
    let url = url::Url::parse(&format!("http://127.0.0.1:{requested_port}/callback"))
        .map_err(|e| CliCoreError::message(format!("invalid default redirect URI: {e}")))?;
    Ok((url, requested_port))
}

/// Binds the local callback listener and returns it together with the exact
/// redirect URI that must be sent both at authorization and at token exchange
/// (RFC 6749 §4.1.3 requires the same value at both steps) and the path to
/// match the incoming callback request against.
///
/// The bound listener's actual port — read back via `TcpListener::local_addr` —
/// always becomes the URI's port. For a [`RedirectPort::Fixed`] port, or an
/// explicit non-zero port in `redirect_uri`, this is just an echo of the port
/// that was requested (bind already fails if that port is taken, so it can't
/// differ). For [`RedirectPort::Ephemeral`] — or a `redirect_uri` that itself
/// names port `0` — it's the only way to learn the port at all.
///
/// Whenever the bound port matches what was requested, an explicit
/// `redirect_uri` override is returned **verbatim**, byte-for-byte, rather than
/// re-serialized from the parsed `Url` — re-serializing can silently rewrite it
/// (an empty path becomes `/`, an explicit port matching the scheme's default
/// gets dropped), and OAuth servers commonly match `redirect_uri` exactly, so a
/// rewrite the caller never asked for can turn a previously-working
/// configuration into an `invalid_grant`/`invalid_request` failure.
pub(super) fn bind_callback_listener(
    redirect_uri: Option<&str>,
    redirect_port: RedirectPort,
) -> Result<(TcpListener, String, String)> {
    let (redirect_url, requested_port) = redirect_url_template(redirect_uri, redirect_port)?;

    let listener = TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], requested_port)))
        .map_err(|err| {
            CliCoreError::message(format!(
                "failed to bind callback server on port {requested_port}: {err}"
            ))
        })?;
    let actual_port = listener
        .local_addr()
        .map_err(|err| CliCoreError::message(format!("failed to read bound callback port: {err}")))?
        .port();

    if let Some(uri) = redirect_uri
        && actual_port == requested_port
    {
        let callback_path = redirect_url.path().to_owned();
        return Ok((listener, uri.to_owned(), callback_path));
    }

    let mut redirect_url = redirect_url;
    redirect_url.set_port(Some(actual_port)).map_err(|()| {
        CliCoreError::message(format!("redirect URI '{redirect_url}' cannot carry a port"))
    })?;
    let callback_path = redirect_url.path().to_owned();
    Ok((listener, redirect_url.to_string(), callback_path))
}
