//! Structured output envelopes and renderers.
//!
//! Command handlers return JSON-serializable data and a backend system id. The
//! middleware wraps that data in an [`Envelope`], applies filtering, pagination,
//! JMESPath expressions, field projection, and then renders the result as JSON,
//! human text, or TOON.
//!
//! JSON is the default for the Rust crate. Human output is intended for terminal
//! readability, and TOON remains available as an explicit migration format.

mod envelope;
mod fields;
mod human;
mod json;
mod pipeline;
mod renderer;
mod schema;
mod toon;

pub use crate::error::{DetailedError, ExitCoder, exit_code_for_error, exit_code_for_exit_coder};
pub use envelope::{
    Envelope, ErrorEnvelope, Metadata, NextAction, NextActionParam, PaginationMeta,
    build_detailed_error_envelope, build_error_envelope,
};
pub use fields::{FieldTree, filter_fields, parse_fields};
pub use human::{
    HumanViewDef, HumanViewFn, HumanViewRegistry, HumanViewRenderer, TableColumn,
    global_human_view_registry_snapshot, lookup_global_human_view_columns,
    lookup_global_human_view_func, register_global_human_view, register_global_human_view_func,
    render_human, render_human_with_registry, render_human_with_registry_for_schema,
    render_human_with_registry_selected, render_human_with_view,
};
pub use json::render_json;
pub use pipeline::{PipelineOpts, apply_pipeline};
pub use renderer::{
    OutputFormat, RendererFactory, is_valid_output_format, render, render_data, render_data_format,
    render_detailed_error, render_detailed_error_format, render_error, render_error_format,
    render_format, write_render,
};
pub(crate) use schema::no_schema_response;
pub use schema::{
    FieldInfo, OutputField, OutputSchema, SchemaInfo, SchemaRegistry, fields_for,
    fields_from_json_schema, format_help_section, get_global_schema_by_path,
    global_schema_registry_snapshot, json_schema_for, json_schema_info,
    register_global_json_schema, register_global_schema, register_global_schema_fields,
    register_global_schema_info,
};
pub use toon::render_toon;
