use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{OnceLock, RwLock},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Compact field summary used in help text and schema output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FieldInfo {
    /// Field name.
    pub name: String,
    /// Field type label.
    #[serde(rename = "type")]
    pub field_type: String,
    /// Whether the field is optional or nullable.
    pub optional: bool,
}

impl FieldInfo {
    /// Creates a compact field summary.
    #[must_use]
    pub fn new(name: impl Into<String>, field_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            field_type: field_type.into(),
            optional: false,
        }
    }

    /// Marks the field as optional or nullable.
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
}

/// Schema information returned by `--schema`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchemaInfo {
    /// Colon-separated command path.
    pub command: String,
    /// Compact field summary for help and quick inspection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldInfo>,
    /// Full JSON Schema when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

impl SchemaInfo {
    /// Creates an empty schema record for a command path.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            fields: Vec::new(),
            schema: None,
        }
    }

    /// Adds compact field summaries.
    #[must_use]
    pub fn with_fields(mut self, fields: impl Into<Vec<FieldInfo>>) -> Self {
        self.fields = fields.into();
        self
    }

    /// Adds a full JSON Schema document.
    #[must_use]
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.schema = Some(schema);
        self
    }
}

/// Builds the `--schema` response body for a command with no registered schema.
///
/// `--schema` must never run the command, so when no schema exists we report
/// that rather than executing. The body mirrors the `{ command, fields }` shape
/// of a real [`SchemaInfo`] response (with an empty `fields` array) and adds a
/// `message`, so callers can parse the schema and no-schema responses with one
/// code path. Shared by the middleware and the `Cli::run` `--schema` bypass so
/// both paths emit an identical body.
pub(crate) fn no_schema_response(command_path: &str) -> Value {
    serde_json::json!({
        "command": command_path,
        "fields": [],
        "message": "No output schema is registered for this command.",
    })
}

/// Manual output field descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputField {
    /// Field name.
    pub name: &'static str,
    /// Field type label.
    pub field_type: &'static str,
    /// Whether the field is optional.
    pub optional: bool,
}

impl OutputField {
    /// Creates a field descriptor.
    #[must_use]
    pub const fn new(name: &'static str, field_type: &'static str) -> Self {
        Self {
            name,
            field_type,
            optional: false,
        }
    }

    /// Marks the field optional.
    #[must_use]
    pub const fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// Creates a string field.
    #[must_use]
    pub const fn string(name: &'static str) -> Self {
        Self::new(name, "string")
    }

    /// Creates an integer field.
    #[must_use]
    pub const fn int(name: &'static str) -> Self {
        Self::new(name, "int")
    }

    /// Creates a float field.
    #[must_use]
    pub const fn float(name: &'static str) -> Self {
        Self::new(name, "float")
    }

    /// Creates a boolean field.
    #[must_use]
    pub const fn bool(name: &'static str) -> Self {
        Self::new(name, "bool")
    }

    /// Creates a list field with a custom type label.
    #[must_use]
    pub const fn list(name: &'static str, field_type: &'static str) -> Self {
        Self::new(name, field_type)
    }

    /// Creates a string-list field.
    #[must_use]
    pub const fn string_list(name: &'static str) -> Self {
        Self::new(name, "[]string")
    }

    /// Creates an integer-list field.
    #[must_use]
    pub const fn int_list(name: &'static str) -> Self {
        Self::new(name, "[]int")
    }

    /// Creates a float-list field.
    #[must_use]
    pub const fn float_list(name: &'static str) -> Self {
        Self::new(name, "[]float")
    }

    /// Creates a boolean-list field.
    #[must_use]
    pub const fn bool_list(name: &'static str) -> Self {
        Self::new(name, "[]bool")
    }

    /// Creates an object-list field.
    #[must_use]
    pub const fn object_list(name: &'static str) -> Self {
        Self::new(name, "[]object")
    }

    /// Creates an object field.
    #[must_use]
    pub const fn object(name: &'static str) -> Self {
        Self::new(name, "object")
    }

    /// Creates a field with unknown or mixed type.
    #[must_use]
    pub const fn any(name: &'static str) -> Self {
        Self::new(name, "any")
    }
}

/// Trait for manually declared output schemas.
pub trait OutputSchema {
    /// Returns the schema's compact field descriptors.
    fn fields() -> &'static [OutputField];
}

