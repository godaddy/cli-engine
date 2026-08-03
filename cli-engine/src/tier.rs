use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// Command risk tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Safe read-only behavior.
    Read,
    /// State-changing behavior.
    Mutate,
    /// Irreversible or high-risk state-changing behavior.
    Destructive,
}

impl Tier {
    /// Returns true for tiers that should short-circuit under `--dry-run`.
    #[must_use]
    pub const fn is_mutating(self) -> bool {
        matches!(self, Self::Mutate | Self::Destructive)
    }

    /// Returns the wire string for the tier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Mutate => "mutate",
            Self::Destructive => "destructive",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Tier {
    type Err = ParseTierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "read" => Ok(Self::Read),
            "mutate" => Ok(Self::Mutate),
            "destructive" => Ok(Self::Destructive),
            other => Err(ParseTierError {
                value: other.to_owned(),
            }),
        }
    }
}

/// Error returned when parsing an unknown risk tier.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid tier {value:?}: must be one of read, mutate, destructive")]
pub struct ParseTierError {
    value: String,
}
