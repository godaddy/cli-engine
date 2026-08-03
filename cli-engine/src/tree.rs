use clap::Command;
use serde::{Deserialize, Serialize};

/// Command tree node used by the built-in `tree` command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TreeNode {
    /// Command or group name.
    pub name: String,
    /// One-line command or group description.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Space-separated display path, including the root command.
    pub path: String,
    /// Visible child commands and groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// Creates a tree node with no children.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            path: path.into(),
            children: Vec::new(),
        }
    }

    /// Adds one child node.
    #[must_use]
    pub fn with_child(mut self, child: TreeNode) -> Self {
        self.children.push(child);
        self
    }

    /// Adds several child nodes.
    #[must_use]
    pub fn with_children(mut self, children: impl IntoIterator<Item = TreeNode>) -> Self {
        self.children.extend(children);
        self
    }
}

/// Builds a tree node from explicit parts.
#[must_use]
pub fn build_tree_from_parts(
    name: impl Into<String>,
    description: impl Into<String>,
    path: impl Into<String>,
    children: Vec<TreeNode>,
) -> TreeNode {
    TreeNode::new(name, description, path).with_children(children)
}

/// Builds a tree from a `clap` command hierarchy.
#[must_use]
pub fn build_tree_from_clap(command: &Command) -> TreeNode {
    build_tree_from_clap_with_path(command, command.get_name().to_owned())
}

fn build_tree_from_clap_with_path(command: &Command, path: String) -> TreeNode {
    let children = command
        .get_subcommands()
        .filter(|child| !child.is_hide_set() && child.get_name() != "completion")
        .map(|child| {
            let child_path = format!("{path} {}", child.get_name());
            build_tree_from_clap_with_path(child, child_path)
        })
        .collect::<Vec<_>>();

    TreeNode {
        name: command.get_name().to_owned(),
        description: command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default(),
        path,
        children,
    }
}

/// Renders a command tree for human output.
#[must_use]
pub fn render_tree_human(node: &TreeNode) -> String {
    let mut out = String::new();
    render_node(node, "", true, true, &mut out);
    out
}

fn render_node(node: &TreeNode, prefix: &str, is_last: bool, is_root: bool, out: &mut String) {
    if is_root {
        out.push_str(&node.name);
        out.push('\n');
    } else {
        let connector = if is_last { "└── " } else { "├── " };
        out.push_str(prefix);
        out.push_str(connector);
        out.push_str(&node.name);
        if !node.description.is_empty() {
            out.push_str(" ··· ");
            out.push_str(&node.description);
        }
        out.push('\n');
    }

    let child_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };
    let child_len = node.children.len();
    for (index, child) in node.children.iter().enumerate() {
        render_node(child, &child_prefix, index + 1 == child_len, false, out);
    }
}
