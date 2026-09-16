use std::{borrow::Cow, collections::HashMap};

use super::RenderNotes;
use crate::cli::quote_pagination_value;
use crate::output::{CursorMeta, NextAction, NextActionParam, PaginationMeta};

/// Appends footer hints for truncated cells and/or hidden columns to `out`
/// (a no-op when neither happened). Mirrors `append_next_actions`: writes
/// directly into `out` rather than building a separate string.
pub(super) fn append_render_notes(out: &mut String, notes: &RenderNotes) {
    // `--fields` only ever selects among top-level declared columns: it can
    // drop a `TableColumn::nested` column entirely, but can't narrow what
    // shows *inside* one. Suggesting it as a fix once any of the reported
    // narrowing happened inside a nested block would be wrong — there's no
    // flag that reaches that fine-grained, so `--json` is the only real
    // remedy in that case.
    let fields_helps = !notes.nested_narrowing;
    if notes.truncated {
        if fields_helps {
            out.push_str(
                "\nOutput truncated to fit the display width — use --fields to show fewer columns, or --json for full values.\n",
            );
        } else {
            out.push_str(
                "\nOutput truncated to fit the display width — use --json for full values.\n",
            );
        }
    }
    if !notes.hidden_columns.is_empty() {
        let suggestion = if fields_helps {
            "use --fields to choose columns, or --json for full output"
        } else {
            "use --json for full output"
        };
        out.push_str(&format!(
            "\n{} column{} hidden to fit the display width ({}) — {suggestion}.\n",
            notes.hidden_columns.len(),
            if notes.hidden_columns.len() == 1 {
                ""
            } else {
                "s"
            },
            notes.hidden_columns.join(", "),
        ));
    }
}

/// Where a pagination/cursor summary clause is being rendered. The two
/// contexts always describe the same underlying facts for the same case —
/// only the wording differs, between a compact parenthetical merged into a
/// table's row-count footer and a full standalone sentence for output that
/// didn't render as a table. [`pagination_summary_text`]/
/// [`cursor_summary_text`] hold both wordings for every case side by side in
/// one place, so [`super::body::render_table`]'s footer and
/// [`append_pagination_summary`]/[`append_cursor_summary`] can't drift apart
/// from each other the way two independent `format!` call sites could.
#[derive(Clone, Copy)]
pub(super) enum SummaryStyle {
    /// Merged into a table's row-count footer, e.g. `N of M rows`.
    TableFooter,
    /// A standalone sentence, e.g. `Showing N of M`.
    Standalone,
}

/// Builds the offset-pagination summary clause for `count` shown items,
/// worded for `style`. `count` takes anything `Display`s so callers can pass
/// either a `usize` row count (`render_table`) or an `i64` shown count
/// (`append_pagination_summary`) without a cast.
pub(super) fn pagination_summary_text(
    style: SummaryStyle,
    count: impl std::fmt::Display,
    pagination: &PaginationMeta,
) -> String {
    match style {
        SummaryStyle::TableFooter => format!(
            "{count} of {} rows, offset {}, limit {}",
            pagination.total, pagination.offset, pagination.limit
        ),
        SummaryStyle::Standalone => format!(
            "Showing {count} of {} (offset {}, limit {})",
            pagination.total, pagination.offset, pagination.limit
        ),
    }
}

/// Builds the cursor-pagination summary clause for `count` shown items,
/// worded for `style`. Unlike offset pagination, a `total` is not
/// guaranteed — a pure opaque cursor may never report one — so this falls
/// back to a "so far" phrasing naming the resume token, or a bare count when
/// the backend reported nothing at all.
pub(super) fn cursor_summary_text(
    style: SummaryStyle,
    count: impl std::fmt::Display,
    cursor: &CursorMeta,
) -> String {
    match (style, cursor.total, cursor.remaining, &cursor.continue_from) {
        (SummaryStyle::TableFooter, Some(total), _, _) => format!("{count} of {total} rows"),
        (SummaryStyle::TableFooter, None, Some(remaining), _) => {
            format!("{count} rows, {remaining} remaining")
        }
        (SummaryStyle::TableFooter, None, None, Some(token)) => {
            format!(
                "{count} rows so far; use {} for more",
                resume_hint(cursor, token)
            )
        }
        (SummaryStyle::TableFooter, None, None, None) => format!("{count} rows"),
        (SummaryStyle::Standalone, Some(total), _, _) => format!("Showing {count} of {total}"),
        (SummaryStyle::Standalone, None, Some(remaining), _) => {
            format!("Showing {count} ({remaining} remaining)")
        }
        (SummaryStyle::Standalone, None, None, Some(token)) => {
            format!(
                "Showing {count} items so far; use {} for more",
                resume_hint(cursor, token)
            )
        }
        (SummaryStyle::Standalone, None, None, None) => format!("Showing {count}"),
    }
}

