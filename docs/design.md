# cli-engine Rust Design

`cli_engine` is a Rust library for building consistent, domain-oriented command-line applications.
It is not a binary and does not assume one product's command set. Consumer CLIs provide their own
`main`, register domain modules, and let the framework handle shared CLI concerns.

The design priorities are:

- Make new commands easy to add by copying a nearby command and filling in command-specific details.
- Keep domain behavior close to the command that owns it.
- Centralize cross-cutting behavior: authentication, authorization, audit, activity, output rendering,
  schemas, guides, search, command trees, and transport helpers.
- Preserve stable user-facing contracts such as command names, flag names, output envelopes, auth
  provider JSON shapes, and colon-separated command paths.
- Follow normal Rust library and CLI practices: `clap` for argument parsing, `tokio` for async work,
  `serde` for data, `schemars` for JSON Schema, `thiserror` for framework errors, and `reqwest`
  for HTTP transport.

## Crate Shape

The repository root is the Rust crate:

```text
Cargo.toml
AGENTS.md
CLAUDE.md
docs/
  auth.md
  concepts.md
  design.md
examples/
  basic.rs
  typed.rs
src/
  lib.rs
  cli.rs
  command.rs
  module.rs
  middleware.rs
  flags.rs
  guide.rs
  search.rs
  tree.rs
  tier.rs
  error.rs
  auth/
  output/
  transport/
tests/
  foundation.rs
  derive_bridge.rs
```

The root module re-exports the common authoring surface so consumer modules can usually import from
`cli_engine::{...}` without knowing the internal file layout.

## Consumer Application Model

A consumer CLI should keep its binary entrypoint small:

```rust
use std::process::ExitCode;

use cli_engine::{BuildInfo, Cli, CliConfig};

mod modules;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
            .with_default_auth_provider("primary")
            .with_modules(modules::all()),
    );

    cli.execute().await
}
```

Application code should be organized by domain or team ownership:

```text
src/
  main.rs
  modules/
    mod.rs
    project.rs
    certificate.rs
```

Each module owns its command group, leaf commands, response types, output schemas, human views, and
module-local guides.

## CLI Assembly

`CliConfig` is the declarative root configuration. It contains:

- Root command name, short help, and optional long help.
- Build/version metadata.
- Application id.
- Default auth provider.
- Domain modules.
- Top-level commands.
- Guides and human output views.
- Auth providers.
- Lifecycle hooks for dependency initialization, custom global flags, pre-run behavior, metadata
  resolution, shutdown, and extra search documents.

`Cli::new(config)` builds the `clap::Command` tree, registers framework global flags, mounts domain
modules, registers built-in commands, seeds schema and human-view registries, and prepares
middleware.

`Cli::execute()` is the normal binary entrypoint helper. Tests and generated integration harnesses
should prefer `Cli::run(args)` or `Cli::execute_from(args, stdout, stderr)` so stdout, stderr, and
exit status are asserted separately.

## Modules

Modules are domain-bounded collections of CLI functionality. Small modules can use a closure:

```rust
use cli_engine::{GroupSpec, Module, RuntimeGroupSpec};

pub fn module() -> Module {
    Module::new("Platform Systems", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
    })
}
```

Larger modules can implement `CommandModule` when named dependency ownership is clearer than a
closure. Both forms register through `ModuleContext`, which exposes middleware, schema registration,
human-view registration, and guide registration without exposing parser internals.

## Commands And Groups

Groups are noun-based containers. Commands are leaf actions. The framework derives colon-separated
paths from the command tree:

```text
my-cli project list  ->  project:list
```

Those colon paths are stable identifiers for policy, authorization, audit, activity, schemas,
search, and tree output.

Command definitions use `CommandSpec`; executable commands use `RuntimeCommandSpec`:

