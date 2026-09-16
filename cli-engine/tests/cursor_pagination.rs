//! End-to-end coverage for opt-in cursor pagination
//! (`CommandSpec::with_cursor`), driven through `Cli::run` the way a real
//! consumer binary would.
//!
//! `--limit`/`--continue` are deliberately not framework-global: a command
//! only gets them — in `--help` and on its command line — by declaring a
//! `CursorConfig`. Unlike offset pagination (`tests/pagination.rs`), the
//! engine never slices or measures a cursor itself: these tests drive a fake
//! in-memory "backend" through the handler, which reads back the parsed
//! `--limit`/`--continue` off `ctx.middleware` and reports what it learned
//! via `CommandResult::with_cursor`.

use clap::Arg;
use cli_engine::{
    Cli, CliConfig, CommandResult, CommandSpec, CursorConfig, CursorContinuation,
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

/// A fake cursor-backed handler: `--continue` is the next start index
/// (as a string), `--limit` is the page size. Reports a fresh continuation
/// token whenever more items remain, mirroring how a real handler would
/// resume an opaque backend cursor.
fn cli_with_cursor_list_command(spec: CommandSpec) -> Cli {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new_with_context(spec, async |ctx| {
        let all = items();
        let start = ctx
            .middleware
            .continue_token
            .as_deref()
            .and_then(|token| token.parse::<usize>().ok())
            .unwrap_or(0);
        let limit = usize::try_from(ctx.middleware.cursor_limit).unwrap_or(0);
        let end = start.saturating_add(limit).min(all.len());
        let page = all.get(start..end).unwrap_or_default().to_vec();
        let mut result = CommandResult::new(json!(page));
        if end < all.len() {
            result = result.with_cursor(CursorContinuation::more(end.to_string()));
        }
        Ok(result)
    }));
    cli
}

#[tokio::test]
async fn limit_and_continue_are_unknown_arguments_for_a_command_that_did_not_opt_in() {
    let cli = cli_with_cursor_list_command(CommandSpec::new("list", "List things").no_auth(true));

    let output = cli.run(["my-cli", "list", "--limit", "1"]).await;
    assert_eq!(
        output.exit_code, 2,
        "unopted command should reject --limit as unknown: {}",
        output.rendered
    );

    let output = cli.run(["my-cli", "list", "--continue", "1"]).await;
    assert_eq!(
        output.exit_code, 2,
        "unopted command should reject --continue as unknown: {}",
        output.rendered
    );

    let help = cli.run(["my-cli", "list", "--help"]).await;
    assert!(
        !help.rendered.contains("--limit") && !help.rendered.contains("--continue"),
        "unopted command's --help should not mention cursor flags: {}",
        help.rendered
    );
}

#[tokio::test]
async fn opted_in_command_documents_limit_and_continue_in_help() {
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 3,
            }),
    );

    let help = cli.run(["my-cli", "list", "--help"]).await;
    assert!(help.rendered.contains("--limit"), "{}", help.rendered);
    assert!(help.rendered.contains("--continue"), "{}", help.rendered);
}

#[tokio::test]
async fn default_limit_applies_when_neither_flag_is_passed() {
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
    );

    let output = cli.run(["my-cli", "list", "--output", "json"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["data"],
        json!([{"name": "alpha"}, {"name": "beta"}])
    );
    // total/remaining are absent — this fake backend never reports them.
    assert_eq!(
        rendered["cursor"],
        json!({"limit": 2, "count": 2, "continue_from": "2", "has_more": true})
    );
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "my-cli list --limit 2 --continue 2"
    );
    assert!(rendered.get("metadata").is_none(), "{}", output.rendered);
}

#[tokio::test]
async fn explicit_limit_and_continue_fetch_the_requested_page() {
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
    );

    let output = cli
        .run([
            "my-cli",
            "list",
            "--continue",
            "1",
            "--limit",
            "2",
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
    assert_eq!(rendered["cursor"]["continue_from"], "3");
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "my-cli list --limit 2 --continue 3"
    );
}

#[tokio::test]
async fn last_page_has_no_next_action_and_has_more_is_false() {
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
    );

    let output = cli
        .run([
            "my-cli",
            "list",
            "--continue",
            "2",
            "--limit",
            "2",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["cursor"]["has_more"], false);
    assert!(rendered["cursor"].get("continue_from").is_none());
    assert!(
        rendered.get("next_actions").is_none(),
        "no next page exists: {}",
        output.rendered
    );
}

