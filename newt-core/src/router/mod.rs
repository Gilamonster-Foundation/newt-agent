//! Tier-based router — NeMoCode inheritance.
//!
//! Heuristics are deliberately small in v0. The classifier returns a `Tier`;
//! the inference layer then asks each registered backend `supports_tier(t)`
//! and picks the first match in config order.

use serde::{Deserialize, Serialize};

mod classifier;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Tier {
    Fast,
    Standard,
    Complex,
    Review,
}

/// Result of [`Router::classify_detailed`]: the chosen tier, a confidence
/// score in `[0.0, 1.0]`, and human-readable reasons explaining the decision.
#[derive(Debug, Clone)]
pub struct Classification {
    pub tier: Tier,
    pub confidence: f64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Router {
    tier_override: Option<Tier>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            tier_override: None,
        }
    }

    /// Create a router that always returns the given tier (confidence 1.0).
    pub fn with_override(tier: Tier) -> Self {
        Self {
            tier_override: Some(tier),
        }
    }

    /// Classify an incoming prompt. v0 heuristics: length + keyword triggers.
    /// Refine with empirical signals before v1.
    pub fn classify(&self, prompt: &str) -> Tier {
        self.classify_detailed(prompt).tier
    }

    /// Like [`classify`](Self::classify), but returns confidence and reasons
    /// alongside the tier.
    pub fn classify_detailed(&self, prompt: &str) -> Classification {
        classifier::classify_detailed(prompt, self.tier_override)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_prompt_is_fast() {
        assert_eq!(Router::new().classify("rename foo to bar"), Tier::Fast);
    }

    #[test]
    fn review_keyword_routes_review() {
        assert_eq!(Router::new().classify("review this diff"), Tier::Review);
    }

    #[test]
    fn refactor_keyword_routes_complex() {
        assert_eq!(
            Router::new().classify("refactor the auth middleware to use traits"),
            Tier::Complex
        );
    }

    #[test]
    fn classify_detailed_short_has_confidence() {
        let c = Router::new().classify_detailed("fix typo");
        assert_eq!(c.tier, Tier::Fast);
        assert!(c.confidence >= 0.5, "expected confidence >= 0.5");
    }

    #[test]
    fn classify_detailed_review_has_reasons() {
        let c = Router::new().classify_detailed("review this PR");
        assert!(!c.reasons.is_empty(), "reasons should be non-empty");
        assert!(
            c.reasons.iter().any(|r| r.contains("review")),
            "reasons should mention 'review'"
        );
    }

    #[test]
    fn classify_detailed_confidence_bounded() {
        for prompt in &["hi", "review me", "refactor everything", &"x".repeat(300)] {
            let c = Router::new().classify_detailed(prompt);
            assert!(
                (0.0..=1.0).contains(&c.confidence),
                "confidence {} out of [0,1] for {:?}",
                c.confidence,
                c.tier
            );
        }
    }

    #[test]
    fn override_always_returns_its_tier() {
        assert_eq!(
            Router::with_override(Tier::Complex).classify("short"),
            Tier::Complex
        );
    }

    #[test]
    fn override_has_full_confidence() {
        let c = Router::with_override(Tier::Fast).classify_detailed("anything");
        assert!(
            (c.confidence - 1.0).abs() < f64::EPSILON,
            "override confidence should be 1.0"
        );
    }

    #[test]
    fn override_reason_mentions_override() {
        let c = Router::with_override(Tier::Review).classify_detailed("whatever");
        assert!(
            c.reasons.iter().any(|r| r.contains("override")),
            "reasons should mention 'override'"
        );
    }
}
