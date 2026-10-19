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
use serde_json::Value;

use crate::config::ConfigFile;
use crate::error::CliCoreError;

mod derive;

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
/// The typed settings a file currently resolves to, as JSON, if it is valid.
type Effective = Arc<dyn Fn(&ConfigFile) -> Option<Value> + Send + Sync>;

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
    effective: Option<Effective>,
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
            effective: None,
        }
    }

    /// The engine's keys plus every setting declared by `T`, strict.
    ///
    /// Keys, types, and descriptions (doc comments) come from `T`'s JSON
    /// Schema. Defaults are what `T` deserializes to from an empty file (via
    /// `#[serde(default)]`), so `T` needs `Serialize` as well. A value is
    /// accepted only if it parses as the key's type and the resulting file
    /// still deserializes into `T`, so `T`'s own `Deserialize` rules apply.
    ///
    /// Every field of `T` needs a default, because `config set` writes one key
    /// at a time and each write must leave a file that deserializes into `T`.
    /// `T` should not use `#[serde(deny_unknown_fields)]`: the file also holds
    /// the engine's own tables.
    ///
    /// # Panics
    /// In debug builds, if `T` does not deserialize from an empty file.
    #[must_use]
    pub fn of<T>() -> Self
    where
        T: DeserializeOwned + Serialize + JsonSchema + 'static,
    {
        debug_assert!(
            ConfigFile::default().deserialize::<T>().is_ok(),
            "config settings struct {} must deserialize from an empty file: give every field a \
             default (e.g. #[serde(default)]), or `config set` can never initialize it",
            std::any::type_name::<T>()
        );
        let mut schema = Self::engine();
        let root = serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null);
        let mut keys = derive::collect_keys(&root);
        derive::overlay_defaults::<T>(&mut keys);
        schema.keys.extend(keys);
        schema.strict = true;
        schema.check = Some(Arc::new(|file| {
            file.deserialize::<T>().map(drop).map_err(|e| e.to_string())
        }));
        schema.effective = Some(Arc::new(|file| {
            let typed = file.deserialize::<T>().ok()?;
            serde_json::to_value(typed).ok()
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
            .map(|k| self.entry_for(k, file))
            .chain(undeclared)
            .collect()
    }

    /// Returns the entry for `key` if it is a known key.
    #[must_use]
    pub fn entry(&self, file: &ConfigFile, key: &str) -> Option<ConfigEntry> {
        self.find(key).map(|k| self.entry_for(k, file))
    }

    /// What the registered settings struct resolves `file` to, or `None` when
    /// there is no struct or the file does not deserialize into it.
    fn effective_settings(&self, file: &ConfigFile) -> Option<Value> {
        self.effective
            .as_ref()
            .and_then(|effective| effective(file))
    }

    /// Parses `raw` as `key`'s type, validates the result, and writes it to
    /// `file` (not to disk), returning the stored value.
    ///
    /// Every write, including engine keys and (in a non-strict schema) keys
    /// the schema does not declare, is applied to a copy and checked against
    /// the registered settings struct and rules first, so `file` is left
    /// untouched when validation fails.
    ///
    /// # Errors
    /// Returns an error with a `fix` for an unknown key (strict schemas), a
    /// value that does not parse as the key's type, or a value the settings
    /// struct or a registered rule rejects.
    pub fn set(&self, file: &mut ConfigFile, key: &str, raw: &str) -> crate::Result<Value> {
        let mut candidate = file.clone();
        match self.find(key) {
            None if self.strict => return Err(self.unknown_key(key)),
            Some(known) if !known.engine => {
                let value = parse_value(known, raw)?;
                candidate.set_value(key, value).map_err(|e| {
                    validation_error(
                        e.to_string(),
                        "A parent of this key is already set to a plain value. Fix or remove it in \
                         the config file (`config path` prints the location).",
                    )
                })?;
            }
            // Engine keys validate themselves; an undeclared key in a
            // permissive schema has its type inferred.
            _ => candidate.set(key, raw).map_err(|e| {
                validation_error(
                    e.to_string(),
                    "Run `config list` to see what each setting accepts.",
                )
            })?,
        }
        self.run_check(&candidate, key)?;
        *file = candidate;
        Ok(file.get_json(key).unwrap_or(Value::Null))
    }

    /// Removes `key` from `file` (not from disk), returning whether it was
    /// present. A key that is known, or already in the file, can be unset even
    /// if a newer or older version of the CLI no longer declares it.
    ///
    /// Every removal is checked like a `set`, since it can change the
    /// effective settings (the default applies again, or an entry leaves a
    /// map), and is refused if it would make them invalid. If the file is
    /// already invalid, the removal is allowed: it may be part of the repair.
    ///
    /// # Errors
    /// Returns an error with a `fix` for an unknown, absent key (strict
    /// schemas), or a removal that would leave invalid settings.
    pub fn unset(&self, file: &mut ConfigFile, key: &str) -> crate::Result<bool> {
        if self.strict && !self.is_declared_path(key) && file.get_json(key).is_none() {
            return Err(self.unknown_key(key));
        }
        let mut candidate = file.clone();
        let removed = candidate.unset(key)?;
        if removed && self.run_check(file, key).is_ok() {
            self.run_check(&candidate, key)?;
        }
        *file = candidate;
        Ok(removed)
    }

    fn entry_for(&self, key: &ConfigKey, file: &ConfigFile) -> ConfigEntry {
        let mut declared = key.clone();
        let (value, source) = match file.get_json(&key.key) {
            Some(value) => {
                // What would apply if this key were removed, which can differ
                // from the empty-file default when a parent table would remain.
                declared.default = self.default_without(file, key);
                (value, SettingSource::File)
            }
            // Resolve against the typed settings the file actually yields, so
            // this agrees with what a handler reads even when a parent table
            // exists but is empty, or an optional section is absent.
            None => match lookup(self.effective_settings(file).as_ref(), &key.key) {
                Lookup::Value(value) => {
                    declared.default = Some(value.clone());
                    (value.clone(), SettingSource::Default)
                }
                // Also covers an absent optional section: it has no default either.
                Lookup::Unset => {
                    declared.default = None;
                    (Value::Null, SettingSource::Unset)
                }
                Lookup::Unknown => match &key.default {
                    Some(default) => (default.clone(), SettingSource::Default),
                    None => (Value::Null, SettingSource::Unset),
                },
            },
        };
        ConfigEntry {
            key: declared,
            value,
            source,
        }
    }

    /// The default `key` would take if it were unset, resolved against a copy
    /// of `file` with the key removed (pruning emptied parent tables exactly as
    /// `unset` does), falling back to the declared default when that cannot be
    /// determined.
    fn default_without(&self, file: &ConfigFile, key: &ConfigKey) -> Option<Value> {
        let mut without = file.clone();
        if without.unset(&key.key).is_err() {
            return key.default.clone();
        }
        match lookup(self.effective_settings(&without).as_ref(), &key.key) {
            Lookup::Value(value) => Some(value.clone()),
            Lookup::Unset => None,
            Lookup::Unknown => key.default.clone(),
        }
    }

    /// Whether `path` is a declared key or a section holding declared keys.
    fn is_declared_path(&self, path: &str) -> bool {
        self.keys.iter().any(|k| {
            k.key == path
                || k.key
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    fn run_check(&self, candidate: &ConfigFile, key: &str) -> crate::Result<()> {
        let Some(check) = &self.check else {
            return Ok(());
        };
        check(candidate).map_err(|reason| {
            validation_error(
                format!("cannot change {key:?}: {reason}"),
                "Run `config list` to see what each setting accepts, or fix the conflicting \
                 setting first.",
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

/// The result of looking a dotted key up in serialized typed settings.
enum Lookup<'typed> {
    /// A concrete value.
    Value(&'typed Value),
    /// The value is `null`, or sits under an optional section that is absent.
    Unset,
    /// Not determinable: no typed settings, or serialization omitted the field.
    Unknown,
}

fn lookup<'typed>(typed: Option<&'typed Value>, path: &str) -> Lookup<'typed> {
    let Some(mut value) = typed else {
        return Lookup::Unknown;
    };
    for segment in path.split('.') {
        match value {
            Value::Null => return Lookup::Unset,
            Value::Object(map) => match map.get(segment) {
                Some(next) => value = next,
                None => return Lookup::Unknown,
            },
            _ => return Lookup::Unknown,
        }
    }
    if value.is_null() {
        Lookup::Unset
    } else {
        Lookup::Value(value)
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

#[cfg(test)]
mod tests;
