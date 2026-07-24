//! First-class environment definitions and layered resolution.
//!
//! An [`Environments`] value holds compiled-in environment definitions and,
//! optionally, an `environments.toml` file plus `<ENV>_*` env-var overrides.
//! Resolving a name merges those layers (later wins) into an [`Environment`].

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;

use crate::{Result, Stage, error::CliCoreError};

/// A consumer-supplied computed default for a bag (`extra`) field, evaluated
/// against the environment's other already-resolved fields. See
/// [`EnvironmentDef::with_field_default`].
type FieldDefault = Arc<dyn Fn(&Environment) -> String + Send + Sync>;

/// A consumer-supplied validator for a bag (`extra`) field override. See
/// [`EnvironmentDef::with_field_validator`].
type FieldValidator = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// A consumer-supplied seam that defines an environment purely from outside
/// the compiled/file layers (for example, from environment variables) when a
/// name isn't known to either. See [`Environments::with_fallback`].
type EnvironmentFallback = Arc<dyn Fn(&str) -> Option<EnvironmentDef> + Send + Sync>;

/// A consumer-supplied function that scaffolds environment fields.
/// See [`Environments::with_init`].
type EnvironmentInit = Arc<dyn Fn(&str) -> EnvironmentDef + Send + Sync>;

/// The three named OAuth fields, as `with_field_validator`/`with_field_default`
/// keys. Excluded from the generic bag (`extra`) wherever a key set is built
/// from it, since these are dedicated `EnvironmentDef` fields — serde's
/// `Deserialize` routes them there directly, never into `extra` — and are
/// merged/defaulted through their own dedicated code, not the bag's.
const OAUTH_KEYS: [&str; 3] = ["client_id", "auth_url", "token_url"];

/// Standard OAuth slice of an environment, consumed by `PkceAuthProvider`.
///
/// `auth_url`, `token_url`, and `scopes` may be empty when a layer set only
/// `client_id`. Consumers should treat empty endpoint strings as "fall back to
/// the provider's default base endpoints".
#[non_exhaustive]
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
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// Environment name (e.g. `prod`).
    pub name: String,
    /// OAuth configuration, present when the environment participates in OAuth.
    pub oauth: Option<OAuthConfig>,
    /// App-specific fields (for example `api_url`).
    pub extra: BTreeMap<String, String>,
    /// Bulk override for this environment's minimum visible feature stage, if set
    /// by any layer (compiled, file, or env var).
    pub min_stage: Option<Stage>,
    /// Per-key feature-stage overrides declared for this environment.
    pub feature_overrides: BTreeMap<String, Stage>,
}

/// An unresolved per-environment declaration (one layer of configuration).
///
/// Fields are optional so layers can override individual values during
/// resolution. The same shape parses the `environments.toml` file.
#[derive(Clone, Default, Deserialize)]
pub struct EnvironmentDef {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    auth_url: Option<String>,
    #[serde(default)]
    token_url: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    min_stage: Option<Stage>,
    #[serde(default)]
    feature_overrides: BTreeMap<String, Stage>,
    /// Everything not recognised above is captured here (app-specific fields).
    #[serde(flatten, default)]
    extra: BTreeMap<String, String>,
    /// Computed defaults for `extra` keys, applied at `finalize()` only when no
    /// layer set the key explicitly. Never populated by file parsing.
    #[serde(skip)]
    field_defaults: BTreeMap<String, FieldDefault>,
    /// Validators for `extra` keys; a later layer's value that fails
    /// validation is dropped, keeping the prior value. Never populated by file
    /// parsing.
    #[serde(skip)]
    field_validators: BTreeMap<String, FieldValidator>,
}

impl std::fmt::Debug for EnvironmentDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentDef")
            .field("client_id", &self.client_id)
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("scopes", &self.scopes)
            .field("min_stage", &self.min_stage)
            .field("feature_overrides", &self.feature_overrides)
            .field("extra", &self.extra)
            .field(
                "field_defaults",
                &self.field_defaults.keys().collect::<Vec<_>>(),
            )
            .field(
                "field_validators",
                &self.field_validators.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
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

    /// Sets a bulk override for this environment's minimum visible feature stage.
    #[must_use]
    pub fn with_min_stage(mut self, stage: Stage) -> Self {
        self.min_stage = Some(stage);
        self
    }

    /// Sets a per-key feature-stage override for this environment.
    #[must_use]
    pub fn with_feature_override(mut self, key: impl Into<String>, stage: Stage) -> Self {
        self.feature_overrides.insert(key.into(), stage);
        self
    }

    /// Registers a computed default for config key `key`, used when nothing
    /// else set it.
    ///
    /// Use this whenever `key`'s fallback value depends on another field. The
    /// function receives the [`Environment`] with every explicitly-set
    /// field already resolved.
    #[must_use]
    pub fn with_field_default<F>(mut self, key: impl Into<String>, default: F) -> Self
    where
        F: Fn(&Environment) -> String + Send + Sync + 'static,
    {
        self.field_defaults.insert(key.into(), Arc::new(default));
        self
    }

    /// Registers a validator for bag key `key`, or for the named
    /// `"client_id"`/`"auth_url"`/`"token_url"` OAuth fields.
    ///
    /// Whenever a later layer (file or env-var) would set `key`, its new
    /// value is passed to `validator` first; if it returns `false` the
    /// override is dropped and the prior value (from an earlier layer) is
    /// kept instead of being overwritten. The validator itself is not applied
    /// to the layer that registers it — only to layers merged in afterward.
    #[must_use]
    pub fn with_field_validator<F>(mut self, key: impl Into<String>, validator: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.field_validators
            .insert(key.into(), Arc::new(validator));
        self
    }
}

