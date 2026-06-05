//! Integration tests for router tier classification.
//!
//! The router uses these heuristics (v0):
//!   "review" / "grade" / "critique"       → Review
//!   "refactor" / "redesign" / "architect"  → Complex
//!   prompt.len() < 200 (no keyword match)  → Fast
//!   prompt.len() >= 200 (no keyword match) → Standard
//!
//! Tests use prompts that actually trigger the documented heuristics.

use newt_core::router::{Router, Tier};

#[test]
fn short_prompt_with_no_keywords_is_fast() {
    let tier = Router::new().classify("list files in the current directory");
    assert_eq!(tier, Tier::Fast);
}

#[test]
fn long_prompt_with_no_keywords_is_standard() {
    // Must be >= 200 chars with no keyword triggers.
    let prompt = "write a function that reads a file, processes each line by \
                  splitting on commas, converts the first column to an integer, \
                  accumulates a running total, and returns the result as a vector \
                  of parsed values along with the final sum";
    assert!(
        prompt.len() >= 200,
        "test prompt must be >= 200 chars, got {}",
        prompt.len()
    );
    let tier = Router::new().classify(prompt);
    assert_eq!(tier, Tier::Standard);
}

#[test]
fn refactor_keyword_is_complex() {
    let tier = Router::new().classify("refactor the authentication middleware to use traits");
    assert_eq!(tier, Tier::Complex);
}

#[test]
fn redesign_keyword_is_complex() {
    let tier = Router::new().classify("redesign the error handling strategy across all crates");
    assert_eq!(tier, Tier::Complex);
}

#[test]
fn review_keyword_is_review() {
    let tier = Router::new().classify("review this diff for correctness and style");
    assert_eq!(tier, Tier::Review);
}

#[test]
fn critique_keyword_is_review() {
    let tier = Router::new().classify("critique the current API surface for ergonomics");
    assert_eq!(tier, Tier::Review);
}

#[test]
fn with_override_always_returns_override_tier() {
    for override_tier in [Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review] {
        let tier = Router::with_override(override_tier)
            .classify("perform a full security audit and code review of the entire codebase");
        assert_eq!(
            tier, override_tier,
            "override should win regardless of prompt content"
        );
    }
}
