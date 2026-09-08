use std::{
    fmt,
    sync::{Arc, OnceLock, RwLock},
};

use serde_json::Value;

use super::Envelope;

mod body;
mod columns;
mod footer;
#[cfg(test)]
mod tests;
mod value_format;

use body::render_data_body;
use footer::{append_next_actions, append_pagination_summary, append_render_notes};

pub(crate) use columns::terminal_width;

/// Column text alignment for the human table view.
///
/// Only affects the array/table rendering path (`render_array_with_columns`
/// via `render_table`) — property-bag rendering (`render_object_with_columns`)
/// prints `header: value` with no column widths to align, so alignment is a
/// no-op there.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Alignment {
    /// Left-aligned (the default) — appropriate for text-like columns.
    #[default]
    Left,
    /// Right-aligned — use for numeric/price columns so values line up on
    /// their least-significant digit.
    Right,
}

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
///
/// Construct with [`TableColumn::new`], then chain builder methods like
/// [`no_truncate`](TableColumn::no_truncate)/[`nested`](TableColumn::nested)
/// — never as a struct literal. No known consumer constructs `TableColumn`
/// via struct literal, so marking it `#[non_exhaustive]` carries no real
/// breaking impact today; going forward it means the engine can add fields
/// (as it did for `nested`) without that becoming a breaking release either.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TableColumn {
    /// JSON field path. Supports simple dotted paths to reach a value nested
    /// under intermediate objects, so a column can point through a wrapper
    /// shape (a pagination envelope, a `Summary<T>`, etc.). A literal field
    /// name containing a `.` is not supported — this mirrors the dotted-path
    /// convention `crate::output::fields` already uses for `--fields`
    /// projection.
    pub field: String,
    /// Display header.
    pub header: String,
    /// When true, this column's values are never shrunk to fit the terminal
    /// (still capped at `NO_TRUNCATE_MAX_WIDTH` to bound pathologically long
    /// values). Use this for values that are useless when cut short, such as
    /// URLs.
    pub no_truncate: bool,
    /// When set, and the resolved value is list-of-objects or object shaped,
    /// this column renders as an indented child table or child property bag
    /// instead of a one-line dump — see [`TableColumn::nested`]. `None` (the
    /// default from [`TableColumn::new`]) is a complete no-op: rendering is
    /// identical to a column with no opinion about nesting.
    pub nested: Option<Vec<TableColumn>>,
    /// Header and cell text alignment — see [`TableColumn::align`].
    pub align: Alignment,
}

impl TableColumn {
    /// Creates a table column from a JSON field path and display header.
    #[must_use]
    pub fn new(field: impl Into<String>, header: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            header: header.into(),
            no_truncate: false,
            nested: None,
            align: Alignment::Left,
        }
    }

    /// Opts this column out of terminal-width-driven shrinking. Values are
    /// still capped at `NO_TRUNCATE_MAX_WIDTH`.
    #[must_use]
    pub fn no_truncate(mut self, value: bool) -> Self {
        self.no_truncate = value;
        self
    }

    /// Sets this column's header and cell alignment. Defaults to
    /// `Alignment::Left`; use `Alignment::Right` for numeric or price
    /// columns so decimal points and digits line up instead of looking
    /// ragged on the left.
    #[must_use]
    pub fn align(mut self, alignment: Alignment) -> Self {
        self.align = alignment;
        self
    }

    /// Opts this column into rendering a nested list/object value as an
    /// indented child table or property bag, using `columns` as that child's
    /// own column definitions (which may themselves set `.nested(...)`).
    ///
    /// Nesting is only consulted when this column is rendered inside an
    /// object property bag (top-level, or itself a nested property bag) — a
    /// row cell inside an array-of-objects table always renders as a single
    /// flat value, ignoring `nested`, because a table row is one monospace
    /// line and can't itself contain a rendered sub-block without breaking
    /// column alignment. Recursion is otherwise unbounded through the object
    /// chain: a nested column's own child columns may set `.nested(...)`
    /// again for a grandchild table or property bag.
    #[must_use]
    pub fn nested(mut self, columns: impl Into<Vec<TableColumn>>) -> Self {
        self.nested = Some(columns.into());
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
    by_schema_id: std::collections::BTreeMap<String, Vec<TableColumn>>,
    custom_by_schema_id: std::collections::BTreeMap<String, HumanViewRenderer>,
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
    let mut seen = std::collections::BTreeSet::new();
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
        let mut out = format!("Error: {}\n", error.message);
        if let Some(fix) = &envelope.fix {
            out.push_str("Fix: ");
            out.push_str(fix);
            out.push('\n');
        }
        return out;
    }
    let available_width = terminal_width();
    let (mut body, notes) = match &envelope.data {
        None => ("(no data)\n".to_owned(), RenderNotes::default()),
        Some(data) => render_data_body(
            data,
            columns,
            fields,
            available_width,
            envelope.pagination.as_ref(),
        ),
    };
    // Footers are appended in place: the common no-footer path leaves `body`
    // untouched (no realloc/copy), and non-empty content is written directly
    // into it (no per-footer temporaries).
    append_render_notes(&mut body, &notes);
    if !notes.pagination_shown {
        // `envelope.data` already reflects the fully piped result (filter ->
        // paginate -> expr -> fields), so its length is what this non-table
        // path actually rendered — unlike `pagination.count`, which is only
        // the pre-`--expr` slice size and can go stale once `--expr` reshapes
        // the array (mirrors the same fix in `render_table`).
        let shown = envelope
            .data
            .as_ref()
            .and_then(Value::as_array)
            .and_then(|items| i64::try_from(items.len()).ok());
        append_pagination_summary(&mut body, envelope.pagination.as_ref(), shown);
    }
    append_next_actions(&mut body, &envelope.next_actions);
    body
}

/// Signals produced while rendering a table body, used to build human-output
/// footer hints. `Default` means nothing was hidden or shortened.
#[derive(Default)]
pub(crate) struct RenderNotes {
    /// Whether any cell was shortened to fit the terminal.
    pub(crate) truncated: bool,
    /// Headers of columns dropped entirely because there wasn't room for
    /// them, in their original declared/requested order (the order they
    /// would have appeared in the table, had they fit) — not reverse
    /// priority order.
    pub(crate) hidden_columns: Vec<String>,
    /// Whether any of the truncation/hiding captured above happened inside a
    /// nested child block (a `TableColumn::nested` column's own table or
    /// property bag) rather than at this level's own top-level columns.
    /// `--fields` only ever selects among top-level declared columns — it
    /// can drop a nested column entirely, but can't narrow what's shown
    /// *inside* one — so [`append_render_notes`] must not suggest `--fields`
    /// as a fix when this is set, even though `hidden_columns`/`truncated`
    /// are otherwise reported identically either way.
    pub(crate) nested_narrowing: bool,
    /// Whether the table footer already merged in the pagination summary
    /// (`render_table`'s `(N of M rows, offset O, limit L)` line) — so
    /// [`render_human_with_view`] doesn't also append the standalone
    /// `append_pagination_summary` line and duplicate the same facts.
    pub(crate) pagination_shown: bool,
}
