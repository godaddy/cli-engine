//! Engine configuration file and credential-storage selection.
//!
//! cli-engine reads an optional per-application TOML config file at
//! `<config-base>/<app_id>/config.toml`, where `<config-base>` is
//! `$XDG_CONFIG_HOME`, `$HOME/.config`, or `%APPDATA%` (see
//! [`config_base_dir`](crate::fs::config_base_dir)).
//! Loading is best-effort: a missing file yields defaults, and a malformed file
//! logs a warning and falls back to defaults rather than failing the command.
//!
//! The primary setting today selects where credentials are stored — see
//! [`CredentialStore`]. The effective mode is resolved with the precedence
//!
//! ```text
//! --credential-store flag  >  ${PREFIX}_CREDENTIAL_STORE env  >  config file  >  default (Keyring)
//! ```
//!
//! where `${PREFIX}` is the app id sanitized by
//! [`app_id_env_prefix`](crate::flags::app_id_env_prefix). See
//! [`resolve_credential_store`] and the pure [`resolve_credential_store_with`].

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use toml_edit::DocumentMut;

use crate::error::CliCoreError;

/// Where an auth provider stores credentials.
///
/// The variant selects a concrete storage backend
/// (see [`crate::auth::storage`]). `Keyring` is the default and preserves the
/// historical behavior (system keychain only, hard error when unavailable);
/// `File` is the escape hatch for environments without a working keychain
/// (headless Linux, WSL).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialStore {
    /// Try the system keychain; transparently fall back to an unencrypted file
    /// when the keychain backend is unavailable.
    Auto,
    /// System keychain only. A keychain failure is a hard error and no file is
    /// ever written. This is the default.
    #[default]
    Keyring,
    /// File only: never contact the system keychain. Credentials are written as
    /// unencrypted JSON under the config base directory.
    File,
}

impl CredentialStore {
    /// Returns the lowercase canonical name (`auto`, `keyring`, or `file`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialStore::Auto => "auto",
            CredentialStore::Keyring => "keyring",
            CredentialStore::File => "file",
        }
    }
}

impl std::fmt::Display for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string does not name a [`CredentialStore`] variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCredentialStoreError(String);

impl std::fmt::Display for ParseCredentialStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid credential store {:?} (expected one of: auto, keyring, file)",
            self.0
        )
    }
}

impl std::error::Error for ParseCredentialStoreError {}

impl FromStr for CredentialStore {
    type Err = ParseCredentialStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(CredentialStore::Auto),
            // `keychain` is accepted as an alias for the keychain-only mode.
            "keyring" | "keychain" => Ok(CredentialStore::Keyring),
            "file" => Ok(CredentialStore::File),
            _ => Err(ParseCredentialStoreError(s.to_owned())),
        }
    }
}

impl<'de> Deserialize<'de> for CredentialStore {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Top-level engine configuration parsed from `config.toml`.
///
/// Unknown keys are ignored so older binaries tolerate config written for newer
/// ones. New sections can be added as additional fields over time.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Credential-storage settings (`[credentials]` table).
    pub credentials: CredentialsConfig,
}

/// The `[credentials]` table of the engine config file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CredentialsConfig {
    /// Selected credential store, or `None` when the key is absent.
    pub store: Option<CredentialStore>,
}

// Per-thread override from the `--credential-store` global flag.
//
// Stored in a `thread_local!` `Cell` so concurrent `Cli::run` calls on
// different OS threads cannot interfere with each other. Each thread writes
// its own flag at the start of `Cli::run` (via `set_credential_store_flag`)
// and clears it at the end (via `clear_credential_store_flag`).
//
// Limitation: concurrent `Cli::run` calls sharing the same OS thread (e.g.
// concurrent tokio tasks on a single-threaded runtime) are not supported —
// the second call will observe the first run's flag. This scenario is atypical
// for a CLI library.
thread_local! {
    static CREDENTIAL_STORE_FLAG: Cell<u8> = const { Cell::new(0) };
}

fn encode_store(store: Option<CredentialStore>) -> u8 {
    match store {
        None => 0,
        Some(CredentialStore::Auto) => 1,
        Some(CredentialStore::Keyring) => 2,
        Some(CredentialStore::File) => 3,
    }
}

fn decode_store(byte: u8) -> Option<CredentialStore> {
    match byte {
        1 => Some(CredentialStore::Auto),
        2 => Some(CredentialStore::Keyring),
        3 => Some(CredentialStore::File),
        _ => None,
    }
}

