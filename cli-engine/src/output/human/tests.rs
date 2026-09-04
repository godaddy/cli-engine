use serde_json::{Value, json};

use super::body::{
    NO_TRUNCATE_MAX_WIDTH, render_array, render_array_with_columns, render_object_with_columns,
};
use super::columns::{dynamic_columns, fit_column_widths};
use super::value_format::{
    format_value, resolve_field_parent, resolve_field_path, resolve_nested_pagination,
};
use super::{
    Alignment, HumanViewDef, HumanViewRegistry, TableColumn, render_human,
    render_human_with_registry_selected, render_human_with_view, select_columns,
};
use crate::output::{Envelope, NextAction, NextActionParam, PaginationMeta};

#[test]
fn format_plain_value_round_trips_a_bare_string_verbatim() {
    // No quoting/escaping — the exact convention `raw_output` bypass
    // relies on to render a `CommandResult` string byte-for-byte.
    assert_eq!(
        super::value_format::format_plain_value(&Value::String("some\nverbatim\ntext".to_owned())),
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

#[test]
fn no_truncate_column_keeps_long_values_intact() {
    let long_url = "https://example.com/legal/agreements/registration-agreement-v2";
    assert!(long_url.len() > 40, "fixture must exceed the default cap");
    let items = vec![json!({ "title": long_url, "url": long_url })];
    let columns = vec![
        // Declared first (higher priority) so it survives hide-before-
        // truncate rather than the lower-priority title column
        // absorbing truncation instead — with only two columns, any
        // truncation now cascades to hiding the lower-priority one.
        TableColumn::new("url", "URL").no_truncate(true),
        TableColumn::new("title", "Title"),
    ];

    let (out, notes) = render_array_with_columns(&items, &columns, 80, None);

    assert!(
        out.contains(long_url),
        "no_truncate column must keep the full value: {out}"
    );
    assert!(
        !out.contains("..."),
        "hiding the lower-priority column avoided any truncation: {out}"
    );
    assert_eq!(
        notes.hidden_columns,
        vec!["Title".to_owned()],
        "the lower-priority truncatable column is hidden rather than shown truncated: {out}"
    );
}

#[test]
fn no_truncate_column_still_caps_pathologically_long_values() {
    let huge_value = "x".repeat(NO_TRUNCATE_MAX_WIDTH * 2);
    let items = vec![json!({ "url": huge_value })];
    let columns = vec![TableColumn::new("url", "URL").no_truncate(true)];

    let (out, _notes) = render_array_with_columns(&items, &columns, 80, None);

    assert!(
        out.contains("..."),
        "values far beyond the no_truncate cap should still be truncated: {out}"
    );
    assert!(
        !out.contains(&huge_value),
        "the full pathological value should not be rendered verbatim: {out}"
    );
}

#[test]
fn right_aligned_column_pads_header_and_cells_on_the_left() {
    let items = vec![
        json!({ "period": "1 year", "price": "71.99" }),
        json!({ "period": "2 years", "price": "143.99" }),
    ];
    let columns = vec![
        TableColumn::new("period", "Period"),
        TableColumn::new("price", "Price").align(Alignment::Right),
    ];

    let (out, _notes) = render_array_with_columns(&items, &columns, 80, None);
    let mut lines = out.lines();
    let header_line = lines.next().expect("header line");
    let row_lines: Vec<&str> = lines.skip(1).take(2).collect();

    // "PRICE" (5 chars) right-aligned in a 6-wide column ("143.99")
    // leaves one leading space and no trailing space.
    assert!(header_line.ends_with(" PRICE"), "{header_line}");
    assert!(row_lines[0].ends_with(" 71.99"), "{}", row_lines[0]);
    assert!(row_lines[1].ends_with("143.99"), "{}", row_lines[1]);
    // The unaligned leading column is untouched (still left-aligned).
    assert!(header_line.starts_with("PERIOD "), "{header_line}");
}

#[test]
fn column_alignment_defaults_to_left() {
    let items = vec![json!({ "name": "a" }), json!({ "name": "bb" })];
    let columns = vec![TableColumn::new("name", "Name")];

    let (out, _notes) = render_array_with_columns(&items, &columns, 80, None);
    let mut lines = out.lines();
    let header_line = lines.next().expect("header line");

    assert!(
        header_line.starts_with("NAME"),
        "Alignment::Left is the default: {header_line}"
    );
}

#[test]
fn column_width_never_shrinks_below_a_long_header() {
    let long_header = "A Very Long Header That Exceeds The Default Width Cap";
    let items = vec![json!({ "field": "short" })];
    let columns = vec![TableColumn::new("field", long_header)];

    // Deliberately far narrower than the header: the header must still
    // render in full even though the row ends up wider than the terminal.
    let (out, _notes) = render_array_with_columns(&items, &columns, 10, None);
    let header_line = out.lines().next().expect("header line");
    let separator_line = out.lines().nth(1).expect("separator line");

    assert_eq!(
        header_line.len(),
        separator_line.len(),
        "header and separator must stay aligned even when the header alone exceeds the terminal: {out}"
    );
    assert!(
        header_line.len() >= long_header.len(),
        "header must not be cut short: {out}"
    );
}

#[test]
fn wide_terminal_shows_full_values_without_truncation() {
    let description = "a description that is well past the old forty-character cap";
    assert!(description.len() > 40, "fixture must exceed the old cap");
    let items = vec![json!({ "id": "1", "description": description })];
    let columns = vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("description", "Description"),
    ];

    let (out, notes) = render_array_with_columns(&items, &columns, 200, None);

    assert!(
        !notes.truncated,
        "plenty of room, nothing to shorten: {out}"
    );
    assert!(notes.hidden_columns.is_empty(), "{out}");
    assert!(out.contains(description), "{out}");
    assert!(!out.contains("..."), "{out}");
}

#[test]
fn narrow_terminal_truncates_and_reports_it() {
    // A single column whose value is far longer than the terminal
    // allows: there's nothing else to hide (hide-before-truncate has no
    // lower-priority column to drop), so truncation is the only option
    // and it must still be reported.
    let description = "a description that is well past the old forty-character cap";
    let items = vec![json!({ "description": description })];
    let columns = vec![TableColumn::new("description", "Description")];

    let (out, notes) = render_array_with_columns(&items, &columns, 20, None);

    assert!(
        notes.truncated,
        "narrow terminal must shorten a cell: {out}"
    );
    assert!(
        notes.hidden_columns.is_empty(),
        "only one column exists to begin with: {out}"
    );
    assert!(out.contains("..."), "{out}");
}

#[test]
fn narrow_terminal_hides_columns_before_truncating_any_of_the_survivors() {
    // Three equally-competing columns: at this width, showing all three
    // (or even two) would require truncating every survivor a little.
    // Hide-before-truncate should instead cascade down to the single
    // highest-priority column and show it in full.
    let items = vec![json!({ "a": "x".repeat(5), "b": "x".repeat(5), "c": "x".repeat(5) })];
    let columns = vec![
        TableColumn::new("a", "A"),
        TableColumn::new("b", "B"),
        TableColumn::new("c", "C"),
    ];

    let (out, notes) = render_array_with_columns(&items, &columns, 10, None);

    assert!(
        !notes.truncated,
        "hiding B and C should leave A fully shown, untruncated: {out}"
    );
    assert_eq!(
        notes.hidden_columns,
        vec!["B".to_owned(), "C".to_owned()],
        "should cascade down to the single highest-priority column: {out}"
    );
    assert!(!out.contains("..."), "{out}");
}

#[test]
fn overflow_hides_lowest_priority_columns_first() {
    let items = vec![json!({
        "id": "1",
        "name": "acme",
        "status": "active",
        "created_at": "2026-01-01",
    })];
    let columns = vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("name", "Name"),
        TableColumn::new("status", "Status"),
        TableColumn::new("created_at", "Created At"),
    ];

    let (out, notes) = render_array_with_columns(&items, &columns, 10, None);

    assert_eq!(
        notes.hidden_columns,
        vec!["Status".to_owned(), "Created At".to_owned()],
        "lowest-priority (trailing) columns are dropped first: {out}"
    );
    let header_line = out.lines().next().expect("header line");
    assert!(header_line.contains("ID"), "{out}");
    assert!(header_line.contains("NAME"), "{out}");
    assert!(!header_line.contains("STATUS"), "{out}");
    assert!(!header_line.contains("CREATED"), "{out}");
}