/// On-disk shape of `environments.toml`. The recommended (and only documented)
/// shape is a flat top-level table per environment (`[prod]`), captured by
/// `top_level`'s flatten. `environments` is an undocumented compatibility shim
/// accepting a nested top-level `[environments.prod]` table, so files written
/// against that shape keep working without being rewritten. The named
/// `environments` field claims that key before `top_level`'s flatten catches
/// everything else, so the two shapes never conflict.
#[derive(Debug, Default, Deserialize)]
struct FileShape {
    #[serde(default)]
    environments: BTreeMap<String, EnvironmentDef>,
    #[serde(flatten, default)]
    top_level: BTreeMap<String, EnvironmentDef>,
}

/// Engine-owned environment system: definitions + resolution + active-env state.
#[derive(Clone)]
pub struct Environments {
    default: String,
    defs: BTreeMap<String, EnvironmentDef>,
    use_config_file: bool,
    app_id: String,
    file_path_override: Option<std::path::PathBuf>,
    fallback: Option<EnvironmentFallback>,
    init: Option<EnvironmentInit>,
}

impl std::fmt::Debug for Environments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environments")
            .field("default", &self.default)
            .field("defs", &self.defs)
            .field("use_config_file", &self.use_config_file)
            .field("app_id", &self.app_id)
            .field("file_path_override", &self.file_path_override)
            .field("fallback", &self.fallback.is_some())
            .field("init", &self.init.is_some())
            .finish()
    }
}

