use std::collections::BTreeMap;

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
    let mut categories = Vec::<String>::new();
    let mut by_category = BTreeMap::<String, Vec<&ModuleHelpEntry>>::new();
    for entry in entries {
        if !by_category.contains_key(&entry.category) {
            categories.push(entry.category.clone());
        }
        by_category
            .entry(entry.category.clone())
            .or_default()
            .push(entry);
    }

    let max_width = entries
        .iter()
        .map(|entry| entry.name.len())
        .max()
        .unwrap_or_default();
    let mut out = intro.to_owned();
    for category in categories {
        out.push_str(&format!("\n\n  {category}:"));
        if let Some(category_entries) = by_category.get(&category) {
            for entry in category_entries {
                out.push_str(&format!(
                    "\n    {:<width$}  {}",
                    entry.name,
                    entry.short,
                    width = max_width
                ));
            }
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
