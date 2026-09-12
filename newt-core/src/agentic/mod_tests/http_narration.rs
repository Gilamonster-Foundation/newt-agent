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
            pending_replay: Default::default(),
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
            pending_replay: Default::default(),
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
async fn readonly_completion_retries_an_unfinished_promise_without_action_authority() {
    // The branch-count incident ended after eighteen reads with only "Let me
    // check...". A read-only boundary must not turn that promise into an answer.
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            pending_replay: Default::default(),
            round: round.clone(),
            script: vec![
                serde_json::json!({"tool_calls": [{"id": "read_evidence", "function": {
                    "name": "tool_search", "arguments": "{\"query\":\"read_file\"}"
                }}]}),
                serde_json::json!({ "content": "Let me check the local branches and the top-level remote remotes to be complete." }),
                serde_json::json!({ "content": "Do you mean local branches or open pull requests?" }),
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
    c.task = "Can you tell me how many branches are open in this repo?";
    c.action_nudges = false;
    c.end_reason = Some(&mut end_reason);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(reply, "Do you mean local branches or open pull requests?");
    assert_eq!(
        round.load(Ordering::SeqCst),
        3,
        "one evidence read, promise, clarification"
    );
    let requests = server.received_requests().await.unwrap();
    let recovery = body_json(&requests[2]);
    let guidance = recovery["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(
        guidance.contains("answer") && guidance.contains("read-only"),
        "{guidance}"
    );
    assert!(
        !guidance.contains("edit_file") && !guidance.contains("write_file"),
        "{guidance}"
    );
    for request in requests {
        let body = body_json(&request);
        let names: Vec<_> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        assert!(
            !names.contains(&"write_file") && !names.contains(&"run_command"),
            "{names:?}"
        );
    }
    assert_eq!(
        end_reason,
        Some(crate::TurnEndReason::Completed),
        "a concrete clarification completes the turn"
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
async fn readonly_completion_repeated_promise_hands_back_unresolved_with_bounded_retry() {
    let promise = "Let me check the current implementation and identify any gaps.";
    for max_rounds in [1, 8] {
        let server = MockServer::start().await;
        let round = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ScriptedOpenAi {
                pending_replay: Default::default(),
                round: round.clone(),
                script: vec![serde_json::json!({"content": promise})],
            })
            .mount(&server)
            .await;
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut end_reason = None;
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.prompt_disposition = PromptDisposition::Research;
        c.max_tool_rounds = max_rounds;
        c.end_reason = Some(&mut end_reason);
        let (reply, streamed, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
        assert!(
            reply.contains("unresolved") && reply.contains("continue"),
            "{reply}"
        );
        assert!(
            reply.contains(promise),
            "never discard the candidate: {reply}"
        );
        assert!(reply.contains("appears unfinished"), "{reply}");
        assert!(!streamed, "the caller must print the unresolved handoff");
        assert_ne!(end_reason, Some(crate::TurnEndReason::Completed));
        assert_eq!(round.load(Ordering::SeqCst), max_rounds.min(2));
    }
}

#[tokio::test]
async fn readonly_completion_accepts_answers_questions_and_blockers_without_retry() {
    for answer in [
        "There are three local branches.",
        "Do you mean local branches or open pull requests?",
        "Should I check the current implementation and identify any gaps?",
        "I cannot determine the remote count from the available evidence.",
        "I cannot check the current implementation because access is unavailable.",
        "There are three local branches. Let me check the current implementation and identify any gaps.",
        "Here is a summary of what I found across the tool calls.",
        "I found the issue: there is an extra closing brace causing a syntax error. I need to remove this stray brace.",
        "Current blocker: the for loop needs a one line fix. Next steps needed: fix the iteration type error in lib.rs.",
    ] {
        let server = MockServer::start().await;
        let round = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ScriptedOpenAi {
                pending_replay: Default::default(),
                round: round.clone(),
                script: vec![serde_json::json!({"content": answer})],
            })
            .mount(&server)
            .await;
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.prompt_disposition = PromptDisposition::Explain;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
        assert_eq!(reply, answer);
        assert_eq!(round.load(Ordering::SeqCst), 1);
    }
}

/// Grounds mocked exhausted-response retention in the real finalizer and a
/// temporary workspace, proving retained text still crosses disclosure.
#[test]
fn readonly_completion_handoff_preserves_the_disclosure_boundary() {
    let workspace = tempfile::tempdir().unwrap();
    let canary = "NEWT-CANARY-completion-7f3a9c2b1d";
    let mut filter = crate::ocap::DisclosureFilter::new();
    filter.register(canary);
    let candidate = format!("Let me check the current implementation: {canary}");
    let reply = readonly_completion_handoff(
        &candidate,
        &mut None,
        false,
        workspace.path().to_str().unwrap(),
        &crate::Scope::All,
        None,
        Some(&filter),
    );
    assert!(reply.contains("Let me check the current implementation"));
    assert!(reply.contains("[REDACTED]"), "{reply}");
    assert!(!filter.leaks(&reply), "{reply}");
}

#[test]
fn readonly_completion_shared_guard_preserves_authority_and_bounds_recovery() {
    let classifier = crate::NudgeClassifier::builtin();
    let promise = "Let me check the current implementation and identify any gaps.";
    for disposition in [PromptDisposition::Explain, PromptDisposition::Research] {
        assert!(readonly_completion_pending(
            disposition,
            &classifier,
            promise
        ));
        assert!(!readonly_completion_pending(disposition, &classifier,
            "Recommended next action if session resumes: fix duplicate functions, clean up broken tests, read lib.rs, then wire the progressive dispatch. The build is currently broken and that is the blocker for further progress."));
    }
    for disposition in [
        PromptDisposition::Act,
        PromptDisposition::Ask,
        PromptDisposition::Plan,
    ] {
        assert!(!readonly_completion_pending(
            disposition,
            &classifier,
            promise
        ));
    }
    let mut retried = false;
    let mut messages = vec![serde_json::json!({"role": "user", "content": "Count the branches?"})];
    assert!(!retry_readonly_completion(
        &mut messages,
        promise,
        None,
        &mut retried,
        false
    ));
    assert!(!retried, "no retry was spent without room to continue");
    assert!(retry_readonly_completion(
        &mut messages,
        promise,
        None,
        &mut retried,
        true
    ));
    let after_retry = messages.clone();
    assert!(!retry_readonly_completion(
        &mut messages,
        promise,
        None,
        &mut retried,
        true
    ));
    assert_eq!(
        messages, after_retry,
        "a persistent promise cannot replenish its retry"
    );
}

#[tokio::test]
async fn readonly_completion_ollama_and_responses_recover_with_the_same_readonly_guidance() {
    for responses in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &Request| {
                let body = body_json(request);
                let text = if body.to_string().contains("Your reply promised another step") {
                    "There are three local branches."
                } else {
                    "Let me check the current implementation and identify any gaps."
                };
                let reply = if responses {
                    serde_json::json!({"status": "completed", "output": [{
                        "type": "reasoning", "id": "rs_probe", "summary": [], "encrypted_content": "opaque"
                    }, {
                        "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]
                    }]})
                } else {
                    serde_json::json!({"message": {"role": "assistant", "content": text}, "done": true})
                };
                ResponseTemplate::new(200).set_body_json(reply)
            })
            .mount(&server).await;
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.prompt_disposition = PromptDisposition::Explain;
        c.action_nudges = false;
        let (reply, _, _, _) = if responses {
            openai_responses_complete(c, &mut NoMcp).await
        } else {
            chat_complete(c, &mut NoMcp).await
        }
        .unwrap();
        assert_eq!(reply, "There are three local branches.");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), if responses { 2 } else { 3 });
        if responses {
            let second = body_json(&requests[1]);
            assert!(
                second["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["id"] == "rs_probe" && item["encrypted_content"] == "opaque"),
                "the retry must preserve Responses reasoning items: {second}"
            );
        }
    }
}

#[tokio::test]
async fn genuine_completion_reports_completed_end_reason() {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            pending_replay: Default::default(),
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
