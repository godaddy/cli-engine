use std::sync::Arc;

use clap::Arg;
use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, HumanViewDef, Module,
    NextAction, RuntimeCommandSpec, RuntimeGroupSpec, TableColumn, Tier,
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

fn consumer_cli_with_root_actions() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(platform_module())
            .with_root_next_actions(Arc::new(|| {
                vec![
                    NextAction::new("my-cli project list", "List projects"),
                    NextAction::new("my-cli tree", "Display the full command tree"),
                ]
            })),
    )
}

#[tokio::test]
async fn bare_invocation_human_shows_help_with_next_actions() {
    let cli = consumer_cli_with_root_actions();

    // `--human` forces the interactive format (under `cargo test` stdout is not a
    // TTY, so the default would otherwise resolve to json).
    let bare = cli.run(["my-cli", "--human"]).await;
    assert_eq!(bare.exit_code, 0);
    // Long help is preserved (the command summary still lists `tree`).
    assert!(bare.rendered.contains("tree"), "{}", bare.rendered);
    // Cold-start guidance is appended.
    assert!(
        bare.rendered.contains("Suggested next actions:"),
        "{}",
        bare.rendered
    );
    assert!(
        bare.rendered.contains("my-cli project list"),
        "{}",
        bare.rendered
    );
    // It is human text, not a JSON envelope.
    assert!(
        serde_json::from_str::<Value>(&bare.rendered).is_err(),
        "{}",
        bare.rendered
    );
    // The root help template drops the global options wall.
    assert!(!bare.rendered.contains("--fields"), "{}", bare.rendered);
}

#[tokio::test]
async fn group_help_keeps_subcommands_but_drops_global_options() {
    let cli = consumer_cli();

    let group = cli.run(["my-cli", "project"]).await;
    assert_eq!(group.exit_code, 0);
    // Group page lists its child commands...
    assert!(group.rendered.contains("Commands:"), "{}", group.rendered);
    assert!(group.rendered.contains("list"), "{}", group.rendered);
    assert!(group.rendered.contains("delete"), "{}", group.rendered);
    // ...but not the global options wall.
    assert!(!group.rendered.contains("--fields"), "{}", group.rendered);

    // Leaf commands keep their full flag set.
    let leaf = cli.run(["my-cli", "project", "list", "--help"]).await;
    assert!(leaf.rendered.contains("--fields"), "{}", leaf.rendered);
}

#[tokio::test]
async fn bare_invocation_emits_discovery_envelope_for_explicit_json() {
    let cli = consumer_cli_with_root_actions();

    let bare = cli.run(["my-cli", "--output", "json"]).await;
    assert_eq!(bare.exit_code, 0);
    let envelope = serde_json::from_str::<Value>(&bare.rendered).expect("discovery json");
    assert_eq!(envelope["data"]["description"], "Team CLI");
    assert_eq!(envelope["data"]["version"], "0.1.0");
    let actions = envelope["next_actions"]
        .as_array()
        .expect("next_actions array");
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["command"], "my-cli project list");
    // No env/auth/command_tree embedded — reachable via next_actions instead.
    assert!(envelope["data"].get("command_tree").is_none());

    // The `--json` shorthand is also an explicit machine-format request.
    let shorthand = cli.run(["my-cli", "--json"]).await;
    assert_eq!(shorthand.exit_code, 0);
    let shorthand_envelope =
        serde_json::from_str::<Value>(&shorthand.rendered).expect("shorthand discovery json");
    assert_eq!(shorthand_envelope["data"]["description"], "Team CLI");
    assert!(shorthand_envelope["next_actions"].is_array());
}

#[tokio::test]
async fn bare_invocation_rejects_invalid_output_format() {
    let cli = consumer_cli_with_root_actions();

    // An unrecognized explicit `--output` must error on the bare-root discovery
    // path just as it does for a normal command, rather than silently coercing
    // to JSON.
    let bare = cli.run(["my-cli", "--output", "yaml"]).await;
    assert_ne!(bare.exit_code, 0, "{}", bare.rendered);
    assert!(
        bare.rendered.contains("invalid output format"),
        "{}",
        bare.rendered
    );
}

#[tokio::test]
async fn bare_invocation_without_hook_falls_back_to_long_help() {
    let cli = consumer_cli();

    let bare = cli.run(["my-cli"]).await;
    assert_eq!(bare.exit_code, 0);
    assert!(
        !bare.rendered.contains("Suggested next actions:"),
        "{}",
        bare.rendered
    );

    // Even with explicit json, no hook means the unchanged long-help fallback.
    let bare_json = cli.run(["my-cli", "--output", "json"]).await;
    assert_eq!(bare_json.exit_code, 0);
    assert!(
        serde_json::from_str::<Value>(&bare_json.rendered).is_err(),
        "{}",
        bare_json.rendered
    );
}