/// Records the value of the `--credential-store` flag for the current thread.
///
/// Called at the start of each CLI run with the parsed flag value (`None` when
/// the flag was not supplied). Crate-internal: only the engine publishes
/// per-run flag state, so library consumers cannot mutate this latch.
pub(crate) fn set_credential_store_flag(store: Option<CredentialStore>) {
    CREDENTIAL_STORE_FLAG.with(|f| f.set(encode_store(store)));
}

/// Clears the thread-local flag set by [`set_credential_store_flag`].
///
/// Called at the end of each `Cli::run` so the flag does not leak into
/// subsequent sequential runs on the same thread.
pub(crate) fn clear_credential_store_flag() {
    CREDENTIAL_STORE_FLAG.with(|f| f.set(0));
}

/// Returns the flag override recorded by [`set_credential_store_flag`], if any.
/// Crate-internal accessor for the per-thread flag latch.
#[must_use]
pub(crate) fn credential_store_flag() -> Option<CredentialStore> {
    CREDENTIAL_STORE_FLAG.with(|f| decode_store(f.get()))
}

/// Derives the credential-store override env var from an app id, e.g.
/// `godaddy` -> `GODADDY_CREDENTIAL_STORE`.
#[must_use]
pub fn credential_store_env_var(app_id: &str) -> String {
    format!(
        "{}_CREDENTIAL_STORE",
        crate::flags::app_id_env_prefix(app_id)
    )
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
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self::from_doc(None, DocumentMut::new())
    }
}

