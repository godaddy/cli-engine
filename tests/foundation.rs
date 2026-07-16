use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use async_trait::async_trait;
use clap::{Arg, ArgAction, Command, value_parser};
use cli_engine::transport;
use cli_engine::{
    ActivityEmitter, ActivityEvent, Auditor, AuthProvider, Authorizer, Cli, CliConfig,
    CommandContext, CommandMeta, CommandModule, CommandResult, CommandSpec, Credential,
    CredentialResolver, Dispatcher, FieldInfo, GroupSpec, GuideEntry, HumanViewDef,
    HumanViewRegistry, Middleware, MiddlewareRequest, Module, ModuleContext, ModuleHelpEntry,
    OutputField, OutputSchema, Result, RuntimeCommandSpec, RuntimeGroupSpec, SchemaRegistry,
    TableColumn, Tier, TreeNode,
    auth::commands::{
        auth_command_group, login_and_build, logout_result, status_result, to_status_entry,
    },
    auth::exec::{ACTION_AUTHENTICATE, AuthnRequest, ExecProvider},
    build_module_group, build_root_long, build_tree_from_clap, derive_bool_flags,
    derive_value_flags, extract_command_path, extract_output_format, extract_search_query,
    format_help_section,
    guide::guide_content,
    has_true_schema_flag,
    output::render_human_with_view,
    output::{Envelope, OutputFormat, PipelineOpts, apply_pipeline, filter_fields, render},
    register_global_flags, register_global_human_view, register_global_schema,
    register_reason_flag, render_tree_human,
    search::{SearchDocument, SearchIndex, tokenize},
    transport::{
        ApiKeyInjector, AuthInjector, BasicAuthInjector, BearerTokenInjector,
        ClientCredentialsInjector, CookieInjector, HttpClient, HttpClientBuilder, NoopInjector,
        ProviderBearerInjector, TokenFunc, TransportLogEvent, TransportLogger,
    },
};
use pretty_assertions::assert_eq;
use schemars::JsonSchema;
use serde_json::json;
use tokio::sync::Mutex;

fn middleware_request<'request>(
    meta: CommandMeta,
    command_path: &'request str,
    user_args: cli_engine::middleware::ValueMap,
    args: cli_engine::middleware::ValueMap,
    default_fields: &'request str,
    no_auth: bool,
) -> MiddlewareRequest<'request> {
    MiddlewareRequest {
        meta,
        command_path,
        system: command_path
            .split_once(':')
            .map_or(command_path, |(system, _)| system),
        user_args,
        args,
        default_fields,
        view_id: None,
        auth: auth_requirement(no_auth),
    }
}

/// Builds a request that declares a human view id, the way the engine does for a
/// command with `with_view`/`with_view_id`.
fn middleware_request_with_view<'request>(
    meta: CommandMeta,
    command_path: &'request str,
    view_id: &'request str,
    user_args: cli_engine::middleware::ValueMap,
    args: cli_engine::middleware::ValueMap,
    default_fields: &'request str,
    no_auth: bool,
) -> MiddlewareRequest<'request> {
    MiddlewareRequest {
        meta,
        command_path,
        system: command_path
            .split_once(':')
            .map_or(command_path, |(system, _)| system),
        user_args,
        args,
        default_fields,
        view_id: Some(view_id),
        auth: auth_requirement(no_auth),
    }
}

/// Maps the legacy `no_auth` bool used by these helpers to an [`AuthRequirement`]:
/// `true` means the command never authenticates, `false` keeps the fail-closed
/// `Required` default.
fn auth_requirement(no_auth: bool) -> cli_engine::AuthRequirement {
    if no_auth {
        cli_engine::AuthRequirement::None
    } else {
        cli_engine::AuthRequirement::Required
    }
}

fn middleware_request_with_system<'request>(
    meta: CommandMeta,
    command_path: &'request str,
    system: &'request str,
    user_args: cli_engine::middleware::ValueMap,
    args: cli_engine::middleware::ValueMap,
    default_fields: &'request str,
    no_auth: bool,
) -> MiddlewareRequest<'request> {
    MiddlewareRequest {
        meta,
        command_path,
        system,
        user_args,
        args,
        default_fields,
        view_id: None,
        auth: auth_requirement(no_auth),
    }
}

#[derive(Debug, Default)]
struct RecordingTransportLogger {
    events: StdMutex<Vec<TransportLogEvent>>,
}

impl TransportLogger for RecordingTransportLogger {
    fn debug(&self, event: &TransportLogEvent) {
        self.events.lock().expect("logger lock").push(event.clone());
    }
}

impl RecordingTransportLogger {
    fn messages(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("logger lock")
            .iter()
            .map(|event| event.message.to_owned())
            .collect()
    }

    fn events(&self) -> Vec<TransportLogEvent> {
        self.events.lock().expect("logger lock").clone()
    }
}

static USER_AGENT_TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// Serializes tests in this binary that mutate the process-wide default
/// transport logger, so an install/assert/reset window in one test cannot be
/// disturbed by another test resetting the global concurrently.
static TRANSPORT_LOGGER_TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// Restores the process-wide default User-Agent to the builtin on drop, so a
/// panicking assertion in a test that publishes a config-derived UA cannot leak
/// it into later tests. Hold alongside (declared after) the `USER_AGENT_TEST_LOCK`
/// guard so the reset runs while the lock is still held.
struct RestoreDefaultUserAgent;

impl Drop for RestoreDefaultUserAgent {
    fn drop(&mut self) {
        transport::set_default_user_agent("cli/dev");
    }
}

#[test]
fn tier_string_forms_and_mutating_parity() {
    assert_eq!(Tier::Read.to_string(), "read");
    assert_eq!(Tier::Mutate.to_string(), "mutate");
    assert_eq!(Tier::Destructive.to_string(), "destructive");
    assert!(!Tier::Read.is_mutating());
    assert!(Tier::Mutate.is_mutating());
    assert!(Tier::Destructive.is_mutating());
    assert_eq!("read".parse::<Tier>(), Ok(Tier::Read));
}

#[test]
fn build_info_formats_version_preserves_legacy_cli_config() {
    let build = cli_engine::BuildInfo {
        version: "1.2.3".to_owned(),
        commit: Some("abc123".to_owned()),
        date: Some("2026-05-18".to_owned()),
    };

    assert_eq!(
        build.version_string(),
        "1.2.3 (commit abc123, built 2026-05-18)"
    );

    assert_eq!(
        cli_engine::BuildInfo {
            version: "1.2.3".to_owned(),
            commit: None,
            date: None,
        }
        .version_string(),
        "1.2.3"
    );
    assert_eq!(
        cli_engine::BuildInfo {
            version: "1.2.3".to_owned(),
            commit: Some("abc123".to_owned()),
            date: None,
        }
        .version_string(),
        "1.2.3 (commit abc123, built )"
    );
    assert_eq!(
        cli_engine::BuildInfo {
            version: "1.2.3".to_owned(),
            commit: None,
            date: Some("2026-05-18".to_owned()),
        }
        .version_string(),
        "1.2.3 (commit , built 2026-05-18)"
    );
    assert_eq!(
        cli_engine::BuildInfo {
            version: "1.2.3".to_owned(),
            commit: Some(String::new()),
            date: Some(String::new()),
        }
        .version_string(),
        "1.2.3"
    );
}

#[test]
fn cli_config_builders_cover_common_adoption_path() {
    let module = Module::new("Platform Systems", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
    });
    let guide = GuideEntry::new("project", "Project workflows", "Use project commands.");

    let config = CliConfig::new("my-cli", "Developer tooling", "my-cli")
        .with_long("Long help")
        .with_build(
            cli_engine::BuildInfo::new("1.2.3")
                .with_commit("abc123")
                .with_date("2026-05-19"),
        )
        .with_default_auth_provider("primary")
        .with_module(module)
        .with_guide(guide);

    assert_eq!(config.name, "my-cli");
    assert_eq!(config.short, "Developer tooling");
    assert_eq!(config.long.as_deref(), Some("Long help"));
    assert_eq!(
        config.build.version_string(),
        "1.2.3 (commit abc123, built 2026-05-19)"
    );
    assert_eq!(config.default_auth_provider.as_deref(), Some("primary"));
    assert_eq!(config.modules.len(), 1);
    assert_eq!(config.guides.len(), 1);
}

#[test]
fn small_public_builders_cover_common_registration_shapes() {
    let field = FieldInfo::new("owner", "string").optional();
    assert_eq!(field.name, "owner");
    assert_eq!(field.field_type, "string");
    assert!(field.optional);

    let schema = cli_engine::SchemaInfo::new("project:list").with_fields(vec![field.clone()]);
    assert_eq!(schema.command, "project:list");
    assert_eq!(schema.fields, vec![field]);
    assert!(schema.schema.is_none());

    let view = HumanViewDef::new(
        "project:list",
        vec![
            TableColumn::new("id", "ID"),
            TableColumn::new("owner.name", "Owner"),
        ],
    );
    assert_eq!(view.schema_id, "project:list");
    assert_eq!(view.columns[1].field, "owner.name");

    let guide = GuideEntry::new("deploy", "Deploy projects", "# Deploy\n");
    assert_eq!(guide.name, "deploy");
    assert_eq!(guide.summary, "Deploy projects");

    let doc = SearchDocument::new("guide:deploy", "guide", "Deploy")
        .with_summary("Deploy projects")
        .with_content("Deploy projects safely");
    assert_eq!(doc.summary, "Deploy projects");
    assert_eq!(doc.content, "Deploy projects safely");

    let tree = TreeNode::new("my-cli", "Developer tooling", "my-cli").with_child(TreeNode::new(
        "project",
        "Manage projects",
        "my-cli project",
    ));
    assert_eq!(tree.children[0].path, "my-cli project");
}

#[tokio::test]
async fn cli_runtime_version_flag_matches_parser_output() {
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        build: cli_engine::BuildInfo {
            version: "1.2.3".to_owned(),
            commit: Some("abc123".to_owned()),
            date: Some("2026-05-19".to_owned()),
        },
        ..CliConfig::default()
    });

    let long = cli.run(["my-cli", "--version"]).await;
    assert_eq!(long.exit_code, 0);
    assert_eq!(
        long.rendered,
        "my-cli version 1.2.3 (commit abc123, built 2026-05-19)\n"
    );

    let short = cli.run(["my-cli", "-v"]).await;
    assert_eq!(short.exit_code, 0);
    assert_eq!(short.rendered, long.rendered);

    let after_global = cli.run(["my-cli", "--output", "json", "--version"]).await;
    assert_eq!(after_global.exit_code, 0);
    assert_eq!(after_global.rendered, long.rendered);
}

#[tokio::test]
async fn cli_runtime_version_shortcut_does_not_steal_command_flags() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        build: cli_engine::BuildInfo {
            version: "1.2.3".to_owned(),
            ..cli_engine::BuildInfo::default()
        },
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("inspect", "Inspect")
            .no_auth(true)
            .with_arg(
                Arg::new("very-verbose")
                    .short('v')
                    .action(ArgAction::SetTrue),
            ),
        async |context| {
            Ok(CommandResult::new(
                json!({"verbose": context.args["very-verbose"]}),
            ))
        },
    ));

    let output = cli
        .run(["my-cli", "inspect", "-v", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!({"verbose": true}));
}

#[tokio::test]
async fn cli_runtime_dispatches_nested_command_through_middleware() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    let command = RuntimeCommandSpec::new(
        CommandSpec::new("list", "List projects")
            .no_auth(true)
            .with_flag(
                Arg::new("project")
                    .long("project")
                    .default_value("default-project"),
            )
            .with_flag(Arg::new("all").long("all").action(ArgAction::SetTrue))
            .mutates(false),
        async |_credential, args| {
            assert_eq!(args.get("project"), Some(&json!("alpha")));
            assert_eq!(args.get("all"), Some(&json!(true)));
            Ok(CommandResult::new(
                json!({"project": args["project"], "all": args["all"]}),
            ))
        },
    );
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(command),
    );

    let output = cli
        .run([
            "my-cli",
            "project",
            "list",
            "--project",
            "alpha",
            "--all",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!({"project": "alpha", "all": true}));
    assert_eq!(rendered["metadata"], serde_json::Value::Null);
}

#[tokio::test]
async fn cli_runtime_renders_root_help_without_subcommand() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let output = cli.run(["my-cli"]).await;

    assert_eq!(output.exit_code, 0);
    assert!(output.rendered.contains("Developer tooling"));
    assert!(output.rendered.contains("project"));
    assert!(output.rendered.contains("--search"));
}

#[tokio::test]
async fn cli_runtime_root_help_includes_find_commands_without_modules() {
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });

    let output = cli.run(["my-cli"]).await;

    assert_eq!(output.exit_code, 0);
    assert!(output.rendered.contains("Developer tooling"));
    assert!(output.rendered.contains("Find Commands"));
    assert!(output.rendered.contains("--search <keyword>"));
    assert!(
        output
            .rendered
            .contains("tree                Display full command tree")
    );
}

#[tokio::test]
async fn cli_execute_from_writes_success_to_stdout_and_errors_to_stderr() {
    // execute_from publishes the config-derived User-Agent process-wide, so this
    // test shares the lock with the user-agent tests and restores the default
    // on exit (including panic) via the RAII guard, while the lock is held.
    let _ua_guard = USER_AGENT_TEST_LOCK.lock().await;
    let _restore_ua = RestoreDefaultUserAgent;
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        commands: vec![RuntimeCommandSpec::new(
            CommandSpec::new("ping", "Ping").no_auth(true),
            async |_credential, _args| Ok(CommandResult::new(json!({"ok": true}))),
        )],
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("pong", "Pong").no_auth(true),
        async |_credential, _args| Ok(CommandResult::new(json!({"ok": true}))),
    ));

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = cli
        .execute_from(
            ["my-cli", "ping", "--output", "json"],
            &mut stdout,
            &mut stderr,
        )
        .await
        .expect("execute should write");
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    assert!(stderr.is_empty());
    let rendered: serde_json::Value =
        serde_json::from_slice(&stdout).expect("stdout should contain json");
    assert_eq!(rendered["data"], json!({"ok": true}));

    stdout.clear();
    stderr.clear();
    let code = cli
        .execute_from(["my-cli", "missing"], &mut stdout, &mut stderr)
        .await
        .expect("execute should write");
    assert_eq!(code, std::process::ExitCode::from(1));
    assert!(stdout.is_empty());
    let rendered = String::from_utf8(stderr).expect("utf8");
    assert!(rendered.contains("missing"));
}

#[tokio::test]
async fn cli_execute_from_shutdown_signal_writes_interrupt_to_stderr() {
    // execute_from_until_signal publishes the config-derived User-Agent
    // process-wide; share the lock and restore the default (panic-safe) like above.
    let _ua_guard = USER_AGENT_TEST_LOCK.lock().await;
    let _restore_ua = RestoreDefaultUserAgent;
    let shutdown_count = Arc::new(AtomicUsize::new(0));
    let shutdown_for_closure = Arc::clone(&shutdown_count);
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        on_shutdown: Some(Arc::new(move || {
            shutdown_for_closure.fetch_add(1, Ordering::SeqCst);
        })),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("slow", "Slow command").no_auth(true),
        async |_credential, _args| {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(CommandResult::new(json!({"done": true})))
        },
    ));

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = cli
        .execute_from_until_signal(["my-cli", "slow"], &mut stdout, &mut stderr, async {})
        .await
        .expect("execute should write interrupt output");

    assert_eq!(code, std::process::ExitCode::from(130));
    assert!(stdout.is_empty());
    assert_eq!(
        String::from_utf8(stderr).expect("stderr should be utf8"),
        "command interrupted\n"
    );
    assert_eq!(shutdown_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_runtime_help_command_renders_root_and_command_help() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let root = cli.run(["my-cli", "help"]).await;
    assert_eq!(root.exit_code, 0);
    assert!(root.rendered.contains("Developer tooling"));

    let command = cli.run(["my-cli", "help", "project"]).await;
    assert_eq!(command.exit_code, 0);
    assert!(command.rendered.contains("Manage projects"));
}

#[tokio::test]
async fn cli_runtime_group_help_subcommand_renders_group_help() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    // The root disables clap's auto-generated help subcommand (the engine ships
    // a curated root `help` command), and that setting propagates to every
    // group. So `<group> help` is handled by an engine-level compatibility shim
    // that renders the group help, rather than by a native clap help subcommand.
    // It must render the group help rather than report `help` as unknown.
    let group = cli.run(["my-cli", "project", "help"]).await;
    assert_eq!(group.exit_code, 0);
    assert!(
        group.rendered.contains("Manage projects"),
        "expected group help, got: {}",
        group.rendered
    );

    // The same form must work for a leaf command's help subcommand argument.
    let leaf = cli.run(["my-cli", "project", "help", "list"]).await;
    assert_eq!(leaf.exit_code, 0);
    assert!(
        leaf.rendered.contains("List projects"),
        "expected leaf help, got: {}",
        leaf.rendered
    );
}

#[tokio::test]
async fn cli_runtime_group_help_preserves_global_flags() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    // A global flag before the group must be preserved through the rewrite —
    // its value (`json`) must not be dropped or mistaken for a positional, so
    // parsing still succeeds and the group help renders.
    let before = cli
        .run(["my-cli", "--output", "json", "project", "help"])
        .await;
    assert_eq!(before.exit_code, 0, "rendered: {}", before.rendered);
    assert!(
        before.rendered.contains("Manage projects"),
        "expected group help, got: {}",
        before.rendered
    );

    // A `key=value` flag after the group must likewise survive in place.
    let after = cli
        .run(["my-cli", "project", "help", "--output=json"])
        .await;
    assert_eq!(after.exit_code, 0, "rendered: {}", after.rendered);
    assert!(
        after.rendered.contains("Manage projects"),
        "expected group help, got: {}",
        after.rendered
    );
}

#[tokio::test]
async fn cli_runtime_group_help_defers_to_consumer_help_command() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    // A consumer is free to register a real command literally named `help`
    // under a group. The group-help shim must defer to it rather than hijack
    // the invocation to render help.
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("help", "Consumer help command").no_auth(true),
                async |_credential, _args| {
                    Ok(CommandResult::new(json!({ "ran": "consumer-help" })))
                },
            ),
        ),
    );

    let output = cli.run(["my-cli", "project", "help"]).await;
    assert_eq!(output.exit_code, 0);
    assert!(
        output.rendered.contains("consumer-help"),
        "expected the consumer help command to run, got: {}",
        output.rendered
    );
}

#[tokio::test]
async fn cli_runtime_group_help_after_double_dash_is_literal() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    // After `--`, positionals are literal operands, not command keywords. A
    // `help` token there must NOT trigger the group-help shim; it is treated
    // as a literal (unknown) subcommand name instead of rendering group help.
    let literal = cli.run(["my-cli", "project", "--", "help"]).await;
    assert_ne!(
        literal.exit_code, 0,
        "expected `help` after `--` to be a literal operand, got: {}",
        literal.rendered
    );
    assert!(
        !literal.rendered.contains("Manage projects"),
        "expected no group help for a post-`--` `help`, got: {}",
        literal.rendered
    );

    // A `help` *before* `--` is still a help request; the `--` only guards the
    // suffix, so `<group> help -- <sub>` still renders the subcommand's help.
    let before = cli.run(["my-cli", "project", "help", "--", "list"]).await;
    assert_eq!(before.exit_code, 0, "rendered: {}", before.rendered);
    assert!(
        before.rendered.contains("List projects"),
        "expected leaf help, got: {}",
        before.rendered
    );
}

#[tokio::test]
async fn cli_runtime_nested_group_help_subcommand_renders_group_help() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    // A nested group: `project platform` is a group with its own leaf. The
    // group-help shim must walk the full prefix and render the nested group's
    // help for `<group> <subgroup> help`.
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_group(
            RuntimeGroupSpec::new(GroupSpec::new("platform", "Manage platforms")).with_command(
                RuntimeCommandSpec::new(
                    CommandSpec::new("list", "List platforms").no_auth(true),
                    async |_credential, _args| Ok(CommandResult::new(json!({}))),
                ),
            ),
        ),
    );

    // `<group> <subgroup> help` renders the nested group's help.
    let nested = cli.run(["my-cli", "project", "platform", "help"]).await;
    assert_eq!(nested.exit_code, 0, "rendered: {}", nested.rendered);
    assert!(
        nested.rendered.contains("Manage platforms"),
        "expected nested group help, got: {}",
        nested.rendered
    );

    // `<group> <subgroup> help <leaf>` renders the nested leaf's help.
    let leaf = cli
        .run(["my-cli", "project", "platform", "help", "list"])
        .await;
    assert_eq!(leaf.exit_code, 0, "rendered: {}", leaf.rendered);
    assert!(
        leaf.rendered.contains("List platforms"),
        "expected nested leaf help, got: {}",
        leaf.rendered
    );
}

#[tokio::test]
async fn cli_runtime_help_command_matches_parser_find_leftover_args() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let group = cli.run(["my-cli", "help", "project", "missing"]).await;
    assert_eq!(group.exit_code, 0);
    assert!(group.rendered.contains("Manage projects"));

    let leaf = cli
        .run(["my-cli", "help", "project", "list", "ignored"])
        .await;
    assert_eq!(leaf.exit_code, 0);
    assert!(leaf.rendered.contains("List projects"));

    let unknown = cli.run(["my-cli", "help", "missing"]).await;
    assert_eq!(unknown.exit_code, 1);
    assert_eq!(
        unknown.rendered,
        "unknown command \"missing\" — run 'my-cli help' for available commands"
    );
}

#[tokio::test]
async fn cli_runtime_bare_group_renders_group_help() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let output = cli.run(["my-cli", "project"]).await;

    assert_eq!(output.exit_code, 0);
    assert!(output.rendered.contains("Manage projects"));
    assert!(output.rendered.contains("list"));
    assert!(!output.rendered.contains("unknown command"));
}

#[tokio::test]
async fn cli_runtime_bare_group_runs_pre_run_before_help_preserves_legacy_group_run_e() {
    let calls = Arc::new(StdMutex::new(Vec::<String>::new()));
    let calls_for_closure = Arc::clone(&calls);
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        pre_run: Some(Arc::new(move |_middleware, command_path, args| {
            assert!(args.is_empty());
            calls_for_closure
                .lock()
                .expect("calls lock")
                .push(command_path.to_owned());
            Ok(())
        })),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let output = cli.run(["my-cli", "project"]).await;

    assert_eq!(output.exit_code, 0);
    assert!(output.rendered.contains("Manage projects"));
    assert_eq!(
        calls.lock().expect("calls lock").as_slice(),
        &["project".to_owned()]
    );
}

#[tokio::test]
async fn cli_runtime_group_unknown_command_matches_legacy_group_run_e() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects").with_alias("p"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            )),
    );

    let output = cli
        .run(["my-cli", "--output", "json", "p", "missing"])
        .await;

    assert_eq!(output.exit_code, 1);
    assert_eq!(
        output.rendered,
        "unknown command \"missing\" for \"my-cli project\""
    );
}

#[tokio::test]
async fn cli_runtime_group_unknown_command_respects_registered_value_flags() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        register_flags: Some(Arc::new(|command: Command| {
            command.arg(Arg::new("profile").long("profile").global(true))
        })),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects").with_alias("p"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            )),
    );

    let output = cli
        .run([
            "my-cli",
            "--profile",
            "prod",
            "--output",
            "json",
            "p",
            "missing",
        ])
        .await;

    assert_eq!(output.exit_code, 1);
    assert_eq!(
        output.rendered,
        "unknown command \"missing\" for \"my-cli project\""
    );
}

#[tokio::test]
async fn cli_runtime_help_command_errors_for_unknown_target() {
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });

    let output = cli.run(["my-cli", "help", "missing"]).await;

    assert_eq!(output.exit_code, 1);
    assert_eq!(
        output.rendered,
        "unknown command \"missing\" — run 'my-cli help' for available commands"
    );
}

#[tokio::test]
async fn cli_runtime_auto_tree_command_renders_command_hierarchy() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let output = cli.run(["my-cli", "tree", "--output", "json"]).await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["name"], "my-cli");
    let children = rendered["data"]["children"]
        .as_array()
        .expect("children array");
    assert!(children.iter().any(|child| child["name"] == "help"));
    assert!(children.iter().any(|child| child["name"] == "tree"));
    let project = children
        .iter()
        .find(|child| child["name"] == "project")
        .expect("project command should be in tree");
    assert_eq!(project["children"][0]["name"], "list");

    let human = cli.run(["my-cli", "tree", "--output", "human"]).await;
    assert_eq!(human.exit_code, 0);
    assert!(human.rendered.starts_with("my-cli\n"));
    assert!(human.rendered.contains("project ··· Manage projects"));
    assert!(human.rendered.contains("list ··· List projects"));
    assert!(!human.rendered.contains("NAME"));
}

#[tokio::test]
async fn cli_runtime_guide_command_lists_topics_and_renders_content() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_guides([GuideEntry {
        name: "deploy".to_owned(),
        summary: "Deploy safely".to_owned(),
        content: "# Deploy\n".to_owned(),
    }]);

    let list = cli.run(["my-cli", "guide"]).await;
    assert_eq!(list.exit_code, 0);
    assert_eq!(
        list.rendered,
        "Available guide topics:\n\n  deploy           Deploy safely\n\nUsage: <cli> guide <topic>"
    );

    let topic = cli
        .run(["my-cli", "guide", "deploy", "--output", "json"])
        .await;
    assert_eq!(topic.exit_code, 0);
    assert_eq!(topic.rendered, "# Deploy\n");
}

#[tokio::test]
async fn cli_runtime_guide_human_reflows_long_lines_but_other_formats_stay_raw() {
    use std::io::IsTerminal;

    let long_line = "This is a deliberately long single line of guide prose that should be \
        reflowed by the renderer instead of wrapping mid-word inside a narrow terminal window.";
    let content = format!("# Deploy\n\n{long_line}\n");

    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_guides([GuideEntry {
        name: "deploy".to_owned(),
        summary: "Deploy safely".to_owned(),
        content: content.clone(),
    }]);

    // json (and the non-TTY default) keep the raw markdown body verbatim.
    let raw = cli
        .run(["my-cli", "guide", "deploy", "--output", "json"])
        .await;
    assert_eq!(raw.exit_code, 0);
    assert_eq!(raw.rendered, content);

    // An invalid explicit --output is rejected, matching normal commands
    // rather than silently emitting raw content.
    let invalid = cli
        .run(["my-cli", "guide", "deploy", "--output", "yaml"])
        .await;
    assert_ne!(invalid.exit_code, 0);
    assert!(
        invalid.rendered.contains("invalid output format"),
        "expected invalid-output-format error, got: {:?}",
        invalid.rendered,
    );

    let human = cli.run(["my-cli", "guide", "deploy", "--human"]).await;
    assert_eq!(human.exit_code, 0);

    // The remaining checks describe the deterministic non-TTY path (no color,
    // fixed width 80). If the test runs attached to a real terminal (e.g. with
    // `-- --nocapture`), `stdout().is_terminal()` is true and the renderer uses
    // ANSI color and the live width instead, so skip the strict assertions.
    if std::io::stdout().is_terminal() {
        return;
    }

    assert_ne!(human.rendered, content, "human output should be reflowed");
    assert!(
        !human.rendered.contains('\u{1b}'),
        "no-color human output must not contain ANSI escapes",
    );
    for line in human.rendered.lines() {
        assert!(
            line.trim_end().chars().count() <= 80,
            "human line exceeds width: {line:?}",
        );
    }
    // The long source line must have been split across several visible lines...
    assert!(
        human.rendered.lines().count() > content.lines().count(),
        "expected the long line to wrap: {:?}",
        human.rendered,
    );
    // ...without breaking any word across a line boundary.
    for word in long_line.split_whitespace() {
        assert!(
            human.rendered.lines().any(|line| line.contains(word)),
            "word was split across lines: {word:?}",
        );
    }
}

#[tokio::test]
async fn cli_config_registers_modules_guides_views_and_init_once() {
    #[derive(Debug)]
    struct Thing;

    impl OutputSchema for Thing {
        fn fields() -> &'static [OutputField] {
            &[
                OutputField {
                    name: "name",
                    field_type: "string",
                    optional: false,
                },
                OutputField {
                    name: "enabled",
                    field_type: "bool",
                    optional: false,
                },
            ]
        }
    }

    let init_count = Arc::new(AtomicUsize::new(0));
    let init_count_for_closure = Arc::clone(&init_count);
    let module = Module::new("Platform Systems", |ctx| {
        ctx.register_schema::<Thing>("things:list");
        ctx.register_view(HumanViewDef {
            schema_id: "things".to_owned(),
            columns: vec![
                TableColumn {
                    field: "name".to_owned(),
                    header: "Name".to_owned(),
                    no_truncate: false,
                },
                TableColumn {
                    field: "enabled".to_owned(),
                    header: "Enabled".to_owned(),
                    no_truncate: false,
                },
            ],
        });
        ctx.add_guide(GuideEntry {
            name: "deploy".to_owned(),
            summary: "Module guide".to_owned(),
            content: "module guide content".to_owned(),
        });
        ctx.add_guides_from_markdown([(
            "guides/operate.md",
            b"---\nsummary: Operate module\n---\nmodule operate content".as_slice(),
        )]);
        RuntimeGroupSpec::new(GroupSpec::new("things", "Manage things")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List things")
                    .no_auth(true)
                    .with_view_id("things"),
                async |_credential, _args| {
                    Ok(CommandResult::new(json!([
                            {"name": "alpha", "enabled": true, "ignored": "x"},
                            {"name": "beta", "enabled": false, "ignored": "y"}
                    ])))
                },
            ),
        )
    });
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        modules: vec![module],
        guides: vec![GuideEntry {
            name: "deploy".to_owned(),
            summary: "CLI guide".to_owned(),
            content: "cli guide content".to_owned(),
        }],
        init_deps: Some(Arc::new(move |middleware| {
            init_count_for_closure.fetch_add(1, Ordering::SeqCst);
            middleware.env = "prod".to_owned();
            Ok(())
        })),
        ..CliConfig::default()
    });

    let guide = cli.run(["my-cli", "guide", "deploy"]).await;
    assert_eq!(guide.exit_code, 0);
    assert_eq!(guide.rendered, "cli guide content");
    let embedded_guide = cli.run(["my-cli", "guide", "operate"]).await;
    assert_eq!(embedded_guide.exit_code, 0);
    assert_eq!(embedded_guide.rendered, "module operate content");
    assert_eq!(init_count.load(Ordering::SeqCst), 0);

    let schema = cli
        .run(["my-cli", "things", "list", "--schema", "--output", "json"])
        .await;
    assert_eq!(schema.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&schema.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "things:list");
    assert_eq!(init_count.load(Ordering::SeqCst), 0);

    let human = cli
        .run(["my-cli", "things", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0);
    assert_eq!(
        human.rendered,
        "NAME   ENABLED\n-----  -------\nalpha  yes    \nbeta   no     \n\n(2 rows)\n"
    );
    assert_eq!(init_count.load(Ordering::SeqCst), 1);

    let json_output = cli
        .run([
            "my-cli",
            "things",
            "list",
            "--output",
            "json",
            "--verbose",
            "env",
        ])
        .await;
    assert_eq!(json_output.exit_code, 0);
    let rendered: serde_json::Value =
        serde_json::from_str(&json_output.rendered).expect("valid json");
    assert_eq!(rendered["metadata"]["env"], "prod");
    assert_eq!(init_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_config_accepts_trait_based_command_modules() {
    #[derive(Debug)]
    struct TraitThing;

    impl OutputSchema for TraitThing {
        fn fields() -> &'static [OutputField] {
            &[OutputField {
                name: "name",
                field_type: "string",
                optional: false,
            }]
        }
    }

    #[derive(Debug)]
    struct TraitThingsModule {
        group_name: String,
    }

    impl CommandModule for TraitThingsModule {
        fn category(&self) -> String {
            "Platform Systems".to_owned()
        }

        fn guides(&self) -> Vec<GuideEntry> {
            vec![GuideEntry {
                name: "trait-things".to_owned(),
                summary: "Trait module guide".to_owned(),
                content: "trait module guide content".to_owned(),
            }]
        }

        fn views(&self) -> Vec<HumanViewDef> {
            vec![HumanViewDef {
                schema_id: "trait-things".to_owned(),
                columns: vec![TableColumn {
                    field: "name".to_owned(),
                    header: "Name".to_owned(),
                    no_truncate: false,
                }],
            }]
        }

        fn register(&self, context: &mut ModuleContext<'_>) -> RuntimeGroupSpec {
            context.register_schema::<TraitThing>("trait-things:list");
            RuntimeGroupSpec::new(GroupSpec::new(&self.group_name, "Manage trait things"))
                .with_command(RuntimeCommandSpec::new(
                    CommandSpec::new("list", "List trait things")
                        .no_auth(true)
                        .with_view_id("trait-things"),
                    async |_credential, _args| {
                        Ok(CommandResult::new(json!([
                                {"name": "alpha", "ignored": "x"},
                                {"name": "beta", "ignored": "y"}
                        ])))
                    },
                ))
        }
    }

    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        modules: vec![Module::from_command_module(TraitThingsModule {
            group_name: "trait-things".to_owned(),
        })],
        ..CliConfig::default()
    });

    let guide = cli.run(["my-cli", "guide", "trait-things"]).await;
    assert_eq!(guide.exit_code, 0);
    assert_eq!(guide.rendered, "trait module guide content");

    let schema = cli
        .run([
            "my-cli",
            "trait-things",
            "list",
            "--schema",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(schema.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&schema.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "trait-things:list");
    assert_eq!(rendered["data"]["fields"][0]["name"], "name");

    let human = cli
        .run(["my-cli", "trait-things", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0);
    assert_eq!(human.rendered, "NAME \n-----\nalpha\nbeta \n\n(2 rows)\n");
}

#[tokio::test]
async fn module_builder_accepts_embedded_markdown_guides() {
    let module = Module::new("Platform Systems", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("things", "Manage things")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List things").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!([]))),
            ),
        )
    })
    .with_guides_from_markdown([(
        "guides/team.md",
        b"---\nsummary: Team guide\n---\nteam guide content".as_slice(),
    )]);
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        modules: vec![module],
        ..CliConfig::default()
    });

    let guide = cli.run(["my-cli", "guide", "team"]).await;

    assert_eq!(guide.exit_code, 0);
    assert_eq!(guide.rendered, "team guide content");
}

