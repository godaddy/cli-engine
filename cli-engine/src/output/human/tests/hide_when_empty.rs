use serde_json::json;

use crate::output::Envelope;
use crate::output::human::body::render_array_with_columns;
use crate::output::human::columns::column_has_value;
use crate::output::human::{TableColumn, render_human_with_view};

fn columns() -> Vec<TableColumn> {
    vec![
        TableColumn::new("type", "Type").essential(true),
        TableColumn::new("name", "Name").essential(true),
        TableColumn::new("port", "Port").hide_when_empty(true),
        TableColumn::new("service", "Service").hide_when_empty(true),
    ]
}

fn render(items: &[serde_json::Value], cols: &[TableColumn], explicit: bool) -> String {
    render_array_with_columns(items, cols, 80, None, explicit, None).0
}

#[test]
fn a_flagged_column_with_no_values_is_left_out() {
    let items = vec![
        json!({ "type": "A", "name": "www" }),
        json!({ "type": "TXT", "name": "@", "port": null, "service": "" }),
    ];

    let out = render(&items, &columns(), false);

    assert!(out.contains("TYPE") && out.contains("NAME"), "{out}");
    assert!(!out.contains("PORT"), "empty column must be omitted: {out}");
    assert!(
        !out.contains("SERVICE"),
        "empty column must be omitted: {out}"
    );
}

#[test]
fn a_flagged_column_is_shown_when_any_row_has_a_value() {
    let items = vec![
        json!({ "type": "A", "name": "www" }),
        json!({ "type": "SRV", "name": "chat", "port": 5222, "service": "_xmpp" }),
    ];

    let out = render(&items, &columns(), false);

    assert!(out.contains("PORT") && out.contains("SERVICE"), "{out}");
    assert!(out.contains("5222") && out.contains("_xmpp"), "{out}");
    // The A row stays in the table, with blanks under the extra columns.
    let a_row = out.lines().find(|l| l.contains("www")).expect("A row");
    assert!(!a_row.contains("5222"), "{a_row}");
}

#[test]
fn zero_false_and_non_empty_lists_count_as_values() {
    for value in [json!(0), json!(false), json!(["x"])] {
        let items = vec![json!({ "type": "A", "name": "n", "port": value })];
        let out = render(&items, &columns(), false);
        assert!(out.contains("PORT"), "{value} is a value: {out}");
    }
    for empty in [json!(null), json!(""), json!([])] {
        let items = vec![json!({ "type": "A", "name": "n", "port": empty })];
        let out = render(&items, &columns(), false);
        assert!(!out.contains("PORT"), "{empty} is not a value: {out}");
    }
}

#[test]
fn an_explicit_fields_selection_keeps_an_empty_column() {
    let items = vec![json!({ "type": "A", "name": "www" })];

    let out = render(&items, &columns(), true);

    assert!(out.contains("PORT") && out.contains("SERVICE"), "{out}");
}

#[test]
fn an_unflagged_empty_column_is_still_shown() {
    let items = vec![json!({ "type": "A", "name": "www" })];
    let cols = vec![
        TableColumn::new("type", "Type"),
        TableColumn::new("note", "Note"),
    ];

    let out = render(&items, &cols, false);

    assert!(
        out.contains("NOTE"),
        "only flagged columns are trimmed: {out}"
    );
}

#[test]
fn a_view_that_would_lose_every_column_keeps_them() {
    let items = vec![json!({ "other": 1 })];
    let cols = vec![
        TableColumn::new("port", "Port").hide_when_empty(true),
        TableColumn::new("service", "Service").hide_when_empty(true),
    ];

    let out = render(&items, &cols, false);

    assert!(out.contains("PORT") && out.contains("SERVICE"), "{out}");
}

#[test]
fn an_omitted_empty_column_is_not_reported_as_hidden_for_width() {
    let items = vec![json!({ "type": "A", "name": "www" })];

    let (out, notes) = render_array_with_columns(&items, &columns(), 80, None, false, None);

    assert!(
        notes.hidden_columns.is_empty(),
        "{:?}",
        notes.hidden_columns
    );
    assert!(!out.contains("hidden"), "{out}");
}

#[test]
fn a_populated_column_still_yields_to_width_like_any_other() {
    let items = vec![json!({
        "type": "SRV", "name": "chat", "port": 5222, "service": "_xmpp"
    })];

    // Too narrow for every column: the trailing flagged column is hidden for
    // width, and the footer names it, exactly as for an unflagged column.
    let (_, notes) = render_array_with_columns(&items, &columns(), 22, None, false, None);

    assert!(
        notes.hidden_columns.contains(&"Service".to_owned()),
        "{:?}",
        notes.hidden_columns
    );
}

#[test]
fn the_flag_applies_through_a_registered_view_end_to_end() {
    let envelope = Envelope::success(vec![json!({ "type": "A", "name": "www" })], "");

    let out = render_human_with_view(&envelope, Some(&columns()), "", false);

    assert!(!out.contains("PORT"), "{out}");
}

#[test]
fn column_has_value_ignores_missing_null_and_blank() {
    let items = vec![
        json!({ "a": null }),
        json!({ "a": "" }),
        json!({ "a": [] }),
        json!({}),
    ];
    assert!(!column_has_value(&items, "a"));
    let items = vec![json!({ "a": null }), json!({ "a": "x" })];
    assert!(column_has_value(&items, "a"));
}
