//! Engine configuration file and credential-storage selection.
//!
//! cli-engine reads an optional per-application TOML config file at
//! `<config-base>/<app_id>/config.toml`, where `<config-base>` is
//! `$XDG_CONFIG_HOME`, `$HOME/Library/Application Support` (macOS),
//! `$HOME/.config` (other Unix), or `%APPDATA%` (Windows) (see
//! [`config_base_dir`](crate::fs::config_base_dir)).
//! Loading is best-effort: a missing file yields defaults, and a malformed file
//! logs a warning and falls back to defaults rather than failing the command.
//!
//! Two engine-reserved settings live here today:
//!
//! - Where credentials are stored — see [`CredentialStore`]. The effective
//!   mode is resolved with the precedence
//!
//!   ```text
//!   --credential-store flag  >  ${PREFIX}_CREDENTIAL_STORE env  >  config file  >  default (Auto)
//!   ```
//!
//!   See [`resolve_credential_store`] and the pure
//!   [`resolve_credential_store_with`].
//!
//! - The default output format for a user who never sets
//!   `--output`/`--json`/`--human`/`--toon` — see [`crate::config::OutputConfig`]. Resolved
//!   with the precedence
//!
//!   ```text
//!   --output/--json/--human/--toon flag  >  ${PREFIX}_OUTPUT env  >  config file  >  TTY-based default
//!   ```
//!
//!   See [`crate::flags::default_output_format`] and the pure
//!   [`crate::flags::resolve_default_output_format`].
//!
//! where `${PREFIX}` is the app id sanitized by
//! [`crate::flags::app_id_env_prefix`].

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use toml_edit::DocumentMut;

use crate::error::CliCoreError;

mod credential_store;

pub use credential_store::{
    CredentialStore, ParseCredentialStoreError, credential_store_env_var, resolve_credential_store,
    resolve_credential_store_with,
};
pub(crate) use credential_store::{clear_credential_store_flag, set_credential_store_flag};

/// Top-level engine configuration parsed from `config.toml`.
///
/// Unknown keys are ignored so older binaries tolerate config written for newer
/// ones. New sections can be added as additional fields over time.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Credential-storage settings (`[credentials]` table).
    pub credentials: CredentialsConfig,
    /// Output-format settings (`[output]` table).
    pub output: OutputConfig,
}

/// The `[credentials]` table of the engine config file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CredentialsConfig {
    /// Selected credential store, or `None` when the key is absent.
    pub store: Option<CredentialStore>,
}

/// The `[output]` table of the engine config file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// Default output format (`"json"`, `"human"`, or `"toon"`), or `None`
    /// when the key is absent. Kept as a plain string rather than
    /// [`crate::output::OutputFormat`] so validation stays in one place —
    /// [`crate::flags::resolve_default_output_format`] — with the same
    /// silently-ignored-if-invalid treatment already applied to the
    /// `${PREFIX}_OUTPUT` env override (falls through to the next tier, no
    /// log line), rather than `OutputFormat`'s permissive `FromStr` (which
    /// silently maps unknown strings to `Json`).
    pub format: Option<String>,
}

/// Returns the path to the engine config file for `app_id`, if a base config
/// directory can be resolved and `app_id` is a safe single path component.
#[must_use]
pub fn config_file_path(app_id: &str) -> Option<PathBuf> {
    if !crate::fs::is_safe_path_component(app_id) {
        tracing::warn!(app_id, "refusing config path with unsafe app id");
        return None;
    }
    crate::fs::config_base_dir().map(|base| base.join(app_id).join("config.toml"))
}

/// Loads the engine-reserved config for `app_id`.
///
/// Convenience wrapper over [`ConfigFile::load`] + [`ConfigFile::engine`].
/// Best-effort: a missing/unreadable/malformed file yields
/// [`EngineConfig::default`], so a broken config file cannot take the CLI down.
#[must_use]
pub fn load(app_id: &str) -> EngineConfig {
    ConfigFile::load(app_id).engine()
}

