use clap::Arg;
use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, HumanViewDef, Module,
    RuntimeCommandSpec, RuntimeGroupSpec, TableColumn, Tier,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize, JsonSchema)]
struct Project {
    id: String,
    name: String,
    status: String,
}

fn platform_module() -> Module {
    Module::new("Platform Systems", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
            .with_command(list_projects())
            .with_command(delete_project())
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
            .with_arg(Arg::new("team").long("team").required(true))
            .no_auth(true),
        async |_credential, args| {
            let team = args.get("team").and_then(Value::as_str).unwrap_or_default();
            Ok(CommandResult::new(json!([
                {"id": "p1", "name": format!("{team}-api"), "status": "active"},
                {"id": "p2", "name": format!("{team}-web"), "status": "disabled"}
            ])))
        },
    )
}

fn delete_project() -> RuntimeCommandSpec {
    RuntimeCommandSpec::new(
        CommandSpec::new("delete", "Delete a project")
            .with_system("projects-api")
            .with_tier(Tier::Destructive)
            .with_arg(Arg::new("id").long("id").required(true))
            .no_auth(true),
        async |_credential, args| {
            Ok(CommandResult::new(json!({
                    "deleted": args.get("id").and_then(Value::as_str).unwrap_or_default()
            })))
        },
    )
}

fn consumer_cli() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(platform_module()),
    )
}

#[tokio::test]
async fn consumer_style_cli_supports_json_human_schema_search_and_dry_run() {
    let cli = consumer_cli();

    let list = cli
        .run([
            "my-cli",
            "project",
            "list",
            "--team",
            "platform",
            "--filter",
            "status == 'active'",
            "--fields",
            "id,name",
        ])
        .await;
    assert_eq!(list.exit_code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&list.rendered).expect("json"),
        json!({"data": [{"id": "p1", "name": "platform-api"}]})
    );

    let human = cli
        .run([
            "my-cli", "project", "list", "--team", "platform", "--output", "human",
        ])
        .await;
    assert_eq!(human.exit_code, 0);
    assert!(human.rendered.contains("NAME"), "{}", human.rendered);
    assert!(human.rendered.contains("STATUS"), "{}", human.rendered);
    assert!(
        human.rendered.contains("p1  platform-api  active"),
        "{}",
        human.rendered
    );

    let schema = cli
        .run(["my-cli", "project", "list", "--schema", "--output", "json"])
        .await;
    assert_eq!(schema.exit_code, 0);
    let schema_json = serde_json::from_str::<Value>(&schema.rendered).expect("schema json");
    assert_eq!(schema_json["data"]["command"], "project:list");
    assert_eq!(schema_json["data"]["schema"]["title"], "Project");

    let search = cli
        .run([
            "my-cli", "project", "--search", "projects", "--output", "json",
        ])
        .await;
    assert_eq!(search.exit_code, 0);
    assert!(search.rendered.contains("project list"));

    let dry_run = cli
        .run([
            "my-cli",
            "project",
            "delete",
            "--id",
            "p1",
            "--dry-run",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(dry_run.exit_code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&dry_run.rendered).expect("dry-run json")["data"]["action"],
        "dry-run: would execute"
    );
}

#[tokio::test]
async fn consumer_style_cli_reports_invalid_args_and_output_separately() {
    let cli = consumer_cli();

    let missing = cli.run(["my-cli", "project", "list"]).await;
    assert_ne!(missing.exit_code, 0);
    assert!(missing.rendered.contains("required"));

    let invalid_output = cli
        .run([
            "my-cli", "project", "list", "--team", "platform", "--output", "yaml",
        ])
        .await;
    assert_ne!(invalid_output.exit_code, 0);
    assert!(invalid_output.rendered.contains("invalid output format"));
}
