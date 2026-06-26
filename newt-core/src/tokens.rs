//! Token-estimation settings — the chars-per-token heuristic as a config-backed
//! value threaded explicitly through the estimators (no global state).
//!
//! Lives at the crate root (not under `agentic`) so [`crate::config`] can hold it
//! without a backwards dependency, and so `newt-tui` can reach it for its own
//! probe-side estimates.

use serde::{Deserialize, Serialize};

fn default_chars_per_token() -> usize {
    4
}

/// How many characters of serialized JSON count as one token in the cheap
/// no-tokenizer heuristic. Configured under `[context.estimation]` and threaded
/// through `estimate_tokens` and friends.
///
/// The default (4) is the universal rough heuristic; the per-model calibration
/// `ratio` (model-self-tuning, `docs/design/model-self-tuning.md` §2.3) scales the
/// result on top. Exposed as a setting because a different model/tokenizer may
/// pack a different number of chars per token — a smart default that adapts when
/// an expert (human or LLM) tunes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenEstimation {
    #[serde(default = "default_chars_per_token")]
    pub chars_per_token: usize,
}

impl Default for TokenEstimation {
    fn default() -> Self {
        Self {
            chars_per_token: default_chars_per_token(),
        }
    }
}

impl TokenEstimation {
    /// Construct from an explicit ratio (for tests / call sites that aren't
    /// config-driven). A zero is clamped to 1 so division never panics.
    pub fn new(chars_per_token: usize) -> Self {
        Self {
            chars_per_token: chars_per_token.max(1),
        }
    }

    /// Token estimate for a `chars`-character string (ceiling-divide, so a
    /// 1-char fragment never estimates to zero — err toward counting, 18.1).
    pub fn tokens_for_chars(&self, chars: usize) -> usize {
        chars.div_ceil(self.chars_per_token.max(1))
    }

    /// The inverse: a character budget for `tokens` tokens. Used to size a
    /// summarizer input cap from a token budget.
    pub fn chars_for_tokens(&self, tokens: usize) -> usize {
        tokens.saturating_mul(self.chars_per_token.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_four_chars_per_token() {
        assert_eq!(TokenEstimation::default().chars_per_token, 4);
    }

    #[test]
    fn ratio_drives_both_directions_consistently() {
        let est = TokenEstimation::new(4);
        assert_eq!(est.tokens_for_chars(8), 2);
        assert_eq!(est.tokens_for_chars(9), 3, "ceiling-divide, never zero");
        assert_eq!(est.chars_for_tokens(2), 8);
        // A configured ratio changes estimate AND cap-sizing together — the whole
        // point of one shared setting (#661 follow-up).
        let wide = TokenEstimation::new(5);
        assert_eq!(wide.tokens_for_chars(10), 2);
        assert_eq!(wide.chars_for_tokens(2), 10);
    }

    #[test]
    fn it_deserializes_from_the_context_estimation_table() {
        let est: TokenEstimation = toml::from_str("chars_per_token = 5").unwrap();
        assert_eq!(est.chars_per_token, 5);
        // An omitted field falls back to the default.
        assert_eq!(
            toml::from_str::<TokenEstimation>("")
                .unwrap()
                .chars_per_token,
            4
        );
    }

    #[test]
    fn zero_ratio_is_clamped_so_division_never_panics() {
        assert_eq!(TokenEstimation::new(0).tokens_for_chars(3), 3);
    }
}
