//! End-to-end coverage for opt-in pagination (`CommandSpec::with_pagination`),
//! driven through `Cli::run` the way a real consumer binary would.
//!
//! `--limit`/`--offset` are deliberately not framework-global: a command only
//! gets them — in `--help` and on its command line — by declaring a
//! `PaginationConfig`. These tests pin that gating, plus `default_limit` and
//! `max_limit` behavior, at the only surface a real consumer CLI uses.

use clap::Arg;
use cli_engine::{
    Cli, CliConfig, CommandResult, CommandSpec, CredentialResolver, PaginationConfig,
    RuntimeCommandSpec,
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
    // Pagination facts and the next-page suggestion are always present — no
    // `--verbose` needed, unlike `metadata`.
    assert_eq!(
        rendered["pagination"],
        json!({"total": 4, "offset": 0, "limit": 2, "count": 2, "has_more": true})
    );
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "list --limit 2 --offset 2"
    );
    assert!(rendered.get("metadata").is_none(), "{}", output.rendered);
}

#[tokio::test]
async fn explicit_limit_and_offset_override_the_default_and_expose_pagination() {
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
            "my-cli", "list", "--offset", "1", "--limit", "2", "--output", "json",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!([{"name": "beta"}, {"name": "gamma"}])
    );
    assert_eq!(
        rendered["pagination"],
        json!({"total": 4, "offset": 1, "limit": 2, "count": 2, "has_more": true})
    );
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "list --limit 2 --offset 3"
    );
    assert_eq!(
        rendered["next_actions"][0]["description"],
        "View the next page (offset 3 of 4 total)"
    );
}

#[tokio::test]
async fn last_page_has_no_next_action_and_has_more_is_false() {
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
            "my-cli", "list", "--offset", "2", "--limit", "2", "--output", "json",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["pagination"]["has_more"], false);
    assert!(
        rendered.get("next_actions").is_none(),
        "no next page exists: {}",
        output.rendered
    );
}

#[tokio::test]
async fn next_page_action_replays_other_flags_the_user_passed() {
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_arg(Arg::new("status").long("status"))
            .with_pagination(PaginationConfig {
                default_limit: 2,
                ..PaginationConfig::default()
            }),
    );

    let output = cli
        .run(["my-cli", "list", "--status", "active", "--output", "json"])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "list --status active --limit 2 --offset 2"
    );
}

#[tokio::test]
async fn next_page_action_quotes_values_with_whitespace() {
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_arg(Arg::new("status").long("status"))
            .with_pagination(PaginationConfig {
                default_limit: 2,
                ..PaginationConfig::default()
            }),
    );

    let output = cli
        .run([
            "my-cli",
            "list",
            "--status",
            "in review",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "list --status \"in review\" --limit 2 --offset 2"
    );
}

#[derive(Debug, Clone, clap::Args)]
struct ListArgs {
    #[arg(long)]
    sort_order: String,
}

#[tokio::test]
async fn next_page_action_uses_the_real_long_flag_not_the_value_map_key() {
    // `sort_order`'s clap id is the field name, but its long flag is
    // kebab-cased (`--sort-order`) — the reconstructed command must use the
    // real flag, not the value-map key (see `tests/derive_bridge.rs` for the
    // same id/flag mismatch on `page_size`/`--page-size`).
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new_typed::<ListArgs, _, _, _>(
        CommandSpec::from_args::<ListArgs>("list", "List things")
            .no_auth(true)
            .with_pagination(PaginationConfig {
                default_limit: 2,
                ..PaginationConfig::default()
            }),
        async |_credential: CredentialResolver, _args: ListArgs| {
            Ok(CommandResult::new(json!(items())))
        },
    ));

    let output = cli
        .run(["my-cli", "list", "--sort-order", "asc", "--output", "json"])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "list --sort-order asc --limit 2 --offset 2"
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

#[tokio::test]
async fn negative_offset_is_rejected_at_parse_time_not_at_runtime() {
    // Unlike `--limit`, a negative `--offset` has no meaning downstream —
    // `apply_pagination` in `output/pipeline.rs` rejects it unconditionally.
    // Reject it as a `clap` usage error (exit code 2) up front instead of
    // letting the command run and fail partway through.
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_pagination(PaginationConfig::default()),
    );

    let output = cli.run(["my-cli", "list", "--offset", "-1"]).await;
    assert_eq!(
        output.exit_code, 2,
        "negative --offset should be a usage error: {}",
        output.rendered
    );
}