/// Top-level tables owned by the engine. `ConfigFile::set` validates their keys,
/// and a consumer's settings struct may not use these names.
pub(crate) const ENGINE_RESERVED_TABLES: &[&str] = &["credentials", "output"];

/// A loaded per-application config file.
///
/// cli-engine reads a single TOML file at `<config-base>/<app_id>/config.toml`
/// (see [`config_file_path`]). Engine-reserved settings live in documented
/// top-level tables (today just `[credentials]`, see [`EngineConfig`]); consumer
/// CLIs own every other top-level table and read them with [`section`] or
/// [`deserialize`]. The file is also surfaced to command handlers via
/// [`CommandContext::config`](crate::command::CommandContext::config) and to
/// module registration via
/// [`ModuleContext::config`](crate::module::ModuleContext::config).
///
/// Edits made with [`set`] preserve existing comments and formatting (backed by
/// `toml_edit`) and are persisted with [`save`].
///
/// [`section`]: ConfigFile::section
/// [`deserialize`]: ConfigFile::deserialize
/// [`set`]: ConfigFile::set
/// [`save`]: ConfigFile::save
#[derive(Clone, Debug)]
pub struct ConfigFile {
    path: Option<PathBuf>,
    doc: DocumentMut,
    /// Why the file on disk could not be used (malformed TOML or an I/O error
    /// other than not-found). `doc` is empty in that case.
    load_error: Option<String>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self::from_doc(None, DocumentMut::new())
    }
}

impl ConfigFile {
    fn from_doc(path: Option<PathBuf>, doc: DocumentMut) -> Self {
        Self {
            path,
            doc,
            load_error: None,
        }
    }

    /// Loads the config file for `app_id`.
    ///
    /// Best-effort: a missing file, unresolvable config directory, or malformed
    /// TOML yields an empty document (a warning is logged for the malformed
    /// case). The resolved path is retained for [`save`](ConfigFile::save) even
    /// when the file does not yet exist.
    ///
    /// **Blocking**: this function performs synchronous filesystem I/O. The
    /// engine calls it once at `Cli::new` time (outside of command execution),
    /// which is acceptable for a one-shot CLI. Avoid calling it from hot paths
    /// or from within an async executor without `spawn_blocking`.
    #[must_use]
    pub fn load(app_id: &str) -> Self {
        let path = config_file_path(app_id);
        let mut load_error = None;
        let doc = match &path {
            None => DocumentMut::new(),
            Some(p) => match std::fs::read_to_string(p) {
                Ok(contents) => contents.parse::<DocumentMut>().unwrap_or_else(|e| {
                    tracing::warn!(path = %p.display(), error = %e, "ignoring malformed config file");
                    load_error = Some(format!("{} is not valid TOML: {e}", p.display()));
                    DocumentMut::new()
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "could not read config file");
                    load_error = Some(format!("could not read {}: {e}", p.display()));
                    DocumentMut::new()
                }
            },
        };
        Self {
            load_error,
            ..Self::from_doc(path, doc)
        }
    }

    /// Like [`load`](ConfigFile::load), but fails when the file exists and cannot
    /// be read or parsed.
    ///
    /// Use this before a read-modify-write: [`load`](ConfigFile::load) degrades
    /// a broken file to an empty document, and saving that would silently
    /// overwrite whatever the user had.
    ///
    /// # Errors
    /// Returns an error (with a `fix`) when the file is unreadable or malformed.
    pub fn try_load(app_id: &str) -> crate::Result<Self> {
        let file = Self::load(app_id);
        file.check_loaded()?;
        Ok(file)
    }

    /// Fails when the file on disk was malformed or unreadable, so a caller
    /// that reports settings can say so instead of presenting an empty
    /// document's defaults as if they were the user's configuration.
    pub(crate) fn check_loaded(&self) -> crate::Result<()> {
        match &self.load_error {
            None => Ok(()),
            Some(reason) => Err(crate::config_schema::validation_error(
                format!("invalid config file: {reason}"),
                "Fix or delete the config file, then retry.",
            )),
        }
    }