impl ConfigFile {
    fn from_doc(path: Option<PathBuf>, doc: DocumentMut) -> Self {
        Self { path, doc }
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
        let doc = match &path {
            None => DocumentMut::new(),
            Some(p) => match std::fs::read_to_string(p) {
                Ok(contents) => contents.parse::<DocumentMut>().unwrap_or_else(|e| {
                    tracing::warn!(path = %p.display(), error = %e, "ignoring malformed config file");
                    DocumentMut::new()
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "could not read config file");
                    DocumentMut::new()
                }
            },
        };
        Self::from_doc(path, doc)
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
    /// Returns an error when the table is present but does not deserialize into
    /// `T`.
    pub fn section<T: DeserializeOwned>(&self, name: &str) -> crate::Result<Option<T>> {
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
    /// Returns an error when the document does not deserialize into `T`.
    pub fn deserialize<T: DeserializeOwned>(&self) -> crate::Result<T> {
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
    /// The engine-reserved `[credentials]` table is validated: only the known
    /// key `credentials.store` is accepted; unknown keys in that table are
    /// rejected. Existing comments and formatting elsewhere in the file are
    /// preserved. Call [`save`](ConfigFile::save) to persist.
    ///
    /// # Errors
    /// Returns an error for an empty/invalid key, an unknown engine-reserved
    /// key, an invalid engine value, or a key whose parent path is not a
    /// table.
    pub fn set(&mut self, dotted_key: &str, value: &str) -> crate::Result<()> {
        // Validate engine-reserved keys under [credentials].
        // Only the documented key `credentials.store` is accepted; any other
        // key in that table is rejected to prevent silently writing unknown
        // engine config that would be ignored (and confuse the user).
        const ENGINE_RESERVED_TABLES: &[&str] = &["credentials"];
        let first_segment = dotted_key.split('.').next().unwrap_or("");
        if ENGINE_RESERVED_TABLES.contains(&first_segment) {
            match dotted_key {
                "credentials.store" => {
                    value
                        .parse::<CredentialStore>()
                        .map_err(|e| CliCoreError::message(e.to_string()))?;
                }
                other => {
                    return Err(CliCoreError::message(format!(
                        "unknown engine-reserved key {other:?}; \
                         the only supported key in [credentials] is \"credentials.store\""
                    )));
                }
            }
        }
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
        table[last] = toml_edit::Item::Value(infer_toml_value(value));
        Ok(())
    }

    /// Renders the whole config document back to a TOML string (preserving
    /// comments and formatting).
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        self.doc.to_string()
    }

    /// Persists the document to its config path via an atomic write.
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

/// Resolves the effective [`CredentialStore`] from explicit inputs.
///
/// Pure and side-effect free so the precedence is unit-testable without touching
/// process state. Precedence (highest first): CLI `flag`, then `env` (an invalid
/// value is logged and ignored, falling through), then the config `file`, then
/// the default [`CredentialStore::Keyring`].
#[must_use]
pub fn resolve_credential_store_with(
    flag: Option<CredentialStore>,
    env: Option<&str>,
    file: &EngineConfig,
) -> CredentialStore {
    if let Some(store) = flag {
        return store;
    }
    if let Some(raw) = env {
        match raw.parse::<CredentialStore>() {
            Ok(store) => return store,
            Err(e) => tracing::warn!(error = %e, "ignoring invalid credential-store env var"),
        }
    }
    if let Some(store) = file.credentials.store {
        return store;
    }
    CredentialStore::default()
}

/// Resolves the effective [`CredentialStore`] for `app_id` against process state.
///
/// Reads the CLI-flag override (`credential_store_flag`), the
/// `${PREFIX}_CREDENTIAL_STORE` env var via the injected `var` getter, and the
/// config file ([`load`]), then applies [`resolve_credential_store_with`]. The
/// `var` getter is injected so callers/tests can supply environment lookups
/// without mutating the process environment.
pub fn resolve_credential_store(
    app_id: &str,
    var: impl Fn(&str) -> Option<String>,
) -> CredentialStore {
    let env = var(&credential_store_env_var(app_id));
    let file = load(app_id);
    resolve_credential_store_with(credential_store_flag(), env.as_deref(), &file)
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
    fn parses_known_variants_case_insensitively() {
        assert_eq!("auto".parse(), Ok(CredentialStore::Auto));
        assert_eq!("Keyring".parse(), Ok(CredentialStore::Keyring));
        assert_eq!("KEYCHAIN".parse(), Ok(CredentialStore::Keyring));
        assert_eq!("  file  ".parse(), Ok(CredentialStore::File));
    }

    #[test]
    fn rejects_unknown_variant() {
        let err = "vault"
            .parse::<CredentialStore>()
            .expect_err("should reject");
        assert!(err.to_string().contains("vault"));
    }

    #[test]
    fn display_round_trips_through_from_str() {
        for store in [
            CredentialStore::Auto,
            CredentialStore::Keyring,
            CredentialStore::File,
        ] {
            assert_eq!(store.to_string().parse(), Ok(store));
        }
    }

    #[test]
    fn env_var_name_is_derived_from_app_id() {
        assert_eq!(
            credential_store_env_var("godaddy"),
            "GODADDY_CREDENTIAL_STORE"
        );
        assert_eq!(
            credential_store_env_var("my-cli"),
            "MY_CLI_CREDENTIAL_STORE"
        );
    }

    #[test]
    fn deserializes_store_from_toml() {
        let config: EngineConfig =
            toml_edit::de::from_str("[credentials]\nstore = \"file\"\n").expect("valid toml");
        assert_eq!(config.credentials.store, Some(CredentialStore::File));
    }

    #[test]
    fn deserialize_rejects_bad_store_value() {
        let result = toml_edit::de::from_str::<EngineConfig>("[credentials]\nstore = \"nope\"\n");
        assert!(result.is_err(), "bad store value should fail to parse");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config: EngineConfig =
            toml_edit::de::from_str("future_section = true\n[credentials]\nstore = \"auto\"\n")
                .expect("unknown keys tolerated");
        assert_eq!(config.credentials.store, Some(CredentialStore::Auto));
    }

    #[test]
    fn resolution_precedence_flag_beats_env_beats_file() {
        let file = EngineConfig {
            credentials: CredentialsConfig {
                store: Some(CredentialStore::Keyring),
            },
        };
        // flag wins over everything
        assert_eq!(
            resolve_credential_store_with(Some(CredentialStore::Auto), Some("file"), &file),
            CredentialStore::Auto
        );
        // env wins over file
        assert_eq!(
            resolve_credential_store_with(None, Some("file"), &file),
            CredentialStore::File
        );
        // file wins over default
        assert_eq!(
            resolve_credential_store_with(None, None, &file),
            CredentialStore::Keyring
        );
    }

    #[test]
    fn resolution_defaults_to_keyring() {
        assert_eq!(
            resolve_credential_store_with(None, None, &EngineConfig::default()),
            CredentialStore::Keyring
        );
    }

    #[test]
    fn resolution_ignores_invalid_env_and_falls_through() {
        let file = EngineConfig {
            credentials: CredentialsConfig {
                store: Some(CredentialStore::File),
            },
        };
        // invalid env is ignored, so the file value applies
        assert_eq!(
            resolve_credential_store_with(None, Some("garbage"), &file),
            CredentialStore::File
        );
        // invalid env with no file falls through to the default
        assert_eq!(
            resolve_credential_store_with(None, Some("garbage"), &EngineConfig::default()),
            CredentialStore::Keyring
        );
    }

    #[test]
    fn config_file_path_rejects_unsafe_app_id() {
        assert_eq!(config_file_path("../evil"), None);
        assert_eq!(config_file_path("a/b"), None);
    }

    #[test]
    fn credential_store_flag_encodes_round_trips() {
        for store in [
            None,
            Some(CredentialStore::Auto),
            Some(CredentialStore::Keyring),
            Some(CredentialStore::File),
        ] {
            assert_eq!(decode_store(encode_store(store)), store);
        }
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
}
