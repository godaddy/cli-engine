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

    /// A command with optional args, some opted into prompting.
    fn optional_cli(prompter: Arc<ScriptedPrompter>) -> Cli {
        Cli::new(
            CliConfig::new("test-cli", "Test CLI", "test-cli")
                .with_prompter(prompter)
                .with_modules(vec![Module::new("Test", |_ctx| {
                    RuntimeGroupSpec::new(GroupSpec::new("tool", "Tools")).with_command(
                        RuntimeCommandSpec::new(
                            CommandSpec::new("make", "Make a thing")
                                .no_auth(true)
                                .with_arg(Arg::new("name").long("name").required(true))
                                .with_arg(
                                    Arg::new("note")
                                        .long("note")
                                        .help("A short note")
                                        .value_name("TEXT"),
                                )
                                .with_arg(
                                    Arg::new("retries")
                                        .long("retries")
                                        .value_parser(clap::value_parser!(u8)),
                                )
                                .with_arg(
                                    Arg::new("quiet")
                                        .long("quiet")
                                        .action(clap::ArgAction::SetTrue),
                                )
                                .with_arg(Arg::new("unprompted").long("unprompted"))
                                .with_prompted_args(["note", "retries", "quiet"]),
                            async |_credential, args| {
                                Ok(CommandResult::new(json!({
                                    "note": args.get("note"),
                                    "retries": args.get("retries"),
                                    "quiet": args.get("quiet"),
                                    "unprompted": args.get("unprompted"),
                                })))
                            },
                        ),
                    )
                })]),
        )
    }

    async fn run_optional(
        answers: impl IntoIterator<Item = Answer>,
        flags: &[&str],
    ) -> (serde_json::Value, Arc<ScriptedPrompter>, i32, String) {
        let prompter = Arc::new(ScriptedPrompter::new(answers));
        let mut args = vec!["test-cli", "tool", "make", "--name", "x", "--json"];
        args.extend_from_slice(flags);
        let output = optional_cli(Arc::clone(&prompter)).run(args).await;
        let value = serde_json::from_str(&output.rendered).unwrap_or(json!(null));
        (value, prompter, output.exit_code, output.rendered)
    }

    #[tokio::test]
    async fn prompted_optional_args_are_asked_and_applied() {
        let (value, prompter, code, rendered) = run_optional(
            [
                Answer::text("hello"),
                Answer::text("3"),
                Answer::Confirm(true),
            ],
            &["--interactive"],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["note"], json!("hello"));
        assert_eq!(value["data"]["retries"], json!(3));
        assert_eq!(value["data"]["quiet"], json!(true));
        assert_eq!(value["data"]["unprompted"], json!(null));
        assert_eq!(prompter.remaining(), 0);
        let asked = prompter.transcript().join("\n");
        assert!(asked.contains("(optional)"), "{asked}");
        assert!(
            !asked.contains("unprompted"),
            "an arg not opted in is never asked: {asked}"
        );
    }

    #[tokio::test]
    async fn a_blank_answer_skips_an_optional_arg() {
        let (value, _, code, rendered) = run_optional(
            [Answer::text(""), Answer::text(""), Answer::Confirm(false)],
            &["--interactive"],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["note"], json!(null));
        assert_eq!(value["data"]["retries"], json!(null));
        assert_eq!(value["data"]["quiet"], json!(false));
    }

    #[tokio::test]
    async fn an_optional_arg_already_supplied_is_not_asked_again() {
        let (value, prompter, code, rendered) = run_optional(
            [Answer::text("3"), Answer::Confirm(false)],
            &["--interactive", "--note", "given"],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["note"], json!("given"));
        assert_eq!(prompter.remaining(), 0);
        assert!(
            !prompter
                .transcript()
                .iter()
                .any(|line| line.contains("note")),
            "{:?}",
            prompter.transcript()
        );
    }

    #[tokio::test]
    async fn a_bad_optional_answer_is_explained_and_asked_again() {
        let (value, prompter, code, rendered) = run_optional(
            [
                Answer::text(""),
                Answer::text("many"),
                Answer::text("2"),
                Answer::Confirm(false),
            ],
            &["--interactive"],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["retries"], json!(2));
        assert!(
            prompter
                .transcript()
                .iter()
                .any(|line| line.starts_with("  rejected:")),
            "{:?}",
            prompter.transcript()
        );
    }

    #[tokio::test]
    async fn optional_args_are_not_asked_when_prompting_is_off() {
        let (value, prompter, code, rendered) =
            run_optional([Answer::text("unused")], &["--non-interactive"]).await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["note"], json!(null));
        assert_eq!(prompter.remaining(), 1, "nothing may be asked");
    }

    #[tokio::test]
    async fn cancelling_an_optional_prompt_exits_130_with_a_resume_command() {
        let (_, _, code, rendered) = run_optional([Answer::Cancel], &["--interactive"]).await;

        assert_eq!(code, 130, "{rendered}");
        assert!(rendered.contains("Resume"), "{rendered}");
        assert!(rendered.contains("--name x"), "{rendered}");
    }

    #[test]
    fn prompt_repeat_asks_again_until_the_user_declines() {
        let prompter = ScriptedPrompter::new([
            Answer::Confirm(true),
            Answer::Confirm(true),
            Answer::Confirm(false),
        ]);
        let mut seen = 0;
        let rounds =
            cli_engine::prompt::prompt_repeat(&prompter, "Add one?", "Add another?", |_| {
                seen += 1;
                Ok(())
            })
            .expect("repeat");

        assert_eq!((rounds, seen), (2, 2));
        assert_eq!(
            prompter.transcript(),
            ["Add one?", "Add another?", "Add another?"]
        );
    }

    #[test]
    fn prompt_repeat_stops_with_the_error_from_a_round() {
        let prompter = ScriptedPrompter::new([Answer::Confirm(true), Answer::Confirm(true)]);
        let err = cli_engine::prompt::prompt_repeat(&prompter, "Add one?", "Again?", |_| {
            Err(cli_engine::CliCoreError::message("round failed"))
        })
        .expect_err("round error");

        assert!(err.to_string().contains("round failed"), "{err}");
        assert_eq!(prompter.remaining(), 1);
    }

    #[tokio::test]
    async fn missing_required_args_are_asked_before_the_optional_ones() {
        let prompter = Arc::new(ScriptedPrompter::new([
            Answer::text("widget"),
            Answer::text("a note"),
            Answer::text(""),
            Answer::Confirm(false),
        ]));
        let output = optional_cli(Arc::clone(&prompter))
            .run(["test-cli", "tool", "make", "--interactive", "--json"])
            .await;

        assert_eq!(output.exit_code, 0, "{}", output.rendered);
        let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
        assert_eq!(value["data"]["note"], json!("a note"));
        assert_eq!(prompter.remaining(), 0);
        let asked = prompter.transcript();
        let name_at = asked.iter().position(|line| line.contains("name"));
        let note_at = asked.iter().position(|line| line.contains("note"));
        assert!(name_at < note_at, "required first: {asked:?}");
    }

    /// A command with a positional, a short-only option, a repeated option
    /// and an option that conflicts with another, for the review regressions.
    fn edge_cli(prompter: Arc<ScriptedPrompter>) -> Cli {
        Cli::new(
            CliConfig::new("test-cli", "Test CLI", "test-cli")
                .with_prompter(prompter)
                .with_modules(vec![Module::new("Test", |_ctx| {
                    RuntimeGroupSpec::new(GroupSpec::new("tool", "Tools")).with_command(
                        RuntimeCommandSpec::new(
                            CommandSpec::new("edge", "Edge cases")
                                .no_auth(true)
                                .with_arg(Arg::new("files").num_args(0..).allow_hyphen_values(true))
                                .with_arg(Arg::new("description").short('d'))
                                .with_arg(
                                    Arg::new("quiet")
                                        .short('q')
                                        .action(clap::ArgAction::SetTrue),
                                )
                                .with_arg(
                                    Arg::new("pair")
                                        .long("pair")
                                        .value_delimiter(',')
                                        .num_args(2),
                                )
                                .with_arg(Arg::new("mode").long("mode").conflicts_with("pair"))
                                .with_prompted_args(["description", "quiet", "pair", "mode"]),
                            async |_credential, args| {
                                Ok(CommandResult::new(json!({
                                    "files": args.get("files"),
                                    "description": args.get("description"),
                                    "quiet": args.get("quiet"),
                                    "pair": args.get("pair"),
                                    "mode": args.get("mode"),
                                })))
                            },
                        ),
                    )
                })]),
        )
    }

    async fn run_edge(
        answers: impl IntoIterator<Item = Answer>,
        extra: &[&str],
    ) -> (serde_json::Value, Arc<ScriptedPrompter>, i32, String) {
        let prompter = Arc::new(ScriptedPrompter::new(answers));
        let mut args = vec!["test-cli", "tool", "edge", "--interactive", "--json"];
        args.extend_from_slice(extra);
        let output = edge_cli(Arc::clone(&prompter)).run(args).await;
        let value = serde_json::from_str(&output.rendered).unwrap_or(json!(null));
        (value, prompter, output.exit_code, output.rendered)
    }

    #[tokio::test]
    async fn a_short_only_option_is_prompted_and_applied() {
        let (value, _, code, rendered) = run_edge(
            [
                Answer::text("hello"),
                Answer::Confirm(true),
                Answer::text(""),
                Answer::text(""),
            ],
            &[],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["description"], json!("hello"));
        assert_eq!(value["data"]["quiet"], json!(true));
        assert_eq!(
            value["data"]["files"],
            json!(null),
            "no positional may be populated"
        );
    }

    #[tokio::test]
    async fn prompted_options_go_before_the_double_dash_separator() {
        let (value, _, code, rendered) = run_edge(
            [
                Answer::text("hello"),
                Answer::Confirm(false),
                Answer::text(""),
                Answer::text(""),
            ],
            &["--", "-file.json"],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["description"], json!("hello"));
        assert!(
            value["data"]["files"].to_string().contains("-file.json"),
            "the positional after -- must stay positional: {value}"
        );
        assert_eq!(value["data"]["quiet"], json!(false));
    }

    #[tokio::test]
    async fn an_answer_with_the_wrong_number_of_values_is_asked_again_not_aborted() {
        // `--pair` needs two values, so no single typed answer can satisfy it.
        // The point is that the wrong count is explained and asked again,
        // instead of the final parse aborting the run; a blank answer skips.
        let (value, prompter, code, rendered) = run_edge(
            [
                Answer::text(""),
                Answer::Confirm(false),
                Answer::text("one"),
                Answer::text(""),
                Answer::text(""),
            ],
            &[],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["pair"], json!(null));
        let transcript = prompter.transcript();
        assert!(
            transcript
                .iter()
                .any(|line| line.starts_with("  rejected:")),
            "{transcript:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_conflicting_with_a_supplied_option_is_asked_again() {
        let (value, prompter, code, rendered) = run_edge(
            [
                Answer::text(""),
                Answer::Confirm(false),
                Answer::text("a,b"),
                Answer::text(""),
            ],
            &["--mode", "fast"],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["pair"], json!(null));
        assert!(
            prompter
                .transcript()
                .iter()
                .any(|line| line.starts_with("  rejected:")),
            "the conflicting answer should have been rejected: {:?}",
            prompter.transcript()
        );
    }

    #[tokio::test]
    async fn a_prompt_failure_keeps_its_diagnostic_instead_of_reading_as_a_cancel() {
        // The script runs out at the first prompt: that is a failure, not a cancel.
        let (_, _, code, rendered) = run_edge([], &[]).await;

        assert_ne!(code, 130, "{rendered}");
        assert_ne!(code, 0, "{rendered}");
        assert!(rendered.contains("ran out of answers"), "{rendered}");
        assert!(!rendered.contains("Resume"), "{rendered}");
    }

    #[tokio::test]
    async fn a_missing_arg_recovery_failure_keeps_its_diagnostic() {
        let prompter = Arc::new(ScriptedPrompter::new([]));
        let output = recovery_cli(prompter)
            .run(["test-cli", "tool", "deploy", "--interactive", "--json"])
            .await;

        assert_ne!(output.exit_code, 130, "{}", output.rendered);
        assert_ne!(output.exit_code, 0, "{}", output.rendered);
        assert!(
            output.rendered.contains("ran out of answers"),
            "{}",
            output.rendered
        );
    }

    /// Options that stress the answer plumbing: an option that must be written
    /// `--opt=value`, a flag and an enum that conflict with another option, and
    /// a required numeric positional for the recovery validator.
    fn plumbing_cli(prompter: Arc<ScriptedPrompter>) -> Cli {
        Cli::new(
            CliConfig::new("test-cli", "Test CLI", "test-cli")
                .with_prompter(prompter)
                .with_modules(vec![Module::new("Test", |_ctx| {
                    RuntimeGroupSpec::new(GroupSpec::new("tool", "Tools"))
                        .with_command(RuntimeCommandSpec::new(
                            CommandSpec::new("plumb", "Plumbing")
                                .no_auth(true)
                                .with_arg(Arg::new("label").long("label").require_equals(true))
                                .with_arg(
                                    Arg::new("fast")
                                        .long("fast")
                                        .conflicts_with("level")
                                        .action(clap::ArgAction::SetTrue),
                                )
                                .with_arg(
                                    Arg::new("level")
                                        .long("level")
                                        .value_parser(["low", "high"])
                                        .conflicts_with("fast"),
                                )
                                .with_prompted_args(["label", "fast", "level"]),
                            async |_credential, args| {
                                Ok(CommandResult::new(json!({
                                    "label": args.get("label"),
                                    "fast": args.get("fast"),
                                    "level": args.get("level"),
                                })))
                            },
                        ))
                        .with_command(RuntimeCommandSpec::new(
                            CommandSpec::new("count", "Needs a number")
                                .no_auth(true)
                                .with_arg(
                                    Arg::new("n")
                                        .required(true)
                                        .value_parser(clap::value_parser!(u16)),
                                ),
                            async |_credential, args| {
                                Ok(CommandResult::new(json!({ "n": args.get("n") })))
                            },
                        ))
                        .with_command(RuntimeCommandSpec::new(
                            CommandSpec::new("who", "Two short-only required options")
                                .no_auth(true)
                                .with_arg(
                                    Arg::new("provider-name")
                                        .short('p')
                                        .value_name("NAME")
                                        .required(true),
                                )
                                .with_arg(
                                    Arg::new("service-name")
                                        .short('s')
                                        .value_name("NAME")
                                        .required(true),
                                ),
                            async |_credential, args| {
                                Ok(CommandResult::new(json!({
                                    "provider": args.get("provider-name"),
                                    "service": args.get("service-name"),
                                })))
                            },
                        ))
                })]),
        )
    }

    async fn run_plumb(
        command: &str,
        answers: impl IntoIterator<Item = Answer>,
        extra: &[&str],
    ) -> (serde_json::Value, Arc<ScriptedPrompter>, i32, String) {
        let prompter = Arc::new(ScriptedPrompter::new(answers));
        let mut args = vec!["test-cli", "tool", command, "--interactive", "--json"];
        args.extend_from_slice(extra);
        let output = plumbing_cli(Arc::clone(&prompter)).run(args).await;
        let value = serde_json::from_str(&output.rendered).unwrap_or(json!(null));
        (value, prompter, output.exit_code, output.rendered)
    }

    #[tokio::test]
    async fn an_option_that_requires_equals_is_written_attached() {
        let (value, _, code, rendered) = run_plumb(
            "plumb",
            [
                Answer::text("hello"),
                Answer::Confirm(false),
                Answer::Select(2),
            ],
            &[],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["label"], json!("hello"));
    }

    #[tokio::test]
    async fn a_flag_that_conflicts_with_a_supplied_option_is_not_offered() {
        // `--level high` is supplied, so asking about `--fast` would abort the
        // run: it is skipped without consuming an answer.
        let (value, prompter, code, rendered) =
            run_plumb("plumb", [Answer::text("")], &["--level", "high"]).await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["fast"], json!(false));
        assert_eq!(prompter.remaining(), 0);
        assert!(
            !prompter
                .transcript()
                .iter()
                .any(|line| line.contains("fast")),
            "{:?}",
            prompter.transcript()
        );
    }

    #[tokio::test]
    async fn a_menu_choice_that_conflicts_with_a_supplied_option_is_not_offered() {
        let (value, prompter, code, rendered) = run_plumb(
            "plumb",
            [Answer::text(""), Answer::Confirm(true)],
            &["--fast"],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["level"], json!(null));
        assert!(
            !prompter
                .transcript()
                .iter()
                .any(|line| line.contains("level")),
            "{:?}",
            prompter.transcript()
        );
    }

    #[tokio::test]
    async fn an_answer_that_reads_as_a_flag_is_rejected_not_accepted() {
        // `--bogus` and `--help` would be parsed as options, not as the value.
        let (value, prompter, code, rendered) = run_plumb(
            "count",
            [
                Answer::text("--bogus"),
                Answer::text("--help"),
                Answer::text("7"),
            ],
            &[],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["n"], json!(7));
        let rejected = prompter
            .transcript()
            .iter()
            .filter(|line| line.starts_with("  rejected:"))
            .count();
        assert_eq!(rejected, 2, "{:?}", prompter.transcript());
    }

    #[tokio::test]
    async fn recovery_finds_the_right_short_only_option_among_shared_value_names() {
        let (value, _, code, rendered) =
            run_plumb("who", [Answer::text("Acme"), Answer::text("Email")], &[]).await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["provider"], json!("Acme"));
        assert_eq!(value["data"]["service"], json!("Email"));
    }

    /// Options with requirements, for the answer-introduces-a-requirement cases.
    fn requires_cli(prompter: Arc<ScriptedPrompter>) -> Cli {
        Cli::new(
            CliConfig::new("test-cli", "Test CLI", "test-cli")
                .with_prompter(prompter)
                .with_modules(vec![Module::new("Test", |_ctx| {
                    RuntimeGroupSpec::new(GroupSpec::new("tool", "Tools")).with_command(
                        RuntimeCommandSpec::new(
                            CommandSpec::new("ship", "Ship it")
                                .no_auth(true)
                                .with_arg(Arg::new("destination").long("destination"))
                                .with_arg(
                                    Arg::new("publish").long("publish").requires("destination"),
                                )
                                .with_prompted_args(["publish", "destination"]),
                            async |_credential, args| {
                                Ok(CommandResult::new(json!({
                                    "publish": args.get("publish"),
                                    "destination": args.get("destination"),
                                })))
                            },
                        ),
                    )
                })]),
        )
    }

    #[tokio::test]
    async fn an_answer_that_introduces_a_new_requirement_is_not_accepted() {
        // `--publish` needs `--destination`, which has not been given.
        let prompter = Arc::new(ScriptedPrompter::new([
            Answer::text("now"),
            Answer::text("prod"),
        ]));
        let output = requires_cli(Arc::clone(&prompter))
            .run(["test-cli", "tool", "ship", "--interactive", "--json"])
            .await;

        let transcript = prompter.transcript();
        assert!(
            transcript
                .iter()
                .any(|line| line.starts_with("  rejected:")),
            "an answer that leaves the command invalid must be re-asked: {transcript:?}"
        );
        assert_ne!(output.exit_code, 0, "{}", output.rendered);
    }

    #[tokio::test]
    async fn options_go_before_a_variadic_list_that_accepts_hyphenated_values() {
        let (value, _, code, rendered) = run_edge(
            [
                Answer::text("hello"),
                Answer::Confirm(false),
                Answer::text(""),
                Answer::text(""),
            ],
            &["file.json"],
        )
        .await;

        assert_eq!(code, 0, "{rendered}");
        assert_eq!(value["data"]["description"], json!("hello"));
        assert!(
            value["data"]["files"].to_string().contains("file.json")
                && !value["data"]["files"].to_string().contains("hello"),
            "the answer must not be swallowed by the variadic list: {value}"
        );
    }

    #[test]
    fn a_resume_command_keeps_an_answer_with_spaces_as_one_argument() {
        let resume = cli_engine::prompt::build_resume_command(
            "my cli",
            &[
                "--note".to_owned(),
                "hello world".to_owned(),
                "plain".to_owned(),
            ],
        );
        assert_eq!(resume, "\"my cli\" --note \"hello world\" plain");
    }

    #[tokio::test]
    async fn cancelling_after_a_spaced_answer_prints_a_copyable_resume_command() {
        let (_, _, code, rendered) = run_optional(
            [Answer::text("hello world"), Answer::Cancel],
            &["--interactive"],
        )
        .await;

        assert_eq!(code, 130, "{rendered}");
        assert!(rendered.contains("--note \"hello world\""), "{rendered}");
    }

    #[tokio::test]
    async fn a_prompt_failure_uses_the_requested_output_format() {
        let prompter = Arc::new(ScriptedPrompter::new([]));
        let output = optional_cli(prompter)
            .run([
                "test-cli",
                "tool",
                "make",
                "--name",
                "x",
                "--interactive",
                "--toon",
            ])
            .await;

        assert_ne!(output.exit_code, 0, "{}", output.rendered);
        assert!(
            serde_json::from_str::<serde_json::Value>(&output.rendered).is_err(),
            "--toon must not produce a JSON diagnostic: {}",
            output.rendered
        );
        assert!(
            output.rendered.contains("ran out of answers"),
            "{}",
            output.rendered
        );
    }

    #[tokio::test]
    async fn a_recovery_failure_uses_the_requested_output_format() {
        let prompter = Arc::new(ScriptedPrompter::new([]));
        let output = recovery_cli(prompter)
            .run(["test-cli", "tool", "deploy", "--interactive", "--toon"])
            .await;

        assert_ne!(output.exit_code, 0, "{}", output.rendered);
        assert!(
            serde_json::from_str::<serde_json::Value>(&output.rendered).is_err(),
            "--toon must not produce a JSON diagnostic: {}",
            output.rendered
        );
    }

    /// Nested groups and a global value option, so the leaf is not the first
    /// word after the program name.
    fn nested_cli(prompter: Arc<ScriptedPrompter>) -> Cli {
        let make = |name: &'static str| {
            RuntimeCommandSpec::new(
                CommandSpec::new(name, "Make")
                    .no_auth(true)
                    .with_arg(Arg::new("note").long("note"))
                    .with_prompted_args(["note"]),
                async |_credential, args| {
                    Ok(CommandResult::new(json!({ "note": args.get("note") })))
                },
            )
        };
        Cli::new(
            CliConfig::new("test-cli", "Test CLI", "test-cli")
                .with_prompter(prompter)
                .with_register_flags(Arc::new(|command: clap::Command| {
                    command.arg(
                        Arg::new("region")
                            .long("region")
                            .global(true)
                            .help("A global option that takes a value"),
                    )
                }))
                .with_modules(vec![
                    Module::new("Test", move |_ctx| {
                        RuntimeGroupSpec::new(GroupSpec::new("project", "Projects"))
                            .with_command(make("list"))
                    }),
                    Module::new("Test", move |_ctx| {
                        RuntimeGroupSpec::new(GroupSpec::new("domain", "Domains"))
                            .with_command(make("make"))
                    }),
                ]),
        )
    }

    #[tokio::test]
    async fn the_leaf_is_found_after_an_option_whose_value_names_another_group() {
        // `--region project` consumes `project`; the command is `domain make`.
        let prompter = Arc::new(ScriptedPrompter::new([Answer::text("hello")]));
        let output = nested_cli(Arc::clone(&prompter))
            .run([
                "test-cli",
                "--region",
                "project",
                "domain",
                "make",
                "--interactive",
                "--json",
            ])
            .await;

        assert_eq!(output.exit_code, 0, "{}", output.rendered);
        let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
        assert_eq!(value["data"]["note"], json!("hello"));
        assert_eq!(prompter.remaining(), 0);
    }

    /// A required positional whose value name matches an unrelated option.
    fn shaped_cli(prompter: Arc<ScriptedPrompter>) -> Cli {
        Cli::new(
            CliConfig::new("test-cli", "Test CLI", "test-cli")
                .with_prompter(prompter)
                .with_modules(vec![Module::new("Test", |_ctx| {
                    RuntimeGroupSpec::new(GroupSpec::new("tool", "Tools")).with_command(
                        RuntimeCommandSpec::new(
                            CommandSpec::new("shape", "Positional vs option")
                                .no_auth(true)
                                .with_arg(Arg::new("input").value_name("name").required(true))
                                .with_arg(Arg::new("name").long("name")),
                            async |_credential, args| {
                                Ok(CommandResult::new(json!({
                                    "input": args.get("input"),
                                    "name": args.get("name"),
                                })))
                            },
                        ),
                    )
                })]),
        )
    }

    #[tokio::test]
    async fn a_missing_positional_is_not_mistaken_for_an_option_with_the_same_name() {
        let prompter = Arc::new(ScriptedPrompter::new([Answer::text("value")]));
        let output = shaped_cli(Arc::clone(&prompter))
            .run(["test-cli", "tool", "shape", "--interactive", "--json"])
            .await;

        assert_eq!(output.exit_code, 0, "{}", output.rendered);
        let value: serde_json::Value = serde_json::from_str(&output.rendered).expect("json");
        assert_eq!(value["data"]["input"], json!("value"));
        assert_eq!(
            value["data"]["name"],
            json!(null),
            "the option must stay unset"
        );
        assert!(
            !prompter
                .transcript()
                .iter()
                .any(|line| line.starts_with("  rejected:")),
            "{:?}",
            prompter.transcript()
        );
    }
}
