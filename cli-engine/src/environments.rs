//! First-class environment definitions and layered resolution.
//!
//! An [`Environments`] value holds compiled-in environment TOML tables and,
//! optionally, an `environments.toml` file plus a fallback for names known to
//! neither. Resolving a name merges those layers (later wins, shallow —
//! no deep-merging into nested tables/arrays) into one [`EnvSource`], then
//! threads it (plus an app-scoped environment-variable source) through an
//! [`crate::env_config::EnvConfig`] struct's own assembly instructions.
//!
//! This module owns no per-field knowledge at all. Consumers can create structs
//! that use `#[derive(EnvConfig)]` to create strongly-typed environment
//! configs populated by [`Environments`].

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::env_config::{EnvConfig, EnvConfigError, EnvSource, EnvVarSource};
use crate::{Result, error::CliCoreError};

/// A consumer-supplied callback that defines an environment purely from outside
/// the compiled/file layers (for example, from environment variables) when a
/// name isn't known to either. See [`Environments::with_fallback`].
type EnvironmentFallback = Arc<dyn Fn(&str) -> Option<EnvTable> + Send + Sync>;

/// A compiled-in environment's raw configuration, expressed as a TOML table so
/// it can merge with the `environments.toml` file layer on equal footing.
/// Values accepted by [`EnvTable::with`] cover the common Rust literal types
/// (`&str`/`String`/`bool`/integers/floats and `Vec<T>` of those) via
/// [`Into<toml::Value>`], so a compiled-in environment reads like ordinary
/// Rust, not embedded TOML text.
#[derive(Debug, Clone, Default)]
pub struct EnvTable(toml::Table);

impl EnvTable {
    /// Creates an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self(toml::Table::new())
    }

    /// Sets `key` to `value`.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<toml::Value>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }
}

/// Engine-owned environment system: compiled/file tables + resolution +
/// active-env state.
#[derive(Clone)]
pub struct Environments {
    default: String,
    compiled: BTreeMap<String, EnvTable>,
    use_config_file: bool,
    app_id: String,
    file_path_override: Option<std::path::PathBuf>,
    fallback: Option<EnvironmentFallback>,
}

impl std::fmt::Debug for Environments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environments")
            .field("default", &self.default)
            .field("compiled", &self.compiled)
            .field("use_config_file", &self.use_config_file)
            .field("app_id", &self.app_id)
            .field("file_path_override", &self.file_path_override)
            .field("fallback", &self.fallback.is_some())
            .finish()
    }
}

impl Environments {
    /// Creates an environment system with the given default environment name.
    ///
    /// If `default_env` is sourced from the consumer's own persisted state,
    /// read that state *raw* rather than through anything that calls back
    /// into [`resolve`](Self::resolve) or a lazily-initialized singleton's
    /// `instance()`. A consumer wiring a lazy singleton whose default depends
    /// on its own config can otherwise deadlock re-entering that singleton's
    /// own initialization while it is still being constructed.
    #[must_use]
    pub fn new(default_env: impl Into<String>) -> Self {
        Self {
            default: default_env.into(),
            compiled: BTreeMap::new(),
            use_config_file: false,
            app_id: String::new(),
            file_path_override: None,
            fallback: None,
        }
    }

    /// Registers a compiled-in environment's table, merging onto whatever is
    /// already registered for `name` (later call wins, key-by-key — same
    /// overlay rule as the `environments.toml` file layer). Accepts an
    /// [`EnvTable`] directly, or any `#[derive(EnvConfig)]` struct *value* —
    /// the derive generates `impl From<Self> for EnvTable`, so a compiled-in
    /// environment can be written as a plain typed struct instead of a
    /// stringly-keyed builder:
    ///
    /// ```
    /// use cli_engine::{EnvConfig, environments::Environments};
    ///
    /// #[derive(Default, EnvConfig)]
    /// struct ApiConfig {
    ///     api_url: String,
    ///     #[env_config(default = String::new())]
    ///     client_id: String,
    /// }
    ///
    /// let environments = Environments::new("prod").with_environment(
    ///     "prod",
    ///     ApiConfig { api_url: "https://api.example.com".to_owned(), client_id: "abc".to_owned() },
    /// );
    /// # let _ = environments;
    /// ```
    ///
    /// A struct value has no "absent" state — every field is written, even
    /// ones left at their type's default — so splitting concerns across
    /// several smaller structs (each covering only the keys it cares about)
    /// composes better than one struct with placeholder values for fields it
    /// doesn't set; merging (rather than replacing) is what makes that
    /// composition possible across repeated calls for the same `name`.
    #[must_use]
    pub fn with_environment(mut self, name: impl Into<String>, table: impl Into<EnvTable>) -> Self {
        let table = table.into();
        self.compiled
            .entry(name.into())
            .and_modify(|existing| overlay(&mut existing.0, &table.0))
            .or_insert(table);
        self
    }

