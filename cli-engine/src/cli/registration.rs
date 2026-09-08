//! Post-construction registration: module/group mounting, guide-topic sync,
//! the root long-help rebuild, and the lazily-mounted built-in command
//! groups (`auth`, `config`, `env`, `flags`).

use clap::builder::PossibleValuesParser;

use super::{
    Cli,
    config::{BUILTIN_COMMAND_NAMES, DEFAULT_ADMIN_CATEGORY},
    help::{ModuleHelpEntry, build_root_long},
    lookup::has_subcommand,
    schema_tree::{
        prune_feature_flag_tree, register_runtime_group_metadata,
        runtime_group_clap_command_with_schema_help,
    },
};
use crate::{FeatureFlag, RuntimeGroupSpec, auth::commands::auth_command_group};

/// Lists the auto-registered `auth` command under the admin help category so
/// it is never uncategorized once clap's auto subcommand list is suppressed.
/// Defaults to [`DEFAULT_ADMIN_CATEGORY`]; `admin_category` overrides it to
/// align with a consumer's own taxonomy.
fn register_auth_help_entry(cli: &mut Cli) {
    let category = cli
        .config
        .admin_category
        .clone()
        .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
    let already_listed = cli.module_entries.iter().any(|entry| entry.name == "auth");
    let short = cli
        .root
        .find_subcommand("auth")
        .filter(|auth| !auth.is_hide_set())
        .map(|auth| {
            auth.get_about()
                .map(ToString::to_string)
                .unwrap_or_default()
        });
    if !already_listed && let Some(short) = short {
        cli.module_entries.push(ModuleHelpEntry {
            category,
            name: "auth".to_owned(),
            short,
        });
    }
    refresh_root_long(cli);
}

/// Shared implementation behind [`Cli::add_module_group`] and
/// [`Cli::add_module`]. `inherited` is the effective feature flag the
/// group's enclosing module declared (if any), so a module-level flag
/// cascades down to the group even though `add_module_group` itself has no
/// concept of a module.
pub(super) fn add_module_group_inner(
    cli: &mut Cli,
    category: impl Into<String>,
    group: RuntimeGroupSpec,
    inherited: Option<FeatureFlag>,
) -> &mut Cli {
    // Prevent consumer modules from shadowing engine built-ins in the clap
    // command tree.  A reserved group name would override the engine's own
    // subcommand (last-writer-wins in clap) and corrupt the dispatch path.
    if BUILTIN_COMMAND_NAMES.contains(&group.group.name.as_str()) {
        tracing::warn!(
            name = %group.group.name,
            "module group name is reserved by cli-engine built-ins; the group will not be registered"
        );
        return cli;
    }

    let mut prefix = Vec::new();
    let Some(group) = prune_feature_flag_tree(
        group,
        inherited.as_ref(),
        &cli.middleware.flag_policy,
        &mut prefix,
        &mut cli.middleware.flag_registry,
    ) else {
        return cli;
    };

    let category = category.into();
    if !group.group.hidden {
        cli.module_entries.push(ModuleHelpEntry {
            category,
            name: group.group.name.clone(),
            short: group.group.short.clone(),
        });
    }

    let mut prefix = Vec::new();
    register_runtime_group_metadata(
        &group,
        &mut prefix,
        &mut cli.middleware.schema_registry,
        &mut cli.middleware.human_views,
    );
    let mut prefix = Vec::new();
    group.register_commands(&mut prefix, &mut cli.commands);
    let mut prefix = Vec::new();
    let clap_group = runtime_group_clap_command_with_schema_help(
        &group,
        &mut prefix,
        &cli.middleware.schema_registry,
    );
    cli.root = cli.root.clone().subcommand(clap_group);
    refresh_root_long(cli);
    cli
}

