# Environments

`cli_engine` provides a first-class environment system that lets CLIs target named deployment environments — `prod`, `ote`, `dev`, and any others the application defines — without consumers having to wire flags, config lookups, or OAuth overrides by hand.

When `CliConfig::with_environments` is called, the engine:

- Registers a global `--env` flag on every command.
- Seeds the active environment into middleware at startup.
- Exposes the resolved environment to handlers via `CommandContext::environment` (raw, generic) or `CommandContext::environment_config::<T>()` (typed).
- Mounts the built-in `env list / get / set / info` commands under the admin help category.

## How environment config is derived

A typed `#[derive(EnvConfig)]` struct, which declares per field where to find its value and how to convert it, gets built by `Environments` threading an ordered chain of sources — the active environment's merged TOML table plus an app-scoped environment-variable source — through that struct's generated `assemble` function.

## `EnvConfig` and `#[derive(EnvConfig)]`

```rust
use cli_engine::EnvConfig;

#[derive(Debug, Clone, EnvConfig)]
struct ApiConfig {
    api_url: String,

    #[env_config(default_fn = derive_domains_api_url)]
    domains_api_url: String,

    #[env_config(env = "ACCOUNT_URL", default_fn = derive_account_url)]
    account_url: String,
}

fn derive_domains_api_url(sources: &cli_engine::env_config::SourceChain<'_>) -> String {
    // Look up another key on the same chain to derive this one.
    # let _ = sources; String::new()
}
fn derive_account_url(sources: &cli_engine::env_config::SourceChain<'_>) -> String {
    # let _ = sources; String::new()
}
```

Per-field `#[env_config(...)]` attributes:

| Attribute | Meaning | Default when omitted |
| --- | --- | --- |
| `key = "..."` | TOML key to look up in the environment's merged table | the field's Rust name |
| `env = "SUFFIX"` | opt-in; final env var checked is `<APP_ID_UPPER>_<SUFFIX>` (see [Environment-Variable Overrides](#environment-variable-overrides-are-app-scoped)) | not environment-variable overridable at all |
| `default = <expr>` | literal fallback of the field's own type | none |
| `default_fn = <path>` | `fn(&SourceChain<'_>) -> T`, computed lazily only when no source has a value; takes the whole chain so it can look up *other* keys itself | none |
| `from_toml = <path>` | `fn(&toml::Value) -> Result<T, String>`, replaces the default `T: serde::de::DeserializeOwned` conversion | `T::deserialize` |
| `from_env = <path>` | `fn(&str) -> Result<T, String>`, replaces the default `T: FromStr` conversion | `T::from_str` (a field with `env` set and no `from_env` needs `T: FromStr`) |
| `to_toml = <path>` | `fn(T) -> toml::Value`, replaces the default `T: Into<toml::Value>` conversion used the *other* direction — building an `EnvTable` from a struct value (see [Registering compiled-in environments as typed values](#registering-compiled-in-environments-as-typed-values)) | `Into::into` |
| `allow_blank` | bare marker, no value — see below | not applied; a blank value is treated as absent |

`default` and `default_fn` are mutually exclusive. Resolution per field, in order: env var (if `env` set and a source answers it with a non-blank string) → TOML value (if a source answers it with a non-blank value) → `default`/`default_fn` → a `MissingField` error. Any present, non-blank value that fails its conversion is a hard `InvalidField` error.

Because a field's type participates directly, a TOML array becomes a `Vec<String>` with no attribute at all, a TOML table becomes a nested struct, and `Stage` (already `Deserialize` + `FromStr`) needs no per-field wiring either.

**Blank is absent by default.** A source that answers a field with an empty-or-whitespace-only string, or an empty TOML array (`[]`), is treated the same as that source not answering at all — resolution keeps walking the rest of the chain and ultimately falls to `default`/`default_fn`, exactly as if the blank source had said nothing. (Any other empty TOML shape, like an empty table, doesn't count as blank.)

**`allow_blank` opts a field *out* of that default**, for the rare case where `""` or `[]` is itself a meaningful, literal answer rather than a stand-in for "unset."

## `ConfigSource` and `SourceChain`

A [`ConfigSource`] answers three questions: "do you have a TOML value for this key," "do you have an env-var string for this suffix," and "do you represent an environment, and if so, which one." Three sources are provided:

- **`EnvSource`** — one environment's merged TOML table (compiled-in table overlaid by the `environments.toml` file table for the same name, file wins key-by-key, shallow — no deep-merging into nested tables/arrays). TOML-only; never answers an env var. The only source that answers `env_name`.
- **`EnvVarSource { prefix }`** — env-var-only, under an arbitrary prefix. Used for the app-scoped override tier (prefix = app id).
- **`ValueSource`** — an in-memory table built from values a consumer already has in hand (for example a provider's own constructor arguments). TOML-only in the sense that it's a plain key → value lookup; never answers an env var.

`EnvSource` and `ValueSource` store `toml::Value`s directly rather than a `cli_engine`-owned type — deliberately, since that's the one representation the `environments.toml` file, a compiled-in value, and a typed struct field can all share with no conversion step in between. An environment variable is the one source that's never TOML-shaped — env vars are always plain strings.

A [`SourceChain`] is an ordered list of these. Assembly walks the chain: within one source, its env var (if the field opted in) is checked before its TOML value; the first source to answer *either* wins for that field, and the rest of the chain is never consulted for it. Two read-only helpers are for a `default_fn`'s use, so it can derive a field from context beyond its own key:

- `SourceChain::toml_value(key)` — the first source in the chain with a TOML value for `key`, so one field can derive from a *sibling* field's raw value (`domains_api_url` deriving from `api_url`, for example) without hand-rolling the chain-walk `resolve_field` already does.
- `SourceChain::env_name()` — the environment name of the first source that has one, so a field can derive from the environment's own identity (`account_url` deriving from the environment name, for example). `None` when the chain has no `EnvSource` in it at all (for instance, a chain built entirely from `ValueSource`s).

Together, `default_fn` and these two helpers are usually enough for a struct's own fields to fully self-derive, with no separate post-processing function needed after `resolve`/`assemble` returns — see `gddy`'s `GddyEnvConfig` for a worked example.

## Registering compiled-in environments as typed values

`Environments::with_environment` doesn't require building an `EnvTable` by hand — `#[derive(EnvConfig)]` also generates `impl From<Self> for EnvTable`, mapping each field back to its own `key`, so `with_environment` accepts a struct *value* directly:

```rust
use cli_engine::{EnvConfig, environments::Environments};

#[derive(Default, EnvConfig)]
struct ApiConfig {
    api_url: String,
    #[env_config(default = String::new())]
    client_id: String,
}

let environments = Environments::new("prod").with_environment(
    "prod",
    ApiConfig {
        api_url: "https://api.example.com".to_owned(),
        client_id: "prod-client-id".to_owned(),
    },
);
```

## `Environments::resolve` — the common path

```rust,no_run
use cli_engine::environments::{EnvTable, Environments};

let environments = Environments::new("prod")
    .with_app_id("my-cli")
    .with_environment(
        "prod",
        EnvTable::new().with("api_url", "https://api.example.com"),
    )
    .with_config_file(true);

# #[derive(cli_engine::EnvConfig)] struct ApiConfig { api_url: String }

let api: ApiConfig = environments.resolve("prod").expect("resolves");
```

`resolve::<T>(name)` merges the chain of configuration sources and stores it in data structure `T`.

For generic introspection without any particular `T` in mind, use `Environments::source(name)`, which returns the merged `EnvSource` directly.

### Resolution layers

For a field that opts in via `env = "SUFFIX"` (see [Environment-Variable Overrides](#environment-variable-overrides-are-app-scoped)), precedence is, highest first:

1. **App-scoped environment variable** — `<APP_ID_UPPER>_<SUFFIX>`.
2. **`environments.toml`** — the file at `<config-dir>/<app-id>/environments.toml`.
3. **Compiled-in defaults** — values registered with `Environments::with_environment` in application source code.

A field that doesn't opt in via `env = "..."` skips layer 1 entirely — only the file and the compiled-in default apply, file winning when both set the same key.

A name unknown to both the compiled-in and file layers can still resolve via `Environments::with_fallback(fn(&str) -> Option<EnvTable>)`, for a consumer that wants some environments to be discoverable purely from their own env vars rather than requiring a compiled table or file entry. This is an opt-in escape hatch, not a default: most consumers are better served requiring every non-compiled environment to go through `environments.toml`, since a name defined only by ambient env vars is easy to define by accident and has no `env list`/`env info` visibility.

## environments.toml schema

The file uses one top-level TOML table per environment name. Its shape is entirely up to the application — cli-engine's config *system* imposes no fixed keys. A handful of keys are given special meaning, but only by specific cli-engine *features*, and only when the application actually uses that feature:

| Key | Meaning | Used by |
| --- | --- | --- |
| `min_stage` | Overrides the visible feature-flag floor for this environment | The engine itself, whenever `CliConfig::with_environments` is wired — see [Feature-Flag Layering](#feature-flag-layering) |
| `feature_overrides` | Per-flag-key stage overrides, as a nested `[<env>.feature_overrides]` table | Same |
| `client_id` | OAuth client id | `PkceAuthProvider`, only if wired via `with_environments` — see [Per-Environment OAuth via PkceAuthProvider](#per-environment-oauth-via-pkceauthprovider) |
| `auth_url` | OAuth authorization endpoint | Same |
| `token_url` | OAuth token endpoint | Same |
| `scopes` | Default OAuth scopes, as an array of strings | Same |

Any other key is an ordinary app-defined value. Here's an example `environments.toml` file:

```toml
[test]
client_id = "test-client-id"
auth_url   = "https://api.test/example.com/v2/oauth2/authorize"
token_url  = "https://api.test.example.com/v2/oauth2/token"
scopes     = ["openid", "profile"]
api_url    = "https://api.test.example.com"

[dev]
client_id = "ote-client-id"
auth_url   = "https://api.dev.example.com/v2/oauth2/authorize"
token_url  = "https://api.dev.example.com/v2/oauth2/token"
api_url    = "https://api.dev.example.com"
min_stage = "experimental"

[dev.feature_overrides]
"domain-bulk-transfer" = "beta"
```

## Environment-variable overrides

Environment variables can override environment config fields when you have configured the fields with `env = "SUFFIX"`. The engine will look for a variable of the format `<APP_ID_UPPER>_<SUFFIX>`.

## Active Environment

The active environment controls which environment is targeted when no `--env` flag is passed.

**Precedence** (highest first):

1. `--env <name>` on the command line.
2. The `environment.active` key in the per-application config file (persisted by `env set`).
3. The default set in `Environments::new(default_env)`.

`env set <name>` validates that the environment is defined and then writes `environment.active` to the config file. The next invocation (without `--env`) reads the default from that file.

The built-in commands are:

| Command | Description |
| --- | --- |
| `env list` | Lists all known environments (compiled + file), marking the active one. |
| `env get` | Prints the active environment name. |
| `env set <name>` | Validates and persists `name` as the active environment. |
| `env info` | Prints the active environment's name and its config fields. |

## Feature-Flag Layering

Feature-flag visibility is controlled by the active environment's `min_stage`/`feature_overrides` TOML keys. See [Feature Flags & Stages](concepts.md#feature-flags--stages) for what `Stage` and `FlagPolicy` mean generally.

Precedence, lowest to highest:

```text
consumer .with_min_stage(...) / .with_feature_override(...)  (compiled CliConfig policy)
  > global env var min-stage           (${APP_ID}_MIN_STAGE, see below)
  > active environment's resolved min_stage / feature_overrides TOML keys (compiled + file, file wins)
```

## Per-Environment OAuth via PkceAuthProvider

`PkceAuthProvider::with_environments(Arc<Environments>)` lets an application's OAuth config (`client_id`/`auth_url`/`token_url`/`scopes` — see the schema table above) vary per environment, instead of being fixed once at construction time. There is no environment-variable override for OAuth fields.

Precedence, highest first:

1. The active environment's `client_id`/`auth_url`/`token_url`/`scopes` keys (`environments.toml` wins over a compiled-in value).
2. The base `client_id`/`auth_url`/`token_url`/`scopes` passed to `PkceAuthProvider::new`.

`client_id`/`auth_url`/`token_url` have no default: if neither tier supplies one, resolving OAuth config for that environment fails outright rather than proceeding with a blank value. `scopes` does have a default (an empty list) — an OAuth provider with no default scopes configured is normal, not an error.

By default, scope coverage checks (deciding whether a step-up/re-auth is needed) treat scopes as opaque strings. If the identity provider's scopes nest — a granted `write` scope also covering `read`, for example — declare that with `PkceAuthProvider::with_scope_hierarchy(...)`, passing a `cli_engine::auth::pkce::ScopeHierarchy` (`ScopeHierarchy::new().with_implication("write", &["read"])`), so a broader existing grant satisfies a narrower requirement without an unnecessary browser step-up.

## Example

```rust,no_run
use std::sync::Arc;
use cli_engine::{
    BuildInfo, Cli, CliConfig,
    auth::pkce::PkceAuthProvider,
    environments::{EnvTable, Environments},
};

// Build one Arc<Environments> and share it. `with_app_id` must match the
// CliConfig app_id ("my-cli") so the environments.toml file path resolves.
let environments = Arc::new(
    Environments::new("prod")
        .with_app_id("my-cli")
        .with_environment(
            "prod",
            EnvTable::new()
                .with("client_id", "prod-client-id")
                .with("auth_url", "https://api.example.com/v2/oauth2/authorize")
                .with("token_url", "https://api.example.com/v2/oauth2/token")
                .with("scopes", vec!["openid", "profile"])
                .with("api_url", "https://api.example.com"),
        )
        .with_environment(
            "ote",
            EnvTable::new()
                .with("client_id", "ote-client-id")
                .with("auth_url", "https://api.ote.example.com/v2/oauth2/authorize")
                .with("token_url", "https://api.ote.example.com/v2/oauth2/token")
                .with("api_url", "https://api.ote.example.com"),
        )
        .with_config_file(true),
);

let provider = Arc::new(
    PkceAuthProvider::new(
        "primary",
        "https://api.example.com/v2/oauth2/authorize",
        "https://api.example.com/v2/oauth2/token",
        "fallback-client-id",
        &["openid"],
    )
    .with_environments(Arc::clone(&environments)),
);

let cli = Cli::new(
    CliConfig::new("my-cli", "My CLI", "my-cli")
        .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
        .with_default_auth_provider("primary")
        .with_auth_provider(provider)
        // The same Arc the provider was wired with — not a separate copy — so the
        // file layer and active-env persistence resolve identically for both.
        .with_environments(environments),
);
```

With this setup:

- Running `my-cli env list` prints `ote` and `prod`, marking whichever is active.
- Running `my-cli env set ote` persists `ote` as active; subsequent invocations target OTE.
- Running `my-cli --env prod <command>` overrides the active environment for that invocation only.
- A user-supplied `environments.toml` in the config directory can add new environments or override fields without recompiling.