/// A command author setting `default_limit` above `max_limit` is a
/// misconfiguration that can never satisfy an unset `--limit`; caught at
/// registration time as a development-time safety net, same idiom as
/// `CommandSpec::from_args`'s empty-required-`ArgGroup` debug_assert. The
/// check is a `debug_assert!` (compiled out in release builds, same as that
/// precedent), so this test only holds under `debug_assertions` — skip it
/// under `cargo test --release` rather than have it fail there.
#[test]
#[cfg_attr(
    debug_assertions,
    should_panic(expected = "greater than its max_limit")
)]
fn with_pagination_panics_when_default_limit_exceeds_max_limit() {
    let _unused = CommandSpec::new("list", "List things").with_pagination(PaginationConfig {
        default_limit: 10,
        max_limit: 5,
    });
}

#[tokio::test]
async fn human_output_shows_pagination_summary_and_next_steps() {
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_pagination(PaginationConfig {
                default_limit: 2,
                ..PaginationConfig::default()
            }),
    );

    let output = cli.run(["my-cli", "list", "--output", "human"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    // The pagination facts are merged into the table's row-count footer
    // rather than repeated on a separate line.
    assert!(
        output.rendered.contains("(2 of 4 rows, offset 0, limit 2)"),
        "{}",
        output.rendered
    );
    assert!(
        output.rendered.contains("Next steps:"),
        "{}",
        output.rendered
    );
    assert!(
        output.rendered.contains("list --limit 2 --offset 2"),
        "{}",
        output.rendered
    );
}

#[tokio::test]
async fn human_output_on_last_page_shows_summary_but_no_next_steps() {
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
            "my-cli", "list", "--offset", "2", "--limit", "2", "--output", "human",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    assert!(
        output.rendered.contains("(2 of 4 rows, offset 2, limit 2)"),
        "{}",
        output.rendered
    );
    assert!(
        !output.rendered.contains("Next steps:"),
        "no next page exists: {}",
        output.rendered
    );
}

#[tokio::test]
async fn next_page_action_preserves_filter_expr_and_fields() {
    // `--filter`/`--expr`/`--fields` sit in the same output pipeline as
    // pagination and change what data comes back — dropping them from the
    // suggested next-page command would make it return different results
    // than the command the user actually ran.
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
            "--filter",
            "name != 'alpha'",
            "--expr",
            "sort_by(@, &name)",
            "--fields",
            "name",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "list --filter \"name != 'alpha'\" --expr \"sort_by(@, &name)\" --fields name --limit 2 --offset 2"
    );
}

#[tokio::test]
async fn next_page_action_replays_a_set_false_flag_as_a_bare_switch() {
    // A `SetFalse` flag (e.g. `--no-cache`) never takes an explicit
    // `=value` token — its mere presence sets the value to `false`. Emitting
    // `--no-cache=false` would be an invalid replay.
    let cli = cli_with_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_arg(
                Arg::new("no_cache")
                    .long("no-cache")
                    .action(clap::ArgAction::SetFalse),
            )
            .with_pagination(PaginationConfig {
                default_limit: 2,
                ..PaginationConfig::default()
            }),
    );

    let output = cli
        .run(["my-cli", "list", "--no-cache", "--output", "json"])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "list --no-cache --limit 2 --offset 2"
    );
}

#[tokio::test]
async fn human_footer_shows_rows_actually_rendered_after_expr_reshapes_data() {
    // `--expr` runs after pagination in the output pipeline, so it can
    // change the rendered row count independently of `pagination.count`
    // (which reflects the pre-`--expr` slice). The footer's "shown" number
    // must track what's actually in the table, not the stale pagination count.
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
            "--expr",
            "[?name=='alpha']",
            "--output",
            "human",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    assert!(
        output.rendered.contains("(1 of 4 rows, offset 0, limit 2)"),
        "{}",
        output.rendered
    );
}
