//! Command-tree lookup helpers: resolving colon-separated paths and help
//! targets against the `clap` tree, and building `search` documents from it.

use std::{io::Write, path::Path};

use clap::Command;

use super::Cli;
use crate::search::SearchDocument;

pub(super) fn find_command_by_colon_path<'command>(
    root: &'command Command,
    path: &str,
) -> Option<&'command Command> {
    find_command_and_canonical_path_by_colon_path(root, path).map(|(command, _)| command)
}

pub(super) fn find_help_target<'command>(
    root: &'command Command,
    parts: &[&str],
) -> Option<&'command Command> {
    let mut current = root;
    let mut matched_any = false;
    for part in parts {
        let Some(next) = current.find_subcommand(part) else {
            break;
        };
        current = next;
        matched_any = true;
    }
    matched_any.then_some(current)
}

fn find_command_and_canonical_path_by_colon_path<'command>(
    root: &'command Command,
    path: &str,
) -> Option<(&'command Command, Vec<String>)> {
    if path.is_empty() {
        return Some((root, Vec::new()));
    }
    let mut current = root;
    let mut canonical = Vec::new();
    for part in path.split(':') {
        current = current.find_subcommand(part)?;
        canonical.push(current.get_name().to_owned());
    }
    Some((current, canonical))
}

fn canonical_path_from_parts(root: &Command, parts: &[String]) -> Option<String> {
    if parts.is_empty() {
        return Some(String::new());
    }
    let mut current = root;
    let mut canonical = Vec::new();
    for part in parts {
        current = current.find_subcommand(part)?;
        canonical.push(current.get_name().to_owned());
    }
    Some(canonical.join(":"))
}

/// Best-effort stderr hint for a `--scope` value that didn't resolve to a
/// known command path — `resolve_search_scope` still searches everything
/// (matching a bare `search` with no `--scope` at all), so this is the only
/// signal the user gets that their scope was ignored rather than applied.
/// Written directly to a locked stderr handle (not `eprintln!`), matching
/// the transport module's own `StderrTransportLogger` convention for this
/// kind of side-channel diagnostic: best-effort, so a write failure is
/// discarded rather than surfaced as a command error.
fn warn_unresolvable_search_scope(scope_path: &str) {
    let mut stderr = std::io::stderr().lock();
    stderr
        .write_all(
            format!(
                "warning: --scope {scope_path:?} did not match a known command path; searching everything instead\n"
            )
            .as_bytes(),
        )
        .ok();
}

fn collect_command_search_documents(
    command: &Command,
    prefix: &mut Vec<String>,
    aliases: &mut Vec<String>,
    docs: &mut Vec<SearchDocument>,
) {
    if command.is_hide_set() || super::config::BUILTIN_COMMAND_NAMES.contains(&command.get_name()) {
        return;
    }
    if command.get_subcommands().next().is_some() {
        for child in command.get_subcommands() {
            prefix.push(child.get_name().to_owned());
            let alias_len = aliases.len();
            append_command_alias_terms(child, aliases);
            collect_command_search_documents(child, prefix, aliases, docs);
            aliases.truncate(alias_len);
            prefix.pop();
        }
        return;
    }
    if prefix.is_empty() {
        prefix.push(command.get_name().to_owned());
        append_command_alias_terms(command, aliases);
    }
    let path = prefix.join(" ");
    let alias_text = aliases.join(" ");
    docs.push(SearchDocument {
        id: format!("cmd:{path}"),
        kind: "command".to_owned(),
        title: path,
        summary: command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default(),
        content: format!(
            "{} {} {} {}",
            command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default(),
            command
                .get_long_about()
                .map(ToString::to_string)
                .unwrap_or_default(),
            command_flag_text(command),
            alias_text
        ),
    });
    if prefix.len() == 1 && prefix[0] == command.get_name() {
        prefix.pop();
    }
}

fn append_command_alias_terms(command: &Command, aliases: &mut Vec<String>) {
    aliases.extend(command.get_all_aliases().map(str::to_owned));
    aliases.extend(
        command
            .get_all_short_flag_aliases()
            .map(|alias| alias.to_string()),
    );
    aliases.extend(command.get_all_long_flag_aliases().map(str::to_owned));
}

