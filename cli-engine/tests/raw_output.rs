//! End-to-end coverage for `CommandSpec::raw_output`: a successful string
//! result must print verbatim regardless of `--output`/`--json`/`--human`/
//! `--toon`, the TTY/env/config default, or the `--fields`/`--filter`/
//! `--expr` pipeline — and those now-meaningless flags are hidden from the
//! command's own `--help` (but still parse harmlessly if passed anyway).

use cli_engine::{Cli, CliConfig, CommandResult, CommandSpec, RuntimeCommandSpec};
use serde_json::json;

const VERBATIM: &str = "some\nverbatim\ntext";

fn build_cli() -> Cli {
    let mut cli = Cli::new(CliConfig {
        name: "my-cli".to_owned(),
        short: "Raw output test CLI".to_owned(),
        app_id: "my-cli".to_owned(),
        ..CliConfig::default()
    });
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("dump", "Print verbatim text")
            .no_auth(true)
            .raw_output(true),
        async |_credential, _args| Ok(CommandResult::new(json!(VERBATIM))),
    ));
    cli.add_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("dump-preview", "Print verbatim text, previewably")
            .no_auth(true)
            .mutates(true)
            .handles_dry_run(true)
            .raw_output(true),
        async |ctx| {
            if ctx.dry_run() {
                Ok(CommandResult::new(json!({"would": "print", "text": VERBATIM})).with_dry_run())
            } else {
                Ok(CommandResult::new(json!(VERBATIM)))
            }
        },
    ));
    cli
}

fn expected() -> String {
    format!("{VERBATIM}\n")
}

#[tokio::test]
async fn raw_output_ignores_output_json_flag() {
    let out = build_cli()
        .run(["my-cli", "dump", "--output", "json"])
        .await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_eq!(out.rendered, expected());
}

#[tokio::test]
async fn raw_output_ignores_output_toon_flag() {
    let out = build_cli()
        .run(["my-cli", "dump", "--output", "toon"])
        .await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_eq!(out.rendered, expected());
}

#[tokio::test]
async fn raw_output_ignores_human_flag() {
    let out = build_cli().run(["my-cli", "dump", "--human"]).await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_eq!(out.rendered, expected());
}

#[tokio::test]
async fn raw_output_ignores_no_flag_default() {
    // No `--output`/`--json`/`--human`/`--toon` at all: TTY/env/config
    // resolution never gets a chance to matter.
    let out = build_cli().run(["my-cli", "dump"]).await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_eq!(out.rendered, expected());
}

#[tokio::test]
async fn raw_output_still_rejects_invalid_output_value() {
    let out = build_cli()
        .run(["my-cli", "dump", "--output", "garbage"])
        .await;
    assert_ne!(out.exit_code, 0, "{}", out.rendered);
}

#[tokio::test]
async fn raw_output_ignores_fields_filter_expr_flags() {
    let out = build_cli()
        .run([
            "my-cli", "dump", "--fields", "x", "--filter", "true", "--expr", "@",
        ])
        .await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_eq!(out.rendered, expected());
}

#[tokio::test]
async fn raw_output_hides_output_fields_filter_expr_from_help() {
    let help = build_cli().run(["my-cli", "dump", "--help"]).await;
    assert_eq!(help.exit_code, 0, "{}", help.rendered);
    assert!(!help.rendered.contains("--output"), "{}", help.rendered);
    assert!(!help.rendered.contains("--fields"), "{}", help.rendered);
    assert!(!help.rendered.contains("--filter"), "{}", help.rendered);
    assert!(!help.rendered.contains("--expr"), "{}", help.rendered);
}

#[tokio::test]
async fn raw_output_command_with_dry_run_preview_uses_normal_rendering() {
    // A `handles_dry_run` preview is diagnostic, not the command's real
    // output, so it renders through the normal envelope path even though
    // the command also opted into `raw_output`.
    let out = build_cli()
        .run(["my-cli", "dump-preview", "--dry-run", "--output", "json"])
        .await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_ne!(out.rendered, expected());
    let rendered: serde_json::Value = serde_json::from_str(&out.rendered).expect("valid json");
    assert_eq!(rendered["data"]["would"], "print");

    // A real (non-dry-run) invocation still renders raw.
    let out = build_cli().run(["my-cli", "dump-preview"]).await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_eq!(out.rendered, expected());
}
