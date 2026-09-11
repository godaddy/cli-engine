use cli_engine::{
    Envelope, HumanViewRegistry, NextAction, NextActionParam, render_human_with_registry_selected,
};
use serde_json::json;

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

    let out = render_human_with_registry_selected(&envelope, &registry, "shopping-cart", "");

    assert!(out.starts_with("Cart: ready\n"), "{out}");
    assert!(out.contains("\nNext steps:\n"), "{out}");
    assert!(
        out.contains("shopping checkout complete cart-1 --agree"),
        "{out}"
    );
    assert!(out.contains("Place the order"), "{out}");
}
