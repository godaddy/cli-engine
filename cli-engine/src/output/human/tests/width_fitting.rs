use serde_json::json;

use crate::output::Envelope;
use crate::output::human::body::{
    NO_TRUNCATE_MAX_WIDTH, render_array, render_array_with_columns, render_object_with_columns,
};
use crate::output::human::columns::fit_column_widths;
use crate::output::human::{TableColumn, render_human_with_view};

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

    let (out, notes) = render_array_with_columns(&items, &columns, 80, None, false);

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

    let (out, _notes) = render_array_with_columns(&items, &columns, 80, None, false);

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
fn column_width_never_shrinks_below_a_long_header() {
    let long_header = "A Very Long Header That Exceeds The Default Width Cap";
    let items = vec![json!({ "field": "short" })];
    let columns = vec![TableColumn::new("field", long_header)];

    // Deliberately far narrower than the header: the header must still
    // render in full even though the row ends up wider than the terminal.
    let (out, _notes) = render_array_with_columns(&items, &columns, 10, None, false);
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

    let (out, notes) = render_array_with_columns(&items, &columns, 200, None, false);

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

    let (out, notes) = render_array_with_columns(&items, &columns, 20, None, false);

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

    let (out, notes) = render_array_with_columns(&items, &columns, 10, None, false);

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

    let (out, notes) = render_array_with_columns(&items, &columns, 10, None, false);

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
fn essential_column_survives_even_when_a_higher_priority_column_is_dropped() {
    // "Notes" is declared first (highest priority) but isn't essential;
    // type/name/data are declared after it but are essential. At this
    // width, keeping all four doesn't fit, but keeping just the three
    // essential ones does — so hiding must drop the non-essential column
    // even though normal priority order would hide the trailing ones first.
    let items = vec![json!({
        "notes": "irrelevant",
        "type": "A",
        "name": "www",
        "data": "1234",
    })];
    let columns = vec![
        TableColumn::new("notes", "Notes"),
        TableColumn::new("type", "Type").essential(true),
        TableColumn::new("name", "Name").essential(true),
        TableColumn::new("data", "Data").essential(true),
    ];

    let (out, notes) = render_array_with_columns(&items, &columns, 16, None, false);

    assert_eq!(
        notes.hidden_columns,
        vec!["Notes".to_owned()],
        "the non-essential column is hidden to make room for the essential ones: {out}"
    );
    assert!(!notes.truncated, "{out}");
    let header_line = out.lines().next().expect("header line");
    assert!(header_line.contains("TYPE"), "{out}");
    assert!(header_line.contains("NAME"), "{out}");
    assert!(header_line.contains("DATA"), "{out}");
}

#[test]
fn essential_columns_overflow_instead_of_hidden_or_truncated_when_the_terminal_is_too_narrow() {
    // Same fixture as `narrow_terminal_hides_columns_before_truncating_any_of_the_survivors`,
    // but every column is essential this time: none may be hidden or
    // shrunk, so all three survive in full and the row simply overflows the
    // cramped terminal instead.
    let items = vec![json!({ "a": "x".repeat(5), "b": "x".repeat(5), "c": "x".repeat(5) })];
    let columns = vec![
        TableColumn::new("a", "A").essential(true),
        TableColumn::new("b", "B").essential(true),
        TableColumn::new("c", "C").essential(true),
    ];

    let (out, notes) = render_array_with_columns(&items, &columns, 10, None, false);

    assert!(
        notes.hidden_columns.is_empty(),
        "essential columns must never be hidden: {out}"
    );
    assert!(
        !notes.truncated,
        "essential columns must never be truncated either: {out}"
    );
    assert!(!out.contains("..."), "{out}");
    let header_line = out.lines().next().expect("header line");
    assert!(
        header_line.contains('A') && header_line.contains('B') && header_line.contains('C'),
        "{out}"
    );
    assert!(
        header_line.len() > 10,
        "overflows the 10-column terminal rather than shrinking any essential column: {out}"
    );
}

#[test]
fn explicit_fields_selection_disables_column_hiding_entirely() {
    // Same fixture and width as `overflow_hides_lowest_priority_columns_first`,
    // but this simulates an explicit `--fields` selection: the caller asked
    // for exactly these columns, so none of them may be dropped for width.
    // Every value here is no longer than its header, so there's nothing left
    // to shrink either (headers never truncate) — the row simply overflows
    // the 10-column terminal instead of losing a column.
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

    let (out, notes) = render_array_with_columns(&items, &columns, 10, None, true);

    assert!(
        notes.hidden_columns.is_empty(),
        "an explicit --fields selection must never drop a column for width: {out}"
    );
    assert!(!notes.truncated, "{out}");
    let header_line = out.lines().next().expect("header line");
    assert!(header_line.contains("STATUS"), "{out}");
    assert!(header_line.contains("CREATED AT"), "{out}");
    assert!(
        header_line.len() > 10,
        "overflows rather than dropping a column to fit the 10-column terminal: {out}"
    );
}

#[test]
fn explicit_fields_selection_disables_truncation_even_when_a_value_outgrows_its_header() {
    // Unlike the sibling test above, this value is longer than its header,
    // so hiding-before-truncating would normally still shrink it to fit.
    // An explicit `--fields` selection must skip that entirely: the user
    // asked to see this field, so it renders at full natural width (and the
    // row overflows the terminal) rather than losing any characters.
    let description = "a description that is well past the old forty-character cap";
    let items = vec![json!({ "description": description })];
    let columns = vec![TableColumn::new("description", "Description")];

    let (out, notes) = render_array_with_columns(&items, &columns, 20, None, true);

    assert!(!notes.truncated, "{out}");
    assert!(notes.hidden_columns.is_empty(), "{out}");
    assert!(out.contains(description), "{out}");
    assert!(!out.contains("..."), "{out}");
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
    let out = render_human_with_view(&envelope, Some(&columns), "", false);

    assert!(out.contains("hidden to fit the display width"), "{out}");
    assert!(
        out.contains("This Is An Extremely Long Trailing Column Header"),
        "{out}"
    );
    assert!(out.contains("--fields"), "{out}");
    assert!(out.contains("--json"), "{out}");
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
    let (out, notes) = render_array_with_columns(&items, &columns, 42, None, false);

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
    let (out, notes) = render_array_with_columns(&items, &[], 80, None, false);

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
    let (out, notes) = render_array(&items, "", 80, None, false);

    assert_eq!(out, "(no results)\n");
    assert!(notes.hidden_columns.is_empty(), "{out}");
}
