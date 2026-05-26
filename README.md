# cli-engine

`cli-engine` is a Rust library for building consistent, domain-oriented CLI applications. It provides shared framework pieces for command registration, authentication, authorization hooks, middleware, structured output, schemas, guides, search, command tree rendering, and HTTP transport.

Consumer CLIs bring their own binary entrypoint and domain modules. `cli-engine` owns the common
execution pipeline so teams can add commands by copying a nearby command and filling in the
domain-specific details.

## Quick Start

```rust
use std::process::ExitCode;

use clap::Arg;
use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec,
    RuntimeGroupSpec,
};
use serde_json::json;

#[tokio::main]
async fn main() -> ExitCode {
    let list = RuntimeCommandSpec::new(
        CommandSpec::new("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name,status")
            .with_arg(Arg::new("team").long("team").required(true))
            .no_auth(true),
        async |_credential, args| {
            let team = args
                .get("team")
                .and_then(|value| value.as_str())
                .unwrap_or_default();

            Ok(CommandResult::new(json!([
                { "id": "project-1", "name": "Portal", "status": "active", "team": team }
            ])))
        },
    );

    let module = Module::new("Platform Systems", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
            .with_command(list.clone())
    });

    let cli = Cli::new(
        CliConfig::new("example", "Example cli-engine application", "example")
            .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
            .with_module(module),
    );

    cli.execute().await
}
```

Command paths are colon-separated, such as `project:list`, for policy, schema, audit, activity, and
authorization metadata.

## Guide Embedding

Guide markdown can be read from disk during development with `parse_guides("guides")`, or embedded
in the consumer CLI binary with standard Rust compile-time embedding:

```rust
use cli_engine::Module;

let module = Module::new("Platform Systems", register).with_guides_from_markdown([
    ("guides/deploy.md", include_bytes!("../guides/deploy.md").as_slice()),
    ("guides/operate.md", include_bytes!("../guides/operate.md").as_slice()),
]);
```

For large guide directories, applications can use an embedding crate or a `build.rs` generated
manifest that produces the same `(path, bytes)` pairs.

## Documentation

- [Concepts](docs/concepts.md)
- [Design](docs/design.md)
- [Authentication and transport](docs/auth.md)
- [Basic example](examples/basic.rs)
- [Agent instructions](AGENTS.md)

## Cargo Features

| Feature | Description |
| --- | --- |
| `pkce-auth` | Enables `auth::pkce::PkceAuthProvider`, a built-in browser-based OAuth 2.0 PKCE flow with system keychain storage via the `keyring` crate. Adds optional dependencies: `keyring`, `open`, `rand`, `sha2`, `url`. |

## Verification

```sh
cargo fmt --all --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps
cargo test --doc
```
