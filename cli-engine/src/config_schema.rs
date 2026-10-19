//! Typed, discoverable settings for the per-application
//! [config file](crate::config::ConfigFile).
//!
//! A CLI describes its own preferences as a plain Rust struct and registers it
//! with [`CliConfig::with_config_schema`](crate::cli::CliConfig::with_config_schema):
//!
//! ```
//! use cli_engine::CliConfig;
//! use schemars::JsonSchema;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
//! #[serde(default)]
//! struct Prefs {
//!     /// Show domain names in their ASCII (punycode) form.
//!     punycode: bool,
//! }
//!
//! let config = CliConfig::new("my-cli", "Team CLI", "my-cli").with_config_schema::<Prefs>();
//! ```
//!
//! The engine derives each key's type, description, default, and allowed values
//! from the struct's JSON Schema, so the `config` commands can reject unknown
//! keys and ill-typed values and show users what is settable. Nested structs
//! become dotted keys (`domain.punycode`). Handlers read the same struct back
//! with [`ConfigFile::deserialize`](crate::config::ConfigFile::deserialize).
//!
//! Without a registered schema the `config` commands stay permissive, as they
//! were before this module existed: any key is accepted and the value's type
//! is inferred.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::config::ConfigFile;
use crate::error::CliCoreError;

/// How deep a nested settings struct may go before the walker stops. Guards
/// against recursive types.
const MAX_DEPTH: usize = 6;

/// The type of a [`ConfigKey`], which decides how `config set` parses its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ConfigKind {
    /// `true` or `false`.
    Bool,
    /// A whole number.
    Integer,
    /// Any finite number.
    Float,
    /// Text, optionally limited to [`ConfigKey::allowed`] values.
    String,
    /// An array or table. Readable, but not settable with `config set`.
    Structured,
}

/// One settable key, as declared by the engine or a consumer's settings struct.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConfigKey {
    /// Dotted path in the config file, e.g. `punycode` or `domain.punycode`.
    pub key: String,
    /// Value type.
    #[serde(rename = "type")]
    pub kind: ConfigKind,
    /// One-line description (the field's doc comment), or empty.
    pub description: String,
    /// Value used when the key is absent, if the struct declares one.
    pub default: Option<Value>,
    /// Accepted values for an enum-like string key; empty means unrestricted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
    /// Validated by [`ConfigFile::set`] itself rather than by this schema.
    #[serde(skip)]
    engine: bool,
}

/// Where a setting's effective value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    /// Written in the config file.
    File,
    /// Not in the file; the declared default applies.
    Default,
    /// Not in the file and no default is declared.
    Unset,
}

/// A [`ConfigKey`] together with its current value in a config file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConfigEntry {
    /// The key's declaration.
    #[serde(flatten)]
    pub key: ConfigKey,
    /// File value if set, else the default, else `null`.
    pub value: Value,
    /// Where `value` came from.
    pub source: SettingSource,
}

/// An invalid-input error: stable `VALIDATION_ERROR` code, a message, and a
/// recovery hint the output envelope surfaces as `fix`.
pub(crate) fn validation_error(message: impl Into<String>, fix: impl Into<String>) -> CliCoreError {
    CliCoreError::with_fix(
        fix,
        CliCoreError::SystemMessage {
            message: message.into(),
            system: "config".to_owned(),
            code: "VALIDATION_ERROR".to_owned(),
            request_id: String::new(),
        },
    )
}

type Check = Arc<dyn Fn(&ConfigFile) -> Result<(), String> + Send + Sync>;

/// The set of keys `config get`/`set`/`unset`/`list` know about.
///
/// Always includes the engine's own keys (`credentials.store`,
/// `output.format`). [`ConfigSchema::of`] adds a consumer's keys and makes the
/// schema *strict*: keys outside it are rejected by `set`.
#[derive(Clone)]
pub struct ConfigSchema {
    keys: Vec<ConfigKey>,
    strict: bool,
    check: Option<Check>,
}