    /// Returns the resolved config file path, if a config directory was
    /// available. `None` means neither `XDG_CONFIG_HOME`/`HOME` nor `APPDATA`
    /// resolved to an absolute path (so nothing can be loaded or saved).
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Deserializes the engine-reserved sections into an [`EngineConfig`].
    ///
    /// Lenient: any deserialization error (for example an invalid
    /// `[credentials].store`) yields [`EngineConfig::default`].
    #[must_use]
    pub fn engine(&self) -> EngineConfig {
        self.deserialize().unwrap_or_default()
    }

    /// Deserializes a single top-level table `name` into `T`, or `Ok(None)` when
    /// the key is absent.
    ///
    /// Use this to read a consumer-owned section such as `[deploy]`:
    /// `cfg.section::<DeployConfig>("deploy")?`.
    ///
    /// # Errors
    /// Returns an error when the config file on disk was malformed or
    /// unreadable (so a typo is never silently read as "unset"), or when the
    /// table is present but does not deserialize into `T`.
    pub fn section<T: DeserializeOwned>(&self, name: &str) -> crate::Result<Option<T>> {
        self.check_loaded()?;
        let item = match self.doc.get(name) {
            None => return Ok(None),
            Some(item) => item,
        };
        // Extract the section's key-value pairs into a temporary root-level
        // document so `from_document` sees a plain `T`-shaped struct.
        let mut tmp = DocumentMut::new();
        if let Some(tbl) = item.as_table_like() {
            for (k, v) in tbl.iter() {
                tmp[k] = v.clone();
            }
        }
        toml_edit::de::from_document::<T>(tmp)
            .map(Some)
            .map_err(|e| CliCoreError::message(format!("config section {name:?}: {e}")))
    }

    /// Deserializes the entire config file into a consumer root type `T`.
    ///
    /// The root type may include the engine-reserved sections alongside its own;
    /// unknown keys are tolerated when `T` allows them.
    ///
    /// # Errors
    /// Returns an error when the config file on disk was malformed or
    /// unreadable, or when the document does not deserialize into `T`.
    pub fn deserialize<T: DeserializeOwned>(&self) -> crate::Result<T> {
        self.check_loaded()?;
        toml_edit::de::from_document::<T>(self.doc.clone())
            .map_err(|e| CliCoreError::message(format!("config deserialize error: {e}")))
    }

    /// Returns the string form of the value at a dotted key (for example
    /// `credentials.store` or `deploy.region`), or `None` when absent.
    ///
    /// Scalars render without quotes; a table renders as its TOML fragment.
    #[must_use]
    pub fn get(&self, dotted_key: &str) -> Option<String> {
        let mut item = self.doc.as_item();
        for segment in dotted_key.split('.') {
            item = item.as_table_like()?.get(segment)?;
        }
        match item.as_value() {
            Some(toml_edit::Value::String(s)) => Some(s.value().clone()),
            Some(other) => Some(other.to_string().trim().to_owned()),
            None => Some(item.to_string()),
        }
    }