#[test]
fn render_human_with_view_reports_hidden_columns_in_footer() {
    let envelope = Envelope::success(
        json!([{
            "id": "1",
            "name": "acme",
            "status": "active",
            "region": "us-west",
            "created_at": "2026-01-01",
            "updated_at": "2026-01-02",
            "notes": "irrelevant, lowest priority",
        }]),
        "resource",
    );
    let columns = vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("name", "Name"),
        TableColumn::new("status", "Status"),
        TableColumn::new("region", "Region"),
        TableColumn::new("created_at", "Created At"),
        TableColumn::new("updated_at", "Updated At"),
        // Deliberately long enough that, combined with the columns above,
        // it can't fit alongside them at the fallback 80-column width.
        TableColumn::new("notes", "This Is An Extremely Long Trailing Column Header"),
    ];

    // In test runs stdout is not a TTY, so `terminal_width()` deterministically
    // falls back to 80 — these headers don't all fit at that width.
    let out = render_human_with_view(&envelope, Some(&columns), "");

    assert!(out.contains("hidden to fit the display width"), "{out}");
    assert!(
        out.contains("This Is An Extremely Long Trailing Column Header"),
        "{out}"
    );
    assert!(out.contains("--fields"), "{out}");
    assert!(out.contains("--json"), "{out}");
}

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
fn no_view_array_rendering_right_aligns_a_column_that_is_numeric_on_every_row() {
    let items = vec![
        json!({ "name": "small", "count": 3 }),
        json!({ "name": "bigger", "count": 42 }),
    ];

    let (out, _notes) = render_array(&items, "name,count", 80, None);
    let mut lines = out.lines();
    let header_line = lines.next().expect("header line");
    let row_lines: Vec<&str> = lines.skip(1).take(2).collect();

    assert!(header_line.ends_with(" COUNT"), "{header_line}");
    assert!(row_lines[0].ends_with("   3"), "{}", row_lines[0]);
    assert!(row_lines[1].ends_with("  42"), "{}", row_lines[1]);
    assert!(header_line.starts_with("NAME "), "{header_line}");
}

