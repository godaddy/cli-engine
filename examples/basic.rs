use std::process::ExitCode;

use clap::Arg;
use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec,
    RuntimeGroupSpec,
};
use serde_json::json;

#[tokio::main]
async fn main() -> ExitCode {
    let list_projects = RuntimeCommandSpec::new(
        CommandSpec::new("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name,status")
            .with_arg(
                Arg::new("team")
                    .long("team")
                    .value_name("TEAM")
                    .required(true),
            )
            .no_auth(true),
        async |_credential, args| {
            let team = args
                .get("team")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned();
            Ok(CommandResult::new(json!([
                {
                    "id": "project-1",
                    "name": "Portal",
                    "status": "active",
                    "team": team,
                }
            ])))
        },
    );

    let project_module = Module::new("Platform Systems", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
            .with_command(list_projects.clone())
    });

    let cli = Cli::new(
        CliConfig::new("example", "Example cli-engine application", "example")
            .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
            .with_module(project_module),
    );

    cli.execute().await
}
