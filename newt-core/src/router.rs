//! Tier-based router — NeMoCode inheritance.
//!
//! Heuristics are deliberately small in v0. The classifier returns a `Tier`;
//! the inference layer then asks each registered backend `supports_tier(t)`
//! and picks the first match in config order.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Tier {
    Fast,
    Standard,
    Complex,
    Review,
}

#[derive(Debug, Default)]
pub struct Router;

impl Router {
    pub fn new() -> Self {
        Self
    }

    /// Classify an incoming prompt. v0 heuristics: length + keyword triggers.
    /// Refine with empirical signals before v1.
    pub fn classify(&self, prompt: &str) -> Tier {
        let len = prompt.len();
        let lower = prompt.to_ascii_lowercase();

        if lower.contains("review") || lower.contains("grade") || lower.contains("critique") {
            return Tier::Review;
        }
        if lower.contains("refactor") || lower.contains("redesign") || lower.contains("architect") {
            return Tier::Complex;
        }
        if len < 200 {
            return Tier::Fast;
        }
        Tier::Standard
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
}