    /// Enables loading `<config-dir>/<app_id>/environments.toml` during resolution.
    #[must_use]
    pub fn with_config_file(mut self, enabled: bool) -> Self {
        self.use_config_file = enabled;
        self
    }

    /// Sets the application id used to locate the config file and as the
    /// prefix for the app-scoped environment-variable override tier (see
    /// [`resolve`](Self::resolve)).
    ///
    /// The consumer must set this to the same `app_id` passed to
    /// [`CliConfig::new`](crate::CliConfig::new) before sharing the
    /// [`Environments`] with both
    /// [`CliConfig::with_environments`](crate::CliConfig::with_environments) and
    /// `PkceAuthProvider::with_environments` (with the `pkce-auth` feature),
    /// or [`config_file_path`](Self::config_file_path) returns `None` and the
    /// `environments.toml` file layer silently resolves empty.
    #[must_use]
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = app_id.into();
        self
    }

    /// Test/advanced seam: force the environments file path.
    #[must_use]
    pub fn with_config_file_path_override(mut self, path: std::path::PathBuf) -> Self {
        self.file_path_override = Some(path);
        self.use_config_file = true;
        self
    }

    /// Registers an opt-in seam for defining an environment purely from
    /// outside the compiled/file layers.
    ///
    /// [`resolve`](Self::resolve)/[`source`](Self::source) — and therefore
    /// every path built on them, including the built-in `--env` flag and the
    /// `env` command group — consult `fallback` with the requested name
    /// whenever that name is unknown to both the compiled-in and
    /// `environments.toml` layers. Returning `Some(table)` lets a brand-new,
    /// never-declared name resolve (typically by having `fallback` read its
    /// own `<NAME>_*` environment variables and build a table from them);
    /// returning `None` preserves the existing "unknown environment" error.
    ///
    /// The returned [`EnvTable`] is treated the same as a compiled-in table:
    /// it does not skip the `environments.toml` layer, which still merges on
    /// top of it (later wins). `fallback` is never consulted for a name
    /// already known to the compiled-in or file layer.
    #[must_use]
    pub fn with_fallback<F>(mut self, fallback: F) -> Self
    where
        F: Fn(&str) -> Option<EnvTable> + Send + Sync + 'static,
    {
        self.fallback = Some(Arc::new(fallback));
        self
    }

    /// The default environment name.
    #[must_use]
    pub fn default_env(&self) -> &str {
        &self.default
    }

    /// The app id set via [`with_app_id`](Self::with_app_id), or empty if
    /// never set. Exposed so a consumer building its own
    /// [`crate::env_config::SourceChain`] (for example `PkceAuthProvider`,
    /// which has fallback tiers outside this system's own compiled/file
    /// layers) can reuse the same app-scoped environment-variable prefix that
    /// [`resolve`](Self::resolve) uses internally.
    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Enumerable environment names (compiled-in + file-defined), sorted.
    ///
    /// Any error from reading or parsing the environments file (missing file,
    /// permission/read error, or malformed TOML) is silently swallowed and only
    /// the compiled-in names are returned. Use [`source`](Self::source) or
    /// [`resolve`](Self::resolve) when you need those errors surfaced.
    ///
    /// # Blocking
    ///
    /// When the config-file layer is enabled, this performs synchronous
    /// filesystem I/O to read and parse `environments.toml` (like
    /// [`resolve`](Self::resolve)). Avoid calling it repeatedly on a
    /// latency-sensitive async path.
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = self.compiled.keys().cloned().collect();
        if let Ok(file) = self.file_tables() {
            names.extend(file.into_keys());
        }
        names.into_iter().collect()
    }

    /// Builds the merged [`EnvSource`] for `name`: the compiled-in table
    /// overlaid by the `environments.toml` file table for the same name
    /// (file wins key-by-key), or a registered [`with_fallback`](Self::with_fallback)
    /// table when `name` is unknown to both.
    ///
    /// This is the seam behind [`resolve`](Self::resolve), exposed directly
    /// for generic introspection (for example, `env info` printing whatever
    /// keys an environment's merged table actually has) without needing to
    /// know about any particular [`EnvConfig`] struct.
    ///
    /// # Blocking
    ///
    /// When the config-file layer is enabled, this performs synchronous
    /// filesystem I/O to read and parse `environments.toml`. Avoid calling it
    /// repeatedly on a latency-sensitive async path.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is not known to any layer (including a
    /// registered [`with_fallback`](Self::with_fallback)) or when the
    /// environments file exists but cannot be read or parsed.
    pub fn source(&self, name: &str) -> Result<EnvSource> {
        let compiled = self.compiled.get(name);
        let mut all_file_tables = self.file_tables()?;
        let file = all_file_tables.remove(name);
        // The fallback only ever introduces a name unknown to the compiled-in
        // and file layers; a name known to either never consults it.
        let fallback = if compiled.is_none() && file.is_none() {
            self.fallback.as_ref().and_then(|f| f(name))
        } else {
            None
        };
        if compiled.is_none() && file.is_none() && fallback.is_none() {
            let mut known: std::collections::BTreeSet<String> =
                self.compiled.keys().cloned().collect();
            known.extend(all_file_tables.into_keys());
            let known_list: Vec<String> = known.into_iter().collect();
            let known_display = if known_list.is_empty() {
                "(none defined)".to_owned()
            } else {
                known_list.join(", ")
            };
            return Err(CliCoreError::message(format!(
                "unknown environment {name:?}; known: {known_display}"
            )));
        }
        let mut merged = toml::Table::new();
        if let Some(table) = compiled {
            overlay(&mut merged, &table.0);
        }
        if let Some(table) = &fallback {
            overlay(&mut merged, &table.0);
        }
        if let Some(table) = &file {
            overlay(&mut merged, table);
        }
        Ok(EnvSource::new(name, merged))
    }

    /// Resolves `name` into a typed [`EnvConfig`] section: the common path,
    /// `T::assemble` over a chain of the app-scoped environment-variable
    /// source (see the design note below) and `name`'s merged [`EnvSource`].
    ///
    /// # Environment-variable overrides are app-scoped, not environment-scoped
    ///
    /// A field's `#[env_config(env = "SUFFIX")]` checks
    /// `<APP_ID_UPPER>_<SUFFIX>` here — not `<NAME_UPPER>_<SUFFIX>`. At any
    /// single resolution there is exactly one environment being asked about,
    /// so scoping the override variable by environment name buys nothing an
    /// app-scoped name doesn't already give for free, while a bare
    /// environment name as a prefix (`PROD_`, `DEV_`) is a real collision
    /// risk in a shared shell/CI environment that an app-scoped prefix
    /// (`GDDY_...`) avoids categorically. A consumer needing extra fallback
    /// tiers outside this system's own compiled/file/fallback layers (for
    /// example a legacy provider-scoped env var) builds its own
    /// [`crate::env_config::SourceChain`] and calls `T::assemble` directly —
    /// see `PkceAuthProvider`.
    ///
    /// # Blocking
    ///
    /// See [`source`](Self::source).
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as [`source`](Self::source),
    /// or when a field's present value fails to convert to its type, or a
    /// required field has no value in any source and no default (see
    /// [`EnvConfigError`]).
    pub fn resolve<T: EnvConfig>(&self, name: &str) -> std::result::Result<T, EnvConfigError> {
        let source = self.source(name)?;
        if self.app_id.is_empty() {
            let chain = crate::env_config::SourceChain::new().push(&source);
            T::assemble(&chain)
        } else {
            let app_scoped = EnvVarSource {
                prefix: self.app_id.to_uppercase(),
            };
            let chain = crate::env_config::SourceChain::new()
                .push(&app_scoped)
                .push(&source);
            T::assemble(&chain)
        }
    }

    /// Path to `environments.toml` next to the engine config file, or `None`
    /// when the file layer is disabled or the config dir cannot be determined.
    #[must_use]
    pub fn config_file_path(&self) -> Option<std::path::PathBuf> {
        if !self.use_config_file {
            return None;
        }
        let config = crate::config::config_file_path(&self.app_id)?;
        Some(config.with_file_name("environments.toml"))
    }

    fn effective_file_path(&self) -> Option<std::path::PathBuf> {
        if let Some(path) = &self.file_path_override {
            return Some(path.clone());
        }
        self.config_file_path()
    }

    /// Parses the environments file into a name -> table map. Missing file = empty.
    ///
    /// Also accepts a legacy, undocumented nested top-level `[environments.prod]`
    /// table alongside the recommended flat `[prod]` shape, so files already
    /// written against that shape keep parsing without being rewritten. When a
    /// name appears under both, the nested entry's keys win, per-key.
    fn file_tables(&self) -> Result<BTreeMap<String, toml::Table>> {
        let Some(path) = self.effective_file_path() else {
            return Ok(BTreeMap::new());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(err) => {
                return Err(CliCoreError::message(format!(
                    "reading environments file {path:?}: {err}"
                )));
            }
        };
        let top: toml::Table = toml::from_str(&text).map_err(|err| {
            CliCoreError::message(format!("parsing environments file {path:?}: {err}"))
        })?;
        let mut tables: BTreeMap<String, toml::Table> = BTreeMap::new();
        for (name, value) in &top {
            if name == "environments" {
                continue;
            }
            if let Some(table) = value.as_table() {
                tables.insert(name.clone(), table.clone());
            }
        }
        if let Some(nested) = top.get("environments").and_then(toml::Value::as_table) {
            for (name, value) in nested {
                let Some(table) = value.as_table() else {
                    continue;
                };
                match tables.get_mut(name) {
                    Some(existing) => overlay(existing, table),
                    None => {
                        tables.insert(name.clone(), table.clone());
                    }
                }
            }
        }
        Ok(tables)
    }

    /// Config-file key under which the sticky active environment is stored.
    pub(crate) const ACTIVE_ENV_KEY: &'static str = "environment.active";

    /// Reads the persisted active environment from a loaded config file.
    #[must_use]
    pub fn active_from_config(config: &crate::config::ConfigFile) -> Option<String> {
        config.get(Self::ACTIVE_ENV_KEY)
    }

    /// Resolves the active environment name with precedence:
    /// explicit `--env` override > persisted active > configured default.
    #[must_use]
    pub fn effective_active(
        &self,
        flag: Option<&str>,
        config: &crate::config::ConfigFile,
    ) -> String {
        flag.map(ToOwned::to_owned)
            .or_else(|| Self::active_from_config(config))
            .unwrap_or_else(|| self.default.clone())
    }

    /// Persists `name` as the active environment (loads, sets, saves a fresh
    /// config file for `app_id`). Validates that `name` resolves first.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` does not resolve to a known environment, or
    /// when the config file cannot be written.
    pub fn persist_active(&self, name: &str) -> Result<()> {
        self.source(name)?; // reject unknown names
        // Persisting writes the engine config file, which is keyed by app_id.
        // Validate it up front so a missing/invalid app_id yields a clear,
        // actionable error rather than a misleading "no config path" failure
        // from ConfigFile::save() that points at XDG/HOME.
        if crate::config::config_file_path(&self.app_id).is_none() {
            return Err(CliCoreError::message(format!(
                "cannot persist active environment {name:?}: the environment system has no usable app_id; \
                 set one via Environments::with_app_id (matching the CliConfig app_id)"
            )));
        }
        let mut config = crate::config::ConfigFile::load(&self.app_id);
        config.set(Self::ACTIVE_ENV_KEY, name)?;
        config.save()
    }
}