impl Environments {
    /// Creates an environment system with the given default environment name.
    ///
    /// If `default_env` (or any field on a compiled-in [`EnvironmentDef`]) is
    /// sourced from the consumer's own persisted state, read that state
    /// *raw* rather than through anything that calls back into `resolve` or
    /// a lazily-initialized singleton's `instance()`. A consumer wiring a
    /// lazy singleton whose default depends on its own config can otherwise
    /// deadlock re-entering that singleton's own initialization while it is
    /// still being constructed.
    #[must_use]
    pub fn new(default_env: impl Into<String>) -> Self {
        Self {
            default: default_env.into(),
            defs: BTreeMap::new(),
            use_config_file: false,
            app_id: String::new(),
            file_path_override: None,
            fallback: None,
            init: None,
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

    /// Sets the application id used to locate the config file.
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
    /// [`resolve`](Self::resolve) — and therefore every path built on it,
    /// including the built-in `--env` flag and the `env` command group —
    /// consults `fallback` with the requested name whenever that name is
    /// unknown to both the compiled-in and `environments.toml` layers.
    /// Returning `Some(def)` lets a brand-new, never-declared name resolve
    /// (typically by having `fallback` read its own `<NAME>_*` environment
    /// variables and build a def from them); returning `None` preserves the
    /// existing "unknown environment" error.
    ///
    /// The returned [`EnvironmentDef`] is treated the same as a compiled-in
    /// definition: it does not skip the `environments.toml` or `<ENV>_*`
    /// env-var layers, which still merge on top of it (later wins).
    /// `fallback` is never consulted for a name already known to the
    /// compiled-in or file layer.
    #[must_use]
    pub fn with_fallback<F>(mut self, fallback: F) -> Self
    where
        F: Fn(&str) -> Option<EnvironmentDef> + Send + Sync + 'static,
    {
        self.fallback = Some(Arc::new(fallback));
        self
    }

    /// Registers a function that scaffolds an environment's field defaults
    /// and validators.
    #[must_use]
    pub fn with_init<F>(mut self, init: F) -> Self
    where
        F: Fn(&str) -> EnvironmentDef + Send + Sync + 'static,
    {
        self.init = Some(Arc::new(init));
        self
    }

    /// The default environment name.
    #[must_use]
    pub fn default_env(&self) -> &str {
        &self.default
    }

    /// Enumerable environment names (compiled-in + file-defined), sorted.
    ///
    /// Any error from reading or parsing the environments file (missing file,
    /// permission/read error, or malformed TOML) is silently swallowed and only
    /// the compiled-in names are returned. Use [`resolve`](Self::resolve) when
    /// you need those errors surfaced; a fallible listing variant can be added
    /// later if needed.
    ///
    /// # Blocking
    ///
    /// When the config-file layer is enabled, this performs synchronous
    /// filesystem I/O to read and parse `environments.toml` (like
    /// [`resolve`](Self::resolve)). Avoid calling it repeatedly on a
    /// latency-sensitive async path.
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = self.defs.keys().cloned().collect();
        if let Ok(file) = self.file_defs() {
            names.extend(file.into_keys());
        }
        names.into_iter().collect()
    }

    /// Resolves an environment by name, merging in any file or environment
    /// variable overrides.
    ///
    /// When only `client_id` was set on the matching layer(s), the returned
    /// [`Environment`]'s `oauth.auth_url` / `oauth.token_url` are empty
    /// strings; treat an empty endpoint as "fall back to the provider's
    /// default base endpoints".
    ///
    /// # Blocking
    ///
    /// When the config-file layer is enabled, this performs synchronous
    /// filesystem I/O to read and parse `environments.toml` (like
    /// [`list`](Self::list)). Resolve once at startup rather than per
    /// request inside an async handler.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is not known to any layer (including a
    /// registered [`with_fallback`](Self::with_fallback)) or when the
    /// environments file exists but cannot be read or parsed.
    pub fn resolve(&self, name: &str) -> Result<Environment> {
        let compiled = self.defs.get(name);
        // Parse the file once; reuse for both membership check and merge.
        let mut all_file_defs = self.file_defs()?;
        let file = all_file_defs.remove(name);
        // The fallback only ever introduces a name unknown to the compiled-in
        // and file layers; a name known to either never consults it.
        let fallback = if compiled.is_none() && file.is_none() {
            self.fallback.as_ref().and_then(|f| f(name))
        } else {
            None
        };
        if compiled.is_none() && file.is_none() && fallback.is_none() {
            let mut known: std::collections::BTreeSet<String> = self.defs.keys().cloned().collect();
            known.extend(all_file_defs.into_keys());
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
        let mut merged = EnvironmentDef::default();
        if let Some(init) = &self.init {
            merge_into(name, &mut merged, &init(name));
        }
        if let Some(def) = compiled {
            merge_into(name, &mut merged, def);
        }
        if let Some(def) = &fallback {
            merge_into(name, &mut merged, def);
        }
        if let Some(def) = &file {
            merge_into(name, &mut merged, def);
        }
        apply_env_vars(name, &mut merged)?;
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
    ///
    /// Also accepts a legacy, undocumented nested top-level `[environments.prod]`
    /// table alongside the recommended flat `[prod]` shape, so files already
    /// written against that shape keep parsing without being rewritten. When a
    /// name appears under both, the nested entry's fields win.
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
        let shape = toml_edit::de::from_str::<FileShape>(&text).map_err(|err| {
            CliCoreError::message(format!("parsing environments file {path:?}: {err}"))
        })?;
        let mut defs = shape.top_level;
        for (name, def) in shape.environments {
            match defs.get_mut(&name) {
                Some(existing) => merge_into(&name, existing, &def),
                None => {
                    defs.insert(name, def);
                }
            }
        }
        Ok(defs)
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

/// Merges `src` into `dst`, with `src` winning on any field it sets.
///
/// A bag (`extra`) key, or one of the named `client_id`/`auth_url`/
/// `token_url` fields, already covered by a validator registered on `dst`
/// (from an earlier layer) is only overwritten when `src`'s value passes
/// that validator; otherwise `dst`'s prior value is kept and the rejection
/// is logged at `warn` level (`name` identifies which environment). This
/// never fails resolution outright — the previous good value survives a
/// stray or malformed override — but it's not silent either, so a real typo
/// doesn't disappear with zero trace. `"client_id"`, `"auth_url"`, and
/// `"token_url"` are unambiguous as validator/default keys —
/// [`EnvironmentDef`]'s `Deserialize` routes those names to these dedicated
/// fields, never into `extra`, so there's no bag key of the same name to
/// collide with. A validator or default that `src` itself registers only
/// takes effect for layers merged in after this call — it never applies
/// retroactively to `src`'s own values.
fn merge_into(name: &str, dst: &mut EnvironmentDef, src: &EnvironmentDef) {
    if let Some(v) = &src.client_id {
        if dst.field_validators.get("client_id").is_none_or(|f| f(v)) {
            dst.client_id = Some(v.clone());
        } else {
            tracing::warn!(env = name, key = "client_id", value = %v, "rejecting invalid override; keeping prior value");
        }
    }
    if let Some(v) = &src.auth_url {
        if dst.field_validators.get("auth_url").is_none_or(|f| f(v)) {
            dst.auth_url = Some(v.clone());
        } else {
            tracing::warn!(env = name, key = "auth_url", value = %v, "rejecting invalid override; keeping prior value");
        }
    }
    if let Some(v) = &src.token_url {
        if dst.field_validators.get("token_url").is_none_or(|f| f(v)) {
            dst.token_url = Some(v.clone());
        } else {
            tracing::warn!(env = name, key = "token_url", value = %v, "rejecting invalid override; keeping prior value");
        }
    }
    if src.scopes.is_some() {
        dst.scopes = src.scopes.clone();
    }
    if src.min_stage.is_some() {
        dst.min_stage = src.min_stage;
    }
    for (k, v) in &src.feature_overrides {
        dst.feature_overrides.insert(k.clone(), *v);
    }
    for (k, v) in &src.extra {
        // A reserved OAuth key name can only land in `extra` via a consumer
        // mistakenly calling `with_field("client_id", ..)` instead of
        // `with_client_id(..)` — never through normal file/env-var layers
        // (see `OAUTH_KEYS`'s doc comment). Drop it rather than letting it
        // sit in the bag alongside the dedicated field it shadows.
        if OAUTH_KEYS.contains(&k.as_str()) {
            tracing::warn!(
                env = name,
                key = %k,
                "ignoring with_field override for a reserved OAuth key name; use with_client_id/with_auth_url/with_token_url instead"
            );
            continue;
        }
        let accepted = dst
            .field_validators
            .get(k)
            .is_none_or(|validator| validator(v));
        if accepted {
            dst.extra.insert(k.clone(), v.clone());
        } else {
            tracing::warn!(env = name, key = %k, value = %v, "rejecting invalid override; keeping prior value");
        }
    }
    for (k, f) in &src.field_validators {
        dst.field_validators.insert(k.clone(), Arc::clone(f));
    }
    for (k, f) in &src.field_defaults {
        dst.field_defaults.insert(k.clone(), Arc::clone(f));
    }
}

/// Applies `<ENV>_*` overrides: the three OAuth fields (subject to that
/// field's validator, if any, on the same terms as [`merge_into`] — a
/// rejection is logged at `warn` and the prior value kept, same as there),
/// any bag key already present in the merged record *or* carrying a
/// registered default (keyed `<ENV>_<KEY>`, validated the same way — see
/// [`EnvironmentDef::with_field_default`] for why a default alone is enough
/// to make a key eligible here), the bulk `<ENV>_MIN_STAGE` stage override,
/// and any feature key already present in the merged record (keyed
/// `<ENV>_FEATURE_<KEY>`).
///
/// The prefix is `name.to_uppercase().replace('-', "_")`, so environment names
/// that differ only by `-` vs `_` map to the same prefix and will collide.
///
/// Scopes are intentionally not env-var overridable; set them via the
/// compiled-in layer or the `environments.toml` file.
///
/// # Errors
///
/// Returns an error when `<ENV>_MIN_STAGE` or `<ENV>_FEATURE_<KEY>` is set but
/// fails to parse as a [`Stage`]. Unlike a missing var (a no-op), a malformed
/// value is not silently ignored — unlike a `with_field_validator` rejection,
/// which logs a warning and keeps the prior value rather than failing
/// resolution (`<ENV>_MIN_STAGE`/`<ENV>_FEATURE_<KEY>` have no validator
/// concept of their own; this asymmetry is deliberate — see
/// `docs/environments.md`'s "Per-Field Validation and Computed Defaults").
fn apply_env_vars(name: &str, def: &mut EnvironmentDef) -> Result<()> {
    let prefix = name.to_uppercase().replace('-', "_");
    let client_id_var = format!("{prefix}_OAUTH_CLIENT_ID");
    if let Ok(v) = std::env::var(&client_id_var) {
        if def.field_validators.get("client_id").is_none_or(|f| f(&v)) {
            def.client_id = Some(v);
        } else {
            tracing::warn!(var = %client_id_var, value = %v, "rejecting invalid override; keeping prior value");
        }
    }
    let auth_url_var = format!("{prefix}_OAUTH_AUTH_URL");
    if let Ok(v) = std::env::var(&auth_url_var) {
        if def.field_validators.get("auth_url").is_none_or(|f| f(&v)) {
            def.auth_url = Some(v);
        } else {
            tracing::warn!(var = %auth_url_var, value = %v, "rejecting invalid override; keeping prior value");
        }
    }
    let token_url_var = format!("{prefix}_OAUTH_TOKEN_URL");
    if let Ok(v) = std::env::var(&token_url_var) {
        if def.field_validators.get("token_url").is_none_or(|f| f(&v)) {
            def.token_url = Some(v);
        } else {
            tracing::warn!(var = %token_url_var, value = %v, "rejecting invalid override; keeping prior value");
        }
    }
    // A key with a registered default is eligible even if no layer has set
    // it yet — the default is itself the signal that the consumer cares
    // about this key, so it shouldn't also require a separate blank
    // placeholder in `extra` just to be reachable here. The three named
    // OAuth fields are excluded from both `extra` and `field_defaults`:
    // they're handled by the dedicated blocks above and must never leak
    // into the bag. `merge_into` already keeps them out of `extra` before
    // `def` reaches here, so filtering `extra.keys()` too is defense in
    // depth, not a path reachable through the public builder API today.
    let keys: std::collections::BTreeSet<String> = def
        .extra
        .keys()
        .filter(|k| !OAUTH_KEYS.contains(&k.as_str()))
        .cloned()
        .chain(
            def.field_defaults
                .keys()
                .filter(|k| !OAUTH_KEYS.contains(&k.as_str()))
                .cloned(),
        )
        .collect();
    for key in keys {
        let var = format!("{prefix}_{}", key.to_uppercase().replace('-', "_"));
        if let Ok(v) = std::env::var(&var) {
            let accepted = def
                .field_validators
                .get(&key)
                .is_none_or(|validator| validator(&v));
            if accepted {
                def.extra.insert(key, v);
            } else {
                tracing::warn!(%var, value = %v, "rejecting invalid override; keeping prior value");
            }
        }
    }
    if let Ok(v) = std::env::var(format!("{prefix}_MIN_STAGE")) {
        def.min_stage = Some(v.parse::<Stage>().map_err(|err| {
            CliCoreError::message(format!("invalid {prefix}_MIN_STAGE {v:?}: {err}"))
        })?);
    }
    let feature_keys: Vec<String> = def.feature_overrides.keys().cloned().collect();
    for key in feature_keys {
        let var = format!("{prefix}_FEATURE_{}", key.to_uppercase().replace('-', "_"));
        if let Ok(v) = std::env::var(&var) {
            let stage = v
                .parse::<Stage>()
                .map_err(|err| CliCoreError::message(format!("invalid {var} {v:?}: {err}")))?;
            def.feature_overrides.insert(key, stage);
        }
    }
    Ok(())
}

/// Turns a fully-merged declaration into a resolved [`Environment`]. OAuth is
/// present when a client id was set by any layer. Any [`FieldDefault`]s
/// registered by a layer are then computed, but only for a key left empty
/// or absent by every layer — computed against the `Environment` as merged
/// so far, so two defaults never see each other's computed values.
///
/// `"auth_url"`/`"token_url"` defaults apply to [`OAuthConfig`]'s fields
/// (only when `oauth` is present at all — i.e. some layer set `client_id`),
/// not to the bag; a `"client_id"` default is not supported (there is no
/// sensible fallback for a missing credential) and is silently a no-op if
/// registered, same as any other key nothing ever consults.
fn finalize(name: &str, def: EnvironmentDef) -> Environment {
    let EnvironmentDef {
        client_id,
        auth_url,
        token_url,
        scopes,
        min_stage,
        feature_overrides,
        extra,
        field_defaults,
        ..
    } = def;
    let oauth = client_id.map(|id| OAuthConfig {
        client_id: id,
        auth_url: auth_url.unwrap_or_default(),
        token_url: token_url.unwrap_or_default(),
        scopes: scopes.unwrap_or_default(),
    });
    let mut env = Environment {
        name: name.to_owned(),
        oauth,
        extra,
        min_stage,
        feature_overrides,
    };
    let computed: Vec<(String, String)> = field_defaults
        .iter()
        .filter(|(key, _)| {
            !OAUTH_KEYS.contains(&key.as_str()) && env.extra.get(*key).is_none_or(String::is_empty)
        })
        .map(|(key, default)| (key.clone(), default(&env)))
        .collect();
    for (key, value) in computed {
        env.extra.insert(key, value);
    }
    // Computed with `env` immutably borrowed (a default closure may read
    // `env.extra`/`env.name`), fully resolved *before* the mutable borrow
    // below applies them — `oauth.as_mut()` can't coexist with calling a
    // closure that takes `&env`.
    let auth_url_default = env
        .oauth
        .as_ref()
        .filter(|o| o.auth_url.is_empty())
        .and_then(|_| field_defaults.get("auth_url"))
        .map(|default| default(&env));
    let token_url_default = env
        .oauth
        .as_ref()
        .filter(|o| o.token_url.is_empty())
        .and_then(|_| field_defaults.get("token_url"))
        .map(|default| default(&env));
    if let Some(oauth) = env.oauth.as_mut() {
        if let Some(v) = auth_url_default {
            oauth.auth_url = v;
        }
        if let Some(v) = token_url_default {
            oauth.token_url = v;
        }
    }
    env
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
mod tests {
    use super::*;

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

    /// With no environments defined at all, the unknown-env error renders a
    /// readable placeholder instead of a dangling `known: `.
    #[test]
    fn resolve_unknown_env_with_no_defs_uses_placeholder() {
        let err = Environments::new("prod")
            .resolve("prod")
            .expect_err("nothing defined should fail");
        let message = err.to_string();
        assert!(
            message.contains("(none defined)"),
            "expected placeholder, got: {message}"
        );
    }

    /// `persist_active` without an `app_id` returns a clear, actionable error
    /// (mentioning `app_id`) rather than a misleading config-path failure.
    #[test]
    fn persist_active_without_app_id_errors_clearly() {
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
    fn resolve_with_only_client_id_yields_partial_oauth() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("dev")
            .with_environment("dev", EnvironmentDef::new().with_client_id("dev-only"));
        let env = envs.resolve("dev").expect("dev resolves");
        let oauth = env.oauth.expect("oauth present when client_id is set");
        assert_eq!(oauth.client_id, "dev-only");
        assert!(
            oauth.auth_url.is_empty(),
            "auth_url should be empty (fall back to provider default)"
        );
        assert!(
            oauth.token_url.is_empty(),
            "token_url should be empty (fall back to provider default)"
        );
        assert!(oauth.scopes.is_empty());
    }

    #[test]
    fn env_var_layer_overrides_oauth_and_known_bag_keys() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guards remove vars on any exit incl. panic.
        unsafe { std::env::set_var("PROD_OAUTH_CLIENT_ID", "override-client") };
        let _g1 = EnvGuard("PROD_OAUTH_CLIENT_ID");
        unsafe { std::env::set_var("PROD_API_URL", "https://api.override.example.com") };
        let _g2 = EnvGuard("PROD_API_URL");

        let env = sample().resolve("prod").expect("prod resolves");
        assert_eq!(env.oauth.unwrap().client_id, "override-client");
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.override.example.com")
        );
    }

    #[test]
    fn environment_def_min_stage_and_feature_override_builders_set_fields() {
        let def = EnvironmentDef::new()
            .with_min_stage(Stage::Experimental)
            .with_feature_override("domain-bulk-transfer", Stage::Beta);
        assert_eq!(def.min_stage, Some(Stage::Experimental));
        assert_eq!(
            def.feature_overrides.get("domain-bulk-transfer"),
            Some(&Stage::Beta)
        );
    }

    #[test]
    fn resolve_returns_compiled_min_stage_and_feature_overrides() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("dev").with_environment(
            "dev",
            EnvironmentDef::new()
                .with_client_id("dev-client")
                .with_min_stage(Stage::Experimental)
                .with_feature_override("domain-bulk-transfer", Stage::Beta),
        );
        let env = envs.resolve("dev").expect("dev resolves");
        assert_eq!(env.min_stage, Some(Stage::Experimental));
        assert_eq!(
            env.feature_overrides.get("domain-bulk-transfer"),
            Some(&Stage::Beta)
        );
    }

    #[test]
    fn env_var_min_stage_overrides_compiled_and_file_layers() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Local fixture with a compiled `min_stage` (Experimental) that differs
        // from the env-var value (Beta), so a passing assertion proves the env
        // var *wins over* an already-set compiled value — not merely that it
        // populates an otherwise-empty field (which `sample()`'s prod, with no
        // compiled `min_stage`, could not distinguish).
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_min_stage(Stage::Experimental),
        );
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("PROD_MIN_STAGE", "beta") };
        let _guard = EnvGuard("PROD_MIN_STAGE");

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(env.min_stage, Some(Stage::Beta));
    }

    #[test]
    fn env_var_feature_override_updates_existing_key() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new().with_feature_override("domain-bulk-transfer", Stage::Beta),
        );
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("PROD_FEATURE_DOMAIN_BULK_TRANSFER", "ga") };
        let _guard = EnvGuard("PROD_FEATURE_DOMAIN_BULK_TRANSFER");

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.feature_overrides.get("domain-bulk-transfer"),
            Some(&Stage::Ga)
        );
    }

    #[test]
    fn env_var_feature_override_for_undeclared_key_is_ignored() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("PROD_FEATURE_NEVER_DECLARED", "beta") };
        let _guard = EnvGuard("PROD_FEATURE_NEVER_DECLARED");

        let env = sample().resolve("prod").expect("prod resolves");
        assert!(
            !env.feature_overrides.contains_key("never-declared"),
            "an env var must not introduce a brand-new feature key"
        );
    }

    #[test]
    fn malformed_min_stage_env_var_errors_clearly() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("PROD_MIN_STAGE", "nightly") };
        let _guard = EnvGuard("PROD_MIN_STAGE");

        let err = sample().resolve("prod").unwrap_err().to_string();
        assert!(
            err.contains("PROD_MIN_STAGE") && err.contains("nightly"),
            "expected error to mention the var name and bad value, got: {err}"
        );
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

        let dev = envs.resolve("dev").expect("dev");
        assert_eq!(
            dev.oauth.unwrap().client_id,
            "94488449-5769-4ecf-8bf4-9f8aa83859a3"
        );
        assert_eq!(
            dev.extra.get("api_url").map(String::as_str),
            Some("https://api.dev-godaddy.com")
        );

        let test = envs.resolve("test").expect("test");
        assert_eq!(
            test.oauth.unwrap().client_id,
            "e710d8b9-f4e5-4178-b1bf-98dfcd15d4ed"
        );
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

        let prod = envs.resolve("prod").expect("prod");
        assert_eq!(prod.oauth.unwrap().client_id, "nested-client");
        assert_eq!(
            prod.extra.get("api_url").map(String::as_str),
            Some("https://api.flat.example.com")
        );
    }

    #[test]
    fn file_layer_min_stage_and_features_table_merge_into_feature_overrides() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[dev]
