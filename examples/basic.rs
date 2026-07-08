use std::process::ExitCode;

use clap::Arg;
use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec,
    RuntimeGroupSpec, Stage,
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

    // Gated behind Stage::Experimental, so it is pruned from help/schema/dispatch under the
    // default policy (min_stage: Stage::Ga). To see it, uncomment the .with_min_stage(...) line
    // below (or add .with_feature_override("project-preview", Stage::Ga) instead).
    let preview = RuntimeCommandSpec::new(
        CommandSpec::new("preview", "Preview an upcoming project feature")
            .with_system("projects-api")
            .with_feature_flag("project-preview", Stage::Experimental)
            .no_auth(true),
        async |_credential, _args| Ok(CommandResult::new(json!({ "status": "coming-soon" }))),
    );

    let project_module = Module::new("Platform Systems", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
            .with_command(list_projects.clone())
            .with_command(preview.clone())
    });

    let cli = Cli::new(
        CliConfig::new("example", "Example cli-engine application", "example")
            .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
            // .with_min_stage(Stage::Experimental)
            .with_module(project_module),
    );

    cli.execute().await
}
