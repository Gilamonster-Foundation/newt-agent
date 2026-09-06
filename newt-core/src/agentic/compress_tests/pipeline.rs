use super::*;

use super::test_support::{
    assistant_call, failing_summarizer, recording_summarizer, run, run_count_only, sys, tool_heavy,
    tool_result, user, EST,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// -- pipeline order -------------------------------------------------------

/// Under budget → untouched, no anti-thrash accounting.
#[tokio::test]
async fn within_budget_is_a_noop() {
    let msgs = tool_heavy("task", 2, 100);
    let mut state = CompressState::new();
    let out = run(&msgs, 100_000, None, None, &mut state).await;
    assert_eq!(out.action, CompressAction::Fit);
    assert!(!out.fired);
    assert_eq!(out.messages, msgs);
    assert_eq!(state.attempts, 0, "a no-op never counts as a compression");
}

/// Prune-first short-circuit: when the structural passes reclaim enough,
/// the summarizer is never invoked (zero LLM cost).
#[tokio::test]
async fn prune_short_circuits_when_sufficient() {
    // 14 messages: 2 aged huge identical results (dedupe + one-liner
    // fodder) + 10 protected-tail fillers.
    let big = "y".repeat(8_000);
    let mut msgs = vec![
        sys("you are newt"),
        user("task"),
        assistant_call("run_command", json!({"command": "cargo test"})),
        tool_result(&big),
        assistant_call("run_command", json!({"command": "cargo test"})),
        tool_result(&big),
    ];
    for i in 0..10 {
        msgs.push(user(&format!("filler {i}")));
    }
    let before = estimate_tokens(&msgs, EST);
    let budget = before - 1_000; // prune reclaims ~4k tokens — plenty
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");
    let mut state = CompressState::new();
    let out = run(&msgs, budget, None, Some(&*s), &mut state).await;
    assert_eq!(out.action, CompressAction::Pruned);
    assert!(out.fired);
    assert!(out.tokens_after <= budget);
    assert_eq!(out.messages.len(), msgs.len(), "prune never drops messages");
    assert!(
        prompts.lock().unwrap().is_empty(),
        "summarizer must not be called when pruning suffices"
    );
}

/// Prune insufficient → the middle is summarized; head + tail survive
/// verbatim, markers wrap the summary, the old placeholder is gone.
#[tokio::test]
async fn summarizes_middle_with_markers_when_prune_insufficient() {
    let msgs = tool_heavy("ACTIVE TASK GAUNTLET-7f3d9c: do the thing", 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "## Active Task\nGAUNTLET summary");
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;

    assert_eq!(out.action, CompressAction::Summarized);
    assert!(out.fired);
    assert!(out.tokens_after < before);
    // Head anchored verbatim.
    assert_eq!(out.messages[0], msgs[0]);
    assert_eq!(out.messages[1], msgs[1]);
    assert_eq!(out.messages[2], msgs[2]);
    // The summary message carries both markers and the summary body.
    let summary = out.messages[3]["content"].as_str().unwrap();
    assert!(summary.starts_with(SUMMARY_PREFIX), "{summary}");
    assert!(summary.contains("GAUNTLET summary"), "{summary}");
    assert!(summary.contains(SUMMARY_END_MARKER), "{summary}");
    // The old amputation placeholder must be gone from this path.
    assert!(
        !out.messages.iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|c| c.contains("earlier tool-call messages omitted"))),
        "the old placeholder-discard line must not appear"
    );
}

/// No summarizer → static fallback marker with the exact removed count.
#[tokio::test]
async fn no_summarizer_uses_static_fallback_marker() {
    let msgs = tool_heavy("task", 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, None, &mut state).await;
    assert_eq!(out.action, CompressAction::StaticFallback);
    let summary = out.messages[3]["content"].as_str().unwrap();
    assert!(summary.starts_with(SUMMARY_PREFIX), "{summary}");
    assert!(summary.contains(SUMMARY_END_MARKER), "{summary}");
    // middle = messages [2, tail_start): compute the expected count from
    // the output shape (protected pair head + marker + tail).
    let removed = msgs.len() - (out.messages.len() - 1);
    assert!(
        summary.contains(&format!(
            "Summary generation was unavailable. {removed} message(s) were removed."
        )),
        "{summary}"
    );
}

