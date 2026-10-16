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
    /// Output cap raised by a retry; it moves up and holds for the turn,
    /// re-clamped down to each round's recomputed headroom (#2907 P2 1)
    /// and dropped when no room remains.
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
        // #2907 P2 1: the recomputed headroom bounds the cap actually SENT,
        // not only proposed increases — a retained raise (`next_cap` could not
        // raise it) is re-clamped DOWN to this round's room. Zero headroom
        // drops the raise entirely: the note-only retry stands on the wire's
        // own cap, never a zeroed one.
        self.raised = raised
            .or(self.raised)
            .map(|cap| cap.min(ceiling))
            .filter(|cap| *cap > 0);
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

/// Clamp a retry ceiling to the room a raised cap can actually occupy: the
/// window left after the capped reply's actual input (#2874 C1, §1.6 item 2
/// — a raised cap is sent WITHOUT admission, so on a small window an
/// unclamped raise would overflow the request). Unknown window or input
/// leaves the ceiling unchanged (today's behaviour); input at or past the
/// window clamps to zero — no raise is admissible, the note-only retry
/// stands. Recomputed each round from that round's actual input.
pub(super) fn window_clamp(ceiling: u32, window: Option<u32>, input: Option<u32>) -> u32 {
    match (window, input) {
        (Some(window), Some(input)) => ceiling.min(window.saturating_sub(input)),
        _ => ceiling,
    }
}

#[cfg(test)]
mod tests {
    use super::{next_cap, window_clamp, Recovery};

    /// #2907 P2 1: a retained raise is re-clamped to THIS round's
    /// recomputed headroom — the cap actually sent can never exceed the
    /// room the newest capped reply leaves, even when `next_cap` cannot
    /// raise it and the old raise is retained.
    #[test]
    fn retained_raise_is_reclamped_to_the_recomputed_headroom() {
        let mut recovery = Recovery::default();
        let mut messages = Vec::new();
        // Round 1: cap 5000 inside 16384 headroom → raise to 10000.
        assert!(recovery
            .retry(true, "", Some(5_000), 16_384, &mut messages, None)
            .unwrap());
        assert_eq!(recovery.allowance(Some(5_000)), Some(10_000));
        // Round 2: the capped reply reports 12000 input of a 20000 window —
        // only 8000 remains, so the RETAINED 10000 must come back down.
        assert!(recovery
            .retry(true, "", Some(5_000), 8_000, &mut messages, None)
            .unwrap());
        assert_eq!(
            recovery.allowance(Some(5_000)),
            Some(8_000),
            "the cap actually sent is re-clamped to this round's headroom"
        );
    }

    /// #2907 P2 1: zero headroom drops the raise entirely — the note-only
    /// retry stands on the wire's own cap, never a zeroed one.
    #[test]
    fn zero_headroom_drops_the_retained_raise() {
        let mut recovery = Recovery::default();
        let mut messages = Vec::new();
        assert!(recovery
            .retry(true, "", Some(5_000), 16_384, &mut messages, None)
            .unwrap());
        assert!(recovery
            .retry(true, "", Some(5_000), 0, &mut messages, None)
            .unwrap());
        assert_eq!(
            recovery.allowance(Some(5_000)),
            Some(5_000),
            "no headroom means no admissible raise: the base cap stands"
        );
    }

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

    /// #2874 C1 §1.5 test 9: a probed ceiling larger than the window leaves
    /// only the window minus this round's actual input.
    #[test]
    fn window_clamp_clamps_to_the_window_minus_input() {
        assert_eq!(window_clamp(128_000, Some(12_424), Some(40)), 12_384);
    }

    /// #2874 C1's derived finding: a raise beyond the window would be sent
    /// without admission — the clamp blocks it.
    #[test]
    fn window_clamp_blocks_raises_beyond_the_window() {
        assert_eq!(window_clamp(16_384, Some(8_200), Some(40)), 8_160);
    }

    #[test]
    fn window_clamp_leaves_a_ceiling_within_headroom_alone() {
        assert_eq!(window_clamp(16_384, Some(16_424), Some(40)), 16_384);
    }

    #[test]
    fn window_clamp_needs_both_window_and_input() {
        assert_eq!(window_clamp(16_384, None, Some(40)), 16_384);
        assert_eq!(window_clamp(16_384, Some(8_200), None), 16_384);
    }

    #[test]
    fn window_clamp_saturates_to_zero_past_the_window() {
        assert_eq!(window_clamp(16_384, Some(10), Some(40)), 0);
    }
}
