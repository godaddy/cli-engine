//! Feature-flag tree pruning and building the `clap` command tree with
//! schema-derived help text (`--fields`/`--filter`/`--expr` examples,
//! `--dry-run`/`--output` visibility, pagination args).

use clap::{Arg, Command};

use super::help::GROUP_HELP_TEMPLATE;
use crate::{
    CommandSpec, FeatureFlag, GroupSpec, RuntimeGroupSpec,
    feature_flags::{FlagEntry, FlagPolicy, FlagRegistry},
    output::{FieldInfo, HumanViewDef, HumanViewRegistry, SchemaRegistry, format_help_section},
};

/// Walks a runtime group tree, resolving each node's effective feature flag by
/// cascading from `inherited` — a node's own [`GroupSpec::feature_flag`] or
/// [`CommandSpec::feature_flag`] wins if set, otherwise it inherits the
/// nearest ancestor's effective flag, otherwise (nothing in the ancestor
/// chain declared a flag) it implicitly resolves to [`Stage::Ga`] with no key.
/// Every node that resolves to a *named* flag (own or inherited) is recorded
/// into `registry` under its colon-separated path, together with whether
/// `policy` judged it visible. Nodes that resolve to the implicit no-flag
/// default are not recorded (there is nothing to introspect) and are always
/// visible.
///
/// Returns `None` when this group itself should be dropped from the tree —
/// either because its effective flag is not visible under `policy`, or
/// because every one of its commands and subgroups was pruned away, leaving
/// an empty group with nothing to mount. An emptied-out group is dropped
/// unconditionally, even if its own flag was visible: a `clap` subcommand
/// group with zero children is useless either way, so this simplifies the
/// pruning logic rather than threading through a "was this group itself
/// visible but empty" distinction that no caller needs.
///
/// Note that an invisible ancestor short-circuits before its children are
/// even visited: a more permissive flag on a descendant cannot resurrect a
/// subtree whose enclosing group already failed the visibility check.
pub(super) fn prune_feature_flag_tree(
    mut group: RuntimeGroupSpec,
    inherited: Option<&FeatureFlag>,
    policy: &FlagPolicy,
    prefix: &mut Vec<String>,
    registry: &mut FlagRegistry,
) -> Option<RuntimeGroupSpec> {
    prefix.push(group.group.name.clone());

    let effective = group
        .group
        .feature_flag
        .clone()
        .or_else(|| inherited.cloned());
    if !record_and_check_visibility(effective.as_ref(), policy, prefix, registry) {
        prefix.pop();
        return None;
    }

    let mut kept_groups = Vec::with_capacity(group.groups.len());
    for child in std::mem::take(&mut group.groups) {
        if let Some(pruned) =
            prune_feature_flag_tree(child, effective.as_ref(), policy, prefix, registry)
        {
            kept_groups.push(pruned);
        }
    }
    group.groups = kept_groups;

    let mut kept_commands = Vec::with_capacity(group.commands.len());
    for command in std::mem::take(&mut group.commands) {
        prefix.push(command.spec.name.clone());
        let command_effective = command
            .spec
            .feature_flag
            .clone()
            .or_else(|| effective.clone());
        let visible =
            record_and_check_visibility(command_effective.as_ref(), policy, prefix, registry);
        prefix.pop();
        if visible {
            kept_commands.push(command);
        }
    }
    group.commands = kept_commands;

    prefix.pop();

    if group.commands.is_empty() && group.groups.is_empty() {
        None
    } else {
        Some(group)
    }
}

/// Records `effective` at the current `prefix` path into `registry` (only
/// when it names a flag key — the implicit Ga default is not recorded) and
/// returns whether the node is visible under `policy`.
fn record_and_check_visibility(
    effective: Option<&FeatureFlag>,
    policy: &FlagPolicy,
    prefix: &[String],
    registry: &mut FlagRegistry,
) -> bool {
    let Some(flag) = effective else {
        return true;
    };
    let visible = policy.visible(Some(flag.key.as_str()), flag.stage);
    registry.record(FlagEntry {
        path: prefix.join(":"),
        key: flag.key.clone(),
        stage: flag.stage,
        visible,
    });
    visible
}