min_stage = "experimental"

[staging]
client_id = "staging-client"

[staging.feature_overrides]
"domain-bulk-transfer" = "beta"
"#,
        )
        .expect("write file");

        let envs = Environments::new("prod")
            .with_config_file(true)
            .with_config_file_path_override(file);

        let dev = envs.resolve("dev").expect("dev");
        assert_eq!(dev.min_stage, Some(Stage::Experimental));

        let staging = envs.resolve("staging").expect("staging");
        assert_eq!(
            staging.feature_overrides.get("domain-bulk-transfer"),
            Some(&Stage::Beta)
        );
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
            Some(EnvironmentDef::new().with_client_id(format!("{name}-fallback-client")))
        });
        let env = envs.resolve("throwaway").expect("fallback should resolve");
        assert_eq!(env.oauth.unwrap().client_id, "throwaway-fallback-client");
    }

    /// A fallback returning `None` preserves the original "unknown environment"
    /// error, including the known-names listing.
    #[test]
    fn fallback_returning_none_preserves_unknown_env_error() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = sample().with_fallback(|_name| None);
        let err = envs.resolve("nope").unwrap_err().to_string();
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
            .with_fallback(|_name| Some(EnvironmentDef::new().with_client_id("should-not-win")));
        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(env.oauth.unwrap().client_id, "prod-client");
    }

    /// Mirrors gddy's DEVEX-947 case: a brand-new environment name, never
    /// declared in the compiled-in or file layers, becomes selectable purely
    /// because its own `<NAME>_API_URL`-style env var is set. The fallback
    /// pre-registers a placeholder bag key so the normal `<ENV>_*` env-var
    /// layer (which only overrides already-known bag keys) has something to
    /// fill in.
    #[test]
    fn fallback_plus_env_var_layer_defines_a_brand_new_environment() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("THROWAWAY_API_URL", "https://api.throwaway.example.com") };
        let _guard = EnvGuard("THROWAWAY_API_URL");

        let envs =
            sample().with_fallback(|_name| Some(EnvironmentDef::new().with_field("api_url", "")));
        let env = envs.resolve("throwaway").expect("fallback should resolve");
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.throwaway.example.com")
        );
    }

    #[test]
    fn init_applies_a_validator_to_a_compiled_environment() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(&file, "[prod]\napi_url = \"not-a-url\"\n").expect("write file");

        let envs = Environments::new("prod")
            .with_init(|_name| {
                EnvironmentDef::new().with_field_validator("api_url", |v| v.starts_with("https://"))
            })
            .with_environment(
                "prod",
                EnvironmentDef::new()
                    .with_client_id("prod-client")
                    .with_field("api_url", "https://api.example.com"),
            )
            .with_config_file(true)
            .with_config_file_path_override(file);

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.example.com"),
            "init's validator should reject the bad file override, keeping the compiled value"
        );
    }

    #[test]
    fn init_applies_a_default_to_a_file_only_environment() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(&file, "[dev]\napi_url = \"https://api.dev.example.com\"\n")
            .expect("write file");

        let envs = Environments::new("prod")
            .with_init(|_name| {
                EnvironmentDef::new().with_field_default("domains_api_url", |env| {
                    env.extra.get("api_url").cloned().unwrap_or_default()
                })
            })
            .with_config_file(true)
            .with_config_file_path_override(file);

        let env = envs.resolve("dev").expect("dev resolves");
        assert_eq!(
            env.extra.get("domains_api_url").map(String::as_str),
            Some("https://api.dev.example.com"),
            "a file-only environment should still get init's registered default"
        );
    }

    #[test]
    fn init_does_not_make_an_unknown_name_resolve() {
        let envs = Environments::new("prod")
            .with_environment("prod", EnvironmentDef::new())
            .with_init(|_name| {
                EnvironmentDef::new().with_field("api_url", "https://init.example.com")
            });

        let err = envs.resolve("nope").unwrap_err().to_string();
        assert!(err.contains("nope"));
        assert!(err.contains("prod"));
    }

    #[test]
    fn init_composes_with_fallback() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("THROWAWAY_API_URL", "https://api.throwaway.example.com") };
        let _guard = EnvGuard("THROWAWAY_API_URL");

        let envs = Environments::new("prod")
            .with_environment("prod", EnvironmentDef::new())
            .with_init(|_name| {
                EnvironmentDef::new().with_field_default("domains_api_url", |env| {
                    env.extra.get("api_url").cloned().unwrap_or_default()
                })
            })
            .with_fallback(|_name| Some(EnvironmentDef::new().with_field("api_url", "")));

        let env = envs.resolve("throwaway").expect("fallback should resolve");
        assert_eq!(
            env.extra.get("domains_api_url").map(String::as_str),
            Some("https://api.throwaway.example.com"),
            "the fallback-defined environment should still get init's default"
        );
    }

    #[test]
    fn field_default_applies_only_when_no_layer_set_the_key() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_field("api_url", "https://api.example.com")
                .with_field_default("domains_api_url", |env| {
                    env.extra.get("api_url").cloned().unwrap_or_default()
                })
                .with_field_default("account_url", |env| {
                    format!("https://account.{}.example.com", env.name)
                }),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("domains_api_url").map(String::as_str),
            Some("https://api.example.com"),
            "domains_api_url should default to api_url"
        );
        assert_eq!(
            env.extra.get("account_url").map(String::as_str),
            Some("https://account.prod.example.com"),
            "account_url should derive from the environment name"
        );
    }

    /// A blank placeholder value (registered so an `<ENV>_<KEY>` env var has
    /// something to override, per [`apply_env_vars`]'s "already known bag
    /// key" rule) is treated the same as an absent key: the default still
    /// applies when nothing overrides it.
    #[test]
    fn field_default_applies_when_the_key_is_an_explicit_blank_placeholder() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_field("api_url", "https://api.example.com")
                .with_field("domains_api_url", "")
                .with_field_default("domains_api_url", |env| {
                    env.extra.get("api_url").cloned().unwrap_or_default()
                }),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("domains_api_url").map(String::as_str),
            Some("https://api.example.com")
        );
    }

    #[test]
    fn field_default_alone_makes_the_key_env_var_eligible_without_a_placeholder() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("PROD_DOMAINS_API_URL", "https://domains.example.com") };
        let _guard = EnvGuard("PROD_DOMAINS_API_URL");

        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_field("api_url", "https://api.example.com")
                .with_field_default("domains_api_url", |env| {
                    env.extra.get("api_url").cloned().unwrap_or_default()
                }),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("domains_api_url").map(String::as_str),
            Some("https://domains.example.com"),
            "the env var should reach domains_api_url even with no blank placeholder registered"
        );
    }

    /// An explicit value set by any layer — compiled, file, or env-var — wins
    /// over a registered default; the default is a fallback of last resort.
    #[test]
    fn field_default_does_not_override_an_explicit_value() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_field("api_url", "https://api.example.com")
                .with_field("domains_api_url", "https://domains.example.com")
                .with_field_default("domains_api_url", |env| {
                    env.extra.get("api_url").cloned().unwrap_or_default()
                }),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("domains_api_url").map(String::as_str),
            Some("https://domains.example.com")
        );
    }

    /// A default registered on the compiled layer still applies after the
    /// file layer merges in, as long as the file layer didn't set the key
    /// itself — the default map propagates through `merge_into`.
    #[test]
    fn field_default_survives_file_layer_merge() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[prod]
