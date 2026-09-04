use serde_json::Value;

use crate::output::PaginationMeta;

/// Resolves a column's (possibly dotted) field path against an object,
/// walking down through nested objects one segment at a time — e.g.
/// `"parameters.items"` reaches `map["parameters"]["items"]`.
///
/// Returns `None` when: `field` is empty; any segment (including a
/// leading/trailing/doubled `.`) is empty; an intermediate or leaf segment is
/// missing; or an intermediate segment's value is not an object. The leaf
/// segment's value is returned as-is whatever its `Value` variant is —
/// callers decide what to do with that.
pub(crate) fn resolve_field_path<'value>(
    map: &'value serde_json::Map<String, Value>,
    field: &str,
) -> Option<&'value Value> {
    let mut segments = field.split('.');
    let first = segments.next()?;
    if first.is_empty() {
        return None;
    }
    let mut current = map.get(first)?;
    for segment in segments {
        if segment.is_empty() {
            return None;
        }
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

/// Resolves the object that directly contains `field`'s leaf segment — e.g.
/// for `"parameters.items"`, the object at `"parameters"` (the one whose keys
/// include `"items"` as a direct child). A field with no `.` has `map` itself
/// as its parent, since the leaf is already one of `map`'s direct keys.
///
/// Used to reach a nested array's `pagination` sibling (see
/// [`resolve_nested_pagination`]) that `resolve_field_path` alone can't see,
/// since that function only ever returns the leaf.
pub(crate) fn resolve_field_parent<'value>(
    map: &'value serde_json::Map<String, Value>,
    field: &str,
) -> Option<&'value serde_json::Map<String, Value>> {
    match field.rsplit_once('.') {
        None => Some(map),
        Some((parent_path, _leaf)) => resolve_field_path(map, parent_path)?.as_object(),
    }
}

/// Resolves a `pagination` field on `parent` — the same object that directly
/// contains a `TableColumn::nested` column's array — as a [`PaginationMeta`],
/// so nested tables get the exact same `"(N of M rows, offset O, limit L)"`
/// footer a top-level paginated array gets.
pub(crate) fn resolve_nested_pagination(
    parent: &serde_json::Map<String, Value>,
) -> Option<PaginationMeta> {
    serde_json::from_value(parent.get("pagination")?.clone()).ok()
}

/// Prefixes every non-empty line of `block` with `indent`, leaving blank
/// lines (e.g. the blank line before a table's `(N rows)` footer) bare so no
/// line ever carries trailing-whitespace-only indent. Round-trips a block's
/// existing single-trailing-newline convention.
pub(crate) fn indent_block(block: &str, indent: &str) -> String {
    block
        .lines()
        .map(|line| {
            if line.is_empty() {
                line.to_owned()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Whether `value` is a shape `TableColumn::nested` can render as a child
/// block: a single object, or an array whose items are all objects (an empty
/// array trivially qualifies, rendering as an indented "no results"). Gates
/// entry into nested rendering so a column with `.nested(...)` set is a true
/// no-op — the exact same single-line `format_value` rendering an un-opted-in
/// column would have produced — whenever the runtime value doesn't actually
/// have this shape (a scalar, or an array mixing objects with non-objects).
pub(crate) fn is_nestable(value: &Value) -> bool {
    matches!(value, Value::Object(_))
        || matches!(value, Value::Array(items) if items.iter().all(Value::is_object))
}

pub(crate) fn format_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "yes".to_owned(),
        Value::Bool(false) => "no".to_owned(),
        Value::Number(number) => format_number(number),
        Value::String(value) => value.clone(),
        Value::Array(items) => items
            .iter()
            .map(format_value)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(_) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned()),
    }
}

pub(crate) fn format_plain_value(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => format_number(number),
        Value::String(value) => value.clone(),
        Value::Array(items) => {
            let values = items
                .iter()
                .map(format_plain_value)
                .collect::<Vec<_>>()
                .join(" ");
            format!("[{values}]")
        }
        Value::Object(object) => {
            let mut pairs = object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>();
            pairs.sort_by(|left, right| left.0.cmp(&right.0));
            let object = pairs
                .into_iter()
                .collect::<serde_json::Map<String, Value>>();
            serde_json::to_string(&Value::Object(object)).unwrap_or_else(|_| "{}".to_owned())
        }
    }
}

pub(crate) fn truncate(value: &str, width: usize) -> String {
    if value.len() <= width {
        return value.to_owned();
    }
    if width <= 3 {
        return value.chars().take(width).collect();
    }
    let mut out = value.chars().take(width - 3).collect::<String>();
    out.push_str("...");
    out
}

fn format_number(number: &serde_json::Number) -> String {
    number.to_string()
}
