use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandContext, CommandResult, CommandSpec, CredentialResolver,
    GroupSpec, Module, RuntimeCommandSpec, RuntimeGroupSpec, StreamSender,
};
use serde_json::{Value, json};

#[derive(Debug, Clone, clap::Args)]
struct GreetArgs {
    #[arg(long)]
    name: String,

    #[arg(long, default_value = "1")]
    count: u32,
}

fn greet_module() -> Module {
    Module::new("Greet", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("greet", "Greeting commands"))
            .with_command(greet_command())
    })
}

fn greet_command() -> RuntimeCommandSpec {
    RuntimeCommandSpec::new_typed::<GreetArgs, _, _, _>(
        CommandSpec::from_args::<GreetArgs>("hello", "Say hello").no_auth(true),
        async |_credential: CredentialResolver, args: GreetArgs| {
            let messages: Vec<String> = (0..args.count)
                .map(|_| format!("Hello, {}!", args.name))
                .collect();
            Ok(CommandResult::new(json!({ "messages": messages })))
        },
    )
}

fn derive_cli() -> Cli {
    Cli::new(
        CliConfig::new("derive-test", "Derive Test CLI", "derive-test")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(greet_module()),
    )
}

#[tokio::test]
async fn derive_bridge_parses_typed_args_and_returns_result() {
    let cli = derive_cli();

    let result = cli
        .run([
            "derive-test",
            "greet",
            "hello",
            "--name",
            "World",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "stderr: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["messages"], json!(["Hello, World!"]));
}

#[tokio::test]
async fn derive_bridge_respects_default_values() {
    let cli = derive_cli();

    let result = cli
        .run([
            "derive-test",
            "greet",
            "hello",
            "--name",
            "Jay",
            "--count",
            "3",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "stderr: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(
        json["data"]["messages"],
        json!(["Hello, Jay!", "Hello, Jay!", "Hello, Jay!"])
    );
}

#[tokio::test]
async fn derive_bridge_reports_missing_required_arg() {
    let cli = derive_cli();

    let result = cli.run(["derive-test", "greet", "hello"]).await;
    assert_ne!(result.exit_code, 0);
    assert!(result.rendered.contains("required"), "{}", result.rendered);
}

// --- typed_args() via new_with_context ---

#[derive(Debug, Clone, clap::Args)]
struct InfoArgs {
    #[arg(long)]
    tag: String,
}

fn context_cli() -> Cli {
    let info_command = RuntimeCommandSpec::new_with_context(
        CommandSpec::from_args::<InfoArgs>("info", "Show info").no_auth(true),
        async |context: CommandContext| {
            let args: InfoArgs = context.typed_args()?;
            Ok(CommandResult::new(json!({
                "tag": args.tag,
                "command_path": context.command_path,
            })))
        },
    );

    let module = Module::new("Context", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("ctx", "Context commands"))
            .with_command(info_command.clone())
    });

    Cli::new(
        CliConfig::new("ctx-test", "Context Test CLI", "ctx-test")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(module),
    )
}

#[tokio::test]
async fn typed_args_works_from_new_with_context_handler() {
    let cli = context_cli();

    let result = cli
        .run([
            "ctx-test", "ctx", "info", "--tag", "hello", "--output", "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["tag"], "hello");
    assert_eq!(json["data"]["command_path"], "ctx:info");
}

// --- positional arguments ---

#[derive(Debug, Clone, clap::Args)]
struct EchoArgs {
    /// The message to echo.
    message: String,

    #[arg(long, default_value = "false")]
    uppercase: bool,
}

fn positional_cli() -> Cli {
    let echo_command = RuntimeCommandSpec::new_typed::<EchoArgs, _, _, _>(
        CommandSpec::from_args::<EchoArgs>("echo", "Echo a message").no_auth(true),
        async |_credential: CredentialResolver, args: EchoArgs| {
            let msg = if args.uppercase {
                args.message.to_uppercase()
            } else {
                args.message
            };
            Ok(CommandResult::new(json!({ "output": msg })))
        },
    );

    let module = Module::new("Echo", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("util", "Utility commands"))
            .with_command(echo_command.clone())
    });

    Cli::new(
        CliConfig::new("pos-test", "Positional Test CLI", "pos-test")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(module),
    )
}

#[tokio::test]
async fn derive_bridge_handles_positional_arguments() {
    let cli = positional_cli();

    let result = cli
        .run([
            "pos-test",
            "util",
            "echo",
            "hello world",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["output"], "hello world");
}

#[tokio::test]
async fn derive_bridge_handles_positional_with_flags() {
    let cli = positional_cli();

    let result = cli
        .run([
            "pos-test",
            "util",
            "echo",
            "test",
            "--uppercase",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["output"], "TEST");
}

// --- flattened structs ---

#[derive(Debug, Clone, clap::Args)]
struct Pagination {
    #[arg(long, default_value = "20")]
    page_size: u32,

    #[arg(long, default_value = "0")]
    page: u32,
}

#[derive(Debug, Clone, clap::Args)]
struct SearchArgs {
    #[arg(long)]
    query: String,

    #[command(flatten)]
    pagination: Pagination,
}

fn flatten_cli() -> Cli {
    let search_command = RuntimeCommandSpec::new_typed::<SearchArgs, _, _, _>(
        CommandSpec::from_args::<SearchArgs>("find", "Search items").no_auth(true),
        async |_credential: CredentialResolver, args: SearchArgs| {
            Ok(CommandResult::new(json!({
                "query": args.query,
                "page_size": args.pagination.page_size,
                "page": args.pagination.page,
            })))
        },
    );

    let module = Module::new("Search", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("items", "Item commands"))
            .with_command(search_command.clone())
    });

    Cli::new(
        CliConfig::new("flat-test", "Flatten Test CLI", "flat-test")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(module),
    )
}

#[tokio::test]
async fn derive_bridge_handles_flattened_structs() {
    let cli = flatten_cli();

    let result = cli
        .run([
            "flat-test",
            "items",
            "find",
            "--query",
            "rust",
            "--page-size",
            "50",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["query"], "rust");
    assert_eq!(json["data"]["page_size"], 50);
    assert_eq!(json["data"]["page"], 0);
}

// --- output shorthand aliases ---

#[tokio::test]
async fn json_shorthand_flag_produces_json_output() {
    let cli = derive_cli();

    let result = cli
        .run(["derive-test", "greet", "hello", "--name", "World", "--json"])
        .await;
    assert_eq!(result.exit_code, 0, "stderr: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["messages"], json!(["Hello, World!"]));
}

#[tokio::test]
async fn toon_shorthand_flag_produces_toon_output() {
    let cli = derive_cli();

    let result = cli
        .run(["derive-test", "greet", "hello", "--name", "World", "--toon"])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    assert!(
        !result.rendered.starts_with('{'),
        "toon output should not be raw JSON: {}",
        result.rendered
    );
}

#[tokio::test]
async fn human_shorthand_flag_produces_human_output() {
    let cli = derive_cli();

    let result = cli
        .run([
            "derive-test",
            "greet",
            "hello",
            "--name",
            "World",
            "--human",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    assert!(
        !result.rendered.starts_with('{'),
        "human output should not be raw JSON: {}",
        result.rendered
    );
}

// --- new_typed_with_context ---

#[derive(Debug, Clone, clap::Args)]
struct WhoamiArgs {
    #[arg(long)]
    tag: String,
}

fn typed_with_context_cli() -> Cli {
    let whoami_command = RuntimeCommandSpec::new_typed_with_context::<WhoamiArgs, _, _, _>(
        CommandSpec::from_args::<WhoamiArgs>("whoami", "Show tag and command path").no_auth(true),
        async |context: CommandContext, args: WhoamiArgs| {
            Ok(CommandResult::new(json!({
                "tag": args.tag,
                "command_path": context.command_path,
            })))
        },
    );

    let module = Module::new("Ident", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("ident", "Identity commands"))
            .with_command(whoami_command.clone())
    });

    Cli::new(
        CliConfig::new("typed-ctx-test", "Typed Context Test CLI", "typed-ctx-test")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(module),
    )
}

#[tokio::test]
async fn new_typed_with_context_exposes_parsed_args_and_command_path() {
    let cli = typed_with_context_cli();

    let result = cli
        .run([
            "typed-ctx-test",
            "ident",
            "whoami",
            "--tag",
            "hello",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["tag"], "hello");
    assert_eq!(json["data"]["command_path"], "ident:whoami");
}

#[tokio::test]
async fn new_typed_with_context_reports_missing_required_arg() {
    let cli = typed_with_context_cli();

    let result = cli.run(["typed-ctx-test", "ident", "whoami"]).await;
    assert_ne!(result.exit_code, 0);
    assert!(result.rendered.contains("required"), "{}", result.rendered);
}

// --- new_typed_streaming ---

#[derive(Debug, Clone, clap::Args)]
struct CountArgs {
    #[arg(long)]
    label: String,

    #[arg(long, default_value = "2")]
    count: u32,
}

fn typed_streaming_cli() -> Cli {
    let count_command = RuntimeCommandSpec::new_typed_streaming::<CountArgs, _, _>(
        CommandSpec::from_args::<CountArgs>("count", "Stream counted events").no_auth(true),
        async |_context: CommandContext, args: CountArgs, sender: StreamSender| {
            // Successful NDJSON events are written straight to real stdout by
            // the writer task (see tests/streaming.rs), so this test can't
            // capture them via `result.rendered`. Prove the typed args were
            // parsed correctly by failing loudly — into `rendered` — if they
            // don't match what was passed on the command line.
            if args.label != "tick" || args.count != 3 {
                return Err(cli_engine::CliCoreError::message(format!(
                    "unexpected parsed args: label={:?} count={}",
                    args.label, args.count
                )));
            }
            for index in 0..args.count {
                sender
                    .send(json!({ "label": args.label, "index": index }))
                    .await;
            }
            Ok(())
        },
    );

    let module = Module::new("Stream", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("stream", "Streaming commands"))
            .with_command(count_command.clone())
    });

    Cli::new(
        CliConfig::new(
            "typed-stream-test",
            "Typed Streaming Test CLI",
            "typed-stream-test",
        )
        .with_build(BuildInfo::new("0.1.0"))
        .with_module(module),
    )
}

#[tokio::test]
async fn new_typed_streaming_parses_args_before_invoking_handler() {
    let cli = typed_streaming_cli();

    let result = cli
        .run([
            "typed-stream-test",
            "stream",
            "count",
            "--label",
            "tick",
            "--count",
            "3",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    assert!(
        result.rendered.is_empty(),
        "successful stream should produce no rendered envelope, got: {:?}",
        result.rendered
    );
}

#[tokio::test]
async fn new_typed_streaming_reports_missing_required_arg() {
    let cli = typed_streaming_cli();

    let result = cli
        .run(["typed-stream-test", "stream", "count", "--count", "3"])
        .await;
    assert_ne!(result.exit_code, 0);
    assert!(result.rendered.contains("required"), "{}", result.rendered);
}

// --- ArgGroup passthrough via from_args ---

#[derive(Debug, Clone, clap::Args)]
#[group(required = true, multiple = true)]
struct UpdateArgs {
    #[arg(long)]
    label: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    status: Option<String>,
}

fn update_cli() -> Cli {
    let update_command = RuntimeCommandSpec::new_typed::<UpdateArgs, _, _, _>(
        CommandSpec::from_args::<UpdateArgs>("update", "Update at least one field").no_auth(true),
        async |_credential: CredentialResolver, args: UpdateArgs| {
            Ok(CommandResult::new(json!({
                "label": args.label,
                "description": args.description,
                "status": args.status,
            })))
        },
    );

    let module = Module::new("Update", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("thing", "Thing commands"))
            .with_command(update_command.clone())
    });

    Cli::new(
        CliConfig::new("group-test", "Group Test CLI", "group-test")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(module),
    )
}

#[tokio::test]
async fn arg_group_from_args_rejects_when_none_present() {
    let cli = update_cli();

    let result = cli.run(["group-test", "thing", "update"]).await;
    assert_ne!(result.exit_code, 0);
}

#[tokio::test]
async fn arg_group_from_args_accepts_exactly_one() {
    let cli = update_cli();

    let result = cli
        .run([
            "group-test",
            "thing",
            "update",
            "--label",
            "new-label",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["label"], "new-label");
    assert!(json["data"]["description"].is_null());
}

#[tokio::test]
async fn arg_group_from_args_accepts_multiple_when_group_allows_multiple() {
    let cli = update_cli();

    let result = cli
        .run([
            "group-test",
            "thing",
            "update",
            "--label",
            "new-label",
            "--status",
            "ACTIVE",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["label"], "new-label");
    assert_eq!(json["data"]["status"], "ACTIVE");
}

#[derive(Debug, Clone, clap::Args)]
#[group(required = true, multiple = false)]
struct VersionArgs {
    #[arg(long)]
    major: bool,

    #[arg(long)]
    minor: bool,
}

fn version_cli() -> Cli {
    let bump_command = RuntimeCommandSpec::new_typed::<VersionArgs, _, _, _>(
        CommandSpec::from_args::<VersionArgs>("bump", "Bump exactly one version component")
            .no_auth(true),
        async |_credential: CredentialResolver, args: VersionArgs| {
            Ok(CommandResult::new(
                json!({ "major": args.major, "minor": args.minor }),
            ))
        },
    );

    let module = Module::new("Version", move |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("version", "Version commands"))
            .with_command(bump_command.clone())
    });

    Cli::new(
        CliConfig::new(
            "exclusive-group-test",
            "Exclusive Group Test CLI",
            "exclusive-group-test",
        )
        .with_build(BuildInfo::new("0.1.0"))
        .with_module(module),
    )
}

#[tokio::test]
async fn arg_group_from_args_rejects_mutually_exclusive_flags_together() {
    let cli = version_cli();

    let result = cli
        .run([
            "exclusive-group-test",
            "version",
            "bump",
            "--major",
            "--minor",
        ])
        .await;
    assert_ne!(result.exit_code, 0);
}

#[tokio::test]
async fn arg_group_from_args_accepts_one_of_mutually_exclusive_flags() {
    let cli = version_cli();

    let result = cli
        .run([
            "exclusive-group-test",
            "version",
            "bump",
            "--major",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(result.exit_code, 0, "output: {}", result.rendered);
    let json: Value = serde_json::from_str(&result.rendered).expect("valid json");
    assert_eq!(json["data"]["major"], true);
    assert_eq!(json["data"]["minor"], false);
}
