use std::collections::BTreeSet;
use std::io::IsTerminal;

use serde_json::Value;

use super::TableColumn;
use super::value_format::resolve_field_path;

/// Space between adjacent rendered columns. Must match the gutter
/// `render_table` actually writes, since width-fitting math (how much room
/// is left for column content) has to agree with what gets printed.
const COLUMN_GUTTER: usize = 2;

/// Detects how wide to render human-output tables and guides.
///
/// An interactive terminal gets its live width (via `termimad`); anything
/// else (pipes, files, CI) gets a fixed `80` so non-interactive `--human`
/// output stays deterministic. Floored at `20` in case a terminal reports an
/// unusably small or zero width.
#[must_use]
pub(crate) fn terminal_width() -> usize {
    if std::io::stdout().is_terminal() {
        usize::from(termimad::terminal_size().0).max(20)
    } else {
        80
    }
}

/// Builds the column catalog for data with no registered view: when `fields`
/// names specific fields (not empty/`all`/`*`), columns are derived from that
/// list, in the order given (deduplicated) — the same order source a
/// registered view's `--fields` selection uses (see `select_columns`).
/// Otherwise falls back to `natural_keys()` sorted alphabetically, since a
/// bare JSON object has no other order signal to offer.
pub(crate) fn dynamic_columns(
    fields: &str,
    natural_keys: impl FnOnce() -> Vec<String>,
) -> Vec<TableColumn> {
    let fields = fields.trim();
    if fields.is_empty() || fields == "all" || fields == "*" {
        let mut keys = natural_keys();
        keys.sort();
        return keys
            .into_iter()
            .map(|key| TableColumn::new(key.clone(), key))
            .collect();
    }
    let mut seen = BTreeSet::new();
    fields
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty() && seen.insert(*part))
        .map(|field| TableColumn::new(field, field))
        .collect()
}

/// True when at least one item has a JSON number at `field`, and no item
/// with a present, non-null value at `field` holds anything else.
pub(crate) fn column_is_all_numeric(items: &[Value], field: &str) -> bool {
    let mut saw_number = false;
    for item in items {
        match item
            .as_object()
            .and_then(|map| resolve_field_path(map, field))
        {
            Some(Value::Number(_)) => saw_number = true,
            Some(Value::Null) | None => {}
            Some(_) => return false,
        }
    }
    saw_number
}

/// Chooses which column indexes fit in `available_width` — preferring to drop
/// the lowest-priority (highest-index) non-essential columns first when the
/// terminal is too narrow for all of them. `min_widths[i]` should be the
/// column's header length for a column that can still shrink, or its full
/// natural width for one that can't (e.g. `no_truncate`) — using a
/// shrinkable column's header length here lets it still be counted as
/// fitting even though its eventual rendered width may be larger.
/// `essential[i]` exempts that column from being dropped at all, even if it
/// alone exceeds `available_width`; at least one column always survives
/// regardless of essential-ness.
pub(crate) fn columns_fitting_width(
    min_widths: &[usize],
    essential: &[bool],
    available_width: usize,
) -> Vec<usize> {
    let mut kept: Vec<usize> = (0..min_widths.len()).collect();
    while kept.len() > 1 && total_width(&kept, min_widths) > available_width {
        match kept.iter().rposition(|&index| !essential[index]) {
            Some(position) => {
                kept.remove(position);
            }
            None => break,
        }
    }
    kept
}

/// Sum of `widths` at `indices` plus the gutters between them.
fn total_width(indices: &[usize], widths: &[usize]) -> usize {
    let gutters = COLUMN_GUTTER * indices.len().saturating_sub(1);
    gutters + indices.iter().map(|&index| widths[index]).sum::<usize>()
}

/// Fits `natural` (fully-untruncated) column widths into `available_width`.
///
/// Columns where `fixed[i]` is true (a `no_truncate` column, an essential
/// column, or every column at once when the caller disabled truncation
/// entirely) are never shrunk — they keep their natural width unconditionally
/// — and their width is reserved out of the budget up front. The remaining
/// columns are never shrunk below their header length, and share whatever
/// budget is left beyond that, smallest-need-first, so a column that wants
/// only a little gets exactly that instead of an equal-but-wasteful split.
///
/// Returns the fitted widths and whether any non-fixed column ended up
/// narrower than its natural width (i.e. some cell will actually be cut).
pub(crate) fn fit_column_widths(
    headers: &[usize],
    natural: &[usize],
    fixed: &[bool],
    available_width: usize,
) -> (Vec<usize>, bool) {
    let mut widths = natural.to_vec();
    let truncatable: Vec<usize> = (0..fixed.len()).filter(|&index| !fixed[index]).collect();
    if truncatable.is_empty() {
        return (widths, false);
    }
    let gutters = COLUMN_GUTTER * headers.len().saturating_sub(1);
    let reserved: usize = (0..fixed.len())
        .filter(|&index| fixed[index])
        .map(|index| natural[index])
        .sum();
    let budget = available_width
        .saturating_sub(gutters)
        .saturating_sub(reserved);
    let header_floor: usize = truncatable.iter().map(|&index| headers[index]).sum();
    for &index in &truncatable {
        widths[index] = headers[index];
    }
    let mut leftover = budget.saturating_sub(header_floor);
    let mut needy: Vec<usize> = truncatable
        .iter()
        .copied()
        .filter(|&index| natural[index] > headers[index])
        .collect();
    needy.sort_by_key(|&index| natural[index] - headers[index]);
    // Smallest-need-first, take exactly what's wanted or whatever's left,
    // whichever is less. Deliberately not an even split of `leftover` across
    // the remaining columns: dividing first and taking `min(wants, share)`
    // can floor a small want to zero when `leftover < remaining columns`,
    // denying it entirely while a later, greedier column absorbs the
    // remainder — worse than just letting small wants claim what they need
    // outright before anyone larger gets a turn.
    for &index in &needy {
        let wants = natural[index] - headers[index];
        let take = wants.min(leftover);
        widths[index] += take;
        leftover -= take;
    }
    let truncated = truncatable
        .iter()
        .any(|&index| widths[index] < natural[index]);
    (widths, truncated)
}
