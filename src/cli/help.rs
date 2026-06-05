use std::collections::BTreeMap;

use crate::output::NextAction;

/// Help template for the root command. Renders the curated long-about (which
/// already lists every command grouped by category) and usage, but omits
/// clap's auto-generated subcommand list and the global options wall — those
/// are noise on the top-level navigation page.
pub const ROOT_HELP_TEMPLATE: &str = "\
{before-help}{about-with-newline}
{usage-heading} {usage}{after-help}";

/// Help template for group (noun) commands. Keeps the child command list, which
/// is the point of a group page, but drops the global options wall. Leaf
/// commands keep clap's default template so their flags remain documented.
pub const GROUP_HELP_TEMPLATE: &str = "\
{before-help}{about-with-newline}
{usage-heading} {usage}

Commands:
{subcommands}{after-help}";

/// One module row in root long help.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleHelpEntry {
    /// Help category label.
    pub category: String,
    /// Top-level module group name.
    pub name: String,
    /// One-line module description.
    pub short: String,
}

/// Builds the root long help text from module categories and built-in command hints.
#[must_use]
pub fn build_root_long(intro: &str, entries: &[ModuleHelpEntry], has_guide: bool) -> String {
    // Group by category. `BTreeMap` keeps categories in sorted order so the
    // rendered sections are deterministic regardless of registration order.
    let mut by_category = BTreeMap::<&str, Vec<&ModuleHelpEntry>>::new();
    for entry in entries {
        by_category
            .entry(entry.category.as_str())
            .or_default()
            .push(entry);
    }

    let max_width = entries
        .iter()
        .map(|entry| entry.name.len())
        .max()
        .unwrap_or_default();
    let mut out = intro.to_owned();
    for (category, category_entries) in &mut by_category {
        category_entries.sort_by(|left, right| left.name.cmp(&right.name));
        out.push_str(&format!("\n\n  {category}:"));
        for entry in category_entries {
            out.push_str(&format!(
                "\n    {:<width$}  {}",
                entry.name,
                entry.short,
                width = max_width
            ));
        }
    }
    out.push_str("\n\n  Find Commands:");
    out.push_str("\n    --search <keyword>  Search all commands and guides by keyword");
    out.push_str("\n    tree                Display full command tree");
    if has_guide {
        out.push_str("\n    guide               Built-in guides for AI agents and developers");
    }
    out
}

/// Builds a "Next actions" section appended to bare-root human help. Returns an
/// empty string when there are no actions so the help output is unchanged.
#[must_use]
pub fn render_next_actions_human(actions: &[NextAction]) -> String {
    if actions.is_empty() {
        return String::new();
    }
    let max_width = actions
        .iter()
        .map(|action| action.command.len())
        .max()
        .unwrap_or_default();
    let mut out = String::from("\n\n  Suggested next actions:");
    for action in actions {
        out.push_str(&format!(
            "\n    {:<width$}  {}",
            action.command,
            action.description,
            width = max_width
        ));
    }
    out
}
