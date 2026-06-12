# cli-engine Concepts

`cli_engine` is a Rust library for building consistent CLI tools. It provides the shared framework
pieces that every command-line application needs: command registration, authentication,
authorization hooks, middleware, audit/activity hooks, structured output, schemas, guides, search,
and transport helpers.

The crate is a library. Consumer CLIs provide their own binary entrypoint and use `cli_engine` to build
the command tree and execution pipeline.

## CLI Applications

A CLI application starts with [`CliConfig`](../src/cli.rs). The config declares the root command,
build metadata, modules, global commands, auth providers, guides, views, and lifecycle hooks.

```rust
use cli_engine::{BuildInfo, Cli, CliConfig};

let cli = Cli::new(
    CliConfig::new("my-cli", "Developer tooling", "my-cli")
        .with_build(BuildInfo::new("1.2.3").with_commit("abc123").with_date("2026-05-19"))
        .with_default_auth_provider("primary"),
);
```

`Cli::new` builds a `clap::Command` tree with owned command metadata, registers global flags, mounts
modules, registers built-in commands, and prepares middleware. `Cli::execute` is the normal binary
entrypoint helper and handles process shutdown signals. Tests can call `Cli::run(args)`,
`Cli::execute_from(args, stdout, stderr)`, or inject a deterministic shutdown future with
`Cli::execute_from_until_signal`.

The builder helpers cover the common path. Direct `CliConfig` struct literals remain available for
tests and uncommon setup where setting several fields at once is clearer.

Small registration data types also have constructors so examples stay readable:

```rust
use cli_engine::{GuideEntry, HumanViewDef, TableColumn};

let guide = GuideEntry::new("deploy", "Deploy workflows", "# Deploy\n");
let view = HumanViewDef::new(
    "project:list",
    vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("status", "Status"),
    ],
);
```

## Command Modules

A command module is a domain-bounded collection of CLI functionality. Modules should map to systems,
products, resource families, or team ownership boundaries. A module can provide:

- A help category.
- Command groups and commands.
- Guides.
- Human views.
- Schema registrations.

Small modules can use `Module::new` with a closure. Larger modules can implement `CommandModule` to
keep dependencies in named Rust types.

```rust
use cli_engine::{CommandModule, GroupSpec, ModuleContext, RuntimeGroupSpec};

#[derive(Debug)]
struct ProjectModule;

impl CommandModule for ProjectModule {
    fn category(&self) -> String {
        "Platform Systems".to_owned()
    }

    fn register(&self, _context: &mut ModuleContext<'_>) -> RuntimeGroupSpec {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
    }
}
```

`ModuleContext` gives registration code access to shared middleware, schema registration, and
human-view registration without exposing the command parser internals.

## Command Groups

Command groups are noun-based containers for commands. They establish scope and keep command trees
easy to scan:

```text
my-cli project list
|   |       |
|   |       leaf command
|   group
root CLI
```

Groups are represented by `GroupSpec` and `RuntimeGroupSpec`. Groups can contain commands and nested
groups. The framework derives colon-separated command paths such as `project:list` for policy,
authorization, audit, and metadata use.

## Commands

Commands are declared with `CommandSpec` and executed with `RuntimeCommandSpec`. The spec contains
metadata and `clap::Arg` definitions; the runtime command pairs the spec with an async handler.

```rust
use clap::Arg;
use cli_engine::{CommandResult, CommandSpec, RuntimeCommandSpec};
use serde_json::json;

let command = RuntimeCommandSpec::new(
    CommandSpec::new("list", "List projects")
        .with_system("projects-api")
        .with_default_fields("id,name,status")
        .with_arg(Arg::new("team").long("team").required(true)),
    async |_credential, args| {
        let team = args["team"].as_str().unwrap_or_default();
        Ok(CommandResult::new(json!([
            {"id": "p1", "name": "Portal", "status": "active", "team": team}
        ])))
    },
);
```

Command definitions should stay close to business logic. Use the builder methods to set optional
metadata:

### Streaming Commands

For commands that emit a sequence of progress events rather than a single result — for example, a
deploy command that streams build logs — use `RuntimeCommandSpec::new_streaming`. The handler
receives a `StreamSender` and writes individual `serde_json::Value` events. Each value is written
to stdout as a newline-delimited JSON (NDJSON) line as it arrives.