/// Re-attaches the `guide` subcommand's `topic` arg possible values from
/// the current guide entries, so shell completion knows about guide names,
/// which are not all registered up front.
pub(super) fn sync_guide_topic_values(cli: &mut Cli) {
    if cli.guide_entries.is_empty() {
        return;
    }
    let names = cli
        .guide_entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    if let Some(guide_cmd) = cli.root.find_subcommand_mut("guide") {
        let taken = std::mem::replace(guide_cmd, clap::Command::new("guide"));
        *guide_cmd = taken.mut_arg("topic", |arg| {
            arg.value_parser(PossibleValuesParser::new(names))
        });
    }
}

pub(super) fn refresh_root_long(cli: &mut Cli) {
    // Module-categorized entries, plus any visible top-level command that is
    // neither categorized nor an engine built-in, listed under a generic
    // "Commands" section. This keeps every command discoverable once clap's
    // auto subcommand list is suppressed by the root help template.
    let builtins = BUILTIN_COMMAND_NAMES;
    let categorized: std::collections::BTreeSet<&str> = cli
        .module_entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let mut generic: Vec<ModuleHelpEntry> = cli
        .root
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .filter(|command| !builtins.contains(&command.get_name()))
        .filter(|command| !categorized.contains(command.get_name()))
        .map(|command| ModuleHelpEntry {
            category: "Commands".to_owned(),
            name: command.get_name().to_owned(),
            short: command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default(),
        })
        .collect();
    generic.sort_by(|left, right| left.name.cmp(&right.name));

    let mut entries = cli.module_entries.clone();
    entries.extend(generic);
    let has_guide = !cli.guide_entries.is_empty() || has_subcommand(&cli.root, "guide");
    let intro = cli
        .config
        .long
        .as_deref()
        .filter(|long| !long.is_empty())
        .unwrap_or(cli.config.short.as_str());
    cli.root = cli
        .root
        .clone()
        .long_about(build_root_long(intro, &entries, has_guide));
}

pub(super) fn ensure_auth_command(cli: &mut Cli) {
    let default_provider = default_auth_provider(cli);
    let registered_names = cli.middleware.auth.registered_names();
    if default_provider.is_empty() && registered_names.is_empty() {
        return;
    }
    let replacing_builtin = cli.commands.contains_key("auth:login");
    if has_subcommand(&cli.root, "auth") && !replacing_builtin {
        return;
    }
    let mut group = auth_command_group(&default_provider, &registered_names);
    let mut seen_names: std::collections::HashSet<String> =
        group.commands.iter().map(|c| c.spec.name.clone()).collect();
    for extra in cli.config.auth_extra_commands.clone() {
        if !seen_names.insert(extra.spec.name.clone()) {
            tracing::warn!(
                command = %extra.spec.name,
                "auth_extra_commands entry collides with a built-in auth subcommand or an \
                 earlier auth_extra_commands entry; ignoring"
            );
            continue;
        }
        group = group.with_command(extra);
    }
    let mut prefix = Vec::new();
    register_runtime_group_metadata(
        &group,
        &mut prefix,
        &mut cli.middleware.schema_registry,
        &mut cli.middleware.human_views,
    );
    let mut prefix = Vec::new();
    group.register_commands(&mut prefix, &mut cli.commands);
    let mut prefix = Vec::new();
    let clap_group = runtime_group_clap_command_with_schema_help(
        &group,
        &mut prefix,
        &cli.middleware.schema_registry,
    );
    cli.root = if replacing_builtin {
        cli.root.clone().mut_subcommand("auth", |_| clap_group)
    } else {
        cli.root.clone().subcommand(clap_group)
    };
    // Categorize `auth` wherever it is ensured (construction or a later
    // `register_auth_provider`), so it never falls into the generic
    // "Commands" bucket. Idempotent via the `already_listed` guard.
    register_auth_help_entry(cli);
}

