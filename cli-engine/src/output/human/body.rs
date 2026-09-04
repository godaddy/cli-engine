use serde_json::Value;

use super::columns::{
    column_is_all_numeric, columns_fitting_width, dynamic_columns, fit_column_widths,
};
use super::value_format::{
    format_plain_value, format_value, indent_block, is_nestable, resolve_field_parent,
    resolve_field_path, resolve_nested_pagination, truncate,
};
use super::{Alignment, RenderNotes, TableColumn};
use crate::output::PaginationMeta;

/// Upper bound on a `no_truncate` column's width, even though it otherwise
/// skips the normal 40-char cap. Prevents a pathologically long field value
/// (not expected in practice, but not guaranteed by any schema) from padding
/// every row and the separator line out to an unusable or memory-heavy width.
///
/// This bounds runtime *values*, not the column *header*: width is always
/// widened back up to `column.header.len()` after the cap is applied, so a
/// header can never be truncated or misaligned even in the (unrealistic)
/// case where it exceeds `NO_TRUNCATE_MAX_WIDTH` itself. Headers are static,
/// developer-authored labels, not the pathological runtime data this cap
/// guards against.
pub(crate) const NO_TRUNCATE_MAX_WIDTH: usize = 4096;

/// Indent applied to a nested table/property-bag block under a parent
/// object's field. Matches the two-space depth-step the TOON encoder already
/// uses (`crate::output::toon`'s `push_line`), for a consistent look across
/// human and TOON nested rendering.
const NESTED_INDENT: &str = "  ";

/// Render just the data portion of a success envelope (no next-steps footer).
pub(super) fn render_data_body(
    data: &Value,
    columns: Option<&[TableColumn]>,
    fields: &str,
    available_width: usize,
    pagination: Option<&PaginationMeta>,
) -> (String, RenderNotes) {
    if let Some(columns) = columns {
        return match data {
            Value::Array(items) => {
                render_array_with_columns(items, columns, available_width, pagination)
            }
            Value::Object(map) => render_object_with_columns(map, columns, available_width),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                (format!("{}\n", format_value(data)), RenderNotes::default())
            }
        };
    }
    match data {
        Value::Array(items) => render_array(items, fields, available_width, pagination),
        Value::Object(map) => {
            let columns = dynamic_columns(fields, || map.keys().cloned().collect());
            render_object_with_columns(map, &columns, available_width)
        }
        other => (
            format!("{}\n", format_plain_value(other)),
            RenderNotes::default(),
        ),
    }
}

pub(crate) fn render_array_with_columns(
    items: &[Value],
    columns: &[TableColumn],
    available_width: usize,
    pagination: Option<&PaginationMeta>,
) -> (String, RenderNotes) {
    if items.is_empty() || columns.is_empty() {
        // Empty columns happens when every item is `{}` (the no-view
        // dynamic catalog has no keys to show) or a view's `--fields`
        // filtered out every declared column — either way there's nothing
        // to build a table from, so fall back to the same message used for
        // no items at all rather than rendering a blank header/rows table.
        return ("(no results)\n".to_owned(), RenderNotes::default());
    }
    if !items.iter().all(Value::is_object) {
        return (render_array_lines(items), RenderNotes::default());
    }
    // Natural widths (and rows) are computed for every original column
    // before deciding what to hide: a `no_truncate` column never shrinks
    // below its natural width, so the hiding decision has to know that real
    // requirement — using just its header length here could keep a
    // low-priority trailing column that would never have fit anyway,
    // producing an overflow that hiding it would have avoided.
    let header_lens: Vec<usize> = columns.iter().map(|column| column.header.len()).collect();
    let no_truncate_all: Vec<bool> = columns.iter().map(|column| column.no_truncate).collect();
    let mut natural = header_lens.clone();
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let value = item
                        .as_object()
                        .and_then(|map| resolve_field_path(map, &column.field))
                        .map_or_else(String::new, format_value);
                    let cap = if column.no_truncate {
                        NO_TRUNCATE_MAX_WIDTH
                    } else {
                        usize::MAX
                    };
                    natural[index] = natural[index].max(value.len().min(cap));
                    value
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let min_widths: Vec<usize> = (0..columns.len())
        .map(|index| {
            if no_truncate_all[index] {
                natural[index]
            } else {
                header_lens[index]
            }
        })
        .collect();
    let mut kept = columns_fitting_width(&min_widths, available_width);

    // Hiding a column is preferred over truncating a cell: if the survivors
    // still don't fit their natural width, keep dropping the lowest-priority
    // one and re-fitting, until either everyone remaining fits in full or
    // only one column is left (which always stays, however it fits).
    let (fitted, truncated) = loop {
        let (fitted, truncated) = fit_column_widths(
            &header_lens[..kept],
            &natural[..kept],
            &no_truncate_all[..kept],
            available_width,
        );
        if !truncated || kept <= 1 {
            break (fitted, truncated);
        }
        kept -= 1;
    };

    let hidden_columns = columns[kept..]
        .iter()
        .map(|column| column.header.clone())
        .collect::<Vec<_>>();
    let columns = &columns[..kept];
    let rows: Vec<Vec<String>> = rows
        .into_iter()
        .map(|row| row.into_iter().take(kept).collect())
        .collect();

    let table = render_table(
        &columns
            .iter()
            .map(|column| column.header.clone())
            .collect::<Vec<_>>(),
        &fitted,
        &columns
            .iter()
            .map(|column| column.align)
            .collect::<Vec<_>>(),
        &rows,
        pagination,
    );
    (
        table,
        RenderNotes {
            truncated,
            hidden_columns,
            nested_narrowing: false,
            pagination_shown: pagination.is_some(),
        },
    )
}

