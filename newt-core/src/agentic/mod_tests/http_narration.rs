use super::*;

/// Like [`run_openai_script`] but with a configured narrate-then-stop
/// rescue budget (`[tui] narration_nudge_cap`, lever L3).
async fn run_openai_script_with_cap(
    script: Vec<serde_json::Value>,
    narration_nudge_cap: usize,
) -> (String, usize) {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script,
            last_content: Default::default(),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.narration_nudge_cap = narration_nudge_cap;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    (reply, round.load(Ordering::SeqCst))
}

#[test]
fn strip_trailing_nudge_keeps_only_the_current_correction() {
    // #1158: successive nudges must REPLACE, not pile up — otherwise the
    // model's own accumulated dithering drives the "genuinely finished"
    // defense loop. After stripping, the tail nudge pair is gone; a
    // non-nudge tail is untouched.
    use serde_json::json;
    let guidance = format!("{} act now", compress::LOOP_GUIDANCE_PREFIX);
    let mut msgs = vec![
        json!({"role": "user", "content": "do the thing"}),
        json!({"role": "assistant", "content": "Let me start."}),
        json!({"role": "user", "content": guidance.clone()}),
    ];
    strip_trailing_nudge_exchange(&mut msgs);
    assert_eq!(msgs.len(), 1, "the narration + nudge pair is removed");
    assert_eq!(msgs[0]["content"], "do the thing");

    let mut clean = vec![
        json!({"role": "user", "content": "do the thing"}),
        json!({"role": "assistant", "content": "here is the answer"}),
    ];
    let before = clean.clone();
    strip_trailing_nudge_exchange(&mut clean);
    assert_eq!(clean, before, "a real answer tail is never stripped");

    let mut loop_msgs = vec![json!({"role": "user", "content": "fix it"})];
    for i in 0..3 {
        strip_trailing_nudge_exchange(&mut loop_msgs);
        loop_msgs.push(json!({"role": "assistant", "content": format!("narration {i}")}));
        loop_msgs.push(json!({"role": "user", "content": guidance.clone()}));
    }
    assert_eq!(
        loop_msgs.len(),
        3,
        "user task + exactly one (narration, nudge) pair — not three"
    );
}

#[tokio::test]
async fn narrated_intent_with_no_tool_call_nudges_and_continues() {
    // The model narrates intent to act but calls no tool. Instead of ending
    // the turn (the bug), the loop nudges and runs another round, returning
    // the post-nudge answer.
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({ "content": "Let me edit the file now." }),
        serde_json::json!({ "content": "All done — the edit is complete." }),
    ])
    .await;
    assert_eq!(rounds, 2, "must run a second round after the nudge");
    assert!(
        reply.contains("complete"),
        "returns the post-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("Let me edit"),
        "must not return the narration: {reply}"
    );
}

#[tokio::test]
async fn narration_auto_continue_is_bounded_by_the_cap() {
    // The model narrates intent EVERY round. The cap (1) allows exactly one
    // nudge, then the narration is accepted as the final answer — no loop.
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({ "content": "Let me keep editing now." }),
        serde_json::json!({ "content": "Let me keep editing now." }),
        serde_json::json!({ "content": "Let me keep editing now." }),
    ])
    .await;
    assert_eq!(
        rounds, 2,
        "exactly one nudge (cap=1), then accept, got {rounds}"
    );
    assert!(
        reply.contains("editing"),
        "narration accepted as final: {reply}"
    );
}

#[tokio::test]
async fn narration_nudge_cap_two_allows_a_second_escalated_rescue() {
    // Lever L3: with `narration_nudge_cap = 2` a chronic narrator gets TWO
    // rescues (the second escalated), and the post-rescue answer is
    // returned; a genuine recovery on round 3 proves the extra budget is
    // what converts the stall.
    let (reply, rounds) = run_openai_script_with_cap(
        vec![
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "All done — the edit is complete." }),
        ],
        2,
    )
    .await;
    assert_eq!(
        rounds, 3,
        "two nudges (cap=2) before the recovery, got {rounds}"
    );
    assert!(
        reply.contains("complete"),
        "returns the post-nudge answer: {reply}"
    );
}

#[tokio::test]
async fn narration_nudge_cap_two_still_accepts_after_exhaustion() {
    // The raised cap is still a cap: a model that narrates through both
    // rescues has its third narration accepted as the final answer.
    let (reply, rounds) = run_openai_script_with_cap(
        vec![
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "Let me keep editing now." }),
        ],
        2,
    )
    .await;
    assert_eq!(rounds, 3, "two nudges, then accept, got {rounds}");
    assert!(
        reply.contains("editing"),
        "narration accepted as final: {reply}"
    );
}

#[test]
fn escalated_narration_nudge_names_attempt_cap_and_active_step() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};

    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![
            Step {
                description: "inspect".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "fix conflict markers".to_string(),
                status: StepStatus::Active,
            },
        ],
    });
    let text = escalated_narration_action_nudge(2, 3, Some(&ledger as &dyn StepLedger));
    assert!(text.contains("Reminder 2/3"), "{text}");
    assert!(text.contains("fix conflict markers"), "{text}");
    assert!(text.contains("tool call"), "{text}");

    // No ledger: no step clause, the demand still stands.
    let bare = escalated_narration_action_nudge(2, 2, None);
    assert!(bare.contains("Reminder 2/2"), "{bare}");
    assert!(!bare.contains("Active step"), "{bare}");
}