#[tokio::test]
async fn module_context_registration_merges_guides_views_schemas_and_middleware_changes() {
    #[derive(Debug)]
    struct ContextThing;

    impl OutputSchema for ContextThing {
        fn fields() -> &'static [OutputField] {
            &[
                OutputField {
                    name: "name",
                    field_type: "string",
                    optional: false,
                },
                OutputField {
                    name: "enabled",
                    field_type: "bool",
                    optional: false,
                },
            ]
        }
    }

    #[derive(Debug, serde::Serialize, JsonSchema)]
    struct JsonThing {
        id: String,
        nested: JsonNested,
    }

    #[derive(Debug, serde::Serialize, JsonSchema)]
    struct JsonNested {
        owner: String,
    }

    let module = Module::new("Platform Systems", |context| {
        assert_eq!(context.middleware().app_id, "my-cli");
        context.middleware_mut().debug = "module-debug".to_owned();
        context.register_schema::<ContextThing>("context:list");
        context.register_json_schema::<JsonThing>("context:json-schema");
        context.register_view(HumanViewDef::new(
            "context",
            vec![TableColumn::new("name", "Name")],
        ));
        context.add_guide(GuideEntry::new(
            "context",
            "Context guide",
            "context guide body",
        ));
        context.add_guides_from_markdown([(
            "guides/context-extra.md",
            b"---\nsummary: Extra\n---\nextra guide body".as_slice(),
        )]);
        RuntimeGroupSpec::new(GroupSpec::new("context", "Context commands"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List context things")
                    .no_auth(true)
                    .with_view_id("context"),
                async |_credential, _args| {
                    Ok(CommandResult::new(json!([
                            {"name": "alpha", "enabled": true},
                            {"name": "beta", "enabled": false}
                    ])))
                },
            ))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("json-schema", "Show JSON schema").no_auth(true),
                async |_credential, _args| {
                    Ok(CommandResult::new(
                        json!({"id": "one", "nested": {"owner": "platform"}}),
                    ))
                },
            ))
    });
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        modules: vec![module],
        ..CliConfig::default()
    });

    assert_eq!(cli.middleware().debug, "module-debug");

    let guide = cli.run(["my-cli", "guide", "context"]).await;
    assert_eq!(guide.exit_code, 0);
    assert_eq!(guide.rendered, "context guide body");

    let extra_guide = cli.run(["my-cli", "guide", "context-extra"]).await;
    assert_eq!(extra_guide.exit_code, 0);
    assert_eq!(extra_guide.rendered, "extra guide body");

    let schema = cli
        .run(["my-cli", "context", "list", "--schema", "--output", "json"])
        .await;
    assert_eq!(schema.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&schema.rendered).expect("valid json");
    assert_eq!(rendered["data"]["fields"][0]["name"], "name");
    assert_eq!(rendered["data"]["fields"][1]["name"], "enabled");

    let json_schema = cli
        .run([
            "my-cli",
            "context",
            "json-schema",
            "--schema",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(json_schema.exit_code, 0);
    let rendered: serde_json::Value =
        serde_json::from_str(&json_schema.rendered).expect("valid json");
    assert_eq!(rendered["data"]["fields"][0]["name"], "id");
    assert_eq!(rendered["data"]["fields"][1]["type"], "object");

    let human = cli
        .run(["my-cli", "context", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0);
    assert_eq!(human.rendered, "NAME \n-----\nalpha\nbeta \n\n(2 rows)\n");
}

#[test]
fn module_builder_and_trait_defaults_cover_debug_and_default_contributions() {
    #[derive(Debug)]
    struct EmptyModule;

    impl CommandModule for EmptyModule {
        fn category(&self) -> String {
            "Empty".to_owned()
        }

        fn register(&self, _context: &mut ModuleContext<'_>) -> RuntimeGroupSpec {
            RuntimeGroupSpec::new(GroupSpec::new("empty", "Empty commands"))
        }
    }

    assert!(EmptyModule.guides().is_empty());
    assert!(EmptyModule.views().is_empty());

    let module = Module::new("Platform Systems", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("things", "Manage things"))
    })
    .with_guide(GuideEntry::new("one", "One", "one body"))
    .with_view(HumanViewDef::new(
        "things",
        vec![TableColumn::new("name", "Name")],
    ));

    assert_eq!(module.guides[0].name, "one");
    assert_eq!(module.views[0].schema_id, "things");
    let debug = format!("{module:?}");
    assert!(debug.contains("Module"));
    assert!(debug.contains("Platform Systems"));

    let trait_module = Module::from_command_module(EmptyModule);
    assert_eq!(trait_module.category, "Empty");
    assert!(trait_module.guides.is_empty());
    assert!(trait_module.views.is_empty());
}

#[test]
fn build_module_group_materializes_the_real_command_tree_standalone() {
    let module = Module::new("Platform Systems", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("things", "Manage things"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List things")
                    .with_scopes(&["things:read"])
                    .no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({"ok": true}))),
            ))
            .with_group(RuntimeGroupSpec::new(GroupSpec::new("sub", "Sub things")))
    })
    .with_guide(GuideEntry::new("one", "One", "one body"));

    let group = build_module_group(&module);

    assert_eq!(group.group.name, "things");
    let list = group
        .commands
        .iter()
        .find(|command| command.spec.name == "list")
        .expect("list command should be present");
    assert_eq!(list.spec.metadata().scopes, vec!["things:read"]);
    assert_eq!(group.groups.len(), 1);
    assert_eq!(group.groups[0].group.name, "sub");
}

#[tokio::test]
async fn cli_seeds_schema_and_human_views_from_global_registries() {
    #[derive(Debug)]
    struct GlobalThing;

    impl OutputSchema for GlobalThing {
        fn fields() -> &'static [OutputField] {
            &[
                OutputField {
                    name: "name",
                    field_type: "string",
                    optional: false,
                },
                OutputField {
                    name: "enabled",
                    field_type: "bool",
                    optional: false,
                },
            ]
        }
    }

    register_global_schema::<GlobalThing>("global-things:list");
    register_global_human_view(HumanViewDef {
        schema_id: "global-things".to_owned(),
        columns: vec![
            TableColumn {
                field: "name".to_owned(),
                header: "Name".to_owned(),
                no_truncate: false,
            },
            TableColumn {
                field: "enabled".to_owned(),
                header: "Enabled".to_owned(),
                no_truncate: false,
            },
        ],
    });
    let global_schema =
        cli_engine::get_global_schema_by_path("global-things:list").expect("global schema");
    assert_eq!(global_schema.command, "global-things:list");
    assert_eq!(global_schema.fields[0].name, "name");
    let global_columns =
        cli_engine::lookup_global_human_view_columns("global-things").expect("global columns");
    assert_eq!(global_columns[0].field, "name");

    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("global-things", "Manage global things"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List global things")
                    .no_auth(true)
                    .with_view_id("global-things"),
                async |_credential, _args| {
                    Ok(CommandResult::new(json!([
                            {"name": "alpha", "enabled": true, "ignored": "x"},
                            {"name": "beta", "enabled": false, "ignored": "y"}
                    ])))
                },
            )),
    );

    let schema = cli
        .run([
            "my-cli",
            "global-things",
            "list",
            "--schema",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(schema.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&schema.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "global-things:list");
    assert_eq!(
        rendered["data"]["fields"],
        json!([
            {"name": "name", "type": "string", "optional": false},
            {"name": "enabled", "type": "bool", "optional": false}
        ])
    );

    let help = cli.run(["my-cli", "help", "global-things", "list"]).await;
    assert_eq!(help.exit_code, 0);
    assert!(help.rendered.contains("Output fields:"));
    assert!(help.rendered.contains("name     string"));
    assert!(help.rendered.contains("enabled  bool"));

    let human = cli
        .run(["my-cli", "global-things", "list", "--output", "human"])
        .await;
    assert_eq!(human.exit_code, 0);
    assert_eq!(
        human.rendered,
        "NAME   ENABLED\n-----  -------\nalpha  yes    \nbeta   no     \n\n(2 rows)\n"
    );
}

#[tokio::test]
async fn command_spec_output_schema_registers_schema_and_help_when_mounted() {
    #[derive(Debug)]
    struct DeclarativeThing;

    impl OutputSchema for DeclarativeThing {
        fn fields() -> &'static [OutputField] {
            &[
                OutputField {
                    name: "name",
                    field_type: "string",
                    optional: false,
                },
                OutputField {
                    name: "count",
                    field_type: "int",
                    optional: true,
                },
            ]
        }
    }

    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("decl-things", "Manage declarative things"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List declarative things")
                    .no_auth(true)
                    .with_output_schema::<DeclarativeThing>(),
                async |_credential, _args| {
                    Ok(CommandResult::new(json!([
                            {"name": "alpha", "count": 2},
                            {"name": "beta", "count": 3}
                    ])))
                },
            )),
    );

    let schema = cli
        .run([
            "my-cli",
            "decl-things",
            "list",
            "--schema",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(schema.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&schema.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "decl-things:list");
    assert_eq!(
        rendered["data"]["fields"],
        json!([
            {"name": "name", "type": "string", "optional": false},
            {"name": "count", "type": "int", "optional": true}
        ])
    );

    let help = cli.run(["my-cli", "help", "decl-things", "list"]).await;
    assert_eq!(help.exit_code, 0);
    assert!(help.rendered.contains("Output fields:"));
    assert!(help.rendered.contains("name   string"));
    assert!(help.rendered.contains("count  int  (optional)"));
}

#[tokio::test]
async fn command_spec_can_publish_rust_native_json_schema_with_field_summary() {
    #[derive(Debug, serde::Serialize, JsonSchema)]
    struct NativeThing {
        name: String,
        count: i64,
        enabled: bool,
        tags: Vec<String>,
        owner: Option<String>,
    }

    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("native-things", "Manage native things"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List native things")
                    .no_auth(true)
                    .with_json_schema::<NativeThing>(),
                async |_credential, _args| {
                    Ok(CommandResult::new(json!([{
                            "name": "alpha",
                            "count": 2,
                            "enabled": true,
                            "tags": ["prod"],
                            "owner": null
                    }])))
                },
            )),
    );

    let schema = cli
        .run([
            "my-cli",
            "native-things",
            "list",
            "--schema",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(schema.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&schema.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "native-things:list");
    assert_eq!(rendered["data"]["schema"]["title"], "NativeThing");
    assert_eq!(
        rendered["data"]["schema"]["properties"]["name"]["type"],
        "string"
    );
    assert_eq!(
        rendered["data"]["fields"],
        json!([
            {"name": "count", "type": "int", "optional": false},
            {"name": "enabled", "type": "bool", "optional": false},
            {"name": "name", "type": "string", "optional": false},
            {"name": "owner", "type": "string", "optional": true},
            {"name": "tags", "type": "[]string", "optional": false}
        ])
    );
}

#[tokio::test]
async fn cli_config_extension_hooks_support_custom_flags_search_and_shutdown() {
    let shutdown_count = Arc::new(AtomicUsize::new(0));
    let shutdown_for_closure = Arc::clone(&shutdown_count);
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        register_flags: Some(Arc::new(|command: Command| {
            command.arg(
                Arg::new("env")
                    .long("env")
                    .global(true)
                    .value_name("ENV")
                    .help("Target environment"),
            )
        })),
        apply_flags: Some(Arc::new(|matches, middleware| {
            if let Some(env) = matches.get_one::<String>("env") {
                middleware.env = env.clone();
            }
            Ok(())
        })),
        on_shutdown: Some(Arc::new(move || {
            shutdown_for_closure.fetch_add(1, Ordering::SeqCst);
        })),
        extra_search_docs: Some(Arc::new(|| {
            vec![SearchDocument {
                id: "kb:network".to_owned(),
                kind: "kb".to_owned(),
                title: "kb network".to_owned(),
                summary: "Network playbook".to_owned(),
                content: "network peering routes".to_owned(),
            }]
        })),
        commands: vec![RuntimeCommandSpec::new(
            CommandSpec::new("whoami", "Show execution context").no_auth(true),
            async |_credential, _args| Ok(CommandResult::new(json!({"ok": true}))),
        )],
        ..CliConfig::default()
    });

    let search = cli
        .run(["my-cli", "--search", "peering", "--output", "json"])
        .await;
    assert_eq!(search.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&search.rendered).expect("valid json");
    assert_eq!(rendered["data"][0]["command"], "kb network");
    assert_eq!(shutdown_count.load(Ordering::SeqCst), 0);

    let command = cli
        .run([
            "my-cli",
            "whoami",
            "--env",
            "prod",
            "--verbose",
            "env",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(command.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&command.rendered).expect("valid json");
    assert_eq!(rendered["metadata"]["env"], "prod");
    assert_eq!(shutdown_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_config_pre_run_runs_after_init_for_real_commands_only() {
    let init_count = Arc::new(AtomicUsize::new(0));
    let pre_run_count = Arc::new(AtomicUsize::new(0));
    let init_count_for_closure = Arc::clone(&init_count);
    let pre_run_count_for_closure = Arc::clone(&pre_run_count);
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        init_deps: Some(Arc::new(move |middleware| {
            init_count_for_closure.fetch_add(1, Ordering::SeqCst);
            middleware.env = "init-env".to_owned();
            Ok(())
        })),
        pre_run: Some(Arc::new(move |middleware, command_path, args| {
            pre_run_count_for_closure.fetch_add(1, Ordering::SeqCst);
            assert_eq!(command_path, "whoami");
            assert_eq!(args.get("name"), Some(&json!("tester")));
            assert_eq!(middleware.env, "init-env");
            middleware.reason = "pre-run reason".to_owned();
            Ok(())
        })),
        commands: vec![RuntimeCommandSpec::new(
            CommandSpec::new("whoami", "Show execution context")
                .no_auth(true)
                .with_flag(Arg::new("name").long("name").default_value("tester")),
            async |_credential, _args| Ok(CommandResult::new(json!({"ok": true}))),
        )],
        ..CliConfig::default()
    });

    let search = cli
        .run(["my-cli", "--search", "whoami", "--output", "json"])
        .await;
    assert_eq!(search.exit_code, 0);
    assert_eq!(init_count.load(Ordering::SeqCst), 0);
    assert_eq!(pre_run_count.load(Ordering::SeqCst), 0);

    let output = cli
        .run([
            "my-cli",
            "whoami",
            "--output",
            "json",
            "--verbose",
            "env,effective_args",
        ])
        .await;
    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["metadata"]["env"], "init-env");
    assert_eq!(rendered["metadata"]["effective_args"]["name"], "tester");
    assert_eq!(init_count.load(Ordering::SeqCst), 1);
    assert_eq!(pre_run_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_config_init_deps_failure_preserves_structured_error_and_exit_code() {
    let init_count = Arc::new(AtomicUsize::new(0));
    let init_count_for_closure = Arc::clone(&init_count);
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        init_deps: Some(Arc::new(move |_middleware| {
            init_count_for_closure.fetch_add(1, Ordering::SeqCst);
            Err(cli_engine::CliCoreError::with_exit_code(
                42,
                cli_engine::CliCoreError::with_detailed_error(CustomDetailedError {
                    message: "policy denied initialization",
                    code: "POLICY_DENIED",
                    system: Some("policy-api"),
                    request_id: Some("req-init"),
                }),
            ))
        })),
        commands: vec![RuntimeCommandSpec::new(
            CommandSpec::new("whoami", "Show execution context").no_auth(true),
            async |_credential, _args| Ok(CommandResult::new(json!({"ok": true}))),
        )],
        ..CliConfig::default()
    });

    let first = cli
        .run(["my-cli", "whoami", "--output", "json", "--verbose", "all"])
        .await;
    assert_eq!(first.exit_code, 42);
    let parsed: serde_json::Value = serde_json::from_str(&first.rendered).expect("valid json");
    assert_eq!(parsed["error"]["code"], "POLICY_DENIED");
    assert_eq!(parsed["error"]["message"], "policy denied initialization");
    assert_eq!(parsed["error"]["system"], "policy-api");
    assert_eq!(parsed["error"]["request_id"], "req-init");
    assert_eq!(parsed["metadata"]["system"], "policy-api");
    assert_eq!(parsed["metadata"]["request_id"], "req-init");

    let second = cli
        .run(["my-cli", "whoami", "--output", "json", "--verbose", "all"])
        .await;
    assert_eq!(second.exit_code, 42);
    assert_eq!(second.rendered, first.rendered);
    assert_eq!(init_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_config_pre_run_runs_for_builtins_without_init_deps_preserves_legacy() {
    let init_count = Arc::new(AtomicUsize::new(0));
    let init_count_for_closure = Arc::clone(&init_count);
    let calls = Arc::new(StdMutex::new(Vec::<(String, serde_json::Value)>::new()));
    let calls_for_closure = Arc::clone(&calls);
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        init_deps: Some(Arc::new(move |middleware| {
            init_count_for_closure.fetch_add(1, Ordering::SeqCst);
            middleware.env = "init-env".to_owned();
            Ok(())
        })),
        pre_run: Some(Arc::new(move |_middleware, command_path, args| {
            calls_for_closure.lock().expect("calls lock").push((
                command_path.to_owned(),
                serde_json::to_value(args).expect("args should serialize"),
            ));
            Ok(())
        })),
        ..CliConfig::default()
    });
    cli.add_guides([GuideEntry {
        name: "deploy".to_owned(),
        summary: "Deploy safely".to_owned(),
        content: "# Deploy\n".to_owned(),
    }]);

    let help = cli.run(["my-cli", "help", "guide"]).await;
    assert_eq!(help.exit_code, 0);
    let tree = cli.run(["my-cli", "tree", "--output", "json"]).await;
    assert_eq!(tree.exit_code, 0);
    let guide = cli.run(["my-cli", "guide", "deploy"]).await;
    assert_eq!(guide.exit_code, 0);

    assert_eq!(init_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        calls.lock().expect("calls lock").as_slice(),
        &[
            ("help".to_owned(), json!({"command": "guide"})),
            ("tree".to_owned(), json!({})),
            ("guide".to_owned(), json!({"topic": "deploy"})),
        ]
    );
}

#[tokio::test]
async fn cli_config_meta_resolver_can_adjust_command_metadata() {
    let authorized_tiers = Arc::new(StdMutex::new(Vec::new()));
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        authz: Some(Arc::new(RecordingAuthorizer {
            tiers: Arc::clone(&authorized_tiers),
        })),
        auth_providers: vec![
            Arc::new(FakeProvider::new("primary", "default-user")),
            Arc::new(FakeProvider::new("oauth", "resolved-user")),
        ],
        meta_resolver: Some(Arc::new(|command_path, mut meta: CommandMeta| {
            assert_eq!(command_path, "whoami");
            meta.auth_metadata
                .insert("provider".to_owned(), "oauth".to_owned());
            meta.auth_metadata
                .insert("tier".to_owned(), "destructive".to_owned());
            meta
        })),
        commands: vec![RuntimeCommandSpec::new_with_context(
            CommandSpec::new("whoami", "Show execution context"),
            async |context| {
                let credential = context
                    .credential()
                    .await
                    .expect("credential should resolve");
                Ok(CommandResult::new(json!({
                        "identity": credential.identity,
                        "tier": context.middleware.reason
                })))
            },
        )],
        ..CliConfig::default()
    });

    let output = cli
        .run([
            "my-cli", "whoami", "--reason", "ticket-1", "--output", "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["identity"], "resolved-user");
    assert_eq!(rendered["data"]["tier"], "ticket-1");
    assert_eq!(
        authorized_tiers.lock().expect("tiers lock").as_slice(),
        &[Tier::Destructive]
    );
}

#[tokio::test]
async fn cli_runtime_guide_command_errors_with_valid_topics() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_guides([GuideEntry {
        name: "deploy".to_owned(),
        summary: "Deploy safely".to_owned(),
        content: "# Deploy\n".to_owned(),
    }]);

    let output = cli.run(["my-cli", "guide", "missing"]).await;

    assert_eq!(output.exit_code, 1);
    assert_eq!(
        output.rendered,
        "unknown guide topic \"missing\" — valid topics: deploy"
    );
}

#[tokio::test]
async fn cli_runtime_guide_command_rejects_extra_args_preserves_parser_maximum_one_arg() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_guides([GuideEntry {
        name: "deploy".to_owned(),
        summary: "Deploy safely".to_owned(),
        content: "# Deploy\n".to_owned(),
    }]);

    let output = cli.run(["my-cli", "guide", "deploy", "extra"]).await;

    assert_ne!(output.exit_code, 0);
    assert!(output.rendered.contains("extra"));
}

#[tokio::test]
async fn cli_runtime_search_bypasses_required_command_flags() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects")
                    .no_auth(true)
                    .with_flag(Arg::new("project").long("project").required(true)),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let output = cli
        .run([
            "my-cli", "project", "list", "--search", "project", "--output", "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"][0]["command"], "project list");
    assert_eq!(rendered["metadata"], serde_json::Value::Null);
}

#[tokio::test]
async fn cli_runtime_search_scope_resolves_group_aliases_preserves_legacy() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects").with_alias("p"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            )),
    );
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("noise", "Noise")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("find", "Find projects elsewhere").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let output = cli
        .run(["my-cli", "p", "--search", "projects", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!([{
            "command": "project list",
            "snippet": "List projects",
            "confidence": std::f64::consts::FRAC_1_SQRT_2
        }])
    );
}

#[tokio::test]
async fn cli_runtime_search_scope_preserves_legacy_no_opt_flag_consumption_quirk() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("noise", "Noise")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("find", "Find projects elsewhere").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let output = cli
        .run([
            "my-cli",
            "--verbose",
            "project",
            "--search",
            "projects",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    let commands = rendered["data"]
        .as_array()
        .expect("search results should be an array")
        .iter()
        .map(|result| result["command"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(
        commands.contains(&"noise find"),
        "Legacy scope resolution treats --verbose as consuming project before --search, so search falls back to root scope; got {commands:?}"
    );
}

#[tokio::test]
async fn cli_runtime_search_indexes_group_command_and_flag_aliases() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects").with_alias("portfolio"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects")
                    .with_alias("inventory")
                    .with_arg(
                        Arg::new("project")
                            .long("project")
                            .alias("domain")
                            .short_alias('d'),
                    )
                    .no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            )),
    );

    for query in ["portfolio", "inventory", "domain"] {
        let output = cli
            .run(["my-cli", "--search", query, "--output", "json"])
            .await;
        assert_eq!(output.exit_code, 0);
        let rendered: serde_json::Value =
            serde_json::from_str(&output.rendered).expect("valid json");
        assert_eq!(rendered["data"][0]["command"], "project list");
        assert_eq!(rendered["data"][0]["snippet"], "List projects");
    }
}

#[tokio::test]
async fn cli_runtime_hidden_commands_run_but_stay_out_of_discovery() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("internal", "Internal maintenance command")
            .hidden(true)
            .no_auth(true),
        async |_credential, _args| Ok(CommandResult::new(json!({"ok": true}))),
    ));

    let command = cli.run(["my-cli", "internal", "--output", "json"]).await;
    assert_eq!(command.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&command.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!({"ok": true}));

    let search = cli
        .run(["my-cli", "--search", "internal", "--output", "json"])
        .await;
    assert_eq!(search.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&search.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!([]));

    let tree = cli.run(["my-cli", "tree", "--output", "json"]).await;
    assert_eq!(tree.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&tree.rendered).expect("valid json");
    let children = rendered["data"]["children"]
        .as_array()
        .expect("tree children");
    assert!(
        !children
            .iter()
            .any(|child| child["name"] == serde_json::Value::String("internal".to_owned()))
    );

    let root_help = cli.run(["my-cli", "help"]).await;
    assert_eq!(root_help.exit_code, 0);
    assert!(!root_help.rendered.contains("Internal maintenance command"));
    assert!(!root_help.rendered.contains("internal"));

    let command_help = cli.run(["my-cli", "help", "internal"]).await;
    assert_eq!(command_help.exit_code, 0);
    assert!(
        command_help
            .rendered
            .contains("Internal maintenance command")
    );
}

#[tokio::test]
async fn cli_runtime_hidden_groups_run_but_hide_their_subtree_from_discovery() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("internal", "Internal tools").hidden(true))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("repair", "Repair internal state").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({"repaired": true}))),
            )),
    );

    let command = cli
        .run(["my-cli", "internal", "repair", "--output", "json"])
        .await;
    assert_eq!(command.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&command.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!({"repaired": true}));

    let search = cli
        .run(["my-cli", "--search", "repair", "--output", "json"])
        .await;
    assert_eq!(search.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&search.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!([]));

    let tree = cli.run(["my-cli", "tree", "--output", "json"]).await;
    assert_eq!(tree.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&tree.rendered).expect("valid json");
    let children = rendered["data"]["children"]
        .as_array()
        .expect("tree children");
    assert!(
        !children
            .iter()
            .any(|child| child["name"] == serde_json::Value::String("internal".to_owned()))
    );

    let root_help = cli.run(["my-cli", "help"]).await;
    assert_eq!(root_help.exit_code, 0);
    assert!(!root_help.rendered.contains("Internal tools"));
    assert!(!root_help.rendered.contains("internal"));

    let group_help = cli.run(["my-cli", "help", "internal"]).await;
    assert_eq!(group_help.exit_code, 0);
    assert!(group_help.rendered.contains("Internal tools"));

    let command_help = cli.run(["my-cli", "help", "internal", "repair"]).await;
    assert_eq!(command_help.exit_code, 0);
    assert!(command_help.rendered.contains("Repair internal state"));
}

#[tokio::test]
async fn cli_runtime_help_resolves_group_and_command_aliases() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects").with_alias("p"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects")
                    .with_alias("ls")
                    .no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            )),
    );

    let output = cli.run(["my-cli", "help", "p", "ls"]).await;

    assert_eq!(output.exit_code, 0);
    assert!(output.rendered.contains("List projects"));
    assert!(!output.rendered.contains("unknown command"));
}

#[tokio::test]
async fn cli_runtime_search_includes_guides_at_root_scope() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_guides([GuideEntry {
        name: "deploy".to_owned(),
        summary: "Deploy safely".to_owned(),
        content: "release rollout checklist".to_owned(),
    }]);

    let output = cli
        .run(["my-cli", "--search", "rollout", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"][0]["command"], "guide deploy");
    assert_eq!(rendered["data"][0]["snippet"], "Deploy safely");
}

#[tokio::test]
async fn cli_runtime_accepts_negative_limit_preserves_legacy_int_flag() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("list", "List things").no_auth(true),
        async |_credential, _args| {
            Ok(CommandResult::new(json!([
                    {"name": "alpha"},
                    {"name": "beta"},
                    {"name": "gamma"}
            ])))
        },
    ));

    let output = cli
        .run([
            "my-cli",
            "list",
            "--offset",
            "1",
            "--limit",
            "-1",
            "--verbose",
            "pagination",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!([
            {"name": "beta"},
            {"name": "gamma"}
        ])
    );
    assert_eq!(
        rendered["metadata"]["pagination"],
        json!({"total": 3, "offset": 1, "limit": -1, "count": 2})
    );
}

#[tokio::test]
async fn cli_runtime_auth_login_uses_registered_provider_default() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.register_auth_provider(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));
    cli.register_auth_provider(Arc::new(FakeProvider {
        name: "oauth".to_owned(),
        identity: "oauth-user".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let output = cli
        .run([
            "my-cli", "auth", "login", "--env", "prod", "--output", "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({
            "provider": "primary",
            "env": "prod",
            "identity": "tester",
            "expires_at": "2099-01-01T00:00:00Z"
        })
    );

    let login = cli
        .root_command()
        .find_subcommand("auth")
        .and_then(|auth| auth.find_subcommand("login"))
        .expect("auth login command should be registered");
    let provider_arg = login
        .get_arguments()
        .find(|arg| arg.get_id() == "provider")
        .expect("provider flag should be registered");
    assert_eq!(
        provider_arg.get_default_values(),
        &[std::ffi::OsStr::new("primary")]
    );
    assert!(
        provider_arg
            .get_help()
            .expect("provider help")
            .to_string()
            .contains("one of: [primary, oauth]")
    );
}

#[tokio::test]
async fn cli_runtime_auth_login_uses_middleware_env_when_env_flag_omitted() {
    let cli = auth_cli_with_default_env("dev");

    let output = cli
        .run(["my-cli", "auth", "login", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({
            "provider": "primary",
            "env": "dev",
            "identity": "tester",
            "expires_at": "2099-01-01T00:00:00Z"
        })
    );
}

#[tokio::test]
async fn cli_runtime_auth_login_env_flag_overrides_middleware_env() {
    let cli = auth_cli_with_default_env("dev");

    let output = cli
        .run([
            "my-cli", "auth", "login", "--env", "prod", "--output", "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["env"], "prod");
}

#[tokio::test]
async fn cli_runtime_auth_login_empty_env_flag_errors_instead_of_using_middleware_env() {
    let cli = auth_cli_with_default_env("dev");

    let output = cli
        .run(["my-cli", "auth", "login", "--env", "", "--output", "json"])
        .await;

    assert_ne!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["error"]["message"],
        "auth: missing environment; pass --env or configure a default environment"
    );
}

#[tokio::test]
async fn cli_runtime_auth_login_errors_when_env_missing() {
    let cli = auth_cli_without_default_env();

    let output = cli
        .run(["my-cli", "auth", "login", "--output", "json"])
        .await;

    assert_ne!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["error"]["message"],
        "auth: missing environment; pass --env or configure a default environment"
    );
}

#[tokio::test]
async fn cli_runtime_auth_logout_uses_middleware_env_when_env_flag_omitted() {
    let cli = auth_cli_with_default_env("dev");

    let output = cli
        .run(["my-cli", "auth", "logout", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({"provider": "primary", "env": "dev", "status": "logged out"})
    );
}

#[tokio::test]
async fn cli_runtime_auth_logout_env_flag_overrides_middleware_env() {
    let cli = auth_cli_with_default_env("dev");

    let output = cli
        .run([
            "my-cli", "auth", "logout", "--env", "prod", "--output", "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({"provider": "primary", "env": "prod", "status": "logged out"})
    );
}

#[tokio::test]
async fn cli_runtime_auth_commands_use_init_deps_registered_providers() {
    let init_count = Arc::new(AtomicUsize::new(0));
    let init_count_for_closure = Arc::clone(&init_count);
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        init_deps: Some(Arc::new(move |middleware| {
            init_count_for_closure.fetch_add(1, Ordering::SeqCst);
            middleware.auth.register(Arc::new(FakeProvider {
                name: "primary".to_owned(),
                identity: "init-user".to_owned(),
                logout_fails: false,
                environments: vec!["prod".to_owned()],
            }));
            Ok(())
        })),
        ..CliConfig::default()
    });

    let output = cli
        .run([
            "my-cli", "auth", "login", "--env", "prod", "--output", "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["identity"], "init-user");
    assert_eq!(rendered["data"]["provider"], "primary");
    assert_eq!(rendered["data"]["env"], "prod");
    assert_eq!(init_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_runtime_auth_status_and_logout_render_legacy_shapes() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        ..CliConfig::default()
    });
    cli.register_auth_provider(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let status = cli
        .run([
            "my-cli",
            "auth",
            "status",
            "--provider",
            "primary",
            "--env",
            "prod",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(status.exit_code, 0);
    let status_json: serde_json::Value =
        serde_json::from_str(&status.rendered).expect("valid json");
    assert_eq!(
        status_json["data"],
        json!({
            "provider": "primary",
            "env": "prod",
            "identity": "tester",
            "expires_at": "2099-01-01T00:00:00Z",
            "scopes": [],
            "expired": false
        })
    );

    let logout = cli
        .run([
            "my-cli", "auth", "logout", "--env", "prod", "--output", "json",
        ])
        .await;
    assert_eq!(logout.exit_code, 0);
    let logout_json: serde_json::Value =
        serde_json::from_str(&logout.rendered).expect("valid json");
    assert_eq!(
        logout_json["data"],
        json!({"provider": "primary", "env": "prod", "status": "logged out"})
    );
}

#[tokio::test]
async fn cli_runtime_auth_commands_preserve_user_and_effective_args_preserves_legacy_cmd_build() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        ..CliConfig::default()
    });
    cli.register_auth_provider(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let implicit_provider = cli
        .run([
            "my-cli",
            "auth",
            "login",
            "--env",
            "prod",
            "--verbose",
            "args,effective_args",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(implicit_provider.exit_code, 0);
    let rendered: serde_json::Value =
        serde_json::from_str(&implicit_provider.rendered).expect("valid json");
    assert_eq!(rendered["metadata"]["args"], json!({"env": "prod"}));
    assert_eq!(
        rendered["metadata"]["effective_args"],
        json!({"provider": "primary", "env": "prod"})
    );

    let explicit_provider = cli
        .run([
            "my-cli",
            "auth",
            "status",
            "--provider",
            "primary",
            "--env",
            "prod",
            "--verbose",
            "args,effective_args",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(explicit_provider.exit_code, 0);
    let rendered: serde_json::Value =
        serde_json::from_str(&explicit_provider.rendered).expect("valid json");
    assert_eq!(
        rendered["metadata"]["args"],
        json!({"provider": "primary", "env": "prod"})
    );
    assert_eq!(
        rendered["metadata"]["effective_args"],
        json!({"provider": "primary", "env": "prod"})
    );
}

#[tokio::test]
async fn cli_runtime_schema_bypasses_required_command_flags() {
    #[derive(Debug)]
    struct Thing;

    impl OutputSchema for Thing {
        fn fields() -> &'static [OutputField] {
            &[OutputField {
                name: "name",
                field_type: "string",
                optional: false,
            }]
        }
    }

    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    let mut registry = SchemaRegistry::new();
    registry.register::<Thing>("things:list");
    cli.middleware_mut().schema_registry = registry;
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("things", "Manage things")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List things")
                    .no_auth(true)
                    .with_flag(Arg::new("project").long("project").required(true)),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            ),
        ),
    );

    let output = cli
        .run(["my-cli", "things", "list", "--schema", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "things:list");
    assert_eq!(rendered["data"]["fields"][0]["name"], "name");
    assert_eq!(rendered["metadata"], serde_json::Value::Null);

    let output = cli
        .run([
            "my-cli",
            "things",
            "list",
            "--schema=true",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "things:list");

    let output = cli
        .run(["my-cli", "things", "list", "--schema=1", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "things:list");
}

#[tokio::test]
async fn cli_runtime_accepts_explicit_bool_values_for_global_flags() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("mutate", "Mutate safely")
            .no_auth(true)
            .mutates(true),
        async |_credential, _args| Ok(CommandResult::new(json!({"executed": true}))),
    ));

    let output = cli
        .run(["my-cli", "mutate", "--dry-run=false", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!({"executed": true}));
    assert_eq!(rendered["metadata"], serde_json::Value::Null);

    let output = cli
        .run(["my-cli", "mutate", "--dry-run=0", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!({"executed": true}));
}

#[tokio::test]
async fn cli_runtime_optional_value_flags_before_command_do_not_consume_command_token_like_optional_flag_parser()
 {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("mutate", "Mutate safely")
            .no_auth(true)
            .mutates(true),
        async |_credential, _args| Ok(CommandResult::new(json!({"executed": true}))),
    ));

    let output = cli
        .run(["my-cli", "--dry-run", "mutate", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({"command": "mutate", "action": "dry-run: would execute"})
    );

    let output = cli
        .run([
            "my-cli",
            "--verbose",
            "mutate",
            "--dry-run",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({"command": "mutate", "action": "dry-run: would execute"})
    );
    assert!(rendered["metadata"].is_object());

    cli.add_module_group(
        "Projects",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects")
                    .no_auth(true)
                    .mutates(true),
                async |_credential, _args| Ok(CommandResult::new(json!({"executed": true}))),
            ),
        ),
    );

    let output = cli
        .run(["my-cli", "project", "--dry-run", "list", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({"command": "project:list", "action": "dry-run: would execute"})
    );
}

#[tokio::test]
async fn cli_runtime_optional_string_flags_use_no_opt_default_and_do_not_consume_positionals_like_optional_flag_parser()
 {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("mutate", "Mutate safely").no_auth(true),
        async |context| {
            Ok(CommandResult::new(
                json!({"ran": "mutate", "verbose": context.middleware.verbose}),
            ))
        },
    ));
    cli.add_module_group(
        "Projects",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new_with_context(
                CommandSpec::new("list", "List projects").no_auth(true),
                async |context| {
                    Ok(CommandResult::new(
                        json!({"ran": "project:list", "debug": context.middleware.debug}),
                    ))
                },
            ),
        ),
    );

    let output = cli
        .run(["my-cli", "--verbose", "list", "mutate", "--output", "json"])
        .await;

    assert_ne!(output.exit_code, 0);
    assert_eq!(output.rendered, "unknown command \"list\" for \"my-cli\"");

    let output = cli
        .run(["my-cli", "--verbose=list", "mutate", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({"ran": "mutate", "verbose": "list"})
    );

    let output = cli
        .run([
            "my-cli",
            "project",
            "--debug=mutate",
            "list",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({"ran": "project:list", "debug": "mutate"})
    );
}

#[tokio::test]
async fn cli_runtime_schema_bypass_resolves_group_and_command_aliases() {
    #[derive(Debug)]
    struct Project;

    impl OutputSchema for Project {
        fn fields() -> &'static [OutputField] {
            &[OutputField {
                name: "name",
                field_type: "string",
                optional: false,
            }]
        }
    }

    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    let mut registry = SchemaRegistry::new();
    registry.register::<Project>("project:list");
    cli.middleware_mut().schema_registry = registry;
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects").with_alias("p"))
            .with_command(RuntimeCommandSpec::new(
                CommandSpec::new("list", "List projects")
                    .with_alias("ls")
                    .no_auth(true)
                    .with_flag(Arg::new("project").long("project").required(true)),
                async |_credential, _args| Ok(CommandResult::new(json!({}))),
            )),
    );

    let output = cli
        .run(["my-cli", "p", "ls", "--schema", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "project:list");
    assert_eq!(rendered["data"]["fields"][0]["name"], "name");
}

#[tokio::test]
async fn cli_runtime_applies_global_output_pipeline_flags() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Platform Systems",
        RuntimeGroupSpec::new(GroupSpec::new("things", "Manage things")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List things").no_auth(true),
                async |_credential, _args| {
                    Ok(CommandResult::new(json!([
                            {"name": "alpha", "status": "inactive", "enabled": false, "extra": "drop"},
                            {"name": "beta", "status": "active", "enabled": true, "extra": "drop"},
                            {"name": "gamma", "status": "active", "enabled": true, "extra": "drop"}
                    ])))
                },
            ),
        ),
    );

    let output = cli
        .run([
            "my-cli",
            "things",
            "list",
            "--filter",
            "status == 'active'",
            "--offset",
            "1",
            "--limit",
            "1",
            "--fields",
            "name,enabled",
            "--verbose",
            "pagination",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!([{"name": "gamma", "enabled": true}])
    );
    assert_eq!(
        rendered["metadata"]["pagination"],
        json!({"total": 2, "offset": 1, "limit": 1, "count": 1})
    );
}

#[tokio::test]
async fn command_spec_system_and_default_fields_builders_drive_runtime_output() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("things", "List things")
            .no_auth(true)
            .with_system("things-api")
            .with_default_fields("name"),
        async |_credential, _args| {
            Ok(CommandResult::new(json!([
                    {"name": "alpha", "ignored": "x"},
                    {"name": "beta", "ignored": "y"}
            ])))
        },
    ));

    let output = cli
        .run([
            "my-cli",
            "things",
            "--output",
            "json",
            "--verbose",
            "system",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["metadata"]["system"], "things-api");
    assert_eq!(
        rendered["data"],
        json!([
            {"name": "alpha"},
            {"name": "beta"}
        ])
    );
}

