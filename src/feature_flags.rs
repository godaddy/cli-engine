//! Stage-based feature flagging primitives.
//!
//! These types describe *readiness gating*: a command, group, or module can declare
//! the [`Stage`] at which it becomes visible, and a run-wide [`FlagPolicy`] decides
//! whether that stage (or an override for a specific flag key) is currently enabled.

use std::{collections::BTreeMap, fmt, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Feature readiness stage, used to gate commands/groups/modules before they are
/// fully promoted to general availability.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Early, unstable functionality; visible only when explicitly opted in.
    Experimental,
    /// Functionally complete but still gathering feedback before general availability.
    Beta,
    /// Fully promoted and visible by default.
    Ga,
}

impl Stage {
    /// Returns the wire string for the stage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Experimental => "experimental",
            Self::Beta => "beta",
            Self::Ga => "ga",
        }
    }
}

impl Default for Stage {
    /// Every command with no explicit stage declaration is implicitly [`Stage::Ga`].
    fn default() -> Self {
        Self::Ga
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Stage {
    type Err = ParseStageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "experimental" => Ok(Self::Experimental),
            "beta" => Ok(Self::Beta),
            "ga" => Ok(Self::Ga),
            other => Err(ParseStageError {
                value: other.to_owned(),
            }),
        }
    }
}

/// Error returned when parsing an unknown feature stage.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid stage {value:?}: must be one of experimental, beta, ga")]
pub struct ParseStageError {
    value: String,
}

/// A named feature flag: a key (used for policy overrides and introspection) paired
/// with the stage at which the flagged node becomes visible.
#[derive(Debug, Clone)]
pub struct FeatureFlag {
    /// Stable identifier used for policy overrides and introspection.
    pub key: String,
    /// Stage at which the flagged node becomes visible.
    pub stage: Stage,
}

impl FeatureFlag {
    /// Creates a new feature flag with the given key and stage.
    #[must_use]
    pub fn new(key: impl Into<String>, stage: Stage) -> Self {
        Self {
            key: key.into(),
            stage,
        }
    }
}

/// The fully-merged decision inputs for one CLI run: the minimum stage required for
/// a node to be visible, plus any per-key overrides that force a specific effective
/// stage regardless of the node's declared stage.
#[derive(Debug, Clone)]
pub struct FlagPolicy {
    /// Minimum stage a node must meet (or exceed) to be visible.
    pub min_stage: Stage,
    /// Per-key overrides that substitute a forced effective stage for a flag key,
    /// in place of the node's own declared stage, when checking visibility.
    pub overrides: BTreeMap<String, Stage>,
}

impl Default for FlagPolicy {
    fn default() -> Self {
        Self {
            min_stage: Stage::Ga,
            overrides: BTreeMap::new(),
        }
    }
}

impl FlagPolicy {
    /// Creates a new policy with the default minimum stage ([`Stage::Ga`]) and no
    /// overrides.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the minimum stage required for a node to be visible.
    #[must_use]
    pub fn with_min_stage(mut self, stage: Stage) -> Self {
        self.min_stage = stage;
        self
    }

    /// Adds (or replaces) a per-key override that forces an effective stage for the
    /// given flag key, regardless of the node's own declared stage.
    #[must_use]
    pub fn with_override(mut self, key: impl Into<String>, stage: Stage) -> Self {
        self.overrides.insert(key.into(), stage);
        self
    }

    /// Returns whether a node is visible under this policy.
    ///
    /// If `key` is `Some` and an override is registered for it, the override's stage
    /// substitutes for `stage` in the comparison against [`Self::min_stage`].
    /// Otherwise, the node's own `stage` is compared directly against
    /// [`Self::min_stage`].
    #[must_use]
    pub fn visible(&self, key: Option<&str>, stage: Stage) -> bool {
        let effective = key
            .and_then(|key| self.overrides.get(key))
            .copied()
            .unwrap_or(stage);
        effective >= self.min_stage
    }
}

/// One flagged node discovered while pruning a command tree.
///
/// `path` is the colon-separated command path of the node (module/group/
/// command name chain), matching the same convention used elsewhere in this
/// crate for command paths — e.g. a `list` command nested under a `project`
/// group records `"project:list"`. `key` and `stage` are the flag that
/// resolved for this node (its own declaration, or the nearest ancestor's,
/// per cascading resolution). `visible` is whether the policy that produced
/// this entry judged the node visible.
///
/// Only nodes that resolve to a *named* flag are recorded; a node with no
/// flag anywhere in its ancestor chain implicitly resolves to [`Stage::Ga`]
/// with no key and is not recorded (nothing to introspect).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagEntry {
    /// Colon-separated command path of the flagged node.
    pub path: String,
    /// Flag key that resolved for this node (own declaration or inherited).
    pub key: String,
    /// Stage the resolved flag key declared.
    pub stage: Stage,
    /// Whether the node was judged visible under the policy that produced it.
    pub visible: bool,
}