```rust
use clap::Arg;
use cli_engine::{CommandResult, CommandSpec, RuntimeCommandSpec};
use serde_json::json;

fn list_projects() -> RuntimeCommandSpec {
    RuntimeCommandSpec::new(
        CommandSpec::new("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name,status")
            .with_arg(Arg::new("team").long("team").required(true)),
        async |_credential, args| {
            let team = args
                .get("team")
                .and_then(|value| value.as_str())
                .unwrap_or_default();

            Ok(CommandResult::new(json!([{ "id": "project-1", "team": team }])))
        },
    )
}
```

Use `RuntimeCommandSpec::new_with_context` only when a handler needs the colon command path,
user-supplied args, or a middleware snapshot.

### Typed Arguments

When commands have many flags or already use `#[derive(clap::Args)]` structs, the typed path avoids
manual `Arg` construction and `ValueMap` extraction:

```rust
use cli_engine::{CommandResult, CommandSpec, Credential, RuntimeCommandSpec};
use serde_json::json;

#[derive(Debug, Clone, clap::Args)]
struct ListArgs {
    #[arg(long)]
    team: String,

    #[arg(long, default_value = "10")]
    limit: u32,
}

fn list_projects() -> RuntimeCommandSpec {
    RuntimeCommandSpec::new_typed::<ListArgs, _, _, _>(
        CommandSpec::from_args::<ListArgs>("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name,status"),
        async |_credential: Option<Credential>, args: ListArgs| {
            Ok(CommandResult::new(json!([
                {"id": "p1", "name": "Portal", "team": args.team, "limit": args.limit}
            ])))
        },
    )
}
```

`CommandSpec::from_args::<T>()` calls `T::augment_args` to extract argument definitions.
`RuntimeCommandSpec::new_typed` deserializes parsed matches into the typed struct. Handlers that use
`RuntimeCommandSpec::new` or `new_with_context` can also call `context.typed_args::<T>()` for
on-demand deserialization.

The builder and derive paths are equivalent at runtime and can be mixed within a module.

Command metadata should be explicit:

- `with_system` sets backend attribution for output and errors.
- `with_default_fields` sets default field projection for list-like output.
- `with_auth_provider` and `with_auth_metadata` select provider behavior.
- `with_tier` and `mutates` mark risk and dry-run behavior.
- `with_json_schema::<T>()` publishes JSON Schema for output.
- `with_arg` adds typed `clap::Arg` values.

## Global Flags

Framework global flags populate middleware and apply consistently to every command:

| Flag | Purpose |
| --- | --- |
| `--output`, `-o` | Output format: `json`, `human`, or `toon`. |
| `--json` | Shorthand for `--output json`. |
| `--toon` | Shorthand for `--output toon`. |
| `--human` | Shorthand for `--output human`. |
| `--verbose` | Includes metadata; no value means all metadata. |
| `--dry-run` | Short-circuits mutating/destructive commands. |
| `--fields` | Selects comma-separated output fields. |
| `--filter` | Runs a JMESPath predicate against each list item. |
| `--expr` | Runs a JMESPath query against the whole result. |
| `--limit` | Client-side page size for list output. |
| `--offset` | Client-side starting offset for list output. |
| `--schema` | Renders command schema instead of running business logic. |
| `--reason` | Reason passed to authorization, audit, and activity. |
| `--timeout` | Command deadline; `0s` disables the deadline. |
| `--debug` | Debug selector for integrations that use it. |
| `--search` | Searches command and guide documentation before command execution. |
| `--version`, `-v` | Prints version/build metadata. |

Applications can add their own global flags with `CliConfig::with_register_flags` and copy parsed
values into middleware with `CliConfig::with_apply_flags`.

## Middleware

Middleware owns the execution pipeline:

1. Resolve command metadata.
2. Resolve credentials unless the command is no-auth.
3. Run authorization if configured.
4. Short-circuit `--schema` or mutating `--dry-run` when applicable.
5. Run command business logic.
6. Audit and emit activity.
7. Apply the output pipeline.
8. Render success or error output.

Command handlers should not print directly. They return data or an error; middleware builds the
output envelope and renderer output. This keeps stdout machine-friendly and stderr reserved for
diagnostics in executable paths.

