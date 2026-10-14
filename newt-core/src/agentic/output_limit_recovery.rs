//! Bounded repair of provider-capped tool batches, never partial execution.
use serde_json::{json, Value};

const NOTE: &str = "[loop-guidance] [harness] your last reply was cut off at the output limit; nothing ran. Make the change in smaller steps: use write_file with copy_from for large moves, or several edit_file calls.";

/// Ephemeral turn-local allowance; successful tool rounds renew it. Recovery
/// requests do not consume the separate tool-round allowance.
#[derive(Default)]
pub(super) struct Recovery {
    consecutive: u8,
    pub skipped_rounds: usize,
}

impl Recovery {
    pub fn retry(
        &mut self,
        has_tool: bool,
        messages: &mut Vec<Value>,
        harness: Option<&super::smart_harness::SmartHarness>,
    ) -> anyhow::Result<bool> {
        if !has_tool || self.consecutive >= 2 {
            return Ok(false);
        }
        // Keep one current host note, never an assistant/partial-tool exchange.
        // Host history retains each intervention through its existing envelope.
        messages.retain(|m| !(m["role"] == "user" && m["content"] == NOTE));
        super::smart_harness::push_tool_message(
            messages,
            json!({"role":"user", "content":NOTE}),
            harness,
            true,
            "output_limit_recovery",
        )?;
        self.consecutive += 1;
        self.skipped_rounds += 1;
        Ok(true)
    }

    pub fn completed_tool_round(&mut self) {
        self.consecutive = 0;
    }
}

/// The cap for the retry after a capped request: doubled, clamped to the
/// ceiling, and `None` when that would not raise it (already at the ceiling,
/// or a zero cap that doubling cannot move).
pub(super) fn next_cap(_cap: u32, _ceiling: u32) -> Option<u32> {
    None
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
