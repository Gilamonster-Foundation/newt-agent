/// # Tier classification logic
///
/// This module contains the heuristics that decide which tier a prompt belongs to.
/// The algorithm is intentionally simple and mirrors the original `Router::classify_detailed`
/// implementation. It can be exercised directly for testing or extended without touching
/// the router struct.
///
/// The function `classify_detailed` returns a `Classification` value that includes:
/// - the determined `tier`,
/// - a `confidence` score in the range `[0.0, 1.0]`,
/// - a list of `reasons` explaining the decision.
///
/// The classifier is a pure function; it does not depend on any internal state,
/// making it straightforward to test in isolation.
use super::{Classification, Tier};

pub fn classify_detailed(prompt: &str, tier_override: Option<Tier>) -> Classification {
    if let Some(tier) = tier_override {
        return Classification {
            tier,
            confidence: 1.0,
            reasons: vec!["tier override active".to_string()],
        };
    }

    let len = prompt.len();
    let lower = prompt.to_ascii_lowercase();

    if lower.contains("review") || lower.contains("grade") || lower.contains("critique") {
        return Classification {
            tier: Tier::Review,
            confidence: 0.85,
            reasons: vec!["review keyword detected".to_string()],
        };
    }
    if lower.contains("refactor") || lower.contains("redesign") || lower.contains("architect") {
        return Classification {
            tier: Tier::Complex,
            confidence: 0.80,
            reasons: vec!["complex keyword detected".to_string()],
        };
    }
    if len < 200 {
        return Classification {
            tier: Tier::Fast,
            confidence: 0.70,
            reasons: vec![format!("short prompt ({len} chars < 200)")],
        };
    }
    Classification {
        tier: Tier::Standard,
        confidence: 0.5,
        reasons: vec!["no keyword match, length >= 200".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_returns_given_tier_at_full_confidence() {
        let c = classify_detailed("anything", Some(Tier::Complex));
        assert_eq!(c.tier, Tier::Complex);
        assert!((c.confidence - 1.0).abs() < f64::EPSILON);
        assert!(c.reasons.iter().any(|r| r.contains("override")));
    }

    #[test]
    fn review_keyword_routes_review_tier() {
        for kw in &["review this", "grade my work", "critique the design"] {
            let c = classify_detailed(kw, None);
            assert_eq!(c.tier, Tier::Review, "failed for {kw:?}");
        }
    }

    #[test]
    fn complex_keyword_routes_complex_tier() {
        for kw in &["refactor auth", "redesign the API", "architect a system"] {
            let c = classify_detailed(kw, None);
            assert_eq!(c.tier, Tier::Complex, "failed for {kw:?}");
        }
    }

    #[test]
    fn short_prompt_routes_fast_tier() {
        let c = classify_detailed("fix typo", None);
        assert_eq!(c.tier, Tier::Fast);
        assert!(c.confidence >= 0.5);
    }

    #[test]
    fn long_prompt_without_keywords_routes_standard() {
        let prompt = "x".repeat(200);
        let c = classify_detailed(&prompt, None);
        assert_eq!(c.tier, Tier::Standard);
    }

    #[test]
    fn confidence_is_always_in_unit_interval() {
        for prompt in &[
            "hi",
            "review me",
            "refactor all the things",
            &"x".repeat(300),
        ] {
            let c = classify_detailed(prompt, None);
            assert!(
                (0.0..=1.0).contains(&c.confidence),
                "confidence {:.2} out of [0,1] for {:?}",
                c.confidence,
                prompt
            );
        }
    }
}
