//! Built-in `env` command group: list/get/set/info for environments.
//!
//! Mounted automatically when [`crate::CliConfig::with_environments`] is used.
//! The group exposes:
//!
//! - `env list`  — list known environments with active flag.
//! - `env get`   — print the active environment name.
//! - `env set <name>` — persist the active environment.
//! - `env info`  — show the fully resolved active environment.

use serde_json::json;

use crate::{
    CommandResult, CommandSpec, GroupSpec, RuntimeCommandSpec, RuntimeGroupSpec,
    error::CliCoreError,
};

/// Builds the built-in `env` command group.
#[must_use]
pub fn env_command_group() -> RuntimeGroupSpec {
    RuntimeGroupSpec::new(GroupSpec::new("env", "Manage the active environment"))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("list", "List known environments").no_auth(true),
            async |ctx| {
                let envs = ctx
                    .middleware
                    .environments
                    .as_ref()
                    .ok_or_else(|| CliCoreError::message("no environment system configured"))?;
                let active = ctx.middleware.env.clone();
                let items: Vec<_> = envs
                    .list()
                    .into_iter()
                    .map(|name| {
                        let is_active = name == active;
                        json!({ "name": name, "active": is_active })
                    })
                    .collect();
                Ok(CommandResult::new(json!(items)))
            },
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("get", "Show the active environment name").no_auth(true),
            async |ctx| Ok(CommandResult::new(json!({ "active": ctx.middleware.env }))),
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("info", "Show the resolved active environment").no_auth(true),
            async |ctx| {
                let env = ctx.environment()?;
                let oauth = env.oauth.map(|o| {
                    json!({
                        "client_id": o.client_id,
                        "auth_url": o.auth_url,
                        "token_url": o.token_url,
                        "scopes": o.scopes,
                    })
                });
                Ok(CommandResult::new(json!({
                    "name": env.name,
                    "oauth": oauth,
                    "extra": env.extra,
                })))
            },
        ))
        .with_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("set", "Set and persist the active environment")
                .no_auth(true)
                .with_arg(clap::Arg::new("name").required(true)),
            async |ctx| {
                let envs = ctx
                    .middleware
                    .environments
                    .as_ref()
                    .ok_or_else(|| CliCoreError::message("no environment system configured"))?;
                let name = string_arg(&ctx.args, "name");
                if name.is_empty() {
                    return Err(CliCoreError::message("missing environment name"));
                }
                envs.persist_active(&name)?;
                Ok(CommandResult::new(json!({ "active": name })))
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
