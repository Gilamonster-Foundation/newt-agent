//! Bounded repair of provider-capped replies, never partial tool execution.
//!
//! One owner for all four wires (#2874): a capped tool batch is discarded and
//! retried with a doubled cap when headroom remains; capped prose is kept and
//! continued, and the segments are stitched into the final answer.
use serde_json::{json, Value};

const NOTE: &str = "[loop-guidance] [harness] your last reply was cut off at the output limit; nothing ran. Make the change in smaller steps: use write_file with copy_from for large moves, or several edit_file calls.";
const CONTINUE: &str = "[loop-guidance] [harness] your last reply was cut off at the output limit; continue exactly where it stopped, without repeating what you already wrote.";

/// Ephemeral turn-local allowance; successful tool rounds renew it. Recovery
/// requests do not consume the separate tool-round allowance.
#[derive(Default)]
pub(super) struct Recovery {
    consecutive: u8,
    pub skipped_rounds: usize,
    /// Output cap raised by a retry; it only moves up and holds for the turn.
    raised: Option<u32>,
    /// Visible prose from capped replies that were continued.
    prefix: String,
}

impl Recovery {
    /// Decide whether a capped reply is repaired in-turn. `partial` is its
    /// visible prose, `base` the cap the wire would send without recovery
    /// (`None` when it sends none) and `ceiling` the most it may send.
    pub fn retry(
        &mut self,
        has_tool: bool,
        partial: &str,
        base: Option<u32>,
        ceiling: u32,
        messages: &mut Vec<Value>,
        harness: Option<&super::smart_harness::SmartHarness>,
    ) -> anyhow::Result<bool> {
        // Empty prose (reasoning-only) stays terminal: there is nothing to
        // continue, and #2824 keeps that stop for the operator.
        if self.consecutive >= 2 || (!has_tool && partial.is_empty()) {
            return Ok(false);
        }
        let raised = self.allowance(base).and_then(|cap| next_cap(cap, ceiling));
        self.raised = raised.or(self.raised);
        if has_tool {
            // Keep one current host note, never an assistant/partial-tool
            // exchange. Host history retains each intervention through its
            // existing envelope. With headroom the raised cap is the repair.
            messages.retain(|m| !(m["role"] == "user" && m["content"] == NOTE));
            if raised.is_none() {
                super::smart_harness::push_tool_message(
                    messages,
                    json!({"role":"user", "content":NOTE}),
                    harness,
                    true,
                    "output_limit_recovery",
                )?;
            }
        } else {
            // Visible prose is never discarded: replay it and ask for the rest.
            messages.push(json!({"role":"assistant", "content":partial}));
            super::smart_harness::push_tool_message(
                messages,
                json!({"role":"user", "content":CONTINUE}),
                harness,
                true,
                "output_limit_recovery",
            )?;
            self.prefix.push_str(partial);
        }
        self.consecutive += 1;
        self.skipped_rounds += 1;
        Ok(true)
    }

    /// The cap to send: a raised one wins over the wire's own.
    pub fn allowance(&self, base: Option<u32>) -> Option<u32> {
        self.raised.or(base)
    }

    /// Prefix continued prose onto this round's visible text, consuming the
    /// prefix: the assembled answer is adjudicated and kept in history as one
    /// reply, so a later answer must never be stitched to it again.
    pub fn stitch(&mut self, text: String) -> String {
        let mut whole = std::mem::take(&mut self.prefix);
        whole.push_str(&text);
        whole
    }

    pub fn completed_tool_round(&mut self) {
        self.consecutive = 0;
        self.prefix.clear();
    }
}

/// The cap for the retry after a capped request: doubled, clamped to the
/// ceiling, and `None` when that would not raise it (already at the ceiling,
/// or a zero cap that doubling cannot move).
pub(super) fn next_cap(cap: u32, ceiling: u32) -> Option<u32> {
    Some(cap.saturating_mul(2).min(ceiling)).filter(|next| *next > cap)
}

/// The most a retry may send on the Chat, Responses and Ollama wires — the
/// same `NEWT_GENERATION_MAX_TOKENS` bound `generation_bounds::output_cap`
/// clamps every request to.
pub(super) fn ceiling() -> u32 {
    super::generation_bounds::output_cap(None, None, 0).unwrap_or(16_384)
}

#[cfg(test)]
mod tests {
    use super::next_cap;

    #[test]
    fn next_cap_doubles_within_the_ceiling() {
        assert_eq!(next_cap(1_000, 16_384), Some(2_000));
    }

    #[test]
    fn next_cap_clamps_to_the_ceiling() {
        assert_eq!(next_cap(10_000, 16_384), Some(16_384));
    }

    #[test]
    fn next_cap_at_or_above_the_ceiling_does_not_raise() {
        assert_eq!(next_cap(16_384, 16_384), None);
        assert_eq!(next_cap(20_000, 16_384), None);
    }

    #[test]
    fn next_cap_saturates_instead_of_overflowing() {
        assert_eq!(next_cap(u32::MAX - 1, u32::MAX), Some(u32::MAX));
        assert_eq!(next_cap(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn next_cap_zero_cannot_move() {
        assert_eq!(next_cap(0, 16_384), None);
    }
}
