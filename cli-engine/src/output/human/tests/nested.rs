use serde_json::json;

use crate::output::Envelope;
use crate::output::human::body::render_object_with_columns;
use crate::output::human::value_format::format_value;
use crate::output::human::{TableColumn, render_human_with_view};

#[test]
fn nested_array_of_objects_renders_as_indented_child_table() {
    let map = json!({
        "name": "getPets",
        "parameters": {
            "items": [
                {"name": "limit", "in": "query"},
                {"name": "id", "in": "path"},
            ],
        },
    });
    let columns = vec![
        TableColumn::new("name", "Name"),
        TableColumn::new("parameters.items", "Parameters").nested(vec![
            TableColumn::new("name", "Name"),
            TableColumn::new("in", "In"),
        ]),
    ];

    let (out, notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 80);

    assert!(out.starts_with("Name: getPets\nParameters:\n"), "{out}");
    assert!(
        out.contains("  NAME"),
        "child header must be indented: {out}"
    );
    assert!(out.contains("  limit"), "child row must be indented: {out}");
    assert!(
        !out.contains('{'),
        "no raw JSON should leak into output: {out}"
    );
    assert!(!notes.truncated, "{out}");
    assert!(
        out.contains("(2 rows)"),
        "no pagination sibling means the plain row-count footer, unchanged: {out}"
    );
}

#[test]
fn nested_array_with_pagination_sibling_renders_pagination_style_footer() {
    let map = json!({
        "name": "getPets",
        "parameters": {
            "items": [
                {"name": "limit", "in": "query"},
                {"name": "id", "in": "path"},
            ],
            "pagination": { "total": 26, "offset": 0, "limit": 2, "count": 2, "has_more": true },
        },
    });
    let columns = vec![
        TableColumn::new("name", "Name"),
        TableColumn::new("parameters.items", "Parameters").nested(vec![
            TableColumn::new("name", "Name"),
            TableColumn::new("in", "In"),
        ]),
    ];

    let (out, _notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 80);

    assert!(
        out.contains("(2 of 26 rows, offset 0, limit 2)"),
        "nested table should reuse the pagination sibling's PaginationMeta facts: {out}"
    );
}

#[test]
fn nested_array_without_pagination_sibling_keeps_the_plain_row_count_footer() {
    let map = json!({ "items": [{"name": "limit"}] });
    let columns =
        vec![TableColumn::new("items", "Items").nested(vec![TableColumn::new("name", "Name")])];

    let (out, _notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 80);

    assert!(
        out.contains("(1 rows)"),
        "no pagination sibling means no opt-in — behavior is unchanged: {out}"
    );
}

#[test]
fn nested_array_with_malformed_pagination_sibling_keeps_the_plain_row_count_footer() {
    let map = json!({ "items": [{"name": "limit"}], "pagination": { "total": "not-a-number" } });
    let columns =
        vec![TableColumn::new("items", "Items").nested(vec![TableColumn::new("name", "Name")])];

    let (out, _notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 80);

    assert!(
        out.contains("(1 rows)"),
        "a pagination sibling that fails to deserialize degrades to the plain footer: {out}"
    );
}

#[test]
fn nested_child_table_narrows_and_reports_via_merged_render_notes() {
    let map = json!({
        "items": [
            {"a": "x".repeat(5), "b": "x".repeat(5), "c": "x".repeat(5)},
        ],
    });
    let columns = vec![TableColumn::new("items", "Items").nested(vec![
        TableColumn::new("a", "A"),
        TableColumn::new("b", "B"),
        TableColumn::new("c", "C"),
    ])];

    // Narrow enough to force the child table's own hide-before-truncate
    // cascade (mirrors `narrow_terminal_hides_columns_before_truncating_any_of_the_survivors`).
    let (out, notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 12);

    assert_eq!(
        notes.hidden_columns,
        vec!["Items > B".to_owned(), "Items > C".to_owned()],
        "hidden columns bubble up prefixed with the parent header: {out}"
    );
    assert!(
        notes.nested_narrowing,
        "narrowing happened inside the nested child, not at this level's own columns: {out}"
    );
}