impl std::fmt::Debug for ConfigSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigSchema")
            .field("keys", &self.keys)
            .field("strict", &self.strict)
            .finish_non_exhaustive()
    }
}

impl Default for ConfigSchema {
    fn default() -> Self {
        Self::engine()
    }
}

impl ConfigSchema {
    /// The engine's own keys only. Non-strict: any other key is still accepted
    /// by `set` with an inferred type.
    #[must_use]
    pub fn engine() -> Self {
        let engine_key = |key: &str, description: &str, default: Option<&str>| ConfigKey {
            key: key.to_owned(),
            kind: ConfigKind::String,
            description: description.to_owned(),
            default: default.map(Value::from),
            allowed: Vec::new(),
            engine: true,
        };
        Self {
            keys: vec![
                engine_key(
                    "credentials.store",
                    "Where credentials are stored: auto, keyring, or file.",
                    Some("auto"),
                ),
                engine_key(
                    "output.format",
                    "Default output format: json, human, or toon. Unset means human on a \
                     terminal and JSON otherwise.",
                    None,
                ),
            ],
            strict: false,
            check: None,
        }
    }

    /// The engine's keys plus every setting declared by `T`, strict.
    ///
    /// Keys, types, and descriptions (doc comments) come from `T`'s JSON
    /// Schema. Defaults are what `T` deserializes to from an empty file (via
    /// `#[serde(default)]`), so `T` needs `Serialize` as well; a `T` with
    /// required fields simply shows no defaults. A value is accepted only if it parses as the key's type and
    /// the resulting file still deserializes into `T`, so `T`'s own `Deserialize`
    /// rules apply. `T` should not use `#[serde(deny_unknown_fields)]`: the file
    /// also holds the engine's own tables.
    #[must_use]
    pub fn of<T>() -> Self
    where
        T: DeserializeOwned + Serialize + JsonSchema + 'static,
    {
        let mut schema = Self::engine();
        let root = serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null);
        let mut keys = collect_keys(&root);
        overlay_defaults::<T>(&mut keys);
        schema.keys.extend(keys);
        schema.strict = true;
        schema.check = Some(Arc::new(|file| {
            file.deserialize::<T>().map(drop).map_err(|e| e.to_string())
        }));
        schema
    }

    /// Adds a cross-field rule over the typed settings, run after the per-key
    /// type check on every `set`. Return `Err(reason)` to reject the change.
    #[must_use]
    pub fn validate_with<T>(
        mut self,
        rule: impl Fn(&T) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self
    where
        T: DeserializeOwned + 'static,
    {
        let previous = self.check.take();
        self.check = Some(Arc::new(move |file| {
            if let Some(previous) = &previous {
                previous(file)?;
            }
            let value = file.deserialize::<T>().map_err(|e| e.to_string())?;
            rule(&value)
        }));
        self
    }

    /// Returns every known key, engine keys first.
    #[must_use]
    pub fn keys(&self) -> &[ConfigKey] {
        &self.keys
    }

    /// Returns the declaration for `key`, if known.
    #[must_use]
    pub fn find(&self, key: &str) -> Option<&ConfigKey> {
        self.keys.iter().find(|k| k.key == key)
    }

    /// Reports whether keys outside the schema are rejected by `set`.
    #[must_use]
    pub fn is_strict(&self) -> bool {
        self.strict
    }

    /// Returns each known key with its current value in `file`, followed by
    /// any keys in the file the schema does not declare (stale or mistyped
    /// keys, or tables the CLI reads without registering).
    #[must_use]
    pub fn entries(&self, file: &ConfigFile) -> Vec<ConfigEntry> {
        let undeclared = file
            .leaf_keys()
            .into_iter()
            .filter(|key| self.find(key).is_none())
            .map(|key| undeclared_entry(key, file));
        self.keys
            .iter()
            .map(|k| entry_for(k, file))
            .chain(undeclared)
            .collect()
    }

    /// Returns the entry for `key` if it is a known key.
    #[must_use]
    pub fn entry(&self, file: &ConfigFile, key: &str) -> Option<ConfigEntry> {
        self.find(key).map(|k| entry_for(k, file))
    }

    /// Parses `raw` as `key`'s type, validates the result, and writes it to
    /// `file` (not to disk), returning the stored value.
    ///
    /// `file` is left untouched when validation fails.
    ///
    /// # Errors
    /// Returns an error with a `fix` for an unknown key (strict schemas), a
    /// value that does not parse as the key's type, or a value the settings
    /// struct rejects.
    pub fn set(&self, file: &mut ConfigFile, key: &str, raw: &str) -> crate::Result<Value> {
        let known = match self.find(key) {
            Some(known) => known,
            None if self.strict => return Err(self.unknown_key(key)),
            None => {
                file.set(key, raw)?;
                return Ok(file.get_json(key).unwrap_or(Value::Null));
            }
        };
        if known.engine {
            file.set(key, raw)?;
            return Ok(file.get_json(key).unwrap_or(Value::Null));
        }
        let value = parse_value(known, raw)?;
        let mut candidate = file.clone();
        candidate.set_value(key, value)?;
        self.run_check(&candidate, key)?;
        *file = candidate;
        Ok(file.get_json(key).unwrap_or(Value::Null))
    }

    /// Removes `key` from `file` (not from disk), returning whether it was
    /// present. A key that is known, or already in the file, can be unset even
    /// if a newer or older version of the CLI no longer declares it.
    ///
    /// # Errors
    /// Returns an error with a `fix` for an unknown, absent key (strict
    /// schemas).
    pub fn unset(&self, file: &mut ConfigFile, key: &str) -> crate::Result<bool> {
        if self.strict && self.find(key).is_none() && file.get_json(key).is_none() {
            return Err(self.unknown_key(key));
        }
        file.unset(key)
    }

    fn run_check(&self, candidate: &ConfigFile, key: &str) -> crate::Result<()> {
        let Some(check) = &self.check else {
            return Ok(());
        };
        check(candidate).map_err(|reason| {
            validation_error(
                format!("invalid value for {key:?}: {reason}"),
                format!("Choose a different value for {key:?}; `config list` shows what each setting accepts."),
            )
        })
    }

    pub(crate) fn unknown_key(&self, key: &str) -> CliCoreError {
        let suggestion = self
            .keys
            .iter()
            .map(|k| (strsim::jaro_winkler(&k.key, key), &k.key))
            .filter(|(score, _)| *score > 0.8)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, k)| format!(" Did you mean {k:?}?"))
            .unwrap_or_default();
        validation_error(
            format!("unknown config key {key:?}"),
            format!("Run `config list` to see the available settings.{suggestion}"),
        )
    }
}

