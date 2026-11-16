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
    // A list with items is a value even when the items render as blanks.
    for value in [
        json!(0),
        json!(false),
        json!(["x"]),
        json!([null]),
        json!([""]),
        json!([[]]),
        json!([null, null]),
    ] {
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

#[test]
fn a_nested_table_inside_an_object_is_trimmed_too() {
    use crate::output::human::body::render_object_with_columns;
    let map = json!({
        "name": "zone",
        "records": [{ "type": "A", "name": "www" }]
    });
    let columns = vec![
        TableColumn::new("name", "Name"),
        TableColumn::new("records", "Records").nested(vec![
            TableColumn::new("type", "Type"),
            TableColumn::new("name", "Name"),
            TableColumn::new("port", "Port").hide_when_empty(true),
        ]),
    ];

    let (out, _) = render_object_with_columns(map.as_object().expect("object"), &columns, 80);

    assert!(out.contains("TYPE"), "{out}");
    assert!(
        !out.contains("PORT"),
        "nested empty column is omitted: {out}"
    );
}

fn object_columns() -> Vec<TableColumn> {
    vec![
        TableColumn::new("name", "Name"),
        TableColumn::new("records", "Records")
            .hide_when_empty(true)
            .nested(vec![
                TableColumn::new("type", "Type"),
                TableColumn::new("data", "Data"),
            ]),
        TableColumn::new("plan", "Plan")
            .hide_when_empty(true)
            .nested(vec![TableColumn::new("action", "Action")]),
    ]
}

fn render_object(map: &serde_json::Value, cols: &[TableColumn], explicit: bool) -> String {
    let envelope = Envelope::success(map.clone(), "");
    let columns = Some(cols);
    // `fields_explicit` only matters when the user typed `--fields`.
    crate::output::human::body::render_data_body(
        envelope.data.as_ref().expect("data"),
        columns,
        "",
        80,
        None,
        explicit,
        None,
    )
    .0
}

#[test]
fn an_object_view_leaves_out_a_flagged_nested_column_it_has_no_value_for() {
    let result = json!({
        "name": "set",
        "records": [{ "type": "A", "data": "1.2.3.4" }]
    });

    let out = render_object(&result, &object_columns(), false);

    assert!(out.contains("Records:"), "{out}");
    assert!(out.contains("1.2.3.4"), "{out}");
    assert!(
        !out.contains("Plan"),
        "absent flagged column must not print: {out}"
    );
}

#[test]
fn an_object_view_still_shows_a_flagged_column_that_has_a_value() {
    let result = json!({
        "name": "dry-run",
        "plan": [{ "action": "unchanged" }]
    });

    let out = render_object(&result, &object_columns(), false);

    assert!(out.contains("Plan:") && out.contains("unchanged"), "{out}");
    assert!(!out.contains("Records"), "{out}");
}

#[test]
fn null_empty_string_and_empty_list_leave_a_flagged_object_column_out() {
    for empty in [json!(null), json!(""), json!([])] {
        let result = json!({ "name": "x", "records": empty });
        let out = render_object(&result, &object_columns(), false);
        assert!(!out.contains("Records"), "{empty} is no value: {out}");
    }
}

#[test]
fn zero_and_false_keep_a_flagged_object_column() {
    let cols = vec![
        TableColumn::new("name", "Name"),
        TableColumn::new("deleted", "Deleted").hide_when_empty(true),
        TableColumn::new("dry", "Dry").hide_when_empty(true),
    ];
    let result = json!({ "name": "x", "deleted": 0, "dry": false });

    let out = render_object(&result, &cols, false);

    assert!(out.contains("Deleted: 0"), "{out}");
    assert!(out.contains("Dry: no"), "{out}");
}

#[test]
fn an_unflagged_absent_object_column_still_prints_its_blank_line() {
    let cols = vec![
        TableColumn::new("name", "Name"),
        TableColumn::new("note", "Note"),
    ];
    let result = json!({ "name": "x" });

    let out = render_object(&result, &cols, false);

    assert!(
        out.contains("Note: "),
        "only flagged columns are trimmed: {out}"
    );
}

#[test]
fn an_explicit_fields_selection_keeps_an_absent_flagged_object_column() {
    let result = json!({ "name": "set" });

    let out = render_object(&result, &object_columns(), true);

    assert!(out.contains("Records:") && out.contains("Plan:"), "{out}");
}

#[test]
fn an_object_whose_every_column_is_flagged_and_empty_keeps_them() {
    let cols = vec![
        TableColumn::new("a", "A").hide_when_empty(true),
        TableColumn::new("b", "B").hide_when_empty(true),
    ];
    let result = json!({ "other": 1 });

    let out = render_object(&result, &cols, false);

    assert!(out.contains("A:") && out.contains("B:"), "{out}");
}

#[test]
fn a_nested_object_block_trims_its_own_flagged_columns() {
    let cols = vec![
        TableColumn::new("name", "Name"),
        TableColumn::new("detail", "Detail").nested(vec![
            TableColumn::new("kind", "Kind"),
            TableColumn::new("port", "Port").hide_when_empty(true),
        ]),
    ];
    let result = json!({ "name": "x", "detail": { "kind": "srv" } });

    let out = render_object(&result, &cols, false);

    assert!(out.contains("Kind: srv"), "{out}");
    assert!(!out.contains("Port"), "{out}");
}

#[test]
fn the_object_renderer_reports_no_hidden_columns_for_an_omitted_one() {
    use crate::output::human::body::render_data_body;
    let result = json!({ "name": "set" });

    let (_, notes) = render_data_body(&result, Some(&object_columns()), "", 80, None, false, None);

    assert!(
        notes.hidden_columns.is_empty(),
        "{:?}",
        notes.hidden_columns
    );
}
