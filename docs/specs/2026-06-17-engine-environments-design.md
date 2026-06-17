# First-Class Environments in cli-engine — Design

Date: 2026-06-17
Status: Implemented (plan in `docs/plans/2026-06-17-engine-environments-plan.md`)

## Context

Two consumers of `cli-engine` — `gddy` (the GoDaddy developer CLI) and `gdx` —
each independently reimplement "multiple environments" (e.g. `dev`/`test`/`ote`/`prod`):

- `gddy` (`src/environments/mod.rs`, `src/auth.rs`, `src/env/mod.rs`): a rich,
  runtime-extensible system — compiled built-ins, a `~/.config/gddy/environments.toml`
  file, and `<PREFIX>_*` env-var overrides; records carry `client_id`, `auth_url`,
  `token_url`, `api_url`, `domains_api_url`, `account_url`, `api_key`/`api_secret`; a
  global `--env` flag with a sticky `.gdenv` state file; and `env list/get/set/info`
  commands. It builds one `PkceAuthProvider` per environment on demand, with the
  provider name set to the env name.
- `gdx` (`rust/src/auth.rs`): a hardcoded `match env { "dev"|"test"|"ote"|"prod" }`
  mapping to `client_id` + `api_base`. No environment config file.

The engine already owns *part* of an environment concept: `Middleware.env`,
default-env injection (`middleware.rs:586`), `CommandMeta::fixed_env()`, and the
`env` argument passed to `AuthProvider::get_credential`. It does not own the
`--env` flag, active-env persistence, environment definitions/resolution, or the
`env` command group — so each consumer rebuilds those.

**Goal:** make environments a first-class engine mechanism so `gddy` and `gdx`
delete their hand-rolled flag/persistence/subcommands/resolution and net-remove
code, with the environment as the single source of truth for both API base URLs
and per-environment OAuth config.

This supersedes the unreleased `PkceAuthProvider::with_environment` /
`OAuthEnvironment` builder added earlier in this work stream (still uncommitted),
which is replaced by the resolver described here.

## Decisions (from brainstorming)

1. Primary objective: **kill duplication** — both consumers migrate and remove code.
2. Boundary: **engine owns the format + resolution** (definitions, file schema,
   env-var convention, merging), not just the lifecycle.
3. Record shape: **standard typed OAuth fields + a `BTreeMap<String,String>` bag**
   for app-specific fields. No generics on `Cli`/`Middleware`.
4. Resolution: **three layers, later wins** — compiled defaults < config file <
   env-var overrides.
5. Selection: **sticky active env persisted in the per-app config file**, a `--env`
   override, and an auto-mounted `env list/get/set/info` group.
6. OAuth tie-in: **the environment is the single source**; `PkceAuthProvider` reads
   its OAuth config from the resolver. Replaces the unreleased `with_environment`.

Rejected alternative: a generic `Environment<T>` threaded through `Cli`/`Middleware`
(fully typed end-to-end but a large blast radius). The string bag is chosen instead.

## Core Types

```rust
/// Standard OAuth slice of an environment, consumed by PkceAuthProvider.
#[derive(Clone, Debug, Default)]
pub struct OAuthConfig {
    pub client_id: String,
    pub auth_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
}

/// One fully-resolved environment.
#[derive(Clone, Debug)]
pub struct Environment {
    pub name: String,
    /// Present when the environment participates in OAuth.
    pub oauth: Option<OAuthConfig>,
    /// App-specific fields (api_url, domains_api_url, account_url, …).
    pub extra: BTreeMap<String, String>,
}

/// Engine-owned environment system: definitions + resolution + active-env state.
/// Built once by the consumer and shared (Arc) between CliConfig and any
/// PkceAuthProvider that is environment-driven.
#[derive(Clone, Debug)]
pub struct Environments { /* compiled defaults, file path, config handle, default name */ }
```

Builder sketch:

```rust
Environments::new("prod")                                  // default env name
    .with_environment("prod", EnvironmentDef::new()
        .with_oauth(OAuthConfig { client_id: "…".into(), auth_url: "…".into(),
                                  token_url: "…".into(), scopes: vec!["openid".into()] })
        .with_field("api_url", "https://api.godaddy.com"))
    .with_environment("ote", /* … */)
    .with_config_file(true);    // enable ~/.config/<app>/environments.toml + env-var layer
```

`EnvironmentDef` is the *unresolved* per-env declaration (the same shape used to
parse the TOML file); `Environment` is the *resolved* result after merging layers.

## Resolution

`Environments::resolve(&self, name: &str) -> Result<Environment>` merges, later wins:

1. **Compiled defaults** declared via `with_environment` on `CliConfig`/`Environments`.
2. **Config file** `~/.config/<app>/environments.toml` (XDG/`%APPDATA%` per platform,
   same dir the engine already uses for credentials/config). Schema mirrors `gddy`'s:
   standard OAuth keys parse into `OAuthConfig`; every other key lands in `extra`.
   Enables user-defined custom environments.
3. **Env-var overrides**, keyed by env name (uppercased, `-`→`_`):
   - `<ENV>_OAUTH_CLIENT_ID`, `<ENV>_OAUTH_AUTH_URL`, `<ENV>_OAUTH_TOKEN_URL`
   - `<ENV>_<KEY>` for bag fields, e.g. `PROD_API_URL`.

