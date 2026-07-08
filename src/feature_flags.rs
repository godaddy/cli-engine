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
#[serde(rename_all = "snake_case")]
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
}