api_url = "https://api.file.example.com"
"#,
        )
        .expect("write file");

        let envs = Environments::new("prod")
            .with_environment(
                "prod",
                EnvironmentDef::new()
                    .with_client_id("prod-client")
                    .with_field_default("domains_api_url", |env| {
                        env.extra.get("api_url").cloned().unwrap_or_default()
                    }),
            )
            .with_config_file(true)
            .with_config_file_path_override(file);

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("domains_api_url").map(String::as_str),
            Some("https://api.file.example.com")
        );
    }

    /// A file-layer override that fails validation is dropped, keeping the
    /// compiled layer's prior value instead of a blank/malformed one.
    #[test]
    fn field_validator_rejects_bad_file_layer_override() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[prod]
api_url = "not-a-url"
"#,
        )
        .expect("write file");

        let envs = Environments::new("prod")
            .with_environment(
                "prod",
                EnvironmentDef::new()
                    .with_client_id("prod-client")
                    .with_field("api_url", "https://api.example.com")
                    .with_field_validator("api_url", |v| v.starts_with("https://")),
            )
            .with_config_file(true)
            .with_config_file_path_override(file);

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.example.com"),
            "malformed file override should be dropped, keeping the compiled value"
        );
    }

    /// A valid file-layer override still passes validation and applies
    /// normally.
    #[test]
    fn field_validator_accepts_good_file_layer_override() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[prod]
