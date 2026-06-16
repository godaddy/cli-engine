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
}

fn list_projects() -> RuntimeCommandSpec {
    RuntimeCommandSpec::new(
        CommandSpec::new("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name,status")
            .with_json_schema::<Project>()
            // Inline view assigned directly to this command.
            .with_view(vec![
                TableColumn::new("id", "ID"),
                TableColumn::new("name", "Name"),
                TableColumn::new("status", "Status"),
            ])
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

// A command whose `default_fields` is a strict *subset* of its data keys, with
// no registered HumanViewDef (so the table auto-derives columns from the data).
// This makes the field projection directly observable in human output.
fn fields_demo_cli() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(Module::new("Demo", |_context| {
                RuntimeGroupSpec::new(GroupSpec::new("widget", "Manage widgets")).with_command(
                    RuntimeCommandSpec::new(
                        CommandSpec::new("list", "List widgets")
                            .with_system("widgets-api")
                            .with_default_fields("id,name")
                            .no_auth(true),
                        async |_credential, _args| {
                            Ok(CommandResult::new(json!([
                                {"id": "w1", "name": "alpha", "secret": "hidden"}
                            ])))
                        },
                    ),
                )
            })),
    )
}

#[tokio::test]
async fn human_output_projects_to_default_fields_when_no_fields_flag() {
    let cli = fields_demo_cli();
    let human = cli
        .run(["my-cli", "widget", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0, "{}", human.rendered);
    assert!(human.rendered.contains("NAME"), "{}", human.rendered);
    // `default_fields` is "id,name", so the `secret` column must be projected
    // out of the human table even though no `--fields` flag was passed.
    assert!(
        !human.rendered.contains("SECRET") && !human.rendered.contains("hidden"),
        "human output should honor default_fields and omit `secret`: {}",
        human.rendered
    );
}

#[tokio::test]
async fn human_output_with_fields_all_overrides_default_fields() {
    // The escape hatch: `--fields all` shows every column in human mode.
    let cli = fields_demo_cli();
    let human = cli
        .run([
            "my-cli", "widget", "list", "--output", "human", "--fields", "all",
        ])
        .await;
    assert_eq!(human.exit_code, 0, "{}", human.rendered);
    assert!(human.rendered.contains("hidden"), "{}", human.rendered);
}

#[tokio::test]
async fn schema_on_no_schema_command_reports_no_schema_when_required_arg_missing() {
    // `project delete` has a required `--id` and no registered schema. Asking for
    // `--schema` must short-circuit with the no-schema message through the public
    // `Cli::run` path, not let clap reject the missing `--id` first.
    let cli = consumer_cli();
    let out = cli
        .run([
            "my-cli", "project", "delete", "--schema", "--output", "json",
        ])
        .await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    let envelope: Value = serde_json::from_str(&out.rendered).expect("json envelope");
    assert_eq!(
        envelope["data"]["message"], "No output schema is registered for this command.",
        "{}",
        out.rendered
    );
    // Same `{command, fields}` shape as a real SchemaInfo response.
    assert_eq!(
        envelope["data"]["command"], "project:delete",
        "{}",
        out.rendered
    );
    assert_eq!(envelope["data"]["fields"], json!([]), "{}", out.rendered);
}

#[tokio::test]
async fn schema_on_unknown_command_still_reports_unknown_command() {
    // The no-schema short-circuit must only fire for a real leaf command. A
    // typo'd command with `--schema` should still surface an unknown-command
    // error, not a misleading "no schema registered" body.
    let cli = consumer_cli();
    let out = cli
        .run(["my-cli", "project", "bogus", "--schema", "--output", "json"])
        .await;
    assert_ne!(out.exit_code, 0, "{}", out.rendered);
    assert!(out.rendered.contains("unknown command"), "{}", out.rendered);
    assert!(
        !out.rendered.contains("No output schema is registered"),
        "{}",
        out.rendered
    );
}

// A command that registers a HumanViewDef under its *command path* while setting
// a separate `system` (the common case: many commands share one backend system).
// `default_fields` is a strict subset of the view's columns, so if the view is
// resolved, projection is skipped and the view's extra column survives.
fn viewed_widgets_cli() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(
                Module::new("Demo", |_context| {
                    RuntimeGroupSpec::new(GroupSpec::new("widget", "Manage widgets")).with_command(
                        RuntimeCommandSpec::new(
                            CommandSpec::new("list", "List widgets")
                                .with_system("widgets-api")
                                // Reference a shared view registered on the module.
                                .with_view_id("widget-table")
                                .no_auth(true),
                            async |_credential, _args| {
                                Ok(CommandResult::new(json!([
                                    {"id": "w1", "name": "alpha", "status": "active"}
                                ])))
                            },
                        ),
                    )
                })
                .with_view(HumanViewDef::new(
                    "widget-table",
                    vec![
                        TableColumn::new("name", "Name"),
                        TableColumn::new("status", "Status"),
                    ],
                )),
            ),
    )
}