/// Summarizer failure → static marker; the pipeline never errors out.
#[tokio::test]
async fn summarizer_failure_falls_back_to_static_marker() {
    let msgs = tool_heavy("task", 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let calls = Arc::new(AtomicUsize::new(0));
    let s = failing_summarizer(calls.clone());
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "summarizer was attempted");
    assert_eq!(out.action, CompressAction::StaticFallback);
    let summary = out.messages[3]["content"].as_str().unwrap();
    assert!(summary.contains("Summary generation was unavailable."));
}

/// An empty/whitespace summary counts as a failure (static marker).
#[tokio::test]
async fn empty_summary_falls_back_to_static_marker() {
    let msgs = tool_heavy("task", 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "  \n ");
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;
    assert_eq!(out.action, CompressAction::StaticFallback);
}

/// The count trigger (`max_messages`) forces the summary stage even when
/// tokens already fit — pruning can never reduce the message count.
#[tokio::test]
async fn max_messages_forces_summary_stage() {
    let msgs = tool_heavy("task", 8, 50); // small payloads: tokens fit
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");
    let mut state = CompressState::new();
    let out = run_count_only(&msgs, before + 1_000, Some(8), Some(&*s), &mut state).await;
    assert_eq!(out.action, CompressAction::Summarized);
    assert!(out.messages.len() < msgs.len());
}

/// F1 (the headline regression): a SECOND compression of an already-
/// compressed conversation must still shrink it. The bug anchored the
/// boundary on the first pass's own summary message, the middle went
/// empty, the count never dropped, and the fit pass destroyed every
/// fresh tool result pre-dispatch from then on.
#[tokio::test]
async fn second_compression_still_shrinks_and_keeps_fresh_results() {
    let fresh = format!("9:{}", "x".repeat(4_000));
    let msgs = tool_heavy("fix the failing test", 10, 4_000);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY ONE");
    let mut state = CompressState::new();
    let budget = estimate_tokens(&msgs, EST) / 2;
    let first = run_count_only(&msgs, budget, Some(8), Some(&*s), &mut state).await;
    assert!(first.messages.len() < msgs.len(), "first pass shrinks");
    assert!(first.messages.iter().any(is_compaction_message));

    // Six more rounds land on top of the compressed list.
    let mut grown = first.messages.clone();
    for i in 10..16 {
        grown.push(assistant_call(
            "read_file",
            json!({"path": format!("src/file_{i}.rs")}),
        ));
        grown.push(tool_result(&format!("{i}:{}", "x".repeat(4_000))));
    }
    let grown_fresh = grown.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    let budget2 = estimate_tokens(&grown, EST) / 2;
    let second = run_count_only(&grown, budget2, Some(8), Some(&*s), &mut state).await;
    assert!(
        second.messages.len() < grown.len(),
        "second compression must still shrink ({} -> {})",
        grown.len(),
        second.messages.len()
    );
    assert!(
        second.messages.len() <= 10,
        "count goal must stay reachable, got {}",
        second.messages.len()
    );
    // The freshest tool result reaches the model intact, both passes.
    assert_eq!(
        first.messages.last().unwrap()["content"].as_str(),
        Some(fresh.as_str()),
        "first pass fresh result intact"
    );
    assert_eq!(
        second.messages.last().unwrap()["content"].as_str(),
        Some(grown_fresh.as_str()),
        "second pass fresh result intact"
    );
    // Count-only passes never feed anti-thrash (F2).
    assert!(!state.disabled);
    assert_eq!(state.attempts, 0);
}
