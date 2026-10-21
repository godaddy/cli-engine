//! End-to-end coverage for `CommandResult::with_warning`, driven through
//! `Cli::run` the way a real consumer binary would.

use cli_engine::{Cli, CliConfig, CommandResult, CommandSpec, RuntimeCommandSpec};
use serde_json::{Value, json};

fn cli() -> Cli {
    let mut cli = Cli::new(CliConfig::new("my-cli", "Dev tooling", "my-cli"));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("warn", "Succeeds with warnings").no_auth(true),
        async |_credential, _args| {
            Ok(CommandResult::new(json!({"id": 1}))
                .with_warning("first")
                .with_warning("second"))
        },
    ));
    cli.add_command(RuntimeCommandSpec::new(
        CommandSpec::new("clean", "Succeeds cleanly").no_auth(true),
        async |_credential, _args| Ok(CommandResult::new(json!({"id": 1}))),
    ));
    cli
}

#[tokio::test]
async fn warnings_appear_in_json_envelope_in_order() {
    let output = cli().run(["my-cli", "warn", "--output", "json"]).await;
    assert_eq!(output.exit_code, 0, "{}", output.rendered);
    let envelope: Value = serde_json::from_str(&output.rendered).expect("json envelope");
    assert_eq!(envelope["warnings"], json!(["first", "second"]));
    assert_eq!(envelope["data"], json!({"id": 1}));
}

#[tokio::test]
async fn warnings_appear_in_toon_and_human_output() {
    let toon = cli().run(["my-cli", "warn", "--output", "toon"]).await;
    assert!(toon.rendered.contains("warnings"), "{}", toon.rendered);
    assert!(toon.rendered.contains("second"), "{}", toon.rendered);

    let human = cli().run(["my-cli", "warn", "--output", "human"]).await;
    assert!(
        human.rendered.contains("Warnings:\n  first\n  second\n"),
        "{}",
        human.rendered
    );
}

#[tokio::test]
async fn warnings_field_is_absent_without_warnings() {
    let output = cli().run(["my-cli", "clean", "--output", "json"]).await;
    let envelope: Value = serde_json::from_str(&output.rendered).expect("json envelope");
    assert!(envelope.get("warnings").is_none(), "{}", output.rendered);
}