#[tokio::test]
async fn accepted_narration_reports_cap_exhausted_end_reason() {
    // The acceptance-forensics record: a narration that exhausts the
    // rescue budget ends the turn with a visible reason instead of
    // masquerading as a normal completion.
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            last_content: Default::default(),
            round: round.clone(),
            script: vec![
                serde_json::json!({ "content": "Let me keep editing now." }),
                serde_json::json!({ "content": "Let me keep editing now." }),
            ],
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.end_reason = Some(&mut end_reason);
    let _ = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(
        end_reason,
        Some(crate::TurnEndReason::NarrationCapExhausted)
    );
}

#[tokio::test]
async fn explain_turn_narration_reports_completed_not_cap_exhausted() {
    // #1261 regression (the diagnosed ornith:35b footer): in a NON-Act turn the
    // narration rescue can never arm (`action_nudges` is forced false, so
    // `action_turn` is false) — the budget is untouched and no cap value could
    // change anything. Ending on pending-action-looking prose is therefore this
    // turn's LEGITIMATE completion. Before the fix the end reason was computed
    // WITHOUT the gate's `action_turn` guard, reporting NarrationCapExhausted —
    // rendered as "⚠ ended on narration (rescue budget spent)", blaming the
    // model for a harness decision.
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            last_content: Default::default(),
            round: round.clone(),
            script: vec![
                // The same pending-action phrasing the Act-turn test uses — the
                // classifier sees "pending action" either way; only the turn
                // type differs.
                serde_json::json!({ "content": "Let me keep editing now." }),
            ],
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.prompt_disposition = PromptDisposition::Explain;
    c.end_reason = Some(&mut end_reason);
    let _ = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(
        end_reason,
        Some(crate::TurnEndReason::Completed),
        "a non-Act turn ending on prose is a completion, never 'rescue budget spent'"
    );
    // The footer stays clean too — the ⚠ never renders for Completed.
    let metrics = crate::TurnMetrics {
        end_reason,
        ..Default::default()
    };
    assert!(
        !metrics.display_line().contains('⚠'),
        "no warning may render: {}",
        metrics.display_line()
    );
}

#[tokio::test]
async fn genuine_completion_reports_completed_end_reason() {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            last_content: Default::default(),
            round: round.clone(),
            script: vec![serde_json::json!({ "content": "The capital of France is Paris." })],
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.end_reason = Some(&mut end_reason);
    let _ = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(end_reason, Some(crate::TurnEndReason::Completed));
}

#[tokio::test]
async fn narration_nudge_reaches_the_wire_tagged_as_loop_guidance() {
    // The rescue nudge must arrive tagged so the compaction pipeline can
    // keep it (and the model's echo of it) out of later summaries.
    let server = MockServer::start().await;
    let saw_tag = Arc::new(AtomicBool::new(false));

    struct TagProbe {
        saw_tag: Arc<AtomicBool>,
    }
    impl Respond for TagProbe {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let tagged = body["messages"].as_array().is_some_and(|ms| {
                ms.iter().any(|m| {
                    m["role"] == "user"
                        && m["content"]
                            .as_str()
                            .is_some_and(|c| c.starts_with(compress::LOOP_GUIDANCE_PREFIX))
                })
            });
            if tagged {
                self.saw_tag.store(true, Ordering::SeqCst);
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "All done — edit complete." } }]
                }));
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "Let me edit the file now." } }]
            }))
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(TagProbe {
            saw_tag: saw_tag.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert!(
        saw_tag.load(Ordering::SeqCst),
        "the narration nudge must carry LOOP_GUIDANCE_PREFIX on the wire"
    );
    assert!(reply.contains("complete"), "{reply}");
}

#[tokio::test]
async fn ollama_loop_honors_cap_two_and_escalates_the_second_nudge() {
    // Ollama-path parity for lever L3 (the macro chain is separate code
    // from the OpenAI inline chain): with narration_nudge_cap = 2 the
    // first rescue carries the [loop-guidance]-tagged generic corrective
    // and the SECOND carries the escalated "Reminder 2/2" variant — both
    // observed on the wire — before the model recovers.
    let server = MockServer::start().await;
    let saw_first = Arc::new(AtomicBool::new(false));
    let saw_escalated = Arc::new(AtomicBool::new(false));

    struct EscalationProbe {
        saw_first: Arc<AtomicBool>,
        saw_escalated: Arc<AtomicBool>,
    }
    impl Respond for EscalationProbe {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let has = |needle: &str| {
                body["messages"].as_array().is_some_and(|ms| {
                    ms.iter()
                        .any(|m| m["content"].as_str().is_some_and(|c| c.contains(needle)))
                })
            };
            if has("Reminder 2/2") {
                self.saw_escalated.store(true, Ordering::SeqCst);
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "All done — the edit is complete." }
                }));
            }
            if has(compress::LOOP_GUIDANCE_PREFIX) {
                self.saw_first.store(true, Ordering::SeqCst);
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "Let me keep editing now." }
            }))
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(EscalationProbe {
            saw_first: saw_first.clone(),
            saw_escalated: saw_escalated.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Ollama;
    c.narration_nudge_cap = 2;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert!(
        saw_first.load(Ordering::SeqCst),
        "the first nudge must reach the Ollama wire tagged [loop-guidance]"
    );
    assert!(
        saw_escalated.load(Ordering::SeqCst),
        "the second nudge must be the escalated Reminder 2/2 variant"
    );
    assert!(reply.contains("complete"), "{reply}");
}

#[tokio::test]
async fn genuine_final_answer_is_not_nudged() {
    // No prior tool call and no intent-to-act cue → a real answer returns
    // immediately, un-nudged (no wasted round).
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({ "content": "The capital of France is Paris." }),
    ])
    .await;
    assert_eq!(
        rounds, 1,
        "a plain final answer is not nudged, got {rounds}"
    );
    assert!(reply.contains("Paris"), "returns the answer: {reply}");
}