    /// Sets the value at a dotted key, creating intermediate tables as needed.
    ///
    /// `value` is coerced to a TOML scalar type using these rules (in order):
    /// 1. `"true"` / `"false"` (case-sensitive) → TOML boolean.
    /// 2. Any string parseable as an `i64` → TOML integer.
    /// 3. Any string parseable as an `f64` (including `"1e5"`, `"inf"`,
    ///    `"nan"`) → TOML float.
    /// 4. Everything else → TOML string.
    ///
    /// To force a value to be stored as a string when it looks numeric (e.g.
    /// a version like `"1.0"`), this API does not currently support quoting —
    /// wrap the value in the config file by hand.
    ///
    /// The engine-reserved `[credentials]` and `[output]` tables are
    /// validated: only their known keys (`credentials.store`,
    /// `output.format`) are accepted; unknown keys in those tables are
    /// rejected. Existing comments and formatting elsewhere in the file are
    /// preserved. Call [`save`](ConfigFile::save) to persist.
    ///
    /// # Errors
    /// Returns an error for an empty/invalid key, an unknown engine-reserved
    /// key, an invalid engine value, or a key whose parent path is not a
    /// table.
    pub fn set(&mut self, dotted_key: &str, value: &str) -> crate::Result<()> {
        // Validate engine-reserved keys under [credentials] and [output].
        // Only the documented key in each table is accepted; any other key
        // in those tables is rejected to prevent silently writing unknown
        // engine config that would be ignored (and confuse the user).
        let first_segment = dotted_key.split('.').next().unwrap_or("");
        if ENGINE_RESERVED_TABLES.contains(&first_segment) {
            match dotted_key {
                "credentials.store" => {
                    value
                        .parse::<CredentialStore>()
                        .map_err(|e| CliCoreError::message(e.to_string()))?;
                }
                "output.format" => {
                    if !crate::output::is_valid_output_format(&value.trim().to_ascii_lowercase()) {
                        return Err(CliCoreError::message(format!(
                            "invalid output format {value:?} (expected one of: json, human, toon)"
                        )));
                    }
                }
                other => {
                    return Err(CliCoreError::message(format!(
                        "unknown engine-reserved key {other:?}; the only supported keys are \
                         \"credentials.store\" and \"output.format\""
                    )));
                }
            }
        }
        self.set_value(dotted_key, infer_toml_value(value))
    }

    /// Writes an already-typed `value` at a dotted key, creating intermediate
    /// tables. Unlike [`set`](ConfigFile::set) this performs no validation or
    /// type inference; callers own both.
    pub(crate) fn set_value(
        &mut self,
        dotted_key: &str,
        value: toml_edit::Value,
    ) -> crate::Result<()> {
        let segments: Vec<&str> = dotted_key.split('.').collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(CliCoreError::message(format!(
                "invalid config key {dotted_key:?}"
            )));
        }
        let Some((last, parents)) = segments.split_last() else {
            return Err(CliCoreError::message("empty config key"));
        };
        let mut table = self.doc.as_table_mut();
        for segment in parents {
            let entry = table
                .entry(segment)
                .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
            table = entry.as_table_mut().ok_or_else(|| {
                CliCoreError::message(format!("config key {segment:?} is not a table"))
            })?;
        }
        table[last] = toml_edit::Item::Value(value);
        Ok(())
    }

    /// Returns the dotted path of every scalar or array value in the file, in
    /// document order. Tables are descended into, not listed themselves.
    ///
    /// A key that itself contains a `.` (written `"a.b" = 1`) is omitted: a
    /// dotted path cannot tell it apart from the nested key `a` → `b`, so it
    /// could not be read back with [`get`](ConfigFile::get) or removed with
    /// [`unset`](ConfigFile::unset).
    #[must_use]
    pub fn leaf_keys(&self) -> Vec<String> {
        fn walk(table: &dyn toml_edit::TableLike, prefix: &str, out: &mut Vec<String>) {
            for (key, item) in table.iter().filter(|(key, _)| !key.contains('.')) {
                let path = if prefix.is_empty() {
                    key.to_owned()
                } else {
                    format!("{prefix}.{key}")
                };
                match item.as_table_like() {
                    Some(child) => walk(child, &path, out),
                    None => out.push(path),
                }
            }
        }
        let mut out = Vec::new();
        walk(self.doc.as_table(), "", &mut out);
        out
    }

    /// Removes the value at a dotted key, returning whether anything was
    /// removed. Tables left empty by the removal are removed too, so unsetting
    /// the last key in a section does not leave a bare `[section]` behind.
    /// Comments and formatting elsewhere are preserved; call
    /// [`save`](ConfigFile::save) to persist.
    ///
    /// # Errors
    /// Returns an error for an empty/invalid key.
    pub fn unset(&mut self, dotted_key: &str) -> crate::Result<bool> {
        let segments: Vec<&str> = dotted_key.split('.').collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(CliCoreError::message(format!(
                "invalid config key {dotted_key:?}"
            )));
        }
        Ok(remove_path(self.doc.as_table_mut(), &segments))
    }

    /// Returns the value at a dotted key as typed JSON (booleans and numbers
    /// stay booleans and numbers), or `None` when absent.
    ///
    /// Unlike [`get`](ConfigFile::get), which flattens everything to a string.
    #[must_use]
    pub fn get_json(&self, dotted_key: &str) -> Option<serde_json::Value> {
        let mut item = self.doc.as_item();
        for segment in dotted_key.split('.') {
            item = item.as_table_like()?.get(segment)?;
        }
        Some(item_to_json(item))
    }

    /// Renders the whole config document back to a TOML string (preserving
    /// comments and formatting).
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        self.doc.to_string()
    }

    /// Persists the document to its config path via an atomic write.
    ///
    /// For a read-modify-write, start from [`try_load`](ConfigFile::try_load):
    /// a document from [`load`](ConfigFile::load) is empty when the file on
    /// disk was malformed, and saving it replaces the user's file.
    ///
    /// # Errors
    /// Returns an error when no config path is available (no resolvable config
    /// directory) or the write fails.
    pub fn save(&self) -> crate::Result<()> {
        let path = self.path.as_ref().ok_or_else(|| {
            CliCoreError::message(
                "no config path available (set XDG_CONFIG_HOME, HOME, or %APPDATA% \
                 to a directory)",
            )
        })?;
        crate::fs::write_string_atomic(path, &self.doc.to_string())
    }
}