```rust
use cli_engine::{CommandSpec, RuntimeCommandSpec, StreamSender, Tier};
use serde_json::json;

RuntimeCommandSpec::new_streaming(
    CommandSpec::new("deploy", "Deploy and stream progress")
        .with_system("deploy-api")
        .with_tier(Tier::Mutate),
    |_ctx, sender: StreamSender| async move {
        sender.send(json!({ "status": "building" })).await;
        sender.send(json!({ "status": "deploying", "progress": 42 })).await;
        sender.send(json!({ "status": "done" })).await;
        Ok(())
    },
)
```

Streaming commands do not go through the normal output pipeline (filtering, field selection, `--output`).
Each event is written verbatim to the process stdout (`tokio::io::stdout()`), bypassing any custom
writer injection from `execute_from` variants. The handler and the NDJSON writer run concurrently
so the handler can keep sending while the writer flushes to stdout. If stdout is under backpressure
the bounded channel can fill and the handler will wait on `send` until the writer catches up.


- `with_long` for expanded help.
- `with_alias` for alternate group or command names.
- `hidden(true)` for groups or commands that remain runnable but are omitted from help, tree, and
  search discovery.
- `with_system` for the backend/system id used in output metadata.
- `with_default_fields` for default field projection.
- `with_auth_provider` and `with_auth_metadata` for auth selection.
- `with_tier` and `mutates` for risk and dry-run behavior.
- `with_json_schema::<T>()` for output schema registration.
- `with_arg` for command-specific `clap::Arg` values, including options and positionals.
- `with_flag` as a convenience alias when the argument is specifically a flag or option.

`RuntimeCommandSpec::new` is the common handler shape:

```rust
async fn handler(
    credential: cli_engine::CredentialResolver,
    args: cli_engine::middleware::ValueMap,
) -> cli_engine::Result<cli_engine::CommandResult> {
    // Auth is fail-closed by default: the engine resolves the credential before
    // this handler runs, so `credential.resolve().await?` here is a memoized
    // lookup. Mark the command `.auth_optional()` or `.no_auth(true)` to opt out.
    Ok(cli_engine::CommandResult::new(serde_json::json!({ "ok": true })))
}
```

Use `RuntimeCommandSpec::new_with_context` when a handler needs command path, user-supplied args, or
middleware context.

### Typed Arguments

Commands can also define arguments with `#[derive(clap::Args)]` structs instead of manual `Arg`
builders. This gives compile-time type safety from argument definition through handler consumption:

```rust
use cli_engine::{CommandResult, CommandSpec, CredentialResolver, RuntimeCommandSpec};
use serde_json::json;

#[derive(Debug, Clone, clap::Args)]
struct ListArgs {
    #[arg(long)]
    team: String,

    #[arg(long, default_value = "10")]
    limit: u32,
}

let command = RuntimeCommandSpec::new_typed::<ListArgs, _, _, _>(
    CommandSpec::from_args::<ListArgs>("list", "List projects")
        .with_system("projects-api")
        .with_default_fields("id,name,status"),
    async |_credential: CredentialResolver, args: ListArgs| {
        Ok(CommandResult::new(json!([
            {"id": "p1", "name": "Portal", "team": args.team}
        ])))
    },
);
```

`CommandSpec::from_args::<T>()` extracts argument definitions from the derive type.
`RuntimeCommandSpec::new_typed` deserializes the raw matches into the typed struct before calling
the handler. Both approaches produce equivalent runtime commands and can be mixed freely within a
module.

## Built-In Commands

The framework registers built-in commands for common CLI behavior:

| Command | Registered when | Purpose |
| --- | --- | --- |
| `help` | Always | Displays usage for root, groups, and commands. |
| `tree` | Always | Displays the full command hierarchy. |
| `auth login` / `auth status` / `auth logout` | Auth providers are registered or a default provider is configured | Manages credentials. |
| `guide [topic]` | Guides are registered | Lists and displays embedded guides. |

`guide` accepts zero or one topic. Additional positional arguments are rejected before guide content
is rendered.

Application `pre_run` hooks run for executable commands, including bare command groups that render
group help, `help`, `tree`, `guide`, and auth commands. `init_deps` is narrower: it initializes
runtime dependencies for real command execution and auth provider loading, but search/schema
discovery and help-style built-ins should remain cheap and side-effect-light.

`help` walks as far as it can through the command tree, then shows that command's help. Unknown
root-level targets still report an unknown command.

## Multi-Call Dispatch (argv0)

