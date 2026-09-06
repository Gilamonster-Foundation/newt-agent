use super::*;

use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub(super) const EST: TokenEstimation = TokenEstimation { chars_per_token: 4 };

// -- builders ------------------------------------------------------------

pub(super) fn sys(text: &str) -> Value {
    json!({"role": "system", "content": text})
}

pub(super) fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

pub(super) fn active_prompt_card() -> Value {
    sys(&format!(
        "{ACTIVE_PROMPT_PREFIX}\naddress: prompt:test\nmodel_digest: test"
    ))
}

pub(super) fn assistant_call(name: &str, args: Value) -> Value {
    json!({"role": "assistant", "content": "",
               "tool_calls": [{"function": {"name": name, "arguments": args}}]})
}

pub(super) fn tool_result(content: &str) -> Value {
    json!({"role": "tool", "content": content})
}

/// `[system, active-prompt metadata, exact task user, tool rounds…]` —
/// the shape the agentic loop hands to compression.
pub(super) fn tool_heavy(task: &str, rounds: usize, result_chars: usize) -> Vec<Value> {
    let mut msgs = vec![sys("you are newt"), active_prompt_card(), user(task)];
    for i in 0..rounds {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("src/file_{i}.rs")}),
        ));
        msgs.push(tool_result(&format!("{i}:{}", "x".repeat(result_chars))));
    }
    msgs
}

/// A summarizer that records every prompt it receives and returns a
/// canned summary.
pub(super) fn recording_summarizer(
    prompts: Arc<Mutex<Vec<String>>>,
    reply: &'static str,
) -> Summarizer {
    Box::new(move |prompt: String| {
        let prompts = prompts.clone();
        Box::pin(async move {
            prompts.lock().unwrap().push(prompt);
            Ok(reply.to_string())
        })
    })
}

pub(super) fn failing_summarizer(calls: Arc<AtomicUsize>) -> Summarizer {
    Box::new(move |_prompt: String| {
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("summarizer endpoint 500")
        })
    })
}

/// Hard-budget invocation (token threshold / send-budget semantics).
/// Authoritative: the disabled-and-over case refuses (B6).
pub(super) async fn run(
    messages: &[Value],
    budget: usize,
    max_messages: Option<usize>,
    summarizer: Option<&SummarizeFn>,
    state: &mut CompressState,
) -> CompressOutcome {
    compress(
        CompressRequest {
            rewrites_history: true,
            messages,
            budget,
            max_messages,
            replay_protected_tail_len: 0,
            task: "fix the failing test",
            hard_budget: true,
            authoritative: true,
            focus: None,
            est: EST,
            summary_input_cap_floor_chars: 8_192,
            compaction_store: None,
            compaction_stage: None,
        },
        summarizer,
        state,
    )
    .await
}

/// Count-only (VRAM guard) invocation: soft aim-to-halve budget that
/// neither consults nor feeds anti-thrash (F2).
pub(super) async fn run_count_only(
    messages: &[Value],
    budget: usize,
    max_messages: Option<usize>,
    summarizer: Option<&SummarizeFn>,
    state: &mut CompressState,
) -> CompressOutcome {
    compress(
        CompressRequest {
            rewrites_history: true,
            messages,
            budget,
            max_messages,
            replay_protected_tail_len: 0,
            task: "fix the failing test",
            hard_budget: false,
            authoritative: false,
            focus: None,
            est: EST,
            summary_input_cap_floor_chars: 8_192,
            compaction_store: None,
            compaction_stage: None,
        },
        summarizer,
        state,
    )
    .await
}
