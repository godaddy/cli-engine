use std::{borrow::Cow, collections::HashMap};

use super::RenderNotes;
use crate::output::{NextAction, NextActionParam, PaginationMeta};

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
            "\nShowing {count} of {} (offset {}, limit {})\n",
            pagination.total, pagination.offset, pagination.limit
        )),
        None => out.push_str(&format!(
            "\n(pagination: {} total, offset {}, limit {})\n",
            pagination.total, pagination.offset, pagination.limit
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
