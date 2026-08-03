use serde_json::{Map, Value};

use crate::Result;

use super::Envelope;

/// Renders an envelope in the TOON migration format.
pub fn render_toon(envelope: &Envelope) -> Result<String> {
    envelope.serialization_result()?;
    let clean = serde_json::to_value(envelope)?;
    Ok(encode_value(&clean))
}

fn encode_value(value: &Value) -> String {
    if is_primitive(value) {
        return encode_primitive(value);
    }
    let mut lines = Vec::new();
    match value {
        Value::Array(items) => encode_array("", items, &mut lines, 0),
        Value::Object(map) => encode_object(map, &mut lines, 0),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    lines.join("\n")
}

fn encode_object(map: &Map<String, Value>, lines: &mut Vec<String>, depth: usize) {
    let mut keys = map.keys().collect::<Vec<_>>();
    keys.sort();
    for key in keys {
        encode_key_value_pair(key, &map[key], lines, depth);
    }
}

fn encode_key_value_pair(key: &str, value: &Value, lines: &mut Vec<String>, depth: usize) {
    let encoded_key = encode_key(key);
    match value {
        value if is_primitive(value) => push_line(
            lines,
            depth,
            format!("{encoded_key}: {}", encode_primitive(value)),
        ),
        Value::Array(items) => encode_array(key, items, lines, depth),
        Value::Object(map) if map.is_empty() => push_line(lines, depth, format!("{encoded_key}:")),
        Value::Object(map) => {
            push_line(lines, depth, format!("{encoded_key}:"));
            encode_object(map, lines, depth + 1);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn encode_array(key: &str, items: &[Value], lines: &mut Vec<String>, depth: usize) {
    if items.is_empty() {
        push_line(lines, depth, format_header(key, 0, &[]));
        return;
    }

    if items.iter().all(is_primitive) {
        push_line(lines, depth, format_inline_array(key, items));
        return;
    }

    if items
        .iter()
        .all(|item| matches!(item, Value::Array(values) if values.iter().all(is_primitive)))
    {
        push_line(lines, depth, format_header(key, items.len(), &[]));
        for item in items {
            if let Value::Array(values) = item {
                push_line(
                    lines,
                    depth + 1,
                    format!("- {}", format_inline_array("", values)),
                );
            }
        }
        return;
    }

    if let Some(header) = detect_tabular_header(items) {
        push_line(lines, depth, format_header(key, items.len(), &header));
        for item in items {
            let Value::Object(map) = item else {
                continue;
            };
            let values = header.iter().map(|key| &map[key]).collect::<Vec<_>>();
            push_line(lines, depth + 1, join_encoded_values(&values));
        }
        return;
    }

    push_line(lines, depth, format_header(key, items.len(), &[]));
    for item in items {
        match item {
            value if is_primitive(value) => {
                push_line(lines, depth + 1, format!("- {}", encode_primitive(value)));
            }
            Value::Array(values) if values.iter().all(is_primitive) => {
                push_line(
                    lines,
                    depth + 1,
                    format!("- {}", format_inline_array("", values)),
                );
            }
            Value::Object(map) => encode_object_as_list_item(map, lines, depth + 1),
            Value::Array(_)
            | Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_) => {}
        }
    }
}

fn detect_tabular_header(items: &[Value]) -> Option<Vec<String>> {
    let Value::Object(first) = items.first()? else {
        return None;
    };
    if first.is_empty() {
        return None;
    }
    let mut header = first.keys().cloned().collect::<Vec<_>>();
    header.sort();
    for item in items {
        let Value::Object(map) = item else {
            return None;
        };
        if map.len() != header.len() {
            return None;
        }
        for key in &header {
            if !map.get(key).is_some_and(is_primitive) {
                return None;
            }
        }
    }
    Some(header)
}

fn encode_object_as_list_item(map: &Map<String, Value>, lines: &mut Vec<String>, depth: usize) {
    let mut keys = map.keys().collect::<Vec<_>>();
    keys.sort();
    let Some(first_key) = keys.first() else {
        push_line(lines, depth, "-".to_owned());
        return;
    };
    let first_value = &map[*first_key];
    match first_value {
        value if is_primitive(value) => push_line(
            lines,
            depth,
            format!("- {}: {}", encode_key(first_key), encode_primitive(value)),
        ),
        Value::Array(values) if values.iter().all(is_primitive) => push_line(
            lines,
            depth,
            format!("- {}", format_inline_array(first_key, values)),
        ),
        Value::Object(nested) if nested.is_empty() => {
            push_line(lines, depth, format!("- {}:", encode_key(first_key)));
        }
        Value::Object(nested) => {
            push_line(lines, depth, format!("- {}:", encode_key(first_key)));
            encode_object(nested, lines, depth + 2);
        }
        Value::Array(values) => {
            push_line(
                lines,
                depth,
                format!("- {}[{}]:", encode_key(first_key), values.len()),
            );
            for item in values {
                match item {
                    value if is_primitive(value) => {
                        push_line(lines, depth + 1, format!("- {}", encode_primitive(value)));
                    }
                    Value::Array(nested) if nested.iter().all(is_primitive) => {
                        push_line(
                            lines,
                            depth + 1,
                            format!("- {}", format_inline_array("", nested)),
                        );
                    }
                    Value::Object(nested) => encode_object_as_list_item(nested, lines, depth + 1),
                    Value::Array(_)
                    | Value::Null
                    | Value::Bool(_)
                    | Value::Number(_)
                    | Value::String(_) => {}
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }

    for key in keys.into_iter().skip(1) {
        encode_key_value_pair(key, &map[key], lines, depth + 1);
    }
}

fn format_header(key: &str, len: usize, fields: &[String]) -> String {
    let mut out = String::new();
    if !key.is_empty() {
        out.push_str(&encode_key(key));
    }
    out.push('[');
    out.push_str(&len.to_string());
    out.push(']');
    if !fields.is_empty() {
        out.push('{');
        out.push_str(
            &fields
                .iter()
                .map(|field| encode_key(field))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('}');
    }
    out.push(':');
    out
}

fn format_inline_array(key: &str, values: &[Value]) -> String {
    let header = format_header(key, values.len(), &[]);
    if values.is_empty() {
        header
    } else {
        let refs = values.iter().collect::<Vec<_>>();
        format!("{header} {}", join_encoded_values(&refs))
    }
}

fn join_encoded_values(values: &[&Value]) -> String {
    values
        .iter()
        .map(|value| encode_primitive(value))
        .collect::<Vec<_>>()
        .join(",")
}

fn encode_primitive(value: &Value) -> String {
    match value {
        Value::Bool(true) => "true".to_owned(),
        Value::Bool(false) => "false".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(value) => encode_string_literal(value),
        Value::Null | Value::Array(_) | Value::Object(_) => "null".to_owned(),
    }
}

fn encode_string_literal(value: &str) -> String {
    if is_safe_unquoted(value) {
        value.to_owned()
    } else {
        format!("\"{}\"", escape_string(value))
    }
}

fn encode_key(key: &str) -> String {
    if is_valid_unquoted_key(key) {
        key.to_owned()
    } else {
        format!("\"{}\"", escape_string(key))
    }
}

fn is_primitive(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn is_safe_unquoted(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !matches!(value, "true" | "false" | "null")
        && !is_numeric_like(value)
        && !value.contains(':')
        && !value.contains('"')
        && !value.contains('\\')
        && !value.contains(',')
        && !value.contains(['[', ']', '{', '}'])
        && !value.contains(['\n', '\r', '\t'])
        && !value.starts_with('-')
}

fn is_numeric_like(value: &str) -> bool {
    if value.starts_with('0') && value.len() > 1 && value.chars().all(|ch| ch.is_ascii_digit()) {
        return true;
    }
    value.parse::<f64>().is_ok()
}

fn is_valid_unquoted_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
}

fn escape_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn push_line(lines: &mut Vec<String>, depth: usize, line: String) {
    lines.push(format!("{}{line}", "  ".repeat(depth)));
}
