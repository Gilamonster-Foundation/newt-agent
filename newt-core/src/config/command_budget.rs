//! Resolved ordinary-command budget, shared by a turn's catalog and dispatcher.

/// Immutable session configuration. Resolving environment overrides belongs at
/// the composition root, never at catalog construction or command dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunCommandBudget(u64);

impl Default for RunCommandBudget {
    fn default() -> Self {
        Self(Self::DEFAULT_SECS)
    }
}

impl RunCommandBudget {
    pub const MAX_SECS: u64 = 300;

    /// The wall for an ordinary command, in seconds: the default agent-bridle's
    /// `LimitsPolicy` carries, held here so this file needs no confinement backend (#3160, the
    /// `confine` feature). `the_default_wall_is_the_one_bridle_carries` keeps the two equal.
    pub const DEFAULT_SECS: u64 = 60;

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

    /// #3160: the constant is bridle's own default, copied so the default needs no confinement
    /// crate; while bridle is a dependency the two must agree.
    #[cfg(feature = "confine")]
    #[test]
    fn the_default_wall_is_the_one_bridle_carries() {
        assert_eq!(
            RunCommandBudget::DEFAULT_SECS,
            agent_bridle::LimitsPolicy::default().default_timeout_secs
        );
    }

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