A binary can behave differently based on the name it was invoked as (busybox/git style), via
[`CliConfig::with_argv0_alias`](../src/cli.rs) and
[`CliConfig::with_argv0_personality`](../src/cli.rs). This is opt-in and falls through to the default
CLI for any unregistered name. See [argv0 Dispatch](argv0-dispatch.md).

## Flags

Commands define their own flags with `clap::Arg`. The framework also registers global flags that
populate middleware:

| Flag | Middleware field | Default | Purpose |
| --- | --- | --- | --- |
| `--output`, `-o` | `output_format` | `json` | Output format: `json`, `human`, or `toon`. |
| `--json` | `output_format` | — | Shorthand for `--output json`. |
| `--toon` | `output_format` | — | Shorthand for `--output toon`. |
| `--human` | `output_format` | — | Shorthand for `--output human`. |
| `--verbose` | `verbose` | empty | Includes metadata; no value means `all`. |
| `--dry-run` | `dry_run` | `false` | Short-circuits mutating/destructive commands. |
| `--fields` | `fields` | empty | Selects comma-separated output fields. |
| `--filter` | `filter` | empty | JMESPath predicate evaluated against each list item. |
| `--expr` | `expr` | empty | JMESPath query evaluated against the whole result. |
| `--limit` | `limit` | `0` | Client-side page size for list output. |
| `--offset` | `offset` | `0` | Client-side starting offset for list output. |
| `--schema` | `schema` | `false` | Renders command schema instead of running business logic. |
| `--reason` | `reason` | empty | Reason passed to authorization. |
| `--timeout` | `timeout` | `0s` | Command deadline (e.g. `60s`, `5m`); default `0s` = no timeout. |
| `--debug` | `debug` | empty | Enables debug behavior for integrations that use it. |
| `--search` | `search` | empty | Searches command and guide documentation before command execution. |

Applications can add additional global flags through `CliConfig::register_flags` and copy parsed
values into middleware through `CliConfig::apply_flags`.

## Middleware

Command execution flows through a consistent middleware chain:

1. Resolve command metadata.
2. Resolve credentials unless the command is no-auth.
3. Run authorization if an authorizer is configured.
4. Short-circuit `--schema` or mutating `--dry-run` when applicable.
5. Run command business logic.
6. Audit and emit activity.
7. Apply the output pipeline and render success or error output.

This keeps command handlers focused on business behavior while cross-cutting concerns remain
consistent across commands.

## Metadata

Metadata controls authentication, authorization, output, audit, and activity behavior.

Command metadata includes:

- `system`: backend/system id.
- `auth_provider`: credential provider name.
- `auth_metadata`: provider-specific key/value data.
- `tier`: risk classification.
- `mutates`: dry-run prompt behavior.
- `default_fields`: default field projection.

Applications can attach `CliConfig::meta_resolver` to adjust metadata globally after command
metadata is built and before authentication, authorization, dry-run, audit, and activity run. This is useful
for central policy defaults, provider routing, or command-family metadata rules that should not be
duplicated in every command declaration.

Command paths use colon-separated names such as `project:list`. Those paths are used by policy,
authorization, audit, schemas, search, and tree output.

## Authentication

Auth providers implement the `AuthProvider` trait. Providers expose credential retrieval, login,
status, logout, and environment-listing behavior. The framework includes:

- `ExecProvider`, which invokes an external provider command using JSON stdin/stdout.
- `PkceAuthProvider` (requires the `pkce-auth` feature), a built-in browser-based OAuth 2.0 PKCE flow that manages the local callback server, opens the system browser, and persists tokens through a pluggable credential-storage backend (system keychain by default). Auth URL, token URL, and client ID can be overridden via environment variables at runtime.
- A `Dispatcher` that routes auth calls by provider name. Single-provider facades created from the
  dispatcher remain live views of the dispatcher, so transport injectors observe later provider
  registration or replacement.

Command handlers receive `Option<Credential>`. No-auth commands receive `None`.

Provider process contracts and request injectors are described in
[Authentication and Transport](auth.md).

## Credential Storage

Auth providers persist credentials through the injectable `CredentialStorage` trait (`auth::storage`), keyed by `CredentialKey { app_id, provider, env }`. Three built-in backends map to the `CredentialStore` modes:

| Mode | Backend | Behavior |
| --- | --- | --- |
| `keyring` (default) | `KeyringStorage` | System keychain only; failure is a hard error. |
| `auto` | `AutoStorage` | Keychain, with a transparent unencrypted-file fallback when the keychain backend is unavailable. |
| `file` | `FileStorage` | Never contacts the keychain; stores unencrypted JSON under the config base directory. |

