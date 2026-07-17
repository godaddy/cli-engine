use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    io::IsTerminal,
    sync::{Arc, OnceLock, RwLock},
};

use serde_json::Value;

use super::{Envelope, NextAction, NextActionParam};

/// Column definition for registered human table views.
///
/// Column order is a priority order, most important first: table rendering
/// keeps this order on screen, and when the terminal is too narrow to show
/// every column, the lowest-priority (trailing) columns are hidden first. Put
/// the column a reader most needs — usually an id or name — first.
///
/// This declared order is only the *fallback* — whenever a `--fields`/
/// `default_fields` selection is given, its order wins instead (see
/// [`crate::output::render_human_with_registry_selected`]), for both display
/// and hide-priority. Declared order only governs output when no selection is
/// given at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableColumn {
    /// JSON field path.
    pub field: String,
    /// Display header.
    pub header: String,
    /// When true, this column's values are never shrunk to fit the terminal
    /// (still capped at `NO_TRUNCATE_MAX_WIDTH` to bound pathologically long
    /// values). Use this for values that are useless when cut short, such as
    /// URLs.
    pub no_truncate: bool,
}

impl TableColumn {
    /// Creates a table column from a JSON field path and display header.
    #[must_use]
    pub fn new(field: impl Into<String>, header: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            header: header.into(),
            no_truncate: false,
        }
    }

    /// Opts this column out of terminal-width-driven shrinking. Values are
    /// still capped at `NO_TRUNCATE_MAX_WIDTH`.
    #[must_use]
    pub fn no_truncate(mut self, value: bool) -> Self {
        self.no_truncate = value;
        self
    }
}

/// Human view definition keyed by schema id.
///
/// `columns` order is a priority order — see [`TableColumn`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanViewDef {
    /// Schema id, usually the command path.
    pub schema_id: String,
    /// Columns rendered for matching object or list data, most important
    /// first.
    pub columns: Vec<TableColumn>,
}

impl HumanViewDef {
    /// Creates a column-based human view for a schema id or command path.
    #[must_use]
    pub fn new(schema_id: impl Into<String>, columns: impl Into<Vec<TableColumn>>) -> Self {
        Self {
            schema_id: schema_id.into(),
            columns: columns.into(),
        }
    }
}

/// Function used to render custom human output for a JSON value.
pub type HumanViewFn = Arc<dyn Fn(&Value) -> String + Send + Sync>;

/// Custom human renderer wrapper.
#[derive(Clone)]
pub struct HumanViewRenderer {
    render: HumanViewFn,
}

impl HumanViewRenderer {
    /// Creates a custom renderer.
    #[must_use]
    pub fn new(render: impl Fn(&Value) -> String + Send + Sync + 'static) -> Self {
        Self {
            render: Arc::new(render),
        }
    }

    /// Renders data with the custom renderer.
    #[must_use]
    pub fn render(&self, data: &Value) -> String {
        (self.render)(data)
    }
}

impl fmt::Debug for HumanViewRenderer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HumanViewRenderer")
            .finish_non_exhaustive()
    }
}

/// Registry of human column and custom-renderer views.
#[derive(Clone, Debug, Default)]
pub struct HumanViewRegistry {
    by_schema_id: BTreeMap<String, Vec<TableColumn>>,
    custom_by_schema_id: BTreeMap<String, HumanViewRenderer>,
}

impl HumanViewRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a column-based human view.
    pub fn register(&mut self, view: HumanViewDef) {
        self.by_schema_id.insert(view.schema_id, view.columns);
    }

    /// Registers a custom renderer for a schema id.
    pub fn register_func(
        &mut self,
        schema_id: impl Into<String>,
        render: impl Fn(&Value) -> String + Send + Sync + 'static,
    ) {
        self.custom_by_schema_id
            .insert(schema_id.into(), HumanViewRenderer::new(render));
    }

    /// Merges another registry into this one.
    pub fn merge(&mut self, other: &Self) {
        self.by_schema_id.extend(other.by_schema_id.clone());
        self.custom_by_schema_id
            .extend(other.custom_by_schema_id.clone());
    }

    /// Returns column definitions for a schema id.
    #[must_use]
    pub fn columns(&self, schema_id: &str) -> Option<&[TableColumn]> {
        self.by_schema_id.get(schema_id).map(Vec::as_slice)
    }

    /// Returns the custom renderer for a schema id.
    #[must_use]
    pub fn custom(&self, schema_id: &str) -> Option<&HumanViewRenderer> {
        self.custom_by_schema_id.get(schema_id)
    }

    /// Whether any human view (column-based or custom) is registered for a
    /// schema id. Such a view selects its own columns from the full payload, so
    /// callers must not pre-project the data before handing it to the renderer.
    #[must_use]
    pub fn has_view(&self, schema_id: &str) -> bool {
        self.by_schema_id.contains_key(schema_id)
            || self.custom_by_schema_id.contains_key(schema_id)
    }
}