/// Builds the `--limit N --continue <token>`/`--continue <token>` fragment
/// for a cursor "so far" hint, matching exactly what the engine appends to
/// `next_actions` for the same response (`middleware::run::render_envelope`):
/// `--limit` is included unless `cursor.self_sufficient_limit` says the
/// token alone already carries the effective page size. Copy-pasting this
/// hint must produce the same command the machine-readable `next_actions`
/// entry already suggests — including `--limit` when the token doesn't
/// need it would print a fabricated size, but omitting it when the token
/// truly doesn't carry one could resume at a different page size (or, if
/// the handler's effective limit exceeds this command's own `max_limit`,
/// print a `--limit` the parser would reject outright).
fn resume_hint(cursor: &CursorMeta, token: &str) -> String {
    let token = quote_pagination_value(token);
    if cursor.self_sufficient_limit {
        format!("--continue {token}")
    } else {
        format!("--limit {} --continue {token}", cursor.limit)
    }
}

/// Appends a one-line pagination summary to `out` (a no-op when the response
/// wasn't paginated). Unlike `next_actions`, this always shows the underlying
/// facts even on the last page, where there's no follow-up command to
/// suggest.
///
/// Only a fallback: when the data rendered as a table, `render_table` already
/// merged these same facts into its `(N of M rows, ...)` footer
/// (`RenderNotes::pagination_shown` signals that to
/// [`render_human_with_view`](super::render_human_with_view)), so this only
/// actually prints anything for a paginated response that *didn't* render as
/// a table (e.g. a bare array of scalars) — otherwise the two would repeat
/// the same count/offset/limit on consecutive lines.
///
/// `shown` is the caller's actual rendered item count (from `envelope.data`,
/// post-pipeline), used in place of `pagination.count` — which is only the
/// pre-`--expr` slice size and can go stale once `--expr` reshapes the array
/// after pagination ran (mirrors the same fix in `render_table`). `None`
/// means `--expr` reshaped the data into something that's no longer even an
/// array (e.g. `length(@)` turning it into a number) — pagination still ran,
/// but there's no rendered row count left to describe, so this falls back to
/// a more neutral line instead of a "Showing N of M" claim that would no
/// longer match what's actually displayed above it.
pub(super) fn append_pagination_summary(
    out: &mut String,
    pagination: Option<&PaginationMeta>,
    shown: Option<i64>,
) {
    let Some(pagination) = pagination else {
        return;
    };
    match shown {
        Some(count) => out.push_str(&format!(
            "\n{}\n",
            pagination_summary_text(SummaryStyle::Standalone, count, pagination)
        )),
        None => out.push_str(&format!(
            "\n(pagination: {} total, offset {}, limit {})\n",
            pagination.total, pagination.offset, pagination.limit
        )),
    }
}

/// Appends a one-line cursor-pagination summary to `out` (a no-op when the
/// response wasn't cursor-paginated). The cursor counterpart of
/// [`append_pagination_summary`] — same fallback role (only fires when
/// `render_table`'s footer didn't already merge these facts), same `shown`
/// semantics (the actual post-`--expr` rendered count, not the possibly-stale
/// `cursor.count`) and the same `None` handling: `--expr` reshaping the data
/// into something that's no longer an array (e.g. `length(@)`) must not
/// print a "Showing N ..." claim built from the now-stale pre-`--expr`
/// `cursor.count` — falling back to `cursor.count` here (rather than a
/// neutral line, as `append_pagination_summary` does) would do exactly
/// that.
pub(super) fn append_cursor_summary(
    out: &mut String,
    cursor: Option<&CursorMeta>,
    shown: Option<i64>,
) {
    let Some(cursor) = cursor else {
        return;
    };
    match shown {
        Some(count) => out.push_str(&format!(
            "\n{}\n",
            cursor_summary_text(SummaryStyle::Standalone, count, cursor)
        )),
        None => out.push_str(&format!(
            "\n(cursor: limit {}{})\n",
            cursor.limit,
            cursor
                .total
                .map(|total| format!(", {total} total"))
                .unwrap_or_default()
        )),
    }
}

/// Append a "Next steps:" footer listing suggested follow-up commands to `out`
/// (a no-op when there are none). Each action shows its command template with
/// any known param values substituted into their `<placeholder>` (params
/// without a known value, e.g. required-only hints, are shown as-is), followed
/// by the description beneath it. Writes directly into `out` to avoid
/// per-action temporaries.
pub(super) fn append_next_actions(out: &mut String, actions: &[NextAction]) {
    if actions.is_empty() {
        return;
    }
    out.push_str("\nNext steps:\n");
    for action in actions {
        out.push_str("  ");
        out.push_str(&substitute_known_params(&action.command, &action.params));
        out.push_str("\n      ");
        out.push_str(&action.description);
        out.push('\n');
    }
}

/// Fills a `NextAction` command template with any params that carry a known
/// concrete `value` — e.g. `"domain quote <domain>"` with
/// `params["domain"].value == Some("example.com")` becomes
/// `"domain quote example.com"`. A param's placeholder is its key wrapped in
/// angle brackets (`<domain>`); params without a known value (required-only
/// hints) are left as literal placeholder text for the user to fill in.
/// Borrows `command` as-is (no allocation) when nothing has a known value.
fn substitute_known_params<'cmd>(
    command: &'cmd str,
    params: &HashMap<String, NextActionParam>,
) -> Cow<'cmd, str> {
    let mut command = Cow::Borrowed(command);
    for (key, param) in params {
        if let Some(value) = &param.value {
            let placeholder = format!("<{key}>");
            if command.contains(&placeholder) {
                command = Cow::Owned(command.replace(&placeholder, value));
            }
        }
    }
    command
}