/// Every flagged module/group/command path discovered while pruning a
/// command tree, in registration order.
///
/// Populated once, when a [`Cli`](crate::Cli) mounts a module or group and
/// resolves cascading feature flags across its tree. Powers `flags
/// list`/`flags info` introspection (a later addition); for now this is
/// stored on [`Middleware`](crate::Middleware) and populated as a side effect
/// of pruning.
#[derive(Debug, Clone, Default)]
pub struct FlagRegistry {
    entries: Vec<FlagEntry>,
}

impl FlagRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one flagged node.
    pub fn record(&mut self, entry: FlagEntry) {
        self.entries.push(entry);
    }

    /// Returns every recorded entry, in the order they were recorded.
    #[must_use]
    pub fn entries(&self) -> &[FlagEntry] {
        &self.entries
    }

    /// Returns every recorded entry whose flag key matches `key`.
    #[must_use]
    pub fn by_key(&self, key: &str) -> Vec<&FlagEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.key == key)
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn stage_ordering() {
        assert!(Stage::Experimental < Stage::Beta);
        assert!(Stage::Beta < Stage::Ga);
        assert!(Stage::Experimental < Stage::Ga);
    }

    #[test]
    fn stage_default_is_ga() {
        assert_eq!(Stage::default(), Stage::Ga);
    }

    #[test]
    fn stage_from_str_round_trips() {
        assert_eq!(
            "experimental".parse::<Stage>().unwrap(),
            Stage::Experimental
        );
        assert_eq!("beta".parse::<Stage>().unwrap(), Stage::Beta);
        assert_eq!("ga".parse::<Stage>().unwrap(), Stage::Ga);
    }

    #[test]
    fn stage_from_str_rejects_unknown() {
        let err = "nightly".parse::<Stage>().unwrap_err();
        assert_eq!(
            err,
            ParseStageError {
                value: "nightly".to_owned(),
            }
        );
    }

    #[test]
    fn flag_policy_default_is_ga_with_no_overrides() {
        let policy = FlagPolicy::default();
        assert_eq!(policy.min_stage, Stage::Ga);
        assert!(policy.overrides.is_empty());
        assert!(!policy.visible(None, Stage::Beta));
        assert!(policy.visible(None, Stage::Ga));
    }

    #[test]
    fn flag_policy_override_precedence() {
        let policy = FlagPolicy::new()
            .with_min_stage(Stage::Ga)
            .with_override("my-flag", Stage::Beta);
        // Override stage (Beta) is compared against min_stage (Ga), not the node's
        // own declared stage (Experimental).
        assert!(!policy.visible(Some("my-flag"), Stage::Experimental));

        let policy = FlagPolicy::new()
            .with_min_stage(Stage::Beta)
            .with_override("my-flag", Stage::Beta);
        assert!(policy.visible(Some("my-flag"), Stage::Experimental));
    }

    #[test]
    fn flag_policy_no_override_falls_back_to_node_stage() {
        let policy = FlagPolicy::new().with_min_stage(Stage::Beta);
        assert!(!policy.visible(Some("other-flag"), Stage::Experimental));
        assert!(policy.visible(Some("other-flag"), Stage::Beta));
        assert!(policy.visible(None, Stage::Ga));
    }

    #[test]
    fn flag_registry_starts_empty() {
        let registry = FlagRegistry::new();
        assert!(registry.entries().is_empty());
        assert!(registry.by_key("anything").is_empty());
    }

    #[test]
    fn flag_registry_records_entries_in_order() {
        let mut registry = FlagRegistry::new();
        registry.record(FlagEntry {
            path: "project".to_owned(),
            key: "flag-a".to_owned(),
            stage: Stage::Beta,
            visible: true,
        });
        registry.record(FlagEntry {
            path: "project:list".to_owned(),
            key: "flag-b".to_owned(),
            stage: Stage::Experimental,
            visible: false,
        });

        let entries = registry.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "project");
        assert_eq!(entries[1].path, "project:list");
    }

    #[test]
    fn flag_registry_by_key_filters() {
        let mut registry = FlagRegistry::new();
        registry.record(FlagEntry {
            path: "project".to_owned(),
            key: "flag-a".to_owned(),
            stage: Stage::Beta,
            visible: true,
        });
        registry.record(FlagEntry {
            path: "project:list".to_owned(),
            key: "flag-a".to_owned(),
            stage: Stage::Beta,
            visible: true,
        });
        registry.record(FlagEntry {
            path: "domain".to_owned(),
            key: "flag-b".to_owned(),
            stage: Stage::Experimental,
            visible: false,
        });

        let matches = registry.by_key("flag-a");
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|entry| entry.key == "flag-a"));

        assert!(registry.by_key("no-such-flag").is_empty());
    }
}