#[test]
fn no_view_array_rendering_keeps_a_mixed_type_column_left_aligned() {
    // Same field is a number on one row and a string on another — a
    // single non-number value anywhere disqualifies the whole column,
    // matching how right-aligning it would look ragged next to text.
    let items = vec![json!({ "code": 1 }), json!({ "code": "default" })];

    let (out, _notes) = render_array(&items, "", 80, None);
    let header_line = out.lines().next().expect("header line");

    assert!(header_line.starts_with("CODE"), "{header_line}");
}

#[test]
fn no_view_array_rendering_keeps_an_all_null_column_left_aligned() {
    // No row ever has a number at this field, so there's no positive
    // signal to right-align on.
    let items = vec![json!({ "note": null }), json!({ "note": null })];

    let (out, _notes) = render_array(&items, "", 80, None);
    let header_line = out.lines().next().expect("header line");

    assert!(header_line.starts_with("NOTE"), "{header_line}");
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

    let rendered = render_human_with_registry_selected(&envelope, &registry, "things", "status,id");

    let header_line = rendered.lines().next().expect("header line");
    assert!(!header_line.contains("NAME"), "{rendered}");
    let status_pos = header_line.find("STATUS").expect("status header");
    let id_pos = header_line.find("ID").expect("id header");
    assert!(
        status_pos < id_pos,
        "expected STATUS before ID per the requested field order: {header_line}"
    );
}