`File` is the escape hatch for environments where the system keychain is unavailable or impractical (headless Linux, WSL). The selected mode is resolved with the precedence:

```text
PkceAuthProvider::with_storage / with_credential_store   (explicit, highest)
  > --credential-store flag
  > ${PREFIX}_CREDENTIAL_STORE env var
  > [credentials].store in config.toml
  > keyring (default)
```

where `${PREFIX}` is the app id uppercased with non-alphanumerics replaced by `_` (e.g. `godaddy` → `GODADDY_CREDENTIAL_STORE`). Providers resolve their backend lazily, so `--schema` and `--dry-run` build no storage and never touch the keychain. A custom backend (for example an in-memory store in tests, or a remote secret manager) can be injected with `PkceAuthProvider::with_storage`.

## Configuration File

cli-engine provides a single per-application TOML config file that **consumer CLIs share with the engine**. It lives at `<config-base>/<app_id>/config.toml`, where `<config-base>` is `$XDG_CONFIG_HOME`, `$HOME/.config`, or `%APPDATA%`. Loading is best-effort: a missing/unreadable/malformed file yields an empty config (a warning is logged for malformed) rather than failing the
command.

Engine-reserved settings live in documented top-level tables (today just `[credentials]`); the consumer CLI owns **every other top-level table**:

```toml
[credentials]        # engine-reserved
store = "file"       # "auto" | "keyring" | "file"

[deploy]             # consumer-owned
region = "us-west"
```

### Reading config

The loaded file is exposed as a `ConfigFile` and surfaced everywhere it's useful — `toml` stays an internal detail, so access is typed:

- In command handlers: `ctx.config().section::<DeployConfig>("deploy")?`
- In module registration: `module_ctx.config().section::<T>(...)`
- Engine-reserved view: `ConfigFile::engine() -> EngineConfig`
- Whole-file into a consumer root type: `ConfigFile::deserialize::<T>()`

The file is loaded once at startup and cloned into each run's middleware, so reads are cheap.

### Writing config (`config` command group)

`CliConfig::with_config_commands()` mounts a built-in `config` group (filed under the admin help category), opt-in so it never collides with a consumer's own `config` noun:

```text
mycli config path                          # print the file path
mycli config get deploy.region             # read a dotted key
mycli config set deploy.region us-east     # set + save (mutating; --dry-run aware)
mycli config list                          # print the whole file
```

`config set` is dry-run aware, parses the value as a bool/int/float when it looks like one (else a string), preserves existing comments and formatting (backed by `toml_edit`), and validates the engine-reserved `credentials.store` key. Programmatically, `ConfigFile::set` + `ConfigFile::save` do the same.

The `config` module also exposes `load`, `resolve_credential_store`, and the pure `resolve_credential_store_with` for testing credential-store precedence without touching process state.

## Authorization

Authorization is provided by an `Authorizer` attached to middleware. The authorizer receives:

- Command path.
- Effective args.
- Optional credential.
- Reason from `--reason`.
- Risk tier.

If authorization fails, the middleware renders the error and still runs the audit/activity error
path.

## Environments

Environment selection is application-defined. If an application needs an environment flag, it can
register one through `CliConfig::register_flags` and store the parsed value on middleware through
`CliConfig::apply_flags`. Auth providers receive the environment value during credential operations.

## Risk Tiers

Risk tiers classify command impact:

| Tier | Meaning | Dry-run behavior |
| --- | --- | --- |
| `read` | Safe or non-mutating operation | Not short-circuited. |
| `mutate` | Creates or modifies state | Short-circuited by `--dry-run`. |
| `destructive` | Irreversibly removes or compromises state | Short-circuited by `--dry-run`. |

`CommandSpec::mutates(true)` also marks a command as dry-run promptable.

## Output

Handlers return JSON-serializable data and a system id. Middleware wraps the result in an envelope
with data, metadata, errors, and warnings.

### next_actions

Command handlers can attach a list of follow-on command suggestions to any result using
`CommandResult::with_next_actions`. The framework includes these suggestions in the output
envelope under the `next_actions` key in JSON and TOON output formats. Human output does not
display `next_actions`.

```rust
use cli_engine::{CommandResult, NextAction, NextActionParam};
use serde_json::json;
use std::collections::HashMap;

Ok(CommandResult::new(json!({ "id": "app-1", "name": "my-app" }))
    .with_next_actions(vec![
        NextAction {
            command: "application info --name my-app".to_owned(),
            description: "Get full application details".to_owned(),
            params: HashMap::new(),
        },
    ]))
```