/// Copies every key from `src` into `dst`, overwriting; shallow — a nested
/// table or array value replaces `dst`'s prior value wholesale, it is never
/// merged into.
fn overlay(dst: &mut toml::Table, src: &toml::Table) {
    for (key, value) in src {
        dst.insert(key.clone(), value.clone());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
mod tests {
    use super::*;
    use cli_engine_macros::EnvConfig as DeriveEnvConfig;

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that removes an env var on drop, even if a test panics.
    struct EnvGuard(&'static str);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: test holds ENV_LOCK; clean up on any exit including panic.
            unsafe { std::env::remove_var(self.0) }
        }
    }

    #[derive(Debug, Clone, DeriveEnvConfig)]
    struct OAuthLike {
        client_id: String,
        #[env_config(default = String::new())]
        auth_url: String,
        #[env_config(default = String::new())]
        token_url: String,
        #[env_config(default = Vec::new())]
        scopes: Vec<String>,
    }

    #[derive(Debug, Clone, DeriveEnvConfig)]
    struct ApiLike {
        #[env_config(env = "API_URL")]
        api_url: String,
    }

    fn sample() -> Environments {
        Environments::new("prod")
            .with_environment(
                "prod",
                EnvTable::new()
                    .with("client_id", "prod-client")
                    .with("auth_url", "https://api.example.com/authorize")
                    .with("token_url", "https://api.example.com/token")
                    .with("scopes", vec!["openid".to_owned()])
                    .with("api_url", "https://api.example.com"),
            )
            .with_environment("dev", EnvTable::new().with("client_id", "dev-client"))
    }

    #[test]
    fn resolve_unknown_env_with_no_defs_uses_placeholder() {
        let err = Environments::new("prod")
            .source("prod")
            .expect_err("nothing defined should fail");
        let message = err.to_string();
        assert!(
            message.contains("(none defined)"),
            "expected placeholder, got: {message}"
        );
    }

    #[test]
    fn persist_active_without_app_id_errors_clearly() {
        // `persist_active` resolves "prod" internally, which reads the same
        // PROD_* env vars the tests above mutate; take ENV_LOCK so a
        // concurrently-running test can't inject a value (e.g. an invalid
        // PROD_MIN_STAGE) that fails resolution for an unrelated reason.
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let err = sample()
            .persist_active("prod")
            .expect_err("persist without app_id should fail");
        let message = err.to_string();
        assert!(
            message.contains("app_id"),
            "error should mention app_id, got: {message}"
        );
    }

    #[test]
    fn builder_registers_compiled_environment() {
        let envs = Environments::new("prod")
            .with_environment("prod", EnvTable::new().with("client_id", "prod-client"));
        assert_eq!(envs.default_env(), "prod");
        assert_eq!(envs.list(), vec!["prod".to_owned()]);
    }

    /// A struct value works as a compiled-in environment, not just an
    /// `EnvTable` — the derive's `impl From<T> for EnvTable` maps each field
    /// by its own `key`.
    #[test]
    fn with_environment_accepts_a_typed_struct_value() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod").with_environment(
            "prod",
            OAuthLike {
                client_id: "prod-client".to_owned(),
                auth_url: "https://api.example.com/authorize".to_owned(),
                token_url: "https://api.example.com/token".to_owned(),
                scopes: vec!["openid".to_owned()],
            },
        );
        let oauth: OAuthLike = envs.resolve("prod").expect("prod resolves");
        assert_eq!(oauth.client_id, "prod-client");
        assert_eq!(oauth.auth_url, "https://api.example.com/authorize");
    }

    /// Two `with_environment` calls for the same name merge (later call wins
    /// per key) rather than the second replacing the first outright — this is
    /// what lets a consumer split one environment's compiled defaults across
    /// several small structs instead of one struct with placeholder fields
    /// for keys it doesn't set.
    #[test]
    fn with_environment_merges_across_repeated_calls_for_the_same_name() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod")
            .with_environment("prod", EnvTable::new().with("client_id", "prod-client"))
            .with_environment(
                "prod",
                EnvTable::new().with("api_url", "https://api.example.com"),
            );
        let oauth: OAuthLike = envs.resolve("prod").expect("prod resolves");
        assert_eq!(oauth.client_id, "prod-client", "first call's key survives");
        let api: ApiLike = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            api.api_url, "https://api.example.com",
            "second call's key is also present"
        );
    }

    #[test]
    fn resolve_returns_compiled_record() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let oauth: OAuthLike = sample().resolve("prod").expect("prod resolves");
        assert_eq!(oauth.client_id, "prod-client");
        assert_eq!(oauth.auth_url, "https://api.example.com/authorize");
        assert_eq!(oauth.token_url, "https://api.example.com/token");
        assert_eq!(oauth.scopes, vec!["openid".to_owned()]);
    }

    #[test]
    fn resolve_unknown_env_errors_with_known_names() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let err = sample().source("nope").unwrap_err().to_string();
        assert!(err.contains("nope"));
        assert!(err.contains("prod") && err.contains("dev"));
    }

    #[test]
    fn app_scoped_env_var_overrides_toml_value() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit.
        unsafe { std::env::set_var("MYAPP_API_URL", "https://override.example.com") };
        let _guard = EnvGuard("MYAPP_API_URL");

        let envs = sample().with_app_id("myapp");
        let api: ApiLike = envs.resolve("prod").expect("prod resolves");
        assert_eq!(api.api_url, "https://override.example.com");
    }

    #[test]
    fn environments_file_path_sits_next_to_config() {
        let envs = sample().with_app_id("gddy").with_config_file(true);
        let path = envs.config_file_path().expect("path resolves with app id");
        assert!(path.ends_with("gddy/environments.toml"), "got {path:?}");
    }

    #[test]
    fn file_layer_overrides_compiled_and_adds_custom_env() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[prod]