static GLOBAL_HUMAN_VIEW_REGISTRY: OnceLock<RwLock<HumanViewRegistry>> = OnceLock::new();

fn global_human_view_registry() -> &'static RwLock<HumanViewRegistry> {
    GLOBAL_HUMAN_VIEW_REGISTRY.get_or_init(|| RwLock::new(HumanViewRegistry::new()))
}

/// Registers a process-global column view.
pub fn register_global_human_view(view: HumanViewDef) {
    let mut registry = global_human_view_registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.register(view);
}

/// Registers a process-global custom human renderer.
pub fn register_global_human_view_func(
    schema_id: impl Into<String>,
    render: impl Fn(&Value) -> String + Send + Sync + 'static,
) {
    let mut registry = global_human_view_registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.register_func(schema_id, render);
}

/// Looks up global columns for a schema id.
#[must_use]
pub fn lookup_global_human_view_columns(schema_id: &str) -> Option<Vec<TableColumn>> {
    global_human_view_registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .columns(schema_id)
        .map(<[TableColumn]>::to_vec)
}

/// Looks up a global custom renderer for a schema id.
#[must_use]
pub fn lookup_global_human_view_func(schema_id: &str) -> Option<HumanViewRenderer> {
    global_human_view_registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .custom(schema_id)
        .cloned()
}

/// Returns a snapshot of the process-global human view registry.
#[must_use]
pub fn global_human_view_registry_snapshot() -> HumanViewRegistry {
    global_human_view_registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Renders an envelope using generic human output.
///
/// There's no field-selection concept at this entry point, so a no-view
/// array/object falls back to alphabetical key order — use
/// [`render_human_with_registry_selected`] when a `--fields`/`default_fields`
/// value is available, so its order can drive column order too.
#[must_use]
pub fn render_human(envelope: &Envelope) -> String {
    render_human_with_view(envelope, None, "")
}

/// Renders an envelope using a human view registry.
#[must_use]
pub fn render_human_with_registry(envelope: &Envelope, registry: &HumanViewRegistry) -> String {
    let system = envelope
        .metadata
        .as_ref()
        .map(|metadata| metadata.system.as_str())
        .unwrap_or_default();
    render_human_with_registry_for_schema(envelope, registry, system)
}

/// Renders an envelope using registry entries for a specific schema id.
///
/// Shows every column of the registered view. Use
/// [`render_human_with_registry_selected`] to narrow the columns to a field
/// selection.
#[must_use]
pub fn render_human_with_registry_for_schema(
    envelope: &Envelope,
    registry: &HumanViewRegistry,
    schema_id: &str,
) -> String {
    render_human_with_registry_selected(envelope, registry, schema_id, "")
}

/// Renders an envelope using a registered view, narrowed to `fields`.
///
/// `fields` uses the same comma-separated syntax as `--fields`: an empty
/// string, `all`, or `*` keeps every column; otherwise only the view columns
/// whose `field` is listed are shown. A custom view renderer receives the full
/// data and ignores `fields`.
#[must_use]
pub fn render_human_with_registry_selected(
    envelope: &Envelope,
    registry: &HumanViewRegistry,
    schema_id: &str,
    fields: &str,
) -> String {
    if let Some(error) = &envelope.error {
        return format!("Error: {}\n", error.message);
    }
    if let Some(data) = &envelope.data
        && let Some(custom) = registry.custom(schema_id)
    {
        return custom.render(data);
    }
    match registry.columns(schema_id) {
        Some(columns) => {
            let selected = select_columns(columns, fields);
            render_human_with_view(envelope, Some(&selected), fields)
        }
        None => render_human_with_view(envelope, None, fields),
    }
}

/// Narrows and reorders view columns to a `--fields`-style selection. An
/// empty string, `all`, or `*` keeps every column in its declared order;
/// otherwise columns are chosen and ordered by the comma-separated list
/// (deduplicated, first occurrence wins) — a name with no matching column is
/// silently skipped, so a view still only ever shows its own declared
/// fields.
fn select_columns(columns: &[TableColumn], fields: &str) -> Vec<TableColumn> {
    let fields = fields.trim();
    if fields.is_empty() || fields == "all" || fields == "*" {
        return columns.to_vec();
    }
    let mut seen = BTreeSet::new();
    fields
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty() && seen.insert(*part))
        .filter_map(|name| columns.iter().find(|column| column.field == name).cloned())
        .collect()
}