#[tokio::test]
async fn human_output_resolves_shared_view_by_id() {
    // The command references the shared view `widget-table` via `with_view_id`,
    // while its `system` is `widgets-api`. With no field selection, the view
    // resolves by the declared id and renders all its columns.
    let cli = viewed_widgets_cli();
    let human = cli
        .run(["my-cli", "widget", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0, "{}", human.rendered);
    assert!(human.rendered.contains("STATUS"), "{}", human.rendered);
    assert!(human.rendered.contains("active"), "{}", human.rendered);
}

// A command that assigns its view inline with `with_view`.
fn inline_view_gadgets_cli() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(Module::new("Demo", |_context| {
                RuntimeGroupSpec::new(GroupSpec::new("gadget", "Manage gadgets")).with_command(
                    RuntimeCommandSpec::new(
                        CommandSpec::new("list", "List gadgets")
                            .with_view(vec![
                                TableColumn::new("name", "Name"),
                                TableColumn::new("status", "Status"),
                            ])
                            .no_auth(true),
                        async |_credential, _args| {
                            Ok(CommandResult::new(json!([
                                {"id": "g1", "name": "alpha", "status": "active"}
                            ])))
                        },
                    ),
                )
            })),
    )
}

#[tokio::test]
async fn human_output_resolves_inline_view() {
    // The command assigns its view inline with `with_view`. The engine registers
    // it under the command path at build, so it resolves and renders its columns.
    let cli = inline_view_gadgets_cli();
    let human = cli
        .run(["my-cli", "gadget", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0, "{}", human.rendered);
    assert!(human.rendered.contains("STATUS"), "{}", human.rendered);
    assert!(human.rendered.contains("active"), "{}", human.rendered);
}

// A command with a three-column view and `default_fields` that selects a subset
// of those columns — the default `--fields`. Used to verify that field
// selection narrows the view's columns rather than projecting the data.
fn curated_view_cli() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(Module::new("Demo", |_context| {
                RuntimeGroupSpec::new(GroupSpec::new("gizmo", "Manage gizmos")).with_command(
                    RuntimeCommandSpec::new(
                        CommandSpec::new("list", "List gizmos")
                            .with_default_fields("id,name")
                            .with_view(vec![
                                TableColumn::new("id", "ID"),
                                TableColumn::new("name", "Name"),
                                TableColumn::new("status", "Status"),
                            ])
                            .no_auth(true),
                        async |_credential, _args| {
                            Ok(CommandResult::new(json!([
                                {"id": "z1", "name": "alpha", "status": "active"}
                            ])))
                        },
                    ),
                )
            })),
    )
}

#[tokio::test]
async fn default_fields_narrows_view_columns() {
    // `default_fields="id,name"` is the default `--fields`, so by default only the
    // `id` and `name` view columns show — `status` is omitted even though the view
    // defines it and the data carries it.
    let cli = curated_view_cli();
    let human = cli
        .run(["my-cli", "gizmo", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0, "{}", human.rendered);
    assert!(human.rendered.contains("ID"), "{}", human.rendered);
    assert!(human.rendered.contains("NAME"), "{}", human.rendered);
    assert!(!human.rendered.contains("STATUS"), "{}", human.rendered);
    assert!(!human.rendered.contains("active"), "{}", human.rendered);
}

#[tokio::test]
async fn fields_all_shows_every_view_column() {
    // `--fields all` overrides `default_fields` and shows every view column.
    let cli = curated_view_cli();
    let human = cli
        .run([
            "my-cli", "gizmo", "list", "--output", "human", "--fields", "all",
        ])
        .await;
    assert_eq!(human.exit_code, 0, "{}", human.rendered);
    assert!(human.rendered.contains("STATUS"), "{}", human.rendered);
    assert!(human.rendered.contains("active"), "{}", human.rendered);
}

#[tokio::test]
async fn fields_flag_selects_view_columns() {
    // An explicit `--fields` selects which view columns show: `id,status` keeps
    // those two columns and drops `name`, reading values from the full payload.
    let cli = curated_view_cli();
    let human = cli
        .run([
            "my-cli",
            "gizmo",
            "list",
            "--output",
            "human",
            "--fields",
            "id,status",
        ])
        .await;
    assert_eq!(human.exit_code, 0, "{}", human.rendered);
    assert!(human.rendered.contains("ID"), "{}", human.rendered);
    assert!(human.rendered.contains("STATUS"), "{}", human.rendered);
    assert!(human.rendered.contains("active"), "{}", human.rendered);
    assert!(!human.rendered.contains("NAME"), "{}", human.rendered);
}

// A command that sets BOTH an inline view and a shared view id. `with_view_id`
// takes precedence, so the shared view renders and the inline columns are unused
// (and are not registered).
fn combo_view_cli() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(
                Module::new("Demo", |_context| {
                    RuntimeGroupSpec::new(GroupSpec::new("combo", "Manage combos")).with_command(
                        RuntimeCommandSpec::new(
                            CommandSpec::new("list", "List combos")
                                .with_view(vec![TableColumn::new("id", "ID")])
                                .with_view_id("combo-shared")
                                .no_auth(true),
                            async |_credential, _args| {
                                Ok(CommandResult::new(json!([
                                    {"id": "c1", "status": "active"}
                                ])))
                            },
                        ),
                    )
                })
                .with_view(HumanViewDef::new(
                    "combo-shared",
                    vec![TableColumn::new("status", "Status")],
                )),
            ),
    )
}

#[tokio::test]
async fn view_id_takes_precedence_over_inline_view() {
    // The command sets both `with_view([id])` and `with_view_id("combo-shared")`.
    // The shared view (a `status` column) wins; the inline `id` column is not used.
    let cli = combo_view_cli();
    let human = cli
        .run(["my-cli", "combo", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0, "{}", human.rendered);
    assert!(human.rendered.contains("STATUS"), "{}", human.rendered);
    assert!(human.rendered.contains("active"), "{}", human.rendered);
    assert!(!human.rendered.contains("ID"), "{}", human.rendered);
}
