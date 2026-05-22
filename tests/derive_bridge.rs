use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, Credential, GroupSpec, Module,
    RuntimeCommandSpec, RuntimeGroupSpec,
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