/// Registry for command output schemas.
#[derive(Clone, Debug, Default)]
pub struct SchemaRegistry {
    by_path: BTreeMap<String, SchemaInfo>,
}

impl SchemaRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a manual schema for a command path.
    pub fn register<T: OutputSchema>(&mut self, command_path: impl Into<String>) {
        self.register_fields(command_path, fields_for::<T>());
    }

    /// Registers a `schemars` JSON Schema for a command path.
    pub fn register_json_schema<T: JsonSchema>(&mut self, command_path: impl Into<String>) {
        let command_path = command_path.into();
        self.register_info(command_path.clone(), json_schema_info::<T>(command_path));
    }

    /// Registers compact field summaries for a command path.
    pub fn register_fields(
        &mut self,
        command_path: impl Into<String>,
        fields: impl Into<Vec<FieldInfo>>,
    ) {
        let command_path = command_path.into();
        self.register_info(
            command_path.clone(),
            SchemaInfo {
                command: command_path,
                fields: fields.into(),
                schema: None,
            },
        );
    }

    /// Registers a complete schema record for a command path.
    pub fn register_info(&mut self, command_path: impl Into<String>, mut info: SchemaInfo) {
        let command_path = command_path.into();
        info.command = command_path.clone();
        self.by_path.insert(command_path, info);
    }

    /// Merges another registry into this one.
    pub fn merge(&mut self, other: &Self) {
        self.by_path.extend(other.by_path.clone());
    }

    /// Looks up schema information by colon-separated or space-separated path.
    #[must_use]
    pub fn get_by_path(&self, command_path: &str) -> Option<SchemaInfo> {
        if let Some(info) = self.by_path.get(command_path) {
            return Some(schema_with_command(info, command_path));
        }

        let space_path = command_path.replace(':', " ");
        self.by_path.iter().find_map(|(registered, info)| {
            let matches = registered == &space_path
                || registered
                    .split_once(' ')
                    .is_some_and(|(_, without_root)| without_root == space_path);
            matches.then(|| schema_with_command(info, command_path))
        })
    }
}

fn schema_with_command(info: &SchemaInfo, command_path: &str) -> SchemaInfo {
    SchemaInfo {
        command: command_path.to_owned(),
        fields: info.fields.clone(),
        schema: info.schema.clone(),
    }
}

/// Converts an [`OutputSchema`] implementation to compact field info.
#[must_use]
pub fn fields_for<T: OutputSchema>() -> Vec<FieldInfo> {
    T::fields()
        .iter()
        .map(|field| FieldInfo {
            name: field.name.to_owned(),
            field_type: field.field_type.to_owned(),
            optional: field.optional,
        })
        .collect()
}

/// Generates JSON Schema for a Rust type.
#[must_use]
pub fn json_schema_for<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

/// Builds schema info from a Rust type's JSON Schema.
#[must_use]
pub fn json_schema_info<T: JsonSchema>(command_path: impl Into<String>) -> SchemaInfo {
    let schema = json_schema_for::<T>();
    SchemaInfo {
        command: command_path.into(),
        fields: fields_from_json_schema(&schema),
        schema: Some(schema),
    }
}

/// Extracts compact field summaries from a JSON Schema object.
#[must_use]
pub fn fields_from_json_schema(schema: &Value) -> Vec<FieldInfo> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();

    properties
        .iter()
        .map(|(name, property)| FieldInfo {
            name: name.clone(),
            field_type: json_schema_type_name(property, schema),
            optional: !required.contains(name.as_str()) || schema_allows_null(property),
        })
        .collect()
}

fn json_schema_type_name(schema: &Value, root: &Value) -> String {
    let schema = non_null_schema(schema);
    let schema = resolve_local_ref(schema, root).unwrap_or(schema);
    match primary_json_type(schema).as_deref() {
        Some("string") => "string".to_owned(),
        Some("integer") => "int".to_owned(),
        Some("number") => "float".to_owned(),
        Some("boolean") => "bool".to_owned(),
        Some("array") => {
            let item_type = schema
                .get("items")
                .map(|items| json_schema_type_name(items, root))
                .unwrap_or_else(|| "any".to_owned());
            format!("[]{item_type}")
        }
        Some("object") => "object".to_owned(),
        Some(other) => other.to_owned(),
        None => {
            if schema.get("properties").is_some() {
                "object".to_owned()
            } else {
                "any".to_owned()
            }
        }
    }
}

