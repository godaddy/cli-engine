use serde_json::json;

use crate::output::human::{
    HumanViewDef, HumanViewRegistry, TableColumn, render_human_with_registry_selected,
};
use crate::output::{Envelope, NextAction, NextActionParam};

// These test the registry's own dispatch contract (custom-renderer-vs-columns
// precedence, next-steps footer wrapping) rather than anything a command
// module configures per-command, so they stay as crate-internal unit tests
// against the pub(crate) renderers instead of `preview_human_view` — that
// function is scoped to the common "render my TableColumn view" case.

#[test]
fn custom_human_output_appends_next_steps_footer() {
    let mut registry = HumanViewRegistry::new();
    registry.register_func("shopping-cart", |_| "Cart: ready\n".to_owned());
    let envelope =
        Envelope::success(json!({ "id": "cart-1" }), "shopping-cart").with_next_actions(vec![
            NextAction::new(
                "shopping checkout complete <cart-id> --agree",
                "Place the order",
            )
            .with_param("cart-id", NextActionParam::value("cart-1")),
        ]);

    let out = render_human_with_registry_selected(&envelope, &registry, "shopping-cart", "", false);

    assert!(out.starts_with("Cart: ready\n"), "{out}");
    assert!(out.contains("\nNext steps:\n"), "{out}");
    assert!(
        out.contains("shopping checkout complete cart-1 --agree"),
        "{out}"
    );
    assert!(out.contains("Place the order"), "{out}");
}

#[test]
fn human_view_registry_custom_renderer_wins_over_columns() {
    let mut registry = HumanViewRegistry::new();
    registry.register(HumanViewDef {
        schema_id: "things".to_owned(),
        columns: vec![TableColumn::new("name", "Name")],
    });
    registry.register_func("things", |data| {
        format!(
            "custom:{}\n",
            data.get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        )
    });
    let envelope = Envelope::success(json!({"name": "alpha"}), "things");

    let rendered = render_human_with_registry_selected(&envelope, &registry, "things", "", false);

    assert_eq!(rendered, "custom:alpha\n");
}