#[tokio::test]
async fn final_answer_after_a_tool_call_is_not_nudged() {
    // The normal "act, then conclude" turn: a tool call, then a cue-less
    // final answer. The rescue must NOT fire (no intent cue) — else every
    // ordinary tool-using turn would waste a round.
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({
            "content": null,
            "tool_calls": [{
                "id": "c1", "type": "function",
                "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
            }]
        }),
        serde_json::json!({ "content": "The files were examined; everything checks out." }),
    ])
    .await;
    assert_eq!(
        rounds, 2,
        "tool call (r0) then final answer (r1) — no extra round, got {rounds}"
    );
    assert!(
        reply.contains("checks out"),
        "returns the final answer as-is: {reply}"
    );
}

#[tokio::test]
async fn observed_fix_intent_after_a_tool_call_nudges_and_continues() {
    // Live repro: after a read-only observation, the model identified the
    // exact edit but stopped on prose instead of calling the edit tool.
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({
            "content": null,
            "tool_calls": [{
                "id": "c1", "type": "function",
                "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
            }]
        }),
        serde_json::json!({
            "content": "I found the issue - there's an extra closing brace } on line 809 of help_sections.rs that's causing a syntax error. I need to remove this stray brace."
        }),
        serde_json::json!({ "content": "The stray brace is removed and the compile error is fixed." }),
    ])
    .await;
    assert_eq!(
        rounds, 3,
        "tool call, narrated edit intent, then post-nudge answer; got {rounds}"
    );
    assert!(
        reply.contains("compile error is fixed"),
        "returns the post-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("I need to remove"),
        "must not stop on the narrated edit intent: {reply}"
    );
}

#[test]
fn looks_like_intent_to_act_separates_narration_from_final_answers() {
    // Real repro narrations that ended a turn — must read as intent-to-act.
    assert!(looks_like_intent_to_act(
        "Now I have everything I need. Let me make both edits now."
    ));
    assert!(looks_like_intent_to_act(
        "Now I'll add the --home flag to the Cli struct."
    ));
    assert!(looks_like_intent_to_act("Let me keep editing now."));
    assert!(looks_like_intent_to_act(
        "I'm going to edit the config file."
    ));
    assert!(looks_like_intent_to_act(
        "Let me understand what was already done on this branch and compare it with the issue requirements."
    ));
    assert!(looks_like_intent_to_act(
        "Let me check the current implementation and identify any gaps."
    ));
    assert!(looks_like_intent_to_act(
        "The help section logic itself has no tests yet.\n\nLet me commit this first step, then move on:"
    ));
    assert!(looks_like_intent_to_act(
        "Plan is current — no update needed. Continuing with step 2: inserting the progressive dispatch into lib.rs."
    ));
    assert!(looks_like_intent_to_act(
        "I found the issue - there's an extra closing brace } on line 809 of help_sections.rs that's causing a syntax error. I need to remove this stray brace."
    ));
    // Genuine sign-offs / answers — must NOT be nudged.
    assert!(!looks_like_intent_to_act("The capital of France is Paris."));
    assert!(!looks_like_intent_to_act(
        "I have finished editing the file and the tests pass."
    ));
    assert!(!looks_like_intent_to_act(
        "Here is a summary of what I found across the tool calls."
    ));
    // Borrowed-cue sign-off ("let me know" + a verb) — must NOT be nudged.
    assert!(!looks_like_intent_to_act(
        "Done. Let me know if you want any further changes."
    ));
    // A long narration whose 400-byte tail cut lands mid-multibyte-glyph
    // (each `…` is 3 bytes; 200 of them puts the cut at byte 211, not a char
    // boundary) must not panic the slice — and still classify as intent.
    let multibyte = format!("{}let me edit", "…".repeat(200));
    assert!(looks_like_intent_to_act(&multibyte));
}
