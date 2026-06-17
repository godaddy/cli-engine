# Engine Environments Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make environments a first-class `cli-engine` concept — definitions, layered resolution, sticky selection, an `env` command group, and per-environment OAuth — so consumers stop hand-rolling it.

**Architecture:** A new engine-owned `environments` module provides an `Environments` value (compiled defaults + `environments.toml` + `<ENV>_*` env-var overrides, later wins) resolving to an `Environment { name, oauth, extra }`. `CliConfig::with_environments` registers it: it seeds the existing `Middleware.env`, registers a global `--env` flag, and auto-mounts an `env list/get/set/info` group (mirroring the built-in `auth`/`config` groups). `PkceAuthProvider::with_environments` makes the environment the single source of OAuth config, replacing this work stream's unreleased `with_environment`/`OAuthEnvironment` builder.

**Tech Stack:** Rust, `clap`, `toml_edit` (already used by `ConfigFile`), `async-trait`, `tokio`. Feature `pkce-auth` gates the provider changes.

**Spec:** `docs/specs/2026-06-17-engine-environments-design.md`

**Conventions for every task:** Tests-first. Run `cargo test --features pkce-auth ...` for anything touching `src/auth`. Before each commit run `cargo fmt --all` and `cargo clippy --all-targets --features pkce-auth -- -D warnings`. Commit messages end with the project's `Co-Authored-By` trailer. Work on a feature branch, not `main`.

---

## File Structure

- Create `src/environments.rs` — `OAuthConfig`, `EnvironmentDef`, `Environment`, `Environments` (definitions, layered resolution, file + env-var sources, active-env helpers). One module, one responsibility: environment definitions and resolution.
- Create `src/env_commands.rs` — the `env` command group (`list`/`get`/`set`/`info`) as a `RuntimeGroupSpec`, mirroring `src/auth/commands.rs` and `src/config_commands.rs`.
- Modify `src/lib.rs` — declare/export the new modules and public types.
- Modify `src/cli.rs` — `CliConfig.environments` field + `with_environments` builder; `ensure_env_command`; `--env` global flag registration; seed `Middleware.env` from the resolved active environment.
- Modify `src/middleware.rs` — add `environments: Option<Arc<Environments>>` to `Middleware` and the per-run snapshot so handlers can resolve.
- Modify `src/command.rs` — `CommandContext::environment()` accessor (memoized resolve of the active env).
- Modify `src/config.rs` — (only if needed) a constant for the active-env config key; reuse existing `get`/`set`/`save`.
- Modify `src/auth/pkce.rs` — remove `OAuthEnvironment`/`with_environment`; add `with_environments(Arc<Environments>)`; `effective_*` reads the resolved `OAuthConfig` when env-wired, else the legacy `<PROVIDER>_OAUTH_*` fallback.
- Modify `docs/concepts.md` (or add `docs/environments.md`) — document the feature.

---

## Phase 1 — `environments` module: types & resolution

### Task 1: Core types

**Files:**
- Create: `src/environments.rs`
- Modify: `src/lib.rs` (add `pub mod environments;` and re-exports)

- [ ] **Step 1: Write the failing test**

In `src/environments.rs`, start the file with the types and a test module:

```rust
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_config_defaults_are_empty() {
        let c = OAuthConfig::default();
        assert!(c.client_id.is_empty() && c.scopes.is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails (compile-first)**

Run: `cargo test --lib environments::tests::oauth_config_defaults_are_empty`
Expected: FAIL — `environments` module not declared in `lib.rs`.

- [ ] **Step 3: Declare and export the module**

In `src/lib.rs`, alongside the other `pub mod` lines, add:

```rust
pub mod environments;
```

And in the public re-export area (near other `pub use`), add:

```rust
pub use environments::{Environment, EnvironmentDef, Environments, OAuthConfig};
```

(`EnvironmentDef`/`Environments` are added in Task 2; if the re-export fails to compile now, add only `Environment, OAuthConfig` here and extend the re-export in Task 2.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib environments::tests::oauth_config_defaults_are_empty`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/environments.rs src/lib.rs
git commit -m "feat(environments): add OAuthConfig and Environment core types"
```

---

### Task 2: `EnvironmentDef` (partial layer) and `Environments` builder

**Files:**
- Modify: `src/environments.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib environments::tests::builder_registers_compiled_environment`
Expected: FAIL — `EnvironmentDef`/`Environments` not found.

- [ ] **Step 3: Implement the partial-layer type and builder**

Add to `src/environments.rs` (above the `tests` module):

```rust
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
    /// by [`crate::CliConfig::with_environments`]; only call directly in tests.
    #[must_use]
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = app_id.into();
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
        // File names folded in during Task 5; compiled-only for now.
        self.defs.keys().cloned().collect()
    }
}
```

Extend the `lib.rs` re-export to include `EnvironmentDef, Environments` if not already added in Task 1.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib environments::tests::builder_registers_compiled_environment`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/environments.rs src/lib.rs
git commit -m "feat(environments): add EnvironmentDef and Environments builder"
```

---

### Task 3: Layer merge + `resolve` (compiled + env-var)

**Files:**
- Modify: `src/environments.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module. These tests set process env vars, so serialize them on a lock and clean up:

```rust
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
fn resolve_returns_compiled_record() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = sample().resolve("prod").expect("prod resolves");
    let oauth = env.oauth.expect("oauth present");
    assert_eq!(oauth.client_id, "prod-client");
    assert_eq!(oauth.scopes, vec!["openid".to_owned()]);
    assert_eq!(env.extra.get("api_url").map(String::as_str), Some("https://api.example.com"));
}

#[test]
fn resolve_unknown_env_errors_with_known_names() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let err = sample().resolve("nope").unwrap_err().to_string();
    assert!(err.contains("nope"));
    assert!(err.contains("prod") && err.contains("dev"));
}

#[test]
fn env_var_layer_overrides_oauth_and_known_bag_keys() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: serialized by ENV_LOCK; removed below.
    unsafe {
        std::env::set_var("PROD_OAUTH_CLIENT_ID", "override-client");
        std::env::set_var("PROD_API_URL", "https://api.override.example.com");
    }
    let env = sample().resolve("prod").expect("prod resolves");
    assert_eq!(env.oauth.unwrap().client_id, "override-client");
    assert_eq!(env.extra.get("api_url").map(String::as_str), Some("https://api.override.example.com"));
    unsafe {
        std::env::remove_var("PROD_OAUTH_CLIENT_ID");
        std::env::remove_var("PROD_API_URL");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib environments::tests::resolve`
Expected: FAIL — `resolve` not found.

- [ ] **Step 3: Implement merge + resolve**

Add these methods to `impl Environments` and a free merge helper:

```rust
impl Environments {
    /// Resolves `name` by merging compiled defaults, the config file (Task 5),
    /// and `<ENV>_*` env-var overrides (later wins) into an [`Environment`].
    pub fn resolve(&self, name: &str) -> Result<Environment> {
        let compiled = self.defs.get(name);
        let file = self.file_def(name)?; // Task 5 returns None until file support lands.
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

    /// Loads a per-environment definition from the config file. Returns `None`
    /// when the file layer is disabled or the env is absent. Implemented in Task 5.
    fn file_def(&self, _name: &str) -> Result<Option<EnvironmentDef>> {
        Ok(None)
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib environments::tests`
Expected: PASS (all resolve tests).

- [ ] **Step 5: Commit**

```bash
git add src/environments.rs
git commit -m "feat(environments): layered resolve with env-var overrides"
```

---

### Task 4: Config-file path helper

**Files:**
- Modify: `src/config.rs` (reuse `config_file_path`), `src/environments.rs`

- [ ] **Step 1: Write the failing test**

Add to `environments::tests`:

```rust
#[test]
fn environments_file_path_sits_next_to_config() {
    let envs = sample().with_app_id("gddy").with_config_file(true);
    let path = envs.config_file_path().expect("path resolves with app id");
    assert!(path.ends_with("gddy/environments.toml"), "got {path:?}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib environments::tests::environments_file_path_sits_next_to_config`
Expected: FAIL — `config_file_path` not found.

- [ ] **Step 3: Implement the path helper**

In `src/config.rs`, confirm `config_file_path(app_id) -> Option<PathBuf>` returns `<config-dir>/<app_id>/config.toml` (it does, per `config.rs:200`). Add to `impl Environments` in `src/environments.rs`:

```rust
impl Environments {
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
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib environments::tests::environments_file_path_sits_next_to_config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/environments.rs
git commit -m "feat(environments): resolve environments.toml path"
```

---

### Task 5: Load `environments.toml` into `file_def`

**Files:**
- Modify: `src/environments.rs`

- [ ] **Step 1: Write the failing test**

Add to `environments::tests` (writes a temp file; uses `tempfile`, already a dev-dependency). Inject the path via a test-only seam:

```rust
#[test]
fn file_layer_overrides_compiled_and_adds_custom_env() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

    let envs = sample().with_config_file(true).with_config_file_path_override(file);

    // File overrides the compiled prod client id, keeps compiled api_url.
    let prod = envs.resolve("prod").expect("prod");
    assert_eq!(prod.oauth.unwrap().client_id, "file-client");
    assert_eq!(prod.extra.get("api_url").map(String::as_str), Some("https://api.example.com"));

    // Custom env exists only in the file.
    let custom = envs.resolve("custom").expect("custom");
    assert_eq!(custom.oauth.unwrap().client_id, "custom-client");
    assert!(envs.list().contains(&"custom".to_owned()));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib environments::tests::file_layer_overrides_compiled_and_adds_custom_env`
Expected: FAIL — `with_config_file_path_override` and real `file_def` not implemented.

- [ ] **Step 3: Implement file loading**

Add a path override field and parsing. Change the `Environments` struct to add `file_path_override: Option<std::path::PathBuf>` (default `None`); update `new` accordingly. Replace `file_def` and add the parsed-file accessor + `list` update:

```rust
impl Environments {
    /// Test/advanced seam: force the environments file path.
    #[must_use]
    pub fn with_config_file_path_override(mut self, path: std::path::PathBuf) -> Self {
        self.file_path_override = Some(path);
        self.use_config_file = true;
        self
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
}
```

Update `list` to include file names:

```rust
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = self.defs.keys().cloned().collect();
        if let Ok(file) = self.file_defs() {
            names.extend(file.into_keys());
        }
        names.into_iter().collect()
    }
```

Add `use toml_edit;` is unnecessary (path-qualified). Ensure `BTreeMap` import already present.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib environments::tests`
Expected: PASS (all environments tests, including the file test).

- [ ] **Step 5: Commit**

```bash
git add src/environments.rs
git commit -m "feat(environments): load environments.toml as a resolution layer"
```

---

## Phase 2 — Active-environment persistence

### Task 6: Read/write the active environment via `ConfigFile`

**Files:**
- Modify: `src/environments.rs`

- [ ] **Step 1: Write the failing test**

Add to `environments::tests`:

```rust
const ACTIVE_KEY: &str = "environment.active";

#[test]
fn active_env_round_trips_through_config_file() {
    use crate::config::ConfigFile;
    let mut cfg = ConfigFile::default();
    assert_eq!(Environments::active_from_config(&cfg), None);

    cfg.set(ACTIVE_KEY, "ote").expect("set");
    assert_eq!(Environments::active_from_config(&cfg).as_deref(), Some("ote"));
}

#[test]
fn effective_active_prefers_override_then_config_then_default() {
    use crate::config::ConfigFile;
    let envs = sample();
    let mut cfg = ConfigFile::default();
    cfg.set(ACTIVE_KEY, "dev").expect("set");

    assert_eq!(envs.effective_active(Some("prod"), &cfg), "prod"); // explicit wins
    assert_eq!(envs.effective_active(None, &cfg), "dev");          // config next
    let empty = ConfigFile::default();
    assert_eq!(envs.effective_active(None, &empty), "prod");       // default last
}
```

If `ConfigFile::default()` does not exist, construct via `ConfigFile::load("<unused-app>")` against a temp HOME, or add a `#[cfg(test)]` constructor. Check `src/config.rs` — prefer adding `impl Default for ConfigFile` returning an empty document if absent.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib environments::tests::active_env_round_trips_through_config_file`
Expected: FAIL — `active_from_config`/`effective_active` not found.

- [ ] **Step 3: Implement persistence helpers**

Add to `src/environments.rs`:

```rust
/// Config-file key under which the sticky active environment is stored.
pub(crate) const ACTIVE_ENV_KEY: &str = "environment.active";

impl Environments {
    /// Reads the persisted active environment from a loaded config file.
    #[must_use]
    pub fn active_from_config(config: &crate::config::ConfigFile) -> Option<String> {
        config.get(ACTIVE_ENV_KEY)
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
    pub fn persist_active(&self, name: &str) -> Result<()> {
        self.resolve(name)?; // reject unknown names
        let mut config = crate::config::ConfigFile::load(&self.app_id);
        config.set(ACTIVE_ENV_KEY, name)?;
        config.save()
    }
}
```

If `ConfigFile` lacks `Default`, add to `src/config.rs`:

```rust
impl Default for ConfigFile {
    fn default() -> Self {
        Self { doc: toml_edit::DocumentMut::new(), path: None }
    }
}
```

