# Environments

`cli_engine` provides a first-class environment system that lets CLIs target named deployment environments — `prod`, `ote`, `dev`, and any others the application defines — without consumers having to wire flags, config lookups, or OAuth overrides by hand.

When `CliConfig::with_environments` is called, the engine:

- Registers a global `--env` flag on every command.
- Seeds the active environment into middleware at startup.
- Exposes the resolved environment to handlers via `CommandContext::environment`.
- Mounts the built-in `env list / get / set / info` commands under the admin help category.

## Resolution Layers

`Environments::resolve(name)` builds a fully-merged `Environment` by applying layers in order.
Later layers win over earlier ones.

0. **`Environments::with_init(fn(&str) -> EnvironmentDef)`**: callback for creating a scaffolded environment structure. This is where you'll define field validators/defaults.
1. **Compiled-in defaults** — `EnvironmentDef` values registered with `Environments::with_environment` in the application source code.
2. **`environments.toml`** — the file at `<config-dir>/<app-id>/environments.toml`, when enabled with `Environments::with_config_file(true)`.
3. **Environment-variable overrides** — `<ENV>_OAUTH_CLIENT_ID`, `<ENV>_OAUTH_AUTH_URL`, `<ENV>_OAUTH_TOKEN_URL`, and `<ENV>_<FIELD>` for custom config fields.

A name unknown to layers 1–2 can still resolve via `Environments::with_fallback(fn(&str) -> Option<EnvironmentDef>)`, consulted only when both are silent; `with_init`'s scaffold still applies to a fallback-defined name too.

## environments.toml Schema

The file uses one top-level TOML table per environment name:

```toml
[prod]
client_id = "prod-client-id"
auth_url   = "https://api.example.com/v2/oauth2/authorize"
token_url  = "https://api.example.com/v2/oauth2/token"
scopes     = ["openid", "profile"]
api_url    = "https://api.example.com"

[ote]
client_id = "ote-client-id"
auth_url   = "https://api.ote.example.com/v2/oauth2/authorize"
token_url  = "https://api.ote.example.com/v2/oauth2/token"
api_url    = "https://api.ote.example.com"

[dev]
min_stage = "experimental"

[dev.feature_overrides]
"domain-bulk-transfer" = "beta"
```