pub(super) fn register_runtime_group_metadata(
    group: &RuntimeGroupSpec,
    prefix: &mut Vec<String>,
    schemas: &mut SchemaRegistry,
    views: &mut HumanViewRegistry,
) {
    prefix.push(group.group.name.clone());
    for child_group in &group.groups {
        register_runtime_group_metadata(child_group, prefix, schemas, views);
    }
    for child in &group.commands {
        prefix.push(child.spec.name.clone());
        let command_path = prefix.join(":");
        register_command_schema(&child.spec, &command_path, schemas);
        // An inline `with_view` is registered under the command's own path; the
        // dispatch references it by that path. A `with_view_id` takes precedence
        // (dispatch uses it instead), so skip the inline registration when one is
        // set — registering it would leave an unused entry. Shared views are
        // registered separately by the module/CLI.
        if child.spec.view_id.is_none() && !child.spec.view_columns.is_empty() {
            views.register(HumanViewDef::new(
                command_path,
                child.spec.view_columns.clone(),
            ));
        }
        prefix.pop();
    }
    prefix.pop();
}

pub(super) fn register_command_schema(
    spec: &CommandSpec,
    command_path: &str,
    schemas: &mut SchemaRegistry,
) {
    if let Some(schema) = &spec.output_schema {
        schemas.register_info(command_path.to_owned(), schema.clone());
    }
}

pub(super) fn runtime_group_clap_command_with_schema_help(
    group: &RuntimeGroupSpec,
    prefix: &mut Vec<String>,
    schemas: &SchemaRegistry,
) -> Command {
    let mut command = group_clap_command_without_children(&group.group);
    prefix.push(group.group.name.clone());
    for child_group in &group.groups {
        command = command.subcommand(runtime_group_clap_command_with_schema_help(
            child_group,
            prefix,
            schemas,
        ));
    }
    for child in &group.commands {
        prefix.push(child.spec.name.clone());
        let command_path = prefix.join(":");
        command = command.subcommand(command_clap_command_with_schema_help(
            &child.spec,
            &command_path,
            schemas,
        ));
        prefix.pop();
    }
    prefix.pop();
    command
}

fn group_clap_command_without_children(group: &GroupSpec) -> Command {
    let mut command = Command::new(group.name.clone())
        .about(group.short.clone())
        .help_template(GROUP_HELP_TEMPLATE);
    if let Some(long) = &group.long
        && !long.is_empty()
    {
        command = command.long_about(long.clone());
    }
    for alias in &group.aliases {
        command = command.alias(alias.clone());
    }
    if group.hidden {
        command = command.hide(true);
    }
    command
}

pub(super) fn command_clap_command_with_schema_help(
    spec: &CommandSpec,
    command_path: &str,
    schemas: &SchemaRegistry,
) -> Command {
    debug_assert!(
        !(spec.raw_output && spec.pagination.is_some()),
        "command {:?} sets both raw_output and with_pagination; a single verbatim string \
         has no pages, so the two are mutually exclusive",
        spec.name
    );
    let mut command = spec.clap_command();
    command = apply_dry_run_visibility(command, spec);
    command = apply_pagination_args(command, spec);
    let schema = schemas.get_by_path(command_path);
    let default_fields = default_field_names(spec);
    command = apply_fields_arg(
        command,
        spec,
        schema.as_ref().map(|schema| schema.fields.as_slice()),
        &default_fields,
    );
    command = apply_output_format_visibility(command, spec);
    let filter_expr_fields = schema
        .as_ref()
        .map_or(&[][..], |schema| schema.fields.as_slice());
    apply_filter_and_expr_examples(command, spec, filter_expr_fields)
}

