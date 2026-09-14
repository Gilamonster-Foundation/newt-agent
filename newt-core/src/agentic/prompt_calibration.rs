//! Session-local correction of the configured character-count token prior.

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PromptCalibration {
    learned_ratio: Option<f32>,
    has_usage: bool,
}

impl PromptCalibration {
    /// A session observation may tighten the cold/model-cache prior, never
    /// loosen it. Unlike an untrusted cache entry, measured ratios above three
    /// remain usable: discarding them would restore the known-bad cold prior.
    pub(crate) fn ratio(&self, prior: Option<f32>) -> f32 {
        super::super::send_budget::sanitize_estimate_ratio(prior)
            .max(self.learned_ratio.unwrap_or(1.0))
    }

    /// The estimate must describe the very request the server counted, before
    /// appending its answer or executing tools. Missing usage and unusable zero
    /// samples leave the prior untouched; smaller samples cannot erase a known
    /// under-count (some backends report only uncached prompt suffixes).
    pub(crate) fn observe(&mut self, prompt_tokens: Option<u32>, raw_estimate: usize) {
        self.observe_count(prompt_tokens.unwrap_or(0) as usize, raw_estimate);
    }

    /// Tokenizer endpoints may return a count wider than the usage wire's u32.
    pub(crate) fn observe_count(&mut self, prompt_tokens: usize, raw_estimate: usize) {
        if prompt_tokens > 0 && raw_estimate > 0 {
            self.has_usage = true;
            self.learned_ratio = Some(
                self.ratio(None)
                    .max(prompt_tokens as f32 / raw_estimate as f32),
            );
        }
    }

    /// Prefer observed usage over guessing another multiplier. Without usable
    /// usage, multiply the guess by 1.5 up to sanitize_estimate_ratio's 3.0
    /// heuristic ceiling. Measured ratios above that ceiling remain usable.
    /// The caller independently tightens the projection budget on every
    /// overflow, caps shrink attempts at two, and refuses an unchanged request.
    pub(crate) fn overflow(&mut self, applied_ratio: f32) -> f32 {
        let applied = if applied_ratio.is_finite() {
            applied_ratio.max(1.0)
        } else {
            self.ratio(None)
        };
        let current = self.ratio(None).max(applied);
        let ratio = if self.has_usage {
            current
        } else {
            (current * 1.5).min(3.0)
        };
        self.learned_ratio = Some(ratio);
        ratio
    }
}
