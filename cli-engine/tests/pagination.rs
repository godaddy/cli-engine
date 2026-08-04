//! End-to-end coverage for opt-in pagination (`CommandSpec::with_pagination`),
//! driven through `Cli::run` the way a real consumer binary would.
//!
//! `--limit`/`--offset` are deliberately not framework-global: a command only
//! gets them — in `--help` and on its command line — by declaring a
//! `PaginationConfig`. These tests pin that gating, plus `default_limit` and
//! `max_limit` behavior, at the only surface a real consumer CLI uses.

use cli_engine::{
    Cli, CliConfig, CommandResult, CommandSpec, PaginationConfig, RuntimeCommandSpec,
};
use serde_json::json;

fn items() -> Vec<serde_json::Value> {
    vec![
        json!({"name": "alpha"}),
        json!({"name": "beta"}),
        json!({"name": "gamma"}),
        json!({"name": "delta"}),
    ]
}

fn cli_with_list_command(spec: CommandSpec) -> Cli {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(spec, async |_credential, _args| {
        Ok(CommandResult::new(json!(items())))
    }));
    cli
}

#[tokio::test]
async fn limit_and_offset_are_unknown_arguments_for_a_command_that_did_not_opt_in() {
    let cli = cli_with_list_command(CommandSpec::new("list", "List things").no_auth(true));

    let output = cli.run(["my-cli", "list", "--limit", "1"]).await;
    assert_eq!(
        output.exit_code, 2,
        "unopted command should reject --limit as unknown: {}",
        output.rendered
    );

    let output = cli.run(["my-cli", "list", "--offset", "1"]).await;
    assert_eq!(
        output.exit_code, 2,
        "unopted command should reject --offset as unknown: {}",
        output.rendered
    );

    let help = cli.run(["my-cli", "list", "--help"]).await;
    assert!(
        !help.rendered.contains("--limit") && !help.rendered.contains("--offset"),
        "unopted command's --help should not mention pagination flags: {}",
        help.rendered
    );
}

#[tokio::test]
async fn opted_in_command_documents_limit_and_offset_in_help() {
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_pagination(PaginationConfig {
                default_limit: 2,
                max_limit: 3,
            }),
    );

    let help = cli.run(["my-cli", "list", "--help"]).await;
    assert!(help.rendered.contains("--limit"), "{}", help.rendered);
    assert!(help.rendered.contains("--offset"), "{}", help.rendered);
}

#[tokio::test]
async fn default_limit_applies_when_neither_flag_is_passed() {
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_pagination(PaginationConfig {
                default_limit: 2,
                ..PaginationConfig::default()
            }),
    );

    let output = cli.run(["my-cli", "list", "--output", "json"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!([{"name": "alpha"}, {"name": "beta"}])
    );
}

#[tokio::test]
async fn explicit_limit_and_offset_override_the_default_and_attach_pagination_metadata() {
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_pagination(PaginationConfig {
                default_limit: 2,
                ..PaginationConfig::default()
            }),
    );

    let output = cli
        .run([
            "my-cli",
            "list",
            "--offset",
            "1",
            "--limit",
            "2",
            "--verbose",
            "pagination",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!([{"name": "beta"}, {"name": "gamma"}])
    );
    assert_eq!(
        rendered["metadata"]["pagination"],
        json!({"total": 4, "offset": 1, "limit": 2, "count": 2})
    );
}

#[tokio::test]
async fn max_limit_rejects_an_explicit_limit_above_the_cap_but_allows_the_cap_itself() {
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_pagination(PaginationConfig {
                max_limit: 3,
                ..PaginationConfig::default()
            }),
    );

    let output = cli.run(["my-cli", "list", "--limit", "4"]).await;
    assert_eq!(
        output.exit_code, 2,
        "--limit above max_limit should be a usage error: {}",
        output.rendered
    );

    let output = cli
        .run(["my-cli", "list", "--limit", "3", "--output", "json"])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
}

#[tokio::test]
async fn max_limit_does_not_constrain_a_negative_limit() {
    // Negative `--limit` means "no limit" downstream (see `apply_pagination`
    // in `output/pipeline.rs`), same legacy behavior as when `--limit` was a
    // framework-global flag; `max_limit` only caps an explicit positive ask.
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_pagination(PaginationConfig {
                max_limit: 1,
                ..PaginationConfig::default()
            }),
    );

    let output = cli
        .run(["my-cli", "list", "--limit", "-1", "--output", "json"])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["data"], json!(items()));
}

/// A command author setting `default_limit` above `max_limit` is a
/// misconfiguration that can never satisfy an unset `--limit`; caught at
/// registration time as a development-time safety net, same idiom as
/// `CommandSpec::from_args`'s empty-required-`ArgGroup` debug_assert.
#[test]
#[should_panic(expected = "greater than its max_limit")]
fn with_pagination_panics_when_default_limit_exceeds_max_limit() {
    let _unused = CommandSpec::new("list", "List things").with_pagination(PaginationConfig {
        default_limit: 10,
        max_limit: 5,
    });
}