/// Hides this command's inherited `--output` flag when it opted into
/// [`CommandSpec::raw_output`].
fn apply_output_format_visibility(command: Command, spec: &CommandSpec) -> Command {
    if !spec.raw_output {
        return command;
    }
    use std::io::IsTerminal;
    command.arg(
        Arg::new("output")
            .long("output")
            .short('o')
            .value_name("FORMAT")
            .default_value(if std::io::stdout().is_terminal() {
                "human"
            } else {
                "json"
            })
            .conflicts_with_all(["json", "toon", "human"])
            .display_order(crate::flags::global_flag_order::OUTPUT)
            .hide(true)
            .help("Ignored — this command always prints raw text"),
    )
}

/// Hides this command's inherited `--dry-run` flag when the command isn't
/// mutating (per [`CommandSpec::metadata`]'s `dry_run_prompt` — mirrored
/// here rather than reused, since that method returns the broader
/// [`CommandMeta`], not this one bool). `--dry-run` only ever does anything
/// for a command that opted in via `.mutates(true)`/`.with_tier(...)` (see
/// `Middleware::render_envelope`'s `meta.dry_run_prompt` gate), so showing
/// it on every other command is noise. The override still parses `--dry-run`
/// identically (same value parser, same defaults) in case a caller passes
/// it anyway — hidden only changes what `--help` shows, never behavior.
fn apply_dry_run_visibility(command: Command, spec: &CommandSpec) -> Command {
    let mutates = spec.mutates || spec.tier.is_some_and(crate::Tier::is_mutating);
    if mutates {
        return command;
    }
    command.arg(
        Arg::new("dry-run")
            .long("dry-run")
            .num_args(0..=1)
            .require_equals(true)
            .default_missing_value("true")
            .default_value("false")
            .value_parser(crate::flags::compat_bool_value_parser())
            .display_order(crate::flags::global_flag_order::DRY_RUN)
            .hide(true)
            .help("Preview mutations without executing"),
    )
}

/// Registers `--limit`/`--offset` on this command's own `Command` when its
/// spec opted in via [`CommandSpec::with_pagination`], and leaves the command
/// untouched otherwise so a non-paginating command never sees those flags —
/// in `--help` or on its command line. See [`flags::apply_pagination_args`].
fn apply_pagination_args(command: Command, spec: &CommandSpec) -> Command {
    let Some(pagination) = spec.pagination else {
        return command;
    };
    crate::flags::apply_pagination_args(command, pagination.default_limit, pagination.max_limit)
}

/// Splits a command's raw `default_fields` string into individual field
/// names, dropping the `all`/`*` sentinels that mean "every field" rather
/// than naming a real field.
fn default_field_names(spec: &CommandSpec) -> Vec<&str> {
    spec.default_fields
        .as_deref()
        .map(|fields| {
            fields
                .split(',')
                .map(str::trim)
                .filter(|field| !field.is_empty() && *field != "all" && *field != "*")
                .collect()
        })
        .unwrap_or_default()
}

