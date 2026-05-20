# Rust cli-engine Agent Instructions

These instructions apply to the Rust `cli_engine` crate in this directory.

## Project Shape

- This repository is a standalone Rust crate for the `cli_engine` library.
- Keep source in `src/`, crate docs in `docs/`, examples in `examples/`, and integration tests in
  `tests/`.
- Do not add implementation code, docs, fixtures, or tests from unrelated implementations to this repository.

## Design Direction

- Preserve the cli-engine concepts: domain modules, noun-based groups, leaf commands, colon-separated command paths, middleware, authentication, authorization, output envelopes, schemas, guides, search, and transport helpers.
- Preserve colon-separated command paths such as `project:list`; policy files and command metadata depend on them.
- Prefer normal Rust CLI and library patterns.
- Use `clap` for command parsing and command help behavior.
- Use `schemars` and JSON Schema as the primary schema path.
- Use JMESPath for output queries and filters.
- JSON is the default machine-readable output. Human output should be readable and stable.

## Public API Expectations

- Optimize for teams and agentic code generation adding new command modules.
- Favor clear builders and constructors for common authoring paths.
- Keep command definitions close to business logic.
- Keep public names idiomatic Rust: snake_case functions and fields, PascalCase types, clear module names.
- Avoid clever abstractions unless they clearly reduce repeated command-author work.
- Public APIs should have useful rustdoc comments. Explain behavior, errors, and invariants where they matter.
- Source comments should explain non-obvious local decisions.

## Creating A Consumer CLI

When creating a new CLI application that uses this crate as a library, prefer this structure:

```text
my-cli/
  Cargo.toml
  src/
    main.rs
    modules/
      mod.rs
      project.rs
```

The consumer `Cargo.toml` should depend on `cli-engine`, `tokio`, `clap`, `serde`, `serde_json`, and `schemars` when command schemas are generated from Rust types:

```toml
[dependencies]
cli-engine = "0.1"
clap = "4"
schemars = { version = "1", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

When working inside this repository or a sibling checkout, a path dependency is fine:

```toml
cli-engine = { path = "../cli-engine" }
```

The binary entrypoint should be small. It should assemble modules, configure global app metadata, and delegate execution to `Cli::execute`:

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

Module aggregation should be boring and explicit:

```rust
use cli_engine::Module;

mod project;

pub fn all() -> Vec<Module> {
    vec![project::module()]
}
```

Each team or domain should own a module file. Keep the group, commands, response types, schemas, human views, and module-local guides together unless the file becomes too large to scan.

## Command Authoring Pattern

New commands should usually follow this shape:

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
        let team = args
            .get("team")
            .and_then(|value| value.as_str())
            .unwrap_or_default();

        Ok(CommandResult::new(json!([{ "id": "project-1", "team": team }])))
    },
);
```

Use `RuntimeCommandSpec::new_with_context` only when the command needs the command path, user-supplied args, or middleware snapshot.

For a full module, prefer this shape:

```rust
use clap::Arg;
use cli_engine::{
    CommandSpec, GroupSpec, HumanViewDef, Module, RuntimeCommandSpec, RuntimeGroupSpec,
    TableColumn,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Serialize, JsonSchema)]
struct Project {
    id: String,
    name: String,
    status: String,
}

pub fn module() -> Module {
    Module::new("Platform Systems", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
            .with_command(list_projects())
    })
    .with_view(HumanViewDef::new(
        "project:list",
        vec![
            TableColumn::new("id", "ID"),
            TableColumn::new("name", "Name"),
            TableColumn::new("status", "Status"),
        ],
    ))
}

fn list_projects() -> RuntimeCommandSpec {
    RuntimeCommandSpec::new(
        CommandSpec::new("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name,status")
            .with_json_schema::<Project>()
            .with_arg(Arg::new("team").long("team").required(true)),
        async |_credential, args| {
            let team = args
                .get("team")
                .and_then(|value| value.as_str())
                .unwrap_or_default();

            Ok(CommandResult::new(json!([
                { "id": "project-1", "name": team, "status": "active" }
            ])))
        },
    )
}
```

Command checklist:

- Name leaf commands with verbs such as `list`, `get`, `create`, `update`, and `delete`.
- Name groups with nouns such as `project`, `domain`, or `certificate`.
- Set `.with_system(...)` for backend attribution.
- Set `.with_default_fields(...)` for list-style output.
- Set `.with_json_schema::<T>()` when the response shape is known.
- Add `clap::Arg` values with the exact user-facing flag names the CLI should expose.
- Use `.no_auth(true)` only for commands that genuinely do not need credentials.
- Use `.with_tier(...)` or `.mutates(true)` for mutating commands so `--dry-run` can short-circuit them.
- Prefer returning structured JSON values from handlers; let cli-engine render JSON, human, and TOON formats.

## Output And Schemas

- Command handlers return JSON-serializable data. Set `.with_system(...)` on the command spec for
  backend attribution.
- Register schemas with `.with_json_schema::<T>()` when a Rust response type exists.
- Use manual `OutputSchema`, `OutputField`, `FieldInfo`, and `SchemaInfo` only when generated JSON Schema is not practical.
- Register human views with `HumanViewDef::new` and `TableColumn::new` when a command needs column-oriented terminal output.
- Keep stdout machine-friendly and stderr human-friendly for executable paths.

Handlers should not print directly. Return data or an error and let the framework render the output envelope.

When a command calls HTTP APIs, prefer `cli_engine::transport::HttpClient` plus an auth injector instead of open-coded `reqwest` setup. Keep request construction typed and pass user-provided paths, ids, and filters as request parameters rather than interpolating shell commands.

## Agent Workflow

For agentic programming tools generating a new CLI or module:

1. Read `docs/concepts.md` and the nearest existing module.
2. Create or update the module file first.
3. Define response structs with `Serialize` and `JsonSchema` for command output.
4. Add command specs and handlers with the builder API.
5. Register human views for list commands.
6. Add integration tests that call `Cli::run(...)` or the consumer binary and assert exit code, stdout shape, stderr shape, and key output fields.
7. Run the verification commands below.

Keep generated code simple enough that a team can copy one command and fill in new details without learning hidden framework patterns.

## Testing

Run these before handoff after Rust changes:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps
cargo test --all-targets
cargo test --doc
```


If public docs were changed, also check public docs coverage:

```sh
cargo rustdoc --lib -- -W missing-docs
```

The expected missing-docs count for the Rust crate is zero.

## Hygiene

- Do not commit `target/` artifacts.
- Avoid raw `println!`, `eprintln!`, `dbg!`, `unwrap()`, `expect()`, `todo!`, or `unimplemented!` in production code.
- Keep generated or temporary files out of commits.
- If a change intentionally preserves externally visible behavior for compatibility, cover it with a focused test.