The recognized OAuth keys — `client_id`, `auth_url`, `token_url`, and `scopes` (an array of strings) — are parsed into the typed `OAuthConfig` slice of the resolved `Environment`.
`min_stage` and the `[<env>.feature_overrides]` table are the feature-flag layer, described in [Feature-Flag Layering](#feature-flag-layering) below. `feature_overrides` (rather than a shorter name like `features`) is deliberately specific: `EnvironmentDef`'s unrecognized keys fall through to the free-form `extra` bag, so a generic name would collide with any existing app-specific `extra` key of the same name.
Every other key is captured as a free-form field in `Environment::extra`, which is a `BTreeMap<String, String>` — so these values **must be TOML strings** (for example `api_url` above).
A non-OAuth key whose value is a number, boolean, or array fails to parse; quote it as a string instead.
The `extra` bag is printed verbatim by `env info`, so it must not hold secrets.

## Environment-Variable Overrides

The prefix is the environment name uppercased with `-` replaced by `_` (`ote` → `OTE`, `prod-us` → `PROD_US`).
Names that differ only by `-` vs `_` map to the same prefix and will collide; avoid such names.

The three OAuth fields are always overridable (subject to a registered validator, see below):

| Variable | Field overridden |
| --- | --- |
| `<ENV>_OAUTH_CLIENT_ID` | `oauth.client_id` |
| `<ENV>_OAUTH_AUTH_URL` | `oauth.auth_url` |
| `<ENV>_OAUTH_TOKEN_URL` | `oauth.token_url` |

CLI-specific config bag keys are overridable via `<ENV>_<KEY>` as well.

## Per-Field Validation and Computed Defaults

`EnvironmentDef::with_field_validator(key, fn(&str) -> bool)` rejects attempted config field overrides when the predicate returns `false`: it logs a warning and keeps the pre-override value rather than failing resolution outright (unlike a malformed `<ENV>_MIN_STAGE`/`<ENV>_FEATURE_<KEY>`, which has no validator concept and does fail resolution — see below).

`EnvironmentDef::with_field_default(key, fn(&Environment) -> String)` computes a value for `key` at the end of resolution, but only when every layer left it absent or blank. Reach for this specifically when `key`'s fallback depends on *another* field that a later layer could still override

`key` may be any config key, built-in (like the OAuth `"client_id"`/`"auth_url"`/`"token_url"`) or custom.

## Feature-Flag Layering

Feature-flag visibility is a fourth resolution axis, parallel to but independent from the OAuth/bag-key layers above. See [Feature Flags & Stages](concepts.md#feature-flags--stages) for what `Stage` and `FlagPolicy` mean; this section covers only the environment-specific plumbing.

`EnvironmentDef` carries `min_stage: Option<Stage>` and `feature_overrides: BTreeMap<String, Stage>`, set with `.with_min_stage(stage)` and `.with_feature_override(key, stage)`. The resolved `Environment` mirrors both fields. They merge through the same three layers as OAuth/bag fields — compiled defaults, then `environments.toml`, then environment variables — and are then layered onto the consumer's own `CliConfig`-level policy.

In `environments.toml`, `min_stage` is a plain key on the environment's table, and per-key overrides go in a nested `[<env>.feature_overrides]` table:

```toml
[dev]
min_stage = "experimental"

[staging.feature_overrides]
"domain-bulk-transfer" = "ga"
```

An override must itself meet the active `min_stage` floor to reveal a node — overriding a command's stage to `beta` while `min_stage` stays `ga` still hides it (visibility requires `effective_stage >= min_stage`, and `beta < ga`); override to `ga` to reveal it regardless of the node's own declared stage. That is why the `staging` example above forces `domain-bulk-transfer` to `ga`: `staging` sets no `min_stage`, so its floor is the `Ga` default, and only a `ga` override clears it — one surgical unlock of that command while every other still-gated node stays hidden.

Environment-variable overrides:

| Variable | Field overridden |
| --- | --- |
| `<ENV>_MIN_STAGE` | `min_stage` |
| `<ENV>_FEATURE_<KEY>` | `feature_overrides[<key>]` |

`<ENV>_FEATURE_<KEY>` follows the same restriction as bag keys: it only takes effect when `<key>` is already present in `feature_overrides` after the compiled+file merge (layers 1 and 2). `<KEY>` is the flag key uppercased with `-` replaced by `_` (`domain-bulk-transfer` → `PROD_FEATURE_DOMAIN_BULK_TRANSFER`).

The full precedence order, highest wins:

```text
env var for a specific key            (<ENV>_FEATURE_<KEY>)
  > env var min-stage                 (<ENV>_MIN_STAGE)
  > environment file's feature_overrides for that key
  > environment file's min_stage
  > global env var min-stage          (${APP_ID}_MIN_STAGE, see below)
  > consumer .with_feature_override(...)
  > consumer .with_min_stage(...)     (default Stage::Ga)
```

`${APP_ID}_MIN_STAGE` (see `min_stage_env_var` in the `flags` module) is a separate, app-wide override independent of the environments system — it works with or without `CliConfig::with_environments` configured. `Cli::new` folds it into the base `FlagPolicy` before the environment layer above, so a configured environment can still loosen or tighten beyond it. A value that fails to parse as a `Stage` is ignored (with a warning logged), the same as a malformed `<ENV>_MIN_STAGE` or config.toml.

The environment layer is applied only when environment resolution succeeds: if resolving the active environment errors — a malformed `<ENV>_MIN_STAGE` value, an unparsable `environments.toml`, or an active-environment name unknown to every layer — the engine silently falls back to the layers below it (the global `${APP_ID}_MIN_STAGE` override, if set, then the consumer-level `CliConfig` policy) rather than failing the run. In the intended usage pattern (the consumer ships a `Ga` default and an environment *loosens* it for dev/experimental builds) this fails **closed**: a resolution error simply leaves the stricter compiled default in force. But the reverse pattern is unsafe: if a consumer ships a *permissive* compiled `min_stage` (or feature override) and relies on an environment to *tighten* it for a public/production build, a resolution error fails **open** — the gated nodes the environment would have re-hidden are mounted under the permissive compiled policy instead. A security model that depends on an environment tightening a permissive compiled default must therefore validate that environment resolution succeeds (for example via `env info`, or by not caching a build whose active environment cannot resolve) rather than assuming resolution errors cannot occur in practice; do not rely on this crate to fail closed for that direction.

## Active Environment

The active environment controls which environment is targeted when no `--env` flag is passed.

**Precedence** (highest first):

1. `--env <name>` on the command line.
2. The `environment.active` key in the per-application config file (persisted by `env set`).
3. The default set in `Environments::new(default_env)`.

`env set <name>` validates that the environment is defined — by a compiled default, `environments.toml`, or a consumer-registered `Environments::with_fallback` — and then writes `environment.active` to the config file.
An `<ENV>_*` env var alone cannot define a new, selectable environment unless the consumer registered a `with_fallback` that recognizes it (see [Resolution Layers](#resolution-layers)); absent that, a name known only through env vars is rejected.
The next invocation (without `--env`) picks it up from layer 2.

The built-in commands are:

| Command | Description |
| --- | --- |
| `env list` | Lists all known environments (compiled + file), marking the active one. |
| `env get` | Prints the active environment name. |
| `env set <name>` | Validates and persists `name` as the active environment. |
| `env info` | Prints the fully resolved active environment including OAuth and extra fields. |

`env set` is marked `Tier::Mutate` so `--dry-run` short-circuits the config-file write.

## Per-Environment OAuth via PkceAuthProvider

`PkceAuthProvider::with_environments(Arc<Environments>)` wires the provider to the same `Environments` instance, making it the single source of truth for per-environment OAuth config.
Build one `Arc<Environments>` — with `Environments::with_app_id(<same app_id as CliConfig>)` set on it — and share that same `Arc` between the provider and `CliConfig::with_environments`.
If the two receive different instances, a file-defined environment (or a file override of a compiled environment's `client_id`) is visible to `env info` yet invisible to the actual OAuth login.

When the provider resolves a credential for `env`, it calls `Environments::resolve(env)` and uses the resulting `OAuthConfig`.
Each field falls through to the next source when empty:

1. The resolved environment's field (when non-empty).
2. The legacy provider-prefixed env var (`<PROVIDER_PREFIX>_OAUTH_CLIENT_ID`, `_AUTH_URL`, `_TOKEN_URL`), where `<PROVIDER_PREFIX>` is the provider name uppercased with `-` → `_`.
3. The base configuration supplied to `PkceAuthProvider::new`.

Scopes follow the same pattern: the resolved environment's scopes when non-empty, otherwise the provider's base scopes.

If `Environments::resolve` fails — because the name is unknown or the environments file cannot be parsed — the provider logs the error at `DEBUG` level and falls back to the next source.
No token or secret is included in the log; only the environment name and error message appear.

## Example

```rust,no_run
use std::sync::Arc;
use cli_engine::{
    BuildInfo, Cli, CliConfig,
    auth::pkce::PkceAuthProvider,
    environments::{EnvironmentDef, Environments},
};

// Build one Arc<Environments> and share it. `with_app_id` must match the
// CliConfig app_id ("my-cli") so the environments.toml file path resolves.
let environments = Arc::new(
    Environments::new("prod")
        .with_app_id("my-cli")
        .with_environment(
            "prod",
            EnvironmentDef::new()
                .with_client_id("prod-client-id")
                .with_auth_url("https://api.example.com/v2/oauth2/authorize")
                .with_token_url("https://api.example.com/v2/oauth2/token")
                .with_scopes(&["openid", "profile"])
                .with_field("api_url", "https://api.example.com"),
        )
        .with_environment(
            "ote",
            EnvironmentDef::new()
                .with_client_id("ote-client-id")
                .with_auth_url("https://api.ote.example.com/v2/oauth2/authorize")
                .with_token_url("https://api.ote.example.com/v2/oauth2/token")
                .with_field("api_url", "https://api.ote.example.com"),
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
- `PROD_OAUTH_CLIENT_ID=override my-cli --env prod auth login` injects the override at the env-var layer.
- A user-supplied `environments.toml` in the config directory can add new environments or override fields without recompiling.