/// Overrides this command's `--fields` flag with everything specific to this
/// command: its own `default_fields` as a native clap default value (so
/// `--help` shows `[default: ...]` on the flag itself, the same way
/// `--dry-run` shows `[default: false]`), and, when a schema is registered,
/// the output-field summary table appended to the flag's own help text
/// instead of the command's description — a long field table there used to
/// push `Usage:` far down the page. Global args apply to every subcommand,
/// but a subcommand-local arg of the same name takes precedence, so this
/// only affects the one command being built here.
fn apply_fields_arg(
    command: Command,
    spec: &CommandSpec,
    schema_fields: Option<&[FieldInfo]>,
    default_fields: &[&str],
) -> Command {
    if spec.raw_output {
        return command.arg(
            Arg::new("fields")
                .long("fields")
                .value_name("FIELDS")
                .display_order(crate::flags::global_flag_order::FIELDS)
                .hide(true)
                .help("Ignored — this command always prints raw text"),
        );
    }
    let default_value = spec
        .default_fields
        .as_deref()
        .filter(|fields| !fields.is_empty());
    let table = schema_fields
        .filter(|fields| !fields.is_empty())
        .map(|fields| format_help_section(fields, default_fields));
    if default_value.is_none() && table.is_none() {
        return command;
    }

    let mut help = String::from(
        "Comma-separated fields to include in output (use 'all' or '*' for everything)",
    );
    if let Some(table) = &table {
        help.push_str("\n\n");
        help.push_str(table.trim_end());
    }

    let mut arg = Arg::new("fields")
        .long("fields")
        .value_name("FIELDS")
        // Must match `global_flag_order::FIELDS` — this re-registers the
        // same flag with contextual help, not a new one, and needs to keep
        // its place among the other global flags rather than falling back
        // to this subcommand's own low, command-specific counter value.
        .display_order(crate::flags::global_flag_order::FIELDS)
        .help(help);
    if let Some(default_value) = default_value {
        arg = arg.default_value(default_value.to_owned());
    }
    command.arg(arg)
}

/// Overrides this command's `--filter` and `--expr` flags with help text
/// carrying usage examples built from its own output fields, so `--help`
/// shows them right under the flag instead of in a separate "Filter
/// examples:"/"Expr examples:" section disconnected from the flags they
/// demonstrate. Mirrors [`apply_fields_arg`]: a subcommand-local arg of the
/// same name shadows the framework's global one, and must carry the same
/// `global_flag_order` value as that global one for the same reason.
fn apply_filter_and_expr_examples(
    mut command: Command,
    spec: &CommandSpec,
    fields: &[FieldInfo],
) -> Command {
    if spec.raw_output {
        return command
            .arg(
                Arg::new("filter")
                    .long("filter")
                    .value_name("EXPR")
                    .display_order(crate::flags::global_flag_order::FILTER)
                    .hide(true)
                    .help("Ignored — this command always prints raw text"),
            )
            .arg(
                Arg::new("expr")
                    .long("expr")
                    .value_name("EXPR")
                    .display_order(crate::flags::global_flag_order::EXPR)
                    .hide(true)
                    .help("Ignored — this command always prints raw text"),
            );
    }
    if fields.is_empty() {
        return command;
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
        let mut help = String::from("Per-item JMESPath predicate for list data");
        if let Some(name) = first_string {
            help.push_str(&format!("\ne.g. --filter \"contains({name}, 'example')\""));
        }
        if let Some(name) = first_bool {
            help.push_str(&format!("\ne.g. --filter '{name}'"));
        }
        command = command.arg(
            Arg::new("filter")
                .long("filter")
                .value_name("EXPR")
                .display_order(crate::flags::global_flag_order::FILTER)
                .help(help),
        );
    }

    let mut expr_help = String::from("JMESPath query applied to the whole result");
    expr_help.push_str("\ne.g. --expr 'length(@)'");
    if let Some(name) = first_string {
        expr_help.push_str(&format!("\ne.g. --expr '[].{name}'"));
    }
    command.arg(
        Arg::new("expr")
            .long("expr")
            .value_name("EXPR")
            .display_order(crate::flags::global_flag_order::EXPR)
            .help(expr_help),
    )
}

#[cfg(test)]
mod feature_flag_pruning_tests {
    use super::*;
    use crate::{
        CommandResult, GroupSpec, Module, RuntimeCommandSpec, RuntimeGroupSpec,
        cli::{Cli, CliConfig, flags_apply::global_min_stage_override, lookup::has_subcommand},
        feature_flags::Stage,
    };

    fn trivial_command(name: &str) -> RuntimeCommandSpec {
        RuntimeCommandSpec::new(
            CommandSpec::new(name, "short").no_auth(true),
            async |_, _| Ok(CommandResult::new(serde_json::Value::Null)),
        )
    }