(Match `ConfigFile`'s real field names; adjust if they differ.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib environments::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/environments.rs src/config.rs
git commit -m "feat(environments): persist and resolve the active environment"
```

---

## Phase 3 — Engine wiring

### Task 7: `Middleware.environments` field

**Files:**
- Modify: `src/middleware.rs`

- [ ] **Step 1: Write the failing test**

Add to the existing tests in `src/middleware.rs` (or create a small `#[cfg(test)] mod env_wire_tests`):

```rust
#[test]
fn middleware_carries_optional_environments() {
    use std::sync::Arc;
    let mut mw = Middleware::new();
    assert!(mw.environments.is_none());
    mw.environments = Some(Arc::new(crate::environments::Environments::new("prod")));
    assert_eq!(mw.environments.as_ref().unwrap().default_env(), "prod");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib middleware_carries_optional_environments`
Expected: FAIL — no `environments` field.

- [ ] **Step 3: Add the field**

In `src/middleware.rs`, add to `struct Middleware` (near `config`):

```rust
    /// Optional environment system, set by `CliConfig::with_environments`.
    pub environments: Option<std::sync::Arc<crate::environments::Environments>>,
```

Initialize it to `None` in `Middleware::new()` and in any other constructor/`Default`. If a per-run snapshot struct clones selected middleware fields, add `environments: self.environments.clone()` there too (search for where `config` is cloned into the snapshot and mirror it).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib middleware_carries_optional_environments`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/middleware.rs
git commit -m "feat(environments): thread environments through middleware"
```

---

### Task 8: `CliConfig.environments` + `with_environments`

**Files:**
- Modify: `src/cli.rs`

- [ ] **Step 1: Write the failing test**

Add to the `user_agent_tests` module's file (or a new `#[cfg(test)] mod env_config_tests` at the end of `src/cli.rs`):

```rust
#[test]
fn with_environments_stores_and_app_id_is_injected() {
    let cfg = CliConfig::new("gddy", "GoDaddy CLI", "gddy")
        .with_environments(crate::environments::Environments::new("prod").with_config_file(true));
    let envs = cfg.environments.as_ref().expect("environments set");
    // app_id is stamped from CliConfig so the file path resolves.
    assert!(envs.config_file_path().is_some());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib env_config_tests::with_environments_stores_and_app_id_is_injected`
Expected: FAIL — no `environments` field/builder.

- [ ] **Step 3: Add field + builder**

In `src/cli.rs`, add to `struct CliConfig` (near `auth_providers`):

```rust
    /// Optional first-class environment system.
    pub environments: Option<crate::environments::Environments>,
```

Add the builder (near `with_default_auth_provider`), stamping `app_id` so file/persistence paths resolve:

```rust
    /// Registers a first-class environment system: mounts the `env` command
    /// group, registers the global `--env` flag, and seeds the active
    /// environment into middleware. The provided `Environments` has this
    /// config's `app_id` applied so its config file and persistence resolve.
    #[must_use]
    pub fn with_environments(mut self, environments: crate::environments::Environments) -> Self {
        self.environments = Some(environments.with_app_id(self.app_id.clone()));
        self
    }
```

`#[derive(Default)]` covers the new `Option` field.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib env_config_tests::with_environments_stores_and_app_id_is_injected`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "feat(environments): add CliConfig::with_environments"
```

---

### Task 9: Seed active env into middleware + register `--env` flag

**Files:**
- Modify: `src/cli.rs`

- [ ] **Step 1: Write the failing test**

Add to `env_config_tests`:

```rust
#[tokio::test]
async fn env_flag_overrides_default_and_reaches_middleware_env() {
    use crate::{CommandResult, CommandSpec, RuntimeCommandSpec};
    use serde_json::json;
    let mut cli = Cli::new(
        CliConfig::new("envtest", "Env test", "envtest")
            .with_environments(
                crate::environments::Environments::new("prod")
                    .with_environment("prod", crate::environments::EnvironmentDef::new())
                    .with_environment("ote", crate::environments::EnvironmentDef::new()),
            ),
    );
    cli.add_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("whichenv", "echo env").no_auth(true),
        async |ctx| { Ok(CommandResult::new(json!({ "env": ctx.environment()?.name }))) },
    ));
    let out = cli.run(["envtest", "whichenv", "--env", "ote", "--output", "json"]).await;
    assert_eq!(out.exit_code, 0);
    assert!(out.rendered.contains("\"env\""));
    assert!(out.rendered.contains("ote"));
}
```

(Adjust the `new_with_context` closure form to match `auth/commands.rs` usage; see Task 11 for the exact handler signature and the `ctx.environment()` accessor it depends on — implement Task 10 and 11's accessor first if running strictly in order. If so, reorder: do Task 10 before this test passes.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features pkce-auth --lib env_config_tests::env_flag_overrides_default_and_reaches_middleware_env`
Expected: FAIL — `--env` not registered / `ctx.environment()` missing.

- [ ] **Step 3: Register the flag and seed middleware**

In `Cli::new` (`src/cli.rs`), after `middleware.config = …` and before building `cli`, add:

```rust
        if let Some(environments) = &config.environments {
            let environments = std::sync::Arc::new(environments.clone());
            // Seed the sticky/default active env now; the --env flag (applied
            // per run) overrides it in run_with_depth.
            middleware.env = environments.effective_active(None, &middleware.config);
            middleware.environments = Some(environments);
        }
```

Register the `--env` global flag when environments are configured. In `Cli::new`, where the root command is assembled (after `register_global_flags(root)`), add:

```rust
        if config.environments.is_some() {
            root = root.arg(
                clap::Arg::new("env")
                    .long("env")
                    .global(true)
                    .value_name("ENV")
                    .help("Target environment"),
            );
        }
```

Apply the flag per run: in `run_with_depth` (`src/cli.rs:1190`), after args are parsed into matches but before middleware executes the command, set the per-run env from the flag when present. Locate where the per-run middleware/`MiddlewareRequest` is built and insert:

```rust
        // --env overrides the seeded active environment for this invocation.
        if let Some(env) = parsed_global_env(&text_args) {
            if let Some(environments) = &self.middleware.environments {
                // Validate; unknown env -> error envelope.
                environments.resolve(&env)?;
            }
            run_env_override = Some(env);
        }
```

Add a small helper near the other `text_args` parsing helpers:

```rust
fn parsed_global_env(text_args: &[String]) -> Option<String> {
    let mut iter = text_args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--env" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--env=") {
            return Some(value.to_owned());
        }
    }
    None
}
```

Thread `run_env_override` into the per-run middleware so `MiddlewareRequest.env`/`Middleware.env` reflects it. Mirror how the existing code copies `self.middleware` into the per-run middleware (search `run_with_depth` for where it clones middleware) and set `.env` to the override when present, else keep the seeded value.

> Implementation note: if `run_with_depth` already clones `self.middleware` into a mutable per-run value, set `per_run.env = run_env_override.unwrap_or_else(|| self.middleware.env.clone())`. Keep the change minimal and within the existing clone path.

- [ ] **Step 4: Run test to verify it passes** (after Task 10 accessor exists)

Run: `cargo test --features pkce-auth --lib env_config_tests::env_flag_overrides_default_and_reaches_middleware_env`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "feat(environments): register --env and seed active env into middleware"
```

---

### Task 10: `CommandContext::environment()` accessor

**Files:**
- Modify: `src/command.rs`

- [ ] **Step 1: Write the failing test**

The behavior is exercised by Task 9's integration test. Add a focused unit check in `src/command.rs` tests if a context can be constructed there; otherwise rely on Task 9. Minimum: add a doctest on the method (Step 3).

- [ ] **Step 2: Run Task 9 test to verify it fails**

Run: `cargo test --features pkce-auth --lib env_config_tests::env_flag_overrides_default_and_reaches_middleware_env`
Expected: FAIL — `ctx.environment()` missing.

- [ ] **Step 3: Implement the accessor**

In `src/command.rs`, add to `impl CommandContext` (near `config`):

```rust
    /// Resolves the active [`Environment`](crate::environments::Environment) for
    /// this invocation.
    ///
    /// The active environment name is `Middleware.env` (set from `--env`, the
    /// persisted active env, or the configured default). Returns an error if no
    /// environment system was registered via
    /// [`CliConfig::with_environments`](crate::CliConfig::with_environments) or
    /// if the active name does not resolve.
    pub fn environment(&self) -> Result<crate::environments::Environment> {
        let environments = self.middleware.environments.as_ref().ok_or_else(|| {
            crate::error::CliCoreError::message("no environment system configured")
        })?;
        environments.resolve(&self.middleware.env)
    }
```

(Return an owned `Environment` to avoid borrowing/memoization complexity; resolution is cheap and runs at most a few times per invocation. If the per-run snapshot exposes env differently than `self.middleware.env`, use that field name.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --features pkce-auth --lib env_config_tests::env_flag_overrides_default_and_reaches_middleware_env`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/command.rs
git commit -m "feat(environments): add CommandContext::environment accessor"
```

---

## Phase 4 — `env` command group

### Task 11: `env list/get/set/info` group, auto-mounted

**Files:**
- Create: `src/env_commands.rs`
- Modify: `src/lib.rs` (add `mod env_commands;`), `src/cli.rs` (`ensure_env_command`, call it)

- [ ] **Step 1: Write the failing tests**

Add an integration test to `tests/foundation.rs` (mirrors existing `Cli::run` tests):

```rust
#[tokio::test]
async fn env_group_lists_gets_and_sets_active_environment() {
    use cli_engine::environments::{EnvironmentDef, Environments};
    let cli = Cli::new(
        CliConfig::new("envcmds", "Env cmds", "envcmds").with_environments(
            Environments::new("prod")
                .with_environment("prod", EnvironmentDef::new().with_field("api_url", "https://p"))
                .with_environment("ote", EnvironmentDef::new().with_field("api_url", "https://o")),
        ),
    );

    let list = cli.run(["envcmds", "env", "list", "--output", "json"]).await;
    assert_eq!(list.exit_code, 0);
    assert!(list.rendered.contains("prod") && list.rendered.contains("ote"));

    let get = cli.run(["envcmds", "env", "get", "--output", "json"]).await;
    assert_eq!(get.exit_code, 0);
    assert!(get.rendered.contains("prod")); // default active

    let info = cli.run(["envcmds", "env", "info", "--env", "ote", "--output", "json"]).await;
    assert_eq!(info.exit_code, 0);
    assert!(info.rendered.contains("https://o"));
}
```

> Note: `env set` persists to the real per-app config file. Do NOT assert `set` persistence in a shared-process test unless the test isolates `$HOME`/`$XDG_CONFIG_HOME` via a temp dir guard. Cover `set` persistence in a `#[cfg(test)]` unit test in `environments.rs` (Task 6 already covers the round-trip) and keep this integration test to `list`/`get`/`info`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features pkce-auth --test foundation env_group_lists_gets_and_sets_active_environment`
Expected: FAIL — `env` group not mounted.

- [ ] **Step 3: Implement the group**

Create `src/env_commands.rs`, mirroring `src/auth/commands.rs` structure and `src/config_commands.rs`:

```rust
//! Built-in `env` command group: list/get/set/info for environments.

use serde_json::json;

use crate::{
    CommandResult, CommandSpec, GroupSpec, RuntimeCommandSpec, RuntimeGroupSpec,
    error::CliCoreError,
};

/// Builds the built-in `env` command group.
pub fn env_command_group() -> RuntimeGroupSpec {
    RuntimeGroupSpec::new(GroupSpec::new("env", "Manage the active environment"))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("list", "List known environments").no_auth(true),
            async |ctx| {
                let envs = ctx
                    .middleware
                    .environments
                    .as_ref()
                    .ok_or_else(|| CliCoreError::message("no environment system configured"))?;
                let active = ctx.middleware.env.clone();
                let items: Vec<_> = envs
                    .list()
                    .into_iter()
                    .map(|name| json!({ "name": name, "active": name == active }))
                    .collect();
                Ok(CommandResult::new(json!(items)))
            },
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("get", "Show the active environment").no_auth(true),
            async |ctx| {
                Ok(CommandResult::new(json!({ "active": ctx.middleware.env })))
            },
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("info", "Show the resolved active environment").no_auth(true),
            async |ctx| {
                let env = ctx.environment()?;
                let oauth = env.oauth.map(|o| {
                    json!({ "client_id": o.client_id, "auth_url": o.auth_url, "token_url": o.token_url, "scopes": o.scopes })
                });
                Ok(CommandResult::new(json!({
                    "name": env.name,
                    "oauth": oauth,
                    "extra": env.extra,
                })))
            },
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("set", "Set and persist the active environment")
                .no_auth(true)
                .with_arg(clap::Arg::new("name").required(true)),
            async |ctx| {
                let envs = ctx
                    .middleware
                    .environments
                    .as_ref()
                    .ok_or_else(|| CliCoreError::message("no environment system configured"))?;
                let name = ctx
                    .args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| CliCoreError::message("missing environment name"))?;
                envs.persist_active(name)?;
                Ok(CommandResult::new(json!({ "active": name })))
            },
        ))
}
```

> Adjust `ctx.args` access to match how `auth/commands.rs` reads arguments (e.g. it may use `ctx.args.get(...)` or a typed accessor). Copy that idiom exactly.

In `src/lib.rs` add `mod env_commands;` (private; only `cli.rs` needs it). In `src/cli.rs`, add `ensure_env_command` mirroring `ensure_config_command`:

```rust
    fn ensure_env_command(&mut self) {
        if has_subcommand(&self.root, "env") {
            return;
        }
        let group = crate::env_commands::env_command_group();
        let mut prefix = Vec::new();
        group.register_commands(&mut prefix, &mut self.commands);
        let mut prefix = Vec::new();
        let clap_group = runtime_group_clap_command_with_schema_help(
            &group,
            &mut prefix,
            &self.middleware.schema_registry,
        );
        self.root = self.root.clone().subcommand(clap_group);
        let category = self
            .config
            .admin_category
            .clone()
            .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
        if !self.module_entries.iter().any(|e| e.name == "env") {
            self.module_entries.push(ModuleHelpEntry {
                category,
                name: "env".to_owned(),
                short: "Manage the active environment".to_owned(),
            });
        }
        self.refresh_root_long();
    }
```

Call it in `Cli::new`, right after the environments are seeded into middleware:

```rust
        if cli.config.environments.is_some() {
            cli.ensure_env_command();
        }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --features pkce-auth --test foundation env_group_lists_gets_and_sets_active_environment`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/env_commands.rs src/lib.rs src/cli.rs
git commit -m "feat(environments): add built-in env command group"
```

---

## Phase 5 — OAuth tie-in (replace `with_environment`)

### Task 12: Remove `OAuthEnvironment`/`with_environment`; add `with_environments`

**Files:**
- Modify: `src/auth/pkce.rs`

- [ ] **Step 1: Write the failing test**

Replace the existing `environment_override_selects_per_env_oauth_config` and `unconfigured_environment_falls_back_to_base_config` tests in `pkce.rs` with environment-sourced equivalents:

```rust
fn envs_for_test() -> std::sync::Arc<crate::environments::Environments> {
    use crate::environments::{EnvironmentDef, Environments};
    std::sync::Arc::new(
        Environments::new("prod").with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client")
                .with_auth_url("https://prod.example.com/auth")
                .with_token_url("https://prod.example.com/token")
                .with_scopes(&["openid", "prod.read"]),
        ),
    )
}

#[test]
fn environment_wired_provider_sources_oauth_from_resolver() {
    let provider = PkceAuthProvider::new("godaddy", "https://base/auth", "https://base/token", "base-client", &["openid"])
        .with_environments(envs_for_test());
    assert_eq!(provider.effective_client_id("prod"), "prod-client");
    assert_eq!(provider.effective_auth_url("prod"), "https://prod.example.com/auth");
    assert_eq!(provider.effective_token_url("prod"), "https://prod.example.com/token");
    assert_eq!(provider.effective_scopes("prod"), vec!["openid".to_owned(), "prod.read".to_owned()]);
}

#[test]
fn non_wired_provider_uses_base_config() {
    let provider = PkceAuthProvider::new("godaddy", "https://base/auth", "https://base/token", "base-client", &["openid"]);
    assert_eq!(provider.effective_client_id("anything"), "base-client");
    assert_eq!(provider.effective_scopes("anything"), vec!["openid".to_owned()]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --features pkce-auth --lib auth::pkce::tests::environment_wired_provider_sources_oauth_from_resolver`
Expected: FAIL — `with_environments` not found; `OAuthEnvironment` still referenced elsewhere.

- [ ] **Step 3: Replace the per-env map with a resolver**

In `src/auth/pkce.rs`:

1. Delete the `OAuthEnvironment` struct, its `impl`, the `environments: HashMap<String, OAuthEnvironment>` field, the `with_environment` method, and any `OAuthEnvironment` doctest/re-export.
2. Add a resolver field and builder:

```rust
    /// Optional environment resolver; when set, per-env OAuth config comes from
    /// the resolved environment instead of the base config / legacy env override.
    environments: Option<std::sync::Arc<crate::environments::Environments>>,
```

```rust
    /// Sources per-environment OAuth config from a shared [`Environments`].
    ///
    /// Given an `env`, the provider resolves the environment and uses its
    /// `OAuthConfig`. This is the single-source-of-truth path; prefer it over
    /// the base `client_id`/`auth_url`/`token_url` when the consumer registers
    /// environments via [`CliConfig::with_environments`](crate::CliConfig::with_environments).
    #[must_use]
    pub fn with_environments(
        mut self,
        environments: std::sync::Arc<crate::environments::Environments>,
    ) -> Self {
        self.environments = Some(environments);
        self
    }
```

3. Initialize `environments: None` in `new`.
4. Rewrite the `effective_*` methods to prefer the resolver, then the legacy `<PROVIDER>_OAUTH_*` env override (kept for non-wired providers), then base:

```rust
    fn resolved_oauth(&self, env: &str) -> Option<crate::environments::OAuthConfig> {
        self.environments
            .as_ref()
            .and_then(|envs| envs.resolve(env).ok())
            .and_then(|resolved| resolved.oauth)
    }

    fn effective_client_id(&self, env: &str) -> String {
        if let Some(oauth) = self.resolved_oauth(env) {
            if !oauth.client_id.is_empty() {
                return oauth.client_id;
            }
        }
        let key = format!("{}_OAUTH_CLIENT_ID", self.env_prefix);
        std::env::var(&key).unwrap_or_else(|_| self.client_id.clone())
    }

    fn effective_auth_url(&self, env: &str) -> String {
        if let Some(oauth) = self.resolved_oauth(env) {
            if !oauth.auth_url.is_empty() {
                return oauth.auth_url;
            }
        }
        let key = format!("{}_OAUTH_AUTH_URL", self.env_prefix);
        std::env::var(&key).unwrap_or_else(|_| self.auth_url.clone())
    }

    fn effective_token_url(&self, env: &str) -> String {
        if let Some(oauth) = self.resolved_oauth(env) {
            if !oauth.token_url.is_empty() {
                return oauth.token_url;
            }
        }
        let key = format!("{}_OAUTH_TOKEN_URL", self.env_prefix);
        std::env::var(&key).unwrap_or_else(|_| self.token_url.clone())
    }

    fn effective_scopes(&self, env: &str) -> Vec<String> {
        if let Some(oauth) = self.resolved_oauth(env) {
            if !oauth.scopes.is_empty() {
                return oauth.scopes;
            }
        }
        self.scopes.clone()
    }
```

5. Remove the now-stale `perenv_provider` test helper and the old per-env tests replaced in Step 1. Keep all token-timeout, shared-client, and User-Agent tests/code from the prior work stream unchanged.
6. Remove `OAuthEnvironment` from any `lib.rs`/module re-exports.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --features pkce-auth --lib auth::pkce::tests`
Expected: PASS (new env-wired tests + retained timeout/UA tests).

- [ ] **Step 5: Commit**

```bash
git add src/auth/pkce.rs src/lib.rs
git commit -m "feat(environments)!: source PkceAuthProvider OAuth from Environments"
```

---

## Phase 6 — Docs & final verification

### Task 13: Document environments

**Files:**
- Create: `docs/environments.md`; Modify: `docs/concepts.md` (add a short pointer)

- [ ] **Step 1: Write the doc**

Create `docs/environments.md` covering: the three resolution layers and precedence; the `environments.toml` schema (`[<name>]` tables with `client_id`/`auth_url`/`token_url`/`scopes` typed and any other key as a string bag field); the `<ENV>_*` env-var convention; the sticky active env + `--env` + `env list/get/set/info`; and the `PkceAuthProvider::with_environments` single-source pattern with a runnable example. Add a one-line pointer + link in `docs/concepts.md`.

- [ ] **Step 2: Verify doctests/build**

Run: `cargo test --doc --features pkce-auth`
Expected: PASS (any fenced `rust` examples compile).

- [ ] **Step 3: Commit**

```bash
git add docs/environments.md docs/concepts.md
git commit -m "docs(environments): document first-class environments"
```

---

### Task 14: Full verification sweep

- [ ] **Step 1: Format + lint (both feature modes)**

```bash
cargo fmt --all --check
cargo clippy --all-targets --features pkce-auth -- -D warnings
cargo clippy --all-targets -- -D warnings
```
Expected: clean.

- [ ] **Step 2: Tests + doctests + docs**

```bash
cargo test --features pkce-auth --all-targets
cargo test --doc --features pkce-auth
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --features pkce-auth
cargo rustdoc --lib --features pkce-auth -- -W missing-docs
```
Expected: all pass; missing-docs count zero.

- [ ] **Step 3: Final commit if any fmt/doc fixups were needed**

```bash
git add -A
git commit -m "chore(environments): formatting and doc fixups"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** Tasks 1–5 = core/resolution/file; Task 6 = active-env persistence; Tasks 7–10 = engine wiring + context accessor; Task 11 = `env` group; Task 12 = OAuth single-source + `with_environment` removal; Task 13 = docs. Migration (gddy re-login, gdx greenfield) happens in those repos and is out of scope here.
- **Ordering caveat:** Task 9's integration test depends on the `ctx.environment()` accessor from Task 10 — implement Task 10's accessor before expecting Task 9's test to go green (or treat 9+10 as a pair).
- **Verify-as-you-go:** Several wiring snippets (`run_with_depth` per-run middleware clone, `ctx.args` idiom, `ConfigFile` field names for `Default`) must be matched to the real code at implementation time; the referenced mirror points (`ensure_config_command`, `auth_command_group`, `CommandContext::config`) are exact.
- **Non-negotiables:** env-var/config-file tests serialize on a lock and restore process state; `unsafe { set_var }` is test-only and paired with `remove_var`.