fn non_null_schema(schema: &Value) -> &Value {
    for key in ["anyOf", "oneOf"] {
        if let Some(items) = schema.get(key).and_then(Value::as_array)
            && let Some(non_null) = items
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) != Some("null"))
        {
            return non_null;
        }
    }
    schema
}

fn resolve_local_ref<'schema>(
    schema: &'schema Value,
    root: &'schema Value,
) -> Option<&'schema Value> {
    let reference = schema.get("$ref").and_then(Value::as_str)?;
    let pointer = reference.strip_prefix('#')?;
    root.pointer(pointer)
}

fn primary_json_type(schema: &Value) -> Option<String> {
    match schema.get("type") {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .find(|value| *value != "null")
            .map(str::to_owned),
        Some(Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_)) | None => None,
    }
}

fn schema_allows_null(schema: &Value) -> bool {
    matches!(schema.get("type"), Some(Value::String(value)) if value == "null")
        || schema
            .get("type")
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some("null")))
        || ["anyOf", "oneOf"].iter().any(|key| {
            schema
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item.get("type").and_then(Value::as_str) == Some("null"))
                })
        })
}

static GLOBAL_SCHEMA_REGISTRY: OnceLock<RwLock<SchemaRegistry>> = OnceLock::new();

fn global_schema_registry() -> &'static RwLock<SchemaRegistry> {
    GLOBAL_SCHEMA_REGISTRY.get_or_init(|| RwLock::new(SchemaRegistry::new()))
}

/// Registers a process-global manual schema.
pub fn register_global_schema<T: OutputSchema>(command_path: impl Into<String>) {
    let mut registry = global_schema_registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.register::<T>(command_path);
}

/// Registers a process-global JSON Schema.
pub fn register_global_json_schema<T: JsonSchema>(command_path: impl Into<String>) {
    let mut registry = global_schema_registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.register_json_schema::<T>(command_path);
}

/// Registers process-global compact field summaries.
pub fn register_global_schema_fields(
    command_path: impl Into<String>,
    fields: impl Into<Vec<FieldInfo>>,
) {
    let mut registry = global_schema_registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.register_fields(command_path, fields);
}

/// Registers process-global schema info.
pub fn register_global_schema_info(command_path: impl Into<String>, info: SchemaInfo) {
    let mut registry = global_schema_registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.register_info(command_path, info);
}

/// Looks up a process-global schema by command path.
#[must_use]
pub fn get_global_schema_by_path(command_path: &str) -> Option<SchemaInfo> {
    global_schema_registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_by_path(command_path)
}

/// Returns a snapshot of the process-global schema registry.
#[must_use]
pub fn global_schema_registry_snapshot() -> SchemaRegistry {
    global_schema_registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Formats compact field summaries for command long help.
#[must_use]
pub fn format_help_section(fields: &[FieldInfo]) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let max_name = fields
        .iter()
        .map(|field| field.name.len())
        .max()
        .unwrap_or_default();
    let mut out = String::from("Output fields:\n");
    for field in fields {
        let optional = if field.optional { "  (optional)" } else { "" };
        out.push_str(&format!(
            "  {:<width$}  {}{}\n",
            field.name,
            field.field_type,
            optional,
            width = max_name
        ));
    }

    let first_string = fields
        .iter()
        .find(|field| field.field_type == "string")
        .map(|field| field.name.as_str());
    let first_bool = fields
        .iter()
        .find(|field| field.field_type == "bool")
        .map(|field| field.name.as_str());
    if first_string.is_some() || first_bool.is_some() {
        out.push_str("\nFilter examples:\n");
        if let Some(name) = first_string {
            out.push_str(&format!("  --filter \"contains({name}, 'example')\"\n"));
        }
        if let Some(name) = first_bool {
            out.push_str(&format!("  --filter '{name}'\n"));
        }
    }

    out.push_str("\nExpr examples:\n");
    out.push_str("  --expr 'length(@)'\n");
    if let Some(name) = first_string {
        out.push_str(&format!("  --expr '[].{name}'\n"));
    }
    out
}