#[tokio::test]
async fn with_total_and_remaining_surface_on_the_envelope() {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
        async |_credential, _args| {
            Ok(
                CommandResult::new(json!([{"name": "alpha"}, {"name": "beta"}])).with_cursor(
                    CursorContinuation::more("tok-2")
                        .with_total(4)
                        .with_remaining(2),
                ),
            )
        },
    ));

    let output = cli.run(["my-cli", "list", "--output", "json"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["cursor"],
        json!({
            "limit": 2,
            "count": 2,
            "total": 4,
            "remaining": 2,
            "continue_from": "tok-2",
            "has_more": true
        })
    );
}

/// A handler that derives its own effective page size from the `--continue`
/// token (e.g. to let a caller resume with `--continue` alone, without
/// repeating `--limit`) reports that via `CursorContinuation::with_limit`.
/// The envelope's `cursor.limit` reflects that effective size, not the
/// parsed `--limit` this particular invocation happened to carry — and the
/// suggested next-page command omits `--limit` entirely, since `with_limit`
/// also signals that the token is self-sufficient about size.
#[tokio::test]
async fn with_limit_overrides_the_envelope_and_omits_limit_from_the_next_action() {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 25,
                max_limit: 0,
            }),
        async |_credential, _args| {
            Ok(
                CommandResult::new(json!([{"name": "alpha"}, {"name": "beta"}]))
                    .with_cursor(CursorContinuation::more("tok-2").with_limit(2)),
            )
        },
    ));

    // No --limit passed at all — the parsed value defaults to 25, but the
    // handler says it actually applied 2 (inherited from a prior token).
    let output = cli.run(["my-cli", "list", "--output", "json"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(rendered["cursor"]["limit"], json!(2));
    let next_actions = rendered["next_actions"].as_array().expect("next_actions");
    // `with_limit` means the token is self-sufficient about page size, so
    // the replay omits `--limit` entirely rather than repeating a value
    // the token already carries (and that would be wrong here anyway —
    // the parsed 25, not the effective 2).
    assert_eq!(
        next_actions[0]["command"], "my-cli list --continue tok-2",
        "next-page command should omit --limit, not repeat the parsed default (25): {}",
        output.rendered
    );
}

#[tokio::test]
async fn max_limit_rejects_an_explicit_limit_above_the_cap_but_allows_the_cap_itself() {
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 1,
                max_limit: 3,
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
async fn zero_and_negative_limit_are_rejected_at_parse_time() {
    // Unlike offset pagination, a cursor's `--limit` has no "0/negative means
    // unlimited" reading — it's a per-request page size sent to a backend,
    // not a bound on data the framework already holds.
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 1,
                max_limit: 0,
            }),
    );

    let output = cli.run(["my-cli", "list", "--limit", "0"]).await;
    assert_eq!(
        output.exit_code, 2,
        "--limit 0 should be a usage error: {}",
        output.rendered
    );

    let output = cli.run(["my-cli", "list", "--limit", "-1"]).await;
    assert_eq!(
        output.exit_code, 2,
        "negative --limit should be a usage error: {}",
        output.rendered
    );
}

/// A command author setting `default_limit` to `0` (or negative) can never
/// satisfy an unset `--limit` with a valid page size; caught at registration
/// time as a development-time safety net, same idiom as
/// `with_pagination`'s `default_limit > max_limit` debug_assert.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "greater than zero"))]
fn with_cursor_panics_when_default_limit_is_not_positive() {
    let _unused = CommandSpec::new("list", "List things").with_cursor(CursorConfig {
        default_limit: 0,
        max_limit: 5,
    });
}

#[test]
#[cfg_attr(
    debug_assertions,
    should_panic(expected = "greater than its max_limit")
)]
fn with_cursor_panics_when_default_limit_exceeds_max_limit() {
    let _unused = CommandSpec::new("list", "List things").with_cursor(CursorConfig {
        default_limit: 10,
        max_limit: 5,
    });
}

