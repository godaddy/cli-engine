use serde_json::json;

use crate::output::Envelope;
use crate::output::human::columns::dynamic_columns;
use crate::output::human::{
    HumanViewDef, HumanViewRegistry, TableColumn, render_human_with_registry_selected,
    select_columns,
};

#[test]
fn select_columns_orders_by_requested_fields_not_declared_order() {
    let columns = vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("name", "Name"),
        TableColumn::new("status", "Status"),
    ];

    let selected = select_columns(&columns, "status,id");

    assert_eq!(
        selected
            .iter()
            .map(|c| c.field.as_str())
            .collect::<Vec<_>>(),
        vec!["status", "id"],
        "order should follow the requested fields, not declaration order"
    );
}

#[test]
fn select_columns_dedupes_and_skips_unknown_fields() {
    let columns = vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("name", "Name"),
        TableColumn::new("status", "Status"),
    ];

    let selected = select_columns(&columns, "status,bogus,status,id");

    assert_eq!(
        selected
            .iter()
            .map(|c| c.field.as_str())
            .collect::<Vec<_>>(),
        vec!["status", "id"],
        "duplicates collapse to first occurrence; unknown fields are dropped"
    );
}

#[test]
fn dynamic_columns_orders_by_requested_fields() {
    let columns = dynamic_columns("price1Year,domain", || {
        vec![
            "domain".to_owned(),
            "currency".to_owned(),
            "price1Year".to_owned(),
        ]
    });

    assert_eq!(
        columns.iter().map(|c| c.field.as_str()).collect::<Vec<_>>(),
        vec!["price1Year", "domain"]
    );
}

#[test]
fn dynamic_columns_falls_back_to_alphabetical_without_fields() {
    let columns = dynamic_columns("", || vec!["currency".to_owned(), "domain".to_owned()]);

    assert_eq!(
        columns.iter().map(|c| c.field.as_str()).collect::<Vec<_>>(),
        vec!["currency", "domain"],
        "no fields signal at all: alphabetical is the only order available"
    );
}

#[test]
fn no_view_array_rendering_follows_requested_field_order() {
    // Reproduces the real-world `domain suggest` symptom: a command with
    // no registered view whose default_fields lists `domain` first must
    // not silently reorder it after `currency` just because "c" < "d".
    let envelope = Envelope::success(
        json!([{ "domain": "example.com", "currency": "USD", "price1Year": "12.99" }]),
        "domain:suggest",
    );
    let registry = HumanViewRegistry::new();

    let rendered = render_human_with_registry_selected(
        &envelope,
        &registry,
        "domain:suggest",
        "domain,price1Year,currency",
        true,
    );

    let header_line = rendered.lines().next().expect("header line");
    assert!(header_line.contains("DOMAIN"), "{rendered}");
    let domain_pos = header_line.find("DOMAIN").expect("domain header");
    let price_pos = header_line.find("PRICE1YEAR").expect("price1Year header");
    let currency_pos = header_line.find("CURRENCY").expect("currency header");
    assert!(
        domain_pos < price_pos && price_pos < currency_pos,
        "expected DOMAIN, PRICE1YEAR, CURRENCY in that order: {header_line}"
    );
}

#[test]
fn registered_view_rendering_follows_requested_field_order() {
    let mut registry = HumanViewRegistry::new();
    registry.register(HumanViewDef::new(
        "things",
        vec![
            TableColumn::new("id", "ID"),
            TableColumn::new("name", "Name"),
            TableColumn::new("status", "Status"),
        ],
    ));
    let envelope = Envelope::success(
        json!([{ "id": "1", "name": "acme", "status": "active" }]),
        "things",
    );

    let rendered =
        render_human_with_registry_selected(&envelope, &registry, "things", "status,id", true);

    let header_line = rendered.lines().next().expect("header line");
    assert!(!header_line.contains("NAME"), "{rendered}");
    let status_pos = header_line.find("STATUS").expect("status header");
    let id_pos = header_line.find("ID").expect("id header");
    assert!(
        status_pos < id_pos,
        "expected STATUS before ID per the requested field order: {header_line}"
    );
}