fn remove_path(table: &mut dyn toml_edit::TableLike, segments: &[&str]) -> bool {
    let Some((first, rest)) = segments.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return table.remove(first).is_some();
    }
    let Some(child) = table
        .get_mut(first)
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        return false;
    };
    let removed = remove_path(child, rest);
    if removed && child.is_empty() {
        table.remove(first);
    }
    removed
}

fn item_to_json(item: &toml_edit::Item) -> serde_json::Value {
    use serde_json::Value;
    match item {
        toml_edit::Item::None => Value::Null,
        toml_edit::Item::Value(v) => value_to_json(v),
        toml_edit::Item::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.to_owned(), item_to_json(v)))
                .collect(),
        ),
        toml_edit::Item::ArrayOfTables(a) => Value::Array(
            a.iter()
                .map(|t| {
                    Value::Object(
                        t.iter()
                            .map(|(k, v)| (k.to_owned(), item_to_json(v)))
                            .collect(),
                    )
                })
                .collect(),
        ),
    }
}

fn value_to_json(value: &toml_edit::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        toml_edit::Value::String(s) => Value::String(s.value().clone()),
        toml_edit::Value::Integer(i) => Value::from(*i.value()),
        toml_edit::Value::Float(f) => Value::from(*f.value()),
        toml_edit::Value::Boolean(b) => Value::Bool(*b.value()),
        toml_edit::Value::Datetime(d) => Value::String(d.value().to_string()),
        toml_edit::Value::Array(a) => Value::Array(a.iter().map(value_to_json).collect()),
        toml_edit::Value::InlineTable(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.to_owned(), value_to_json(v)))
                .collect(),
        ),
    }
}

/// Parses `value` as a TOML bool/integer/float when possible, else a string.
fn infer_toml_value(value: &str) -> toml_edit::Value {
    if let Ok(b) = value.parse::<bool>() {
        return b.into();
    }
    if let Ok(i) = value.parse::<i64>() {
        return i.into();
    }
    if let Ok(f) = value.parse::<f64>() {
        return f.into();
    }
    value.into()
}

/// Test-only helpers for serializing and mutating `XDG_CONFIG_HOME`.
///
/// `set_var`/`remove_var` are `unsafe` in the Rust 2024 edition; [`XDG_TEST_MUTEX`]
/// serializes all access so usage here is data-race-free. Shared crate-wide so
/// every test that mutates `XDG_CONFIG_HOME` (in `config`, `auth::storage`, and
/// `auth::pkce`) contends on the *same* lock rather than racing across modules.
#[cfg(test)]
#[allow(unsafe_code, dead_code)]
pub(crate) mod test_env {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes access to `XDG_CONFIG_HOME` across all crate tests.
    pub(crate) static XDG_TEST_MUTEX: Mutex<()> = Mutex::new(());