`NextAction` parameters are optional and carry `value`, `enum`, `required`, `default`, and
`description` fields. This is the primary mechanism for agent-first CLIs to tell callers what
command to run next.

The output pipeline runs in this order:

1. **Filtering**: `--filter` evaluates a JMESPath predicate against each item in list data.
2. **Pagination**: `--limit` and `--offset` slice list data and attach pagination metadata.
3. **Expression**: `--expr` evaluates a JMESPath query against the whole current result.
4. **Field selection**: `--fields` selects comma-separated fields and nested dot paths.
5. **Formatting**: `--output` renders `json`, `human`, or `toon`.

Examples:

```bash
my-cli project list --filter "status == 'active'"
my-cli project list --expr "[].name"
my-cli project list --expr "sort_by(@, &createdAt)"
my-cli project list --fields name,status
my-cli project list --output human
```

JSON output is the default. Human output is optimized for terminal reading. Each format has a
shorthand flag: `--json`, `--human`, and `--toon` are equivalent to `--output json`,
`--output human`, and `--output toon` respectively.

Errors are rendered through the same envelope path as successful data. Framework errors are mapped
to process exit codes by category. Callers that need a specific process status can use
`CliCoreError::with_exit_code(code, source)` so the code survives normal error wrapping. Callers
with backend-structured errors can implement `DetailedError` and wrap them with
`CliCoreError::with_detailed_error(source)` before passing them through framework chains; this
preserves error code, system, and request id in the rendered envelope. Command execution wraps
generic business errors with the command's configured system, or the top-level command path when no
system is configured, so error envelopes preserve the same backend attribution as success envelopes.

## Schemas

Commands can publish output schemas for help text and agent comprehension. The preferred schema path
is JSON Schema from Rust types:

```rust
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
struct Project {
    id: String,
    name: String,
    status: String,
    owner: Option<String>,
}

let spec = cli_engine::CommandSpec::new("list", "List projects")
    .with_json_schema::<Project>();
```

`--schema` returns a full JSON Schema document plus a compact field summary. Manual
`OutputSchema`/`OutputField` definitions are also available for simple schemas.

## Human Output

Human output is designed for readable terminal display:

- Custom human renderers win over generic formatting.
- Registered columns win over generic object key sorting.
- Arrays of objects render as tables.
- Objects render as `key: value` lines.
- Mixed object/scalar arrays fall back to line-per-item rendering.
- Objects in fallback lines render as compact JSON.
- JSON numbers use `serde_json` number text.

Column views should be registered by command path or schema id:

```rust
use cli_engine::{HumanViewDef, TableColumn};

let view = HumanViewDef::new(
    "project:list",
    vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("name", "Name"),
        TableColumn::new("status", "Status"),
    ],
);
```

## Guides

Guides are markdown documents registered with the CLI or with modules. They document workflows,
explain command usage, and provide context to users and agents. Applications can embed guides with
their preferred Rust embedding strategy or register static guide values directly.

Use `parse_guides(path)` for guide files on disk. Use `parse_guides_from_markdown` with `(path,
bytes)` pairs for embedded guides from `include_bytes!`, `include_str!`, or a build-generated
manifest. Modules can also call `Module::with_guides_from_markdown` or
`ModuleContext::add_guides_from_markdown`.

## Search

`--search` searches command metadata, aliases, guides, and extra registered search documents. Search
short-circuits normal command execution so users and agents can find help without satisfying command
flags.

## Transport

The transport module provides a `reqwest`-based HTTP client with:

- Auth injection.
- Builder-based default headers, user-agent, and logger support.
- JSON request/response helpers.
- Raw body helpers.
- Multipart helpers.
- ETag and `If-Match` helpers.
- GraphQL helpers.
- Retry behavior.
- Structured error preservation for output envelopes.

Auth injectors include bearer token, provider bearer, cookie, basic auth, API key, client
credentials, and no-op injectors.

## Contributor Model

The intended contributor workflow is:

1. Pick the module owned by your team.
2. Copy a nearby `CommandSpec` and handler.
3. Fill in the command name, help text, flags, risk tier, schema, system id, and handler logic.
4. Add focused tests for command behavior and output shape.

Command code should stay close to business logic. Shared concerns belong in framework traits,
middleware, transport helpers, output schemas, or human views.
