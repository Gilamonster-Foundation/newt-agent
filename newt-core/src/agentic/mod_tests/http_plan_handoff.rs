use super::*;

#[tokio::test]
async fn pending_plan_final_answer_nudges_before_handoff() {
    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![
            Step {
                description: "convert help sections".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "fix format_command_list and update lib.rs".to_string(),
                status: StepStatus::Active,
            },
            Step {
                description: "add tests".to_string(),
                status: StepStatus::Todo,
            },
        ],
    });
    let (reply, rounds) = run_openai_script_with_ledger(
        vec![
            serde_json::json!({
                "content": "I need to finish Step 2, then Steps 3-5."
            }),
            serde_json::json!({
                "content": "Plan updated; continuing with the active step."
            }),
            serde_json::json!({
                "content": "The active step is now complete."
            }),
        ],
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert_eq!(
        rounds, 3,
        "open plan should force a completion-gate round and action-nudge follow-on narration"
    );
    assert!(
        reply.contains("complete"),
        "returns the post-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("I need to finish"),
        "must not accept a plain handoff while plan is open: {reply}"
    );
}

#[tokio::test]
async fn findings_summary_with_stale_plan_nudges_update_plan_then_continues() {
    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![
            Step {
                description: "convert help sections".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "wire progressive dispatch in lib.rs".to_string(),
                status: StepStatus::Active,
            },
            Step {
                description: "add tests".to_string(),
                status: StepStatus::Todo,
            },
        ],
    });
    let findings = "\
Summary of Findings

Across the tool calls, I observed two issues in newt-tui/src/help_sections.rs:
1. Duplicate function definitions
2. Stray closing brace

Current Status

The build is broken due to these syntax errors. The plan was at step 2, but we need to fix the immediate compilation issues first before proceeding with feature work.

Next Steps Required

To continue, I would need to remove the duplicate function using edit_file, locate and remove the stray brace, verify cargo check, then proceed with step 2 of the plan.

However, I've reached the tool-round limit and cannot make these edits now.";
    let (reply, rounds) = run_openai_script_with_ledger(
        vec![
            serde_json::json!({ "content": findings }),
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "plan_1",
                    "type": "function",
                    "function": {
                        "name": "update_plan",
                        "arguments": serde_json::json!({
                            "plan": [
                                {"step": "fix duplicate help rollup functions and stray brace", "status": "in_progress"},
                                {"step": "wire progressive dispatch in lib.rs", "status": "pending"},
                                {"step": "add rollup tests", "status": "pending"}
                            ]
                        }).to_string()
                    }
                }]
            }),
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "edit_1",
                    "type": "function",
                    "function": {
                        "name": "definitely_not_a_real_tool",
                        "arguments": "{}"
                    }
                }]
            }),
            serde_json::json!({ "content": "Done." }),
        ],
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert_eq!(
        rounds, 4,
        "findings summary should be nudged into update_plan, then a concrete tool"
    );
    assert_eq!(reply, "Done.");
    assert!(
        !reply.contains("tool-round limit"),
        "must not accept the handoff summary: {reply}"
    );
    let snap = ledger.snapshot();
    assert_eq!(
        snap.steps[0].description,
        "fix duplicate help rollup functions and stray brace"
    );
    assert_eq!(snap.steps[0].status, StepStatus::Active);
}

#[tokio::test]
async fn completed_plan_final_answer_is_accepted() {
    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![Step {
            description: "done".to_string(),
            status: StepStatus::Done,
        }],
    });
    let (reply, rounds) = run_openai_script_with_ledger(
        vec![serde_json::json!({
            "content": "All plan steps are complete."
        })],
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert_eq!(rounds, 1, "completed plan must not be nudged");
    assert!(reply.contains("complete"), "returns final answer: {reply}");
}

#[tokio::test]
async fn continuing_with_active_step_after_plan_nudge_gets_action_nudge() {
    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![
            Step {
                description: "convert help sections".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "insert progressive dispatch".to_string(),
                status: StepStatus::Active,
            },
            Step {
                description: "add tests".to_string(),
                status: StepStatus::Todo,
            },
        ],
    });
    let (reply, rounds) = run_openai_script_with_ledger(
        vec![
            serde_json::json!({
                "content": "I need to finish Step 2, then Steps 3-5."
            }),
            serde_json::json!({
                "content": "Plan is current — no update needed. Continuing with step 2: inserting the progressive dispatch into lib.rs."
            }),
            serde_json::json!({
                "content": "The edit is now complete."
            }),
        ],
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert_eq!(
        rounds, 3,
        "plan nudge should be followed by an action nudge for continuing-with narration"
    );
    assert!(
        reply.contains("complete"),
        "returns the post-action-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("Continuing with step 2"),
        "must not stop on the continuing-with narration: {reply}"
    );
}
