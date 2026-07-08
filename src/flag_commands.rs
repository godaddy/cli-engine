//! Built-in `flags` command group: introspection for declared feature flags.
//!
//! Mounted unconditionally by [`crate::Cli::new`] (feature-flag introspection does
//! not depend on any other opt-in system, unlike `env` or `config`). The group
//! exposes:
//!
//! - `flags list` — list every flagged command/group/module node discovered while
//!   pruning the command tree, with the stage and visibility decided for this run.
//! - `flags info <key>` — show the active policy for one flag key plus every node
//!   that resolved to it.

use serde_json::json;

use crate::{
    CommandResult, CommandSpec, GroupSpec, RuntimeCommandSpec, RuntimeGroupSpec,
    error::CliCoreError,
};

/// Builds the built-in `flags` command group.
#[must_use]
pub fn flags_command_group() -> RuntimeGroupSpec {
    RuntimeGroupSpec::new(GroupSpec::new("flags", "Inspect declared feature flags"))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("list", "List every flagged command/group/module node")
                .no_auth(true)
                .with_system("flags"),
            async |ctx| {
                let items: Vec<_> = ctx
                    .middleware
                    .flag_registry
                    .entries()
                    .iter()
                    .map(|entry| {
                        json!({
                            "path": entry.path,
                            "key": entry.key,
                            "stage": entry.stage.as_str(),
                            "visible": entry.visible,
                        })
                    })
                    .collect();
                Ok(CommandResult::new(json!(items)))
            },
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("info", "Show the policy and nodes for one flag key")
                .no_auth(true)
                .with_system("flags")
                .with_arg(
                    clap::Arg::new("key")
                        .value_name("KEY")
                        .required(true)
                        .help("Flag key to inspect"),
                ),
            async |ctx| {
                let key = string_arg(&ctx.args, "key");
                if key.is_empty() {
                    return Err(CliCoreError::message("missing flag key"));
                }
                let matches = ctx.middleware.flag_registry.by_key(&key);
                if matches.is_empty() {
                    return Err(CliCoreError::message(format!("no such flag: {key}")));
                }
                let has_override = ctx.middleware.flag_policy.overrides.contains_key(&key);
                let decided_by = if has_override {
                    "override"
                } else {
                    "min_stage"
                };
                let override_stage = ctx
                    .middleware
                    .flag_policy
                    .overrides
                    .get(&key)
                    .map(|stage| json!(stage.as_str()))
                    .unwrap_or(serde_json::Value::Null);
                let entries: Vec<_> = matches
                    .iter()
                    .map(|entry| {
                        json!({
                            "path": entry.path,
                            "stage": entry.stage.as_str(),
                            "visible": entry.visible,
                            "decided_by": decided_by,
                        })
                    })
                    .collect();
                Ok(CommandResult::new(json!({
                    "key": key,
                    "policy": {
                        "min_stage": ctx.middleware.flag_policy.min_stage.as_str(),
                        "override": override_stage,
                    },
                    "entries": entries,
                })))
            },
        ))
}

/// Reads a required string argument, defaulting to empty when absent.
fn string_arg(args: &serde_json::Map<String, serde_json::Value>, name: &str) -> String {
    args.get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
