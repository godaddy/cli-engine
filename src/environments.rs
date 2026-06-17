//! First-class environment definitions and layered resolution.
//!
//! An [`Environments`] value holds compiled-in environment definitions and,
//! optionally, an `environments.toml` file plus `<ENV>_*` env-var overrides.
//! Resolving a name merges those layers (later wins) into an [`Environment`].

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{Result, error::CliCoreError};

/// Standard OAuth slice of an environment, consumed by `PkceAuthProvider`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OAuthConfig {
    /// OAuth client id.
    pub client_id: String,
    /// Authorization endpoint.
    pub auth_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Default scopes.
    pub scopes: Vec<String>,
}

/// A fully-resolved environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// Environment name (e.g. `prod`).
    pub name: String,
    /// OAuth configuration, present when the environment participates in OAuth.
    pub oauth: Option<OAuthConfig>,
    /// App-specific fields (for example `api_url`).
    pub extra: BTreeMap<String, String>,
}

/// An unresolved per-environment declaration (one layer of configuration).
///
/// Fields are optional so layers can override individual values during
/// resolution. The same shape parses the `environments.toml` file.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct EnvironmentDef {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    auth_url: Option<String>,
    #[serde(default)]
    token_url: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    /// Everything not recognised above is captured here (app-specific fields).
    #[serde(flatten, default)]
    extra: BTreeMap<String, String>,
}

impl EnvironmentDef {
    /// Creates an empty declaration; every field falls back to lower layers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the OAuth client id.
    #[must_use]
    pub fn with_client_id(mut self, value: impl Into<String>) -> Self {
        self.client_id = Some(value.into());
        self
    }

    /// Sets the authorization endpoint.
    #[must_use]
    pub fn with_auth_url(mut self, value: impl Into<String>) -> Self {
        self.auth_url = Some(value.into());
        self
    }

    /// Sets the token endpoint.
    #[must_use]
    pub fn with_token_url(mut self, value: impl Into<String>) -> Self {
        self.token_url = Some(value.into());
        self
    }

    /// Sets the default scopes.
    #[must_use]
    pub fn with_scopes(mut self, scopes: &[impl AsRef<str>]) -> Self {
        self.scopes = Some(scopes.iter().map(|s| s.as_ref().to_owned()).collect());
        self
    }

    /// Sets an app-specific bag field.
    #[must_use]
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }
}

/// Engine-owned environment system: definitions + resolution + active-env state.
#[derive(Clone, Debug)]
pub struct Environments {
    default: String,
    defs: BTreeMap<String, EnvironmentDef>,
    use_config_file: bool,
    app_id: String,
    file_path_override: Option<std::path::PathBuf>,
}

impl Environments {
    /// Creates an environment system with the given default environment name.
    #[must_use]
    pub fn new(default_env: impl Into<String>) -> Self {
        Self {
            default: default_env.into(),
            defs: BTreeMap::new(),
            use_config_file: false,
            app_id: String::new(),
            file_path_override: None,
        }
    }

    /// Registers (or replaces) a compiled-in environment definition.
    #[must_use]
    pub fn with_environment(mut self, name: impl Into<String>, def: EnvironmentDef) -> Self {
        self.defs.insert(name.into(), def);
        self
    }

    /// Enables loading `<config-dir>/<app_id>/environments.toml` during resolution.
    #[must_use]
    pub fn with_config_file(mut self, enabled: bool) -> Self {
        self.use_config_file = enabled;
        self
    }

    /// Sets the application id used to locate the config file. Set automatically
    /// by `CliConfig::with_environments`; only call directly in tests.
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

    /// The default environment name.
    #[must_use]
    pub fn default_env(&self) -> &str {
        &self.default
    }

    /// Enumerable environment names (compiled-in + file-defined), sorted.
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = self.defs.keys().cloned().collect();
        if let Ok(file) = self.file_defs() {
            names.extend(file.into_keys());
        }
        names.into_iter().collect()
    }

    /// Resolves `name` by merging compiled defaults, the config file,
    /// and `<ENV>_*` env-var overrides (later wins) into an [`Environment`].
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is not known to any layer or when the
    /// environments file exists but cannot be read or parsed.
    pub fn resolve(&self, name: &str) -> Result<Environment> {
        let compiled = self.defs.get(name);
        let file = self.file_def(name)?;
        if compiled.is_none() && file.is_none() {
            return Err(CliCoreError::message(format!(
                "unknown environment {name:?}; known: {}",
                self.list().join(", ")
            )));
        }
        let mut merged = EnvironmentDef::default();
        if let Some(def) = compiled {
            merge_into(&mut merged, def);
        }
        if let Some(def) = &file {
            merge_into(&mut merged, def);
        }
        apply_env_vars(name, &mut merged);
        Ok(finalize(name, merged))
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

    /// Parses the environments file into a name -> def map. Missing file = empty.
    fn file_defs(&self) -> Result<BTreeMap<String, EnvironmentDef>> {
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
        toml_edit::de::from_str::<BTreeMap<String, EnvironmentDef>>(&text).map_err(|err| {
            CliCoreError::message(format!("parsing environments file {path:?}: {err}"))
        })
    }

    fn file_def(&self, name: &str) -> Result<Option<EnvironmentDef>> {
        Ok(self.file_defs()?.remove(name))
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
        self.resolve(name)?; // reject unknown names
        let mut config = crate::config::ConfigFile::load(&self.app_id);
        config.set(Self::ACTIVE_ENV_KEY, name)?;
        config.save()
    }
}

