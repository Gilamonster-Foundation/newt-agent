use super::*;

/// Restore must route through the shared pipeline exactly once when a
/// later turn overflows, with the shared template and redaction applied
/// — the proof the legacy duplicate path is gone.
#[tokio::test]
async fn summarizing_delegates_to_shared_pipeline() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut s =
        Summarizing::new(512).with_summarizer(capturing_summarizer("PIPE-SUM", calls.clone()));
    let secret = "sk-aaaaaaaaaaaaaaaaaaaaaaaa";
    let big = "x".repeat(200);
    // An early reply carries a credential — it lands in the summarized
    // middle and must be redacted by the SHARED pipeline's pass.
    s.sync_turn(
        "the original task",
        &format!("noted {secret}"),
        &metrics_with_input(10),
    )
    .await;
    for i in 0..4u32 {
        s.sync_turn(&big, &big, &metrics_with_input(11 + i)).await;
    }
    assert!(calls.lock().unwrap().is_empty(), "under budget — no calls");
    let current = "CURRENT-B: compress this conversation";
    s.sync_turn(current, &big, &metrics_with_input(600)).await;

    let reqs = calls.lock().unwrap();
    assert_eq!(reqs.len(), 1, "exactly one summarizer call per compression");
    let req = &reqs[0];
    assert!(
        req.contains("## Conversation middle to summarise"),
        "must be the shared pipeline's request template"
    );
    assert!(req.contains("## Original Task"));
    let task_section = req
        .split("## Original Task")
        .nth(1)
        .expect("shared prompt task section")
        .split("## Conversation middle")
        .next()
        .unwrap_or_default();
    assert!(
        task_section.contains(current),
        "task anchored verbatim: {req}"
    );
    assert!(!task_section.contains("the original task"), "{req}");
    assert!(
        !req.contains(secret),
        "secret must not reach the summarizer"
    );
    assert!(req.contains("[REDACTED]"), "shared redaction pass applied");
    drop(reqs);
    // Assembly used the shared markers around the returned body.
    assert!(s.prev_summary.starts_with(crate::agentic::SUMMARY_PREFIX));
    assert!(s.prev_summary.contains("PIPE-SUM"));
    assert!(s.prev_summary.contains(crate::agentic::SUMMARY_END_MARKER));
}
#[tokio::test]
async fn summarizing_anchors_latest_b_and_does_not_persist_harness_pair() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut provider =
        Summarizing::new(512).with_summarizer(capturing_summarizer("B-SUMMARY", calls.clone()));
    let bulk = "x".repeat(500);
    let mut turns = vec![crate::ConversationTurn::new(
        "TASK-A: inspect ambient services",
        "A completed",
    )];
    for i in 0..8 {
        turns.push(crate::ConversationTurn::new(
            format!("history {i} {bulk}"),
            format!("reply {i} {bulk}"),
        ));
    }
    provider.restore_turns(&turns);

    let task_b = "TASK-B: implement durable prompt provenance";
    provider
        .sync_turn(task_b, "working on B", &metrics_with_input(600))
        .await;

    let requests = calls.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let task_section = requests[0]
        .split("## Original Task")
        .nth(1)
        .expect("shared prompt task section")
        .split("## Conversation middle")
        .next()
        .unwrap_or_default();
    assert!(task_section.contains(task_b), "{}", requests[0]);
    assert!(!task_section.contains("TASK-A"), "{}", requests[0]);
    drop(requests);
    assert!(provider
        .history
        .iter()
        .all(|turn| !turn.user.starts_with("[NEWT ACTIVE PROMPT v1]")));
}
#[tokio::test]
async fn summarizing_persists_retry_but_compresses_against_active_operator() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut provider = Summarizing::new(512)
        .with_summarizer(capturing_summarizer("ACTIVE-SUMMARY", calls.clone()));
    let active_operator = "ORIGINAL OPERATOR B: implement durable prompt provenance";
    let bulk = "x".repeat(300);
    let mut turns = vec![crate::ConversationTurn::new(
        active_operator,
        "original B reply",
    )];
    turns.extend((0..7).map(|i| {
        crate::ConversationTurn::new(format!("history {i} {bulk}"), format!("reply {i} {bulk}"))
    }));
    provider.restore_turns(&turns);

    let submitted_retry = "HARNESS RETRY: act now and do not summarize";
    provider
        .sync_turn_with_active_task(
            submitted_retry,
            "continuing the requested implementation",
            &metrics_with_input(600),
            active_operator,
        )
        .await;

    let requests = calls.lock().unwrap();
    assert_eq!(requests.len(), 1, "one provider compression expected");
    let task_section = requests[0]
        .split("## Original Task")
        .nth(1)
        .expect("shared prompt task section")
        .split("## Conversation middle")
        .next()
        .unwrap_or_default();
    assert!(task_section.contains(active_operator), "{}", requests[0]);
    assert!(!task_section.contains(submitted_retry), "{}", requests[0]);
    drop(requests);

    let retry_turn = provider
        .history
        .iter()
        .find(|turn| turn.user == submitted_retry)
        .expect("submitted retry presentation must remain ordinary history");
    assert!(
        retry_turn.assistant == "continuing the requested implementation",
        "the retry reply must stay paired with the submitted retry; got assistant {:?}",
        retry_turn.assistant
    );
    assert!(
        provider.history.iter().all(|turn| {
            turn.user != active_operator
                || turn.assistant != "continuing the requested implementation"
        }),
        "the transient active operator anchor must never be reinserted and paired with the retry reply"
    );
    assert!(provider
        .history
        .iter()
        .all(|turn| !turn.user.starts_with("[NEWT ACTIVE PROMPT v1]")));
}
/// After restore, the rehydrated compaction message chains into the NEXT
/// compression as part of the summarized middle — it is neither the task
/// anchor nor a fresh user message (the F1 self-poisoning shape).
#[tokio::test]
async fn restored_compaction_chains_into_next_compression() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut s =
        Summarizing::new(512).with_summarizer(capturing_summarizer("NEW-SUM", calls.clone()));
    let marked = format!(
        "{}\nCHAIN-ME: facts from the first compaction\n{}",
        crate::agentic::SUMMARY_PREFIX,
        crate::agentic::SUMMARY_END_MARKER
    );
    let big = "y".repeat(100);
    let mut turns = vec![crate::ConversationTurn::new(marked.clone(), "")];
    for i in 0..4 {
        turns.push(crate::ConversationTurn::new(
            format!("task {i} {big}"),
            format!("reply {i} {big}"),
        ));
    }
    s.restore_turns(&turns);

    // One live over-budget turn → one pipeline compression.
    let current = "CURRENT-B: extend the restored compaction chain";
    s.sync_turn(current, &big, &metrics_with_input(600)).await;
    let reqs = calls.lock().unwrap();
    assert_eq!(reqs.len(), 1, "one call through the shared path");
    assert!(
        reqs[0].contains("CHAIN-ME"),
        "the previous summary must be summarizer INPUT (the chain), got: {}",
        reqs[0]
    );
    // The Original-Task anchor (the text right under the header) is the
    // explicit current prompt, not either the compaction message or an
    // older historical user request.
    let anchored = reqs[0]
        .split("## Original Task")
        .nth(1)
        .and_then(|rest| rest.lines().nth(1).map(str::to_string))
        .unwrap_or_default();
    assert!(
        anchored.starts_with(current),
        "task anchor must be current B, got: {anchored:?}"
    );
    drop(reqs);
    // The old compaction message was replaced by the new one.
    let compactions: Vec<&SumTurn> = s
        .history
        .iter()
        .filter(|t| t.user.starts_with(crate::agentic::SUMMARY_PREFIX))
        .collect();
    assert_eq!(compactions.len(), 1, "exactly one compaction in history");
    assert!(compactions[0].user.contains("NEW-SUM"));
    assert!(s.prev_summary.contains("NEW-SUM"));
}
