//! Stderr sink for transport debug events.
//!
//! [`StderrTransportLogger`] renders [`TransportLogEvent`]s as a curl-style
//! request/response trace on stderr. It is the logger the CLI installs when
//! `--debug` selects the `transport` component. Sensitive headers are redacted,
//! but URLs and request/response bodies are printed in full and may still
//! contain secrets — treat the output as sensitive before sharing it.

use std::io::Write;

use super::client::{TransportLogEvent, TransportLogger};

/// Header names whose values are replaced with `<redacted>` in the dump.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
];

const REDACTED: &str = "<redacted>";

/// Transport logger that prints a redacted, curl-style HTTP trace to stderr.
///
/// Outbound requests are prefixed with `>` and responses with `<`:
///
/// ```text
/// > POST https://api.example.com/v3/repos
/// > authorization: <redacted>
/// > content-type: application/json
/// >
/// > {"name":"foo"}
///
/// < 200 POST https://api.example.com/v3/repos
/// < content-type: application/json
/// <
/// < {"id":"repo-1"}
/// ```
///
/// Bodies are printed for JSON/decode paths; raw byte-download responses report
/// only their size. Sensitive header values (`authorization`,
/// `proxy-authorization`, `cookie`, `set-cookie`, `x-api-key`) are redacted.
#[derive(Clone, Debug, Default)]
pub struct StderrTransportLogger {
    /// Extra header names to redact, in addition to the built-in set. Stored
    /// lowercased; matching is case-insensitive.
    extra_redacted: Vec<String>,
}

impl StderrTransportLogger {
    /// Creates a stderr transport logger that redacts the built-in sensitive
    /// header set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds header names to redact on top of the built-in set.
    ///
    /// Use this for CLI-specific secret-bearing headers that are not standard
    /// auth headers — for example a custom API-key header an auth injector sets.
    /// Names are matched case-insensitively. Additive only: the built-in set
    /// (`authorization`, `proxy-authorization`, `cookie`, `set-cookie`,
    /// `x-api-key`) is always redacted. Names are trimmed and empty entries are
    /// dropped, so a stray-whitespace config value cannot silently fail to
    /// match (which would leak the header).
    #[must_use]
    pub fn with_redacted_headers(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.extra_redacted
            .extend(names.into_iter().filter_map(|name| {
                let name = name.into().trim().to_ascii_lowercase();
                (!name.is_empty()).then_some(name)
            }));
        self
    }

    fn is_sensitive(&self, name: &str) -> bool {
        SENSITIVE_HEADERS
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
            || self
                .extra_redacted
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
    }

    /// Renders a single transport event into its stderr representation.
    ///
    /// Kept private (not `pub`) so unit tests can assert on the formatted text
    /// without capturing the process stderr stream.
    fn format_event(&self, event: &TransportLogEvent) -> String {
        match event.message {
            "http request" => {
                let method = field(event, "method").unwrap_or("?");
                let url = field(event, "url").unwrap_or("?");
                let mut out = format!("> {method} {url}\n");
                self.append_headers(&mut out, ">", event);
                append_body(&mut out, ">", event);
                out
            }
            "http response" => {
                // Method/url are absent on responses logged via the
                // reqwest-direct helper, so omit them rather than rendering
                // trailing spaces (`< 200  `).
                let status = field(event, "status").unwrap_or("?");
                let suffix = match (field(event, "method"), field(event, "url")) {
                    (Some(method), Some(url)) => format!(" {method} {url}"),
                    (Some(value), None) | (None, Some(value)) => format!(" {value}"),
                    (None, None) => String::new(),
                };
                let mut out = format!("< {status}{suffix}\n");
                self.append_headers(&mut out, "<", event);
                append_body(&mut out, "<", event);
                out
            }
            "retrying request" => {
                let attempt = field(event, "attempt").unwrap_or("?");
                let backoff = field(event, "backoff").unwrap_or("?");
                format!("* retrying (attempt {attempt}, backoff {backoff})\n")
            }
            other => {
                let mut out = format!("* {other}");
                for (key, value) in &event.fields {
                    out.push_str(&format!(" {key}={value}"));
                }
                out.push('\n');
                out
            }
        }
    }

    fn append_headers(&self, out: &mut String, prefix: &str, event: &TransportLogEvent) {
        if let Some(headers) = &event.headers {
            for (name, value) in headers {
                let shown = if self.is_sensitive(name) {
                    REDACTED
                } else {
                    value
                };
                out.push_str(&format!("{prefix} {name}: {shown}\n"));
            }
        }
    }
}