client_id = "file-client"

[custom]
client_id = "custom-client"
api_url = "https://api.custom.example.com"
"#,
        )
        .expect("write file");

        let envs = sample()
            .with_config_file(true)
            .with_config_file_path_override(file);

        let prod: OAuthLike = envs.resolve("prod").expect("prod");
        assert_eq!(prod.client_id, "file-client");
        let prod_api: ApiLike = envs.resolve("prod").expect("prod");
        assert_eq!(prod_api.api_url, "https://api.example.com");

        let custom: OAuthLike = envs.resolve("custom").expect("custom");
        assert_eq!(custom.client_id, "custom-client");
        assert!(envs.list().contains(&"custom".to_owned()));
    }

    /// gddy's already-distributed `environments.toml` nests every entry under
    /// a top-level `[environments]` table (mirroring its own hand-rolled
    /// `EnvironmentsFile { environments: BTreeMap<..> }`), unlike cli-engine's
    /// flat `[<name>]` shape. Those files must parse with zero edits.
    #[test]
    fn nested_environments_table_shape_parses_like_flat_shape() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[environments.dev]
api_url = "https://api.dev-godaddy.com"
client_id = "94488449-5769-4ecf-8bf4-9f8aa83859a3"

[environments.test]
api_url = "https://api.test-godaddy.com"
client_id = "e710d8b9-f4e5-4178-b1bf-98dfcd15d4ed"
"#,
        )
        .expect("write file");

        let envs = Environments::new("prod")
            .with_config_file(true)
            .with_config_file_path_override(file);

        let dev: OAuthLike = envs.resolve("dev").expect("dev");
        assert_eq!(dev.client_id, "94488449-5769-4ecf-8bf4-9f8aa83859a3");

        let test: OAuthLike = envs.resolve("test").expect("test");
        assert_eq!(test.client_id, "e710d8b9-f4e5-4178-b1bf-98dfcd15d4ed");
        assert!(envs.list().contains(&"dev".to_owned()));
        assert!(envs.list().contains(&"test".to_owned()));
    }

    /// When a name appears in both the flat top-level shape and the nested
    /// `[environments.<name>]` shape, the nested entry's fields win, and
    /// fields it doesn't set still fall back to the flat entry.
    #[test]
    fn nested_environments_table_wins_over_flat_entry_for_same_name() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[prod]
