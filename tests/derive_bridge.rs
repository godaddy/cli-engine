use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandContext, CommandResult, CommandSpec, Credential, GroupSpec,
    Module, RuntimeCommandSpec, RuntimeGroupSpec,
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
        async |_credential: Option<Credential>, args: GreetArgs| {
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
        async |_credential: Option<Credential>, args: EchoArgs| {
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
        async |_credential: Option<Credential>, args: SearchArgs| {
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