/// Renders an envelope using explicit table columns.
///
/// `columns`, when `Some`, is expected to already be `--fields`-selected and
/// ordered (applied by callers such as
/// [`render_human_with_registry_selected`] before this function runs) — this
/// function does not re-apply `fields` to it. `fields` is only read here when
/// `columns` is `None`, to give the dynamically-derived, no-view column
/// catalog the same field selection and order a view would have gotten. Pass
/// `""` when no field-selection value is available.
#[must_use]
pub fn render_human_with_view(
    envelope: &Envelope,
    columns: Option<&[TableColumn]>,
    fields: &str,
) -> String {
    // Errors render on their own; success output gets the data body plus, when
    // present, a "Next steps:" footer built from the envelope's next_actions
    // (these otherwise appear only in JSON/TOON).
    if let Some(error) = &envelope.error {
        return format!("Error: {}\n", error.message);
    }
    let available_width = terminal_width();
    let (mut body, notes) = match &envelope.data {
        None => ("(no data)\n".to_owned(), RenderNotes::default()),
        Some(data) => render_data_body(data, columns, fields, available_width),
    };
    // Footers are appended in place: the common no-footer path leaves `body`
    // untouched (no realloc/copy), and non-empty content is written directly
    // into it (no per-footer temporaries).
    append_render_notes(&mut body, &notes);
    append_next_actions(&mut body, &envelope.next_actions);
    body
}

/// Render just the data portion of a success envelope (no next-steps footer).
fn render_data_body(
    data: &Value,
    columns: Option<&[TableColumn]>,
    fields: &str,
    available_width: usize,
) -> (String, RenderNotes) {
    if let Some(columns) = columns {
        return match data {
            Value::Array(items) => render_array_with_columns(items, columns, available_width),
            Value::Object(map) => (
                render_object_with_columns(map, columns),
                RenderNotes::default(),
            ),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                (format!("{}\n", format_value(data)), RenderNotes::default())
            }
        };
    }
    match data {
        Value::Array(items) => render_array(items, fields, available_width),
        Value::Object(map) => {
            if map.is_empty() {
                return ("(no data)\n".to_owned(), RenderNotes::default());
            }
            let columns = dynamic_columns(fields, || map.keys().cloned().collect());
            (
                render_object_with_columns(map, &columns),
                RenderNotes::default(),
            )
        }
        other => (
            format!("{}\n", format_plain_value(other)),
            RenderNotes::default(),
        ),
    }
}