Behavior:
- Unknown env name → typed error listing enumerable env names.
- `Environments::list()` returns compiled + file-defined envs. Environment-variable
  layers only *override fields* of an environment already defined by a compiled
  default or the file; env vars alone do not define a new, selectable environment.
- Merge is field-level: a file or env-var layer may override individual OAuth fields
  or individual bag keys without restating the whole record.

## Selection & Lifecycle

- The engine registers a **global `--env` flag** only when environments are configured.
- **Sticky active env** stored in the existing per-app `ConfigFile` under a new
  `active_environment` key (no separate state file). Precedence for the active env:
  `--env` value > persisted active > `Environments` default.
- Auto-mounted **`env` command group** (mounted like the built-in `auth` group):
  - `env list` — enumerable environments, marking the active one.
  - `env get` — prints the active environment name.
  - `env set <name>` — validates via `resolve()` (the name must be defined by a
    compiled default or `environments.toml`), then persists to `ConfigFile`.
  - `env info` — shows the resolved active environment (OAuth endpoints + bag),
    with secrets omitted.
- The resolved active env name is written to the existing `Middleware.env`, so
  default-env injection (`middleware.rs:586`) and `CommandMeta::fixed_env()` keep
  working unchanged.

## Engine Wiring & Handler Access

- `CliConfig::with_environments(Environments)` — registers the system: mounts the
  `env` group, registers `--env`, seeds `Middleware.env` from the resolved active env.
- Handlers access the resolved active environment through a context accessor:
  `ctx.environment() -> Result<&Environment>` (resolved once per run and memoized,
  like the credential resolver). Handlers read base URLs from `env.extra`:
  `env.extra.get("api_url")`.
- `--schema` / `--dry-run` short-circuits must not force file/env resolution
  (resolution is lazy, consistent with credential-store resolution today).

## OAuth Tie-In

- `PkceAuthProvider::with_environments(Arc<Environments>)` **replaces** the unreleased
  `with_environment` / `OAuthEnvironment` builder (removed outright — no deprecation
  needed since it is uncommitted).
- An environment-wired provider, given `env`, calls `environments.resolve(env)?` and
  uses the resulting `OAuthConfig` (`client_id`/`auth_url`/`token_url`/`scopes`) for the
  flow. One definition drives both API base URLs (bag) and OAuth; custom/file/env-var
  environments get OAuth automatically.
- The pre-existing released `<PROVIDER>_OAUTH_*` env override remains **only** for
  providers that are *not* environment-wired (back-compat for current consumers).
  Environment-wired providers use the `<ENV>_OAUTH_*` layer instead.
- Provider-level config that is genuinely not per-env (redirect URI/port, app_id,
  identity claims, token timeout, shared client) stays on `PkceAuthProvider` as today.

## Migration & Back-Compat

- **`with_environment` removal**: safe — uncommitted in this work stream.
- **gddy credential storage keys**: today provider-name = env-name yields keys
  `gddy/<env>/<env>`. A single environment-wired provider (e.g. named `godaddy`)
  yields `gddy/godaddy/<env>`. This changes the keys, so existing users re-authenticate
  once after upgrade. **Decision: accept the one-time re-login**, documented in the
  changelog. No storage-key compat shim.
- **gddy config file**: the engine's `environments.toml` schema is a superset of
  gddy's existing file, so migration is near-zero (standard OAuth keys typed, the rest
  into `extra`). gddy's URL derivation (`auth_url`/`token_url` from `api_url`) becomes
  declaring explicit URLs in compiled defaults, or specifying them per custom env;
  the engine does not own a derivation rule.
- **gdx**: greenfield — declares compiled defaults, deletes its `match env` OAuth maps,
  and may drop its bespoke env plumbing. gdx's `ote`→`test` SSO aliasing stays
  app-side (it is not OAuth-environment resolution).
- The per-env token timeout, shared token client, and User-Agent work from this work
  stream are unaffected and carry forward.

## Testing

- **Unit:** resolution precedence across all three layers; field-level merge;
  unknown-env error contents; `list()` enumeration; env-var layers override fields of
  a defined env (not define new ones);
  env-var keying (`<ENV>_OAUTH_*`, `<ENV>_<KEY>`); active-env persistence round-trip
  through `ConfigFile`; lazy resolution (no file/env access on `--schema`/`--dry-run`).
- **Integration (`Cli::run`):** `env set`/`get`/`list`/`info`; `--env` overrides the
  sticky active env; an environment-wired `PkceAuthProvider` selects per-env OAuth
  (verified with the offline request-builder seam already used for UA/timeout tests).
- **Doctests** on the public builders (`Environments`, `EnvironmentDef`, `OAuthConfig`,
  `CliConfig::with_environments`, `PkceAuthProvider::with_environments`).
- Env-var and config-file tests serialize on a lock and restore process state, matching
  existing `credential_store_config`/User-Agent test conventions.

## Clarifications (decided; called out for implementation)

- `env set` validates via `resolve()`; the name must be defined by a compiled default
  or `environments.toml`. Environment variables override fields of a defined env but
  cannot define a new, selectable environment on their own.
- Exact reconciliation of the `<ENV>_OAUTH_*` (environment layer) vs `<PROVIDER>_OAUTH_*`
  (legacy provider layer) precedence is: environment-wired providers use the environment
  layer exclusively; non-wired providers retain the legacy layer.
