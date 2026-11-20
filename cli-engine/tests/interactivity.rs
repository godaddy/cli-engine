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

mod prompting {
    use std::sync::Arc;

    use cli_engine::prompt::{Answer, Prompter, ScriptedPrompter, TextValidator};

    use super::*;

    /// A wizard that asks for a name (must be lowercase) and a confirmation.
    fn wizard_cli(prompter: Arc<ScriptedPrompter>) -> Cli {
        Cli::new(
            CliConfig::new("test-cli", "Test CLI", "test-cli")
                .with_prompter(prompter)
                .with_modules(vec![Module::new("Test", |_ctx| {
                    RuntimeGroupSpec::new(GroupSpec::new("tool", "Tools")).with_command(
                        RuntimeCommandSpec::new_with_context(
                            CommandSpec::new("init", "Ask questions")
                                .no_auth(true)
                                .with_interactivity(cli_engine::InteractivityPolicy::DefaultOnTty),
                            async |ctx| {
                                ctx.require_interactive("pass --name")?;
                                let prompter = ctx.prompter();
                                let lowercase: TextValidator = Arc::new(|input: &str| {
                                    if !input.is_empty() && input == input.to_lowercase() {
                                        Ok(())
                                    } else {
                                        Err("use lowercase".to_owned())
                                    }
                                });
                                let name = prompter.text("Name", None, Some(lowercase))?;
                                let sure = prompter.confirm("Create it?", true)?;
                                Ok(CommandResult::new(json!({ "name": name, "sure": sure })))
                            },
                        ),
                    )
                })]),
        )
    }

    #[tokio::test]
    async fn scripted_answers_drive_a_command_and_a_bad_answer_is_asked_again() {
        let prompter = Arc::new(ScriptedPrompter::new([
            Answer::text("Acme"),
            Answer::text("acme"),
            Answer::Confirm(false),
        ]));
        let output = wizard_cli(Arc::clone(&prompter))
            .run(["test-cli", "tool", "init", "--interactive", "--json"])
            .await;

        assert_eq!(output.exit_code, 0, "{}", output.rendered);
        let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
        assert_eq!(value["data"], json!({ "name": "acme", "sure": false }));
        assert_eq!(prompter.remaining(), 0);
        assert_eq!(
            prompter.transcript(),
            ["Name", "  rejected: use lowercase", "Name", "Create it?"]
        );
    }

    #[tokio::test]
    async fn non_interactive_stops_before_any_prompt_with_a_fix() {
        let prompter = Arc::new(ScriptedPrompter::new([Answer::text("acme")]));
        let output = wizard_cli(Arc::clone(&prompter))
            .run(["test-cli", "tool", "init", "--non-interactive", "--json"])
            .await;

        assert_ne!(output.exit_code, 0);
        assert!(
            output.rendered.contains("pass --name"),
            "{}",
            output.rendered
        );
        assert!(
            output.rendered.contains("--non-interactive"),
            "{}",
            output.rendered
        );
        assert_eq!(prompter.remaining(), 1, "no prompt may have been asked");
    }

    #[tokio::test]
    async fn cancelling_a_prompt_exits_130() {
        let prompter = Arc::new(ScriptedPrompter::new([Answer::Cancel]));
        let output = wizard_cli(prompter)
            .run(["test-cli", "tool", "init", "--interactive", "--json"])
            .await;

        assert_eq!(output.exit_code, 130, "{}", output.rendered);
        assert!(output.rendered.contains("cancelled"), "{}", output.rendered);
    }

    #[test]
    fn the_scripted_prompter_refuses_a_prompt_it_has_no_answer_for() {
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);
        let err = prompter.text("Name", None, None).expect_err("wrong kind");
        assert!(err.to_string().contains("wanted text"), "{err}");
        let err = prompter.confirm("Sure?", true).expect_err("out of answers");
        assert!(err.to_string().contains("ran out"), "{err}");
    }

    #[test]
    fn an_empty_text_answer_takes_the_default() {
        let prompter = ScriptedPrompter::new([Answer::text("")]);
        assert_eq!(
            prompter.text("TTL", Some("3600"), None).expect("text"),
            "3600"
        );
    }

    /// A command whose required args the engine's recovery asks for.
    fn recovery_cli(prompter: Arc<ScriptedPrompter>) -> Cli {
        Cli::new(
            CliConfig::new("test-cli", "Test CLI", "test-cli")
                .with_prompter(prompter)
                .with_modules(vec![Module::new("Test", |_ctx| {
                    RuntimeGroupSpec::new(GroupSpec::new("tool", "Tools")).with_command(
                        RuntimeCommandSpec::new(
                            CommandSpec::new("deploy", "Deploy")
                                .no_auth(true)
                                .with_arg(
                                    Arg::new("port")
                                        .long("port")
                                        .required(true)
                                        .value_parser(clap::value_parser!(u16)),
                                )
                                .with_arg(
                                    Arg::new("env")
                                        .long("env")
                                        .required(true)
                                        .value_parser(["dev", "prod"]),
                                ),
                            async |_credential, args| {
                                Ok(CommandResult::new(json!({
                                    "port": args.get("port"),
                                    "env": args.get("env"),
                                })))
                            },
                        ),
                    )
                })]),
        )
    }

    #[tokio::test]
    async fn missing_arg_recovery_asks_through_the_prompter_and_reasks_a_bad_value() {
        let prompter = Arc::new(ScriptedPrompter::new([
            Answer::text("eighty"),
            Answer::text("8080"),
            Answer::Select(1),
        ]));
        let output = recovery_cli(Arc::clone(&prompter))
            .run(["test-cli", "tool", "deploy", "--interactive", "--json"])
            .await;

        assert_eq!(output.exit_code, 0, "{}", output.rendered);
        let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
        assert_eq!(value["data"]["port"], json!(8080));
        assert_eq!(value["data"]["env"], json!("prod"));
        assert_eq!(prompter.remaining(), 0);
        let asked = prompter.transcript();
        assert!(
            asked.iter().any(|line| line.starts_with("  rejected:")),
            "the bad port should have been rejected and re-asked: {asked:?}"
        );
    }

    #[tokio::test]
    async fn cancelling_during_missing_arg_recovery_prints_a_resume_command() {
        let prompter = Arc::new(ScriptedPrompter::new([Answer::Cancel]));
        let output = recovery_cli(prompter)
            .run(["test-cli", "tool", "deploy", "--interactive", "--json"])
            .await;

        assert_eq!(output.exit_code, 130, "{}", output.rendered);
        assert!(output.rendered.contains("Resume"), "{}", output.rendered);
    }
}