#[tokio::test]
async fn runtime_command_context_exposes_args_user_args_path_and_middleware() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("whoami", "Show context")
            .no_auth(true)
            .with_flag(Arg::new("name").long("name").default_value("anon"))
            .with_flag(Arg::new("region").long("region").default_value("us-west-2")),
        async |context: CommandContext| {
            Ok(CommandResult::new(json!({
                    "command_path": context.command_path,
                    "name": context.args["name"],
                    "region": context.args["region"],
                    "user_name": context.user_args["name"],
                    "user_region_present": context.user_args.get("region").is_some(),
                    "output": context.middleware.output_format,
                    "debug": context.middleware.debug,
                    "timeout_present": context.middleware.timeout.is_some(),
                    "app": context.middleware.app_id,
                    "credential_present": context
                        .try_credential()
                        .await
                        .expect("try_credential")
                        .is_some()
            })))
        },
    ));

    let output = cli
        .run([
            "my-cli", "whoami", "--name", "tester", "--output", "json", "--debug",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!({
            "command_path": "whoami",
            "name": "tester",
            "region": "us-west-2",
            "user_name": "tester",
            "user_region_present": false,
            "output": "json",
            "debug": "*",
            "timeout_present": false,
            "app": "my-cli",
            "credential_present": false
        })
    );
}

#[tokio::test]
async fn cli_runtime_applies_global_expr_before_fields() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("things", "List things").no_auth(true),
        async |_credential, _args| {
            Ok(CommandResult::new(json!([
                    {"name": "alpha", "enabled": false},
                    {"name": "beta", "enabled": true}
            ])))
        },
    ));

    let output = cli
        .run(["my-cli", "things", "--expr", "[].name", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!(["alpha", "beta"]));
}

#[tokio::test]
async fn cli_runtime_timeout_bounds_command_execution() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("slow", "Slow command").no_auth(true),
        async |_credential, _args| {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(CommandResult::new(json!({"status": "done"})))
        },
    ));

    let output = cli
        .run(["my-cli", "slow", "--timeout", "1ms", "--output", "json"])
        .await;

    assert_ne!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["error"]["message"], "command timed out after 1ms");
}

#[tokio::test]
async fn cli_runtime_timeout_zero_disables_deadline() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("slow", "Slow command").no_auth(true),
        async |_credential, _args| {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(CommandResult::new(json!({"status": "done"})))
        },
    ));

    let output = cli
        .run(["my-cli", "slow", "--timeout", "0s", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!({"status": "done"}));
}

#[tokio::test]
async fn cli_runtime_negative_timeout_disables_deadline_preserves_legacy() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("slow", "Slow command").no_auth(true),
        async |context| {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(CommandResult::new(
                json!({"timeout_present": context.middleware.timeout.is_some()}),
            ))
        },
    ));

    let output = cli
        .run(["my-cli", "slow", "--timeout", "-1s", "--output", "json"])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!({"timeout_present": false}));
}

#[test]
fn root_long_groups_modules_and_builtin_command_hints() {
    let rendered = build_root_long(
        "Intro",
        &[
            ModuleHelpEntry {
                category: "Platform Systems".to_owned(),
                name: "release".to_owned(),
                short: "Deploy apps".to_owned(),
            },
            ModuleHelpEntry {
                category: "Platform Systems".to_owned(),
                name: "settings".to_owned(),
                short: "Manage settings".to_owned(),
            },
        ],
        true,
    );

    assert!(rendered.contains("  Platform Systems:"));
    assert!(rendered.contains("release"));
    assert!(rendered.contains("Deploy apps"));
    assert!(rendered.contains("    settings  Manage settings"));
    assert!(rendered.contains("--search <keyword>"));
    assert!(rendered.contains("guide               Built-in guides"));
}

#[test]
fn command_spec_metadata_matches_legacy_annotation_resolver_behavior() {
    let spec = CommandSpec::new("deploy", "Deploy")
        .with_auth_provider("oauth")
        .with_tier(Tier::Mutate)
        .with_auth_metadata("scopes", "read:apps write:apps");

    let meta = spec.metadata();

    assert!(meta.dry_run_prompt);
    assert_eq!(meta.auth_metadata["provider"], "oauth");
    assert_eq!(meta.auth_metadata["tier"], "mutate");
    assert_eq!(meta.scopes, vec!["read:apps", "write:apps"]);
}

#[test]
fn command_spec_with_scopes_round_trips_through_metadata() {
    let spec = CommandSpec::new("get", "Get").with_scopes(&["commerce.business:read", "x:y"]);

    let meta = spec.metadata();

    assert_eq!(meta.auth_metadata["scopes"], "commerce.business:read x:y");
    assert_eq!(meta.scopes, vec!["commerce.business:read", "x:y"]);
}

#[test]
fn command_spec_metadata_leaves_provider_unset_by_default() {
    let spec = CommandSpec::new("list", "List");

    let meta = spec.metadata();

    assert!(!meta.auth_metadata.contains_key("provider"));
    assert!(!meta.dry_run_prompt);
}

#[test]
fn command_spec_metadata_preserves_empty_provider_metadata() {
    let spec = CommandSpec::new("list", "List").with_auth_metadata("provider", "");

    let meta = spec.metadata();

    assert_eq!(meta.auth_metadata["provider"], "");
}

#[tokio::test]
async fn cli_runtime_uses_cli_default_provider_when_command_provider_is_unset() {
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("device".to_owned()),
        auth_providers: vec![
            Arc::new(FakeProvider::new("device", "device-user")),
            Arc::new(FakeProvider::new("primary", "primary-user")),
        ],
        commands: vec![RuntimeCommandSpec::new_with_context(
            CommandSpec::new("whoami", "Show execution context"),
            async |context| {
                let credential = context
                    .credential()
                    .await
                    .expect("credential should resolve");
                Ok(CommandResult::new(json!({"identity": credential.identity})))
            },
        )],
        ..CliConfig::default()
    });

    let output = cli.run(["my-cli", "whoami", "--output", "json"]).await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["identity"], "device-user");
}

#[tokio::test]
async fn cli_runtime_middleware_auth_errors_render_once_and_exit_nonzero() {
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        commands: vec![RuntimeCommandSpec::new_with_context(
            CommandSpec::new("secure", "Run secure command"),
            // Under lazy resolution the auth flow only runs when the handler
            // asks for the credential, so request it to surface the missing
            // provider error.
            async |context| {
                context.credential().await?;
                Ok(CommandResult::new(json!({"ok": true})))
            },
        )],
        ..CliConfig::default()
    });

    let output = cli
        .run(["my-cli", "secure", "--output", "json", "--verbose=all"])
        .await;

    assert_eq!(output.exit_code, 2);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["error"]["code"], "ERROR");
    assert!(
        rendered["error"]["message"]
            .as_str()
            .expect("message")
            .contains("auth: no provider registered")
    );
}

#[tokio::test]
async fn cli_runtime_middleware_business_errors_render_once_and_exit_nonzero() {
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        auth_providers: vec![Arc::new(FakeProvider::new("primary", "tester"))],
        commands: vec![RuntimeCommandSpec::new_with_context(
            CommandSpec::new("secure", "Run secure command"),
            async |_context| {
                Err::<CommandResult, _>(cli_engine::CliCoreError::message_for_system(
                    "secure-api",
                    "backend rejected request",
                ))
            },
        )],
        ..CliConfig::default()
    });

    let output = cli
        .run(["my-cli", "secure", "--output", "json", "--verbose=all"])
        .await;

    assert_eq!(output.exit_code, 1);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["error"]["message"], "backend rejected request");
    assert_eq!(rendered["error"]["system"], "secure-api");
}

#[tokio::test]
async fn cli_runtime_business_errors_use_command_system_preserves_legacy_cmd_wrapper() {
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        commands: vec![RuntimeCommandSpec::new(
            CommandSpec::new("secure", "Run secure command")
                .no_auth(true)
                .with_system("secure-api"),
            async |_credential, _args| {
                Err::<CommandResult, _>(cli_engine::CliCoreError::message("backend rejected"))
            },
        )],
        ..CliConfig::default()
    });

    let output = cli
        .run(["my-cli", "secure", "--output", "json", "--verbose=all"])
        .await;

    assert_eq!(output.exit_code, 1);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["error"]["message"], "backend rejected");
    assert_eq!(rendered["error"]["system"], "secure-api");
    assert_eq!(rendered["metadata"]["system"], "secure-api");
}

#[tokio::test]
async fn cli_runtime_business_errors_default_system_to_top_level_path_preserves_legacy_cmd_wrapper()
{
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_module_group(
        "Projects",
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("create", "Create project").no_auth(true),
                async |_credential, _args| {
                    Err::<CommandResult, _>(cli_engine::CliCoreError::message("backend rejected"))
                },
            ),
        ),
    );

    let output = cli
        .run([
            "my-cli",
            "project",
            "create",
            "--output",
            "json",
            "--verbose=all",
        ])
        .await;

    assert_eq!(output.exit_code, 1);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["error"]["message"], "backend rejected");
    assert_eq!(rendered["error"]["system"], "project");
    assert_eq!(rendered["metadata"]["system"], "project");
}

#[test]
fn group_spec_builds_clap_subcommand_tree_and_command_path() {
    let group = GroupSpec::new("release", "Deploy commands")
        .with_long("Long deploy command documentation")
        .with_alias("k")
        .with_command(
            CommandSpec::new("deploy", "Deploy apps")
                .with_alias("push")
                .with_flag(Arg::new("project").long("project").required(true)),
        );
    let command = Command::new("my-cli").subcommand(group.clap_command());

    let matches = command
        .try_get_matches_from(["my-cli", "k", "push", "--project", "p1"])
        .expect("command should parse");

    assert_eq!(
        cli_engine::command_path_from_matches("my-cli", &matches),
        "release:deploy"
    );

    assert_eq!(
        cli_engine::command_path_from_parts(empty_path_parts(), None),
        "",
        "nil/no command path mirrors legacy behavior empty result"
    );
    assert_eq!(
        cli_engine::command_path_from_parts(&["my-cli", "release", "deploy"], None),
        "release:deploy",
        "tree paths strip the root command"
    );
    assert_eq!(
        cli_engine::command_path_from_parts(&["deploy"], Some("release:deploy")),
        "release:deploy",
        "isolated commands prefer explicit path annotation"
    );
    assert_eq!(
        cli_engine::command_path_from_parts(&["deploy"], None),
        "deploy",
        "isolated unannotated commands fall back to leaf name"
    );
}

#[test]
fn command_spec_with_arg_supports_positional_and_option_args() {
    let spec = CommandSpec::new("get", "Get project")
        .with_arg(Arg::new("project").required(true))
        .with_arg(Arg::new("format").long("format").default_value("summary"));
    let command = Command::new("my-cli").subcommand(spec.clap_command());
    let matches = command
        .try_get_matches_from(["my-cli", "get", "p1", "--format", "full"])
        .expect("command should parse");
    let leaf = cli_engine::leaf_matches(&matches);

    let effective = cli_engine::command_args_from_matches(leaf, &spec, false);
    let user = cli_engine::command_args_from_matches(leaf, &spec, true);

    assert_eq!(effective["project"], json!("p1"));
    assert_eq!(effective["format"], json!("full"));
    assert_eq!(user["project"], json!("p1"));
    assert_eq!(user["format"], json!("full"));
}

#[test]
fn command_args_from_matches_covers_typed_defaults_changed_only_and_repetition() {
    let spec = CommandSpec::new("typed", "Typed args")
        .with_arg(Arg::new("name").required(true))
        .with_arg(
            Arg::new("i8")
                .long("i8")
                .value_parser(value_parser!(i8))
                .default_value("-8"),
        )
        .with_arg(
            Arg::new("i16")
                .long("i16")
                .value_parser(value_parser!(i16))
                .default_value("-16"),
        )
        .with_arg(
            Arg::new("i32")
                .long("i32")
                .value_parser(value_parser!(i32))
                .default_value("-32"),
        )
        .with_arg(
            Arg::new("u8")
                .long("u8")
                .value_parser(value_parser!(u8))
                .default_value("8"),
        )
        .with_arg(
            Arg::new("u16")
                .long("u16")
                .value_parser(value_parser!(u16))
                .default_value("16"),
        )
        .with_arg(
            Arg::new("u32")
                .long("u32")
                .value_parser(value_parser!(u32))
                .default_value("32"),
        )
        .with_arg(
            Arg::new("usize")
                .long("usize")
                .value_parser(value_parser!(usize))
                .default_value("64"),
        )
        .with_arg(
            Arg::new("f32")
                .long("f32")
                .value_parser(value_parser!(f32))
                .default_value("1.25"),
        )
        .with_arg(
            Arg::new("disabled")
                .long("disabled")
                .action(ArgAction::SetFalse),
        )
        .with_arg(
            Arg::new("label")
                .long("label")
                .action(ArgAction::Append)
                .value_parser(value_parser!(String)),
        );
    let matches = spec
        .clap_command()
        .try_get_matches_from([
            "typed",
            "project-1",
            "--i8=-7",
            "--u8",
            "9",
            "--usize",
            "128",
            "--f32",
            "2.5",
            "--disabled",
            "--label",
            "blue",
            "--label",
            "green",
        ])
        .expect("typed args should parse");

    let effective = cli_engine::command_args_from_matches(&matches, &spec, false);
    assert_eq!(
        effective,
        value_map([
            ("name", json!("project-1")),
            ("i8", json!(-7)),
            ("i16", json!(-16)),
            ("i32", json!(-32)),
            ("u8", json!(9)),
            ("u16", json!(16)),
            ("u32", json!(32)),
            ("usize", json!(128)),
            ("f32", json!(2.5)),
            ("disabled", json!(false)),
            ("label", json!(["blue", "green"])),
        ])
    );

    let user = cli_engine::command_args_from_matches(&matches, &spec, true);
    assert_eq!(
        user,
        value_map([
            ("name", json!("project-1")),
            ("i8", json!(-7)),
            ("u8", json!(9)),
            ("usize", json!(128)),
            ("f32", json!(2.5)),
            ("disabled", json!(false)),
            ("label", json!(["blue", "green"])),
        ])
    );
}

#[test]
fn runtime_group_builder_covers_nested_groups_long_help_aliases_and_hidden_flags() {
    let nested = RuntimeGroupSpec::new(
        GroupSpec::new("nested", "Nested commands")
            .with_long("Nested long help")
            .with_alias("n")
            .hidden(true),
    )
    .with_command(RuntimeCommandSpec::new(
        CommandSpec::new("leaf", "Leaf command")
            .with_long("Leaf long help")
            .hidden(true)
            .no_auth(true),
        async |_credential, _args| Ok(CommandResult::new(json!({"ok": true}))),
    ));
    let group = RuntimeGroupSpec::new(
        GroupSpec::new("root-group", "Root group")
            .with_long("Root long help")
            .with_alias("rg"),
    )
    .with_group(nested);
    let command = group.clap_command();

    assert_eq!(command.get_name(), "root-group");
    assert_eq!(
        command.get_about().map(ToString::to_string).as_deref(),
        Some("Root group")
    );
    assert_eq!(
        command.get_long_about().map(ToString::to_string).as_deref(),
        Some("Root long help")
    );
    assert!(
        command
            .get_subcommands()
            .any(|subcommand| subcommand.get_name() == "nested" && subcommand.is_hide_set())
    );

    let parser = Command::new("my-cli").subcommand(command);
    let matches = parser
        .try_get_matches_from(["my-cli", "rg", "n", "leaf"])
        .expect("aliases should parse");
    assert_eq!(
        cli_engine::command_path_from_matches("my-cli", &matches),
        "root-group:nested:leaf"
    );
}

#[tokio::test]
async fn cli_runtime_command_args_preserve_common_clap_value_types() {
    let cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        commands: vec![RuntimeCommandSpec::new_with_context(
            CommandSpec::new("scale", "Scale things")
                .no_auth(true)
                .with_arg(
                    Arg::new("count")
                        .long("count")
                        .value_parser(value_parser!(i64))
                        .default_value("2"),
                )
                .with_arg(
                    Arg::new("ratio")
                        .long("ratio")
                        .value_parser(value_parser!(f64))
                        .default_value("1.5"),
                )
                .with_arg(
                    Arg::new("enabled")
                        .long("enabled")
                        .action(ArgAction::SetTrue),
                )
                .with_arg(
                    Arg::new("tag")
                        .long("tag")
                        .action(ArgAction::Append)
                        .value_parser(value_parser!(String)),
                )
                .with_arg(
                    Arg::new("level")
                        .short('v')
                        .long("verbose-count")
                        .action(ArgAction::Count),
                ),
            async |context| {
                Ok(CommandResult::new(json!({
                        "args": context.args,
                        "user_args": context.user_args,
                })))
            },
        )],
        ..CliConfig::default()
    });

    let output = cli
        .run([
            "my-cli",
            "scale",
            "--count",
            "5",
            "--ratio",
            "2.25",
            "--enabled",
            "--tag",
            "blue",
            "--tag",
            "green",
            "-vv",
            "--output",
            "json",
        ])
        .await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"]["args"],
        json!({
            "count": 5,
            "ratio": 2.25,
            "enabled": true,
            "tag": ["blue", "green"],
            "level": 2
        })
    );
    assert_eq!(
        rendered["data"]["user_args"],
        json!({
            "count": 5,
            "ratio": 2.25,
            "enabled": true,
            "tag": ["blue", "green"],
            "level": 2
        })
    );

    let output = cli.run(["my-cli", "scale", "--output", "json"]).await;

    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"]["args"],
        json!({
            "count": 2,
            "ratio": 1.5,
            "enabled": false,
            "level": 0
        })
    );
    assert_eq!(rendered["data"]["user_args"], json!({}));
}

#[test]
fn raw_search_and_output_extraction_matches_legacy_bypass_helpers() {
    assert_eq!(
        extract_search_query(&["my-cli", "release", "--search", "promote"]),
        "promote"
    );
    assert_eq!(
        extract_search_query(&["my-cli", "--search=deploy"]),
        "deploy"
    );
    assert_eq!(extract_search_query(&["my-cli", "--search"]), "");

    assert_eq!(
        extract_output_format(&["my-cli", "-o", "json", "--search", "foo"], "json"),
        "json"
    );
    assert_eq!(
        extract_output_format(&["my-cli", "--output=human", "--search", "foo"], "json"),
        "human"
    );
    assert_eq!(
        extract_output_format(&["my-cli", "--output"], "json"),
        "json"
    );
    assert_eq!(
        extract_output_format(&["my-cli", "--search", "foo"], "json"),
        "json"
    );
    assert_eq!(
        extract_output_format(&["my-cli", "project", "list", "--json"], "json"),
        "json"
    );
    assert_eq!(
        extract_output_format(&["my-cli", "project", "list", "--toon"], "json"),
        "toon"
    );
    assert_eq!(
        extract_output_format(&["my-cli", "--toon", "project", "list"], "json"),
        "toon"
    );
    assert_eq!(
        extract_output_format(&["my-cli", "project", "list", "--human"], "json"),
        "human"
    );

    // When no format token is present, the supplied default is returned (a
    // non-"json" default makes this distinct from a hardcoded fallback). A bare
    // `--output` with no value behaves the same way.
    assert_eq!(extract_output_format(&["my-cli"], "human"), "human");
    assert_eq!(
        extract_output_format(&["my-cli", "--search", "foo"], "toon"),
        "toon"
    );
    assert_eq!(
        extract_output_format(&["my-cli", "--output"], "human"),
        "human"
    );
    // An explicit format still wins over the default.
    assert_eq!(
        extract_output_format(&["my-cli", "--human"], "json"),
        "human"
    );

    assert!(has_true_schema_flag(&["my-cli", "release", "--schema"]));
    assert!(has_true_schema_flag(&[
        "my-cli",
        "release",
        "--schema=true"
    ]));
    assert!(has_true_schema_flag(&["my-cli", "release", "--schema=1"]));
    assert!(has_true_schema_flag(&["my-cli", "release", "--schema=T"]));
    assert!(!has_true_schema_flag(&[
        "my-cli",
        "release",
        "--schema=false"
    ]));
    assert!(!has_true_schema_flag(&["my-cli", "release", "--schema=0"]));
}

#[test]
fn global_flag_defaults_and_derived_flag_classes_cover_common_clap_actions() {
    assert_eq!(
        cli_engine::GlobalFlags::default(),
        cli_engine::GlobalFlags {
            output_format: "json".to_owned(),
            verbose: String::new(),
            dry_run: false,
            fields: String::new(),
            filter: String::new(),
            expr: String::new(),
            limit: 0,
            offset: 0,
            schema: false,
            reason: String::new(),
            timeout: "0s".to_owned(),
            debug: String::new(),
            search: String::new(),
            credential_store: None,
        }
    );

    let command = Command::new("my-cli")
        .arg(
            Arg::new("disable-cache")
                .long("disable-cache")
                .action(ArgAction::SetFalse),
        )
        .arg(Arg::new("trace").short('t').action(ArgAction::Count))
        .arg(Arg::new("name").long("name").action(ArgAction::Append))
        .arg(Arg::new("optional").long("optional").num_args(0..=1))
        .subcommand(
            Command::new("child").arg(
                Arg::new("version")
                    .long("version")
                    .action(ArgAction::Version),
            ),
        );

    let bool_flags = derive_bool_flags(&command);
    let value_flags = derive_value_flags(&command);

    assert!(bool_flags.contains("--disable-cache"));
    assert!(bool_flags.contains("-t"));
    assert!(bool_flags.contains("--version"));
    assert!(bool_flags.contains("--optional"));
    assert!(value_flags.contains("--name"));
    assert!(!value_flags.contains("--optional"));
}

#[test]
fn schema_command_path_extraction_skips_bool_and_value_flags() {
    let command = register_reason_flag(register_global_flags(Command::new("my-cli")));
    let bool_flags = derive_bool_flags(&command);
    let value_flags = derive_value_flags(&command);

    assert!(bool_flags.contains("--schema"));
    assert!(bool_flags.contains("--verbose"));
    assert!(bool_flags.contains("--debug"));
    assert!(value_flags.contains("--output"));
    assert!(value_flags.contains("--reason"));
    assert_eq!(
        extract_command_path(
            &[
                "my-cli",
                "release",
                "--verbose",
                "--output",
                "json",
                "deploy",
                "--schema",
                "--limit",
                "10",
            ],
            &bool_flags,
            &value_flags,
        ),
        "release:deploy"
    );
}

#[tokio::test]
async fn reason_flag_is_registered_only_when_authz_auditor_or_activity_is_configured() {
    let bare = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"))
        .run(["my-cli", "tree", "--reason", "test"])
        .await;
    // clap's usage/parse-error exit code (unknown argument, missing required
    // arg, etc.), distinct from a command's own runtime-error exit code — this
    // pins the assertion to "unknown argument" specifically, not any failure.
    assert_eq!(
        bare.exit_code, 2,
        "no authz/auditor/activity configured, so --reason should be an unknown argument: {}",
        bare.rendered
    );

    let authorized_tiers = Arc::new(StdMutex::new(Vec::new()));
    let with_authz = Cli::new(CliConfig {
        authz: Some(Arc::new(RecordingAuthorizer {
            tiers: Arc::clone(&authorized_tiers),
        })),
        ..CliConfig::new("my-cli", "Dev tooling", "my-cli")
    })
    .run(["my-cli", "tree", "--reason", "test"])
    .await;
    assert_eq!(
        with_authz.exit_code, 0,
        "authz is configured, so --reason should parse: {}",
        with_authz.rendered
    );
}

#[test]
fn schema_command_path_extraction_uses_recursive_command_flags() {
    let command = register_global_flags(Command::new("my-cli"))
        .arg(Arg::new("profile").long("profile"))
        .subcommand(Command::new("release").subcommand(
            Command::new("deploy").arg(Arg::new("force").long("force").action(ArgAction::SetTrue)),
        ));
    let bool_flags = derive_bool_flags(&command);
    let value_flags = derive_value_flags(&command);

    assert!(bool_flags.contains("--force"));
    assert!(value_flags.contains("--profile"));
    assert_eq!(
        extract_command_path(
            &[
                "my-cli",
                "--profile",
                "prod",
                "release",
                "--force",
                "deploy",
                "--schema",
            ],
            &bool_flags,
            &value_flags,
        ),
        "release:deploy"
    );
}

#[test]
fn credential_expiry_prefers_cached_at() {
    let credential = Credential {
        cached_at: "2026-05-18T10:00:00Z".to_owned(),
        expires_at: "2099-01-01T00:00:00Z".to_owned(),
        ..Credential::default()
    };

    assert_eq!(credential.effective_expiry(), "2026-05-18T10:30:00Z");
}

#[test]
fn credential_with_invalid_expires_at_is_expired() {
    let credential = Credential {
        expires_at: "not-a-time".to_owned(),
        ..Credential::default()
    };

    assert!(credential.is_expired());
}

#[test]
fn credential_cached_at_drives_effective_expiry_and_expiration_status() {
    let fresh_cached_at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let stale_cached_at = (chrono::Utc::now() - chrono::Duration::minutes(31))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let fresh = Credential {
        cached_at: fresh_cached_at,
        expires_at: "2000-01-01T00:00:00Z".to_owned(),
        ..Credential::default()
    };
    assert!(!fresh.is_expired());

    let stale = Credential {
        cached_at: stale_cached_at,
        expires_at: "2099-01-01T00:00:00Z".to_owned(),
        ..Credential::default()
    };
    assert!(stale.is_expired());

    let explicit = Credential {
        expires_at: "2099-01-01T00:00:00Z".to_owned(),
        ..Credential::default()
    };
    assert_eq!(explicit.effective_expiry(), "2099-01-01T00:00:00Z");
}

#[test]
fn credential_without_any_expiry_is_not_expired_for_back_compat() {
    let credential = Credential::default();

    assert!(!credential.is_expired());
}

#[tokio::test]
async fn dispatcher_preserves_registration_order_and_replaces_provider() {
    let mut dispatcher = Dispatcher::new();
    dispatcher.register(Arc::new(FakeProvider::new("primary", "first")));
    dispatcher.register(Arc::new(FakeProvider::new("oauth", "second")));
    dispatcher.register(Arc::new(FakeProvider::new("primary", "replacement")));

    assert_eq!(dispatcher.registered_names(), vec!["primary", "oauth"]);

    let credential = dispatcher
        .get_credential("primary", "prod", "setting:list", "read")
        .await
        .expect("registered fake provider should return a credential");
    assert_eq!(credential.identity, "replacement");
}

#[tokio::test]
async fn dispatcher_login_ignores_logout_error() {
    let mut dispatcher = Dispatcher::new();
    dispatcher.register(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: true,
        environments: vec![],
    }));

    let credential = dispatcher
        .login("primary", "prod")
        .await
        .expect("login should ignore pre-auth logout errors");

    assert_eq!(credential.identity, "tester");
}

#[tokio::test]
async fn dispatcher_for_provider_facade_matches_legacy_single_provider_behavior() {
    let mut dispatcher = Dispatcher::new();
    dispatcher.register(Arc::new(FakeProvider::new("primary", "tester")));
    let provider: cli_engine::SingleProvider = dispatcher.for_provider("primary");

    assert_eq!(provider.name(), "primary");
    let credential = provider
        .get_credential("prod", "things:list", "read")
        .await
        .expect("single provider should return credential");
    assert_eq!(credential.identity, "tester");
    assert_eq!(credential.env, "prod");
}

#[tokio::test]
async fn dispatcher_for_provider_facade_reflects_later_registration_and_replacement() {
    let mut dispatcher = Dispatcher::new();
    let provider: cli_engine::SingleProvider = dispatcher.for_provider("primary");

    dispatcher.register(Arc::new(FakeProvider::new("primary", "tester")));
    let credential = provider
        .get_credential("prod", "things:list", "read")
        .await
        .expect("single provider should observe late provider registration");
    assert_eq!(credential.identity, "tester");
    assert_eq!(credential.env, "prod");

    dispatcher.register(Arc::new(FakeProvider::new("primary", "replacement")));
    let credential = provider
        .get_credential("staging", "things:list", "read")
        .await
        .expect("single provider should observe provider replacement");
    assert_eq!(credential.identity, "replacement");
    assert_eq!(credential.env, "staging");
}

#[tokio::test]
async fn dispatcher_all_statuses_skips_list_environment_errors() {
    let mut dispatcher = Dispatcher::new();
    dispatcher.register(Arc::new(FakeProvider {
        name: "broken".to_owned(),
        identity: "broken".to_owned(),
        logout_fails: false,
        environments: vec!["__error__".to_owned()],
    }));
    dispatcher.register(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let statuses = dispatcher.all_statuses().await;

    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].provider, "primary");
    assert_eq!(statuses[0].env, "prod");
}

#[tokio::test]
async fn auth_command_helpers_match_login_status_and_logout_shapes() {
    let mut dispatcher = Dispatcher::new();
    dispatcher.register(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let login = login_and_build(&dispatcher, "primary", "prod")
        .await
        .expect("login result should build");
    assert_eq!(login.provider, "primary");
    assert_eq!(login.env, "prod");
    assert_eq!(login.identity, "tester");
    assert_eq!(login.expires_at, "2099-01-01T00:00:00Z");

    let status = status_result(&dispatcher, "primary", "prod")
        .await
        .expect("single status should render");
    assert_eq!(
        status,
        json!({
            "provider": "primary",
            "env": "prod",
            "identity": "tester",
            "expires_at": "2099-01-01T00:00:00Z",
            "scopes": [],
            "expired": false
        })
    );

    let all_status = status_result(&dispatcher, "", "")
        .await
        .expect("all status should render");
    assert_eq!(
        all_status,
        json!([{
            "provider": "primary",
            "env": "prod",
            "identity": "tester",
            "expires_at": "2099-01-01T00:00:00Z",
            "scopes": [],
            "expired": false
        }])
    );

    let logout = logout_result(&dispatcher, "primary", "prod")
        .await
        .expect("logout should render");
    assert_eq!(
        logout,
        json!({"provider": "primary", "env": "prod", "status": "logged out"})
    );
}

#[test]
fn auth_status_entry_treats_missing_credential_as_expired() {
    let entry = to_status_entry("primary", "prod", None);

    assert_eq!(entry.provider, "primary");
    assert_eq!(entry.env, "prod");
    assert!(entry.expired);
}

#[test]
fn auth_command_group_sets_provider_defaults() {
    let group = auth_command_group(
        "oauth",
        &[
            "primary".to_owned(),
            "oauth".to_owned(),
            "device".to_owned(),
        ],
    );
    let login = group
        .commands
        .iter()
        .find(|command| command.spec.name == "login")
        .expect("login subcommand should exist");
    let provider_arg = login
        .spec
        .args
        .iter()
        .find(|arg| arg.get_id() == "provider")
        .expect("provider flag should exist");

    assert_eq!(
        provider_arg.get_default_values(),
        &[std::ffi::OsStr::new("oauth")]
    );
    assert!(
        provider_arg
            .get_help()
            .expect("provider help")
            .to_string()
            .contains("one of: [primary, oauth, device]")
    );

    for command_name in ["login", "logout"] {
        let command = group
            .commands
            .iter()
            .find(|command| command.spec.name == command_name)
            .expect("auth subcommand should exist");
        assert!(
            command
                .spec
                .args
                .iter()
                .any(|arg| arg.get_id() == "env" && !arg.is_required_set())
        );
    }
}

#[tokio::test]
async fn auth_extra_commands_are_mounted_as_siblings_without_losing_builtins() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        auth_extra_commands: vec![RuntimeCommandSpec::new(
            CommandSpec::new("scopes", "List requestable scopes").no_auth(true),
            async |_credential, _args| Ok(CommandResult::new(json!(["a:read", "b:write"]))),
        )],
        ..CliConfig::default()
    });
    cli.register_auth_provider(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let scopes = cli
        .run(["my-cli", "auth", "scopes", "--output", "json"])
        .await;
    assert_eq!(scopes.exit_code, 0, "{}", scopes.rendered);
    let scopes_json: serde_json::Value =
        serde_json::from_str(&scopes.rendered).expect("valid json");
    assert_eq!(scopes_json["data"], json!(["a:read", "b:write"]));

    let status = cli
        .run([
            "my-cli", "auth", "status", "--env", "prod", "--output", "json",
        ])
        .await;
    assert_eq!(status.exit_code, 0, "{}", status.rendered);
    let status_json: serde_json::Value =
        serde_json::from_str(&status.rendered).expect("valid json");
    assert_eq!(status_json["data"]["identity"], "tester");

    let logout = cli
        .run([
            "my-cli", "auth", "logout", "--env", "prod", "--output", "json",
        ])
        .await;
    assert_eq!(logout.exit_code, 0, "{}", logout.rendered);
}