pub(crate) fn render_object_with_columns(
    map: &serde_json::Map<String, Value>,
    columns: &[TableColumn],
    available_width: usize,
) -> (String, RenderNotes) {
    if map.is_empty() || columns.is_empty() {
        // Empty columns happens the same way it does in
        // `render_array_with_columns`: a view's `--fields` filtered out
        // every declared column. Nothing to render either way, so this
        // reports the same "(no data)" a genuinely empty object gets,
        // rather than an unlabeled blank line.
        return ("(no data)\n".to_owned(), RenderNotes::default());
    }
    let mut out = String::new();
    let mut notes = RenderNotes::default();
    for column in columns {
        let value = resolve_field_path(map, &column.field);
        match (&column.nested, value) {
            (Some(nested_columns), Some(value)) if is_nestable(value) => {
                out.push_str(&format!("{}:\n", column.header));
                let child_width = available_width.saturating_sub(NESTED_INDENT.len());
                let nested_pagination = match value {
                    Value::Array(_) => {
                        resolve_field_parent(map, &column.field).and_then(resolve_nested_pagination)
                    }
                    _ => None,
                };
                let (block, child_notes) = render_nested_value(
                    value,
                    nested_columns,
                    child_width,
                    nested_pagination.as_ref(),
                );
                out.push_str(&indent_block(&block, NESTED_INDENT));
                if child_notes.truncated
                    || !child_notes.hidden_columns.is_empty()
                    || child_notes.nested_narrowing
                {
                    notes.nested_narrowing = true;
                }
                notes.truncated |= child_notes.truncated;
                notes.hidden_columns.extend(
                    child_notes
                        .hidden_columns
                        .into_iter()
                        .map(|hidden| format!("{} > {hidden}", column.header)),
                );
            }
            (_, value) => {
                let value_str = value.map_or_else(String::new, format_value);
                out.push_str(&format!("{}: {value_str}\n", column.header));
            }
        }
    }
    (out, notes)
}

pub(crate) fn render_array(
    items: &[Value],
    fields: &str,
    available_width: usize,
    pagination: Option<&PaginationMeta>,
) -> (String, RenderNotes) {
    if items.is_empty() {
        return ("(no results)\n".to_owned(), RenderNotes::default());
    }
    let Some(Value::Object(first_map)) = items.first() else {
        return (render_array_lines(items), RenderNotes::default());
    };
    if !items.iter().all(Value::is_object) {
        return (render_array_lines(items), RenderNotes::default());
    }
    let columns: Vec<TableColumn> = dynamic_columns(fields, || first_map.keys().cloned().collect())
        .into_iter()
        .map(|column| {
            if column_is_all_numeric(items, &column.field) {
                column.align(Alignment::Right)
            } else {
                column
            }
        })
        .collect();
    render_array_with_columns(items, &columns, available_width, pagination)
}

fn render_array_lines(items: &[Value]) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(&format!("{}\n", format_plain_value(item)));
    }
    out
}

/// Pads `text` to `width`, on the left for `Alignment::Right` and on the
/// right otherwise — matching how the header row is padded so a column's
/// header and cells share the same alignment.
fn pad_column(text: &str, width: usize, alignment: Alignment) -> String {
    match alignment {
        Alignment::Left => format!("{text:<width$}"),
        Alignment::Right => format!("{text:>width$}"),
    }
}

fn render_table(
    headers: &[String],
    widths: &[usize],
    alignments: &[Alignment],
    rows: &[Vec<String>],
    pagination: Option<&PaginationMeta>,
) -> String {
    let mut out = String::new();
    for (index, header) in headers.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(&pad_column(
            &header.to_uppercase(),
            widths[index],
            alignments[index],
        ));
    }
    out.push('\n');
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(&"-".repeat(*width));
    }
    out.push('\n');
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                out.push_str("  ");
            }
            out.push_str(&pad_column(
                &truncate(value, widths[index]),
                widths[index],
                alignments[index],
            ));
        }
        out.push('\n');
    }
    // Merge the pagination facts into this footer rather than letting
    // `append_pagination_summary` print a second, redundant line right below
    // it — both would otherwise state the same shown/total count. The shown
    // count comes from `rows.len()`, not `pagination.count`: a later
    // pipeline step (`--expr`) can still reshape `envelope.data` after
    // pagination ran, so `rows.len()` is what's actually rendered above,
    // while `total`/`offset`/`limit` stay pagination's own facts.
    match pagination {
        Some(pagination) => out.push_str(&format!(
            "\n({} of {} rows, offset {}, limit {})\n",
            rows.len(),
            pagination.total,
            pagination.offset,
            pagination.limit
        )),
        None => out.push_str(&format!("\n({} rows)\n", rows.len())),
    }
    out
}

/// Renders a nested column's resolved value as a child block, reusing the
/// same renderers a top-level array/object would use, just at a narrowed
/// width. Only called once [`is_nestable`] has confirmed `value`'s shape, so
/// the array/object arms below are the only ones a real caller reaches; the
/// scalar fallback keeps this function total on its own.
fn render_nested_value(
    value: &Value,
    nested_columns: &[TableColumn],
    available_width: usize,
    pagination: Option<&PaginationMeta>,
) -> (String, RenderNotes) {
    match value {
        Value::Array(items) => {
            render_array_with_columns(items, nested_columns, available_width, pagination)
        }
        Value::Object(map) => render_object_with_columns(map, nested_columns, available_width),
        other => (format!("{}\n", format_value(other)), RenderNotes::default()),
    }
}
