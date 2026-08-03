use crate::Result;

use super::Envelope;

/// Renders an envelope as pretty JSON.
///
/// HTML-sensitive characters are escaped so the output is safe to embed in
/// browser-oriented logs or diagnostics without changing the JSON data model.
pub fn render_json(envelope: &Envelope) -> Result<String> {
    envelope.serialization_result()?;
    let mut rendered = serde_json::to_string_pretty(envelope)?;
    rendered = escape_html_sensitive_json_chars(&rendered);
    rendered.push('\n');
    Ok(rendered)
}

fn escape_html_sensitive_json_chars(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            '\u{2028}' => escaped.push_str("\\u2028"),
            '\u{2029}' => escaped.push_str("\\u2029"),
            _ => escaped.push(character),
        }
    }
    escaped
}
