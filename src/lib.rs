//! Build consistent, domain-oriented CLIs with a small amount of Rust.
//!
//! `cli_engine` provides the shared pieces that most CLI tools need:
//! command registration, authentication provider routing, authorization hooks,
//! audit and activity hooks, structured output, output schemas, guides, search,
//! command tree rendering, and authenticated HTTP transport helpers.
//!
//! The intended shape is:
//!
//! 1. Each team owns one or more [`Module`] values.
//! 2. A module registers noun-based [`GroupSpec`] groups.
//! 3. Groups contain verb-like [`CommandSpec`] leaf commands.
//! 4. Command handlers stay focused on domain behavior while [`Middleware`]
//!    handles authentication, dry-run, audit, activity, output, and errors.
//!
//! # Quick Start
//!
//! ```no_run
//! use clap::Arg;
//! use cli_engine::{
//!     BuildInfo, Cli, CliConfig, CommandSpec, GroupSpec, Module,
//!     RuntimeCommandSpec, RuntimeGroupSpec,
//! };
//! use serde_json::json;
//!
//! #[tokio::main]
//! async fn main() -> std::process::ExitCode {
//!     let list = RuntimeCommandSpec::new(
//!         CommandSpec::new("list", "List projects")
//!             .with_system("projects-api")
//!             .with_default_fields("id,name,status")
//!             .with_arg(Arg::new("team").long("team").required(true))
//!             .no_auth(true),
//!         async |_credential, args| {
//!             let team = args
//!                 .get("team")
//!                 .and_then(|value| value.as_str())
//!                 .unwrap_or_default();
//!             Ok(cli_engine::CommandResult::new(json!([
//!                 { "id": "p1", "name": "Portal", "status": "active", "team": team }
//!             ])))
//!         },
//!     );
//!
//!     let module = Module::new("Platform Systems", move |_context| {
//!         RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
//!             .with_command(list.clone())
//!     });
//!
//!     let cli = Cli::new(
//!         CliConfig::new("example", "Example cli-engine application", "example")
//!             .with_build(BuildInfo::new(env!("CARGO_PKG_VERSION")))
//!             .with_module(module),
//!     );
//!
//!     cli.execute().await
//! }
//! ```
//!
//! Command paths are colon-separated (`project:list`) for policy, audit,
//! schema, and authorization compatibility with existing CLI ecosystems.

/// Auth provider traits, dispatch, and built-in provider commands.
pub mod auth;
/// CLI application assembly and execution.
pub mod cli;
/// Command and command-group specifications.
pub mod command;
/// Shared error type and error traits.
pub mod error;
/// Global framework flags and flag-extraction helpers.
pub mod flags;
/// Embedded or file-backed guide parsing.
pub mod guide;
/// Cross-cutting command execution middleware.
pub mod middleware;
/// Domain module registration helpers.
pub mod module;
/// Structured output envelopes, renderers, schemas, and field projection.
pub mod output;
/// Search indexing for commands, guides, and extra documents.
pub mod search;
/// Command risk tiers used by authentication, authorization, and dry-run.
pub mod tier;
/// HTTP transport client and auth injectors.
pub mod transport;
/// Command tree data model and human rendering.
pub mod tree;

pub use auth::{
    AuthLoginResult, AuthProvider, AuthStatusEntry, CACHE_TTL, Credential, Dispatcher,
    SingleProvider, StatusEntry, auth_command_group, login_and_build, logout_result, status_result,
    to_status_entry,
};
pub use cli::{
    ApplyFlags, BuildInfo, Cli, CliConfig, CliRunOutput, ExtraSearchDocs, InitDeps,
    ModuleHelpEntry, OnShutdown, PreRun, RegisterFlags, ResolveMeta, RootNextActions,
    build_root_long,
};
pub use command::{
    CommandContext, CommandFuture, CommandHandler, CommandResult, CommandResultMetadata,
    CommandSpec, GroupSpec, RuntimeCommandSpec, RuntimeGroupSpec, StreamSender,
    StreamingCommandFuture, StreamingCommandHandler, command_args_from_matches,
    command_path_from_matches, command_path_from_parts, leaf_matches,
};
pub use error::{
    CliCoreError, DetailedError, ExitCoder, Result, exit_code_for_error, exit_code_for_exit_coder,
};
pub use flags::{
    GlobalFlags, default_output_format, derive_bool_flags, derive_value_flags,
    extract_command_path, extract_output_format, extract_search_query, global_flags_from_matches,
    has_true_schema_flag, output_env_var, register_global_flags, resolve_default_output_format,
};
pub use guide::{GuideEntry, parse_guides, parse_guides_from_markdown};
pub use middleware::{
    ActivityEmitter, ActivityEvent, Auditor, AuthRequirement, Authorizer, CommandMeta,
    CredentialResolver, Middleware, MiddlewareOutput, MiddlewareRequest,
};
pub use module::{CommandModule, Module, ModuleContext, ModuleRegister};
pub use output::{
    Envelope, ErrorEnvelope, FieldInfo, HumanViewDef, HumanViewFn, HumanViewRegistry,
    HumanViewRenderer, Metadata, NextAction, NextActionParam, OutputField, OutputFormat,
    OutputSchema, PaginationMeta, PipelineOpts, RendererFactory, SchemaInfo, SchemaRegistry,
    TableColumn, apply_pipeline, build_detailed_error_envelope, build_error_envelope, fields_for,
    fields_from_json_schema, filter_fields, format_help_section, get_global_schema_by_path,
    global_human_view_registry_snapshot, global_schema_registry_snapshot, is_valid_output_format,
    json_schema_for, json_schema_info, lookup_global_human_view_columns,
    lookup_global_human_view_func, parse_fields, register_global_human_view,
    register_global_human_view_func, register_global_json_schema, register_global_schema,
    register_global_schema_fields, register_global_schema_info, render, render_data,
    render_data_format, render_detailed_error, render_detailed_error_format, render_error,
    render_error_format, render_format, render_human, render_human_with_registry,
    render_human_with_registry_for_schema, render_human_with_view, render_json, render_toon,
    write_render,
};
pub use search::{SearchDocument, SearchResult};
pub use tier::Tier;
pub use tree::{TreeNode, build_tree_from_clap, build_tree_from_parts, render_tree_human};
