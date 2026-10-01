use serde_json::{Value, json};

use crate::output::human::render_human;
use crate::output::human::value_format::format_plain_value;
use crate::output::{Envelope, NextAction, NextActionParam};

#[test]
fn format_plain_value_round_trips_a_bare_string_verbatim() {
    // No quoting/escaping — the exact convention `raw_output` bypass
    // relies on to render a `CommandResult` string byte-for-byte.
    assert_eq!(
        format_plain_value(&Value::String("some\nverbatim\ntext".to_owned())),
        "some\nverbatim\ntext"
    );
}

#[test]
fn human_output_appends_next_steps_footer() {
    let envelope = Envelope::success(json!({ "domain": "example.com" }), "domain")
        .with_next_actions(vec![NextAction::new(
            "domain purchase --quote-token <token> --agree --confirm",
            "Register at the quoted price",
        )]);
    let out = render_human(&envelope);
    // Data still renders as before…
    assert!(out.contains("domain: example.com"), "{out}");
    // …followed by a Next steps footer with the command and its description.
    assert!(out.contains("\nNext steps:\n"), "{out}");
    assert!(
        out.contains("domain purchase --quote-token <token> --agree --confirm"),
        "{out}"
    );
    assert!(out.contains("Register at the quoted price"), "{out}");
}

#[test]
fn human_output_substitutes_known_next_action_params() {
    let envelope = Envelope::success(json!({ "domain": "example.com" }), "domain")
        .with_next_actions(vec![
            NextAction::new(
                "domain purchase --quote-token <quote-token> --agree --confirm",
                "Register at the quoted price",
            )
            .with_param("quote-token", NextActionParam::value("abc-123")),
        ]);
    let out = render_human(&envelope);
    assert!(
        out.contains("domain purchase --quote-token abc-123 --agree --confirm"),
        "{out}"
    );
    assert!(!out.contains("<quote-token>"), "{out}");
}

#[test]
fn human_output_leaves_placeholder_without_a_known_value() {
    let envelope = Envelope::success(json!({ "domain": "example.com" }), "domain")
        .with_next_actions(vec![
            NextAction::new("domain quote <domain>", "Price a registration")
                .with_param("domain", NextActionParam::required()),
        ]);
    let out = render_human(&envelope);
    assert!(out.contains("domain quote <domain>"), "{out}");
}

#[test]
fn human_output_has_no_footer_without_next_actions() {
    let envelope = Envelope::success(json!({ "domain": "example.com" }), "domain");
    let out = render_human(&envelope);
    assert!(out.contains("domain: example.com"), "{out}");
    assert!(
        !out.contains("Next steps"),
        "no footer when there are no actions: {out}"
    );
}

#[test]
fn error_output_has_no_next_steps_footer() {
    // An error envelope carries no next_actions and must render only the error.
    let envelope = Envelope::error("ERROR", "boom", "domain");
    let out = render_human(&envelope);
    assert!(out.starts_with("Error:"), "{out}");
    assert!(!out.contains("Next steps"), "{out}");
    assert!(!out.contains("Fix:"), "{out}");
}

#[test]
fn error_output_appends_fix_line() {
    let envelope =
        Envelope::error("AUTH_REQUIRED", "not logged in", "auth").with_fix("Run auth login");
    let out = render_human(&envelope);
    assert_eq!(out, "Error: not logged in\nFix: Run auth login\n");
}
