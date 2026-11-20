//! Integration tests for the generalized interactivity framework.
//!
//! These tests verify that:
//! - Missing required args + non-interactive mode → error with helpful message
//! - All args supplied + interactive mode → no prompts, executes directly
//! - The `--interactive` and `--non-interactive` flags are respected
//!
//! Note: Tests that actually trigger interactive prompts (mocked stdin) are not
//! practical in this test harness because `inquire` reads directly from the
//! terminal. Instead, we verify the non-interactive error paths and the
//! recovery logic via unit tests in `prompt.rs`.

use clap::Arg;
use cli_engine::{
    BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec,
    RuntimeGroupSpec,
};
use serde_json::json;

fn test_cli() -> Cli {
    Cli::new(
        CliConfig::new("test-cli", "Test CLI", "test-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_modules(vec![Module::new("Test", |_ctx| {
                RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects")).with_command(
                    RuntimeCommandSpec::new(
                        CommandSpec::new("create", "Create a project")
                            .no_auth(true)
                            .with_arg(Arg::new("name").long("name").required(true))
                            .with_arg(
                                Arg::new("env")
                                    .long("env")
                                    .required(true)
                                    .value_parser(["dev", "staging", "prod"]),
                            ),
                        async |_credential, args| {
                            let name = args
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let env = args
                                .get("env")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            Ok(CommandResult::new(json!({
                                "name": name,
                                "env": env,
                                "status": "created"
                            })))
                        },
                    ),
                )
            })]),
    )
}

#[tokio::test]
async fn missing_required_arg_non_interactive_shows_error() {
    let cli = test_cli();
    let output = cli
        .run(["test-cli", "project", "create", "--non-interactive"])
        .await;
    // Should fail because --name and --env are required.
    assert_ne!(output.exit_code, 0);
    // The error should mention the missing arguments.
    assert!(
        output.rendered.contains("--name") || output.rendered.contains("required"),
        "Expected error to mention missing args, got: {}",
        output.rendered
    );
}

#[tokio::test]
async fn all_args_supplied_interactive_mode_executes_directly() {
    let cli = test_cli();
    let output = cli
        .run([
            "test-cli",
            "project",
            "create",
            "--name",
            "my-project",
            "--env",
            "prod",
            "--interactive",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "output: {}", output.rendered);
    assert!(
        output.rendered.contains("my-project"),
        "Expected result to contain project name, got: {}",
        output.rendered
    );
}

#[tokio::test]
async fn all_args_supplied_non_interactive_mode_executes_directly() {
    let cli = test_cli();
    let output = cli
        .run([
            "test-cli",
            "project",
            "create",
            "--name",
            "my-project",
            "--env",
            "dev",
            "--non-interactive",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "output: {}", output.rendered);
    assert!(
        output.rendered.contains("my-project"),
        "Expected result to contain project name, got: {}",
        output.rendered
    );
}

#[tokio::test]
async fn interactive_and_non_interactive_conflict() {
    let cli = test_cli();
    let output = cli
        .run([
            "test-cli",
            "project",
            "create",
            "--interactive",
            "--non-interactive",
        ])
        .await;
    // Clap should reject conflicting flags.
    assert_ne!(output.exit_code, 0);
    assert!(
        output.rendered.contains("cannot be used with"),
        "Expected conflict error, got: {}",
        output.rendered
    );
}

/// The `--interactive` help line from `--help` output.
async fn interactive_help(config: CliConfig) -> String {
    let output = Cli::new(config).run(["test-cli", "tree", "--help"]).await;
    output
        .rendered
        .lines()
        .find(|line| line.contains("--interactive"))
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn interactive_help_claims_tty_default_only_when_auto_interactive() {
    let base = || CliConfig::new("test-cli", "Test CLI", "test-cli");

    let manual = interactive_help(base()).await;
    assert!(manual.contains("Force interactive prompts"), "{manual}");
    assert!(!manual.contains("default"), "{manual}");

    let auto = interactive_help(base().with_auto_interactive(true)).await;
    assert!(auto.contains("default when a TTY is detected"), "{auto}");
}

/// A CLI with one command per policy; each reports `ctx.is_interactive()`.
fn policy_cli() -> Cli {
    use cli_engine::InteractivityPolicy;

    let command = |name: &'static str, policy: InteractivityPolicy| {
        RuntimeCommandSpec::new_with_context(
            CommandSpec::new(name, "Report interactivity")
                .no_auth(true)
                .with_interactivity(policy),
            async |ctx| {
                Ok(CommandResult::new(
                    json!({ "interactive": ctx.is_interactive() }),
                ))
            },
        )
    };
    Cli::new(
        CliConfig::new("test-cli", "Test CLI", "test-cli").with_modules(vec![Module::new(
            "Test",
            move |_ctx| {
                RuntimeGroupSpec::new(GroupSpec::new("tool", "Tools"))
                    .with_command(command("inherit", InteractivityPolicy::Inherit))
                    .with_command(command("wizard", InteractivityPolicy::DefaultOnTty))
                    .with_command(command("manual", InteractivityPolicy::OnRequest))
            },
        )]),
    )
}

async fn reports_interactive(command: &str, flags: &[&str]) -> bool {
    let mut args = vec!["test-cli", "tool", command, "--json"];
    args.extend_from_slice(flags);
    let output = policy_cli().run(args).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json output");
    assert!(
        value["data"]["interactive"].is_boolean(),
        "no interactive field in {value}"
    );
    value["data"]["interactive"] == json!(true)
}

#[tokio::test]
async fn explicit_flags_win_over_every_policy() {
    for command in ["inherit", "wizard", "manual"] {
        assert!(
            reports_interactive(command, &["--interactive"]).await,
            "{command} --interactive"
        );
        assert!(
            !reports_interactive(command, &["--non-interactive"]).await,
            "{command} --non-interactive"
        );
    }
}

#[tokio::test]
async fn no_policy_is_interactive_without_a_tty() {
    // The test harness has no TTY on stdin, so even a DefaultOnTty command
    // must not turn prompts on by itself.
    for command in ["inherit", "wizard", "manual"] {
        assert!(!reports_interactive(command, &[]).await, "{command}");
    }
}

#[tokio::test]
async fn default_on_tty_command_says_so_in_its_help() {
    let help = async |command: &'static str| {
        policy_cli()
            .run(["test-cli", "tool", command, "--help"])
            .await
            .rendered
    };
    assert!(
        help("wizard").await.contains("Interactive by default"),
        "wizard help should mention the default"
    );
    assert!(!help("inherit").await.contains("Interactive by default"));
    assert!(!help("manual").await.contains("Interactive by default"));
}
