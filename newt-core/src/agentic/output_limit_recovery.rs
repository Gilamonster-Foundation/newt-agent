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
