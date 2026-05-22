use std::process::ExitCode;

use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, Credential, GroupSpec, Module,
    RuntimeCommandSpec, RuntimeGroupSpec,
};
use serde_json::json;

#[derive(Debug, Clone, clap::Args)]
struct ListArgs {
    /// Team name to filter projects by.
    #[arg(long, value_name = "TEAM")]
    team: String,

    /// Maximum number of results to return.
    #[arg(long, default_value = "10")]
    limit: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    let list_projects = RuntimeCommandSpec::new_typed::<ListArgs, _, _, _>(
        CommandSpec::from_args::<ListArgs>("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name,status")
            .no_auth(true),
        async |_credential: Option<Credential>, args: ListArgs| {
            Ok(CommandResult::new(json!([
                {
                    "id": "project-1",
                    "name": "Portal",
                    "status": "active",
                    "team": args.team,
                    "limit": args.limit,
                }
            ])))
        },
    );

    let project_module = Module::new("Platform Systems", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
            .with_command(list_projects.clone())
    });

    let cli = Cli::new(
        CliConfig::new(
            "typed-example",
            "Example using typed arguments",
            "typed-example",
        )
        .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
        .with_module(project_module),
    );

    cli.execute().await
}