client_id = "flat-client"
api_url = "https://api.flat.example.com"

[environments.prod]
client_id = "nested-client"
"#,
        )
        .expect("write file");

        let envs = Environments::new("prod")
            .with_config_file(true)
            .with_config_file_path_override(file);

        let prod: OAuthLike = envs.resolve("prod").expect("prod");
        assert_eq!(prod.client_id, "nested-client");
        let prod_api: ApiLike = envs.resolve("prod").expect("prod");
        assert_eq!(prod_api.api_url, "https://api.flat.example.com");
    }

    const ACTIVE_KEY: &str = "environment.active";

    #[test]
    fn active_env_round_trips_through_config_file() {
        use crate::config::ConfigFile;
        let mut cfg = ConfigFile::default();
        assert_eq!(Environments::active_from_config(&cfg), None);

        cfg.set(ACTIVE_KEY, "ote").expect("set");
        assert_eq!(
            Environments::active_from_config(&cfg).as_deref(),
            Some("ote")
        );
    }

    #[test]
    fn effective_active_prefers_override_then_config_then_default() {
        use crate::config::ConfigFile;
        let envs = sample();
        let mut cfg = ConfigFile::default();
        cfg.set(ACTIVE_KEY, "dev").expect("set");

        assert_eq!(envs.effective_active(Some("prod"), &cfg), "prod"); // explicit wins
        assert_eq!(envs.effective_active(None, &cfg), "dev"); // config next
        let empty = ConfigFile::default();
        assert_eq!(envs.effective_active(None, &empty), "prod"); // default last
    }

    #[test]
    fn fallback_resolves_a_name_unknown_to_compiled_and_file_layers() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = sample().with_fallback(|name| {
            Some(EnvTable::new().with("client_id", format!("{name}-fallback-client")))
        });
        let env: OAuthLike = envs.resolve("throwaway").expect("fallback should resolve");
        assert_eq!(env.client_id, "throwaway-fallback-client");
    }

    /// A fallback returning `None` preserves the original "unknown environment"
    /// error, including the known-names listing.
    #[test]
    fn fallback_returning_none_preserves_unknown_env_error() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = sample().with_fallback(|_name| None);
        let err = envs.source("nope").unwrap_err().to_string();
        assert!(err.contains("nope"));
        assert!(err.contains("prod") && err.contains("dev"));
    }

    /// The fallback is never consulted for a name already known to the
    /// compiled-in layer — a fallback that would yield different values must
    /// not be able to shadow it.
    #[test]
    fn fallback_is_not_consulted_for_a_known_name() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = sample()
            .with_fallback(|_name| Some(EnvTable::new().with("client_id", "should-not-win")));
        let env: OAuthLike = envs.resolve("prod").expect("prod resolves");
        assert_eq!(env.client_id, "prod-client");
    }

    /// Mirrors gddy's DEVEX-947 case: a brand-new environment name, never
    /// declared in the compiled-in or file layers, becomes selectable purely
    /// because its own `<NAME>_API_URL`-style env var is set.
    #[test]
    fn fallback_plus_env_var_layer_defines_a_brand_new_environment() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit.
        unsafe { std::env::set_var("THROWAWAY_API_URL", "https://api.throwaway.example.com") };
        let _guard = EnvGuard("THROWAWAY_API_URL");

        let envs = sample().with_fallback(|name| {
            std::env::var(format!("{}_API_URL", name.to_uppercase()))
                .ok()
                .map(|api_url| EnvTable::new().with("api_url", api_url))
        });
        let env: ApiLike = envs.resolve("throwaway").expect("fallback should resolve");
        assert_eq!(env.api_url, "https://api.throwaway.example.com");
    }
}