#[tokio::test]
async fn auth_extra_commands_colliding_with_a_builtin_name_is_ignored() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        auth_extra_commands: vec![RuntimeCommandSpec::new(
            CommandSpec::new("status", "Impostor status command").no_auth(true),
            async |_credential, _args| Ok(CommandResult::new(json!("impostor"))),
        )],
        ..CliConfig::default()
    });
    cli.register_auth_provider(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let status = cli
        .run([
            "my-cli", "auth", "status", "--env", "prod", "--output", "json",
        ])
        .await;
    assert_eq!(status.exit_code, 0, "{}", status.rendered);
    let status_json: serde_json::Value =
        serde_json::from_str(&status.rendered).expect("valid json");
    assert_eq!(status_json["data"]["identity"], "tester");
}

#[tokio::test]
async fn auth_extra_commands_output_schema_registers_for_schema_flag() {
    #[derive(Debug)]
    struct ScopeThing;

    impl OutputSchema for ScopeThing {
        fn fields() -> &'static [OutputField] {
            &[OutputField {
                name: "scope",
                field_type: "string",
                optional: false,
            }]
        }
    }

    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        auth_extra_commands: vec![RuntimeCommandSpec::new(
            CommandSpec::new("scopes", "List requestable scopes")
                .no_auth(true)
                .with_output_schema::<ScopeThing>(),
            async |_credential, _args| Ok(CommandResult::new(json!(["a:read"]))),
        )],
        ..CliConfig::default()
    });
    cli.register_auth_provider(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let schema = cli
        .run(["my-cli", "auth", "scopes", "--schema", "--output", "json"])
        .await;
    assert_eq!(schema.exit_code, 0, "{}", schema.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&schema.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "auth:scopes");
    assert_eq!(
        rendered["data"]["fields"],
        json!([{"name": "scope", "type": "string", "optional": false}])
    );
}

#[tokio::test]
async fn auth_extra_commands_colliding_with_each_other_keeps_the_first() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        default_auth_provider: Some("primary".to_owned()),
        auth_extra_commands: vec![
            RuntimeCommandSpec::new(
                CommandSpec::new("scopes", "First scopes command").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!("first"))),
            ),
            RuntimeCommandSpec::new(
                CommandSpec::new("scopes", "Second scopes command").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!("second"))),
            ),
        ],
        ..CliConfig::default()
    });
    cli.register_auth_provider(Arc::new(FakeProvider {
        name: "primary".to_owned(),
        identity: "tester".to_owned(),
        logout_fails: false,
        environments: vec!["prod".to_owned()],
    }));

    let scopes = cli
        .run(["my-cli", "auth", "scopes", "--output", "json"])
        .await;
    assert_eq!(scopes.exit_code, 0, "{}", scopes.rendered);
    let scopes_json: serde_json::Value =
        serde_json::from_str(&scopes.rendered).expect("valid json");
    assert_eq!(scopes_json["data"], json!("first"));
}

fn auth_cli_with_default_env(env: &'static str) -> Cli {
    Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        register_flags: Some(Arc::new(move |command: Command| {
            command.arg(
                Arg::new("env")
                    .long("env")
                    .global(true)
                    .default_value(env)
                    .value_name("ENV")
                    .help("Target environment"),
            )
        })),
        apply_flags: Some(Arc::new(|matches, middleware| {
            if let Some(env) = matches.get_one::<String>("env") {
                middleware.env = env.clone();
            }
            Ok(())
        })),
        auth_providers: vec![Arc::new(FakeProvider {
            name: "primary".to_owned(),
            identity: "tester".to_owned(),
            logout_fails: false,
            environments: vec!["dev".to_owned(), "prod".to_owned()],
        })],
        ..CliConfig::default()
    })
}

fn auth_cli_without_default_env() -> Cli {
    Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        auth_providers: vec![Arc::new(FakeProvider {
            name: "primary".to_owned(),
            identity: "tester".to_owned(),
            logout_fails: false,
            environments: vec!["dev".to_owned(), "prod".to_owned()],
        })],
        ..CliConfig::default()
    })
}

#[test]
fn auth_command_group_defaults_to_first_registered_provider() {
    let group = auth_command_group(
        "",
        &[
            "primary".to_owned(),
            "oauth".to_owned(),
            "device".to_owned(),
        ],
    );
    let login = group
        .commands
        .iter()
        .find(|command| command.spec.name == "login")
        .expect("login subcommand should exist");
    let provider_arg = login
        .spec
        .args
        .iter()
        .find(|arg| arg.get_id() == "provider")
        .expect("provider flag should exist");

    assert_eq!(
        provider_arg.get_default_values(),
        &[std::ffi::OsStr::new("primary")]
    );
}

#[test]
fn authn_request_serializes_compat_fields() {
    let request = AuthnRequest {
        action: ACTION_AUTHENTICATE.to_owned(),
        provider: "primary".to_owned(),
        env: "prod".to_owned(),
        realm: "prod".to_owned(),
        command: "release:deploy:list".to_owned(),
        tier: "read".to_owned(),
    };

    let encoded = serde_json::to_value(request).expect("request should serialize");

    assert_eq!(
        encoded,
        json!({
            "action": "authenticate",
            "provider": "primary",
            "env": "prod",
            "realm": "prod",
            "command": "release:deploy:list",
            "tier": "read"
        })
    );
}

#[test]
fn auth_module_reexports_primary_auth_port_surfaces() {
    use cli_engine::auth::{
        ACTION_AUTHENTICATE as REEXPORTED_AUTHENTICATE, AuthLoginResult,
        AuthnRequest as ReexportedAuthnRequest, ExecProvider as ReexportedExecProvider,
        auth_command_group as reexported_auth_command_group,
    };

    assert_eq!(REEXPORTED_AUTHENTICATE, "authenticate");

    let request = ReexportedAuthnRequest {
        action: REEXPORTED_AUTHENTICATE.to_owned(),
        provider: "primary".to_owned(),
        env: "prod".to_owned(),
        realm: "prod".to_owned(),
        command: "project:list".to_owned(),
        tier: "read".to_owned(),
    };
    assert_eq!(request.provider, "primary");

    let provider = ReexportedExecProvider::new("primary", "authn-primary");
    assert_eq!(provider.name(), "primary");

    let auth_group = reexported_auth_command_group("primary", &["primary".to_owned()]);
    assert_eq!(auth_group.group.name, "auth");

    let login = AuthLoginResult {
        provider: "primary".to_owned(),
        env: "prod".to_owned(),
        identity: "tester".to_owned(),
        expires_at: "2030-01-01T00:00:00Z".to_owned(),
    };
    assert_eq!(login.identity, "tester");
}

#[test]
fn crate_root_reexports_auth_command_result_surfaces() {
    let login = cli_engine::AuthLoginResult {
        provider: "primary".to_owned(),
        env: "prod".to_owned(),
        identity: "tester".to_owned(),
        expires_at: "2030-01-01T00:00:00Z".to_owned(),
    };
    assert_eq!(login.provider, "primary");

    let status = cli_engine::AuthStatusEntry {
        provider: "primary".to_owned(),
        env: "prod".to_owned(),
        identity: "tester".to_owned(),
        expires_at: "2030-01-01T00:00:00Z".to_owned(),
        scopes: vec!["domains.domain:read".to_owned()],
        expired: false,
    };
    assert!(!status.expired);

    let group = auth_command_group("primary", &["primary".to_owned()]);
    assert_eq!(group.group.name, "auth");
}

#[tokio::test]
async fn exec_provider_sends_request_to_stdin_and_parses_credential() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let script = temp.path().join("provider.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
REQ="$(cat)"
case "$REQ" in
  *'"action":"authenticate"'*|*'"action": "authenticate"'*)
    printf '{"token":"abc","expires_at":"2099-01-01T00:00:00Z","identity":"tester"}'
    ;;
  *)
    echo "unexpected request: $REQ" >&2
    exit 2
    ;;
esac
"#,
    )
    .expect("script should be writable");
    make_executable(&script);

    let provider = ExecProvider::new("primary", &script).with_timeout(Duration::from_secs(10));
    let credential = provider
        .get_credential("prod", "setting:list", "read")
        .await
        .expect("provider script should return a credential");

    assert_eq!(credential.token, "abc");
    assert_eq!(credential.identity, "tester");
}

#[tokio::test]
async fn exec_provider_missing_credential_fields_decode_as_zero_values_preserves_legacy() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let script = temp.path().join("provider.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '{}'
"#,
    )
    .expect("script should be writable");
    make_executable(&script);

    let provider = ExecProvider::new("primary", &script).with_timeout(Duration::from_secs(10));
    let credential = provider
        .get_credential("prod", "setting:list", "read")
        .await
        .expect("missing credential fields should decode as empty strings");

    assert_eq!(credential.token, "");
    assert_eq!(credential.expires_at, "");
    assert_eq!(credential.identity, "");
}

#[tokio::test]
async fn exec_provider_lists_environments_and_falls_back_to_legacy_realms() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let script = temp.path().join("provider.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
REQ="$(cat)"
case "$REQ" in
  *'"action":"list-environments"'*|*'"action": "list-environments"'*)
    printf '{"environments":["dev","prod"]}'
    ;;
  *)
    echo "unexpected request: $REQ" >&2
    exit 2
    ;;
esac
"#,
    )
    .expect("script should be writable");
    make_executable(&script);

    let provider = ExecProvider::new("primary", &script).with_timeout(Duration::from_secs(10));
    let environments = provider
        .list_environments()
        .await
        .expect("provider should return environments");
    assert_eq!(environments, vec!["dev", "prod"]);

    let legacy_script = temp.path().join("legacy-provider.sh");
    std::fs::write(
        &legacy_script,
        r#"#!/bin/sh
REQ="$(cat)"
case "$REQ" in
  *'"action":"list-environments"'*|*'"action": "list-environments"'*)
    echo "old provider does not know list-environments" >&2
    exit 2
    ;;
  *'"action":"list-realms"'*|*'"action": "list-realms"'*)
    printf '{"realms":["staging","prod"]}'
    ;;
  *)
    echo "unexpected request: $REQ" >&2
    exit 3
    ;;
esac
"#,
    )
    .expect("legacy script should be writable");
    make_executable(&legacy_script);

    let legacy_provider =
        ExecProvider::new("primary", &legacy_script).with_timeout(Duration::from_secs(10));
    let environments = legacy_provider
        .list_environments()
        .await
        .expect("legacy realms fallback should return environments");
    assert_eq!(environments, vec!["staging", "prod"]);
}

#[tokio::test]
async fn exec_provider_environment_parse_errors_match_legacy_wrappers() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let invalid_script = temp.path().join("invalid-provider.sh");
    std::fs::write(
        &invalid_script,
        r#"#!/bin/sh
printf 'not-json'
"#,
    )
    .expect("invalid script should be writable");
    make_executable(&invalid_script);

    let provider =
        ExecProvider::new("primary", &invalid_script).with_timeout(Duration::from_secs(10));
    let err = provider
        .list_environments()
        .await
        .expect_err("invalid environment response should fail");
    assert!(
        err.to_string()
            .starts_with("auth: parse environments from ")
    );
    assert!(err.to_string().contains("invalid-provider.sh"));

    let empty_script = temp.path().join("empty-provider.sh");
    std::fs::write(
        &empty_script,
        r#"#!/bin/sh
printf '{"environments":[]}'
"#,
    )
    .expect("empty script should be writable");
    make_executable(&empty_script);

    let provider =
        ExecProvider::new("primary", &empty_script).with_timeout(Duration::from_secs(10));
    let environments = provider
        .list_environments()
        .await
        .expect("empty environments fall through to empty legacy realms like legacy behavior");
    assert!(environments.is_empty());

    let legacy_invalid_script = temp.path().join("legacy-invalid-provider.sh");
    std::fs::write(
        &legacy_invalid_script,
        r#"#!/bin/sh
REQ="$(cat)"
case "$REQ" in
  *'"action":"list-environments"'*|*'"action": "list-environments"'*)
    exit 2
    ;;
  *'"action":"list-realms"'*|*'"action": "list-realms"'*)
    printf 'not-json'
    ;;
  *)
    exit 3
    ;;
esac
"#,
    )
    .expect("legacy invalid script should be writable");
    make_executable(&legacy_invalid_script);

    let provider =
        ExecProvider::new("primary", &legacy_invalid_script).with_timeout(Duration::from_secs(10));
    let err = provider
        .list_environments()
        .await
        .expect_err("invalid legacy realms response should fail");
    assert!(err.to_string().starts_with("auth: parse realms from "));
    assert!(err.to_string().contains("legacy-invalid-provider.sh"));
}

#[tokio::test]
async fn exec_provider_wraps_invalid_credential_json_with_provider_command() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let script = temp.path().join("provider.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf 'not-json'
"#,
    )
    .expect("script should be writable");
    make_executable(&script);

    let provider = ExecProvider::new("primary", &script).with_timeout(Duration::from_secs(10));
    let err = provider
        .get_credential("prod", "setting:list", "read")
        .await
        .expect_err("invalid json should fail");

    assert!(err.to_string().starts_with("auth: parse credential from "));
    assert!(err.to_string().contains("provider.sh"));
}

#[tokio::test]
async fn exec_provider_wraps_process_failures_with_command_and_stderr() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let script = temp.path().join("provider-fails.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
echo provider failed >&2
exit 7
"#,
    )
    .expect("script should be writable");
    make_executable(&script);

    let provider = ExecProvider::new("primary", &script).with_timeout(Duration::from_secs(10));
    let err = provider
        .get_credential("prod", "setting:list", "read")
        .await
        .expect_err("process failure should be wrapped");
    let message = err.to_string();

    assert!(message.starts_with(&format!("auth: exec {}:", script.display())));
    assert!(message.contains("exit status 7"));
    assert!(message.contains("provider failed"));
}

#[tokio::test]
async fn exec_provider_wraps_spawn_failures_with_command() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let missing = temp.path().join("missing-provider");
    let provider = ExecProvider::new("primary", &missing).with_timeout(Duration::from_secs(10));

    let err = provider
        .get_credential("prod", "setting:list", "read")
        .await
        .expect_err("spawn failure should be wrapped");
    let message = err.to_string();

    assert!(message.starts_with(&format!("auth: exec {}:", missing.display())));
    assert!(message.contains("No such file") || message.contains("no such file"));
}

#[cfg(unix)]
#[tokio::test]
async fn exec_provider_builders_status_and_logout_pass_extra_args() {
    let tmp = tempfile::tempdir().expect("temp dir should create");
    let provider = tmp.path().join("provider");
    tokio::fs::write(
        &provider,
        b"#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$0.args\"\ninput=$(cat)\naction=$(printf '%s' \"$input\" | sed -n 's/.*\"action\":\"\\([^\"]*\\)\".*/\\1/p')\ncase \"$action\" in\n  status) printf '{\"token\":\"status-token\",\"identity\":\"status-user\",\"expires_at\":\"2099-01-01T00:00:00Z\"}' ;;\n  logout) printf '{}' ;;\n  *) printf '{\"token\":\"token\",\"identity\":\"user\",\"expires_at\":\"2099-01-01T00:00:00Z\"}' ;;\nesac\n",
    )
    .await
    .expect("provider script should write");
    make_executable(&provider);
    let provider_client = ExecProvider::new("primary", &provider)
        .with_args(["--profile", "dev"])
        .with_timeout(Duration::from_secs(5));

    let status = provider_client
        .status("prod")
        .await
        .expect("status should decode credential");
    assert_eq!(status.token, "status-token");
    assert_eq!(status.identity, "status-user");

    provider_client
        .logout("prod")
        .await
        .expect("logout should accept empty object output");

    let args = std::fs::read_to_string(tmp.path().join("provider.args"))
        .expect("provider args should be recorded");
    assert_eq!(args, "--profile\ndev\n--profile\ndev\n");
}

#[tokio::test]
async fn exec_provider_timeout_wraps_killed_process_with_command() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let script = temp.path().join("slow-provider.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
sleep 5
"#,
    )
    .expect("script should be writable");
    make_executable(&script);

    let provider = ExecProvider::new("primary", &script).with_timeout(Duration::from_millis(50));
    let err = provider
        .get_credential("prod", "setting:list", "read")
        .await
        .expect_err("timeout should be wrapped as a killed provider process");
    let message = err.to_string();

    assert!(message.starts_with(&format!("auth: exec {}:", script.display())));
    assert!(message.contains("signal: killed"));
}

#[tokio::test]
async fn exec_provider_zero_timeout_disables_deadline_preserves_legacy() {
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let script = temp.path().join("provider.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
sleep 0.05
printf '{"token":"abc","expires_at":"2099-01-01T00:00:00Z","identity":"tester"}'
"#,
    )
    .expect("script should be writable");
    make_executable(&script);

    let provider = ExecProvider::new("primary", &script).with_timeout(Duration::ZERO);
    let credential = provider
        .get_credential("prod", "setting:list", "read")
        .await
        .expect("zero timeout should not kill provider");

    assert_eq!(credential.token, "abc");
}

#[tokio::test]
async fn transport_injectors_set_exact_headers_and_cookies() {
    let token = token_func("abc");

    let mut request = build_request();
    BearerTokenInjector::new(Arc::clone(&token))
        .inject(&mut request)
        .await
        .expect("bearer injector should work");
    assert_eq!(header(&request, "authorization"), "Bearer abc");

    let mut request = build_request();
    CookieInjector::new("auth", Arc::clone(&token))
        .inject(&mut request)
        .await
        .expect("cookie injector should work");
    assert_eq!(header(&request, "cookie"), "auth=abc");

    let mut request = build_request();
    request.headers_mut().insert(
        "cookie",
        reqwest::header::HeaderValue::from_static("existing=one"),
    );
    CookieInjector::new("auth", Arc::clone(&token))
        .inject(&mut request)
        .await
        .expect("cookie injector should append like net/http AddCookie");
    assert_eq!(header(&request, "cookie"), "existing=one; auth=abc");

    let mut request = build_request();
    BasicAuthInjector::new("user", "pass")
        .inject(&mut request)
        .await
        .expect("basic injector should work");
    assert_eq!(header(&request, "authorization"), "Basic dXNlcjpwYXNz");

    let mut request = build_request();
    ApiKeyInjector::new("key")
        .inject(&mut request)
        .await
        .expect("api key injector should work");
    assert_eq!(header(&request, "x-api-key"), "key");

    let mut request = build_request();
    NoopInjector
        .inject(&mut request)
        .await
        .expect("noop injector should work");
    assert!(request.headers().is_empty());
}

#[test]
fn transport_injector_debug_impls_are_stable_and_do_not_expose_tokens() {
    assert_eq!(
        format!("{:?}", BearerTokenInjector::new(token_func("secret"))),
        "BearerTokenInjector { .. }"
    );
    let cookie_debug = format!("{:?}", CookieInjector::new("sid", token_func("secret")));
    assert!(cookie_debug.contains("CookieInjector"));
    assert!(cookie_debug.contains("sid"));
    assert!(!cookie_debug.contains("secret"));
}

#[tokio::test]
async fn transport_injectors_wrap_token_errors() {
    let failing_token = failing_token_func("token failed");

    let mut request = build_request();
    let err = BearerTokenInjector::new(Arc::clone(&failing_token))
        .inject(&mut request)
        .await
        .expect_err("bearer injector should wrap token errors");
    assert_eq!(err.to_string(), "transport: bearer inject: token failed");

    let mut request = build_request();
    let err = CookieInjector::new("auth", Arc::clone(&failing_token))
        .inject(&mut request)
        .await
        .expect_err("cookie injector should wrap token errors");
    assert_eq!(err.to_string(), "transport: cookie inject: token failed");

    let mut request = build_request();
    let err = ProviderBearerInjector::new(Arc::new(FailingProvider), "prod")
        .inject(&mut request)
        .await
        .expect_err("provider bearer should wrap provider errors");
    assert_eq!(
        err.to_string(),
        "transport: provider bearer: provider failed"
    );

    let token_server = TestServer::new(|request| {
        assert!(request.contains("POST /token HTTP/1.1"));
        http_response(500, &[("Content-Type", "text/plain")], "no token")
    });
    let mut request = build_request();
    let err = ClientCredentialsInjector::new(
        format!("{}/token", token_server.base_url()),
        "client",
        "secret",
        "",
    )
    .inject(&mut request)
    .await
    .expect_err("client credentials should wrap token request errors");
    assert_eq!(
        err.to_string(),
        "transport: client credentials inject: token request: status 500"
    );
}

#[tokio::test]
async fn client_credentials_injector_requires_http_200_token_response_preserves_legacy() {
    let token_server = TestServer::new(|request| {
        assert!(request.contains("POST /token HTTP/1.1"));
        http_response(
            201,
            &[("Content-Type", "application/json")],
            r#"{"access_token":"created","expires_in":3600}"#,
        )
    });
    let mut request = build_request();
    let err = ClientCredentialsInjector::new(
        format!("{}/token", token_server.base_url()),
        "client",
        "secret",
        "",
    )
    .inject(&mut request)
    .await
    .expect_err("client credentials should require exact HTTP 200");

    assert_eq!(
        err.to_string(),
        "transport: client credentials inject: token request: status 201"
    );
}

#[tokio::test]
async fn http_client_wraps_auth_inject_errors() {
    let client = HttpClient::new(
        "http://127.0.0.1:9",
        Arc::new(BearerTokenInjector::new(failing_token_func("token failed"))),
    );

    let err = client
        .get::<serde_json::Value>("/thing")
        .await
        .expect_err("request should fail before transport when auth injection fails");

    assert_eq!(
        err.to_string(),
        "transport: auth inject: transport: bearer inject: token failed"
    );
}

#[tokio::test]
async fn provider_bearer_injector_caches_process_token() {
    let provider = Arc::new(FakeProvider::new("oauth", "tester"));
    let injector = ProviderBearerInjector::new(provider, "prod");

    let mut first = build_request();
    injector
        .inject(&mut first)
        .await
        .expect("provider bearer should inject");
    assert_eq!(header(&first, "authorization"), "Bearer token");

    let mut second = build_request();
    injector
        .inject(&mut second)
        .await
        .expect("provider bearer should reuse cached token");
    assert_eq!(header(&second, "authorization"), "Bearer token");
}