/// Merges `src` into `dst`, with `src` winning on any field it sets.
fn merge_into(dst: &mut EnvironmentDef, src: &EnvironmentDef) {
    if src.client_id.is_some() {
        dst.client_id = src.client_id.clone();
    }
    if src.auth_url.is_some() {
        dst.auth_url = src.auth_url.clone();
    }
    if src.token_url.is_some() {
        dst.token_url = src.token_url.clone();
    }
    if src.scopes.is_some() {
        dst.scopes = src.scopes.clone();
    }
    for (k, v) in &src.extra {
        dst.extra.insert(k.clone(), v.clone());
    }
}

/// Applies `<ENV>_*` overrides: the three OAuth fields always, and any bag key
/// already present in the merged record (keyed `<ENV>_<KEY>`).
fn apply_env_vars(name: &str, def: &mut EnvironmentDef) {
    let prefix = name.to_uppercase().replace('-', "_");
    if let Ok(v) = std::env::var(format!("{prefix}_OAUTH_CLIENT_ID")) {
        def.client_id = Some(v);
    }
    if let Ok(v) = std::env::var(format!("{prefix}_OAUTH_AUTH_URL")) {
        def.auth_url = Some(v);
    }
    if let Ok(v) = std::env::var(format!("{prefix}_OAUTH_TOKEN_URL")) {
        def.token_url = Some(v);
    }
    let keys: Vec<String> = def.extra.keys().cloned().collect();
    for key in keys {
        let var = format!("{prefix}_{}", key.to_uppercase().replace('-', "_"));
        if let Ok(v) = std::env::var(&var) {
            def.extra.insert(key, v);
        }
    }
}

/// Turns a fully-merged declaration into a resolved [`Environment`]. OAuth is
/// present when a client id was set by any layer.
fn finalize(name: &str, def: EnvironmentDef) -> Environment {
    let oauth = def.client_id.as_ref().map(|client_id| OAuthConfig {
        client_id: client_id.clone(),
        auth_url: def.auth_url.clone().unwrap_or_default(),
        token_url: def.token_url.clone().unwrap_or_default(),
        scopes: def.scopes.clone().unwrap_or_default(),
    });
    Environment {
        name: name.to_owned(),
        oauth,
        extra: def.extra,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn sample() -> Environments {
        Environments::new("prod")
            .with_environment(
                "prod",
                EnvironmentDef::new()
                    .with_client_id("prod-client")
                    .with_auth_url("https://api.example.com/authorize")
                    .with_token_url("https://api.example.com/token")
                    .with_scopes(&["openid"])
                    .with_field("api_url", "https://api.example.com"),
            )
            .with_environment("dev", EnvironmentDef::new().with_client_id("dev-client"))
    }

    #[test]
    fn oauth_config_defaults_are_empty() {
        let c = OAuthConfig::default();
        assert!(c.client_id.is_empty() && c.scopes.is_empty());
    }

    #[test]
    fn builder_registers_compiled_environment() {
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_auth_url("https://api.example.com/authorize")
                .with_token_url("https://api.example.com/token")
                .with_scopes(&["openid"])
                .with_field("api_url", "https://api.example.com"),
        );
        assert_eq!(envs.default_env(), "prod");
        assert_eq!(envs.list(), vec!["prod".to_owned()]);
    }

    #[test]
    fn resolve_returns_compiled_record() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = sample().resolve("prod").expect("prod resolves");
        let oauth = env.oauth.expect("oauth present");
        assert_eq!(oauth.client_id, "prod-client");
        assert_eq!(oauth.scopes, vec!["openid".to_owned()]);
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.example.com")
        );
    }

    #[test]
    fn resolve_unknown_env_errors_with_known_names() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let err = sample().resolve("nope").unwrap_err().to_string();
        assert!(err.contains("nope"));
        assert!(err.contains("prod") && err.contains("dev"));
    }

    #[test]
    fn env_var_layer_overrides_oauth_and_known_bag_keys() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; removed below.
        unsafe {
            std::env::set_var("PROD_OAUTH_CLIENT_ID", "override-client");
            std::env::set_var("PROD_API_URL", "https://api.override.example.com");
        }
        let env = sample().resolve("prod").expect("prod resolves");
        assert_eq!(env.oauth.unwrap().client_id, "override-client");
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.override.example.com")
        );
        unsafe {
            std::env::remove_var("PROD_OAUTH_CLIENT_ID");
            std::env::remove_var("PROD_API_URL");
        }
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

        // File overrides the compiled prod client id, keeps compiled api_url.
        let prod = envs.resolve("prod").expect("prod");
        assert_eq!(prod.oauth.unwrap().client_id, "file-client");
        assert_eq!(
            prod.extra.get("api_url").map(String::as_str),
            Some("https://api.example.com")
        );

        // Custom env exists only in the file.
        let custom = envs.resolve("custom").expect("custom");
        assert_eq!(custom.oauth.unwrap().client_id, "custom-client");
        assert!(envs.list().contains(&"custom".to_owned()));
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
}
