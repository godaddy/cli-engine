use std::collections::BTreeMap;

use clap::Command;

use super::{CommandSpec, RuntimeCommandSpec};
use crate::{FeatureFlag, Stage};

/// Declarative command group metadata.
///
/// Groups are noun-based containers. They do not run business logic directly;
/// when invoked bare, the CLI renders group help.
///
/// Construct with [`GroupSpec::new`], then configure with the `with_*` builder
/// methods — never as a struct literal. `#[non_exhaustive]` enforces this so
/// the engine can add fields later without a breaking release.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct GroupSpec {
    /// Group command name.
    pub name: String,
    /// One-line group description.
    pub short: String,
    /// Optional long help text.
    pub long: Option<String>,
    /// Alternate group names accepted by the parser.
    pub aliases: Vec<String>,
    /// Whether the group runs but is hidden from discovery output.
    pub hidden: bool,
    /// Declarative child commands used for static tree construction.
    pub commands: Vec<CommandSpec>,
    /// Declarative nested groups used for static tree construction.
    pub groups: Vec<GroupSpec>,
    /// This group's own feature-flag declaration, if any.
    ///
    /// `None` means the group has no explicit stage declaration of its own, in
    /// which case it inherits its effective stage from its nearest ancestor
    /// (enclosing group, then module — nearest declaration wins), implicitly
    /// resolving to [`Stage::Ga`] if nothing in the ancestor chain declares a
    /// flag either; see [`Stage`]'s documentation for why that is its default.
    /// Set with [`with_feature_flag`](GroupSpec::with_feature_flag). This field
    /// only records the group's own declaration; cascading resolution against
    /// the ancestor chain happens when a [`Cli`](crate::Cli) mounts the
    /// enclosing module or parent group.
    pub feature_flag: Option<FeatureFlag>,
}

impl GroupSpec {
    /// Creates a command group with the required name and one-line help.
    #[must_use]
    pub fn new(name: impl Into<String>, short: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            short: short.into(),
            ..Self::default()
        }
    }

    /// Sets expanded group help.
    #[must_use]
    pub fn with_long(mut self, long: impl Into<String>) -> Self {
        self.long = Some(long.into());
        self
    }

    /// Adds one group alias.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Hides or shows this group in discovery output.
    #[must_use]
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Adds one declarative child command.
    #[must_use]
    pub fn with_command(mut self, command: CommandSpec) -> Self {
        self.commands.push(command);
        self
    }

    /// Adds one declarative nested group.
    #[must_use]
    pub fn with_group(mut self, group: GroupSpec) -> Self {
        self.groups.push(group);
        self
    }

    /// Declares this group's own feature flag: the key used for policy overrides
    /// and introspection, and the stage at which it becomes visible.
    #[must_use]
    pub fn with_feature_flag(mut self, key: impl Into<String>, stage: Stage) -> Self {
        self.feature_flag = Some(FeatureFlag::new(key, stage));
        self
    }

    /// Builds the `clap` command for parser registration.
    #[must_use]
    pub fn clap_command(&self) -> Command {
        let mut command = Command::new(self.name.clone()).about(self.short.clone());
        if let Some(long) = &self.long
            && !long.is_empty()
        {
            command = command.long_about(long.clone());
        }
        for alias in &self.aliases {
            command = command.alias(alias.clone());
        }
        if self.hidden {
            command = command.hide(true);
        }
        for group in &self.groups {
            command = command.subcommand(group.clap_command());
        }
        for child in &self.commands {
            command = command.subcommand(child.clap_command());
        }
        command
    }
}

/// Executable command group with runtime children.
///
/// Construct with [`RuntimeGroupSpec::new`], then chain `with_*` methods —
/// never as a struct literal. `#[non_exhaustive]` enforces this so the engine
/// can add fields without a breaking release.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RuntimeGroupSpec {
    /// Declarative group metadata.
    pub group: GroupSpec,
    /// Executable leaf commands under this group.
    pub commands: Vec<RuntimeCommandSpec>,
    /// Executable nested groups under this group.
    pub groups: Vec<RuntimeGroupSpec>,
}

impl RuntimeGroupSpec {
    /// Creates a runtime group from declarative group metadata.
    #[must_use]
    pub fn new(group: GroupSpec) -> Self {
        Self {
            group,
            ..Self::default()
        }
    }

    /// Adds one executable leaf command.
    #[must_use]
    pub fn with_command(mut self, command: RuntimeCommandSpec) -> Self {
        self.commands.push(command);
        self
    }

    /// Adds one executable nested group.
    #[must_use]
    pub fn with_group(mut self, group: RuntimeGroupSpec) -> Self {
        self.groups.push(group);
        self
    }

    /// Builds the `clap` command for parser registration.
    #[must_use]
    pub fn clap_command(&self) -> Command {
        let mut command = Command::new(self.group.name.clone()).about(self.group.short.clone());
        if let Some(long) = &self.group.long
            && !long.is_empty()
        {
            command = command.long_about(long.clone());
        }
        for alias in &self.group.aliases {
            command = command.alias(alias.clone());
        }
        if self.group.hidden {
            command = command.hide(true);
        }
        for group in &self.groups {
            command = command.subcommand(group.clap_command());
        }
        for child in &self.commands {
            command = command.subcommand(child.spec.clap_command());
        }
        command
    }

    pub(crate) fn register_commands(
        &self,
        prefix: &mut Vec<String>,
        out: &mut BTreeMap<String, RuntimeCommandSpec>,
    ) {
        prefix.push(self.group.name.clone());
        for group in &self.groups {
            group.register_commands(prefix, out);
        }
        for command in &self.commands {
            prefix.push(command.spec.name.clone());
            out.insert(prefix.join(":"), command.clone());
            prefix.pop();
        }
        prefix.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_spec_with_feature_flag_sets_key_and_stage() {
        let group = GroupSpec::new("project", "Manage projects")
            .with_feature_flag("my-flag", Stage::Experimental);

        let flag = group
            .feature_flag
            .as_ref()
            .expect("feature flag should be set");
        assert_eq!(flag.key, "my-flag");
        assert_eq!(flag.stage, Stage::Experimental);
    }

    #[test]
    fn group_spec_feature_flag_defaults_to_none() {
        let group = GroupSpec::new("project", "Manage projects");

        assert!(group.feature_flag.is_none());
    }
}