/// A command picks one pagination style, not both; caught at registration
/// time (inside `Cli::add_command`'s clap-tree build), not left as a silent
/// "cursor wins" or "offset wins" resolution.
#[test]
#[cfg_attr(
    debug_assertions,
    should_panic(expected = "picks one pagination style")
)]
fn with_pagination_and_with_cursor_together_panics_on_registration() {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("bad", "Bad")
            .no_auth(true)
            .with_pagination(cli_engine::PaginationConfig::default())
            .with_cursor(CursorConfig {
                default_limit: 1,
                max_limit: 0,
            }),
        async |_credential, _args| Ok(CommandResult::new(json!([]))),
    ));
}

/// Same footgun as `raw_output_paired_with_pagination_panics_on_registration`
/// in `tests/foundation.rs`, for the cursor flavor: a single verbatim string
/// has no pages either.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "mutually exclusive"))]
fn raw_output_paired_with_cursor_panics_on_registration() {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("bad", "Bad")
            .no_auth(true)
            .raw_output(true)
            .with_cursor(CursorConfig {
                default_limit: 1,
                max_limit: 0,
            }),
        async |_credential, _args| Ok(CommandResult::new(json!("text"))),
    ));
}

#[tokio::test]
async fn next_page_action_replays_other_flags_the_user_passed() {
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_arg(Arg::new("status").long("status"))
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
    );

    let output = cli
        .run(["my-cli", "list", "--status", "active", "--output", "json"])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "my-cli list --status active --limit 2 --continue 2"
    );
}

#[tokio::test]
async fn next_page_action_quotes_a_continuation_token_with_shell_metacharacters() {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
        async |_credential, _args| {
            Ok(CommandResult::new(json!(items())).with_cursor(CursorContinuation::more("a b;c")))
        },
    ));

    let output = cli.run(["my-cli", "list", "--output", "json"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let rendered: serde_json::Value = serde_json::from_str(&output.rendered).expect("valid json");
    assert_eq!(
        rendered["next_actions"][0]["command"],
        "my-cli list --limit 2 --continue \"a b;c\""
    );
}

#[tokio::test]
async fn human_output_shows_so_far_summary_when_total_is_unknown() {
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
    );

    let output = cli.run(["my-cli", "list", "--output", "human"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    assert!(
        output
            .rendered
            .contains("(2 rows so far; use --continue 2 for more)"),
        "{}",
        output.rendered
    );
    assert!(
        output.rendered.contains("Next steps:"),
        "{}",
        output.rendered
    );
    assert!(
        output
            .rendered
            .contains("my-cli list --limit 2 --continue 2"),
        "{}",
        output.rendered
    );
}

#[tokio::test]
async fn human_output_shows_total_when_known() {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
        async |_credential, _args| {
            Ok(
                CommandResult::new(json!([{"name": "alpha"}, {"name": "beta"}]))
                    .with_cursor(CursorContinuation::more("2").with_total(4)),
            )
        },
    ));

    let output = cli.run(["my-cli", "list", "--output", "human"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    assert!(
        output.rendered.contains("(2 of 4 rows)"),
        "{}",
        output.rendered
    );
}

#[tokio::test]
async fn human_output_on_last_page_shows_summary_but_no_next_steps() {
    let cli = cli_with_cursor_list_command(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
    );

    let output = cli
        .run([
            "my-cli",
            "list",
            "--continue",
            "2",
            "--limit",
            "2",
            "--output",
            "human",
        ])
        .await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    assert!(output.rendered.contains("(2 rows)"), "{}", output.rendered);
    assert!(
        !output.rendered.contains("Next steps:"),
        "no next page exists: {}",
        output.rendered
    );
}

#[tokio::test]
async fn human_standalone_summary_for_a_non_table_cursor_response() {
    // Mirrors `tests/pagination.rs`'s `human_standalone_summary_...` for the
    // cursor flavor: a bare array of scalars renders via `render_array_lines`,
    // not `render_table`, so the standalone `append_cursor_summary` line is
    // the one that must fire, not the merged table footer.
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("list", "List things")
            .no_auth(true)
            .with_cursor(CursorConfig {
                default_limit: 2,
                max_limit: 0,
            }),
        async |_credential, _args| {
            Ok(CommandResult::new(json!(["alpha", "beta"]))
                .with_cursor(CursorContinuation::more("2")))
        },
    ));

    let output = cli.run(["my-cli", "list", "--output", "human"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    assert!(
        output
            .rendered
            .contains("Showing 2 items so far; use --continue 2 for more"),
        "{}",
        output.rendered
    );
}
