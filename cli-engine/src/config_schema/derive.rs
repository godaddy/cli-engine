//! Derives [`ConfigKey`]s from a settings struct's JSON Schema.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::{ConfigKey, ConfigKind, Lookup, lookup};
use crate::config::{ConfigFile, ENGINE_RESERVED_TABLES};

/// How deep a nested settings struct may go before the walker stops. Guards
/// against recursive types.
const MAX_DEPTH: usize = 6;

/// Sets each key's default from what `T` deserializes to out of an empty file.
///
/// The JSON Schema alone cannot supply these: schemars omits `default` on
/// `$ref`'d fields (enums, nested structs) and writes `null` for `Option`s.
pub(super) fn overlay_defaults<T: DeserializeOwned + Serialize>(keys: &mut [ConfigKey]) {
    let Ok(defaults) = ConfigFile::default().deserialize::<T>() else {
        return;
    };
    let Ok(defaults) = serde_json::to_value(defaults) else {
        return;
    };
    for key in keys {
        if let Lookup::Value(default) = lookup(Some(&defaults), &key.key) {
            key.default = Some(default.clone());
        }
    }
}

/// Flattens an object schema into dotted [`ConfigKey`]s.
pub(super) fn collect_keys(root: &Value) -> Vec<ConfigKey> {
    let mut out = Vec::new();
    walk(root, &resolve(root, root), "", 0, &mut out);
    out
}

/// Whether a settings field name can be a config key. A name with a `.` cannot
/// be told apart from a nested key in a dotted path (so a write would land in
/// the wrong place), and a top-level name the engine reserves would let a write
/// replace one of its tables.
///
/// Both are authoring errors in the settings struct: debug builds assert, and
/// release builds leave the key out rather than advertise one that misbehaves.
fn usable_name(prefix: &str, name: &str) -> bool {
    let dotted = name.contains('.');
    let reserved = prefix.is_empty() && ENGINE_RESERVED_TABLES.contains(&name);
    debug_assert!(
        !dotted,
        "config setting name {name:?} contains a '.', which a dotted key cannot represent; \
         rename the field"
    );
    debug_assert!(
        !reserved,
        "config setting name {name:?} is a table reserved for the engine ({}); rename the field",
        ENGINE_RESERVED_TABLES.join(", ")
    );
    !dotted && !reserved
}

fn walk(root: &Value, node: &Value, prefix: &str, depth: usize, out: &mut Vec<ConfigKey>) {
    let Some(properties) = node.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, property) in properties {
        if !usable_name(prefix, name) {
            continue;
        }
        let property = resolve(root, property);
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if property.get("properties").is_some() && depth < MAX_DEPTH {
            walk(root, &property, &key, depth + 1, out);
        } else {
            out.push(leaf(key, &property));
        }
    }
}

fn leaf(key: String, schema: &Value) -> ConfigKey {
    let allowed = allowed_values(schema);
    let kind = match schema.get("type").and_then(Value::as_str) {
        Some("boolean") => ConfigKind::Bool,
        Some("integer") => ConfigKind::Integer,
        Some("number") => ConfigKind::Float,
        Some("string") => ConfigKind::String,
        _ if !allowed.is_empty() => ConfigKind::String,
        _ => ConfigKind::Structured,
    };
    ConfigKey {
        key,
        kind,
        description: schema
            .get("description")
            .and_then(Value::as_str)
            .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default(),
        default: schema.get("default").filter(|d| !d.is_null()).cloned(),
        allowed,
        engine: false,
    }
}

/// String choices from `enum`, or from a `oneOf` of `const`s (what schemars
/// emits for an enum whose variants carry doc comments).
fn allowed_values(schema: &Value) -> Vec<String> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    let Some(variants) = schema.get("oneOf").and_then(Value::as_array) else {
        return Vec::new();
    };
    let consts: Vec<&str> = variants
        .iter()
        .filter_map(|v| v.get("const").and_then(Value::as_str))
        .collect();
    if consts.len() == variants.len() {
        consts.into_iter().map(str::to_owned).collect()
    } else {
        Vec::new()
    }
}

/// Normalizes one schema node: follows `$ref`, drops a `null` alternative
/// (`Option<T>`), and merges the result so the declaring property's own
/// `description`/`default` win over the target's.
fn resolve(root: &Value, node: &Value) -> Value {
    let mut merged = node.as_object().cloned().unwrap_or_default();
    for _ in 0..MAX_DEPTH {
        if let Some(target) = take_target(root, &mut merged) {
            for (k, v) in target {
                merged.entry(k).or_insert(v);
            }
        } else {
            break;
        }
    }
    strip_null_type(&mut merged);
    Value::Object(merged)
}

/// Removes and returns the one node `merged` points at, via `$ref` or a
/// single non-null `anyOf`/`oneOf` alternative.
fn take_target(root: &Value, merged: &mut Map<String, Value>) -> Option<Map<String, Value>> {
    if let Some(Value::String(reference)) = merged.remove("$ref") {
        let pointer = reference.strip_prefix('#')?;
        return root.pointer(pointer)?.as_object().cloned();
    }
    for combinator in ["anyOf", "oneOf"] {
        let Some(Value::Array(variants)) = merged.get(combinator) else {
            continue;
        };
        let mut non_null = variants
            .iter()
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"));
        let (Some(only), None) = (non_null.next(), non_null.next()) else {
            continue;
        };
        let only = only.as_object().cloned();
        merged.remove(combinator);
        return only;
    }
    None
}

/// Turns `"type": ["boolean", "null"]` into `"type": "boolean"`.
fn strip_null_type(merged: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = merged.get("type") else {
        return;
    };
    let mut non_null = types.iter().filter(|t| t.as_str() != Some("null"));
    if let (Some(only), None) = (non_null.next().cloned(), non_null.next()) {
        merged.insert("type".to_owned(), only);
    }
}