#[tokio::test]
async fn provider_bearer_injector_empty_token_does_not_short_circuit_cache_preserves_legacy() {
    let calls = Arc::new(AtomicUsize::new(0));
    let injector = ProviderBearerInjector::new(
        Arc::new(EmptyThenFilledProvider {
            calls: Arc::clone(&calls),
        }),
        "prod",
    );

    let mut first = build_request();
    injector
        .inject(&mut first)
        .await
        .expect("first empty provider token should inject");
    assert_eq!(header(&first, "authorization"), "Bearer ");

    let mut second = build_request();
    injector
        .inject(&mut second)
        .await
        .expect("second provider token should refresh after empty cached token");
    assert_eq!(header(&second, "authorization"), "Bearer filled");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn client_credentials_injector_requests_and_caches_bearer_token() {
    // The injector tags its token request with the process default User-Agent;
    // hold the user-agent lock and pin the default so the assertion is stable,
    // restoring it on unwind via the RAII guard while the lock is still held.
    let _ua_guard = USER_AGENT_TEST_LOCK.lock().await;
    let _restore_ua = RestoreDefaultUserAgent;
    transport::set_default_user_agent("cli/dev");
    let token_requests = Arc::new(AtomicUsize::new(0));
    let token_requests_for_server = Arc::clone(&token_requests);
    let server = TestServer::sequence(vec![Box::new(move |request| {
        token_requests_for_server.fetch_add(1, Ordering::SeqCst);
        assert!(request.contains("POST /token HTTP/1.1"));
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("user-agent: cli/dev"));
        assert!(request.contains("grant_type=client_credentials"));
        assert!(request.contains("client_id=client"));
        assert!(request.contains("client_secret=secret"));
        assert!(request.contains("scope=read"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"access_token":"tok","expires_in":3600}"#,
        )
    })]);
    let injector = ClientCredentialsInjector::new(
        format!("{}/token", server.base_url()),
        "client",
        "secret",
        "read",
    );

    let mut first = build_request();
    injector
        .inject(&mut first)
        .await
        .expect("client credentials should inject");
    assert_eq!(header(&first, "authorization"), "Bearer tok");

    let mut second = build_request();
    injector
        .inject(&mut second)
        .await
        .expect("cached token should inject");
    assert_eq!(header(&second, "authorization"), "Bearer tok");
    assert_eq!(token_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn client_credentials_injector_empty_token_does_not_short_circuit_cache_preserves_legacy() {
    let token_requests = Arc::new(AtomicUsize::new(0));
    let token_requests_for_first = Arc::clone(&token_requests);
    let token_requests_for_second = Arc::clone(&token_requests);
    let server = TestServer::sequence(vec![
        Box::new(move |request| {
            token_requests_for_first.fetch_add(1, Ordering::SeqCst);
            assert!(request.contains("POST /token HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"access_token":"","expires_in":3600}"#,
            )
        }),
        Box::new(move |request| {
            token_requests_for_second.fetch_add(1, Ordering::SeqCst);
            assert!(request.contains("POST /token HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"access_token":"filled","expires_in":3600}"#,
            )
        }),
    ]);
    let injector = ClientCredentialsInjector::new(
        format!("{}/token", server.base_url()),
        "client",
        "secret",
        "",
    );

    let mut first = build_request();
    injector
        .inject(&mut first)
        .await
        .expect("first empty token should inject");
    assert_eq!(header(&first, "authorization"), "Bearer ");

    let mut second = build_request();
    injector
        .inject(&mut second)
        .await
        .expect("second token should refresh after empty cached token");
    assert_eq!(header(&second, "authorization"), "Bearer filled");
    assert_eq!(token_requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn client_credentials_injector_missing_token_and_negative_expiry_match_legacy_decode() {
    let token_requests = Arc::new(AtomicUsize::new(0));
    let token_requests_for_first = Arc::clone(&token_requests);
    let token_requests_for_second = Arc::clone(&token_requests);
    let server = TestServer::sequence(vec![
        Box::new(move |request| {
            token_requests_for_first.fetch_add(1, Ordering::SeqCst);
            assert!(request.contains("POST /token HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"expires_in":-1}"#,
            )
        }),
        Box::new(move |request| {
            token_requests_for_second.fetch_add(1, Ordering::SeqCst);
            assert!(request.contains("POST /token HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"access_token":"filled","expires_in":3600}"#,
            )
        }),
    ]);
    let injector = ClientCredentialsInjector::new(
        format!("{}/token", server.base_url()),
        "client",
        "secret",
        "",
    );

    let mut first = build_request();
    injector
        .inject(&mut first)
        .await
        .expect("missing access token should decode as empty string like legacy behavior");
    assert_eq!(header(&first, "authorization"), "Bearer ");

    let mut second = build_request();
    injector
        .inject(&mut second)
        .await
        .expect("negative expiry should not cache the empty token");
    assert_eq!(header(&second, "authorization"), "Bearer filled");
    assert_eq!(token_requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn http_client_get_sets_headers_and_decodes_json() {
    let _guard = USER_AGENT_TEST_LOCK.lock().await;
    let _restore_ua = RestoreDefaultUserAgent;
    transport::set_default_user_agent("cli/dev");
    let server = TestServer::new(|request| {
        assert!(request.contains("GET /thing HTTP/1.1"));
        assert!(request.contains("user-agent: cli/dev"));
        assert!(request.contains("authorization: Bearer abc"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"name":"thing"}"#,
        )
    });
    let client = HttpClient::new(
        server.base_url(),
        Arc::new(BearerTokenInjector::new(token_func("abc"))),
    );

    let value: serde_json::Value = client.get("/thing").await.expect("get should decode json");

    assert_eq!(value, json!({"name": "thing"}));
}

#[tokio::test]
async fn http_client_no_content_returns_default_result_preserves_legacy_skips_decode() {
    #[derive(Debug, Default, serde::Deserialize, PartialEq)]
    struct Thing {
        name: String,
    }

    let server = TestServer::new(|request| {
        assert!(request.contains("GET /empty HTTP/1.1"));
        http_response(204, &[], "")
    });
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let value: Thing = client
        .get("/empty")
        .await
        .expect("204 should not try to decode a response body");

    assert_eq!(value, Thing::default());
}

#[tokio::test]
async fn http_client_common_method_helpers_cover_put_patch_and_delete_variants() {
    let put_server = TestServer::new(|request| {
        assert!(request.contains("PUT /thing HTTP/1.1"));
        assert!(request.ends_with(r#"{"name":"updated"}"#));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":"put"}"#,
        )
    });
    let put_client = HttpClient::new(put_server.base_url(), Arc::new(NoopInjector));
    let put_value: serde_json::Value = put_client
        .put("/thing", &json!({"name": "updated"}))
        .await
        .expect("PUT should decode json");
    assert_eq!(put_value, json!({"ok": "put"}));

    let patch_server = TestServer::new(|request| {
        assert!(request.contains("PATCH /thing HTTP/1.1"));
        assert!(request.ends_with(r#"{"name":"patched"}"#));
        http_response(204, &[], "")
    });
    let patch_client = HttpClient::new(patch_server.base_url(), Arc::new(NoopInjector));
    patch_client
        .patch_without_response("/thing", &json!({"name": "patched"}))
        .await
        .expect("PATCH without response should accept 204");

    let delete_server = TestServer::new(|request| {
        assert!(request.contains("DELETE /thing HTTP/1.1"));
        http_response(204, &[], "")
    });
    let delete_client = HttpClient::new(delete_server.base_url(), Arc::new(NoopInjector));
    delete_client
        .delete("/thing")
        .await
        .expect("DELETE should accept 204");
}

#[tokio::test]
async fn http_client_null_json_returns_default_result_preserves_legacy_zero_value_decode() {
    #[derive(Debug, Default, serde::Deserialize, PartialEq)]
    struct Thing {
        name: String,
    }

    let server = TestServer::new(|request| {
        assert!(request.contains("GET /null HTTP/1.1"));
        http_response(200, &[("Content-Type", "application/json")], "null")
    });
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let value: Thing = client
        .get("/null")
        .await
        .expect("null JSON should decode as the typed zero value like legacy behavior");

    assert_eq!(value, Thing::default());
}

#[tokio::test]
async fn http_client_set_default_user_agent_affects_new_clients_only() {
    let _guard = USER_AGENT_TEST_LOCK.lock().await;
    let _restore_ua = RestoreDefaultUserAgent;
    transport::set_default_user_agent("cli/custom");
    let custom_server = TestServer::new(|request| {
        assert!(request.contains("GET /thing HTTP/1.1"));
        assert!(request.contains("user-agent: cli/custom"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    let custom_client = HttpClient::new(custom_server.base_url(), Arc::new(NoopInjector));

    let value: serde_json::Value = custom_client
        .get("/thing")
        .await
        .expect("custom user agent should apply");
    assert_eq!(value, json!({"ok": true}));

    let explicit_server = TestServer::new(|request| {
        assert!(request.contains("GET /thing HTTP/1.1"));
        assert!(request.contains("user-agent: cli/explicit"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    let explicit_client = HttpClient::builder(explicit_server.base_url(), Arc::new(NoopInjector))
        .with_user_agent("cli/explicit")
        .build();

    let value: serde_json::Value = explicit_client
        .get("/thing")
        .await
        .expect("explicit user agent should override default");
    assert_eq!(value, json!({"ok": true}));
}

#[tokio::test]
async fn http_client_builder_aliases_set_user_agent_headers_and_logger() {
    let logger = Arc::new(RecordingTransportLogger::default());
    let server = TestServer::new(|request| {
        assert!(request.contains("GET /thing HTTP/1.1"));
        assert!(request.contains("user-agent: cli/builder"));
        assert!(request.contains("x-team: platform"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    let client = HttpClientBuilder::new(server.base_url(), Arc::new(NoopInjector))
        .user_agent("cli/builder")
        .with_default_headers(BTreeMap::from([(
            "X-Team".to_owned(),
            "platform".to_owned(),
        )]))
        .logger(logger.clone())
        .build();

    let value: serde_json::Value = client
        .get("/thing")
        .await
        .expect("builder aliases should build a working client");

    assert_eq!(value, json!({"ok": true}));
    assert!(logger.messages().contains(&"http request".to_owned()));
}

#[tokio::test]
async fn http_client_custom_logger_observes_request_response_and_retry_preserves_legacy_option() {
    let logger = Arc::new(RecordingTransportLogger::default());
    let server = TestServer::sequence(vec![
        Box::new(|request| {
            assert!(request.contains("GET /thing HTTP/1.1"));
            http_response(429, &[("Content-Type", "text/plain")], "slow down")
        }),
        Box::new(|request| {
            assert!(request.contains("GET /thing HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"ok":true}"#,
            )
        }),
    ]);
    let client = HttpClient::builder(server.base_url(), Arc::new(NoopInjector))
        .with_logger(logger.clone())
        .build();

    let value: serde_json::Value = client.get("/thing").await.expect("retry should succeed");

    assert_eq!(value, json!({"ok": true}));
    assert_eq!(
        logger.messages(),
        vec![
            "http request",
            "http response",
            "retrying request",
            "http request",
            "http response",
        ]
    );
    let events = logger.events();
    assert_eq!(events[0].fields["method"], "GET");
    assert!(events[0].fields["url"].ends_with("/thing"));
    assert_eq!(events[1].fields["status"], "429");
    assert_eq!(events[2].fields["attempt"], "2");
    assert_eq!(events[4].fields["status"], "200");
    // Response events now carry the buffered body bytes.
    assert_eq!(events[1].body.as_deref(), Some(b"slow down".as_slice()));
    assert_eq!(
        events[4].body.as_deref(),
        Some(br#"{"ok":true}"#.as_slice())
    );
    // The successful response captures its headers (content-type from the server).
    assert!(
        events[4]
            .headers
            .as_ref()
            .expect("response event should capture headers")
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("content-type")
                && value.contains("application/json"))
    );
}

#[tokio::test]
async fn http_client_custom_logger_observes_raw_if_match_and_multipart_events_preserves_legacy() {
    let logger = Arc::new(RecordingTransportLogger::default());
    let server = TestServer::sequence(vec![
        Box::new(|request| {
            assert!(request.contains("OPTIONS /raw HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"ok":true}"#,
            )
        }),
        Box::new(|request| {
            assert!(request.contains("GET /download HTTP/1.1"));
            http_response(429, &[("Content-Type", "text/plain")], "slow down")
        }),
        Box::new(|request| {
            assert!(request.contains("GET /download HTTP/1.1"));
            http_response(200, &[("Content-Type", "text/plain")], "download")
        }),
        Box::new(|request| {
            assert!(request.contains("POST /post-raw HTTP/1.1"));
            assert!(!request.to_lowercase().contains("content-type:"));
            http_response(200, &[("Content-Type", "text/plain")], "post raw")
        }),
        Box::new(|request| {
            assert!(request.contains("PUT /match HTTP/1.1"));
            assert!(request.contains("if-match: v1"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
        Box::new(|request| {
            assert!(request.contains("POST /upload HTTP/1.1"));
            assert!(request.contains("file-content"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
    ]);
    let client = HttpClientBuilder::new(server.base_url(), Arc::new(NoopInjector))
        .logger(logger.clone())
        .build();
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let file_path = temp.path().join("artifact.txt");
    std::fs::write(&file_path, "file-content").expect("file should write");

    let value: serde_json::Value = client
        .do_raw_optional_body(reqwest::Method::OPTIONS, "/raw", "", None)
        .await
        .expect("raw request should decode json");
    assert_eq!(value, json!({"ok": true}));
    let mut downloaded = Vec::new();
    client
        .get_raw("/download", &mut downloaded)
        .await
        .expect("raw get should retry and stream");
    assert_eq!(
        String::from_utf8(downloaded).expect("download should be utf8"),
        "download"
    );
    let mut posted = Vec::new();
    client
        .post_raw::<serde_json::Value>("/post-raw", None, &mut posted)
        .await
        .expect("raw post should stream");
    assert_eq!(
        String::from_utf8(posted).expect("post raw should be utf8"),
        "post raw"
    );
    client
        .put_if_match_without_response("/match", &json!({"ok": true}), "v1")
        .await
        .expect("if-match without response should skip decode");
    client
        .post_multipart_without_response("/upload", "file", &file_path)
        .await
        .expect("multipart without response should skip decode");

    // Raw, if-match, and multipart paths now emit unified `http request` /
    // `http response` events, and the raw/download/post-raw paths log the
    // response too (size only, never the body bytes).
    assert_eq!(
        logger.messages(),
        vec![
            "http request",  // OPTIONS /raw
            "http response", // 200 /raw
            "http request",  // GET /download (429)
            "http response", // 429
            "http request",  // GET /download (200)
            "http response", // 200 download (size only)
            "http request",  // POST /post-raw
            "http response", // 200 post raw (size only)
            "http request",  // PUT /match
            "http response", // 200 /match
            "http request",  // POST /upload (multipart)
            "http response", // 200 /upload
        ]
    );
    let events = logger.events();
    assert_eq!(events[0].fields["method"], "OPTIONS");
    assert!(events[0].fields["url"].ends_with("/raw"));
    assert!(events[2].fields["url"].ends_with("/download"));
    assert_eq!(events[3].fields["status"], "429");
    // Download response logs its size, not the body bytes.
    assert_eq!(events[5].fields["body_bytes"], "download".len().to_string());
    assert!(events[5].body.is_none());
    assert!(events[6].fields["url"].ends_with("/post-raw"));
    assert_eq!(events[8].fields["method"], "PUT");
    assert_eq!(events[9].fields["status"], "200");
    assert!(events[10].fields["url"].ends_with("/upload"));
    // Request events carry captured headers (e.g. the user-agent).
    assert!(
        events[0]
            .headers
            .as_ref()
            .expect("request event should capture headers")
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
    );
}

#[tokio::test]
async fn http_client_picks_up_process_global_default_logger() {
    // The `--debug` feature works by publishing a process-global default logger
    // that every HttpClient built afterward inherits without per-command wiring.
    // This proves a client built WITHOUT `.logger(...)` records to that global.
    //
    // Hold the logger lock so the install/assert/reset window is isolated from
    // any other test that mutates the same process-global.
    let _logger_guard = TRANSPORT_LOGGER_TEST_LOCK.lock().await;
    struct ResetLogger;
    impl Drop for ResetLogger {
        fn drop(&mut self) {
            transport::set_default_transport_logger(Arc::new(transport::NoopTransportLogger));
        }
    }
    let _reset = ResetLogger;

    let logger = Arc::new(RecordingTransportLogger::default());
    transport::set_default_transport_logger(logger.clone());

    let server = TestServer::new(|request| {
        assert!(request.contains("GET /global-logger-probe HTTP/1.1"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    // Built with the default logger (no `.logger(...)`): it must inherit the
    // process-global default installed above.
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));
    let value: serde_json::Value = client
        .get("/global-logger-probe")
        .await
        .expect("request should succeed");
    assert_eq!(value, json!({"ok": true}));

    // Filter by our unique path so concurrent tests sharing the global logger
    // during this window cannot perturb the assertions.
    let events = logger.events();
    let probe: Vec<_> = events
        .iter()
        .filter(|event| {
            event
                .fields
                .get("url")
                .is_some_and(|url| url.ends_with("/global-logger-probe"))
        })
        .collect();
    assert!(
        probe.iter().any(|event| event.message == "http request"),
        "global logger should record the request"
    );
    let response = probe
        .iter()
        .find(|event| event.message == "http response")
        .expect("global logger should record the response");
    assert_eq!(response.fields["status"], "200");
    assert_eq!(
        response.body.as_deref(),
        Some(br#"{"ok":true}"#.as_slice()),
        "response event should carry the buffered body"
    );
}

#[tokio::test]
async fn http_client_reports_request_build_and_multipart_file_errors() {
    let invalid_client = HttpClient::new("not a url", Arc::new(NoopInjector));
    let err = invalid_client
        .get::<serde_json::Value>("/thing")
        .await
        .expect_err("invalid base url should fail while building request");
    assert!(
        err.to_string()
            .starts_with("transport: create request: builder error")
            || err.to_string().starts_with("transport: create request:")
    );

    let upload_client = HttpClient::new("http://127.0.0.1:1", Arc::new(NoopInjector));
    let err = upload_client
        .post_multipart::<serde_json::Value>(
            "/upload",
            "file",
            std::path::Path::new("/definitely/not/a/real/file.txt"),
        )
        .await
        .expect_err("missing multipart file should fail before network io");
    assert!(err.to_string().starts_with("transport: open file:"));
}

#[tokio::test]
async fn http_client_post_sends_json_body() {
    let server = TestServer::new(|request| {
        assert!(request.contains("POST /thing HTTP/1.1"));
        assert!(request.contains("content-type: application/json"));
        assert!(request.ends_with(r#"{"name":"thing"}"#));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let value: serde_json::Value = client
        .post("/thing", &json!({"name": "thing"}))
        .await
        .expect("post should decode json");

    assert_eq!(value, json!({"ok": true}));
}

#[tokio::test]
async fn http_client_default_headers_can_override_json_content_type_preserves_legacy() {
    let server = TestServer::new(|request| {
        assert!(request.contains("POST /thing HTTP/1.1"));
        assert!(request.contains("content-type: application/vnd.cli-engine+json"));
        assert!(request.ends_with(r#"{"name":"thing"}"#));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    let client = HttpClient::builder(server.base_url(), Arc::new(NoopInjector))
        .with_default_headers(BTreeMap::from([(
            "Content-Type".to_owned(),
            "application/vnd.cli-engine+json".to_owned(),
        )]))
        .build();

    let value: serde_json::Value = client
        .post("/thing", &json!({"name": "thing"}))
        .await
        .expect("post should decode json");

    assert_eq!(value, json!({"ok": true}));
}

#[tokio::test]
async fn http_client_do_raw_sends_method_content_type_body_and_decodes_json() {
    let _guard = USER_AGENT_TEST_LOCK.lock().await;
    let _restore_ua = RestoreDefaultUserAgent;
    transport::set_default_user_agent("cli/dev");
    let server = TestServer::new(|request| {
        assert!(request.contains("OPTIONS /raw HTTP/1.1"));
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("user-agent: cli/dev"));
        assert!(request.contains("x-trace: trace-1"));
        assert!(request.contains("authorization: Bearer tok"));
        assert!(request.ends_with("realm=prod"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    let client = HttpClientBuilder::new(
        server.base_url(),
        Arc::new(BearerTokenInjector::new(token_func("tok"))),
    )
    .default_headers(BTreeMap::from([(
        "X-Trace".to_owned(),
        "trace-1".to_owned(),
    )]))
    .build();

    let value: serde_json::Value = client
        .do_raw(
            reqwest::Method::OPTIONS,
            "/raw",
            "application/x-www-form-urlencoded",
            "realm=prod",
        )
        .await
        .expect("raw request should decode json");

    assert_eq!(value, json!({"ok": true}));
}

#[tokio::test]
async fn http_client_do_raw_optional_none_body_matches_legacy_nil_reader() {
    let _ua_guard = USER_AGENT_TEST_LOCK.lock().await;
    let _restore_ua = RestoreDefaultUserAgent;
    transport::set_default_user_agent("cli/dev");
    let server = TestServer::new(|request| {
        assert!(request.contains("OPTIONS /raw HTTP/1.1"));
        assert!(request.contains("user-agent: cli/dev"));
        assert!(!request.to_lowercase().contains("content-type:"));
        assert!(request.ends_with("\r\n\r\n"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"name":"ok"}"#,
        )
    });
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let value: serde_json::Value = client
        .do_raw_optional_body(reqwest::Method::OPTIONS, "/raw", "", None)
        .await
        .expect("nil raw body should decode json");

    assert_eq!(value, json!({"name": "ok"}));
}

#[tokio::test]
async fn http_client_without_response_helpers_skip_success_decode_preserves_legacy_nil_result() {
    let server = TestServer::sequence(vec![
        Box::new(|request| {
            assert!(request.contains("GET /get HTTP/1.1"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
        Box::new(|request| {
            assert!(request.contains("POST /post HTTP/1.1"));
            assert!(request.ends_with(r#"{"ok":true}"#));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
        Box::new(|request| {
            assert!(request.contains("PUT /put HTTP/1.1"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
        Box::new(|request| {
            assert!(request.contains("PATCH /patch HTTP/1.1"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
        Box::new(|request| {
            assert!(request.contains("OPTIONS /raw HTTP/1.1"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    client
        .get_without_response("/get")
        .await
        .expect("get without response should skip decode");
    client
        .post_without_response("/post", &json!({"ok": true}))
        .await
        .expect("post without response should skip decode");
    client
        .put_without_response("/put", &json!({"ok": true}))
        .await
        .expect("put without response should skip decode");
    client
        .patch_without_response("/patch", &json!({"ok": true}))
        .await
        .expect("patch without response should skip decode");
    client
        .do_raw_optional_body_without_response(reqwest::Method::OPTIONS, "/raw", "", None)
        .await
        .expect("raw without response should skip decode");
}

#[tokio::test]
async fn http_client_etag_if_match_and_multipart_without_response_skip_success_decode_preserves_legacy()
 {
    let server = TestServer::sequence(vec![
        Box::new(|request| {
            assert!(request.contains("GET /etag HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json"), ("ETag", "v1")],
                "not-json",
            )
        }),
        Box::new(|request| {
            assert!(request.contains("PUT /match HTTP/1.1"));
            assert!(request.contains("if-match: v1"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
        Box::new(|request| {
            assert!(request.contains("POST /upload HTTP/1.1"));
            assert!(request.contains("file-content"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
        Box::new(|request| {
            assert!(request.contains("POST /upload-fields HTTP/1.1"));
            assert!(request.contains("demo"));
            assert!(request.contains("file-content"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
        Box::new(|request| {
            assert!(request.contains("POST /fields HTTP/1.1"));
            assert!(request.contains("demo"));
            http_response(200, &[("Content-Type", "application/json")], "not-json")
        }),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let file_path = temp.path().join("artifact.txt");
    std::fs::write(&file_path, "file-content").expect("file should write");

    let etag = client
        .get_etag_without_response("/etag")
        .await
        .expect("etag helper should skip decode");
    assert_eq!(etag, "v1");
    client
        .put_if_match_without_response("/match", &json!({"ok": true}), &etag)
        .await
        .expect("put if match helper should skip decode");
    client
        .post_multipart_without_response("/upload", "file", &file_path)
        .await
        .expect("multipart helper should skip decode");
    client
        .post_multipart_with_fields_without_response(
            "/upload-fields",
            "file",
            &file_path,
            &BTreeMap::from([("name".to_owned(), "demo".to_owned())]),
        )
        .await
        .expect("multipart fields helper should skip decode");
    client
        .post_multipart_fields_without_response(
            "/fields",
            &BTreeMap::from([("name".to_owned(), "demo".to_owned())]),
        )
        .await
        .expect("multipart field-only helper should skip decode");
}

#[tokio::test]
async fn http_client_post_raw_none_body_omits_json_content_type_preserves_legacy() {
    let _ua_guard = USER_AGENT_TEST_LOCK.lock().await;
    let _restore_ua = RestoreDefaultUserAgent;
    transport::set_default_user_agent("cli/dev");
    let server = TestServer::new(|request| {
        assert!(request.contains("POST /raw HTTP/1.1"));
        assert!(request.contains("user-agent: cli/dev"));
        assert!(!request.to_lowercase().contains("content-type:"));
        assert!(request.ends_with("\r\n\r\n"));
        http_response(200, &[("Content-Type", "text/plain")], "raw response")
    });
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));
    let mut out = Vec::new();

    client
        .post_raw::<serde_json::Value>("/raw", None, &mut out)
        .await
        .expect("raw post without body should stream response");

    assert_eq!(
        String::from_utf8(out).expect("response should be utf8"),
        "raw response"
    );
}

#[tokio::test]
async fn http_client_post_raw_some_body_sends_json_preserves_legacy() {
    let server = TestServer::new(|request| {
        assert!(request.contains("POST /raw HTTP/1.1"));
        assert!(request.contains("content-type: application/json"));
        assert!(request.ends_with(r#"{"name":"thing"}"#));
        http_response(200, &[("Content-Type", "text/plain")], "raw response")
    });
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));
    let mut out = Vec::new();

    client
        .post_raw("/raw", Some(&json!({"name": "thing"})), &mut out)
        .await
        .expect("raw post with body should stream response");

    assert_eq!(
        String::from_utf8(out).expect("response should be utf8"),
        "raw response"
    );
}

#[tokio::test]
async fn http_client_structured_errors_preserve_code_system_and_request_id() {
    let server = TestServer::new(|_request| {
        http_response(
            403,
            &[("Content-Type", "application/json")],
            r#"{"code":"DENIED","message":"forbidden","system":"policy","request_id":"rid-1"}"#,
        )
    });
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let err = client
        .get::<serde_json::Value>("/denied")
        .await
        .expect_err("403 should return an error");

    let message = err.to_string();
    assert_eq!(message, "forbidden");
    let envelope = cli_engine::output::build_error_envelope(&err, "fallback");
    let error = envelope.error.expect("structured error");
    assert_eq!(error.code, "HTTP_403");
    assert_eq!(error.system, "policy");
    assert_eq!(error.request_id, "rid-1");
}

#[tokio::test]
async fn http_client_get_etag_and_put_if_match_use_expected_headers() {
    let server = TestServer::sequence(vec![
        Box::new(|request| {
            assert!(request.contains("GET /thing HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json"), ("ETag", "v1")],
                r#"{"name":"thing"}"#,
            )
        }),
        Box::new(|request| {
            assert!(request.contains("PUT /thing HTTP/1.1"));
            assert!(request.contains("if-match: v1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"ok":true}"#,
            )
        }),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let (value, etag): (serde_json::Value, String) = client
        .get_etag("/thing")
        .await
        .expect("etag get should work");
    assert_eq!(value, json!({"name": "thing"}));
    assert_eq!(etag, "v1");

    let updated: serde_json::Value = client
        .put_if_match("/thing", &json!({"name": "new"}), &etag)
        .await
        .expect("put if match should work");
    assert_eq!(updated, json!({"ok": true}));
}

#[tokio::test]
async fn http_client_graphql_extracts_data_and_joins_errors() {
    let server = TestServer::sequence(vec![
        Box::new(|request| {
            assert!(request.contains("POST /graphql HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"data":{"thing":{"name":"alpha"}}}"#,
            )
        }),
        Box::new(|_request| {
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"errors":[{"message":"first"},{"message":"second"}]}"#,
            )
        }),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let data: serde_json::Value = client
        .post_graphql("/graphql", "query", BTreeMap::new())
        .await
        .expect("graphql data should decode");
    assert_eq!(data, json!({"thing": {"name": "alpha"}}));

    let err = client
        .post_graphql::<serde_json::Value>("/graphql", "query", BTreeMap::new())
        .await
        .expect_err("graphql errors should fail");
    assert_eq!(err.to_string(), "graphql: first; second");
}

#[tokio::test]
async fn http_client_graphql_missing_and_null_data_leave_result_preserves_legacy() {
    #[derive(Debug, Default, serde::Deserialize, PartialEq)]
    struct Thing {
        name: String,
    }

    let server = TestServer::sequence(vec![
        Box::new(|_request| http_response(200, &[("Content-Type", "application/json")], "{}")),
        Box::new(|_request| {
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"data":null}"#,
            )
        }),
        Box::new(|_request| {
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"data":{"name":"updated"}}"#,
            )
        }),
        Box::new(|_request| {
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"data":null}"#,
            )
        }),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let missing: Thing = client
        .post_graphql("/graphql", "query", BTreeMap::new())
        .await
        .expect("missing data should return default");
    assert_eq!(missing, Thing::default());

    let null: Thing = client
        .post_graphql("/graphql", "query", BTreeMap::new())
        .await
        .expect("null data should return default");
    assert_eq!(null, Thing::default());

    let mut existing = Thing {
        name: "existing".to_owned(),
    };
    client
        .post_graphql_into("/graphql", "query", BTreeMap::new(), &mut existing)
        .await
        .expect("data should update result");
    assert_eq!(existing.name, "updated");

    client
        .post_graphql_into("/graphql", "query", BTreeMap::new(), &mut existing)
        .await
        .expect("null data should leave result unchanged");
    assert_eq!(existing.name, "updated");
}

#[tokio::test]
async fn http_client_graphql_decode_errors_have_legacy_prefix() {
    #[derive(Debug, Default, serde::Deserialize)]
    struct Thing {
        _name: String,
    }

    let server = TestServer::new(|_request| {
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"data":{"_name":123}}"#,
        )
    });
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let err = client
        .post_graphql::<Thing>("/graphql", "query", BTreeMap::new())
        .await
        .expect_err("invalid graphql data should fail");

    assert!(
        err.to_string()
            .starts_with("transport: decode graphql data: "),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn http_client_graphql_without_response_skips_data_decode_but_preserves_errors_preserves_legacy()
 {
    let server = TestServer::sequence(vec![
        Box::new(|request| {
            assert!(request.contains("POST /graphql HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"data":{"name":123}}"#,
            )
        }),
        Box::new(|_request| {
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"errors":[{"message":"first"},{"message":"second"}]}"#,
            )
        }),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    client
        .post_graphql_without_response("/graphql", "query", BTreeMap::new())
        .await
        .expect("nil result should skip graphql data decode");

    let err = client
        .post_graphql_without_response("/graphql", "query", BTreeMap::new())
        .await
        .expect_err("graphql errors should still fail");
    assert_eq!(err.to_string(), "graphql: first; second");
}

#[tokio::test]
async fn http_client_graphql_optional_variables_match_legacy_wire_shape() {
    let server = TestServer::sequence(vec![
        Box::new(|request| {
            assert!(request.contains("POST /graphql HTTP/1.1"));
            assert!(request.ends_with(r#"{"query":"query","variables":null}"#));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"data":{"name":"ok"}}"#,
            )
        }),
        Box::new(|request| {
            assert!(request.ends_with(r#"{"query":"query","variables":{}}"#));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"data":{"name":"ok"}}"#,
            )
        }),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let value: serde_json::Value = client
        .post_graphql_optional_variables("/graphql", "query", None)
        .await
        .expect("nil variables should serialize as null");
    assert_eq!(value, json!({"name": "ok"}));

    let value: serde_json::Value = client
        .post_graphql("/graphql", "query", BTreeMap::new())
        .await
        .expect("empty variables should serialize as object");
    assert_eq!(value, json!({"name": "ok"}));
}

#[tokio::test]
async fn http_client_multipart_uploads_file_and_fields() {
    let server = TestServer::new(|request| {
        assert!(request.contains("POST /upload HTTP/1.1"));
        assert!(request.contains("content-type: multipart/form-data; boundary="));
        assert!(request.contains(r#"name="name""#));
        assert!(request.contains("demo"));
        assert!(request.contains(r#"name="file"; filename="artifact.txt""#));
        assert!(request.contains("file-content"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    let temp = tempfile::tempdir().expect("tempdir should be available");
    let file_path = temp.path().join("artifact.txt");
    std::fs::write(&file_path, "file-content").expect("file should be writable");
    let fields = BTreeMap::from([("name".to_owned(), "demo".to_owned())]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let value: serde_json::Value = client
        .post_multipart_with_fields("/upload", "file", &file_path, &fields)
        .await
        .expect("multipart should decode response");

    assert_eq!(value, json!({"ok": true}));
}

#[tokio::test]
async fn http_client_multipart_fields_without_file() {
    let server = TestServer::new(|request| {
        assert!(request.contains("POST /fields HTTP/1.1"));
        assert!(request.contains(r#"name="a""#));
        assert!(request.contains("one"));
        assert!(request.contains(r#"name="b""#));
        assert!(request.contains("two"));
        http_response(
            200,
            &[("Content-Type", "application/json")],
            r#"{"ok":true}"#,
        )
    });
    let fields = BTreeMap::from([
        ("a".to_owned(), "one".to_owned()),
        ("b".to_owned(), "two".to_owned()),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let value: serde_json::Value = client
        .post_multipart_fields("/fields", &fields)
        .await
        .expect("multipart fields should decode response");

    assert_eq!(value, json!({"ok": true}));
}

#[tokio::test]
async fn http_client_retries_idempotent_5xx() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let first_attempts = Arc::clone(&attempts);
    let second_attempts = Arc::clone(&attempts);
    let server = TestServer::sequence(vec![
        Box::new(move |_request| {
            first_attempts.fetch_add(1, Ordering::SeqCst);
            http_response(500, &[], "temporary")
        }),
        Box::new(move |_request| {
            second_attempts.fetch_add(1, Ordering::SeqCst);
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"ok":true}"#,
            )
        }),
    ]);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let value: serde_json::Value = client.get("/retry").await.expect("retry should succeed");

    assert_eq!(value, json!({"ok": true}));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn http_client_retryable_status_preserves_body_read_failures_preserves_legacy() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let handlers: Vec<Box<dyn Fn(String) -> String + Send + Sync>> = (0..3)
        .map(|_| {
            let attempts = Arc::clone(&attempts);
            let handler: Box<dyn Fn(String) -> String + Send + Sync> =
                Box::new(move |request: String| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    assert!(request.contains("GET /broken HTTP/1.1"));
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial"
                        .to_owned()
                });
            handler
        })
        .collect();
    let server = TestServer::sequence(handlers);
    let client = HttpClient::new(server.base_url(), Arc::new(NoopInjector));

    let err = client
        .get::<serde_json::Value>("/broken")
        .await
        .expect_err("broken retry body should fail");

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert!(
        err.to_string()
            .starts_with("transport: GET /broken: status 500 (body read failed: "),
        "unexpected retry error: {err}"
    );
}

#[tokio::test]
async fn http_client_retries_429_for_post_but_not_non_idempotent_5xx() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let first_attempts = Arc::clone(&attempts);
    let second_attempts = Arc::clone(&attempts);
    let retry_server = TestServer::sequence(vec![
        Box::new(move |request| {
            first_attempts.fetch_add(1, Ordering::SeqCst);
            assert!(request.contains("POST /retry429 HTTP/1.1"));
            http_response(429, &[], "slow down")
        }),
        Box::new(move |request| {
            second_attempts.fetch_add(1, Ordering::SeqCst);
            assert!(request.contains("POST /retry429 HTTP/1.1"));
            http_response(
                200,
                &[("Content-Type", "application/json")],
                r#"{"ok":true}"#,
            )
        }),
    ]);
    let retry_client = HttpClient::new(retry_server.base_url(), Arc::new(NoopInjector));

    let value: serde_json::Value = retry_client
        .post("/retry429", &json!({"name": "thing"}))
        .await
        .expect("429 should retry even for post");

    assert_eq!(value, json!({"ok": true}));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let no_retry_attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_server = Arc::clone(&no_retry_attempts);
    let no_retry_server = TestServer::new(move |request| {
        attempts_for_server.fetch_add(1, Ordering::SeqCst);
        assert!(request.contains("POST /fail500 HTTP/1.1"));
        http_response(500, &[("Content-Type", "text/plain")], "temporary")
    });
    let no_retry_client = HttpClient::new(no_retry_server.base_url(), Arc::new(NoopInjector));

    let err = no_retry_client
        .post::<_, serde_json::Value>("/fail500", &json!({"name": "thing"}))
        .await
        .expect_err("non-idempotent 5xx should not retry");

    assert_eq!(no_retry_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(err.to_string(), "POST /fail500: 500 temporary");
}

#[tokio::test]
async fn http_client_get_raw_and_etag_exhausted_retries_omit_body_preserves_legacy() {
    let raw_attempts = Arc::new(AtomicUsize::new(0));
    let raw_handlers: Vec<Box<dyn Fn(String) -> String + Send + Sync>> = (0..3)
        .map(|_| {
            let attempts = Arc::clone(&raw_attempts);
            let handler: Box<dyn Fn(String) -> String + Send + Sync> =
                Box::new(move |request: String| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    assert!(request.contains("GET /raw HTTP/1.1"));
                    http_response(500, &[("Content-Type", "text/plain")], "retry body")
                });
            handler
        })
        .collect();
    let raw_server = TestServer::sequence(raw_handlers);
    let raw_client = HttpClient::new(raw_server.base_url(), Arc::new(NoopInjector));

    let mut out = Vec::new();
    let raw_err = raw_client
        .get_raw("/raw", &mut out)
        .await
        .expect_err("raw exhausted retry should fail");

    assert_eq!(raw_attempts.load(Ordering::SeqCst), 3);
    assert_eq!(raw_err.to_string(), "transport: GET /raw: status 500");
    assert!(out.is_empty());

    let etag_attempts = Arc::new(AtomicUsize::new(0));
    let etag_handlers: Vec<Box<dyn Fn(String) -> String + Send + Sync>> = (0..3)
        .map(|_| {
            let attempts = Arc::clone(&etag_attempts);
            let handler: Box<dyn Fn(String) -> String + Send + Sync> =
                Box::new(move |request: String| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    assert!(request.contains("GET /etag HTTP/1.1"));
                    http_response(500, &[("Content-Type", "text/plain")], "retry body")
                });
            handler
        })
        .collect();
    let etag_server = TestServer::sequence(etag_handlers);
    let etag_client = HttpClient::new(etag_server.base_url(), Arc::new(NoopInjector));

    let etag_err = etag_client
        .get_etag::<serde_json::Value>("/etag")
        .await
        .expect_err("etag exhausted retry should fail");

    assert_eq!(etag_attempts.load(Ordering::SeqCst), 3);
    assert_eq!(etag_err.to_string(), "transport: GET /etag: status 500");
}

#[tokio::test]
async fn middleware_success_authz_audit_activity_and_fields() {
    let audit = Arc::new(CaptureAudit::default());
    let activity = Arc::new(CaptureActivity::default());
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.app_id = "test-app".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.env = "prod".to_owned();
    middleware.verbose = "all".to_owned();
    middleware.fields = "name".to_owned();
    middleware.auditor = Some(audit.clone());
    middleware.activity = Some(activity.clone());
    middleware.authz = Some(Arc::new(AllowAuthorizer));

    let output = middleware
        .run(
            middleware_request_with_system(
                CommandMeta::default(),
                "things:list",
                "things-api",
                value_map([("project", json!("p1"))]),
                value_map([("project", json!("p1"))]),
                "",
                false,
            ),
            async |credential: CredentialResolver| {
                // Resolve so the credential's identity propagates into metadata,
                // audit, and activity under lazy resolution.
                credential.resolve().await?;
                Ok(CommandResult::new(
                    json!({"name": "thing", "status": "active"}),
                ))
            },
        )
        .await
        .expect("middleware success should render");

    assert_eq!(output.envelope.data, Some(json!({"name": "thing"})));
    let metadata = output
        .envelope
        .metadata
        .expect("verbose all keeps metadata");
    assert_eq!(metadata.system, "things-api");
    assert_eq!(metadata.command, "things:list");
    assert_eq!(metadata.env, "prod");
    assert_eq!(metadata.identity, "tester");
    assert_eq!(
        metadata.effective_args,
        Some(json!({"env": "prod", "project": "p1"}))
    );
    assert_eq!(audit.results().await, vec!["ok"]);
    assert_eq!(activity.statuses().await, vec!["ok"]);
}

#[tokio::test]
async fn middleware_run_injects_env_into_effective_args_and_side_effects() {
    let audit = Arc::new(CaptureAudit::default());
    let activity = Arc::new(CaptureActivity::default());
    let authz = Arc::new(CaptureArgsAuthorizer::default());
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.app_id = "test-app".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.env = "prod".to_owned();
    middleware.verbose = "effective_args".to_owned();
    middleware.auditor = Some(audit.clone());
    middleware.activity = Some(activity.clone());
    middleware.authz = Some(authz.clone());

    let output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([("project", json!("p1"))]),
                value_map([("project", json!("p1"))]),
                "",
                false,
            ),
            async |_credential| Ok(CommandResult::new(json!({"name": "thing"}))),
        )
        .await
        .expect("middleware success should render");

    assert_eq!(
        output.envelope.metadata.expect("metadata").effective_args,
        Some(json!({"env": "prod", "project": "p1"}))
    );
    assert_eq!(authz.args().await[0].get("env"), Some(&json!("prod")));
    assert_eq!(audit.args().await[0].get("env"), Some(&json!("prod")));
    assert_eq!(activity.args().await[0].get("env"), Some(&json!("prod")));
}

#[tokio::test]
async fn middleware_run_does_not_override_explicit_env_arg() {
    let authz = Arc::new(CaptureArgsAuthorizer::default());
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.env = "prod".to_owned();
    middleware.authz = Some(authz.clone());

    middleware
        .run(
            middleware_request_with_system(
                CommandMeta::default(),
                "things:list",
                "things-api",
                value_map([]),
                value_map([("env", json!("staging"))]),
                "",
                false,
            ),
            async |_credential| Ok(CommandResult::new(json!({"name": "thing"}))),
        )
        .await
        .expect("middleware success should render");

    assert_eq!(authz.args().await[0].get("env"), Some(&json!("staging")));
}

#[tokio::test]
async fn middleware_passes_command_scopes_to_provider_and_supports_step_up() {
    let recorded = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let mut middleware = Middleware::new();
    middleware.auth.register(Arc::new(RecordingScopeProvider {
        scopes: Arc::clone(&recorded),
    }));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.app_id = "test-app".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.env = "prod".to_owned();

    let mut meta = CommandMeta::default();
    meta.set_scopes(vec!["base:read".to_owned()]);
    middleware
        .run(
            middleware_request(meta, "things:list", value_map([]), value_map([]), "", false),
            async |credential: CredentialResolver| {
                // Static command scopes reach the provider.
                credential.resolve().await.expect("resolve");
                // A runtime-required scope triggers a re-resolution requesting
                // the union of declared + extra scopes.
                credential
                    .resolve_with_scopes(&["extra:write".to_owned()])
                    .await
                    .expect("resolve with scopes");
                // Already-covered scopes do not re-call the provider.
                credential
                    .resolve_with_scopes(&["extra:write".to_owned()])
                    .await
                    .expect("resolve with covered scopes");
                Ok(CommandResult::new(json!({})))
            },
        )
        .await
        .expect("middleware success should render");

    let calls = recorded.lock().await.clone();
    assert_eq!(calls.len(), 2, "third request was already covered");
    assert_eq!(calls[0], vec!["base:read"]);
    assert_eq!(calls[1], vec!["base:read", "extra:write"]);
}

#[tokio::test]
async fn middleware_aborts_step_up_that_switches_identity() {
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(SwitchingIdentityProvider {
            calls: Arc::new(Mutex::new(0)),
        }));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.app_id = "test-app".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.env = "prod".to_owned();

    let mut meta = CommandMeta::default();
    meta.set_scopes(vec!["base:read".to_owned()]);
    middleware
        .run(
            middleware_request(meta, "things:list", value_map([]), value_map([]), "", false),
            async |credential: CredentialResolver| {
                // First resolution authenticates as user-a (also the `peek` identity).
                credential.resolve().await.expect("first resolve");
                // Step-up forces a fresh resolution; the provider returns user-b,
                // so the engine must refuse rather than misattribute the action.
                let err = credential
                    .resolve_with_scopes(&["extra:write".to_owned()])
                    .await
                    .expect_err("identity switch during step-up must abort");
                assert!(err.to_string().contains("different identity"), "{err}");
                Ok(CommandResult::new(json!({})))
            },
        )
        .await
        .expect("middleware renders");
}

#[tokio::test]
async fn middleware_fixed_env_overrides_only_auth_env_preserves_legacy() {
    let captured_env = Arc::new(Mutex::new(Vec::new()));
    let authz = Arc::new(CaptureArgsAuthorizer::default());
    let activity = Arc::new(CaptureActivity::default());
    let mut middleware = Middleware::new();
    middleware.auth.register(Arc::new(RecordingEnvProvider {
        envs: Arc::clone(&captured_env),
    }));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.app_id = "test-app".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.env = "prod".to_owned();
    middleware.verbose = "all".to_owned();
    middleware.authz = Some(authz.clone());
    middleware.activity = Some(activity.clone());

    let output = middleware
        .run(
            middleware_request(
                CommandMeta {
                    dry_run_prompt: false,
                    auth_metadata: BTreeMap::from([
                        ("provider".to_owned(), "primary".to_owned()),
                        ("fixed_env".to_owned(), "auth-prod".to_owned()),
                    ]),
                    scopes: Vec::new(),
                },
                "things:list",
                value_map([]),
                value_map([("project", json!("p1"))]),
                "",
                false,
            ),
            async |credential: CredentialResolver| {
                assert_eq!(
                    credential.resolve().await.expect("credential").env,
                    "auth-prod"
                );
                Ok(CommandResult::new(json!({"name": "thing"})))
            },
        )
        .await
        .expect("middleware success should render");

    assert_eq!(captured_env.lock().await.as_slice(), ["auth-prod"]);
    assert_eq!(authz.args().await[0].get("env"), Some(&json!("prod")));
    assert_eq!(activity.args().await[0].get("env"), Some(&json!("prod")));
    let metadata = output.envelope.metadata.expect("metadata");
    assert_eq!(metadata.env, "prod");
    assert_eq!(
        metadata.effective_args,
        Some(json!({"env": "prod", "project": "p1"}))
    );
}

#[tokio::test]
async fn middleware_run_no_auth_does_not_inject_env_into_effective_args_or_side_effects() {
    let audit = Arc::new(CaptureAudit::default());
    let activity = Arc::new(CaptureActivity::default());
    let authz = Arc::new(CaptureArgsAuthorizer::default());
    let mut middleware = Middleware::new();
    middleware.output_format = "json".to_owned();
    middleware.env = "prod".to_owned();
    middleware.verbose = "effective_args".to_owned();
    middleware.auditor = Some(audit.clone());
    middleware.activity = Some(activity.clone());
    middleware.authz = Some(authz.clone());

    let output = middleware
        .run_no_auth(
            CommandMeta::default(),
            "auth:status",
            value_map([]),
            value_map([("provider", json!("primary"))]),
            "",
            async || Ok(CommandResult::new(json!({"status": "ok"}))),
        )
        .await
        .expect("no-auth middleware success should render");

    assert_eq!(
        output.envelope.metadata.expect("metadata").effective_args,
        Some(json!({"provider": "primary"}))
    );
    assert!(!authz.args().await[0].contains_key("env"));
    assert!(!audit.args().await[0].contains_key("env"));
    assert!(!activity.args().await[0].contains_key("env"));
}

#[test]
fn middleware_new_matches_legacy_initialized_dispatcher_and_empty_output_format() {
    let middleware = Middleware::new();

    assert_eq!(middleware.output_format, "");
    assert!(middleware.auth.registered_names().is_empty());
}

#[tokio::test]
async fn middleware_invalid_output_format_renders_error_after_business_logic() {
    let audit = Arc::new(CaptureAudit::default());
    let called = Arc::new(AtomicUsize::new(0));
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.app_id = "test-app".to_owned();
    middleware.output_format = "yaml".to_owned();
    middleware.verbose = "all".to_owned();
    middleware.auditor = Some(audit.clone());

    let called_for_handler = Arc::clone(&called);
    let output = middleware
        .run(
            middleware_request_with_system(
                CommandMeta::default(),
                "things:list",
                "things-api",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_credential| {
                called_for_handler.fetch_add(1, Ordering::SeqCst);
                Ok(CommandResult::new(json!({"name": "thing"})))
            },
        )
        .await
        .expect("invalid output format should be rendered as middleware error");

    let error = output.envelope.error.expect("error envelope");
    assert_eq!(
        error.message,
        "invalid output format \"yaml\": must be one of toon, json, human"
    );
    assert_eq!(
        output.envelope.metadata.expect("metadata").system,
        "test-app"
    );
    assert!(output.rendered.contains("invalid output format"));
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert_eq!(audit.results().await, vec!["ok"]);
}

#[tokio::test]
async fn middleware_audit_and_activity_failures_do_not_mask_command_result() {
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.auditor = Some(Arc::new(FailingAuditor));
    middleware.activity = Some(Arc::new(FailingActivity));

    let output = middleware
        .run(
            middleware_request_with_system(
                CommandMeta::default(),
                "things:list",
                "things-api",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_credential| Ok(CommandResult::new(json!({"ok": true}))),
        )
        .await
        .expect("side-effect failures should not mask success");

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.envelope.data, Some(json!({"ok": true})));

    let err_output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_credential| {
                Err::<CommandResult, _>(cli_engine::CliCoreError::message("business failed"))
            },
        )
        .await
        .expect("side-effect failures should not mask business error");

    assert_eq!(err_output.exit_code, 1);
    assert_eq!(
        err_output.envelope.error.expect("error").message,
        "business failed"
    );
}

#[tokio::test]
async fn middleware_authenticated_denial_records_side_effects_and_skips_handler() {
    let audit = Arc::new(CaptureAudit::default());
    let activity = Arc::new(CaptureActivity::default());
    let called = Arc::new(AtomicUsize::new(0));
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.reason = "no ticket".to_owned();
    middleware.auditor = Some(audit.clone());
    middleware.activity = Some(activity.clone());
    middleware.authz = Some(Arc::new(DenyAuthorizer));

    let called_for_handler = Arc::clone(&called);
    let output = middleware
        .run(
            middleware_request(
                CommandMeta {
                    auth_metadata: BTreeMap::from([("tier".to_owned(), "destructive".to_owned())]),
                    ..CommandMeta::default()
                },
                "things:delete",
                value_map([("id", json!("p1"))]),
                value_map([("id", json!("p1"))]),
                "",
                false,
            ),
            async |_credential| {
                called_for_handler.fetch_add(1, Ordering::SeqCst);
                Ok(CommandResult::new(json!({"deleted": true})))
            },
        )
        .await
        .expect("denial should render an error output");

    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert_eq!(output.exit_code, 6);
    assert_eq!(audit.results().await, vec!["denied"]);
    assert_eq!(activity.statuses().await, vec!["denied"]);
    assert_eq!(
        output.envelope.error.expect("error").message,
        "denied by test"
    );
}

#[tokio::test]
async fn middleware_success_records_pagination_metadata() {
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.limit = 2;
    middleware.offset = 1;
    middleware.verbose = "all".to_owned();

    let output = middleware
        .run(
            middleware_request_with_system(
                CommandMeta::default(),
                "things:list",
                "things-api",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_credential| {
                Ok(CommandResult::new(json!([
                        {"id": 1},
                        {"id": 2},
                        {"id": 3},
                        {"id": 4}
                ])))
            },
        )
        .await
        .expect("paginated output should render");

    assert_eq!(output.envelope.data, Some(json!([{"id": 2}, {"id": 3}])));
    let pagination = output
        .envelope
        .metadata
        .expect("metadata")
        .pagination
        .expect("pagination");
    assert_eq!(pagination.total, 4);
    assert_eq!(pagination.offset, 1);
    assert_eq!(pagination.limit, 2);
    assert_eq!(pagination.count, 2);
}

#[tokio::test]
async fn middleware_surfaces_pipeline_errors_and_null_success_without_data() {
    let mut filtered = Middleware::new();
    filtered.output_format = "json".to_owned();
    filtered.filter = "enabled".to_owned();

    let err = filtered
        .run_no_auth(
            CommandMeta::default(),
            "things:get",
            value_map([("id", json!("p1"))]),
            value_map([("id", json!("p1"))]),
            "",
            async || Ok(CommandResult::new(json!({"enabled": true}))),
        )
        .await
        .expect_err("filtering object data should fail");
    assert_eq!(
        err.to_string(),
        "filter requires list data; use --expr for single objects"
    );

    let mut null_success = Middleware::new();
    null_success.output_format = "json".to_owned();
    let output = null_success
        .run_no_auth(
            CommandMeta::default(),
            "things:noop",
            value_map([]),
            value_map([]),
            "",
            async || Ok(CommandResult::new(serde_json::Value::Null)),
        )
        .await
        .expect("null success should render without data");

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.envelope.data, Some(serde_json::Value::Null));

    assert_eq!(
        serde_json::Value::from(cli_engine::CliCoreError::message("plain error")),
        json!("plain error")
    );
}

#[tokio::test]
async fn middleware_covers_authorized_and_non_triggering_branch_combinations() {
    let mut authenticated = Middleware::new();
    authenticated
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    authenticated.default_auth_provider = "primary".to_owned();
    authenticated.output_format = "json".to_owned();
    authenticated.env = "prod".to_owned();
    authenticated.dry_run = true;
    authenticated.verbose = "all".to_owned();
    authenticated.authz = Some(Arc::new(AllowAuthorizer));

    let output = authenticated
        .run(
            middleware_request(
                CommandMeta {
                    dry_run_prompt: false,
                    auth_metadata: BTreeMap::from([
                        ("provider".to_owned(), "primary".to_owned()),
                        ("tier".to_owned(), "not-a-tier".to_owned()),
                    ]),
                    ..CommandMeta::default()
                },
                "things:update",
                value_map([("env", json!("test"))]),
                value_map([("env", json!("test"))]),
                "",
                false,
            ),
            async |_credential| Ok(CommandResult::new(json!({"ok": true}))),
        )
        .await
        .expect("authorized authenticated command should run");

    assert_eq!(output.exit_code, 0);
    assert_eq!(
        output
            .envelope
            .metadata
            .expect("metadata")
            .effective_args
            .expect("effective args"),
        json!({"env": "test"})
    );

    let mut no_auth = Middleware::new();
    no_auth.output_format = "json".to_owned();
    no_auth.dry_run = true;
    no_auth.authz = Some(Arc::new(AllowAuthorizer));
    let output = no_auth
        .run_no_auth(
            CommandMeta {
                dry_run_prompt: false,
                ..CommandMeta::default()
            },
            "things:list",
            value_map([]),
            value_map([]),
            "",
            async || Ok(CommandResult::new(json!([{"id": "p1"}]))),
        )
        .await
        .expect("authorized no-auth command should run");

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.envelope.data, Some(json!([{"id": "p1"}])));
}

#[test]
fn middleware_public_struct_derives_cover_serialization_debug_and_equality() {
    let event = ActivityEvent {
        timestamp: "2026-05-19T00:00:00Z".to_owned(),
        app: "my-cli".to_owned(),
        command: "things:list".to_owned(),
        env: "prod".to_owned(),
        backend: "things-api".to_owned(),
        identity: "tester".to_owned(),
        sub: "subject-1".to_owned(),
        account_type: "employee".to_owned(),
        status: "ok".to_owned(),
        error: String::new(),
        reason: "ticket-1".to_owned(),
        args: value_map([("id", json!("p1"))]),
        duration_ms: 12,
        meta: value_map([("source", json!("test"))]),
    };

    let encoded = serde_json::to_value(&event).expect("activity event should serialize");
    assert_eq!(encoded["command"], "things:list");
    assert_eq!(encoded["args"], json!({"id": "p1"}));
    let decoded: ActivityEvent =
        serde_json::from_value(encoded).expect("activity event should deserialize");
    assert_eq!(decoded, event);

    let middleware = Middleware::new();
    assert!(format!("{middleware:?}").contains("Middleware"));
    assert_eq!(middleware.clone().output_format, middleware.output_format);

    let output = cli_engine::MiddlewareOutput {
        envelope: Envelope::success(json!({"ok": true}), "things-api").prepare_for_render(""),
        rendered: "{\"ok\":true}".to_owned(),
        exit_code: 0,
    };
    assert_eq!(output.clone(), output);
    assert!(format!("{output:?}").contains("MiddlewareOutput"));
}

#[tokio::test]
async fn middleware_schema_short_circuit_precedes_no_auth_authorizer_and_dry_run() {
    #[derive(Debug)]
    struct SchemaThing;

    impl OutputSchema for SchemaThing {
        fn fields() -> &'static [OutputField] {
            &[OutputField {
                name: "name",
                field_type: "string",
                optional: false,
            }]
        }
    }

    let mut middleware = Middleware::new();
    middleware.output_format = "json".to_owned();
    middleware.schema = true;
    middleware.dry_run = true;
    middleware.authz = Some(Arc::new(DenyAuthorizer));
    middleware
        .schema_registry
        .register::<SchemaThing>("things:delete");
    let called = Arc::new(AtomicUsize::new(0));
    let called_for_handler = Arc::clone(&called);

    let output = middleware
        .run_no_auth(
            CommandMeta {
                dry_run_prompt: true,
                ..CommandMeta::default()
            },
            "things:delete",
            value_map([]),
            value_map([]),
            "",
            async || {
                called_for_handler.fetch_add(1, Ordering::SeqCst);
                Ok(CommandResult::new(json!({"deleted": true})))
            },
        )
        .await
        .expect("schema should bypass authorizer, dry-run, and command");

    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert_eq!(output.exit_code, 0);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "things:delete");
    assert_eq!(rendered["data"]["fields"][0]["name"], "name");
}

#[tokio::test]
async fn middleware_schema_without_registration_reports_no_schema_and_skips_command() {
    // `--schema` on a command with no registered output schema must NOT silently
    // run the command; it should report that no schema is registered.
    let mut middleware = Middleware::new();
    middleware.output_format = "json".to_owned();
    middleware.schema = true;
    let called = Arc::new(AtomicUsize::new(0));
    let called_for_handler = Arc::clone(&called);

    let output = middleware
        .run_no_auth(
            CommandMeta::default(),
            "things:list",
            value_map([]),
            value_map([]),
            "",
            async || {
                called_for_handler.fetch_add(1, Ordering::SeqCst);
                Ok(CommandResult::new(json!([{"name": "alpha"}])))
            },
        )
        .await
        .expect("schema request should render");

    assert_eq!(
        called.load(Ordering::SeqCst),
        0,
        "the command must not run under --schema"
    );
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"]["command"], "things:list");
    // Same `{command, fields}` shape as a real SchemaInfo response (empty fields).
    assert_eq!(rendered["data"]["fields"], serde_json::json!([]));
    let message = rendered["data"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("No output schema is registered"),
        "expected a no-schema message, got: {}",
        output.rendered
    );
}

#[tokio::test]
async fn middleware_human_output_default_fields_narrows_view_columns() {
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "human".to_owned();
    // `default_fields` is the default `--fields`, and it narrows a view's columns
    // just like `--fields` would. With `default_fields="name"`, only the `name`
    // column of the `things` view renders; `status` is omitted even though the
    // view defines it and the data carries it.
    middleware.human_views.register(HumanViewDef {
        schema_id: "things".to_owned(),
        columns: vec![
            TableColumn {
                field: "name".to_owned(),
                header: "Name".to_owned(),
                no_truncate: false,
            },
            TableColumn {
                field: "status".to_owned(),
                header: "Status".to_owned(),
                no_truncate: false,
            },
        ],
    });

    let output = middleware
        .run(
            middleware_request_with_view(
                CommandMeta::default(),
                "things:list",
                "things",
                value_map([]),
                value_map([]),
                "name",
                false,
            ),
            async |_credential| {
                Ok(CommandResult::new(json!([
                        {"name": "alpha", "status": "active"},
                        {"name": "beta", "status": "disabled"}
                ])))
            },
        )
        .await
        .expect("middleware human output should render");

    assert!(output.rendered.contains("NAME"), "{}", output.rendered);
    assert!(output.rendered.contains("alpha"), "{}", output.rendered);
    assert!(!output.rendered.contains("STATUS"), "{}", output.rendered);
    assert!(!output.rendered.contains("active"), "{}", output.rendered);
}

#[tokio::test]
async fn middleware_human_output_resolves_declared_view_id() {
    // The renderer resolves the view by the id the command declared, verbatim —
    // not derived from the command path or `system`. Here the declared id
    // (`projects-table`) matches neither, yet the view resolves and (with no
    // field selection) renders all its columns.
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "human".to_owned();
    middleware.human_views.register(HumanViewDef {
        schema_id: "projects-table".to_owned(),
        columns: vec![
            TableColumn {
                field: "name".to_owned(),
                header: "Name".to_owned(),
                no_truncate: false,
            },
            TableColumn {
                field: "status".to_owned(),
                header: "Status".to_owned(),
                no_truncate: false,
            },
        ],
    });

    let output = middleware
        .run(
            middleware_request_with_view(
                CommandMeta::default(),
                "things:list",
                "projects-table",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_credential| {
                Ok(CommandResult::new(json!([
                        {"name": "alpha", "status": "active"}
                ])))
            },
        )
        .await
        .expect("middleware human output should render");

    assert!(output.rendered.contains("STATUS"), "{}", output.rendered);
    assert!(output.rendered.contains("active"), "{}", output.rendered);
}

#[tokio::test]
async fn middleware_human_output_uses_custom_view_function_before_columns() {
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "human".to_owned();
    middleware.human_views.register(HumanViewDef {
        schema_id: "things:list".to_owned(),
        columns: vec![TableColumn {
            field: "name".to_owned(),
            header: "Name".to_owned(),
            no_truncate: false,
        }],
    });
    middleware.human_views.register_func("things:list", |data| {
        format!("custom:{}\n", data.as_array().map_or(0, Vec::len))
    });

    let output = middleware
        .run(
            middleware_request_with_view(
                CommandMeta::default(),
                "things:list",
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_credential| {
                Ok(CommandResult::new(json!([
                        {"name": "alpha"},
                        {"name": "beta"}
                ])))
            },
        )
        .await
        .expect("middleware custom human output should render");

    assert_eq!(output.rendered, "custom:2\n");
}

#[tokio::test]
async fn middleware_business_error_can_preserve_backend_system_preserves_legacy_command_func() {
    let activity = Arc::new(CaptureActivity::default());
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.verbose = "all".to_owned();
    middleware.activity = Some(activity.clone());

    let output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:create",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_credential| {
                Err::<CommandResult, _>(cli_engine::CliCoreError::message_for_system(
                    "things-api",
                    "backend rejected request",
                ))
            },
        )
        .await
        .expect("business errors are rendered into middleware output");

    let error = output.envelope.error.expect("error envelope");
    assert_eq!(error.system, "things-api");
    assert_eq!(error.message, "backend rejected request");
    let metadata = output.envelope.metadata.expect("metadata");
    assert_eq!(metadata.system, "things-api");
    let events = activity.events.lock().await;
    assert_eq!(events[0].backend, "things-api");
    assert_eq!(events[0].error, "backend rejected request");
}

#[tokio::test]
async fn middleware_dry_run_short_circuits_mutating_command() {
    let audit = Arc::new(CaptureAudit::default());
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.dry_run = true;
    middleware.verbose = "all".to_owned();
    middleware.auditor = Some(audit.clone());

    let mut auth_metadata = BTreeMap::new();
    auth_metadata.insert("tier".to_owned(), "mutate".to_owned());
    let meta = CommandMeta {
        dry_run_prompt: true,
        auth_metadata,
        scopes: Vec::new(),
    };

    let output = middleware
        .run(
            middleware_request(meta, "things:set", value_map([]), value_map([]), "", false),
            async |_credential| {
                Err::<CommandResult, _>(cli_engine::CliCoreError::message(
                    "business logic should not run during dry-run",
                ))
            },
        )
        .await
        .expect("dry-run should render success");

    assert_eq!(
        output.envelope.data,
        Some(json!({"action": "dry-run: would execute", "command": "things:set"}))
    );
    assert!(output.envelope.metadata.expect("metadata").dry_run);
    assert_eq!(audit.results().await, vec!["dry-run"]);
}

#[tokio::test]
async fn middleware_no_auth_still_runs_authorizer() {
    let audit = Arc::new(CaptureAudit::default());
    let mut middleware = Middleware::new();
    middleware.authz = Some(Arc::new(DenyAuthorizer));
    middleware.output_format = "json".to_owned();
    middleware.auditor = Some(audit.clone());
    middleware.verbose = "all".to_owned();

    let output = middleware
        .run_no_auth(
            CommandMeta::default(),
            "auth:login",
            value_map([]),
            value_map([]),
            "",
            async || Ok(CommandResult::new(json!({"status": "logged in"}))),
        )
        .await
        .expect("denied error should be rendered");

    assert_eq!(
        output.envelope.error.expect("error").message,
        "denied by test"
    );
    assert_eq!(audit.results().await, vec!["denied"]);
}

#[tokio::test]
async fn middleware_auth_error_audits_and_renders() {
    let audit = Arc::new(CaptureAudit::default());
    let mut middleware = Middleware::new();
    middleware.default_auth_provider = "missing".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.auditor = Some(audit.clone());
    middleware.verbose = "all".to_owned();

    let output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |credential: CredentialResolver| {
                // Lazy resolution surfaces the missing-provider error only when
                // the handler asks for the credential.
                credential.resolve().await?;
                Ok(CommandResult::new(json!({})))
            },
        )
        .await
        .expect("auth errors are rendered into middleware output");

    let error = output.envelope.error.expect("error envelope");
    assert_eq!(error.code, "ERROR");
    assert!(error.message.contains("auth: no provider registered"));
    assert_eq!(audit.results().await, vec!["auth-error"]);
}

#[tokio::test]
async fn middleware_auth_error_activity_attributes_provider_backend() {
    let activity = Arc::new(CaptureActivity::default());
    let mut middleware = Middleware::new();
    middleware.default_auth_provider = "missing".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.activity = Some(activity.clone());

    let _output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |credential: CredentialResolver| {
                credential.resolve().await?;
                Ok(CommandResult::new(json!({})))
            },
        )
        .await
        .expect("auth errors are rendered into middleware output");

    // Auth-provider failures attribute the activity backend to the provider
    // name, not the command path, so telemetry can distinguish them.
    assert_eq!(activity.statuses().await, vec!["auth-error"]);
    assert_eq!(activity.backends().await, vec!["missing"]);
}

#[tokio::test]
async fn middleware_schema_short_circuit_renders_registered_schema_after_auth() {
    #[derive(Debug)]
    struct Thing;

    impl OutputSchema for Thing {
        fn fields() -> &'static [OutputField] {
            &[OutputField {
                name: "name",
                field_type: "string",
                optional: false,
            }]
        }
    }

    let mut registry = SchemaRegistry::new();
    registry.register::<Thing>("things:list");
    let mut middleware = Middleware::new();
    middleware
        .auth
        .register(Arc::new(FakeProvider::new("primary", "tester")));
    middleware.default_auth_provider = "primary".to_owned();
    middleware.app_id = "test-app".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.verbose = "all".to_owned();
    middleware.schema = true;
    middleware.schema_registry = registry;

    let output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_credential| {
                Err::<CommandResult, _>(cli_engine::CliCoreError::message(
                    "business logic should not run for schema",
                ))
            },
        )
        .await
        .expect("schema should render");

    assert_eq!(
        output.envelope.data,
        Some(json!({
            "command": "things:list",
            "fields": [{"name": "name", "type": "string", "optional": false}]
        }))
    );
    assert_eq!(
        output.envelope.metadata.expect("metadata").system,
        "test-app"
    );
}

#[tokio::test]
async fn middleware_no_auth_schema_short_circuits_before_authorizer() {
    #[derive(Debug)]
    struct Thing;

    impl OutputSchema for Thing {
        fn fields() -> &'static [OutputField] {
            &[OutputField {
                name: "name",
                field_type: "string",
                optional: false,
            }]
        }
    }

    let mut registry = SchemaRegistry::new();
    registry.register::<Thing>("auth:status");
    let mut middleware = Middleware::new();
    middleware.authz = Some(Arc::new(DenyAuthorizer));
    middleware.app_id = "test-app".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.verbose = "all".to_owned();
    middleware.schema = true;
    middleware.schema_registry = registry;

    let output = middleware
        .run_no_auth(
            CommandMeta::default(),
            "auth:status",
            value_map([]),
            value_map([]),
            "",
            async || {
                Err(cli_engine::CliCoreError::message(
                    "business logic should not run for schema",
                ))
            },
        )
        .await
        .expect("schema should render before authz");

    assert_eq!(
        output.envelope.data,
        Some(json!({
            "command": "auth:status",
            "fields": [{"name": "name", "type": "string", "optional": false}]
        }))
    );
    assert!(output.envelope.error.is_none());
}

#[tokio::test]
async fn middleware_schema_includes_identity_when_authorizer_resolved() {
    #[derive(Debug)]
    struct Thing;

    impl OutputSchema for Thing {
        fn fields() -> &'static [OutputField] {
            &[OutputField {
                name: "name",
                field_type: "string",
                optional: false,
            }]
        }
    }

    let mut registry = SchemaRegistry::new();
    registry.register::<Thing>("things:list");
    let (provider, calls) = CountingProvider::new("counting");
    let mut middleware = counting_middleware(provider);
    // The authorizer resolves the credential; the schema short-circuit should
    // then carry that identity into the output metadata.
    middleware.authz = Some(Arc::new(ResolvingAuthorizer));
    middleware.verbose = "all".to_owned();
    middleware.schema = true;
    middleware.schema_registry = registry;

    let output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_resolver| {
                Err::<CommandResult, _>(cli_engine::CliCoreError::message(
                    "business logic should not run for schema",
                ))
            },
        )
        .await
        .expect("schema should render");

    assert_eq!(
        output.envelope.metadata.expect("metadata").identity,
        "counted-user"
    );
    // The authorizer resolved exactly once; schema rendering itself never
    // triggers an additional resolution.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn envelope_omits_metadata_without_verbose_and_filters_selective_verbose() {
    let mut envelope = Envelope::success(json!({"name": "thing"}), "things-api");
    envelope.with_context(
        "things:list",
        "prod",
        "tester",
        Duration::from_millis(42),
        Some(json!({"project": "p1"})),
        Some(json!({"project": "p1", "env": "prod"})),
    );

    let lean = envelope.prepare_for_render("");
    assert!(lean.metadata.is_none());

    let selective = envelope.prepare_for_render("system,duration");
    let metadata = selective.metadata.expect("metadata should be present");
    assert_eq!(metadata.system, "things-api");
    assert_eq!(metadata.duration, "42ms");
    assert_eq!(metadata.command, "");
    assert_eq!(metadata.env, "");
}

#[test]
fn envelope_context_duration_matches_legacy_rounded_duration_strings() {
    let mut envelope = Envelope::success(json!({"ok": true}), "things-api");

    envelope.with_context(
        "things:list",
        "prod",
        "tester",
        Duration::from_millis(1500),
        None,
        None,
    );
    assert_eq!(
        envelope.metadata.as_ref().expect("metadata").duration,
        "1.5s"
    );

    envelope.with_context(
        "things:list",
        "prod",
        "tester",
        Duration::from_millis(1010),
        None,
        None,
    );
    assert_eq!(
        envelope.metadata.as_ref().expect("metadata").duration,
        "1.01s"
    );

    envelope.with_context(
        "things:list",
        "prod",
        "tester",
        Duration::from_micros(500),
        None,
        None,
    );
    assert_eq!(
        envelope.metadata.as_ref().expect("metadata").duration,
        "1ms"
    );

    envelope.with_context(
        "things:list",
        "prod",
        "tester",
        Duration::from_micros(499),
        None,
        None,
    );
    assert_eq!(envelope.metadata.as_ref().expect("metadata").duration, "0s");
}

#[test]
fn json_renderer_uses_two_space_pretty_json() {
    let envelope = Envelope::success(json!({"name": "thing"}), "things-api").prepare_for_render("");

    let rendered = render(OutputFormat::Json, &envelope).expect("json render should succeed");

    assert_eq!(
        rendered,
        "{\n  \"data\": {\n    \"name\": \"thing\"\n  }\n}\n"
    );
}

#[test]
fn json_renderer_escapes_html_sensitive_characters_preserves_legacy_encoding_json() {
    let envelope = Envelope::success(
        json!({"message": "<tag>&value>\u{2028}next\u{2029}line"}),
        "things-api",
    )
    .prepare_for_render("");

    let rendered = render(OutputFormat::Json, &envelope).expect("json render should succeed");

    assert!(rendered.contains(r"\u003ctag\u003e\u0026value\u003e"));
    assert!(rendered.contains(r"\u2028"));
    assert!(rendered.contains(r"\u2029"));
    assert!(!rendered.contains("<tag>"));
    assert!(!rendered.contains("&value>"));
}

#[test]
fn null_success_data_omits_json_data_but_renders_human_nil_preserves_legacy() {
    let envelope = Envelope::success(serde_json::Value::Null, "things-api").prepare_for_render("");

    assert_eq!(
        render(OutputFormat::Json, &envelope).expect("json null should render"),
        "{}\n"
    );
    assert_eq!(
        render(OutputFormat::Human, &envelope).expect("human null should render"),
        "<nil>\n"
    );
}

#[test]
fn envelope_context_omits_empty_args_maps_preserves_legacy() {
    let mut envelope = Envelope::success(json!({"ok": true}), "things-api");
    envelope.with_context(
        "things:list",
        "prod",
        "tester",
        Duration::from_millis(1),
        Some(json!({})),
        Some(json!({})),
    );
    let rendered = render(OutputFormat::Json, &envelope).expect("json should render");

    assert!(!rendered.contains("\"args\""));
    assert!(!rendered.contains("\"effective_args\""));

    envelope.with_context(
        "things:list",
        "prod",
        "tester",
        Duration::from_millis(1),
        Some(json!({"name": "alpha"})),
        Some(json!({"env": "prod", "name": "alpha"})),
    );
    let rendered = render(OutputFormat::Json, &envelope).expect("json should render");

    assert!(rendered.contains("\"args\": {"));
    assert!(rendered.contains("\"name\": \"alpha\""));
    assert!(rendered.contains("\"effective_args\": {"));
    assert!(rendered.contains("\"env\": \"prod\""));
}

#[test]
fn output_convenience_helpers_match_legacy_render_data_error_and_error_detail() {
    let rendered =
        cli_engine::output::render_data(OutputFormat::Json, json!({"name": "thing"}), "things-api")
            .expect("render data should succeed");
    assert!(rendered.contains("\"name\": \"thing\""));
    assert!(rendered.contains("\"system\": \"things-api\""));

    let envelope = Envelope::error_detail("DENIED", "forbidden", "policy", "rid-1");
    let metadata = envelope.metadata.expect("metadata");
    let error = envelope.error.expect("error");
    assert_eq!(metadata.system, "policy");
    assert_eq!(metadata.request_id, "rid-1");
    assert_eq!(error.code, "DENIED");
    assert_eq!(error.system, "policy");
    assert_eq!(error.request_id, "rid-1");

    let err = cli_engine::CliCoreError::message_for_system("policy", "forbidden");
    let rendered = cli_engine::output::render_error(OutputFormat::Json, &err, "fallback")
        .expect("render error should succeed");
    assert!(rendered.contains("\"message\": \"forbidden\""));
    assert!(rendered.contains("\"system\": \"policy\""));
    assert_eq!(cli_engine::output::exit_code_for_error(&err), 5);
    assert_eq!(cli_engine::CACHE_TTL, chrono::Duration::minutes(30));
}

#[test]
fn output_string_format_helpers_default_unknown_direct_formats_to_json() {
    let envelope = Envelope::success(json!({"name": "thing"}), "things-api").prepare_for_render("");

    assert_eq!(
        "json".parse::<OutputFormat>().expect("json format"),
        OutputFormat::Json
    );
    assert_eq!(
        "human".parse::<OutputFormat>().expect("human format"),
        OutputFormat::Human
    );
    assert_eq!(
        "unknown"
            .parse::<OutputFormat>()
            .expect("unknown format falls back"),
        OutputFormat::Json
    );

    let rendered =
        cli_engine::output::render_format("unknown", &envelope).expect("unknown should fall back");
    assert!(rendered.contains("\"name\": \"thing\""));

    let rendered = cli_engine::output::render_data_format("json", json!({"name": "thing"}), "api")
        .expect("render data should accept string format");
    assert!(rendered.contains("\"name\": \"thing\""));

    let err = cli_engine::CliCoreError::message("denied");
    let rendered = cli_engine::output::render_error_format("human", &err, "api")
        .expect("render error should accept string format");
    assert_eq!(rendered, "Error: denied\n");

    let mut out = Vec::new();
    cli_engine::output::RendererFactory::new()
        .write(&mut out, "json", &envelope)
        .expect("factory should write rendered output");
    let out = String::from_utf8(out).expect("rendered output should be utf8");
    assert!(out.contains("\"name\": \"thing\""));
}

#[test]
fn output_render_data_returns_serialization_errors_preserves_legacy_render_data() {
    #[derive(Debug)]
    struct BadSerialize;

    impl serde::Serialize for BadSerialize {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("bad serialize"))
        }
    }

    let err = cli_engine::output::render_data_format("json", BadSerialize, "things-api")
        .expect_err("render_data_format should return serialization error");

    assert!(err.to_string().contains("bad serialize"));
}

#[test]
fn direct_success_envelope_preserves_serialization_errors_until_json_or_toon_render_preserves_legacy()
 {
    #[derive(Debug)]
    struct BadSerialize;

    impl serde::Serialize for BadSerialize {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("bad serialize"))
        }
    }

    let envelope = Envelope::success(BadSerialize, "things-api");
    for format in [OutputFormat::Json, OutputFormat::Toon, OutputFormat::Human] {
        let err = render(format, &envelope).expect_err("render should return serialization error");
        assert!(err.to_string().contains("bad serialize"));
    }
}

#[derive(Debug)]
struct CustomDetailedError {
    message: &'static str,
    code: &'static str,
    system: Option<&'static str>,
    request_id: Option<&'static str>,
}

impl std::fmt::Display for CustomDetailedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for CustomDetailedError {}

impl cli_engine::DetailedError for CustomDetailedError {
    fn error_code(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(self.code)
    }

    fn error_system(&self) -> Option<std::borrow::Cow<'static, str>> {
        self.system.map(std::borrow::Cow::Borrowed)
    }

    fn error_request_id(&self) -> Option<std::borrow::Cow<'static, str>> {
        self.request_id.map(std::borrow::Cow::Borrowed)
    }
}

#[test]
fn output_detailed_error_helpers_preserve_generic_structured_errors_preserves_legacy() {
    let err = CustomDetailedError {
        message: "backend rejected request",
        code: "BACKEND_REJECTED",
        system: Some("backend-api"),
        request_id: Some("req-123"),
    };

    let rendered = cli_engine::output::render_detailed_error_format("json", &err, "fallback-api")
        .expect("detailed error should render");
    let parsed: serde_json::Value =
        serde_json::from_str(&rendered).expect("rendered output should be json");
    assert_eq!(parsed["error"]["code"], "BACKEND_REJECTED");
    assert_eq!(parsed["error"]["message"], "backend rejected request");
    assert_eq!(parsed["error"]["system"], "backend-api");
    assert_eq!(parsed["error"]["request_id"], "req-123");
    assert_eq!(parsed["metadata"]["system"], "backend-api");
    assert_eq!(parsed["metadata"]["request_id"], "req-123");

    let err = CustomDetailedError {
        message: "plain backend error",
        code: "",
        system: None,
        request_id: None,
    };
    let envelope = cli_engine::output::build_detailed_error_envelope(&err, "fallback-api");
    let parsed = serde_json::to_value(envelope).expect("envelope should serialize");
    assert_eq!(parsed["error"]["code"], "ERROR");
    assert_eq!(parsed["error"]["system"], "fallback-api");
    assert!(parsed["error"].get("request_id").is_none());
}

#[derive(Debug)]
struct CustomExitCodeError {
    message: &'static str,
    code: i32,
}

impl std::fmt::Display for CustomExitCodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for CustomExitCodeError {}

impl cli_engine::ExitCoder for CustomExitCodeError {
    fn exit_code(&self) -> i32 {
        self.code
    }
}

#[test]
fn output_build_error_envelope_preserves_wrapped_transport_details_preserves_legacy_errors_as() {
    let err = cli_engine::CliCoreError::AuthProvider {
        provider: "primary".to_owned(),
        source: Box::new(transport::Error {
            code: "BACKEND_REJECTED".to_owned(),
            message: "backend rejected request".to_owned(),
            system: "backend-api".to_owned(),
            request_id: "req-123".to_owned(),
        }),
    };

    let envelope = cli_engine::output::build_error_envelope(&err, "fallback-api");

    let error = envelope.error.expect("error envelope");
    assert_eq!(error.code, "BACKEND_REJECTED");
    assert_eq!(
        error.message,
        "auth: provider \"primary\": backend rejected request"
    );
    assert_eq!(error.system, "backend-api");
    assert_eq!(error.request_id, "req-123");
    let metadata = envelope.metadata.expect("metadata");
    assert_eq!(metadata.system, "backend-api");
    assert_eq!(metadata.request_id, "req-123");
}

#[test]
fn output_build_error_envelope_preserves_wrapped_custom_detailed_error() {
    let err = cli_engine::CliCoreError::with_detailed_error(CustomDetailedError {
        message: "backend rejected request",
        code: "BACKEND_REJECTED",
        system: Some("backend-api"),
        request_id: Some("req-123"),
    });
    let framework_wrapped = cli_engine::CliCoreError::AuthProvider {
        provider: "primary".to_owned(),
        source: Box::new(err),
    };

    let envelope = cli_engine::output::build_error_envelope(&framework_wrapped, "fallback-api");

    let error = envelope.error.expect("error envelope");
    assert_eq!(error.code, "BACKEND_REJECTED");
    assert_eq!(
        error.message,
        "auth: provider \"primary\": backend rejected request"
    );
    assert_eq!(error.system, "backend-api");
    assert_eq!(error.request_id, "req-123");
    let metadata = envelope.metadata.expect("metadata");
    assert_eq!(metadata.system, "backend-api");
    assert_eq!(metadata.request_id, "req-123");

    let detailed_source_wins = cli_engine::CliCoreError::with_system(
        "fallback-api",
        cli_engine::CliCoreError::with_detailed_error(CustomDetailedError {
            message: "backend rejected request",
            code: "BACKEND_REJECTED",
            system: Some("backend-api"),
            request_id: Some("req-123"),
        }),
    );
    let envelope = cli_engine::output::build_error_envelope(&detailed_source_wins, "app-api");
    let error = envelope.error.expect("error envelope");
    assert_eq!(error.code, "BACKEND_REJECTED");
    assert_eq!(error.system, "backend-api");
    assert_eq!(error.request_id, "req-123");

    let detailed_empty_system_uses_wrapper_fallback = cli_engine::CliCoreError::with_system(
        "fallback-api",
        cli_engine::CliCoreError::with_detailed_error(CustomDetailedError {
            message: "backend rejected request",
            code: "BACKEND_REJECTED",
            system: None,
            request_id: Some("req-123"),
        }),
    );
    let envelope = cli_engine::output::build_error_envelope(
        &detailed_empty_system_uses_wrapper_fallback,
        "app-api",
    );
    let error = envelope.error.expect("error envelope");
    assert_eq!(error.code, "BACKEND_REJECTED");
    assert_eq!(error.system, "fallback-api");
    assert_eq!(error.request_id, "req-123");
}

#[test]
fn output_exit_code_helper_preserves_custom_exit_coder_preserves_legacy() {
    let err = CustomExitCodeError {
        message: "auth invalid denied",
        code: 42,
    };

    assert_eq!(cli_engine::exit_code_for_exit_coder(&err), 42);
    assert_eq!(cli_engine::output::exit_code_for_exit_coder(&err), 42);
    assert_eq!(cli_engine::exit_code_for_error(&err), 2);

    let wrapped = cli_engine::CliCoreError::with_exit_code(42, err);
    assert_eq!(cli_engine::exit_code_for_error(&wrapped), 42);
    assert_eq!(wrapped.to_string(), "auth invalid denied");

    let framework_wrapped = cli_engine::CliCoreError::AuthProvider {
        provider: "primary".to_owned(),
        source: Box::new(wrapped),
    };
    assert_eq!(cli_engine::exit_code_for_error(&framework_wrapped), 42);

    let system_wrapped = cli_engine::CliCoreError::with_system(
        "things-api",
        cli_engine::CliCoreError::with_exit_code(
            43,
            CustomExitCodeError {
                message: "auth invalid denied",
                code: 43,
            },
        ),
    );
    assert_eq!(cli_engine::exit_code_for_error(&system_wrapped), 43);
}

#[test]
fn output_module_reexports_error_traits_preserves_legacy_output_package() {
    fn assert_exit_coder<T: cli_engine::output::ExitCoder>(value: &T) -> i32 {
        value.exit_code()
    }

    fn detailed_code<T: cli_engine::output::DetailedError>(value: &T) -> String {
        value.error_code().into_owned()
    }

    let exit = CustomExitCodeError {
        message: "wrapped exit",
        code: 77,
    };
    assert_eq!(assert_exit_coder(&exit), 77);

    let detailed = CustomDetailedError {
        message: "backend rejected request",
        code: "BACKEND_REJECTED",
        system: Some("backend-api"),
        request_id: Some("req-123"),
    };
    assert_eq!(detailed_code(&detailed), "BACKEND_REJECTED");
}

#[test]
fn toon_renderer_matches_toon_core_shapes() {
    let envelope = Envelope::success(
        json!({
            "items": [
                {"id": 1, "name": "Alice", "role": "admin"},
                {"id": 2, "name": "Bob", "role": "user"}
            ]
        }),
        "things-api",
    )
    .prepare_for_render("");

    let rendered = render(OutputFormat::Toon, &envelope).expect("toon render should succeed");

    assert_eq!(
        rendered,
        "data:\n  items[2]{id,name,role}:\n    1,Alice,admin\n    2,Bob,user"
    );
}

#[test]
fn toon_renderer_quotes_unsafe_strings_and_mixed_arrays() {
    let envelope = Envelope::success(
        json!({
            "items": [
                1,
                "hello, world",
                {"a": "001", "b": "-dash"}
            ]
        }),
        "things-api",
    )
    .prepare_for_render("");

    let rendered = render(OutputFormat::Toon, &envelope).expect("toon render should succeed");

    assert_eq!(
        rendered,
        "data:\n  items[3]:\n    - 1\n    - \"hello, world\"\n    - a: \"001\"\n      b: \"-dash\""
    );
}

#[test]
fn toon_renderer_covers_nested_empty_and_escaped_goldens() {
    let cases = [
        (
            json!({
                "empty": [],
                "matrix": [[1, 2], ["a,b", "c"]],
                "nothing": null,
                "safe": "plain",
                "unsafe": "line\nquote\"slash\\tab\t",
            }),
            "data:\n  empty[0]:\n  matrix[2]:\n    - [2]: 1,2\n    - [2]: \"a,b\",c\n  nothing: null\n  safe: plain\n  unsafe: \"line\\nquote\\\"slash\\\\tab\\t\"",
        ),
        (
            json!({
                "items": [
                    {"id": "p1", "nested": {"enabled": true, "owner": "platform"}},
                    {"id": "p2", "nested": {}},
                    {}
                ]
            }),
            "data:\n  items[3]:\n    - id: p1\n      nested:\n        enabled: true\n        owner: platform\n    - id: p2\n      nested:\n    -",
        ),
        (
            json!({
                "bad-key": {
                    "0name": "numeric-key",
                    "has space": "space-key",
                    "ok.name": "dot-key"
                }
            }),
            "data:\n  \"bad-key\":\n    \"0name\": numeric-key\n    \"has space\": space-key\n    ok.name: dot-key",
        ),
    ];

    for (data, expected) in cases {
        let envelope = Envelope::success(data, "things-api").prepare_for_render("");
        assert_eq!(
            render(OutputFormat::Toon, &envelope).expect("toon render should succeed"),
            expected
        );
    }
}

#[test]
fn toon_renderer_covers_nested_array_and_non_tabular_object_paths() {
    let envelope = Envelope::success(
        json!({
            "items": [
                {
                    "matrix": [
                        1,
                        [2, 3],
                        {"name": "nested"}
                    ],
                    "name": "first"
                },
                {
                    "matrix": [
                        null,
                        true
                    ],
                    "name": "second"
                }
            ],
            "nonTabular": [
                {"id": "one", "value": 1},
                {"id": "two", "extra": "field", "value": 2}
            ],
            "objectFirst": [
                {"details": {"owner": "platform"}, "id": "p1"},
                {"details": [], "id": "p2"}
            ]
        }),
        "things-api",
    )
    .prepare_for_render("");

    let rendered = render(OutputFormat::Toon, &envelope).expect("toon render should succeed");

    assert_eq!(
        rendered,
        "data:\n  items[2]:\n    - matrix[3]:\n      - 1\n      - [2]: 2,3\n      - name: nested\n      name: first\n    - matrix[2]: null,true\n      name: second\n  nonTabular[2]:\n    - id: one\n      value: 1\n    - extra: field\n      id: two\n      value: 2\n  objectFirst[2]:\n    - details:\n        owner: platform\n      id: p1\n    - details[0]:\n      id: p2"
    );
}

#[test]
fn human_renderer_matches_generic_table_behavior() {
    let envelope = Envelope::success(
        json!([
            {"name": "alpha", "enabled": true},
            {"name": "beta", "enabled": false}
        ]),
        "things-api",
    );

    let rendered = render(OutputFormat::Human, &envelope).expect("human render should succeed");

    assert_eq!(
        rendered,
        "ENABLED  NAME \n-------  -----\nyes      alpha\nno       beta \n\n(2 rows)\n"
    );
}

#[test]
fn human_renderer_preserves_json_number_text() {
    let envelope = Envelope::success(
        json!([
            {"name": "alpha", "ratio": 1.0, "score": 1.25}
        ]),
        "things-api",
    );

    let rendered = render(OutputFormat::Human, &envelope).expect("human render should succeed");

    assert_eq!(
        rendered,
        "NAME   RATIO  SCORE\n-----  -----  -----\nalpha  1.0    1.25 \n\n(1 rows)\n"
    );
}

#[test]
fn human_renderer_formats_non_integer_json_floats_with_serde_json_text() {
    let envelope = Envelope::success(
        json!([
            {"name": "large", "score": 1000000.5},
            {"name": "small", "score": 0.00000012345}
        ]),
        "things-api",
    );

    assert_eq!(
        render(OutputFormat::Human, &envelope).expect("floats should render"),
        "NAME   SCORE    \n-----  ---------\nlarge  1000000.5\nsmall  1.2345e-7\n\n(2 rows)\n"
    );
}

#[test]
fn human_renderer_matches_legacy_scalar_and_non_object_array_fallbacks() {
    let bool_envelope = Envelope::success(json!(true), "things-api");
    let null_envelope = Envelope::success(serde_json::Value::Null, "things-api");
    let array_envelope = Envelope::success(json!([true, false, null, "text", 7]), "things-api");

    assert_eq!(
        render(OutputFormat::Human, &bool_envelope).expect("bool should render"),
        "true\n"
    );
    assert_eq!(
        render(OutputFormat::Human, &null_envelope).expect("null should render"),
        "<nil>\n"
    );
    assert_eq!(
        render(OutputFormat::Human, &array_envelope).expect("array should render"),
        "true\nfalse\n<nil>\ntext\n7\n"
    );
}

#[test]
fn human_renderer_object_values_render_as_plain_key_value_lines() {
    let envelope = Envelope::success(
        json!({
            "a": "one",
            "name": "alpha"
        }),
        "things-api",
    );

    assert_eq!(
        render(OutputFormat::Human, &envelope).expect("object should render"),
        "a: one\nname: alpha\n"
    );
}

#[test]
fn human_renderer_mixed_object_scalar_array_falls_back_to_lines() {
    let envelope = Envelope::success(
        json!([
            {"name": "alpha"},
            true
        ]),
        "things-api",
    );

    assert_eq!(
        render(OutputFormat::Human, &envelope).expect("mixed array should render"),
        "{\"name\":\"alpha\"}\ntrue\n"
    );
}

#[test]
fn human_renderer_column_mixed_object_scalar_array_falls_back_to_lines() {
    let columns = vec![TableColumn {
        field: "name".to_owned(),
        header: "Name".to_owned(),
        no_truncate: false,
    }];
    let envelope = Envelope::success(
        json!([
            {"name": "alpha"},
            true
        ]),
        "things-api",
    );

    assert_eq!(
        render_human_with_view(&envelope, Some(&columns), ""),
        "{\"name\":\"alpha\"}\ntrue\n"
    );
}

#[test]
fn human_view_registry_renders_registered_columns_for_lists() {
    let mut registry = HumanViewRegistry::new();
    registry.register(HumanViewDef {
        schema_id: "things".to_owned(),
        columns: vec![
            TableColumn {
                field: "name".to_owned(),
                header: "Name".to_owned(),
                no_truncate: false,
            },
            TableColumn {
                field: "enabled".to_owned(),
                header: "Enabled".to_owned(),
                no_truncate: false,
            },
        ],
    });
    let envelope = Envelope::success(
        json!([
            {"name": "alpha", "enabled": true, "ignored": "x"},
            {"name": "beta", "enabled": false, "ignored": "y"}
        ]),
        "things",
    );

    let rendered = render_human_with_view(&envelope, registry.columns("things"), "");

    assert_eq!(
        rendered,
        "NAME   ENABLED\n-----  -------\nalpha  yes    \nbeta   no     \n\n(2 rows)\n"
    );
}

#[test]
fn human_view_registry_renders_registered_columns_for_objects() {
    let columns = vec![
        TableColumn {
            field: "name".to_owned(),
            header: "Name".to_owned(),
            no_truncate: false,
        },
        TableColumn {
            field: "missing".to_owned(),
            header: "Missing".to_owned(),
            no_truncate: false,
        },
    ];
    let envelope = Envelope::success(json!({"name": "alpha", "ignored": "x"}), "things");

    let rendered = render_human_with_view(&envelope, Some(&columns), "");

    assert_eq!(rendered, "Name: alpha\nMissing: \n");
}

#[test]
fn human_view_registry_custom_renderer_wins_over_columns_preserves_legacy_view_func() {
    let mut registry = HumanViewRegistry::new();
    registry.register(HumanViewDef {
        schema_id: "things".to_owned(),
        columns: vec![TableColumn {
            field: "name".to_owned(),
            header: "Name".to_owned(),
            no_truncate: false,
        }],
    });
    registry.register_func("things", |data| {
        format!(
            "custom:{}\n",
            data.get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        )
    });
    let envelope = Envelope::success(json!({"name": "alpha"}), "things");

    let rendered = cli_engine::render_human_with_registry(&envelope, &registry);

    assert_eq!(rendered, "custom:alpha\n");
}

#[test]
fn global_human_view_func_registration_can_be_looked_up_and_rendered() {
    cli_engine::register_global_human_view_func("custom-global", |data| {
        format!(
            "global:{}\n",
            data.get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        )
    });

    let renderer =
        cli_engine::lookup_global_human_view_func("custom-global").expect("global custom view");
    assert_eq!(renderer.render(&json!({"name": "alpha"})), "global:alpha\n");
}

#[test]
fn filter_fields_supports_nested_dot_paths_and_whole_key_override() {
    let data = json!({
        "id": "root",
        "content": {"text": "hello", "format": "md"},
        "items": [
            {"name": "a", "status": "active"},
            {"name": "b", "status": "disabled"}
        ]
    });

    assert_eq!(
        filter_fields(&data, "content.text,items.name"),
        json!({
            "content": {"text": "hello"},
            "items": [{"name": "a"}, {"name": "b"}]
        })
    );
    assert_eq!(
        filter_fields(&data, "content,content.text"),
        json!({"content": {"text": "hello", "format": "md"}})
    );
}

#[test]
fn filter_fields_returns_original_mixed_scalar_arrays_preserves_legacy() {
    let data = json!([
        true,
        {"name": "alpha", "ignored": "x"}
    ]);

    assert_eq!(filter_fields(&data, "name"), data);
}

#[test]
fn filter_fields_filters_null_and_object_arrays_preserves_legacy() {
    let data = json!([
        null,
        {"name": "alpha", "ignored": "x"}
    ]);

    assert_eq!(
        filter_fields(&data, "name"),
        json!([
            null,
            {"name": "alpha"}
        ])
    );
}

#[test]
fn output_pipeline_applies_filter_pagination_expr_and_fields_in_order() {
    let mut data = json!([
        {"name": "alpha", "status": "active", "enabled": true, "size": 10},
        {"name": "beta", "status": "disabled", "enabled": false, "size": 20},
        {"name": "gamma", "status": "active", "enabled": true, "size": 30}
    ]);
    let pagination = apply_pipeline(
        &mut data,
        &PipelineOpts {
            filter: "status == 'active'".to_owned(),
            limit: 1,
            offset: 1,
            expr: String::new(),
            fields: "name,status".to_owned(),
        },
    )
    .expect("pipeline should apply");

    assert_eq!(data, json!([{"name": "gamma", "status": "active"}]));
    assert_eq!(
        pagination,
        Some(cli_engine::PaginationMeta {
            total: 2,
            offset: 1,
            limit: 1,
            count: 1,
        })
    );
}

#[test]
fn output_pipeline_supports_documented_jmespath_examples() {
    let mut data = json!([
        {"name": "alpha", "enabled": true},
        {"name": "beta", "enabled": false}
    ]);
    apply_pipeline(
        &mut data,
        &PipelineOpts {
            expr: "[].name".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("projection query should apply");
    assert_eq!(data, json!(["alpha", "beta"]));

    let mut data = json!([{"name": "alpha"}, {"name": "beta"}]);
    apply_pipeline(
        &mut data,
        &PipelineOpts {
            expr: "length(@)".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("length query should apply");
    assert_eq!(data, json!(2));
}

#[test]
fn output_pipeline_negative_limit_with_positive_offset_matches_legacy() {
    let mut data = json!([
        {"name": "alpha"},
        {"name": "beta"},
        {"name": "gamma"}
    ]);
    let pagination = apply_pipeline(
        &mut data,
        &PipelineOpts {
            offset: 1,
            limit: -1,
            ..PipelineOpts::default()
        },
    )
    .expect("pipeline should apply");

    assert_eq!(
        data,
        json!([
            {"name": "beta"},
            {"name": "gamma"}
        ])
    );
    assert_eq!(
        pagination,
        Some(cli_engine::PaginationMeta {
            total: 3,
            offset: 1,
            limit: -1,
            count: 2,
        })
    );
}

#[test]
fn output_pipeline_defaults_and_non_list_pagination_are_noops() {
    assert_eq!(
        PipelineOpts::default(),
        PipelineOpts {
            filter: String::new(),
            limit: 0,
            offset: 0,
            expr: String::new(),
            fields: String::new(),
        }
    );

    let mut object = json!({"name": "alpha"});
    let pagination = apply_pipeline(
        &mut object,
        &PipelineOpts {
            offset: 1,
            limit: 2,
            ..PipelineOpts::default()
        },
    )
    .expect("pagination on non-list data should be a no-op");

    assert_eq!(object, json!({"name": "alpha"}));
    assert_eq!(pagination, None);
}

#[test]
fn output_pipeline_filter_supports_jmespath_contains_and_bool_fields() {
    let mut data = json!([
        {"name": "example-alpha", "enabled": true},
        {"name": "beta", "enabled": true},
        {"name": "example-gamma", "enabled": false}
    ]);
    apply_pipeline(
        &mut data,
        &PipelineOpts {
            filter: "contains(name, 'example')".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("contains filter should apply");
    assert_eq!(
        data,
        json!([
            {"name": "example-alpha", "enabled": true},
            {"name": "example-gamma", "enabled": false}
        ])
    );

    apply_pipeline(
        &mut data,
        &PipelineOpts {
            filter: "enabled".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("bool filter should apply");
    assert_eq!(data, json!([{"name": "example-alpha", "enabled": true}]));
}

#[test]
fn output_pipeline_filter_supports_common_jmespath_predicates() {
    let mut data = json!([
        {"name": "alpha", "enabled": true, "size": 10, "meta": {"region": "us-west-2"}},
        {"name": "beta", "enabled": false, "size": 20, "meta": {"region": "us-east-1"}},
        {"name": "gamma", "enabled": true, "size": 30, "meta": {"region": "us-west-2"}}
    ]);

    apply_pipeline(
        &mut data,
        &PipelineOpts {
            filter: "size >= `20` && enabled".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("numeric and boolean filter should apply");
    assert_eq!(
        data,
        json!([{"name": "gamma", "enabled": true, "size": 30, "meta": {"region": "us-west-2"}}])
    );

    apply_pipeline(
        &mut data,
        &PipelineOpts {
            filter: "meta.region != 'us-east-1'".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("nested inequality should apply");
    assert_eq!(
        data,
        json!([{"name": "gamma", "enabled": true, "size": 30, "meta": {"region": "us-west-2"}}])
    );
}

#[test]
fn output_pipeline_filter_supports_jmespath_membership_and_negation() {
    let mut data = json!([
        {"name": "alpha", "status": "active", "disabled": false, "deleted": false},
        {"name": "beta", "status": "pending", "disabled": true, "deleted": false},
        {"name": "gamma", "status": "disabled", "disabled": false, "deleted": false},
        {"name": "delta", "status": "active", "disabled": false, "deleted": true}
    ]);

    apply_pipeline(
        &mut data,
        &PipelineOpts {
            filter: "contains(['active', 'pending'], status) && !disabled && !deleted".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("in and negated filters should apply");
    assert_eq!(
        data,
        json!([{"name": "alpha", "status": "active", "disabled": false, "deleted": false}])
    );
}

#[test]
fn output_pipeline_expr_supports_common_jmespath_collection_transforms() {
    let data = json!([
        {"name": "alpha", "enabled": true, "size": 10, "meta": {"region": "us-west-2"}},
        {"name": "beta", "enabled": false, "size": 20, "meta": {"region": "us-east-1"}},
        {"name": "gamma", "enabled": true, "size": 30, "meta": {"region": "us-west-2"}}
    ]);

    let mut filtered = data.clone();
    apply_pipeline(
        &mut filtered,
        &PipelineOpts {
            expr: "[?size > `10` && enabled == `true`]".to_owned(),
            fields: "name,size".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("filter expression should apply");
    assert_eq!(filtered, json!([{"name": "gamma", "size": 30}]));

    let mut projected = data.clone();
    apply_pipeline(
        &mut projected,
        &PipelineOpts {
            expr: "[].{name: name, region: meta.region}".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("object projection should apply");
    assert_eq!(
        projected,
        json!([
            {"name": "alpha", "region": "us-west-2"},
            {"name": "beta", "region": "us-east-1"},
            {"name": "gamma", "region": "us-west-2"}
        ])
    );

    let mut disabled_names = data.clone();
    apply_pipeline(
        &mut disabled_names,
        &PipelineOpts {
            expr: "[?enabled == `false`].name".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("filter projection query should apply");
    assert_eq!(disabled_names, json!(["beta"]));

    let mut west_count = data.clone();
    apply_pipeline(
        &mut west_count,
        &PipelineOpts {
            expr: "length([?meta.region == 'us-west-2'])".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("filtered length query should apply");
    assert_eq!(west_count, json!(2));

    let mut total_size = data;
    apply_pipeline(
        &mut total_size,
        &PipelineOpts {
            expr: "sum([].size)".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("sum projection query should apply");
    assert_eq!(total_size, json!(60.0));
}

#[test]
fn output_pipeline_expr_collection_predicates_use_jmespath_filters() {
    let data = json!([
        {"name": "alpha", "status": "active", "deleted": false},
        {"name": "beta", "status": "pending", "deleted": false},
        {"name": "gamma", "status": "disabled", "deleted": true}
    ]);

    let mut matching_statuses = data.clone();
    apply_pipeline(
        &mut matching_statuses,
        &PipelineOpts {
            expr: "[?contains(['active', 'pending'], status)].name".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("membership filter query should apply");
    assert_eq!(matching_statuses, json!(["alpha", "beta"]));

    let mut all_not_deleted = data.clone();
    apply_pipeline(
        &mut all_not_deleted,
        &PipelineOpts {
            expr: "length([?deleted == `false`]) == length(@)".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("whole-list predicate query should apply");
    assert_eq!(all_not_deleted, json!(false));

    let mut filtered = data;
    apply_pipeline(
        &mut filtered,
        &PipelineOpts {
            expr: "[?contains(['active', 'pending'], status) && !deleted]".to_owned(),
            fields: "name".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("filter with membership and not query should apply");
    assert_eq!(filtered, json!([{"name": "alpha"}, {"name": "beta"}]));
}

#[test]
fn output_pipeline_supports_indexed_paths() {
    let data = json!([
        {"name": "alpha", "tags": ["prod", "api"], "meta": {"envs": ["dev", "prod"]}},
        {"name": "beta", "tags": ["dev"], "meta": {"envs": ["test", "stage"]}}
    ]);

    let mut filtered = data.clone();
    apply_pipeline(
        &mut filtered,
        &PipelineOpts {
            filter: "tags[0] == 'prod'".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("indexed filter path should apply");
    assert_eq!(
        filtered,
        json!([{"name": "alpha", "tags": ["prod", "api"], "meta": {"envs": ["dev", "prod"]}}])
    );

    let mut projected = data.clone();
    apply_pipeline(
        &mut projected,
        &PipelineOpts {
            expr: "[].{name: name, firstTag: tags[0], secondEnv: meta.envs[1]}".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("indexed projection path should apply");
    assert_eq!(
        projected,
        json!([
            {"name": "alpha", "firstTag": "prod", "secondEnv": "prod"},
            {"name": "beta", "firstTag": "dev", "secondEnv": "stage"}
        ])
    );

    let mut second_name = data;
    apply_pipeline(
        &mut second_name,
        &PipelineOpts {
            expr: "[1].name".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("indexed data path should apply");
    assert_eq!(second_name, json!("beta"));
}

#[test]
fn output_pipeline_supports_jmespath_membership_strings_and_collection_queries() {
    let data = json!([
        {"name": "api-alpha", "status": "active", "score": 3, "meta": {"owner": "platform"}},
        {"name": "web-beta", "status": "pending", "score": 7, "meta": {"owner": "commerce"}},
        {"name": "api-gamma", "status": "disabled", "score": 11, "meta": {"owner": "platform"}}
    ]);

    let mut filtered = data.clone();
    apply_pipeline(
        &mut filtered,
        &PipelineOpts {
            filter: "starts_with(name, 'api') && contains([`1`, `2`, `3`, `4`, `5`, `6`, `7`, `8`, `9`, `10`], score) && !contains(['disabled'], status)".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("membership and string predicates should apply");
    assert_eq!(
        filtered,
        json!([{"name": "api-alpha", "status": "active", "score": 3, "meta": {"owner": "platform"}}])
    );

    let mut suffix_filtered = data.clone();
    apply_pipeline(
        &mut suffix_filtered,
        &PipelineOpts {
            filter: "ends_with(name, 'gamma') || starts_with(name, 'web-')".to_owned(),
            fields: "name".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("ends_with, starts_with, and or should apply");
    assert_eq!(
        suffix_filtered,
        json!([{"name": "web-beta"}, {"name": "api-gamma"}])
    );

    let mut count_platform = data.clone();
    apply_pipeline(
        &mut count_platform,
        &PipelineOpts {
            expr: "length([?meta.owner == 'platform'])".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("filtered length query should apply");
    assert_eq!(count_platform, json!(2));

    let mut none_archived = data.clone();
    apply_pipeline(
        &mut none_archived,
        &PipelineOpts {
            expr: "length([?status == 'archived']) == `0`".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("none-style query should apply");
    assert_eq!(none_archived, json!(true));

    let mut find_pending = data.clone();
    apply_pipeline(
        &mut find_pending,
        &PipelineOpts {
            expr: "[?status == 'pending'] | [0]".to_owned(),
            fields: "name,status".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("find-style query should apply");
    assert_eq!(
        find_pending,
        json!({"name": "web-beta", "status": "pending"})
    );

    let mut api_names = data;
    apply_pipeline(
        &mut api_names,
        &PipelineOpts {
            expr: "[?starts_with(name, 'api')].name".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("filtered projection query should apply");
    assert_eq!(api_names, json!(["api-alpha", "api-gamma"]));
}

#[test]
fn output_pipeline_supports_jmespath_negative_indices_and_bracket_paths() {
    let mut data = json!([
        {"name": "alpha", "labels": {"primary": "blue"}, "scores": [1, 2, 3]},
        {"name": "beta", "labels": {"primary": "green"}, "scores": [4, 5, 6]}
    ]);

    apply_pipeline(
        &mut data,
        &PipelineOpts {
            expr: "[].{name: name, primary: labels.primary, lastScore: scores[-1]}".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("negative indices and string bracket paths should apply");
    assert_eq!(
        data,
        json!([
            {"name": "alpha", "primary": "blue", "lastScore": 3},
            {"name": "beta", "primary": "green", "lastScore": 6}
        ])
    );
}

#[test]
fn output_pipeline_supports_jmespath_slice_paths_and_null_projection() {
    let data = json!([
        {"name": "alpha", "scores": [1, 2, 3, 4], "owner": {"name": "Ada"}},
        {"name": "beta", "scores": [5, 6, 7, 8], "owner": null}
    ]);

    let mut sliced = data.clone();
    apply_pipeline(
        &mut sliced,
        &PipelineOpts {
            expr: "[0].scores[1:-1]".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("slice path expression should apply");
    assert_eq!(sliced, json!([2, 3]));

    let mut projected = data;
    apply_pipeline(
        &mut projected,
        &PipelineOpts {
            expr: "[].{name: name, middleScores: scores[:2], ownerName: owner.name}".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("slice projection and optional chain should apply");
    assert_eq!(
        projected,
        json!([
            {"name": "alpha", "middleScores": [1, 2], "ownerName": "Ada"},
            {"name": "beta", "middleScores": [5, 6], "ownerName": null}
        ])
    );
}

#[test]
fn output_pipeline_supports_jmespath_or_fallback_for_optional_paths() {
    let mut missing_owner = json!({"owner": null, "name": "beta"});
    apply_pipeline(
        &mut missing_owner,
        &PipelineOpts {
            expr: "owner.name || 'unassigned'".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("or expression should apply fallback");
    assert_eq!(missing_owner, json!("unassigned"));

    let mut present_owner = json!({"owner": {"name": "Ada"}, "name": "alpha"});
    apply_pipeline(
        &mut present_owner,
        &PipelineOpts {
            expr: "owner.name || 'unassigned'".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("or expression should keep present value");
    assert_eq!(present_owner, json!("Ada"));
}

#[test]
fn output_pipeline_supports_jmespath_aggregate_and_sort_transforms() {
    let mut numbers = json!([5, 1, 9, 3]);
    apply_pipeline(
        &mut numbers,
        &PipelineOpts {
            expr: "avg(@)".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("avg aggregate should apply");
    assert_eq!(numbers, json!(4.5));

    let mut sorted_numbers = json!([5, 1, 9, 3]);
    apply_pipeline(
        &mut sorted_numbers,
        &PipelineOpts {
            expr: "reverse(sort(@))".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("sort query should apply");
    assert_eq!(sorted_numbers, json!([9, 5, 3, 1]));

    let data = json!([
        {"name": "gamma", "status": "active", "score": 30},
        {"name": "alpha", "status": "pending", "score": 10},
        {"name": "beta", "status": "active", "score": 20}
    ]);

    let mut by_score = data.clone();
    apply_pipeline(
        &mut by_score,
        &PipelineOpts {
            expr: "sort_by(@, &score)".to_owned(),
            fields: "name,score".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("sort_by query should apply");
    assert_eq!(
        by_score,
        json!([
            {"name": "alpha", "score": 10},
            {"name": "beta", "score": 20},
            {"name": "gamma", "score": 30}
        ])
    );

    let mut active_names = data;
    apply_pipeline(
        &mut active_names,
        &PipelineOpts {
            expr: "[?status == 'active'].name".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect("filtered projection query should apply");
    assert_eq!(active_names, json!(["gamma", "beta"]));
}

#[test]
fn schema_registry_returns_legacy_compatible_schema_shape_and_help_section() {
    #[derive(Debug)]
    struct Thing;

    impl OutputSchema for Thing {
        fn fields() -> &'static [OutputField] {
            const FIELDS: &[OutputField] = &[
                OutputField::string("name"),
                OutputField::bool("enabled").optional(),
            ];
            FIELDS
        }
    }

    let mut registry = SchemaRegistry::new();
    registry.register::<Thing>("things:list");
    let schema = registry
        .get_by_path("things:list")
        .expect("schema should be registered");

    assert_eq!(schema.command, "things:list");
    assert_eq!(schema.fields[0].name, "name");
    assert_eq!(schema.fields[0].field_type, "string");
    assert!(!schema.fields[0].optional);
    assert_eq!(schema.fields[1].name, "enabled");
    assert!(schema.fields[1].optional);

    let help = format_help_section(&schema.fields);
    assert!(help.contains("Output fields:"));
    assert!(help.contains("name     string"));
    assert!(help.contains("enabled  bool  (optional)"));
    assert!(help.contains("--filter \"contains(name, 'example')\""));
    assert!(help.contains("--filter 'enabled'"));
    assert!(help.contains("--expr 'length(@)'"));
    assert!(help.contains("--expr '[].name'"));

    cli_engine::register_global_schema_fields(
        "manual:list",
        vec![FieldInfo {
            name: "id".to_owned(),
            field_type: "string".to_owned(),
            optional: false,
        }],
    );
    let manual = cli_engine::get_global_schema_by_path("manual:list").expect("manual schema");
    assert_eq!(manual.fields[0].name, "id");

    let mut path_registry = SchemaRegistry::new();
    path_registry.register_fields("my-cli project list", schema.fields.clone());
    let by_colon = path_registry
        .get_by_path("project:list")
        .expect("colon path should match root-prefixed space path");
    assert_eq!(by_colon.command, "project:list");
    assert_eq!(by_colon.fields[0].name, "name");
}

#[test]
fn schema_registry_supports_schemars_json_schema_as_primary_rust_contract() {
    #[derive(Debug, serde::Serialize, JsonSchema)]
    struct NativeThing {
        name: String,
        count: i64,
        owner: Option<String>,
    }

    let mut registry = SchemaRegistry::new();
    registry.register_json_schema::<NativeThing>("native:list");
    let schema = registry
        .get_by_path("native:list")
        .expect("json schema should be registered");

    assert_eq!(schema.command, "native:list");
    let json_schema = schema.schema.expect("schemars schema should be present");
    assert_eq!(json_schema["title"], "NativeThing");
    assert_eq!(json_schema["properties"]["name"]["type"], "string");
    assert_eq!(
        schema.fields,
        vec![
            FieldInfo {
                name: "count".to_owned(),
                field_type: "int".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "name".to_owned(),
                field_type: "string".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "owner".to_owned(),
                field_type: "string".to_owned(),
                optional: true,
            },
        ]
    );
}

#[test]
fn output_field_constructors_cover_legacy_schema_type_strings_readably() {
    let fields = [
        OutputField::string("name"),
        OutputField::int("count"),
        OutputField::float("ratio").optional(),
        OutputField::bool("active"),
        OutputField::list("tags", "[]string").optional(),
        OutputField::string_list("names"),
        OutputField::int_list("counts"),
        OutputField::float_list("ratios"),
        OutputField::bool_list("switches"),
        OutputField::object_list("items"),
        OutputField::object("nested"),
        OutputField::object("pointer").optional(),
        OutputField::object("lookup"),
        OutputField::any("anything"),
    ];

    let actual: Vec<_> = fields
        .iter()
        .map(|field| FieldInfo {
            name: field.name.to_owned(),
            field_type: field.field_type.to_owned(),
            optional: field.optional,
        })
        .collect();

    assert_eq!(
        actual,
        vec![
            FieldInfo {
                name: "name".to_owned(),
                field_type: "string".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "count".to_owned(),
                field_type: "int".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "ratio".to_owned(),
                field_type: "float".to_owned(),
                optional: true,
            },
            FieldInfo {
                name: "active".to_owned(),
                field_type: "bool".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "tags".to_owned(),
                field_type: "[]string".to_owned(),
                optional: true,
            },
            FieldInfo {
                name: "names".to_owned(),
                field_type: "[]string".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "counts".to_owned(),
                field_type: "[]int".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "ratios".to_owned(),
                field_type: "[]float".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "switches".to_owned(),
                field_type: "[]bool".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "items".to_owned(),
                field_type: "[]object".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "nested".to_owned(),
                field_type: "object".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "pointer".to_owned(),
                field_type: "object".to_owned(),
                optional: true,
            },
            FieldInfo {
                name: "lookup".to_owned(),
                field_type: "object".to_owned(),
                optional: false,
            },
            FieldInfo {
                name: "anything".to_owned(),
                field_type: "any".to_owned(),
                optional: false,
            },
        ]
    );
}

#[test]
fn tree_node_json_shape_and_human_rendering_match_source_contract() {
    let tree = TreeNode {
        name: "my-cli".to_owned(),
        description: "root".to_owned(),
        path: "my-cli".to_owned(),
        children: vec![
            TreeNode {
                name: "auth".to_owned(),
                description: "Manage authentication credentials".to_owned(),
                path: "my-cli auth".to_owned(),
                children: vec![],
            },
            TreeNode {
                name: "tree".to_owned(),
                description: "Display full command tree".to_owned(),
                path: "my-cli tree".to_owned(),
                children: vec![],
            },
        ],
    };

    let encoded = serde_json::to_value(&tree).expect("tree should serialize");
    assert_eq!(
        encoded,
        json!({
            "name": "my-cli",
            "description": "root",
            "path": "my-cli",
            "children": [
                {
                    "name": "auth",
                    "description": "Manage authentication credentials",
                    "path": "my-cli auth"
                },
                {
                    "name": "tree",
                    "description": "Display full command tree",
                    "path": "my-cli tree"
                }
            ]
        })
    );
    assert_eq!(
        render_tree_human(&tree),
        "my-cli\n├── auth ··· Manage authentication credentials\n└── tree ··· Display full command tree\n"
    );

    let clap_tree = build_tree_from_clap(
        &Command::new("my-cli")
            .subcommand(Command::new("visible"))
            .subcommand(Command::new("completion"))
            .subcommand(Command::new("secret").hide(true)),
    );
    assert_eq!(
        clap_tree
            .children
            .iter()
            .map(|child| child.name.as_str())
            .collect::<Vec<_>>(),
        vec!["visible"]
    );
}

#[test]
fn guide_front_matter_parses_only_summary() {
    let entry = GuideEntry::from_markdown_path(
        "nested/deploy.md",
        "---\nsummary: Deploy safely\nignored: value\n---\n# Deploy\n",
    );

    assert_eq!(entry.name, "deploy");
    assert_eq!(entry.summary, "Deploy safely");
    assert_eq!(entry.content, "# Deploy\n");
}

#[test]
fn guide_front_matter_uses_last_summary_line_preserves_legacy_scanner() {
    let entry = GuideEntry::from_markdown_path(
        "deploy.md",
        "---\nsummary: First summary\nsummary: Second summary\n---\n# Deploy\n",
    );

    assert_eq!(entry.summary, "Second summary");
    assert_eq!(entry.content, "# Deploy\n");
}

#[test]
fn guide_entry_basename_accepts_embed_style_windows_separators() {
    let entry = GuideEntry::from_markdown_path(
        "nested\\windows\\deploy.md",
        "---\nsummary: Deploy from Windows path\n---\n# Deploy\n",
    );

    assert_eq!(entry.name, "deploy");
    assert_eq!(entry.summary, "Deploy from Windows path");
    assert_eq!(entry.content, "# Deploy\n");
}

#[test]
fn parse_guides_walks_markdown_files_and_strips_directories_preserves_legacy() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("deploy.md"),
        "---\nsummary: Deploy apps\nignored: value\n---\n# Deploy\n",
    )
    .expect("write deploy guide");
    std::fs::create_dir(temp.path().join("nested")).expect("create nested guide dir");
    std::fs::write(
        temp.path().join("nested").join("rollback.md"),
        "# Rollback\n",
    )
    .expect("write rollback guide");
    std::fs::write(temp.path().join("nested").join("notes.txt"), "# Notes\n")
        .expect("write ignored guide");

    let entries = cli_engine::parse_guides(temp.path()).expect("parse guides");

    assert_eq!(
        entries,
        vec![
            GuideEntry {
                name: "deploy".to_owned(),
                summary: "Deploy apps".to_owned(),
                content: "# Deploy\n".to_owned(),
            },
            GuideEntry {
                name: "rollback".to_owned(),
                summary: String::new(),
                content: "# Rollback\n".to_owned(),
            },
        ]
    );
}

#[test]
fn parse_guides_from_markdown_supports_embedded_guide_sources() {
    let entries = cli_engine::parse_guides_from_markdown([
        (
            "z-last.md",
            b"---\nsummary: Last summary\n---\n# Last\n".as_slice(),
        ),
        (
            "guides/deploy.md",
            b"---\nsummary: Deploy safely\n---\n# Deploy\n".as_slice(),
        ),
        ("notes.txt", b"ignored".as_slice()),
        (
            "nested/release.md",
            b"---\nsummary: Release summary\n---\n# Release\n".as_slice(),
        ),
        (
            "windows\\operate.md",
            b"---\nsummary: Operate summary\n---\n# Operate\n".as_slice(),
        ),
    ]);

    assert_eq!(
        entries,
        vec![
            GuideEntry {
                name: "deploy".to_owned(),
                summary: "Deploy safely".to_owned(),
                content: "# Deploy\n".to_owned(),
            },
            GuideEntry {
                name: "release".to_owned(),
                summary: "Release summary".to_owned(),
                content: "# Release\n".to_owned(),
            },
            GuideEntry {
                name: "operate".to_owned(),
                summary: "Operate summary".to_owned(),
                content: "# Operate\n".to_owned(),
            },
            GuideEntry {
                name: "z-last".to_owned(),
                summary: "Last summary".to_owned(),
                content: "# Last\n".to_owned(),
            },
        ]
    );
}

#[test]
fn parse_guides_ignores_missing_roots_preserves_legacy_walkdir_callback() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing = temp.path().join("missing");

    let entries = cli_engine::parse_guides(&missing).expect("missing root should be ignored");

    assert!(entries.is_empty());
}

#[test]
fn parse_guides_keeps_invalid_utf8_files_preserves_legacy_byte_to_string_conversion() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("invalid.md"), b"# Bad\n\xff\n").expect("write invalid guide");

    let entries = cli_engine::parse_guides(temp.path()).expect("parse guides");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "invalid");
    assert!(entries[0].content.starts_with("# Bad\n"));
}

#[test]
fn guide_content_lists_topics_returns_topic_and_errors_with_valid_names() {
    let entries = vec![GuideEntry {
        name: "deploy".to_owned(),
        summary: "Deploy safely".to_owned(),
        content: "# Deploy\n".to_owned(),
    }];

    assert_eq!(
        guide_content(&entries, None).expect("guide list should render"),
        "Available guide topics:\n\n  deploy           Deploy safely\n\nUsage: <cli> guide <topic>"
    );
    assert_eq!(
        guide_content(&entries, Some("deploy")).expect("guide topic should render"),
        "# Deploy\n"
    );
    assert_eq!(
        guide_content(&entries, Some("missing")).expect_err("missing guide should error"),
        "unknown guide topic \"missing\" — valid topics: deploy"
    );
}

#[test]
fn guide_content_duplicate_topic_uses_last_entry_preserves_legacy_lookup_map() {
    let entries = vec![
        GuideEntry {
            name: "deploy".to_owned(),
            summary: "First summary".to_owned(),
            content: "first\n".to_owned(),
        },
        GuideEntry {
            name: "deploy".to_owned(),
            summary: "Second summary".to_owned(),
            content: "second\n".to_owned(),
        },
    ];

    assert_eq!(
        guide_content(&entries, Some("deploy")).expect("duplicate topic should resolve"),
        "second\n"
    );
    assert_eq!(
        guide_content(&entries, None).expect("guide list should keep both entries"),
        "Available guide topics:\n\n  deploy           First summary\n  deploy           Second summary\n\nUsage: <cli> guide <topic>"
    );
}

#[tokio::test]
async fn cli_runtime_guide_aggregation_uses_first_name_preserves_legacy_new_cli() {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Developer tooling".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_guides([
        GuideEntry {
            name: "deploy".to_owned(),
            summary: "First summary".to_owned(),
            content: "first\n".to_owned(),
        },
        GuideEntry {
            name: "deploy".to_owned(),
            summary: "Second summary".to_owned(),
            content: "second\n".to_owned(),
        },
    ]);

    let list = cli.run(["my-cli", "guide"]).await;
    assert_eq!(list.exit_code, 0);
    assert_eq!(
        list.rendered,
        "Available guide topics:\n\n  deploy           First summary\n\nUsage: <cli> guide <topic>"
    );

    let topic = cli.run(["my-cli", "guide", "deploy"]).await;
    assert_eq!(topic.exit_code, 0);
    assert_eq!(topic.rendered, "first\n");
}

#[test]
fn search_tokenization_and_ranking_match_source_shape() {
    assert_eq!(tokenize("Deploying the services"), vec!["deploy", "servic"]);
    let index = SearchIndex::new(vec![
        SearchDocument {
            id: "cmd:deploy".to_owned(),
            kind: "command".to_owned(),
            title: "deploy".to_owned(),
            summary: "Deploy applications to Katana".to_owned(),
            content: "Deploy applications to Katana".to_owned(),
        },
        SearchDocument {
            id: "cmd:login".to_owned(),
            kind: "command".to_owned(),
            title: "login".to_owned(),
            summary: "Authenticate with auth".to_owned(),
            content: "Authenticate with auth".to_owned(),
        },
    ]);

    let results = index.search("deploy", 10);

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].command, "deploy");
}

#[derive(Debug)]
struct FakeProvider {
    name: String,
    identity: String,
    logout_fails: bool,
    environments: Vec<String>,
}

impl FakeProvider {
    fn new(name: &str, identity: &str) -> Self {
        Self {
            name: name.to_owned(),
            identity: identity.to_owned(),
            logout_fails: false,
            environments: vec![],
        }
    }
}

#[async_trait]
impl AuthProvider for FakeProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        Ok(Credential {
            token: "token".to_owned(),
            expires_at: "2099-01-01T00:00:00Z".to_owned(),
            env: env.to_owned(),
            identity: self.identity.clone(),
            ..Credential::default()
        })
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.get_credential(env, "", "").await
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        if self.logout_fails {
            Err(cli_engine::CliCoreError::message("logout failed"))
        } else {
            Ok(())
        }
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        if self.environments.iter().any(|env| env == "__error__") {
            Err(cli_engine::CliCoreError::message("list failed"))
        } else {
            Ok(self.environments.clone())
        }
    }
}

/// Auth provider that counts how many times a credential is resolved, so lazy
/// resolution behavior can be asserted directly.
#[derive(Debug)]
struct CountingProvider {
    name: String,
    calls: Arc<AtomicUsize>,
}

impl CountingProvider {
    fn new(name: &str) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name: name.to_owned(),
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait]
impl AuthProvider for CountingProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Credential {
            token: "token".to_owned(),
            env: env.to_owned(),
            identity: "counted-user".to_owned(),
            ..Credential::default()
        })
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.get_credential(env, "", "").await
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

/// Authorizer that always resolves the credential, to exercise memoization when
/// both the authorizer and the handler ask.
#[derive(Debug)]
struct ResolvingAuthorizer;

#[async_trait]
impl Authorizer for ResolvingAuthorizer {
    async fn authorize(
        &self,
        _command_path: &str,
        _args: &serde_json::Map<String, serde_json::Value>,
        credential: &CredentialResolver,
        _reason: &str,
        _tier: Tier,
    ) -> Result<()> {
        credential.try_resolve().await?;
        Ok(())
    }
}

fn counting_middleware(calls_provider: CountingProvider) -> Middleware {
    let mut middleware = Middleware::new();
    middleware.auth.register(Arc::new(calls_provider));
    middleware.default_auth_provider = "counting".to_owned();
    middleware.output_format = "json".to_owned();
    middleware
}

#[tokio::test]
async fn required_default_resolves_before_handler_even_when_credential_ignored() {
    // Fail-closed default: a `Required` command resolves the credential before the
    // handler runs, even though this handler ignores it. The identity is therefore
    // available to audit/activity without the handler doing anything.
    let activity = Arc::new(CaptureActivity::default());
    let (provider, calls) = CountingProvider::new("counting");
    let mut middleware = counting_middleware(provider);
    middleware.activity = Some(activity.clone());

    let output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_resolver| Ok(CommandResult::new(json!({"ok": true}))),
        )
        .await
        .expect("command should succeed");

    assert_eq!(output.exit_code, 0);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a Required command must resolve the credential before the handler runs"
    );
    assert_eq!(activity.statuses().await, vec!["ok"]);
    assert_eq!(
        activity.identities().await,
        vec!["counted-user"],
        "engine-resolved identity must reach activity even when the handler ignores it"
    );
}

#[tokio::test]
async fn optional_skips_auth_when_handler_ignores_credential() {
    // `Optional` defers resolution to the handler: a handler that ignores the
    // credential triggers no auth flow.
    let (provider, calls) = CountingProvider::new("counting");
    let middleware = counting_middleware(provider);

    let output = middleware
        .run(
            MiddlewareRequest {
                meta: CommandMeta::default(),
                command_path: "things:list",
                system: "things",
                user_args: value_map([]),
                args: value_map([]),
                default_fields: "",
                view_id: None,
                auth: cli_engine::AuthRequirement::Optional,
            },
            async |_resolver| Ok(CommandResult::new(json!({"ok": true}))),
        )
        .await
        .expect("optional command should succeed without resolving auth");

    assert_eq!(output.exit_code, 0);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an Optional command must not resolve when the handler ignores the credential"
    );
}

#[tokio::test]
async fn optional_swallowed_auth_failure_then_command_error_is_not_auth_error() {
    // Regression: an Optional handler that best-effort resolves, swallows the
    // resolution failure, and then fails for an unrelated reason must be
    // classified by the error it returns ("error"), not as "auth-error". The
    // activity backend must be the command system, not the auth provider.
    let activity = Arc::new(CaptureActivity::default());
    let mut middleware = Middleware::new();
    middleware.default_auth_provider = "missing".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.activity = Some(activity.clone());

    let output = middleware
        .run(
            MiddlewareRequest {
                meta: CommandMeta::default(),
                command_path: "things:list",
                system: "things-api",
                user_args: value_map([]),
                args: value_map([]),
                default_fields: "",
                view_id: None,
                auth: cli_engine::AuthRequirement::Optional,
            },
            async |resolver: CredentialResolver| {
                // Best-effort identity; the missing provider makes this fail, and
                // the handler deliberately ignores it.
                let _maybe_credential = resolver.try_resolve().await.ok().flatten();
                Err::<CommandResult, _>(cli_engine::CliCoreError::message_for_system(
                    "things-api",
                    "backend rejected request",
                ))
            },
        )
        .await
        .expect("command error is rendered into middleware output");

    assert_ne!(output.exit_code, 0);
    assert_eq!(
        activity.statuses().await,
        vec!["error"],
        "a swallowed auth failure must not promote a later command error to auth-error"
    );
    assert_eq!(
        activity.backends().await,
        vec!["things-api"],
        "the backend must be the command system, not the auth provider"
    );
}

#[tokio::test]
async fn optional_handler_propagated_auth_failure_is_classified_auth_error() {
    // An Optional handler that requires the credential and propagates the
    // resolution failure must be classified `auth-error`, with the backend
    // attributed to the auth provider. Exercises the handler-path
    // `err.is_auth()` branch (Required commands short-circuit earlier).
    let activity = Arc::new(CaptureActivity::default());
    let mut middleware = Middleware::new();
    middleware.default_auth_provider = "missing".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.activity = Some(activity.clone());

    let output = middleware
        .run(
            MiddlewareRequest {
                meta: CommandMeta::default(),
                command_path: "things:list",
                system: "things-api",
                user_args: value_map([]),
                args: value_map([]),
                default_fields: "",
                view_id: None,
                auth: cli_engine::AuthRequirement::Optional,
            },
            async |resolver: CredentialResolver| {
                resolver.resolve().await?;
                Ok(CommandResult::new(json!({})))
            },
        )
        .await
        .expect("auth error is rendered into middleware output");

    assert_ne!(output.exit_code, 0);
    assert_eq!(activity.statuses().await, vec!["auth-error"]);
    assert_eq!(activity.backends().await, vec!["missing"]);
}

#[tokio::test]
async fn authz_propagated_auth_failure_is_classified_auth_error() {
    // An authorizer that resolves and propagates the failure is classified
    // `auth-error`, backend attributed to the provider. Exercises the
    // authz-path `err.is_auth()` branch.
    let activity = Arc::new(CaptureActivity::default());
    let mut middleware = Middleware::new();
    middleware.default_auth_provider = "missing".to_owned();
    middleware.output_format = "json".to_owned();
    middleware.activity = Some(activity.clone());
    middleware.authz = Some(Arc::new(ResolvingAuthorizer));

    let output = middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |_resolver| Ok(CommandResult::new(json!({}))),
        )
        .await
        .expect("auth error is rendered into middleware output");

    assert_ne!(output.exit_code, 0);
    assert_eq!(activity.statuses().await, vec!["auth-error"]);
    assert_eq!(
        activity.backends().await,
        vec!["missing"],
        "an authorizer's propagated auth failure attributes the provider backend"
    );
}

#[tokio::test]
async fn lazy_resolution_resolves_once_across_authz_and_handler() {
    let (provider, calls) = CountingProvider::new("counting");
    let mut middleware = counting_middleware(provider);
    middleware.authz = Some(Arc::new(ResolvingAuthorizer));

    middleware
        .run(
            middleware_request(
                CommandMeta::default(),
                "things:list",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |resolver: CredentialResolver| {
                let credential = resolver.resolve().await?;
                assert_eq!(credential.identity, "counted-user");
                Ok(CommandResult::new(json!({"ok": true})))
            },
        )
        .await
        .expect("command should succeed");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "authorizer and handler must share a single memoized resolution"
    );
}

#[tokio::test]
async fn lazy_resolution_skips_auth_for_dry_run() {
    let (provider, calls) = CountingProvider::new("counting");
    let mut middleware = counting_middleware(provider);
    middleware.dry_run = true;
    middleware.verbose = "all".to_owned();

    // A mutating tier makes dry-run short-circuit before the handler runs.
    let mut meta = CommandMeta::default();
    meta.auth_metadata
        .insert("tier".to_owned(), "mutate".to_owned());
    meta.dry_run_prompt = true;

    let output = middleware
        .run(
            middleware_request(
                meta,
                "things:delete",
                value_map([]),
                value_map([]),
                "",
                false,
            ),
            async |resolver: CredentialResolver| {
                // Would resolve, but dry-run short-circuits before reaching here.
                resolver.resolve().await?;
                Ok(CommandResult::new(json!({"ok": true})))
            },
        )
        .await
        .expect("dry-run should render");

    assert!(
        output.envelope.metadata.expect("metadata").dry_run,
        "expected a dry-run envelope"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "dry-run must short-circuit before any auth resolution"
    );
}

#[tokio::test]
async fn auth_command_is_listed_under_configured_help_category() {
    let module = Module::new("Workflows", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
    });
    let cli = Cli::new(
        CliConfig::new("my-cli", "Dev tooling", "my-cli")
            .with_auth_provider(Arc::new(FakeProvider::new("primary", "me")))
            .with_default_auth_provider("primary")
            .with_admin_category("Account")
            .with_module(module),
    );

    // No discovery hook, so bare invocation renders the root long help.
    let bare = cli.run(["my-cli"]).await;
    // `auth` is folded into the curated category list (it would otherwise be
    // visible only via clap's auto subcommand list, which the root template
    // suppresses).
    assert!(bare.rendered.contains("Account:"), "{}", bare.rendered);
    assert!(bare.rendered.contains("auth"), "{}", bare.rendered);
    // The root help template drops the global options wall.
    assert!(!bare.rendered.contains("--fields"), "{}", bare.rendered);
}

#[tokio::test]
async fn auth_command_uses_baked_in_default_category_without_override() {
    let module = Module::new("Workflows", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
    });
    let cli = Cli::new(
        CliConfig::new("my-cli", "Dev tooling", "my-cli")
            .with_auth_provider(Arc::new(FakeProvider::new("primary", "me")))
            .with_default_auth_provider("primary")
            // No with_admin_category: auth gets the baked-in default category.
            .with_module(module),
    );

    let bare = cli.run(["my-cli"]).await;
    // Baked-in default category ("Admin"), not a generic "Commands" bucket.
    assert!(bare.rendered.contains("Admin:"), "{}", bare.rendered);
    assert!(bare.rendered.contains("auth"), "{}", bare.rendered);
    // No generic "Commands:" heading (distinct from the built-in "Find Commands:").
    assert!(
        !bare.rendered.contains("\n  Commands:"),
        "{}",
        bare.rendered
    );
}

#[tokio::test]
async fn auth_registered_after_construction_is_categorized() {
    let module = Module::new("Workflows", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
    });
    // Built with no auth provider; one is added post-construction.
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli").with_module(module));
    cli.register_auth_provider(Arc::new(FakeProvider::new("primary", "me")));

    let bare = cli.run(["my-cli"]).await;
    // `auth` added by the later provider is still filed under the admin
    // category, not the generic "Commands" bucket.
    assert!(bare.rendered.contains("Admin:"), "{}", bare.rendered);
    assert!(bare.rendered.contains("auth"), "{}", bare.rendered);
    assert!(
        !bare.rendered.contains("\n  Commands:"),
        "{}",
        bare.rendered
    );
}

#[tokio::test]
async fn env_group_lists_gets_and_shows_info_for_active_environment() {
    use cli_engine::environments::{EnvironmentDef, Environments};

    let cli = Cli::new(
        CliConfig::new("envcmds", "Env cmds", "envcmds").with_environments(Arc::new(
            Environments::new("prod")
                .with_environment(
                    "prod",
                    EnvironmentDef::new().with_field("api_url", "https://p"),
                )
                .with_environment(
                    "ote",
                    EnvironmentDef::new().with_field("api_url", "https://o"),
                ),
        )),
    );

    // env list returns both environments.
    let list = cli
        .run(["envcmds", "env", "list", "--output", "json"])
        .await;
    assert_eq!(list.exit_code, 0, "env list failed: {}", list.rendered);
    assert!(
        list.rendered.contains("prod") && list.rendered.contains("ote"),
        "env list missing environments: {}",
        list.rendered
    );

    // env get returns the default active environment.
    let get = cli.run(["envcmds", "env", "get", "--output", "json"]).await;
    assert_eq!(get.exit_code, 0, "env get failed: {}", get.rendered);
    assert!(
        get.rendered.contains("prod"),
        "env get missing default env: {}",
        get.rendered
    );

    // env info --env ote shows ote's extra fields.
    let info = cli
        .run(["envcmds", "env", "info", "--env", "ote", "--output", "json"])
        .await;
    assert_eq!(info.exit_code, 0, "env info failed: {}", info.rendered);
    assert!(
        info.rendered.contains("https://o"),
        "env info missing ote api_url: {}",
        info.rendered
    );
}

#[derive(Debug)]
struct RecordingEnvProvider {
    envs: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl AuthProvider for RecordingEnvProvider {
    fn name(&self) -> &str {
        "primary"
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        self.envs.lock().await.push(env.to_owned());
        Ok(Credential {
            token: "token".to_owned(),
            expires_at: "2099-01-01T00:00:00Z".to_owned(),
            env: env.to_owned(),
            identity: "tester".to_owned(),
            ..Credential::default()
        })
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.get_credential(env, "", "").await
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Records the `meta.scopes` of every `get_credential_for` call, so tests can
/// assert that command scopes (and runtime step-up scopes) reach the provider.
#[derive(Debug)]
struct RecordingScopeProvider {
    scopes: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl AuthProvider for RecordingScopeProvider {
    fn name(&self) -> &str {
        "primary"
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        // Reached only if the framework bypasses get_credential_for; record an
        // empty scope set so such a regression is visible.
        self.scopes.lock().await.push(Vec::new());
        Ok(Credential {
            token: "token".to_owned(),
            env: env.to_owned(),
            ..Credential::default()
        })
    }

    async fn get_credential_for(
        &self,
        req: &cli_engine::CredentialRequest<'_>,
    ) -> Result<Credential> {
        self.scopes.lock().await.push(req.meta.scopes.clone());
        Ok(Credential {
            token: "token".to_owned(),
            env: req.env.to_owned(),
            ..Credential::default()
        })
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.get_credential(env, "", "").await
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Returns identity `user-a` on its first credential call and `user-b` after,
/// so a step-up re-resolution observes a different account.
#[derive(Debug)]
struct SwitchingIdentityProvider {
    calls: Arc<Mutex<usize>>,
}

impl SwitchingIdentityProvider {
    async fn next_credential(&self, env: &str) -> Credential {
        let mut calls = self.calls.lock().await;
        *calls += 1;
        let sub = if *calls == 1 { "user-a" } else { "user-b" };
        Credential {
            token: "token".to_owned(),
            env: env.to_owned(),
            sub: sub.to_owned(),
            ..Credential::default()
        }
    }
}

#[async_trait]
impl AuthProvider for SwitchingIdentityProvider {
    fn name(&self) -> &str {
        "primary"
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        Ok(self.next_credential(env).await)
    }

    async fn get_credential_for(
        &self,
        req: &cli_engine::CredentialRequest<'_>,
    ) -> Result<Credential> {
        Ok(self.next_credential(req.env).await)
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.get_credential(env, "", "").await
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[derive(Debug)]
struct FailingProvider;

#[async_trait]
impl AuthProvider for FailingProvider {
    fn name(&self) -> &str {
        "failing"
    }

    async fn get_credential(&self, _env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        Err(cli_engine::CliCoreError::message("provider failed"))
    }

    async fn status(&self, _env: &str) -> Result<Credential> {
        Err(cli_engine::CliCoreError::message("provider failed"))
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        Ok(vec!["prod".to_owned()])
    }
}

#[derive(Debug)]
struct EmptyThenFilledProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl AuthProvider for EmptyThenFilledProvider {
    fn name(&self) -> &str {
        "oauth"
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Credential {
            token: if call == 0 {
                String::new()
            } else {
                "filled".to_owned()
            },
            expires_at: "2099-01-01T00:00:00Z".to_owned(),
            env: env.to_owned(),
            ..Credential::default()
        })
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        self.get_credential(env, "", "").await
    }

    async fn logout(&self, _env: &str) -> Result<()> {
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        Ok(vec!["prod".to_owned()])
    }
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    // Sync the file to disk before changing permissions and executing.
    // Without this, Linux can return ETXTBSY when the exec races with
    // the kernel flushing the write from std::fs::write.
    let file = std::fs::File::open(path).expect("script should be openable for sync");
    file.sync_all().expect("script sync should succeed");
    drop(file);

    let mut permissions = std::fs::metadata(path)
        .expect("script metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("script permissions should be writable");
}

#[cfg(not(unix))]
fn make_executable(_path: &std::path::Path) {}

fn token_func(value: &str) -> TokenFunc {
    let value = value.to_owned();
    Arc::new(move || {
        let value = value.clone();
        Box::pin(async move { Ok(value) })
    })
}

fn failing_token_func(message: &str) -> TokenFunc {
    let message = message.to_owned();
    Arc::new(move || {
        let message = message.clone();
        Box::pin(async move { Err(cli_engine::CliCoreError::message(message)) })
    })
}

fn empty_path_parts() -> &'static [&'static str] {
    &[]
}

fn build_request() -> reqwest::Request {
    reqwest::Client::new()
        .get("http://localhost/")
        .build()
        .expect("request should build")
}

fn header(request: &reqwest::Request, name: &str) -> String {
    request
        .headers()
        .get(name)
        .expect("header should be present")
        .to_str()
        .expect("header should be valid ascii")
        .to_owned()
}

struct TestServer {
    base_url: String,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn new(handler: impl Fn(String) -> String + Send + Sync + 'static) -> Self {
        Self::sequence(vec![Box::new(handler)])
    }

    fn sequence(handlers: Vec<Box<dyn Fn(String) -> String + Send + Sync>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let address = listener.local_addr().expect("test server address");
        let handle = thread::spawn(move || {
            for handler in handlers {
                let (mut stream, _) = listener.accept().expect("test server should accept");
                let request = read_http_request(&mut stream);
                let response = handler(request);
                stream
                    .write_all(response.as_bytes())
                    .expect("response should write");
            }
        });
        Self {
            base_url: format!("http://{address}"),
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("test server should finish");
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout should set");
    let mut buffer = [0_u8; 8192];
    let mut data = Vec::new();
    loop {
        let read = stream.read(&mut buffer).expect("request should read");
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = find_subslice(&data, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..header_end]).to_lowercase();
            let content_len = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if data.len() >= header_end + 4 + content_len {
                break;
            }
        }
    }
    String::from_utf8(data).expect("request should be utf8")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn http_response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
    let reason = match status {
        403 => "Forbidden",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(body);
    response
}

fn value_map<const N: usize>(
    entries: [(&str, serde_json::Value); N],
) -> serde_json::Map<String, serde_json::Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

#[derive(Debug, Default)]
struct CaptureAudit {
    entries: Mutex<Vec<String>>,
    args: Mutex<Vec<serde_json::Map<String, serde_json::Value>>>,
}

impl CaptureAudit {
    async fn results(&self) -> Vec<String> {
        self.entries.lock().await.clone()
    }

    async fn args(&self) -> Vec<serde_json::Map<String, serde_json::Value>> {
        self.args.lock().await.clone()
    }
}

#[async_trait]
impl Auditor for CaptureAudit {
    async fn append(
        &self,
        _command_path: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        _identity: &str,
        result: &str,
        _reason: &str,
    ) -> Result<()> {
        self.entries.lock().await.push(result.to_owned());
        self.args.lock().await.push(args.clone());
        Ok(())
    }
}

#[derive(Debug)]
struct FailingAuditor;

#[async_trait]
impl Auditor for FailingAuditor {
    async fn append(
        &self,
        _command_path: &str,
        _args: &serde_json::Map<String, serde_json::Value>,
        _identity: &str,
        _result: &str,
        _reason: &str,
    ) -> Result<()> {
        Err(cli_engine::CliCoreError::message("audit sink failed"))
    }
}

#[derive(Debug, Default)]
struct CaptureActivity {
    events: Mutex<Vec<ActivityEvent>>,
}

impl CaptureActivity {
    async fn statuses(&self) -> Vec<String> {
        self.events
            .lock()
            .await
            .iter()
            .map(|event| event.status.clone())
            .collect()
    }

    async fn args(&self) -> Vec<serde_json::Map<String, serde_json::Value>> {
        self.events
            .lock()
            .await
            .iter()
            .map(|event| event.args.clone())
            .collect()
    }

    async fn backends(&self) -> Vec<String> {
        self.events
            .lock()
            .await
            .iter()
            .map(|event| event.backend.clone())
            .collect()
    }

    async fn identities(&self) -> Vec<String> {
        self.events
            .lock()
            .await
            .iter()
            .map(|event| event.identity.clone())
            .collect()
    }
}

#[async_trait]
impl ActivityEmitter for CaptureActivity {
    async fn emit(&self, event: ActivityEvent) -> Result<()> {
        self.events.lock().await.push(event);
        Ok(())
    }
}

#[derive(Debug)]
struct FailingActivity;

#[async_trait]
impl ActivityEmitter for FailingActivity {
    async fn emit(&self, _event: ActivityEvent) -> Result<()> {
        Err(cli_engine::CliCoreError::message("activity sink failed"))
    }
}

#[derive(Debug)]
struct AllowAuthorizer;

#[async_trait]
impl Authorizer for AllowAuthorizer {
    async fn authorize(
        &self,
        _command_path: &str,
        _args: &serde_json::Map<String, serde_json::Value>,
        _credential: &CredentialResolver,
        _reason: &str,
        _tier: Tier,
    ) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct RecordingAuthorizer {
    tiers: Arc<StdMutex<Vec<Tier>>>,
}

#[async_trait]
impl Authorizer for RecordingAuthorizer {
    async fn authorize(
        &self,
        _command_path: &str,
        _args: &serde_json::Map<String, serde_json::Value>,
        _credential: &CredentialResolver,
        _reason: &str,
        tier: Tier,
    ) -> Result<()> {
        self.tiers.lock().expect("tiers lock").push(tier);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct CaptureArgsAuthorizer {
    args: Mutex<Vec<serde_json::Map<String, serde_json::Value>>>,
}

impl CaptureArgsAuthorizer {
    async fn args(&self) -> Vec<serde_json::Map<String, serde_json::Value>> {
        self.args.lock().await.clone()
    }
}

#[async_trait]
impl Authorizer for CaptureArgsAuthorizer {
    async fn authorize(
        &self,
        _command_path: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        _credential: &CredentialResolver,
        _reason: &str,
        _tier: Tier,
    ) -> Result<()> {
        self.args.lock().await.push(args.clone());
        Ok(())
    }
}

#[derive(Debug)]
struct DenyAuthorizer;

#[async_trait]
impl Authorizer for DenyAuthorizer {
    async fn authorize(
        &self,
        _command_path: &str,
        _args: &serde_json::Map<String, serde_json::Value>,
        _credential: &CredentialResolver,
        _reason: &str,
        _tier: Tier,
    ) -> Result<()> {
        Err(cli_engine::CliCoreError::message("denied by test"))
    }
}