fn undeclared_entry(key: String, file: &ConfigFile) -> ConfigEntry {
    let value = file.get_json(&key).unwrap_or(Value::Null);
    let kind = match &value {
        Value::Bool(_) => ConfigKind::Bool,
        Value::Number(n) if n.is_i64() => ConfigKind::Integer,
        Value::Number(_) => ConfigKind::Float,
        Value::String(_) => ConfigKind::String,
        _ => ConfigKind::Structured,
    };
    ConfigEntry {
        key: ConfigKey {
            key,
            kind,
            description: "Not declared by this CLI.".to_owned(),
            default: None,
            allowed: Vec::new(),
            engine: false,
        },
        value,
        source: SettingSource::File,
    }
}

fn entry_for(key: &ConfigKey, file: &ConfigFile) -> ConfigEntry {
    let (value, source) = match (file.get_json(&key.key), &key.default) {
        (Some(value), _) => (value, SettingSource::File),
        (None, Some(default)) => (default.clone(), SettingSource::Default),
        (None, None) => (Value::Null, SettingSource::Unset),
    };
    ConfigEntry {
        key: key.clone(),
        value,
        source,
    }
}

/// Parses `raw` per `key`'s declared type. Never infers: `"123"` stays a
/// string for a string key, and `"banana"` is an error for a boolean key.
fn parse_value(key: &ConfigKey, raw: &str) -> crate::Result<toml_edit::Value> {
    let parsed = match key.kind {
        ConfigKind::Bool => match raw {
            "true" => Some(true.into()),
            "false" => Some(false.into()),
            _ => None,
        },
        ConfigKind::Integer => raw.parse::<i64>().ok().map(Into::into),
        ConfigKind::Float => raw
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(Into::into),
        ConfigKind::String if key.allowed.is_empty() || key.allowed.iter().any(|a| a == raw) => {
            Some(raw.into())
        }
        ConfigKind::String | ConfigKind::Structured => None,
    };
    parsed.ok_or_else(|| {
        let (expected, fix) = expectation(key);
        validation_error(
            format!(
                "invalid value {raw:?} for {:?}: expected {expected}",
                key.key
            ),
            fix,
        )
    })
}