    fn flagged_command(name: &str, key: &str, stage: Stage) -> RuntimeCommandSpec {
        let mut command = trivial_command(name);
        command.spec = command.spec.with_feature_flag(key, stage);
        command
    }

    fn empty_policy() -> FlagPolicy {
        FlagPolicy::default()
    }

    #[test]
    fn no_flags_anywhere_keeps_everything() {
        let group = RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
            .with_command(trivial_command("a"))
            .with_command(trivial_command("b"))
            .with_group(
                RuntimeGroupSpec::new(GroupSpec::new("child", "short"))
                    .with_command(trivial_command("c")),
            );

        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned =
            prune_feature_flag_tree(group, None, &empty_policy(), &mut prefix, &mut registry);

        let pruned = pruned.expect("unflagged tree should never be dropped");
        assert_eq!(pruned.commands.len(), 2);
        assert_eq!(pruned.groups.len(), 1);
        assert_eq!(pruned.groups[0].commands.len(), 1);
        assert!(registry.entries().is_empty());
    }

    #[test]
    fn experimental_command_is_pruned_sibling_is_not() {
        let group = RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
            .with_command(flagged_command("gated", "gated-flag", Stage::Experimental))
            .with_command(trivial_command("sibling"));

        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned =
            prune_feature_flag_tree(group, None, &empty_policy(), &mut prefix, &mut registry)
                .expect("group still has a visible command left");

        assert_eq!(pruned.commands.len(), 1);
        assert_eq!(pruned.commands[0].spec.name, "sibling");

        let entries = registry.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "root:gated");
        assert_eq!(entries[0].key, "gated-flag");
        assert!(!entries[0].visible);
    }

    #[test]
    fn beta_group_pruned_under_ga_min_stage_kept_under_beta_min_stage() {
        let build_tree = || {
            RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
                .with_command(trivial_command("keep-me"))
                .with_group(
                    RuntimeGroupSpec::new(
                        GroupSpec::new("flagged-group", "short")
                            .with_feature_flag("group-flag", Stage::Beta),
                    )
                    .with_command(trivial_command("cmd-default"))
                    .with_command(flagged_command(
                        "cmd-ga",
                        "cmd-ga-flag",
                        Stage::Ga,
                    )),
                )
        };

        // Default policy (min_stage: Ga) drops the whole Beta subtree, including
        // both its undeclared and explicitly-Ga-declared children, because the
        // ancestor group itself already fails visibility before children are
        // even visited.
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned = prune_feature_flag_tree(
            build_tree(),
            None,
            &empty_policy(),
            &mut prefix,
            &mut registry,
        )
        .expect("root keeps its unflagged sibling command");
        assert!(pruned.groups.is_empty());
        assert_eq!(pruned.commands.len(), 1);
        assert_eq!(pruned.commands[0].spec.name, "keep-me");
        // Only the group itself was recorded; its children were never visited.
        assert_eq!(registry.entries().len(), 1);
        assert_eq!(registry.entries()[0].path, "root:flagged-group");
        assert!(!registry.entries()[0].visible);

        // A Beta-permissive policy keeps the group and both of its children.
        let policy = FlagPolicy::default().with_min_stage(Stage::Beta);
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned =
            prune_feature_flag_tree(build_tree(), None, &policy, &mut prefix, &mut registry)
                .expect("root is kept");
        assert_eq!(pruned.groups.len(), 1);
        assert_eq!(pruned.groups[0].commands.len(), 2);
        assert!(registry.entries().iter().all(|entry| entry.visible));
    }

    #[test]
    fn ancestor_invisibility_short_circuits_before_children_are_visited() {
        // The child declares its own, more permissive Ga flag under a distinct
        // key. Per the documented pruning semantics, an invisible ancestor drops
        // its whole subtree unconditionally: the child's own flag is never even
        // considered, because `prune_feature_flag_tree` returns `None` for the
        // ancestor as soon as its own effective flag fails visibility, before
        // recursing into commands or subgroups at all.
        let group = RuntimeGroupSpec::new(
            GroupSpec::new("ancestor", "short").with_feature_flag("ancestor-flag", Stage::Beta),
        )
        .with_command(flagged_command("child", "child-flag", Stage::Ga));

        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned =
            prune_feature_flag_tree(group, None, &empty_policy(), &mut prefix, &mut registry);

        assert!(
            pruned.is_none(),
            "invisible ancestor drops its whole subtree"
        );
        // The child was never visited, so nothing about it was recorded.
        assert_eq!(registry.entries().len(), 1);
        assert_eq!(registry.entries()[0].path, "ancestor");
        assert!(registry.by_key("child-flag").is_empty());
    }

    #[test]
    fn cascading_inherited_flag_key_and_stage_reach_unflagged_descendants() {
        // Simulates a module-level flag with no per-group/per-command
        // declaration anywhere below it: `inherited` here stands in for
        // `Module::feature_flag`, exactly as `add_module_group_inner` passes it.
        let module_flag = FeatureFlag::new("module-flag", Stage::Beta);
        let group = RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
            .with_command(trivial_command("unflagged-child"));

        let policy = FlagPolicy::default().with_min_stage(Stage::Beta);
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned = prune_feature_flag_tree(
            group,
            Some(&module_flag),
            &policy,
            &mut prefix,
            &mut registry,
        )
        .expect("Beta-permissive policy keeps a Beta-inherited tree");
        assert_eq!(pruned.commands.len(), 1);

        // Both the group and the descendant command recorded the *same*
        // inherited key/stage, proving real cascading rather than an implicit
        // Ga default at either level.
        let entries = registry.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "root");
        assert_eq!(entries[0].key, "module-flag");
        assert_eq!(entries[0].stage, Stage::Beta);
        assert_eq!(entries[1].path, "root:unflagged-child");
        assert_eq!(entries[1].key, "module-flag");
        assert_eq!(entries[1].stage, Stage::Beta);

        // Under the default (Ga) policy the same inherited Beta flag makes the
        // whole tree invisible together, since the group and its unflagged
        // child resolve to the identical effective flag.
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned = prune_feature_flag_tree(
            RuntimeGroupSpec::new(GroupSpec::new("root", "short"))
                .with_command(trivial_command("unflagged-child")),
            Some(&module_flag),
            &empty_policy(),
            &mut prefix,
            &mut registry,
        );
        assert!(pruned.is_none());
    }

    #[test]
    fn registry_records_only_named_flags_not_unflagged_nodes() {
        let group = RuntimeGroupSpec::new(GroupSpec::new("root", "short")).with_group(
            RuntimeGroupSpec::new(
                GroupSpec::new("g", "short").with_feature_flag("g-flag", Stage::Beta),
            )
            .with_command(trivial_command("c1"))
            .with_command(flagged_command("c2", "c2-flag", Stage::Ga)),
        );

        // Permissive enough that nothing is pruned, so every node is visited.
        let policy = FlagPolicy::default().with_min_stage(Stage::Experimental);
        let mut prefix = Vec::new();
        let mut registry = FlagRegistry::new();
        let pruned = prune_feature_flag_tree(group, None, &policy, &mut prefix, &mut registry)
            .expect("permissive policy keeps everything");
        assert_eq!(pruned.groups[0].commands.len(), 2);

        let entries = registry.entries();
        assert_eq!(entries.len(), 3, "root has no flag and is not recorded");
        assert_eq!(entries[0].path, "root:g");
        assert_eq!(entries[0].key, "g-flag");
        assert_eq!(entries[1].path, "root:g:c1");
        assert_eq!(entries[1].key, "g-flag");
        assert_eq!(entries[1].stage, Stage::Beta);
        assert_eq!(entries[2].path, "root:g:c2");
        assert_eq!(entries[2].key, "c2-flag");
        assert_eq!(entries[2].stage, Stage::Ga);
        assert!(entries.iter().all(|entry| entry.visible));
    }

    #[test]
    fn module_feature_flag_cascades_into_its_group_via_add_module() {
        // Regression test for the bug this task fixes: `add_module` used to
        // discard `module.feature_flag` entirely, so a module-level flag could
        // never reach its group/commands. `Module::new` returns a group with an
        // unflagged command; the module itself declares Experimental, and the
        // default (Ga) policy must prune the whole group away.
        let module = Module::new("Test Category", |_ctx| {
            RuntimeGroupSpec::new(GroupSpec::new("gated-mod", "short"))
                .with_command(trivial_command("list"))
        })
        .with_feature_flag("module-flag", Stage::Experimental);

        let mut cli = Cli::new(CliConfig::new("modtest", "Module test", "modtest"));
        cli.add_module(module);

        assert!(
            !cli.commands.contains_key("gated-mod:list"),
            "module-level Experimental flag should have pruned the whole group under the default Ga policy"
        );
        assert!(
            !has_subcommand(&cli.root, "gated-mod"),
            "the pruned group must not be mounted in the clap tree either"
        );
    }

    #[test]
    fn module_feature_flag_keeps_group_when_policy_allows_it() {
        let module = Module::new("Test Category", |_ctx| {
            RuntimeGroupSpec::new(GroupSpec::new("gated-mod-2", "short"))
                .with_command(trivial_command("list"))
        })
        .with_feature_flag("module-flag-2", Stage::Experimental);

        let mut cli = Cli::new(
            CliConfig::new("modtest2", "Module test", "modtest2")
                .with_min_stage(Stage::Experimental),
        );
        cli.add_module(module);

        assert!(cli.commands.contains_key("gated-mod-2:list"));
        assert!(has_subcommand(&cli.root, "gated-mod-2"));
    }

    #[test]
    fn active_environment_min_stage_loosens_consumer_level_policy() {
        // The CliConfig itself leaves min_stage at its Ga default, which would
        // normally prune this Experimental-flagged group. The active ("prod")
        // environment's compiled min_stage override should reach
        // `middleware.flag_policy` before pruning runs and keep it instead.
        let module = Module::new("Test Category", |_ctx| {
            RuntimeGroupSpec::new(GroupSpec::new("gated-mod-3", "short"))
                .with_command(trivial_command("list"))
        })
        .with_feature_flag("module-flag-3", Stage::Experimental);

        let mut cli = Cli::new(
            CliConfig::new("modtest3", "Module test", "modtest3")
                .with_environments(std::sync::Arc::new(
                    crate::environments::Environments::new("prod").with_environment(
                        "prod",
                        crate::environments::EnvTable::new().with("min_stage", "experimental"),
                    ),
                ))
                .with_startup_args(Vec::<&str>::new()),
        );
        cli.add_module(module);

        assert!(cli.commands.contains_key("gated-mod-3:list"));
        assert!(has_subcommand(&cli.root, "gated-mod-3"));
    }

    /// The direct proof of the startup `--env` prescan (see `Cli::new`):
    /// unlike [`active_environment_min_stage_loosens_consumer_level_policy`]
    /// (which exercises the *default* active environment), here "prod" is
    /// the default and carries no override, while "dev" loosens `min_stage`.
    /// A `--env dev` supplied via `with_startup_args` — standing in for real
    /// process argv — must be consulted before `add_module` prunes the tree,
    /// in the *same* construction, not just update `middleware.env` for a
    /// later run.
    #[test]
    fn startup_env_flag_reveals_beta_and_experimental_modules_for_the_named_env() {
        fn gated_module() -> Module {
            Module::new("Test Category", |_ctx| {
                RuntimeGroupSpec::new(GroupSpec::new("gated-mod-4", "short"))
                    .with_command(trivial_command("list"))
            })
            .with_feature_flag("module-flag-4", Stage::Experimental)
        }
        fn environments() -> std::sync::Arc<crate::environments::Environments> {
            std::sync::Arc::new(
                crate::environments::Environments::new("prod")
                    .with_environment("prod", crate::environments::EnvTable::new())
                    .with_environment(
                        "dev",
                        crate::environments::EnvTable::new().with("min_stage", "experimental"),
                    ),
            )
        }

        let mut with_dev_flag = Cli::new(
            CliConfig::new("modtest4a", "Module test", "modtest4a")
                .with_environments(environments())
                .with_startup_args(["modtest4a", "--env", "dev"]),
        );
        with_dev_flag.add_module(gated_module());
        assert!(
            with_dev_flag.commands.contains_key("gated-mod-4:list"),
            "--env dev in startup_args should reveal the Experimental module"
        );
        assert!(has_subcommand(&with_dev_flag.root, "gated-mod-4"));

        // Negative counterpart: with no `--env` at all, the default ("prod",
        // no override) still governs — nothing changed for the common case.
        let mut without_flag = Cli::new(
            CliConfig::new("modtest4b", "Module test", "modtest4b")
                .with_environments(environments())
                .with_startup_args(Vec::<&str>::new()),
        );
        without_flag.add_module(gated_module());
        assert!(
            !without_flag.commands.contains_key("gated-mod-4:list"),
            "without --env, the default env's Ga policy should still prune the module"
        );
        assert!(!has_subcommand(&without_flag.root, "gated-mod-4"));
    }

    static GLOBAL_MIN_STAGE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that restores (or removes) an env var on drop, even if a
    /// test panics.
    struct GlobalMinStageEnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl GlobalMinStageEnvGuard {
        /// Sets `key` to `value`. Caller must hold [`GLOBAL_MIN_STAGE_ENV_LOCK`]
        /// for the guard's entire lifetime.
        #[allow(unsafe_code)]
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: serialized by GLOBAL_MIN_STAGE_ENV_LOCK; guard
            // restores/removes on any exit incl. panic.
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }

        /// Removes `key` (if set). Caller must hold
        /// [`GLOBAL_MIN_STAGE_ENV_LOCK`] for the guard's entire lifetime.
        #[allow(unsafe_code)]
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: serialized by GLOBAL_MIN_STAGE_ENV_LOCK; guard restores
            // on any exit incl. panic.
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }
    impl Drop for GlobalMinStageEnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: test holds GLOBAL_MIN_STAGE_ENV_LOCK; restore/clean up
            // on any exit including panic.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn global_min_stage_override_is_a_noop_when_unset() {
        let _g = GLOBAL_MIN_STAGE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        const VAR: &str = "UNSET_MIN_STAGE_APP_MIN_STAGE";
        // Explicitly unset (and restored on drop) rather than assumed absent,
        // so the test is hermetic even if a developer/CI happens to have this
        // var set.
        let _guard = GlobalMinStageEnvGuard::unset(VAR);

        assert_eq!(global_min_stage_override("unset-min-stage-app"), None);
    }

    #[test]
    #[allow(unsafe_code)]
    fn global_min_stage_override_parses_a_valid_value() {
        let _g = GLOBAL_MIN_STAGE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        const VAR: &str = "VALID_MIN_STAGE_APP_MIN_STAGE";
        let _guard = GlobalMinStageEnvGuard::set(VAR, "beta");

        assert_eq!(
            global_min_stage_override("valid-min-stage-app"),
            Some(Stage::Beta)
        );
    }

    #[test]
    #[allow(unsafe_code)]
    fn global_min_stage_override_ignores_a_malformed_value() {
        let _g = GLOBAL_MIN_STAGE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        const VAR: &str = "BAD_MIN_STAGE_APP_MIN_STAGE";
        let _guard = GlobalMinStageEnvGuard::set(VAR, "nightly");

        assert_eq!(global_min_stage_override("bad-min-stage-app"), None);
    }
}
