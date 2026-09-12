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

#[tokio::test]
async fn overflow_smart_projection_cannot_skip_elision_due_to_per_message_rounding() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let mut messages = fixture("newest result must remain");
    for message in &mut messages {
        let padding = (5 - message.to_string().len() % 4) % 4;
        let content = message["content"].as_str().unwrap().to_string();
        message["content"] = json!(format!("{content}{}", "x".repeat(padding)));
        assert_eq!(message.to_string().len() % 4, 1);
    }
    let est = crate::tokens::TokenEstimation::default();
    let before = trim::estimate_tokens(&messages, est);
    let bytes = serde_json::to_vec(&messages).unwrap().len();
    assert!(
        bytes <= est.chars_for_tokens(before - 1),
        "fixture must expose the token/byte rounding mismatch"
    );

    let navigation_calls = Arc::new(AtomicUsize::new(0));
    let counted_calls = navigation_calls.clone();
    let harness = smart_harness::SmartHarness::new(
        agent_harness::Session::new(Default::default()).unwrap(),
        Arc::new(move |prompt| {
            counted_calls.fetch_add(1, Ordering::SeqCst);
            let catalog: Value = serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
            let selected = catalog["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|candidate| {
                    candidate["required"] == true
                        || candidate["pairs"]
                            .as_array()
                            .is_some_and(|pairs| !pairs.is_empty())
                })
                .map(|candidate| candidate["cid"].clone())
                .collect::<Vec<_>>();
            Box::pin(async move { Ok(serde_json::to_string(&selected).unwrap()) })
        }),
        Default::default(),
    )
    .unwrap();
    let mut input = request(&messages);
    input.budget = target(&messages, before + 1_000, est);
    let result = compress(
        input,
        None,
        &mut super::super::CompressState::new(),
        Some(&harness),
    )
    .await
    .expect("a removable source must be projected despite token/byte rounding differences");
    assert_eq!(navigation_calls.load(Ordering::SeqCst), 1);
    assert!(result.fired);
    assert!(result.tokens_after < before);
    // Smart elision inserts its retained-frame pointer before the first user
    // message. The exact protected inputs retain their relative order.
    assert!(result.messages.starts_with(&messages[..2]));
    assert!(result.messages.contains(&messages[2]));
    assert!(result.messages.ends_with(&messages[4..]));
    assert!(!result.messages.contains(&messages[3]));
}

#[tokio::test]
async fn overflow_smart_responses_projection_cannot_skip_elision_due_to_rounding() {
    let task = "keep the exact operator prompt";
    let mut input = vec![
        json!({"role":"user","content":task}),
        json!({"role":"assistant","content":"obsolete reasoning ".repeat(500)}),
        json!({"role":"assistant","content":"old evidence ".repeat(100)}),
        json!({"role":"assistant","content":"earlier notes ".repeat(100)}),
    ];
    for message in &mut input {
        let padding = (5 - message.to_string().len() % 4) % 4;
        let content = message["content"].as_str().unwrap().to_string();
        message["content"] = json!(format!("{content}{}", "x".repeat(padding)));
    }
    let original = input.clone();
    let est = crate::tokens::TokenEstimation::default();
    let before = trim::estimate_tokens(&input, est);
    assert!(serde_json::to_vec(&input).unwrap().len() <= est.chars_for_tokens(before - 1));
    let harness = smart_harness::SmartHarness::new(
        agent_harness::Session::new(Default::default()).unwrap(),
        std::sync::Arc::new(|prompt| {
            let catalog: Value = serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
            let selected = catalog["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|candidate| candidate["required"] == true)
                .map(|candidate| candidate["cid"].clone())
                .collect::<Vec<_>>();
            Box::pin(async move { Ok(serde_json::to_string(&selected).unwrap()) })
        }),
        Default::default(),
    )
    .unwrap();
    let outcome = super::super::compact_responses_input(
        &mut input,
        Some("policy"),
        None,
        Some(before + 1_000),
        before - 1,
        1.0,
        est,
        task,
        8_192,
        true,
        None,
        None,
        &mut super::super::CompressState::new(),
        false,
        Some(&harness),
        true,
    )
    .await;
    assert!(
        matches!(outcome, super::super::ResponsesCompaction::Compacted),
        "Responses must project a smaller candidate despite token/byte rounding differences"
    );
    assert!(trim::estimate_tokens(&input, est) < before);
    assert!(input.contains(&original[0]));
    assert!(!input.contains(&original[1]));
}