/// Mounts the built-in `config` command group and files it under the admin
/// help category. Idempotent and yields to a consumer-defined `config`
/// subcommand if one already exists.
pub(super) fn ensure_config_command(cli: &mut Cli) {
    if has_subcommand(&cli.root, "config") {
        return;
    }
    let group = crate::config_commands::config_command_group();
    let mut prefix = Vec::new();
    group.register_commands(&mut prefix, &mut cli.commands);
    let mut prefix = Vec::new();
    let clap_group = runtime_group_clap_command_with_schema_help(
        &group,
        &mut prefix,
        &cli.middleware.schema_registry,
    );
    cli.root = cli.root.clone().subcommand(clap_group);
    let category = cli
        .config
        .admin_category
        .clone()
        .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
    if !cli
        .module_entries
        .iter()
        .any(|entry| entry.name == "config")
    {
        cli.module_entries.push(ModuleHelpEntry {
            category,
            name: "config".to_owned(),
            short: "Read and write the CLI config file".to_owned(),
        });
    }
    refresh_root_long(cli);
}

/// Mounts the built-in `env` command group and files it under the admin
/// help category. Idempotent and yields to a consumer-defined `env`
/// subcommand if one already exists.
pub(super) fn ensure_env_command(cli: &mut Cli) {
    if has_subcommand(&cli.root, "env") {
        return;
    }
    let group = crate::env_commands::env_command_group();
    let mut prefix = Vec::new();
    group.register_commands(&mut prefix, &mut cli.commands);
    let mut prefix = Vec::new();
    let clap_group = runtime_group_clap_command_with_schema_help(
        &group,
        &mut prefix,
        &cli.middleware.schema_registry,
    );
    cli.root = cli.root.clone().subcommand(clap_group);
    let category = cli
        .config
        .admin_category
        .clone()
        .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
    if !cli.module_entries.iter().any(|e| e.name == "env") {
        cli.module_entries.push(ModuleHelpEntry {
            category,
            name: "env".to_owned(),
            short: "Manage the active environment".to_owned(),
        });
    }
    refresh_root_long(cli);
}

/// Mounts the built-in `flags` command group and files it under the admin
/// help category. Idempotent and yields to a consumer-defined `flags`
/// subcommand if one already exists. Unlike [`ensure_env_command`], this is
/// mounted unconditionally: feature-flag introspection does not depend on
/// any opt-in system, so it is always available.
pub(super) fn ensure_flags_command(cli: &mut Cli) {
    if has_subcommand(&cli.root, "flags") {
        return;
    }
    let group = crate::flag_commands::flags_command_group();
    let mut prefix = Vec::new();
    group.register_commands(&mut prefix, &mut cli.commands);
    let mut prefix = Vec::new();
    let clap_group = runtime_group_clap_command_with_schema_help(
        &group,
        &mut prefix,
        &cli.middleware.schema_registry,
    );
    cli.root = cli.root.clone().subcommand(clap_group);
    let category = cli
        .config
        .admin_category
        .clone()
        .unwrap_or_else(|| DEFAULT_ADMIN_CATEGORY.to_owned());
    if !cli.module_entries.iter().any(|e| e.name == "flags") {
        cli.module_entries.push(ModuleHelpEntry {
            category,
            name: "flags".to_owned(),
            short: "Inspect declared feature flags".to_owned(),
        });
    }
    refresh_root_long(cli);
}

