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
