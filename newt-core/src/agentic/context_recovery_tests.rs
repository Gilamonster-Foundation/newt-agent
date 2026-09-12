use super::*;
use crate::agentic::compress::{CompressAction, CompressRequest, RefusalReason};
use serde_json::json;

fn fixture(result: &str) -> Vec<Value> {
    vec![
        json!({"role":"system","content":"policy"}),
        json!({"role":"system","content":format!("{}\nmetadata", crate::agentic::prompt_read::ACTIVE_PROMPT_PREFIX)}),
        json!({"role":"user","content":"keep the operator prompt exactly"}),
        json!({"role":"assistant","content":"obsolete reasoning ".repeat(500)}),
        json!({"role":"assistant","content":"", "tool_calls":[{"id":"newest", "type":"function", "function":{"name":"run_command","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"newest","content":result}),
    ]
}

fn request(messages: &[Value]) -> CompressRequest<'_> {
    CompressRequest {
        messages,
        budget: 1_000,
        max_messages: None,
        replay_protected_tail_len: protected_tool_tail(messages),
        task: "keep the operator prompt exactly",
        authoritative: true,
        hard_budget: true,
        focus: None,
        est: Default::default(),
        summary_input_cap_floor_chars: 8_192,
        rewrites_history: true,
        compaction_store: None,
        compaction_stage: None,
    }
}

#[tokio::test]
async fn overflow_reclaims_an_optional_third_tail_message_before_refusing() {
    let messages = fixture("newest tool result must stay whole");
    let mut state = super::super::CompressState::new();
    let result = compress(request(&messages), None, &mut state, None)
        .await
        .unwrap();
    assert!(
        result.tokens_after <= 1_000,
        "a removable older tail message is not irreducible: {} tokens remain",
        result.tokens_after
    );
    assert!(result.messages.contains(&messages[2]));
    assert!(result.messages.ends_with(&messages[4..]));
    assert!(!result.messages.contains(&messages[3]));
}

#[tokio::test]
async fn overflow_can_shrink_without_crossing_the_protected_prompt_floor() {
    let mut messages = fixture("done");
    let est = crate::tokens::TokenEstimation::default();
    messages[0]["content"] = json!("");
    let head_tokens = trim::estimate_tokens(&messages[..3], est);
    messages[0]["content"] = json!("p".repeat(est.chars_for_tokens(800 - head_tokens)));
    messages[3]["content"] = json!("");
    let base_tokens = trim::estimate_tokens(&messages, est);
    messages[3]["content"] = json!("x".repeat(est.chars_for_tokens(1_000 - base_tokens)));
    assert_eq!(trim::estimate_tokens(&messages[..3], est), 800);
    assert_eq!(trim::estimate_tokens(&messages, est), 1_000);

    // The corrected window allows the protected prompt and a smaller
    // continuation. An arbitrary additional one-third cut would refuse it.
    let corrected_budget = 1_066;
    let mut input = request(&messages);
    input.budget = target(&messages, corrected_budget, est);
    let result = compress(input, None, &mut super::super::CompressState::new(), None)
        .await
        .unwrap();
    assert_ne!(
        result.action,
        CompressAction::Refused,
        "a smaller fitting continuation must be attempted before refusing"
    );
    assert!(result.tokens_after < 1_000);
    assert!(result.tokens_after <= corrected_budget);
    assert!(result.messages.starts_with(&messages[..3]));
    assert!(result.messages.ends_with(&messages[4..]));
    assert!(!result.messages.contains(&messages[3]));
}

#[tokio::test]
async fn overflow_never_trims_the_newest_result_to_force_a_fit() {
    let messages = fixture(&"observed external result ".repeat(1_000));
    let mut state = super::super::CompressState::new();
    let result = compress(request(&messages), None, &mut state, None)
        .await
        .unwrap();
    assert!(result.tokens_after > 1_000);
    assert!(result.messages.contains(&messages[2]));
    assert!(result.messages.ends_with(&messages[4..]));
}

#[tokio::test]
async fn overflow_respects_append_only_and_endpoint_replay_protection() {
    let messages = fixture("latest observed result");
    let mut state = super::super::CompressState::new();
    let mut input = request(&messages);
    input.rewrites_history = false;
    let append_only = compress(input, None, &mut state, None).await.unwrap();
    assert_eq!(append_only.action, CompressAction::Refused);
    assert_eq!(append_only.refusal, Some(RefusalReason::AppendOnly));
    assert_eq!(append_only.messages, messages);

    let mut input = request(&messages);
    input.replay_protected_tail_len = 3;
    let replay = compress(input, None, &mut state, None).await.unwrap();
    assert!(replay.tokens_after > 1_000);
    assert!(replay.messages.ends_with(&messages[3..]));
}