#[test]
fn fit_column_widths_gives_small_wants_priority_over_larger_ones() {
    // Regression: a naive `leftover / remaining` split can floor a small
    // want to zero (denying a column that needed only 1 more char)
    // while a much larger want absorbs that same unit and stays
    // truncated anyway — net truncation is identical, but a column that
    // could have been fully satisfied wasn't.
    let headers = [1, 1, 1];
    let natural = [2, 2, 6]; // wants: 1, 1, 5
    let no_truncate = [false, false, false];

    let (widths, truncated) = fit_column_widths(&headers, &natural, &no_truncate, 8);

    assert_eq!(
        widths[0], natural[0],
        "a column that only wanted 1 more char should get it in full: {widths:?}"
    );
    assert!(truncated, "budget is still too small overall: {widths:?}");
}

#[test]
fn overflow_hiding_accounts_for_no_truncate_columns_true_width() {
    // Regression: deciding what to hide from header length alone
    // under-counts a `no_truncate` column (it never shrinks below its
    // natural width), which could keep a short-header trailing column
    // that would never have fit anyway — overflowing when hiding it
    // would have let the row fit.
    let url = "x".repeat(40);
    let items = vec![json!({ "url": url, "notes": "irrelevant, lowest priority" })];
    let columns = vec![
        TableColumn::new("url", "URL").no_truncate(true),
        TableColumn::new("notes", "X"),
    ];

    // Exactly enough room for the URL alone (40 chars), not enough for
    // the URL plus even a 1-char trailing column and its gutter (43).
    let (out, notes) = render_array_with_columns(&items, &columns, 42, None);

    assert_eq!(
        notes.hidden_columns,
        vec!["X".to_owned()],
        "the trailing column must be hidden so the no_truncate URL column fits: {out}"
    );
    let header_line = out.lines().next().expect("header line");
    assert!(
        header_line.len() <= 42,
        "must not overflow once the trailing column is hidden: {out}"
    );
}

#[test]
fn render_array_with_columns_handles_no_columns_gracefully() {
    // A view's `--fields` filtered out every declared column: nothing to
    // build a table from, so this must report "no results" rather than
    // a blank header/rows table.
    let items = vec![json!({ "a": "1" })];
    let (out, notes) = render_array_with_columns(&items, &[], 80, None);

    assert_eq!(out, "(no results)\n");
    assert!(!notes.truncated, "{out}");
    assert!(notes.hidden_columns.is_empty(), "{out}");
}

#[test]
fn render_object_with_columns_handles_no_columns_gracefully() {
    // Sibling of the array-path test above (Copilot/human review caught
    // this asymmetry): a view's `--fields` filtered out every declared
    // column on an object-shaped response must report "(no data)"
    // rather than silently rendering an empty string.
    let map = json!({ "a": "1" });
    let (out, notes) =
        render_object_with_columns(map.as_object().expect("object fixture"), &[], 80);

    assert_eq!(out, "(no data)\n");
    assert!(!notes.truncated, "{out}");
    assert!(notes.hidden_columns.is_empty(), "{out}");
}

#[test]
fn no_view_array_of_empty_objects_reports_no_results() {
    // Every item is `{}`, so the dynamic (no-view) column catalog has no
    // keys to derive columns from — same "no columns" case as above,
    // reached through the no-view path instead.
    let items = vec![json!({}), json!({})];
    let (out, notes) = render_array(&items, "", 80, None);

    assert_eq!(out, "(no results)\n");
    assert!(notes.hidden_columns.is_empty(), "{out}");
}

