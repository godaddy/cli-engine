//! Built-in `config` command group for reading and writing the per-application
//! [config file](crate::config::ConfigFile).
//!
//! Mount it on a CLI with
//! [`CliConfig::with_config_commands`](crate::cli::CliConfig::with_config_commands).
//! The group exposes:
//!
//! - `config path` — print the resolved config file path.
//! - `config get <key>` — print the value at a dotted key (e.g. `deploy.region`).
//! - `config set <key> <value>` — set a value and persist (mutating; dry-run aware).
//! - `config list` — print the full config file contents.

use clap::Arg;
use serde_json::{Value, json};

use crate::config::{ConfigFile, config_file_path};
use crate::{CommandResult, CommandSpec, GroupSpec, RuntimeCommandSpec, RuntimeGroupSpec, Tier};

/// Builds the built-in runtime `config` command group.
#[must_use]
pub fn config_command_group() -> RuntimeGroupSpec {
    RuntimeGroupSpec::new(GroupSpec::new(
        "config",
        "Read and write the CLI config file",
    ))
    .with_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("path", "Print the config file path")
            .with_system("config")
            .no_auth(true),
        async |context| {
            let path =
                config_file_path(&context.middleware.app_id).map(|p| p.display().to_string());
            Ok(CommandResult::new(json!({ "path": path })))
        },
    ))
    .with_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("get", "Print a config value by dotted key")
            .with_system("config")
            .no_auth(true)
            .with_arg(
                Arg::new("key")
                    .value_name("KEY")
                    .required(true)
                    .help("Dotted key, e.g. credentials.store or deploy.region"),
            ),
        async |context| {
            let key = string_arg(&context.args, "key");
            let value = context.config().get(&key);
            Ok(CommandResult::new(json!({ "key": key, "value": value })))
        },
    ))
    .with_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("set", "Set a config value and save")
            .with_system("config")
            .with_tier(Tier::Mutate)
            .mutates(true)
            .no_auth(true)
            .with_arg(
                Arg::new("key")
                    .value_name("KEY")
                    .required(true)
                    .help("Dotted key, e.g. credentials.store or deploy.region"),
            )
            .with_arg(
                Arg::new("value")
                    .value_name("VALUE")
                    .required(true)
                    .help("Value (parsed as bool/int/float when possible, else string)"),
            ),
        async |context| {
            let key = string_arg(&context.args, "key");
            let value = string_arg(&context.args, "value");
            // Load fresh from disk (not the startup snapshot) so a concurrent
            // external edit is not clobbered, then set + save.
            let mut config = ConfigFile::load(&context.middleware.app_id);
            config.set(&key, &value)?;
            config.save()?;
            let path = config.path().map(|p| p.display().to_string());
            Ok(CommandResult::new(
                json!({ "key": key, "value": value, "path": path }),
            ))
        },
    ))
    .with_command(RuntimeCommandSpec::new_with_context(
        CommandSpec::new("list", "Print the full config file contents")
            .with_system("config")
            .no_auth(true),
        async |context| {
            let path = context.config().path().map(|p| p.display().to_string());
            Ok(CommandResult::new(json!({
                "path": path,
                "contents": context.config().to_toml_string(),
            })))
        },
    ))
}

/// Reads a required string argument, defaulting to empty when absent.
fn string_arg(args: &serde_json::Map<String, Value>, name: &str) -> String {
    args.get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