/// Builds the column catalog for data with no registered view: when `fields`
/// names specific fields (not empty/`all`/`*`), columns are derived from that
/// list, in the order given (deduplicated) — the same order source a
/// registered view's `--fields` selection uses (see [`select_columns`]).
/// Otherwise falls back to `natural_keys()` sorted alphabetically, since a
/// bare JSON object has no other order signal to offer.
fn dynamic_columns(fields: &str, natural_keys: impl FnOnce() -> Vec<String>) -> Vec<TableColumn> {
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

/// Appends footer hints for truncated cells and/or hidden columns to `out`
/// (a no-op when neither happened). Mirrors `append_next_actions`: writes
/// directly into `out` rather than building a separate string.
fn append_render_notes(out: &mut String, notes: &RenderNotes) {
    if notes.truncated {
        out.push_str(
            "\nOutput truncated to fit the terminal — use --fields to show fewer columns, or --json for full values.\n",
        );
    }
    if !notes.hidden_columns.is_empty() {
        out.push_str(&format!(
            "\n{} column{} hidden to fit the terminal ({}) — use --fields to choose columns, or --json for full output.\n",
            notes.hidden_columns.len(),
            if notes.hidden_columns.len() == 1 { "" } else { "s" },
            notes.hidden_columns.join(", "),
        ));
    }
}

/// Append a "Next steps:" footer listing suggested follow-up commands to `out`
/// (a no-op when there are none). Each action shows its command template with
/// any known param values substituted into their `<placeholder>` (params
/// without a known value, e.g. required-only hints, are shown as-is), followed
/// by the description beneath it. Writes directly into `out` to avoid
/// per-action temporaries.
fn append_next_actions(out: &mut String, actions: &[NextAction]) {
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
const NO_TRUNCATE_MAX_WIDTH: usize = 4096;

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

/// Signals produced while rendering a table body, used to build human-output
/// footer hints. `Default` means nothing was hidden or shortened.
#[derive(Default)]
struct RenderNotes {
    /// Whether any cell was shortened to fit the terminal.
    truncated: bool,
    /// Headers of columns dropped entirely because there wasn't room for
    /// them, in their original declared/requested order (the order they
    /// would have appeared in the table, had they fit) — not reverse
    /// priority order.
    hidden_columns: Vec<String>,
}

/// Chooses how many leading columns (priority order, most important first),
/// each contributing at least `min_widths[i]`, fit in `available_width` — so
/// lower-priority trailing columns can be dropped when the terminal is too
/// narrow for all of them. `min_widths[i]` should be the column's header
/// length for a column that can still shrink, or its full natural width for
/// one that can't (e.g. `no_truncate`) — using a shrinkable column's header
/// length here lets it still be counted as fitting even though its eventual
/// rendered width may be larger. Always keeps at least one column, even if
/// it alone exceeds `available_width`.
fn columns_fitting_width(min_widths: &[usize], available_width: usize) -> usize {
    let mut used = 0_usize;
    let mut kept = 0_usize;
    for (index, &min_width) in min_widths.iter().enumerate() {
        let gutter = if index == 0 { 0 } else { COLUMN_GUTTER };
        let next_used = used + gutter + min_width;
        if next_used > available_width && kept > 0 {
            break;
        }
        used = next_used;
        kept += 1;
    }
    kept
}

/// Fits `natural` (fully-untruncated) column widths into `available_width`.
///
/// `no_truncate` columns are never shrunk (they keep their natural width
/// unconditionally — that's the whole point of the flag) and their width is
/// reserved out of the budget up front. The remaining columns are never
/// shrunk below their header length, and share whatever budget is left
/// beyond that, smallest-need-first, so a column that wants only a little
/// gets exactly that instead of an equal-but-wasteful split.
///
/// Returns the fitted widths and whether any truncatable column ended up
/// narrower than its natural width (i.e. some cell will actually be cut).
fn fit_column_widths(
    headers: &[usize],
    natural: &[usize],
    no_truncate: &[bool],
    available_width: usize,
) -> (Vec<usize>, bool) {
    let mut widths = natural.to_vec();
    let truncatable: Vec<usize> = (0..no_truncate.len())
        .filter(|&index| !no_truncate[index])
        .collect();
    if truncatable.is_empty() {
        return (widths, false);
    }
    let gutters = COLUMN_GUTTER * headers.len().saturating_sub(1);
    let reserved: usize = (0..no_truncate.len())
        .filter(|&index| no_truncate[index])
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

fn render_array_with_columns(
    items: &[Value],
    columns: &[TableColumn],
    available_width: usize,
) -> (String, RenderNotes) {
    if items.is_empty() {
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
                        .and_then(|map| map.get(&column.field))
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
    let kept = columns_fitting_width(&min_widths, available_width);
    let hidden_columns = columns[kept..]
        .iter()
        .map(|column| column.header.clone())
        .collect::<Vec<_>>();
    let columns = &columns[..kept];
    let header_lens = &header_lens[..kept];
    let natural = &natural[..kept];
    let no_truncate = &no_truncate_all[..kept];
    let rows: Vec<Vec<String>> = rows.into_iter().map(|row| row[..kept].to_vec()).collect();

    let (fitted, truncated) = fit_column_widths(header_lens, natural, no_truncate, available_width);
    let table = render_table(
        &columns
            .iter()
            .map(|column| column.header.clone())
            .collect::<Vec<_>>(),
        &fitted,
        &rows,
    );
    (
        table,
        RenderNotes {
            truncated,
            hidden_columns,
        },
    )
}

fn render_object_with_columns(
    map: &serde_json::Map<String, Value>,
    columns: &[TableColumn],
) -> String {
    if map.is_empty() {
        return "(no data)\n".to_owned();
    }
    let mut out = String::new();
    for column in columns {
        let value = map
            .get(&column.field)
            .map_or_else(String::new, format_value);
        out.push_str(&format!("{}: {value}\n", column.header));
    }
    out
}

fn render_array(items: &[Value], fields: &str, available_width: usize) -> (String, RenderNotes) {
    if items.is_empty() {
        return ("(no results)\n".to_owned(), RenderNotes::default());
    }
    let Some(Value::Object(first_map)) = items.first() else {
        return (render_array_lines(items), RenderNotes::default());
    };
    if !items.iter().all(Value::is_object) {
        return (render_array_lines(items), RenderNotes::default());
    }
    let columns = dynamic_columns(fields, || first_map.keys().cloned().collect());
    render_array_with_columns(items, &columns, available_width)
}

fn render_array_lines(items: &[Value]) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(&format!("{}\n", format_plain_value(item)));
    }
    out
}

fn render_table(headers: &[String], widths: &[usize], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (index, header) in headers.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(&format!(
            "{:<width$}",
            header.to_uppercase(),
            width = widths[index]
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
            out.push_str(&format!(
                "{:<width$}",
                truncate(value, widths[index]),
                width = widths[index]
            ));
        }
        out.push('\n');
    }
    out.push_str(&format!("\n({} rows)\n", rows.len()));
    out
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "yes".to_owned(),
        Value::Bool(false) => "no".to_owned(),
        Value::Number(number) => format_number(number),
        Value::String(value) => value.clone(),
        Value::Array(items) => items
            .iter()
            .map(format_value)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(_) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned()),
    }
}