#[test]
fn resolve_field_path_walks_dotted_wrapper_and_reports_missing_or_wrong_shape() {
    let map = json!({
        "parameters": { "items": [{"name": "limit"}], "total": 1 },
        "owner": "not-an-object",
    });
    let map = map.as_object().expect("object fixture");

    assert_eq!(
        resolve_field_path(map, "parameters.items"),
        map.get("parameters").and_then(|value| value.get("items"))
    );
    assert_eq!(resolve_field_path(map, "parameters.missing"), None);
    assert_eq!(
        resolve_field_path(map, "owner.name"),
        None,
        "intermediate value is a string, not an object"
    );
    assert_eq!(resolve_field_path(map, "missing"), None);
    assert_eq!(resolve_field_path(map, ""), None, "empty field");
    assert_eq!(resolve_field_path(map, ".parameters"), None, "leading dot");
    assert_eq!(resolve_field_path(map, "parameters."), None, "trailing dot");
    assert_eq!(
        resolve_field_path(map, "parameters..items"),
        None,
        "doubled dot"
    );
}

#[test]
fn resolve_field_parent_returns_parent_object_for_dotted_and_bare_fields() {
    let map = json!({
        "parameters": { "items": [], "total": 2 },
        "owner": "not-an-object",
    });
    let map = map.as_object().expect("object fixture");

    assert_eq!(
        resolve_field_parent(map, "parameters.items"),
        map.get("parameters").and_then(Value::as_object)
    );
    assert_eq!(
        resolve_field_parent(map, "items"),
        Some(map),
        "a field with no dot has the object being rendered as its own parent"
    );
    assert_eq!(
        resolve_field_parent(map, "owner.name"),
        None,
        "intermediate value is a string, not an object"
    );
    assert_eq!(resolve_field_parent(map, "missing.items"), None);
}

#[test]
fn resolve_nested_pagination_deserializes_a_pagination_meta_shaped_sibling() {
    let parent = json!({
        "pagination": { "total": 26, "offset": 0, "limit": 2, "count": 2, "has_more": true },
    });
    let parent = parent.as_object().expect("object fixture");

    let meta = resolve_nested_pagination(parent).expect("pagination sibling present");
    assert_eq!(
        meta,
        PaginationMeta {
            total: 26,
            offset: 0,
            limit: 2,
            count: 2,
            has_more: true,
        }
    );
}

#[test]
fn resolve_nested_pagination_is_none_when_the_sibling_is_absent_or_malformed() {
    let no_sibling = json!({ "items": [] });
    assert_eq!(
        resolve_nested_pagination(no_sibling.as_object().expect("object fixture")),
        None,
        "no pagination field at all"
    );

    let wrong_shape = json!({ "pagination": { "total": 26 } });
    assert_eq!(
        resolve_nested_pagination(wrong_shape.as_object().expect("object fixture")),
        None,
        "missing required PaginationMeta fields fails to deserialize"
    );

    let not_an_object = json!({ "pagination": "26 total" });
    assert_eq!(
        resolve_nested_pagination(not_an_object.as_object().expect("object fixture")),
        None,
        "pagination field present but not object-shaped"
    );
}

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
    // claim `--fields` fixes it (regression: it used to say so
    // unconditionally, misleading users into trying a flag that does
    // nothing for this case — see PR review discussion). Same fixture
    // shape as `render_human_with_view_reports_hidden_columns_in_footer`
    // (proven to overflow the fallback 80-column width), just nested
    // one level under an "items" field instead of being the top-level
    // view directly.
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

    let out = render_human_with_view(&envelope, Some(&columns), "");

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
    // scalar or a mixed (non-uniform) array at runtime — e.g. a field
    // that's usually a list of objects but is empty/absent for this row,
    // or simply the wrong shape. Rendering must stay the same flat
    // `header: value` line a column with `nested: None` would have
    // produced, not a `header:\n  value` block — regression guard for a
    // shape-drift bug where the header line alone changed to multi-line
    // even though the value itself fell back to `format_value`.
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
