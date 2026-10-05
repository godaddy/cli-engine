use serde_json::json;

use crate::output::human::body::{render_array, render_array_with_columns};
use crate::output::human::{Alignment, TableColumn};

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

    let (out, _notes) = render_array_with_columns(&items, &columns, 80, None, false);
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

    let (out, _notes) = render_array_with_columns(&items, &columns, 80, None, false);
    let mut lines = out.lines();
    let header_line = lines.next().expect("header line");

    assert!(
        header_line.starts_with("NAME"),
        "Alignment::Left is the default: {header_line}"
    );
}

#[test]
fn no_view_array_rendering_right_aligns_a_column_that_is_numeric_on_every_row() {
    let items = vec![
        json!({ "name": "small", "count": 3 }),
        json!({ "name": "bigger", "count": 42 }),
    ];

    let (out, _notes) = render_array(&items, "name,count", 80, None, false);
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

    let (out, _notes) = render_array(&items, "", 80, None, false);
    let header_line = out.lines().next().expect("header line");

    assert!(header_line.starts_with("CODE"), "{header_line}");
}

#[test]
fn no_view_array_rendering_keeps_an_all_null_column_left_aligned() {
    // No row ever has a number at this field, so there's no positive
    // signal to right-align on.
    let items = vec![json!({ "note": null }), json!({ "note": null })];

    let (out, _notes) = render_array(&items, "", 80, None, false);
    let header_line = out.lines().next().expect("header line");

    assert!(header_line.starts_with("NOTE"), "{header_line}");
}
