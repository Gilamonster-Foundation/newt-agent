//! Resolved ordinary-command budget, shared by a turn's catalog and dispatcher.

/// Immutable session configuration. Resolving environment overrides belongs at
/// the composition root, never at catalog construction or command dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunCommandBudget(u64);

impl Default for RunCommandBudget {
    fn default() -> Self {
        Self(agent_bridle::LimitsPolicy::default().default_timeout_secs)
    }
}

impl RunCommandBudget {
    pub const MAX_SECS: u64 = 300;

    /// Capture the operator override once when constructing session configuration.
    pub fn from_env() -> Self {
        Self::from_configured(
            std::env::var("NEWT_RUN_COMMAND_TIMEOUT_SECS")
                .ok()
                .as_deref(),
        )
    }

    /// Pure configuration seam; invalid or zero values retain the default.
    pub fn from_configured(configured: Option<&str>) -> Self {
        Self(Self::default().seconds(configured.and_then(|s| s.trim().parse().ok())))
    }

    /// Select a per-call override without disabling or exceeding the ceiling.
    pub fn seconds(self, requested: Option<u64>) -> u64 {
        requested
            .filter(|n| *n > 0)
            .unwrap_or(self.0)
            .min(Self::MAX_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2747: independent configurations keep their captured budget and bounds.
    #[test]
    fn command_budget_2747_independent_and_bounded() {
        let short = RunCommandBudget::from_configured(Some("1"));
        let normal = RunCommandBudget::default();
        for _ in 0..30 {
            assert_eq!(short.seconds(None), 1);
            assert_eq!(normal.seconds(None), 60);
        }
        for configured in [None, Some("0"), Some("bad"), Some("-1")] {
            assert_eq!(RunCommandBudget::from_configured(configured), normal);
        }
        assert_eq!(
            RunCommandBudget::from_configured(Some(" 900 ")).seconds(None),
            300
        );
        assert_eq!(short.seconds(Some(0)), 1);
        assert_eq!(short.seconds(Some(900)), 300);
        assert_eq!(short.seconds(Some(17)), 17);
    }
}
