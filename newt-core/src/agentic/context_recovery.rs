//! Shared evidence and progress checks for bounded context re-projection.

use serde_json::Value;

use super::{observability, smart_harness, trim};

/// Exhaust the legacy pipeline's optional tail before calling a window
/// irreducible. Its ordinary three-message tail is a convenience; a rejected
/// request may retain only the protocol-protected call/result suffix. The
/// same pipeline still owns summary provenance, append-only policy, and the
/// exact active prompt. Smart projection already operates on admitted units.
pub(super) async fn compress(
    request: super::compress::CompressRequest<'_>,
    summarizer: Option<&super::compress::SummarizeFn>,
    state: &mut super::CompressState,
    harness: Option<&smart_harness::SmartHarness>,
) -> anyhow::Result<super::compress::CompressOutcome> {
    let outcome = smart_harness::compress(request, summarizer, state, harness).await?;
    if outcome.tokens_after <= request.budget || !request.rewrites_history || harness.is_some() {
        return Ok(outcome);
    }
    let head =
        trim::protected_prompt_head_len(request.messages, super::prompt_read::ACTIVE_PROMPT_PREFIX);
    let tail = protected_tool_tail(request.messages)
        .max(request.replay_protected_tail_len)
        .max(1);
    let mut bounded = request;
    bounded.max_messages = Some(head.saturating_add(1).saturating_add(tail));
    smart_harness::compress(bounded, summarizer, state, harness).await
}

/// Protect the newest tool result and its call/result suffix. Dropping its
/// assistant call would make continuation invalid on strict providers.
pub(super) fn protected_tool_tail(messages: &[Value]) -> usize {
    let Some(result) = messages.iter().rposition(|m| m["role"] == "tool") else {
        return 0;
    };
    let call = messages[..result]
        .iter()
        .rposition(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .unwrap_or(result);
    messages.len() - call
}

/// Overflow proves the previous projection insufficient even when its
/// heuristic was below the nominal window. Require a smaller projection.
pub(super) fn target(
    messages: &[Value],
    budget: usize,
    est: crate::tokens::TokenEstimation,
) -> usize {
    budget.min(trim::estimate_tokens(messages, est).saturating_sub(1))
}

pub(super) fn record(
    observation: &mut Option<&mut observability::SolveObservation>,
    harness: Option<&smart_harness::SmartHarness>,
    round: usize,
    attempt: u32,
    estimated_tokens: usize,
    projected_tokens: Option<usize>,
) -> anyhow::Result<()> {
    let event = observability::BehaviorSignal::ContextExceeded {
        round,
        attempt,
        estimated_tokens,
        projected_tokens,
    };
    if let Some(observation) = observation.as_deref_mut() {
        observation.behavior_signals.push(event.clone());
    }
    if let Some(harness) = harness {
        harness.context_exceeded(&event)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "context_recovery_tests.rs"]
mod tests;