fn command_flag_text(command: &Command) -> String {
    command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter_map(|arg| {
            let mut names = Vec::new();
            if let Some(short) = arg.get_short() {
                names.push(format!("-{short}"));
            }
            if let Some(long) = arg.get_long() {
                names.push(format!("--{long}"));
            }
            if let Some(short_aliases) = arg.get_all_short_aliases() {
                names.extend(
                    short_aliases
                        .into_iter()
                        .map(|short_alias| format!("-{short_alias}")),
                );
            }
            if let Some(aliases) = arg.get_all_aliases() {
                names.extend(aliases.into_iter().map(|alias| format!("--{alias}")));
            }
            (!names.is_empty()).then(|| names.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn has_subcommand(command: &Command, name: &str) -> bool {
    command
        .get_subcommands()
        .any(|child| child.get_name() == name)
}

pub(super) fn has_root_version_flag(args: &[String], root: &Command, root_name: &str) -> bool {
    let bool_flags = crate::flags::derive_bool_flags(root);
    let value_flags = crate::flags::derive_value_flags(root);
    let mut iter = args.iter().peekable();
    if iter
        .peek()
        .is_some_and(|arg| arg_matches_root_name(arg, root_name))
    {
        iter.next();
    }

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--version" | "-v" => return true,
            "--" => return false,
            value if value.contains('=') || bool_flags.contains(value) => continue,
            value
                if value_flags.contains(value)
                    || unknown_flag_consumes_value(value, iter.peek()) =>
            {
                iter.next();
            }
            value if value.starts_with('-') => {}
            _ => return false,
        }
    }
    false
}

pub(super) fn normalize_optional_global_flags_before_command(
    root: &Command,
    args: &[String],
) -> Vec<String> {
    let optional_string_defaults =
        std::collections::BTreeMap::from([("--verbose", "all"), ("--debug", "*")]);
    let optional_bool_defaults =
        std::collections::BTreeMap::from([("--dry-run", "true"), ("--schema", "true")]);
    let mut normalized = Vec::with_capacity(args.len());
    let mut index = 0;
    let mut current = root;
    while index < args.len() {
        let arg = &args[index];
        if index == 0 && arg_matches_root_name(arg, root.get_name()) {
            normalized.push(arg.clone());
            index += 1;
            continue;
        }

        if let Some(default) = optional_bool_defaults.get(arg.as_str()) {
            normalized.push(format!("{arg}={default}"));
            index += 1;
            continue;
        }

        if let Some(default) = optional_string_defaults.get(arg.as_str()) {
            match args.get(index + 1) {
                None => {
                    normalized.push(format!("{arg}={default}"));
                    index += 1;
                    continue;
                }
                Some(next)
                    if current.get_name() == root.get_name()
                        || next.starts_with('-')
                        || direct_subcommand(current, next).is_some() =>
                {
                    normalized.push(format!("{arg}={default}"));
                    index += 1;
                    continue;
                }
                Some(next) => {
                    normalized.push(arg.clone());
                    normalized.push(next.clone());
                    index += 2;
                    continue;
                }
            }
        }

        normalized.push(arg.clone());
        if !arg.starts_with('-')
            && let Some(next_command) = direct_subcommand(current, arg)
        {
            current = next_command;
        }
        index += 1;
    }
    normalized
}

fn direct_subcommand<'command>(
    command: &'command Command,
    token: &str,
) -> Option<&'command Command> {
    command.get_subcommands().find(|child| {
        child.get_name() == token || child.get_all_aliases().any(|alias| alias == token)
    })
}

pub(super) fn unknown_flag_consumes_value(arg: &str, next: Option<&&String>) -> bool {
    arg.starts_with('-') && next.is_some_and(|value| !value.starts_with('-'))
}

pub(super) fn arg_matches_root_name(arg: &str, root_name: &str) -> bool {
    arg == root_name
        || Path::new(arg)
            .file_stem()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == root_name)
}

pub(super) fn search_documents(cli: &Cli, scope: &str) -> Vec<SearchDocument> {
    let (scoped, mut prefix) = find_command_and_canonical_path_by_colon_path(&cli.root, scope)
        .unwrap_or((&cli.root, Vec::new()));
    let mut docs = Vec::new();
    let mut aliases = Vec::new();
    append_command_alias_terms(scoped, &mut aliases);
    collect_command_search_documents(scoped, &mut prefix, &mut aliases, &mut docs);
    if scope.is_empty() {
        for entry in &cli.guide_entries {
            docs.push(SearchDocument {
                id: format!("guide:{}", entry.name),
                kind: "guide".to_owned(),
                title: format!("guide {}", entry.name),
                summary: entry.summary.clone(),
                content: format!("{} {}", entry.summary, entry.content),
            });
        }
        if let Some(extra_search_docs) = &cli.extra_search_docs {
            docs.extend(extra_search_docs());
        }
    }
    docs
}

/// Resolves `--scope`'s colon-separated path (e.g. `domain` or
/// `domain:list`) to the canonical scope string [`search_documents`]
/// expects, matching aliases the same way a real command path would (via
/// [`canonical_path_from_parts`]'s `find_subcommand` walk). An empty or
/// unresolvable scope falls back to an unscoped (root) search rather than
/// erroring — `search` staying permissive here matches how a typo in a
/// search *query* just yields fewer results instead of a hard failure.
/// An unresolvable (non-empty) scope prints a best-effort stderr hint
/// first, so a typo like `--scope doamin` doesn't silently widen the
/// search with no explanation for the extra results.
pub(super) fn resolve_search_scope(cli: &Cli, scope_path: &str) -> String {
    if scope_path.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = scope_path.split(':').map(str::to_owned).collect();
    match canonical_path_from_parts(&cli.root, &parts) {
        Some(scope) => scope,
        None => {
            warn_unresolvable_search_scope(scope_path);
            String::new()
        }
    }
}

pub(super) fn canonical_command_path(cli: &Cli, command_path: &str) -> String {
    find_command_and_canonical_path_by_colon_path(&cli.root, command_path).map_or_else(
        || command_path.to_owned(),
        |(_, canonical)| canonical.join(":"),
    )
}