fn default_auth_provider(cli: &Cli) -> String {
    if !cli.middleware.default_auth_provider.is_empty() {
        return cli.middleware.default_auth_provider.clone();
    }
    cli.middleware
        .auth
        .registered_names()
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[cfg(test)]
mod flags_command_tests {
    use super::*;
    use crate::{
        CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec, cli::CliConfig,
        feature_flags::Stage,
    };

    /// Builds a module with one flagged group containing one flagged (via
    /// inheritance) `list` command, so `flag_registry` has something to
    /// introspect once the module is mounted.
    fn flagged_module(group_name: &'static str, key: &'static str, stage: Stage) -> Module {
        Module::new("Test Category", move |_ctx| {
            RuntimeGroupSpec::new(GroupSpec::new(group_name, "short")).with_command(
                RuntimeCommandSpec::new(
                    CommandSpec::new("list", "short").no_auth(true),
                    async |_, _| Ok(CommandResult::new(serde_json::Value::Null)),
                ),
            )
        })
        .with_feature_flag(key, stage)
    }

    #[tokio::test]
    async fn flags_list_reports_flagged_entries() {
        let mut cli = Cli::new(
            CliConfig::new("flagtest", "Flag test", "flagtest").with_min_stage(Stage::Beta),
        );
        cli.add_module(flagged_module("flagged-mod", "list-flag", Stage::Beta));

        let out = cli
            .run(["flagtest", "flags", "list", "--output", "json"])
            .await;
        assert_eq!(out.exit_code, 0, "rendered: {}", out.rendered);
        let rendered: serde_json::Value =
            serde_json::from_str(&out.rendered).expect("stdout should contain json");
        let entries = rendered["data"].as_array().expect("data should be array");
        let command_entry = entries
            .iter()
            .find(|entry| entry["path"] == "flagged-mod:list")
            .expect("flagged command entry should be present");
        assert_eq!(command_entry["key"], "list-flag");
        assert_eq!(command_entry["stage"], "beta");
        assert_eq!(command_entry["visible"], true);
    }

    #[tokio::test]
    async fn flags_info_returns_policy_and_entries_for_known_key() {
        let mut cli = Cli::new(
            CliConfig::new("flagtest2", "Flag test", "flagtest2").with_min_stage(Stage::Beta),
        );
        cli.add_module(flagged_module("flagged-mod-2", "info-flag", Stage::Beta));

        let out = cli
            .run([
                "flagtest2",
                "flags",
                "info",
                "info-flag",
                "--output",
                "json",
            ])
            .await;
        assert_eq!(out.exit_code, 0, "rendered: {}", out.rendered);
        let rendered: serde_json::Value =
            serde_json::from_str(&out.rendered).expect("stdout should contain json");
        let data = &rendered["data"];
        assert_eq!(data["key"], "info-flag");
        assert_eq!(data["policy"]["min_stage"], "beta");
        assert!(data["policy"]["override"].is_null());
        let entries = data["entries"].as_array().expect("entries should be array");
        assert!(!entries.is_empty());
        assert!(entries.iter().any(|entry| {
            entry["path"] == "flagged-mod-2:list" && entry["decided_by"] == "min_stage"
        }));
    }

    #[tokio::test]
    async fn flags_info_reports_override_decided_by() {
        // The module declares Experimental, which the default Ga policy would
        // normally hide; the override forces Ga instead, so the entries stay
        // visible even though `entry.stage` still reports the node's own
        // (Experimental) declaration, not the override.
        let mut cli = Cli::new(
            CliConfig::new("flagtest3", "Flag test", "flagtest3")
                .with_feature_override("override-flag", Stage::Ga),
        );
        cli.add_module(flagged_module(
            "flagged-mod-3",
            "override-flag",
            Stage::Experimental,
        ));

        let out = cli
            .run([
                "flagtest3",
                "flags",
                "info",
                "override-flag",
                "--output",
                "json",
            ])
            .await;
        assert_eq!(out.exit_code, 0, "rendered: {}", out.rendered);
        let rendered: serde_json::Value =
            serde_json::from_str(&out.rendered).expect("stdout should contain json");
        let data = &rendered["data"];
        assert_eq!(data["policy"]["min_stage"], "ga");
        assert_eq!(data["policy"]["override"], "ga");
        let entries = data["entries"].as_array().expect("entries should be array");
        assert!(!entries.is_empty());
        assert!(
            entries
                .iter()
                .all(|entry| entry["decided_by"] == "override")
        );
        assert!(entries.iter().all(|entry| entry["visible"] == true));
        assert!(entries.iter().all(|entry| entry["stage"] == "experimental"));
    }

    #[tokio::test]
    async fn flags_info_unknown_key_errors() {
        let cli = Cli::new(CliConfig::new("flagtest4", "Flag test", "flagtest4"));

        let out = cli
            .run(["flagtest4", "flags", "info", "no-such-flag"])
            .await;
        assert_ne!(out.exit_code, 0);
        assert!(out.rendered.contains("no such flag"));
    }
}