/// The "expected ..." phrase and the matching recovery hint for a key.
fn expectation(key: &ConfigKey) -> (String, String) {
    let k = &key.key;
    match key.kind {
        ConfigKind::Bool => (
            "true or false".to_owned(),
            format!("Use `config set {k} true` or `config set {k} false`."),
        ),
        ConfigKind::Integer => (
            "a whole number".to_owned(),
            format!("Use a whole number, e.g. `config set {k} 10`."),
        ),
        ConfigKind::Float => (
            "a number".to_owned(),
            format!("Use a number, e.g. `config set {k} 1.5`."),
        ),
        ConfigKind::String => {
            let allowed = key.allowed.join(", ");
            (
                format!("one of: {allowed}"),
                format!("Use one of: {allowed}."),
            )
        }
        ConfigKind::Structured => (
            "a table or list, which `config set` cannot write".to_owned(),
            format!("Edit {k:?} in the config file directly (`config path` prints its location)."),
        ),
    }
}

/// Sets each key's default from what `T` deserializes to out of an empty file.
///
/// The JSON Schema alone cannot supply these: schemars omits `default` on
/// `$ref`'d fields (enums, nested structs) and writes `null` for `Option`s.
fn overlay_defaults<T: DeserializeOwned + Serialize>(keys: &mut [ConfigKey]) {
    let Ok(defaults) = ConfigFile::default().deserialize::<T>() else {
        return;
    };
    let Ok(defaults) = serde_json::to_value(defaults) else {
        return;
    };
    for key in keys {
        let found = key
            .key
            .split('.')
            .try_fold(&defaults, |value, segment| value.get(segment));
        if let Some(default) = found.filter(|v| !v.is_null()) {
            key.default = Some(default.clone());
        }
    }
}

/// Flattens an object schema into dotted [`ConfigKey`]s.
fn collect_keys(root: &Value) -> Vec<ConfigKey> {
    let mut out = Vec::new();
    walk(root, &resolve(root, root), "", 0, &mut out);
    out
}