## Auth And Authorization

Auth providers implement `AuthProvider` and are registered with the CLI or during dependency
initialization. The dispatcher routes credential operations by provider name and supports the
built-in `auth login`, `auth status`, and `auth logout` commands.

Credential fields are serialized as provider-contract JSON and are used by transport injectors,
authorization, audit, and activity.

The provider process contract and transport injectors are described in
[Authentication and Transport](auth.md).

Authorization is optional and supplied by an `Authorizer` attached to middleware. The authorizer
receives command path, effective args, optional credential, reason, and tier.

Auditors and activity emitters are also pluggable traits. They receive enough context to record
success, auth failures, authorization denials, dry-runs, command errors, and command duration.

## Output

Handlers return JSON-serializable data and a system id. Middleware wraps the result in an envelope:

- `data`
- `metadata`
- `error`
- `warnings`

Metadata is omitted unless `--verbose` is requested. Selective metadata is supported with
comma-separated verbose fields.

The output pipeline runs in this order:

1. `--filter`
2. `--limit` and `--offset`
3. `--expr`
4. `--fields`
5. `--output`

JSON is the default and preferred machine-readable format. Human output is designed for terminal
reading. TOON remains an optional output format.

Human views are keyed by schema id or command path:

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

Custom human renderers can be registered when column output is not expressive enough.

## Schemas

Schemas exist for help output and agent comprehension. The preferred schema path is JSON Schema
from Rust types:

```rust
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
struct Project {
    id: String,
    name: String,
    status: String,
}
```

Attach schemas with `CommandSpec::with_json_schema::<Project>()`. The framework also derives a
compact field summary for help text. Manual `OutputSchema` and `OutputField` definitions remain
available for simple or dynamic cases.

## Guides And Search

Guides are markdown documents registered globally or by module. They can come from filesystem paths,
embedded `(path, bytes)` pairs, or explicit `GuideEntry` values.

`--search` indexes command metadata, aliases, guide content, and extra registered search documents.
Search bypasses normal command execution so users and agents can discover commands without
satisfying required command flags.

## Transport

`transport::HttpClient` wraps `reqwest` for command implementations. It provides:

- Auth injection.
- Default headers and user-agent configuration.
- JSON request/response helpers.
- Raw response helpers.
- ETag and `If-Match` helpers.
- Multipart helpers.
- GraphQL helpers.
- Retry behavior.
- Structured transport errors that preserve code, system, and request id in output envelopes.

Auth injectors cover bearer tokens, provider-backed bearer tokens, cookies, basic auth, API keys,
OAuth2 client credentials, and no-op requests.

## Error Model

Framework code returns `cli_engine::Result<T>`. `CliCoreError` is the shared error enum for framework
failures, output failures, transport failures, and wrapped domain errors.

Use:

- `CliCoreError::message` for simple framework messages.
- `CliCoreError::message_for_system` for direct system-attributed messages.
- `CliCoreError::with_system` to wrap a source error with backend attribution.
- `CliCoreError::with_detailed_error` when a source error has structured code/system/request id.
- `CliCoreError::with_exit_code` when a specific process exit code must survive error wrapping.

## Testing Design

The Rust crate uses integration tests in `tests/foundation.rs` to exercise the public
framework surface:

- CLI construction and built-ins.
- Command and group dispatch.
- Middleware sequencing.
- Auth provider routing.
- Output envelopes and renderers.
- Output pipeline behavior.
- Schemas and human views.
- Guides, search, and tree rendering.
- Transport clients and auth injectors.

Consumer CLIs should add their own integration tests around generated command trees. Prefer tests
that assert exit code, stdout, stderr, rendered JSON shape, and important command side effects.

Before handoff, run:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps
cargo test --all-targets
cargo test --doc
```

## Non-Goals

- This crate does not define product-specific commands.
- This crate does not own consumer binary entrypoints.
- This crate does not prescribe one guide-embedding crate.
- This crate does not require exact human table bytes across all implementations; the contract is
  readable, stable terminal output.
