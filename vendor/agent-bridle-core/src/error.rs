//! Errors surfaced by the leash and by tools.

use std::fmt;

/// The result type used throughout `agent-bridle-core`.
pub type ToolResult<T> = Result<T, ToolError>;

/// Why a dispatch or a tool invocation failed.
///
/// The first three variants (`Denied`, `Budget`, `Generation`) are *leash*
/// outcomes: the [`crate::Gate`] refused to mint a [`crate::ToolContext`], so
/// the tool never ran. `NotFound` is a registry miss. `Exec` and `Other` are
/// failures from inside a tool that *did* pass the leash.
#[derive(Debug)]
pub enum ToolError {
    /// The requested authority is not within (or below) the granted caveats —
    /// the tool's `required ⊑ granted` check failed, or a per-operation leash
    /// check (`check_exec`, `check_path_*`, `check_net`) denied the operation.
    Denied {
        /// Human-readable reason (safe to surface to the agent).
        reason: String,
    },
    /// No tool registered under the requested name.
    NotFound {
        /// The name that was looked up.
        name: String,
    },
    /// The grant's `max_calls` budget is exhausted.
    Budget,
    /// The gate's generation is not in the grant's `valid_for_generation` set.
    Generation,
    /// A tool that passed the leash failed during execution (I/O, spawn, …).
    Exec(std::io::Error),
    /// Any other failure from inside a tool.
    Other(anyhow::Error),
}

impl ToolError {
    /// Convenience constructor for a denial with a formatted reason.
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }

    /// Convenience constructor for a registry miss.
    pub fn not_found(name: impl Into<String>) -> Self {
        Self::NotFound { name: name.into() }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { reason } => write!(f, "denied: {reason}"),
            Self::NotFound { name } => write!(f, "no such tool: {name}"),
            Self::Budget => write!(f, "denied: call budget (max_calls) exhausted"),
            Self::Generation => {
                write!(f, "denied: grant is not valid for the gate's generation")
            }
            Self::Exec(e) => write!(f, "tool execution failed: {e}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Exec(e) => Some(e),
            Self::Other(e) => e.source(),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        Self::Exec(e)
    }
}

impl From<anyhow::Error> for ToolError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_stable_and_safe() {
        assert_eq!(
            ToolError::denied("nope").to_string(),
            "denied: nope".to_string()
        );
        assert_eq!(
            ToolError::Budget.to_string(),
            "denied: call budget (max_calls) exhausted"
        );
        assert!(ToolError::not_found("shell").to_string().contains("shell"));
    }
}
