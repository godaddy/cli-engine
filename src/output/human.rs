use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, OnceLock, RwLock},
};

use serde_json::Value;

use super::{Envelope, NextAction};

/// Column definition for registered human table views.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableColumn {
    /// JSON field path.
    pub field: String,
    /// Display header.
    pub header: String,
}

impl TableColumn {
    /// Creates a table column from a JSON field path and display header.
    #[must_use]
    pub fn new(field: impl Into<String>, header: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            header: header.into(),
        }
    }
}

/// Human view definition keyed by schema id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanViewDef {
    /// Schema id, usually the command path.
    pub schema_id: String,
    /// Columns rendered for matching object or list data.
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
#[must_use]
pub fn render_human(envelope: &Envelope) -> String {
    render_human_with_view(envelope, None)
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
            render_human_with_view(envelope, Some(&selected))
        }
        None => render_human_with_view(envelope, None),
    }
}

/// Narrows view columns to a `--fields`-style selection. An empty string,
/// `all`, or `*` keeps every column; otherwise a column survives when its
/// `field` appears in the comma-separated list.
fn select_columns(columns: &[TableColumn], fields: &str) -> Vec<TableColumn> {
    let fields = fields.trim();
    if fields.is_empty() || fields == "all" || fields == "*" {
        return columns.to_vec();
    }
    let allowed: BTreeSet<&str> = fields
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    columns
        .iter()
        .filter(|column| allowed.contains(column.field.as_str()))
        .cloned()
        .collect()
}

/// Renders an envelope using explicit table columns.
#[must_use]
pub fn render_human_with_view(envelope: &Envelope, columns: Option<&[TableColumn]>) -> String {
    // Errors render on their own; success output gets the data body plus, when
    // present, a "Next steps:" footer built from the envelope's next_actions
    // (these otherwise appear only in JSON/TOON).
    if let Some(error) = &envelope.error {
        return format!("Error: {}\n", error.message);
    }
    let mut body = match &envelope.data {
        None => "(no data)\n".to_owned(),
        Some(data) => render_data_body(data, columns),
    };
    // Append the footer in place: the common no-footer path leaves `body`
    // untouched (no realloc/copy), and non-empty actions are written directly
    // into it (no per-action temporaries).
    append_next_actions(&mut body, &envelope.next_actions);
    body
}

/// Render just the data portion of a success envelope (no next-steps footer).
fn render_data_body(data: &Value, columns: Option<&[TableColumn]>) -> String {
    if let Some(columns) = columns {
        return match data {
            Value::Array(items) => render_array_with_columns(items, columns),
            Value::Object(map) => render_object_with_columns(map, columns),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                format!("{}\n", format_value(data))
            }
        };
    }
    match data {
        Value::Array(items) => render_array(items),
        Value::Object(map) => {
            if map.is_empty() {
                "(no data)\n".to_owned()
            } else {
                let mut keys = map.keys().collect::<Vec<_>>();
                keys.sort();
                let mut out = String::new();
                for key in keys {
                    out.push_str(&format!("{key}: {}\n", format_value(&map[key])));
                }
                out
            }
        }
        other => format!("{}\n", format_plain_value(other)),
    }
}

/// Append a "Next steps:" footer listing suggested follow-up commands to `out`
/// (a no-op when there are none). Each action shows its command template
/// (placeholders like `<domain>` shown as-is) with the description beneath it.
/// Writes directly into `out` to avoid per-action temporaries.
fn append_next_actions(out: &mut String, actions: &[NextAction]) {
    if actions.is_empty() {
        return;
    }
    out.push_str("\nNext steps:\n");
    for action in actions {
        out.push_str("  ");
        out.push_str(&action.command);
        out.push_str("\n      ");
        out.push_str(&action.description);
        out.push('\n');
    }
}

fn render_array_with_columns(items: &[Value], columns: &[TableColumn]) -> String {
    if items.is_empty() {
        return "(no results)\n".to_owned();
    }
    if !items.iter().all(Value::is_object) {
        return render_array_lines(items);
    }
    let mut widths = columns
        .iter()
        .map(|column| column.header.len())
        .collect::<Vec<_>>();
    let rows = items
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
                    widths[index] = widths[index].max(value.len()).min(40);
                    value
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    render_table(
        &columns
            .iter()
            .map(|column| column.header.clone())
            .collect::<Vec<_>>(),
        &widths,
        &rows,
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

fn render_array(items: &[Value]) -> String {
    if items.is_empty() {
        return "(no results)\n".to_owned();
    }
    let Some(first) = items.first() else {
        return "(no results)\n".to_owned();
    };
    let Value::Object(first_map) = first else {
        return render_array_lines(items);
    };
    if !items.iter().all(Value::is_object) {
        return render_array_lines(items);
    }
    let mut cols = first_map.keys().cloned().collect::<Vec<_>>();
    cols.sort();
    if cols.is_empty() {
        return "(no results)\n".to_owned();
    }
    let mut widths = cols.iter().map(String::len).collect::<Vec<_>>();
    let rows = items
        .iter()
        .map(|item| {
            cols.iter()
                .enumerate()
                .map(|(index, col)| {
                    let value = item
                        .as_object()
                        .and_then(|map| map.get(col))
                        .map_or_else(String::new, format_value);
                    widths[index] = widths[index].max(value.len()).min(40);
                    value
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    render_table(&cols, &widths, &rows)
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
}