impl TransportLogger for StderrTransportLogger {
    fn debug(&self, event: &TransportLogEvent) {
        let rendered = self.format_event(event);
        if rendered.is_empty() {
            return;
        }
        // Write directly to a locked stderr handle (not `eprintln!`) so the
        // whole event lands as one contiguous block under concurrency.
        // Diagnostics are best-effort: ignore write failures rather than break
        // the command. (`let _ =` would trip the crate's `let_underscore_drop`
        // lint, so use `.ok()` to discard the result.)
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(rendered.as_bytes()).ok();
    }
}

fn field<'event>(event: &'event TransportLogEvent, key: &str) -> Option<&'event str> {
    event.fields.get(key).map(String::as_str)
}

fn append_body(out: &mut String, prefix: &str, event: &TransportLogEvent) {
    if let Some(body) = &event.body {
        if body.is_empty() {
            return;
        }
        out.push_str(&format!("{prefix}\n"));
        for line in String::from_utf8_lossy(body).lines() {
            out.push_str(&format!("{prefix} {line}\n"));
        }
    } else if let Some(size) = field(event, "body_bytes")
        && size != "0"
    {
        out.push_str(&format!("{prefix} [body: {size} bytes not captured]\n"));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{REDACTED, StderrTransportLogger};
    use crate::transport::client::TransportLogEvent;

    fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn request_event_redacts_sensitive_headers_and_prints_body() {
        let event = TransportLogEvent {
            message: "http request",
            fields: fields(&[("method", "POST"), ("url", "https://api.example.com/repos")]),
            headers: Some(vec![
                ("authorization".to_owned(), "Bearer super-secret".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ]),
            body: Some(br#"{"name":"foo"}"#.to_vec()),
        };
        let rendered = StderrTransportLogger::new().format_event(&event);
        assert!(rendered.contains("> POST https://api.example.com/repos"));
        assert!(rendered.contains(&format!("> authorization: {REDACTED}")));
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("> content-type: application/json"));
        assert!(rendered.contains(r#"> {"name":"foo"}"#));
    }

    #[test]
    fn response_event_with_size_only_reports_byte_count() {
        let event = TransportLogEvent {
            message: "http response",
            fields: fields(&[
                ("status", "200"),
                ("method", "GET"),
                ("url", "https://api.example.com/blob"),
                ("body_bytes", "2048"),
            ]),
            headers: Some(vec![("set-cookie".to_owned(), "session=abc123".to_owned())]),
            body: None,
        };
        let rendered = StderrTransportLogger::new().format_event(&event);
        assert!(rendered.contains("< 200 GET https://api.example.com/blob"));
        assert!(rendered.contains(&format!("< set-cookie: {REDACTED}")));
        assert!(!rendered.contains("abc123"));
        assert!(rendered.contains("< [body: 2048 bytes not captured]"));
    }

    #[test]
    fn status_only_response_has_no_trailing_space() {
        // The reqwest-direct helper logs responses with only a status (no
        // method/url); the line must not render as `< 200  `.
        let event = TransportLogEvent {
            message: "http response",
            fields: fields(&[("status", "204")]),
            headers: Some(vec![("content-length".to_owned(), "0".to_owned())]),
            body: None,
        };
        let rendered = StderrTransportLogger::new().format_event(&event);
        assert!(
            rendered.starts_with("< 204\n"),
            "status-only response should be `< 204` with no trailing space, got: {rendered:?}"
        );
    }

    #[test]
    fn extra_redacted_headers_are_redacted_case_insensitively() {
        let event = TransportLogEvent {
            message: "http request",
            fields: fields(&[("method", "GET"), ("url", "https://api.example.com/m")]),
            headers: Some(vec![
                ("x-litellm-api-key".to_owned(), "sk-leak-me".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ]),
            body: None,
        };
        // Mixed case + stray whitespace + an empty entry, to prove names are
        // trimmed (so they still match) and empties are dropped.
        let logger = StderrTransportLogger::new().with_redacted_headers([
            "  X-LiteLLM-API-Key  ",
            "   ",
            "",
        ]);
        let rendered = logger.format_event(&event);
        assert!(rendered.contains(&format!("> x-litellm-api-key: {REDACTED}")));
        assert!(!rendered.contains("sk-leak-me"));
        // Non-configured headers are untouched.
        assert!(rendered.contains("> content-type: application/json"));

        // Without configuring it, the same header is shown verbatim.
        let plain = StderrTransportLogger::new().format_event(&event);
        assert!(plain.contains("> x-litellm-api-key: sk-leak-me"));
    }

    #[test]
    fn retry_event_renders_a_note_line() {
        let event = TransportLogEvent {
            message: "retrying request",
            fields: fields(&[("attempt", "2"), ("backoff", "500ms")]),
            headers: None,
            body: None,
        };
        assert_eq!(
            StderrTransportLogger::new().format_event(&event),
            "* retrying (attempt 2, backoff 500ms)\n"
        );
    }
}