#[test]
fn footer_does_not_suggest_fields_for_narrowing_inside_a_nested_column() {
    // `--fields` only selects among top-level declared columns — it
    // cannot narrow what shows *inside* a `TableColumn::nested` column.
    // When a nested child's own columns get hidden, the footer must not
    // claim `--fields` fixes it.
    let envelope = Envelope::success(
        json!({
            "items": [{
                "id": "1",
                "name": "acme",
                "status": "active",
                "region": "us-west",
                "created_at": "2026-01-01",
                "updated_at": "2026-01-02",
                "notes": "irrelevant, lowest priority",
            }],
        }),
        "thing",
    );
    let columns = vec![TableColumn::new("items", "Items").nested(vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("name", "Name"),
        TableColumn::new("status", "Status"),
        TableColumn::new("region", "Region"),
        TableColumn::new("created_at", "Created At"),
        TableColumn::new("updated_at", "Updated At"),
        TableColumn::new("notes", "This Is An Extremely Long Trailing Column Header"),
    ])];

    let out = render_human_with_view(&envelope, Some(&columns), "", false);

    assert!(out.contains("hidden to fit the display width"), "{out}");
    assert!(
        out.contains("Items > This Is An Extremely Long Trailing Column Header"),
        "{out}"
    );
    assert!(
        !out.contains("use --fields"),
        "must not suggest --fields as a fix when the narrowing is inside a nested column \
         (mentioning it to explain why it won't help is fine): {out}"
    );
    assert!(
        out.contains("--json"),
        "must still point at --json as the real remedy: {out}"
    );
}

#[test]
fn empty_nested_array_renders_no_results_indented() {
    let map = json!({ "items": [] });
    let columns = vec![
        TableColumn::new("items", "Parameters").nested(vec![TableColumn::new("name", "Name")]),
    ];

    let (out, _notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 80);

    assert_eq!(out, "Parameters:\n  (no results)\n");
}

#[test]
fn nested_object_field_renders_as_indented_property_bag() {
    let map = json!({ "owner": {"name": "Ada", "email": "ada@example.test"} });
    let columns = vec![TableColumn::new("owner", "Owner").nested(vec![
        TableColumn::new("name", "Name"),
        TableColumn::new("email", "Email"),
    ])];

    let (out, _notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 80);

    assert_eq!(out, "Owner:\n  Name: Ada\n  Email: ada@example.test\n");
}

#[test]
fn unopted_in_nested_value_still_renders_as_raw_json_line() {
    // A column with no `.nested(...)` is a strict no-op even when the
    // runtime value happens to be list/object shaped — locks in the
    // "opt-in, never automatic" guarantee.
    let map = json!({
        "parameters": {"items": [{"name": "limit"}], "total": 1},
    });
    let columns = vec![TableColumn::new("parameters", "Parameters")];

    let (out, _notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 80);

    assert_eq!(
        out,
        format!(
            "Parameters: {}\n",
            format_value(map.get("parameters").expect("parameters"))
        )
    );
    assert!(out.contains('{'), "unchanged raw-JSON fallback: {out}");
}

#[test]
fn nested_column_is_a_no_op_when_the_value_is_not_actually_nestable() {
    // A column can opt into `.nested(...)` while still receiving a
    // scalar or a mixed (non-uniform) array at runtime. Rendering must stay the
    // same flat `header: value` line a column with `nested: None` would have
    // produced.
    let map = json!({
        "scalar": "just a string",
        "mixed": ["a", {"b": 1}],
    });
    let nested_columns = vec![TableColumn::new("x", "X")];
    let columns = vec![
        TableColumn::new("scalar", "Scalar").nested(nested_columns.clone()),
        TableColumn::new("mixed", "Mixed").nested(nested_columns),
    ];
    let unnested_columns = vec![
        TableColumn::new("scalar", "Scalar"),
        TableColumn::new("mixed", "Mixed"),
    ];

    let (nested_out, _) =
        render_object_with_columns(map.as_object().expect("object fixture"), &columns, 80);
    let (unnested_out, _) = render_object_with_columns(
        map.as_object().expect("object fixture"),
        &unnested_columns,
        80,
    );

    assert_eq!(
        nested_out, unnested_out,
        "an opted-in column must render identically to an unopted-in one \
         when the runtime value isn't list-of-objects or object shaped"
    );
    assert_eq!(nested_out, "Scalar: just a string\nMixed: a, {\"b\":1}\n");
}
