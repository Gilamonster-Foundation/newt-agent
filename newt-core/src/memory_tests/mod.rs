use super::*;
use crate::metrics::TokenUsage;

fn dummy_metrics() -> TurnMetrics {
    TurnMetrics {
        elapsed_ms: 100,
        usage: Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        }),
        cost_usd: Some(0.0),
        model_id: "test".into(),
        endpoint: "http://localhost".into(),
        ..Default::default()
    }
}

// --- TokenBudget tests ---

/// Metrics with a given backend-reported prompt size (`input_tokens`).
fn metrics_with_input(input_tokens: u32) -> TurnMetrics {
    let mut m = dummy_metrics();
    m.usage = Some(TokenUsage {
        input_tokens,
        output_tokens: 20,
    });
    m
}

/// Async stub summarizer in the loop's `SummarizeFn` shape (the only
/// shape since Step 18.5 — the provider delegates to the shared
/// pipeline, which is async).
fn stub_summarizer(
    reply: &'static str,
) -> impl Fn(String) -> crate::agentic::SummarizeFuture + Send + Sync {
    move |_req: String| -> crate::agentic::SummarizeFuture {
        Box::pin(async move { Ok(reply.to_string()) })
    }
}

/// Stub summarizer that records every request it receives — proves the
/// provider routes through the shared pipeline (Step 18.5).
fn capturing_summarizer(
    reply: &'static str,
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> impl Fn(String) -> crate::agentic::SummarizeFuture + Send + Sync {
    move |req: String| -> crate::agentic::SummarizeFuture {
        calls.lock().unwrap().push(req);
        Box::pin(async move { Ok(reply.to_string()) })
    }
}

// --- Budget injection (Step 18.2, #247) ---

// --- NoteStore tests live in crate::notes (split out in Step 19.1) ---

// --- Summarizing tests ---

// --- SoulProvider tests ---

// --- Continuity: restore + delegation (Step 18.5, #247) ---

// --- MemoryManager hook coverage ---

// --- TokenBudget additional coverage ---

// --- Summarizing additional coverage ---

// --- RollingWindow additional coverage ---

// -- MemoryIndex (progressive-disclosure memory, Workstream A MVP, #319) --

// Families beside this file. Both attributes are required: rustc needs only
// the `#[path]`, but the ratchets' shared scanner resolves a child ONLY when
// a `#[cfg(test)]` immediately precedes the `mod` (#2149).
#[cfg(test)]
#[path = "budget.rs"]
mod budget;
#[cfg(test)]
#[path = "compaction_prompt.rs"]
mod compaction_prompt;
#[cfg(test)]
#[path = "compaction_trigger.rs"]
mod compaction_trigger;
#[cfg(test)]
#[path = "continuity.rs"]
mod continuity;
#[cfg(test)]
#[path = "manager.rs"]
mod manager;
#[cfg(test)]
#[path = "memory_index.rs"]
mod memory_index;
#[cfg(test)]
#[path = "note_routing.rs"]
mod note_routing;
#[cfg(test)]
#[path = "rolling_window.rs"]
mod rolling_window;
#[cfg(test)]
#[path = "soul.rs"]
mod soul;
#[cfg(test)]
#[path = "token_accounting.rs"]
mod token_accounting;