fn walk(root: &Value, node: &Value, prefix: &str, depth: usize, out: &mut Vec<ConfigKey>) {
    let Some(properties) = node.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, property) in properties {
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

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    /// Output style.
    #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    enum Style {
        /// Plain.
        #[default]
        Plain,
        /// Fancy.
        Fancy,
    }

    #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default)]
    struct Domain {
        /// Show names in ASCII form.
        punycode: bool,
    }

    #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default)]
    struct Prefs {
        /// How many rows to show
        /// per page.
        page_size: Option<u32>,
        /// Pick a style.
        style: Style,
        /// Nickname.
        nickname: String,
        domain: Domain,
    }

    fn schema() -> ConfigSchema {
        ConfigSchema::of::<Prefs>()
    }

    #[test]
    fn derives_keys_types_descriptions_and_defaults() {
        let schema = schema();
        let page = schema.find("page_size").expect("page_size");
        assert_eq!(page.kind, ConfigKind::Integer);
        assert_eq!(page.description, "How many rows to show per page.");
        let style = schema.find("style").expect("style");
        assert_eq!(style.kind, ConfigKind::String);
        assert_eq!(style.allowed, ["plain", "fancy"]);
        assert_eq!(style.default, Some(Value::from("plain")));
        let nested = schema.find("domain.punycode").expect("nested key");
        assert_eq!(nested.kind, ConfigKind::Bool);
        assert_eq!(nested.default, Some(Value::Bool(false)));
        assert_eq!(nested.description, "Show names in ASCII form.");
        assert!(schema.find("credentials.store").is_some(), "engine keys");
    }

    #[test]
    fn set_stores_the_declared_type_not_an_inferred_one() {
        let mut file = ConfigFile::default();
        let schema = schema();
        schema.set(&mut file, "nickname", "123").expect("string");
        schema
            .set(&mut file, "domain.punycode", "true")
            .expect("bool");
        assert_eq!(file.get_json("nickname"), Some(Value::from("123")));
        assert_eq!(file.get_json("domain.punycode"), Some(Value::Bool(true)));
    }

    #[test]
    fn set_rejects_bad_values_and_leaves_file_untouched() {
        let mut file = ConfigFile::default();
        let schema = schema();
        for (key, raw) in [
            ("domain.punycode", "banana"),
            ("page_size", "ten"),
            ("page_size", "-1"),
            ("style", "loud"),
        ] {
            assert!(schema.set(&mut file, key, raw).is_err(), "{key}={raw}");
        }
        assert_eq!(file.to_toml_string(), "");
    }

    #[test]
    fn strict_schema_rejects_unknown_keys_with_a_suggestion() {
        let err = schema()
            .set(&mut ConfigFile::default(), "domain.punycod", "true")
            .expect_err("unknown");
        assert!(
            matches!(&err, CliCoreError::Fix { fix, .. } if fix.contains("domain.punycode")),
            "expected a did-you-mean fix, got {err:?}"
        );
    }

    #[test]
    fn engine_only_schema_stays_permissive() {
        let mut file = ConfigFile::default();
        let schema = ConfigSchema::engine();
        schema
            .set(&mut file, "deploy.replicas", "3")
            .expect("free-form");
        assert_eq!(file.get_json("deploy.replicas"), Some(Value::from(3)));
        assert!(schema.set(&mut file, "credentials.store", "nope").is_err());
    }

    #[test]
    fn validate_with_rejects_cross_field_violations() {
        let schema = ConfigSchema::of::<Prefs>().validate_with(|p: &Prefs| {
            if p.nickname == "root" && p.domain.punycode {
                Err("root cannot use punycode".to_owned())
            } else {
                Ok(())
            }
        });
        let mut file = ConfigFile::default();
        schema.set(&mut file, "nickname", "root").expect("ok alone");
        let err = schema
            .set(&mut file, "domain.punycode", "true")
            .expect_err("rule");
        assert!(
            err.to_string().contains("root cannot use punycode"),
            "{err}"
        );
        assert_eq!(file.get_json("domain.punycode"), None);
    }

    #[test]
    fn unset_removes_key_and_empty_parent_table() {
        let mut file = ConfigFile::default();
        let schema = schema();
        schema
            .set(&mut file, "domain.punycode", "true")
            .expect("set");
        assert!(schema.unset(&mut file, "domain.punycode").expect("unset"));
        assert_eq!(file.to_toml_string().trim(), "");
        assert!(
            !schema
                .unset(&mut file, "domain.punycode")
                .expect("idempotent")
        );
        assert!(schema.unset(&mut file, "nope").is_err());
    }

    #[test]
    fn entries_report_value_and_source() {
        let mut file = ConfigFile::default();
        let schema = schema();
        schema.set(&mut file, "style", "fancy").expect("set");
        let style = schema.entry(&file, "style").expect("style");
        assert_eq!(
            (style.value, style.source),
            (Value::from("fancy"), SettingSource::File)
        );
        let punycode = schema.entry(&file, "domain.punycode").expect("punycode");
        assert_eq!(
            (punycode.value, punycode.source),
            (Value::Bool(false), SettingSource::Default)
        );
        let page = schema.entry(&file, "page_size").expect("page_size");
        assert_eq!(
            (page.value, page.source),
            (Value::Null, SettingSource::Unset)
        );
    }
}