api_url = "https://api.file.example.com"
"#,
        )
        .expect("write file");

        let envs = Environments::new("prod")
            .with_environment(
                "prod",
                EnvironmentDef::new()
                    .with_client_id("prod-client")
                    .with_field("api_url", "https://api.example.com")
                    .with_field_validator("api_url", |v| v.starts_with("https://")),
            )
            .with_config_file(true)
            .with_config_file_path_override(file);

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.file.example.com")
        );
    }

    /// An env-var override that fails validation is dropped, keeping the
    /// prior (compiled) value — mirroring gddy re-validating URL strings
    /// after resolution.
    #[test]
    fn field_validator_rejects_bad_env_var_override() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("PROD_API_URL", "not-a-url") };
        let _guard = EnvGuard("PROD_API_URL");

        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_field("api_url", "https://api.example.com")
                .with_field_validator("api_url", |v| v.starts_with("https://")),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.example.com"),
            "malformed env-var override should be dropped, keeping the compiled value"
        );
    }

    /// `with_field_validator`/`with_field_default` also cover the named OAuth
    /// fields, not just the bag — a malformed `<ENV>_OAUTH_AUTH_URL` override
    /// is rejected the same way a malformed bag-key override is.
    #[test]
    fn field_validator_rejects_bad_oauth_env_var_override() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("PROD_OAUTH_AUTH_URL", "not-a-url") };
        let _guard = EnvGuard("PROD_OAUTH_AUTH_URL");

        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_auth_url("https://api.example.com/authorize")
                .with_field_validator("auth_url", |v| v.starts_with("https://")),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.oauth.unwrap().auth_url,
            "https://api.example.com/authorize",
            "malformed OAuth env-var override should be dropped, keeping the compiled value"
        );
    }

    /// A validator registered for `auth_url` also gates a bad
    /// `environments.toml` file-layer override, not just an env var.
    #[test]
    fn field_validator_rejects_bad_oauth_file_layer_override() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("environments.toml");
        std::fs::write(
            &file,
            r#"
[prod]
auth_url = "not-a-url"
"#,
        )
        .expect("write file");

        let envs = Environments::new("prod")
            .with_environment(
                "prod",
                EnvironmentDef::new()
                    .with_client_id("prod-client")
                    .with_auth_url("https://api.example.com/authorize")
                    .with_field_validator("auth_url", |v| v.starts_with("https://")),
            )
            .with_config_file(true)
            .with_config_file_path_override(file);

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.oauth.unwrap().auth_url,
            "https://api.example.com/authorize"
        );
    }

    /// A consumer mistakenly calling `with_field("client_id", ..)` instead of
    /// `with_client_id(..)` must not let the reserved key leak into the
    /// resolved `extra` bag — it should be dropped, not sit alongside (and
    /// diverge from) the dedicated `oauth.client_id` field.
    #[test]
    fn with_field_override_of_a_reserved_oauth_key_is_dropped_not_merged_into_extra() {
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("real-client-id")
                .with_field("client_id", "sneaky-value")
                .with_field("auth_url", "sneaky-auth-url")
                .with_field("token_url", "sneaky-token-url"),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(env.oauth.as_ref().unwrap().client_id, "real-client-id");
        assert!(!env.extra.contains_key("client_id"));
        assert!(!env.extra.contains_key("auth_url"));
        assert!(!env.extra.contains_key("token_url"));
    }

    /// A `with_field_default` registered for `auth_url`/`token_url` is
    /// invoked and its result lands on the resolved `OAuthConfig`
    #[test]
    fn field_default_for_oauth_url_applies_a_derived_value() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_field("api_url", "https://api.example.com")
                .with_field_default("auth_url", |env| {
                    format!(
                        "{}/v2/oauth2/authorize",
                        env.extra.get("api_url").cloned().unwrap_or_default()
                    )
                })
                .with_field_default("token_url", |env| {
                    format!(
                        "{}/v2/oauth2/token",
                        env.extra.get("api_url").cloned().unwrap_or_default()
                    )
                }),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        let oauth = env.oauth.expect("oauth present");
        assert_eq!(
            oauth.auth_url,
            "https://api.example.com/v2/oauth2/authorize"
        );
        assert_eq!(oauth.token_url, "https://api.example.com/v2/oauth2/token");
    }

    /// An explicit `auth_url` set by any layer still wins over a registered
    /// default — the default is a fallback of last resort, same as for bag
    /// keys.
    #[test]
    fn field_default_does_not_override_an_explicit_oauth_url() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_auth_url("https://auth.example.com/authorize")
                .with_field("api_url", "https://api.example.com")
                .with_field_default("auth_url", |env| {
                    format!(
                        "{}/v2/oauth2/authorize",
                        env.extra.get("api_url").cloned().unwrap_or_default()
                    )
                }),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert_eq!(
            env.oauth.unwrap().auth_url,
            "https://auth.example.com/authorize"
        );
    }

    /// A registered `auth_url`/`token_url` default never applies when no
    /// layer set `client_id` at all — there is no `OAuthConfig` to compute
    /// it onto.
    #[test]
    fn field_default_for_oauth_url_is_inert_without_a_client_id() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_flag = Arc::clone(&called);
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_field("api_url", "https://api.example.com")
                .with_field_default("auth_url", move |_env| {
                    called_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    "should-not-be-set".to_owned()
                }),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert!(env.oauth.is_none());
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "default must not be invoked without a client_id"
        );
    }

    /// A `with_field_default("client_id", ...)` registration is a documented
    /// no-op — there's no sensible fallback for a missing credential, and it
    /// must not leak into the `extra` bag either (client_id is a dedicated
    /// field, never a bag key).
    #[test]
    fn field_default_for_client_id_is_a_documented_noop() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let envs = Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_field("api_url", "https://api.example.com")
                .with_field_default("client_id", |_env| "should-not-apply".to_owned()),
        );

        let env = envs.resolve("prod").expect("prod resolves");
        assert!(env.oauth.is_none(), "no client_id was ever set");
        assert!(
            !env.extra.contains_key("client_id"),
            "client_id must never leak into the extra bag"
        );
    }

    /// End-to-end DEVEX-947 scenario: a brand-new environment name is
    /// introduced purely via `<NAME>_API_URL`, validated, and its
    /// `domains_api_url`/`account_url` bag keys are derived without any
    /// consumer-side re-implementation after `resolve()`.
    #[test]
    fn fallback_default_and_validator_compose_for_a_new_env_var_only_environment() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK; guard removes the var on any exit incl. panic.
        unsafe { std::env::set_var("THROWAWAY_API_URL", "https://api.throwaway.example.com") };
        let _guard = EnvGuard("THROWAWAY_API_URL");

        let envs = sample().with_fallback(|_name| {
            Some(
                EnvironmentDef::new()
                    .with_field("api_url", "")
                    .with_field_validator("api_url", |v| v.starts_with("https://"))
                    .with_field_default("domains_api_url", |env| {
                        env.extra.get("api_url").cloned().unwrap_or_default()
                    })
                    .with_field_default("account_url", |env| {
                        format!("https://account.{}.example.com", env.name)
                    }),
            )
        });

        let env = envs.resolve("throwaway").expect("fallback should resolve");
        assert_eq!(
            env.extra.get("api_url").map(String::as_str),
            Some("https://api.throwaway.example.com")
        );
        assert_eq!(
            env.extra.get("domains_api_url").map(String::as_str),
            Some("https://api.throwaway.example.com")
        );
        assert_eq!(
            env.extra.get("account_url").map(String::as_str),
            Some("https://account.throwaway.example.com")
        );
    }
}
