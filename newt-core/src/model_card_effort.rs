//! Responses effort declarations. This extends the existing model-card layer;
//! it does not infer capabilities from model names or thinking support.

use serde::{Deserialize, Serialize};

/// Supported wire vocabulary in increasing semantic effort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl From<crate::role_profile::Cognition> for ReasoningEffort {
    fn from(value: crate::role_profile::Cognition) -> Self {
        use crate::role_profile::Cognition;
        match value {
            Cognition::Zen => Self::Minimal,
            Cognition::Rational => Self::Low,
            Cognition::Thoughtful => Self::Medium,
            Cognition::Meticulous => Self::High,
        }
    }
}

/// A nonempty, strictly increasing advertised subset. Private storage keeps
/// malformed declarations out of manually constructed capabilities as well.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ReasoningEffort>", into = "Vec<ReasoningEffort>")]
pub struct ReasoningEffortLadder(Vec<ReasoningEffort>);

impl TryFrom<Vec<ReasoningEffort>> for ReasoningEffortLadder {
    type Error = String;

    fn try_from(values: Vec<ReasoningEffort>) -> Result<Self, Self::Error> {
        if values.is_empty() || values.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(
                "reasoning_effort must be nonempty, unique, and in increasing semantic order"
                    .into(),
            );
        }
        Ok(Self(values))
    }
}

impl From<ReasoningEffortLadder> for Vec<ReasoningEffort> {
    fn from(value: ReasoningEffortLadder) -> Self {
        value.0
    }
}

impl ReasoningEffortLadder {
    #[must_use]
    pub fn values(&self) -> &[ReasoningEffort] {
        &self.0
    }

    #[must_use]
    pub fn maximum(&self) -> ReasoningEffort {
        *self
            .0
            .last()
            .expect("a validated effort ladder is nonempty")
    }
}

/// Explicit Responses controls, independent of Chat template thinking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesCapability {
    /// Absence retains the existing four fixed-level projections. A declaration
    /// constrains requests to exactly these values; it is not a thinking toggle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffortLadder>,
}

impl ResponsesCapability {
    pub(super) fn merge(self, overlay: Self) -> Self {
        Self {
            reasoning_effort: overlay.reasoning_effort.or(self.reasoning_effort),
        }
    }

    /// Resolve one fixed semantic selection against the captured declaration.
    ///
    /// # Errors
    /// An explicitly declared ladder does not accept the selected effort.
    pub fn resolve(
        &self,
        cognition: Option<crate::role_profile::Cognition>,
    ) -> Result<Option<ReasoningEffort>, String> {
        let Some(effort) = cognition.map(ReasoningEffort::from) else {
            return Ok(None);
        };
        if let Some(ladder) = &self.reasoning_effort {
            if !ladder.values().contains(&effort) {
                let accepted = ladder
                    .values()
                    .iter()
                    .map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!("reasoning effort `{}` is not advertised by this Responses endpoint (accepted: {accepted}); choose a supported cognition level", effort.as_str()));
            }
        }
        Ok(Some(effort))
    }
}