fn format_plain_value(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => format_number(number),
        Value::String(value) => value.clone(),
        Value::Array(items) => {
            let values = items
                .iter()
                .map(format_plain_value)
                .collect::<Vec<_>>()
                .join(" ");
            format!("[{values}]")
        }
        Value::Object(object) => {
            let mut pairs = object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>();
            pairs.sort_by(|left, right| left.0.cmp(&right.0));
            let object = pairs
                .into_iter()
                .collect::<serde_json::Map<String, Value>>();
            serde_json::to_string(&Value::Object(object)).unwrap_or_else(|_| "{}".to_owned())
        }
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.len() <= width {
        return value.to_owned();
    }
    if width <= 3 {
        return value.chars().take(width).collect();
    }
    let mut out = value.chars().take(width - 3).collect::<String>();
    out.push_str("...");
    out
}

fn format_number(number: &serde_json::Number) -> String {
    number.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    }

    #[test]
    fn no_truncate_column_keeps_long_values_intact() {
        let long_url = "https://example.com/legal/agreements/registration-agreement-v2";
        assert!(long_url.len() > 40, "fixture must exceed the default cap");
        let items = vec![json!({ "title": long_url, "url": long_url })];
        let columns = vec![
            TableColumn::new("title", "Title"),
            TableColumn::new("url", "URL").no_truncate(true),
        ];

        // Just enough room for the URL in full plus a somewhat-shrunk title.
        let (out, notes) = render_array_with_columns(&items, &columns, 80);

        assert!(
            out.contains("..."),
            "default column should still truncate: {out}"
        );
        assert!(
            out.contains(long_url),
            "no_truncate column must keep the full value: {out}"
        );
        assert!(notes.truncated, "title column was shortened: {out}");
    }

    #[test]
    fn no_truncate_column_still_caps_pathologically_long_values() {
        let huge_value = "x".repeat(NO_TRUNCATE_MAX_WIDTH * 2);
        let items = vec![json!({ "url": huge_value })];
        let columns = vec![TableColumn::new("url", "URL").no_truncate(true)];

        let (out, _notes) = render_array_with_columns(&items, &columns, 80);

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
        let (out, _notes) = render_array_with_columns(&items, &columns, 10);
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

        let (out, notes) = render_array_with_columns(&items, &columns, 200);

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
        let description = "a description that is well past the old forty-character cap";
        let items = vec![json!({ "id": "1", "description": description })];
        let columns = vec![
            TableColumn::new("id", "ID"),
            TableColumn::new("description", "Description"),
        ];

        let (out, notes) = render_array_with_columns(&items, &columns, 30);

        assert!(
            notes.truncated,
            "narrow terminal must shorten a cell: {out}"
        );
        assert!(
            notes.hidden_columns.is_empty(),
            "both columns still fit: {out}"
        );
        assert!(out.contains("..."), "{out}");
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

        let (out, notes) = render_array_with_columns(&items, &columns, 10);

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

        assert!(out.contains("hidden to fit the terminal"), "{out}");
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

        let rendered =
            render_human_with_registry_selected(&envelope, &registry, "things", "status,id");

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
        let (out, notes) = render_array_with_columns(&items, &columns, 42);

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
}
