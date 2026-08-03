//! Demonstrates busybox/git-style `argv[0]` dispatch.
//!
//! Build it and exercise the routes without a real symlink via the hidden
//! `argv0` command:
//!
//! ```sh
//! cargo run --example argv0 -- argv0 pl --team platform     # alias -> project list
//! cargo run --example argv0 -- argv0 legacy ping            # separate personality
//! ```
//!
//! In production, symlink (Unix) or add a `.cmd` shim (Windows) named `pl` or
//! `legacy` next to the binary; see `docs/argv0-dispatch.md`.

use std::process::ExitCode;

use clap::Arg;
use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec,
    RuntimeGroupSpec,
};
use serde_json::json;

fn platform_module() -> Module {
    let list_projects = RuntimeCommandSpec::new(
        CommandSpec::new("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name")
            .with_arg(Arg::new("team").long("team").required(true))
            .no_auth(true),
        async |_credential, args| {
            let team = args
                .get("team")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned();
            Ok(CommandResult::new(json!([
                { "id": "p1", "name": format!("{team}-api") }
            ])))
        },
    );

    Module::new("Platform Systems", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
            .with_command(list_projects.clone())
    })
}

/// An entirely separate CLI personality, built lazily only when dispatched.
fn legacy_personality() -> CliConfig {
    CliConfig::new("legacy", "Legacy compatibility shim", "legacy")
        .with_build(BuildInfo::new("9.9.9"))
        .with_command(RuntimeCommandSpec::new(
            CommandSpec::new("ping", "Health check").no_auth(true),
            async |_credential, _args| Ok(CommandResult::new(json!({ "pong": true }))),
        ))
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::new(
        CliConfig::new("something", "Example multi-call CLI", "something")
            .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
            .with_module(platform_module())
            // Invoked as `pl`, behave like `project list`.
            .with_argv0_alias("pl", ["project", "list"])
            // Invoked as `legacy`, run a different application entirely.
            .with_argv0_personality("legacy", legacy_personality),
    );

    cli.execute().await
}