    /// Acquires the shared lock (poison-tolerant). Hold it for the entire span
    /// during which `XDG_CONFIG_HOME` is mutated — including across `.await`
    /// points in async tests (`#[tokio::test]` uses a current-thread runtime,
    /// so the non-`Send` guard is fine).
    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        XDG_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// RAII guard that restores an env var to its prior value when dropped,
    /// including on panic. The caller must hold [`lock`] for the guard's life.
    pub(crate) struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvVarGuard {
        /// Sets `key` to `value` (or removes it when `None`), capturing the
        /// prior value for restoration on drop. Caller must hold [`lock`].
        pub(crate) fn set(key: &'static str, value: Option<&Path>) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: caller holds XDG_TEST_MUTEX, serializing all mutation.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: callers hold XDG_TEST_MUTEX for the guard's lifetime.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Runs `f` with `XDG_CONFIG_HOME` set to `value`, holding the shared lock
    /// and restoring the previous value afterward.
    pub(crate) fn with_xdg_config_home<F: FnOnce() -> R, R>(value: &Path, f: F) -> R {
        let _lock = lock();
        let _restore = EnvVarGuard::set("XDG_CONFIG_HOME", Some(value));
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_store_from_toml() {
        let config: EngineConfig =
            toml_edit::de::from_str("[credentials]\nstore = \"file\"\n").expect("valid toml");
        assert_eq!(config.credentials.store, Some(CredentialStore::File));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config: EngineConfig =
            toml_edit::de::from_str("future_section = true\n[credentials]\nstore = \"auto\"\n")
                .expect("unknown keys tolerated");
        assert_eq!(config.credentials.store, Some(CredentialStore::Auto));
    }

    #[test]
    fn config_file_path_rejects_unsafe_app_id() {
        assert_eq!(config_file_path("../evil"), None);
        assert_eq!(config_file_path("a/b"), None);
    }

    #[test]
    fn config_file_path_uses_xdg_config_home() {
        let dir = std::env::temp_dir().join("cli-engine-config-path-test");
        test_env::with_xdg_config_home(&dir, || {
            assert_eq!(
                config_file_path("myapp"),
                Some(dir.join("myapp").join("config.toml"))
            );
        });
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Deploy {
        region: String,
        replicas: u32,
    }

    fn doc_config(toml: &str) -> ConfigFile {
        ConfigFile::from_doc(None, toml.parse().expect("valid toml"))
    }

    #[test]
    fn section_reads_consumer_table() {
        let cfg = doc_config("[deploy]\nregion = \"us-west\"\nreplicas = 3\n");
        let deploy: Deploy = cfg.section("deploy").expect("ok").expect("present");
        assert_eq!(
            deploy,
            Deploy {
                region: "us-west".to_owned(),
                replicas: 3
            }
        );
        assert!(cfg.section::<Deploy>("absent").expect("ok").is_none());
    }

    #[test]
    fn engine_and_consumer_sections_coexist() {
        let cfg = doc_config(
            "[credentials]\nstore = \"file\"\n[deploy]\nregion = \"eu\"\nreplicas = 1\n",
        );
        assert_eq!(cfg.engine().credentials.store, Some(CredentialStore::File));
        assert_eq!(
            cfg.section::<Deploy>("deploy")
                .expect("ok")
                .expect("present")
                .region,
            "eu"
        );
    }

    #[test]
    fn get_reads_dotted_scalar() {
        let cfg = doc_config("[credentials]\nstore = \"file\"\n[deploy]\nreplicas = 3\n");
        assert_eq!(cfg.get("credentials.store").as_deref(), Some("file"));
        assert_eq!(cfg.get("deploy.replicas").as_deref(), Some("3"));
        assert_eq!(cfg.get("deploy.missing"), None);
        assert_eq!(cfg.get("nope.at.all"), None);
    }

    #[test]
    fn set_infers_scalar_types() {
        let mut cfg = ConfigFile::default();
        cfg.set("telemetry.enabled", "true").expect("set bool");
        cfg.set("deploy.replicas", "5").expect("set int");
        cfg.set("deploy.region", "us-west").expect("set str");
        assert_eq!(cfg.get("telemetry.enabled").as_deref(), Some("true"));
        assert_eq!(cfg.get("deploy.replicas").as_deref(), Some("5"));
        assert_eq!(cfg.get("deploy.region").as_deref(), Some("us-west"));
        // bool/int stored as scalars, not quoted strings
        assert!(cfg.doc.to_string().contains("enabled = true"));
        assert!(cfg.doc.to_string().contains("replicas = 5"));
    }

    #[test]
    fn set_validates_engine_store_key() {
        let mut cfg = ConfigFile::default();
        assert!(cfg.set("credentials.store", "bogus").is_err());
        assert!(cfg.set("credentials.store", "file").is_ok());
        assert_eq!(cfg.engine().credentials.store, Some(CredentialStore::File));
    }

    #[test]
    fn set_rejects_unknown_engine_reserved_keys() {
        let mut cfg = ConfigFile::default();
        // Unknown keys in [credentials] are rejected to prevent silent no-ops.
        assert!(
            cfg.set("credentials.unknown_future_key", "foo").is_err(),
            "unknown credentials key should be rejected"
        );
        assert!(
            cfg.set("credentials.timeout", "30").is_err(),
            "unknown credentials.timeout should be rejected"
        );
        // Consumer-owned tables are unrestricted.
        assert!(
            cfg.set("deploy.region", "us-west").is_ok(),
            "consumer-owned keys should be accepted"
        );
    }

    #[test]
    fn set_rejects_empty_key_segments() {
        let mut cfg = ConfigFile::default();
        assert!(cfg.set("a..b", "x").is_err());
        assert!(cfg.set("", "x").is_err());
    }

    #[test]
    fn set_preserves_comments_and_other_tables() {
        let mut cfg =
            doc_config("# keep me\n[credentials]\nstore = \"file\"\n\n[deploy]\nregion = \"us\"\n");
        cfg.set("deploy.region", "eu").expect("set");
        let rendered = cfg.doc.to_string();
        assert!(
            rendered.contains("# keep me"),
            "comment preserved: {rendered}"
        );
        assert!(
            rendered.contains("store = \"file\""),
            "other table preserved"
        );
        assert!(rendered.contains("region = \"eu\""), "value updated");
    }

    #[test]
    fn load_and_save_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        test_env::with_xdg_config_home(dir.path(), || {
            let mut cfg = ConfigFile::load("roundtrip");
            assert!(cfg.path().is_some());
            cfg.set("deploy.region", "us-west").expect("set");
            cfg.save().expect("save");
            // Reload from disk and confirm persistence.
            let reloaded = ConfigFile::load("roundtrip");
            assert_eq!(reloaded.get("deploy.region").as_deref(), Some("us-west"));
        });
    }

    #[test]
    fn malformed_file_loads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        test_env::with_xdg_config_home(dir.path(), || {
            let path = config_file_path("broken").expect("path");
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, "not = valid = toml").expect("write");
            let cfg = ConfigFile::load("broken");
            assert_eq!(cfg.engine().credentials.store, None);
            assert_eq!(cfg.get("anything"), None);
        });
    }

    #[test]
    fn default_config_has_no_path_and_save_errors() {
        let cfg = ConfigFile::default();
        assert!(cfg.path().is_none());
        assert!(cfg.save().is_err(), "save without a path should error");
    }

    #[test]
    fn leaf_keys_omit_keys_containing_a_dot() {
        let cfg = doc_config("\"a.b\" = 7\nplain = 1\n[a]\nb = 9\n");
        assert_eq!(
            cfg.leaf_keys(),
            ["plain", "a.b"],
            "only the nested a.b, once"
        );
        assert_eq!(cfg.get_json("a.b"), Some(serde_json::Value::from(9)));
    }
}
