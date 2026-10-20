//! Built-in `config` command group for reading and writing the per-application
//! [config file](crate::config::ConfigFile).
//!
//! Mount it on a CLI with
//! [`CliConfig::with_config_commands`](crate::cli::CliConfig::with_config_commands),
//! or register the CLI's own typed settings with
//! [`CliConfig::with_config_schema`](crate::cli::CliConfig::with_config_schema)
//! (see [`ConfigSchema`]), which mounts it too. The group exposes:
//!
//! - `config path` — print the resolved config file path.
//! - `config get <key>` — print a setting's value, default, and description.
//! - `config set <key> <value>` — validate, set, and persist (mutating; dry-run aware).
//! - `config unset <key>` — remove a key so its default applies again (mutating; dry-run aware).
//! - `config list` — print every known setting, plus the raw file contents.
//!
//! With only the engine's own keys registered the group is permissive, exactly
//! as it was before schemas existed: any dotted key is accepted and typed by
//! inference. Registering a consumer schema makes `set` strict.

use std::sync::Arc;

use serde_json::json;

use crate::config::{ConfigFile, config_file_path};
use crate::config_schema::ConfigSchema;
use crate::{
    CommandResult, CommandSpec, GroupSpec, RuntimeCommandSpec, RuntimeGroupSpec, TableColumn, Tier,
};

/// Positional `<KEY>` shared by `config get` and `config unset`.
#[derive(Debug, Clone, clap::Args)]
struct KeyArgs {
    /// Name of the setting, e.g. credentials.store
    #[arg(value_name = "KEY")]
    key: String,
}

/// Positional arguments of `config set`.
#[derive(Debug, Clone, clap::Args)]
struct SetArgs {
    /// Name of the setting, e.g. credentials.store
    #[arg(value_name = "KEY")]
    key: String,

    /// New value for the setting
    #[arg(value_name = "VALUE", allow_hyphen_values = true)]
    value: String,
}

/// Builds the built-in runtime `config` command group for the engine's own keys.
#[must_use]
pub fn config_command_group() -> RuntimeGroupSpec {
    config_command_group_with(ConfigSchema::engine())
}

/// Builds the `config` command group for a CLI's [`ConfigSchema`].
#[must_use]
pub fn config_command_group_with(schema: ConfigSchema) -> RuntimeGroupSpec {
    let schema = Arc::new(schema);
    RuntimeGroupSpec::new(GroupSpec::new(
        "config",
        "Read and write the CLI config file",
    ))
    .with_command(path_command())
    .with_command(get_command(&schema))
    .with_command(set_command(&schema))
    .with_command(unset_command(&schema))
    .with_command(list_command(&schema))
}

fn path_command() -> RuntimeCommandSpec {
    RuntimeCommandSpec::new_with_context(
        CommandSpec::new("path", "Print the config file path")
            .with_system("config")
            .no_auth(true),
        async |context| {
            let path =
                config_file_path(&context.middleware.app_id).map(|p| p.display().to_string());
            Ok(CommandResult::new(json!({ "path": path })))
        },
    )
}

fn get_command(schema: &Arc<ConfigSchema>) -> RuntimeCommandSpec {
    let schema = Arc::clone(schema);
    RuntimeCommandSpec::new_typed_with_context::<KeyArgs, _, _, _>(
        CommandSpec::from_args::<KeyArgs>("get", "Show a setting's value")
            .with_long(known_keys_help(
                "Show a setting's current value and what it does.",
                &schema,
            ))
            .with_system("config")
            .no_auth(true),
        move |context, args: KeyArgs| {
            let schema = Arc::clone(&schema);
            async move {
                let key = args.key;
                let file = context.config();
                file.check_loaded()?;
                if let Some(entry) = schema.entry(file, &key) {
                    return Ok(CommandResult::new(serde_json::to_value(entry)?));
                }
                // Not a declared setting: a strict schema only reports what is
                // actually in the file; otherwise keep the string form.
                let value = file.get(&key);
                if schema.is_strict() && value.is_none() {
                    return Err(schema.unknown_key(&key));
                }
                Ok(CommandResult::new(json!({ "key": key, "value": value })))
            }
        },
    )
}

fn set_command(schema: &Arc<ConfigSchema>) -> RuntimeCommandSpec {
    let schema = Arc::clone(schema);
    RuntimeCommandSpec::new_typed_with_context::<SetArgs, _, _, _>(
        CommandSpec::from_args::<SetArgs>("set", "Change a setting")
            .with_long(known_keys_help(
                "Change a setting and save it to the config file.",
                &schema,
            ))
            .with_system("config")
            .with_tier(Tier::Mutate)
            .mutates(true)
            .no_auth(true),
        move |context, args: SetArgs| {
            let schema = Arc::clone(&schema);
            async move {
                let SetArgs { key, value: raw } = args;
                // Load fresh from disk (not the startup snapshot) to avoid
                // clobbering a concurrent external edit, and refuse a broken
                // file rather than overwrite it. The startup snapshot in
                // `context.config()` is NOT updated by this write.
                let mut file = ConfigFile::try_load(&context.middleware.app_id)?;
                let value = schema.set(&mut file, &key, &raw)?;
                file.save()?;
                let path = file.path().map(|p| p.display().to_string());
                Ok(CommandResult::new(
                    json!({ "key": key, "value": value, "path": path }),
                ))
            }
        },
    )
}

fn unset_command(schema: &Arc<ConfigSchema>) -> RuntimeCommandSpec {
    let schema = Arc::clone(schema);
    RuntimeCommandSpec::new_typed_with_context::<KeyArgs, _, _, _>(
        CommandSpec::from_args::<KeyArgs>("unset", "Reset a setting to its default")
            .with_system("config")
            .with_tier(Tier::Mutate)
            .mutates(true)
            .no_auth(true),
        move |context, args: KeyArgs| {
            let schema = Arc::clone(&schema);
            async move {
                let key = args.key;
                let mut file = ConfigFile::try_load(&context.middleware.app_id)?;
                let removed = schema.unset(&mut file, &key)?;
                if removed {
                    file.save()?;
                }
                let path = file.path().map(|p| p.display().to_string());
                Ok(CommandResult::new(
                    json!({ "key": key, "removed": removed, "path": path }),
                ))
            }
        },
    )
}

fn list_command(schema: &Arc<ConfigSchema>) -> RuntimeCommandSpec {
    let schema = Arc::clone(schema);
    RuntimeCommandSpec::new_with_context(
        CommandSpec::new("list", "List settings and their current values")
            .with_system("config")
            .no_auth(true)
            .with_view(vec![
                TableColumn::new("key", "Key").essential(true),
                TableColumn::new("value", "Value"),
                TableColumn::new("default", "Default"),
                TableColumn::new("source", "Source"),
                TableColumn::new("description", "Description"),
            ]),
        move |context| {
            let schema = Arc::clone(&schema);
            async move {
                context.config().check_loaded()?;
                let entries = schema.entries(context.config());
                Ok(CommandResult::new(serde_json::to_value(entries)?))
            }
        },
    )
}

/// `intro` followed by a table of the schema's known keys, for `--help`.
fn known_keys_help(intro: &str, schema: &ConfigSchema) -> String {
    let width = schema.keys().iter().map(|k| k.key.len()).max().unwrap_or(0);
    let rows: Vec<String> = schema
        .keys()
        .iter()
        .map(|k| format!("  {:<width$}  {}", k.key, k.description))
        .collect();
    format!("{intro}\n\nKnown settings:\n{}", rows.join("\n"))
}
