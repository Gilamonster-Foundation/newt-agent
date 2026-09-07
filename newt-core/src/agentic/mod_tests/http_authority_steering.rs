use super::*;

const BRANCH_TASK: &str = "please count the branches in this repo (newt-agent repo)";
const PROMISE: &str = "Let me check the current implementation and identify any gaps.";

fn readonly_caveats(workspace: &std::path::Path) -> Caveats {
    Caveats {
        fs_read: crate::Scope::only([workspace.to_string_lossy().into_owned()]),
        ..tools::plan_phase_clamp()
    }
}

async fn run_confined_act_script(
    script: Vec<serde_json::Value>,
    action_nudges: bool,
) -> (String, usize, Vec<Request>, Option<crate::TurnEndReason>) {
    run_act_script(script, action_nudges, false, false, None).await
}

async fn run_act_script(
    script: Vec<serde_json::Value>,
    action_nudges: bool,
    command_authority: bool,
    write_authority: bool,
    persona_tools: Option<&[String]>,
) -> (String, usize, Vec<Request>, Option<crate::TurnEndReason>) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let expected_reads = script
        .iter()
        .filter(|message| message["tool_calls"].is_array())
        .count();
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
    // Real files ground the steering fixture's assumption that its read calls
    // succeed under the same Caveats used by dispatch.
    let workspace = tempfile::tempdir().unwrap();
    for name in ["local", "remote", "aliases"] {
        std::fs::write(workspace.path().join(name), name).unwrap();
    }
    let mut caveats = readonly_caveats(workspace.path());
    if command_authority {
        caveats.exec = crate::Scope::only(["pwd".to_string()]);
    }
    if write_authority {
        caveats.fs_write = caveats.fs_read.clone();
    }
    let original = caveats.clone();
    let intake = PromptIntake::analyze(BRANCH_TASK);
    assert_eq!(intake.disposition(), PromptDisposition::Act);
    let messages = vec![MemMessage::user(BRANCH_TASK)];
    let uri = server.uri();
    let workspace_path = workspace.path().to_string_lossy();
    let mut end_reason = None;
    let mut tool_events = Vec::new();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = BRANCH_TASK;
    c.workspace = &workspace_path;
    c.prompt_intake = Some(&intake);
    c.prompt_disposition = intake.disposition();
    c.action_nudges = action_nudges;
    c.persona_tools = persona_tools;
    c.end_reason = Some(&mut end_reason);
    c.tool_events = Some(&mut tool_events);
    let (reply, _, _, _) = openai_chat_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(tool_events.len(), expected_reads);
    assert!(tool_events
        .iter()
        .all(|event| event.tool == "read_file" && event.ok));
    assert_eq!(caveats, original, "steering must not change authority");
    for name in ["local", "remote", "aliases"] {
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(name)).unwrap(),
            name
        );
    }
    (
        reply,
        round.load(Ordering::SeqCst),
        server.received_requests().await.unwrap(),
        end_reason,
    )
}

fn assert_no_edit_pressure(requests: &[Request]) {
    for request in requests {
        let body = body_json(request);
        let messages = body
            .get("messages")
            .or_else(|| body.get("input"))
            .and_then(serde_json::Value::as_array)
            .expect("Chat/Ollama messages or Responses input");
        for message in messages {
            if message["role"] == "user" {
                let content = message["content"].to_string();
                for forbidden in ["edit_file", "write_file", "request_permissions"] {
                    assert!(
                        !content.contains(forbidden),
                        "authority-incompatible guidance: {content}"
                    );
                }
            }
        }
    }
}

/// Real workspace reads ground the mocked model's successful read-only rounds;
/// the same loop must recover an unfinished answer without demanding a write.
#[tokio::test]
async fn confined_act_reads_do_not_create_edit_or_grant_pressure() {
    let mut script: Vec<_> = ["local", "remote", "aliases"]
        .into_iter()
        .map(|name| {
            serde_json::json!({"tool_calls": [{
                "id": name,
                "function": {"name": "read_file", "arguments": format!("{{\"path\":\"{name}\"}}")}
            }]})
        })
        .collect();
    script.push(serde_json::json!({"content": PROMISE}));
    script.push(serde_json::json!({"content": "There are two branches."}));
    let (reply, rounds, requests, reason) = run_confined_act_script(script, true).await;
    assert_eq!(reply, "There are two branches.");
    assert_eq!(rounds, 5, "three reads, one unfinished reply, one answer");
    assert_eq!(reason, Some(crate::TurnEndReason::Completed));
    assert_no_edit_pressure(&requests);
}

/// Real reads ground the loop's read-only round counter. A write grant alone
/// must not create pressure to call edit tools excluded by the selected role.
#[tokio::test]
async fn confined_act_persona_hidden_edit_tools_do_not_create_edit_pressure() {
    let hidden_edits = vec!["read_file".to_string()];
    let mut script: Vec<_> = ["local", "remote", "aliases"]
        .into_iter()
        .map(|name| {
            serde_json::json!({"tool_calls": [{
                "id": name,
                "function": {"name": "read_file", "arguments": format!("{{\"path\":\"{name}\"}}")}
            }]})
        })
        .collect();
    script.push(serde_json::json!({"content": "There are two branches."}));
    let (reply, rounds, requests, reason) =
        run_act_script(script, true, false, true, Some(&hidden_edits)).await;
    assert_eq!(reply, "There are two branches.");
    assert_eq!(rounds, 4, "three successful reads, then the answer");
    assert_eq!(reason, Some(crate::TurnEndReason::Completed));
    for request in &requests {
        let body = body_json(request);
        let definitions = body["tools"].as_array().expect("advertised tools");
        assert!(definitions
            .iter()
            .any(|tool| tool["function"]["name"] == "read_file"));
        assert!(definitions.iter().all(|tool| {
            !matches!(
                tool["function"]["name"].as_str(),
                Some("edit_file" | "write_file")
            )
        }));
    }
    assert_no_edit_pressure(&requests);
}

#[tokio::test]
async fn confined_act_recovers_completion_with_action_nudges_off() {
    let (reply, rounds, requests, reason) = run_confined_act_script(
        vec![
            serde_json::json!({"content": PROMISE}),
            serde_json::json!({"content": "There are two branches."}),
        ],
        false,
    )
    .await;
    assert_eq!(reply, "There are two branches.");
    assert_eq!(rounds, 2);
    assert_eq!(reason, Some(crate::TurnEndReason::Completed));
    assert_no_edit_pressure(&requests);
}

#[tokio::test]
async fn confined_act_accepts_answers_clarifications_and_authority_blockers() {
    for answer in [
        "There are two branches. Let me know if you need a breakdown.",
        "Do you mean local branches or open pull requests?",
        "run_command is permission denied; I cannot run the build.",
    ] {
        let (reply, rounds, requests, reason) =
            run_confined_act_script(vec![serde_json::json!({"content": answer})], true).await;
        assert_eq!(reply, answer);
        assert_eq!(
            rounds, 1,
            "a complete answer or actual authority blocker is terminal"
        );
        assert_eq!(reason, Some(crate::TurnEndReason::Completed));
        assert_no_edit_pressure(&requests);
    }
}

#[tokio::test]
async fn confined_act_repeated_promise_has_one_recovery_then_handoff() {
    let (reply, rounds, requests, reason) =
        run_confined_act_script(vec![serde_json::json!({"content": PROMISE})], false).await;
    assert_eq!(
        rounds, 2,
        "quality recovery stays bounded independently of action nudges"
    );
    assert!(
        reply.contains(PROMISE),
        "preserve the last model candidate: {reply}"
    );
    assert!(
        reply.contains("appears unfinished"),
        "qualify the handoff: {reply}"
    );
    assert_eq!(reason, Some(crate::TurnEndReason::NarrationCapExhausted));
    assert_no_edit_pressure(&requests);
}

#[tokio::test]
async fn confined_act_command_authority_keeps_generic_recovery_without_edit_pressure() {
    let (reply, rounds, requests, reason) = run_act_script(
        vec![
            serde_json::json!({"content": PROMISE}),
            serde_json::json!({"content": "There are two branches."}),
        ],
        true,
        true,
        false,
        None,
    )
    .await;
    assert_eq!(reply, "There are two branches.");
    assert_eq!(
        rounds, 2,
        "authorized command work must retain narration recovery"
    );
    assert_eq!(reason, Some(crate::TurnEndReason::Completed));
    assert_no_edit_pressure(&requests);
}

#[tokio::test]
async fn confined_act_ollama_and_responses_recover_without_edit_pressure() {
    for responses in [false, true] {
        run_confined_completion_wire(responses, false, false).await;
    }
}

#[tokio::test]
async fn confined_act_command_only_openai_recovers_with_nudges_off() {
    let (reply, rounds, requests, reason) = run_act_script(
        vec![
            serde_json::json!({"content": PROMISE}),
            serde_json::json!({"content": "There are two branches."}),
        ],
        false,
        true,
        false,
        None,
    )
    .await;
    assert_eq!(reply, "There are two branches.");
    assert_eq!(
        rounds, 2,
        "command authority does not disable factual completion recovery"
    );
    assert_eq!(reason, Some(crate::TurnEndReason::Completed));
    assert_no_edit_pressure(&requests);
}

#[tokio::test]
async fn confined_act_command_only_ollama_recovers_with_nudges_off() {
    run_confined_completion_wire(false, true, false).await;
}

#[tokio::test]
async fn confined_act_command_only_repeated_promise_has_one_quality_retry() {
    let (reply, rounds, requests, reason) = run_act_script(
        vec![serde_json::json!({"content": PROMISE})],
        false,
        true,
        false,
        None,
    )
    .await;
    assert_eq!(rounds, 2);
    assert!(
        reply.contains(PROMISE) && reply.contains("appears unfinished"),
        "{reply}"
    );
    assert_eq!(reason, Some(crate::TurnEndReason::NarrationCapExhausted));
    assert_no_edit_pressure(&requests);
}

#[tokio::test]
async fn confined_act_writable_turn_keeps_existing_nudges_off_behavior() {
    let (reply, rounds, requests, reason) = run_act_script(
        vec![serde_json::json!({"content": PROMISE})],
        false,
        true,
        true,
        None,
    )
    .await;
    assert_eq!(reply, PROMISE);
    assert_eq!(
        rounds, 1,
        "do not broaden factual recovery into writable Act turns"
    );
    assert_eq!(reason, Some(crate::TurnEndReason::Completed));
    assert_no_edit_pressure(&requests);
}

#[tokio::test]
async fn confined_act_command_only_responses_recovers_independently_of_nudges() {
    for action_nudges in [false, true] {
        run_confined_completion_wire(true, true, action_nudges).await;
    }
}

async fn run_confined_completion_wire(
    responses: bool,
    command_authority: bool,
    action_nudges: bool,
) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
            .respond_with(move |request: &Request| {
                let body = body_json(request);
                let text = if body.to_string().contains("Your reply promised another step") {
                    "There are two branches."
                } else {
                    PROMISE
                };
                let reply = if responses {
                    serde_json::json!({"status": "completed", "output": [{
                        "type": "reasoning", "id": "rs_confined", "summary": [], "encrypted_content": "opaque"
                    }, {
                        "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]
                    }]})
                } else {
                    serde_json::json!({"message": {"role": "assistant", "content": text}, "done": true})
                };
                ResponseTemplate::new(200).set_body_json(reply)
            })
            .mount(&server).await;
    let messages = vec![MemMessage::user(BRANCH_TASK)];
    let caveats = Caveats {
        exec: if command_authority {
            crate::Scope::only(["pwd".to_string()])
        } else {
            crate::Scope::none()
        },
        ..tools::plan_phase_clamp()
    };
    let uri = server.uri();
    let mut end_reason = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.task = BRANCH_TASK;
    c.action_nudges = action_nudges;
    c.end_reason = Some(&mut end_reason);
    let (reply, _, _, _) = if responses {
        openai_responses_complete(c, &mut NoMcp).await
    } else {
        chat_complete(c, &mut NoMcp).await
    }
    .unwrap();
    assert_eq!(reply, "There are two branches.");
    assert_eq!(end_reason, Some(crate::TurnEndReason::Completed));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), if responses { 2 } else { 3 });
    if responses {
        let second = body_json(&requests[1]);
        assert!(second["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "rs_confined" && item["encrypted_content"] == "opaque"));
    }
    assert_no_edit_pressure(&requests);
}

/// Real refused editing and successful reads ground Ollama's round counter:
/// a failed edit must not reset exploration progress or hide the forcing gate.
#[tokio::test]
async fn confined_act_ollama_refused_edit_does_not_reset_readonly_rounds() {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let workspace = tempfile::tempdir().unwrap();
    for (file, contents) in [
        ("target.txt", "before"),
        ("first.txt", "first"),
        ("second.txt", "second"),
    ] {
        std::fs::write(workspace.path().join(file), contents).unwrap();
    }
    let caveats = Caveats {
        fs_write: crate::Scope::only([workspace.path().to_string_lossy().into_owned()]),
        ..readonly_caveats(workspace.path())
    };
    let persona = vec!["read_file".to_string(), "write_file".to_string()];
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    let round_seen = round.clone();
    let script = [
        serde_json::json!({"tool_calls": [{"function": {"name": "edit_file", "arguments": {
            "path": "target.txt", "old_string": "before", "new_string": "after"
        }}}]}),
        serde_json::json!({"tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "first.txt"}}}]}),
        serde_json::json!({"tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "second.txt"}}}]}),
        serde_json::json!({"content": "Do you mean local branches or open pull requests?"}),
    ];
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |request: &Request| {
            let index = if body_json(request)["stream"] == true {
                script.len() - 1
            } else {
                round_seen
                    .fetch_add(1, Ordering::SeqCst)
                    .min(script.len() - 1)
            };
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": script[index], "done": true}))
        })
        .mount(&server)
        .await;
    let task = "Update target.txt from before to after.";
    let messages = vec![MemMessage::user(task)];
    let uri = server.uri();
    let workspace_path = workspace.path().to_string_lossy();
    let mut events = Vec::new();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Ollama;
    c.workspace = &workspace_path;
    c.task = task;
    c.persona_tools = Some(&persona);
    c.tool_events = Some(&mut events);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(reply, "Do you mean local branches or open pull requests?");
    assert_eq!(round.load(Ordering::SeqCst), 4);
    assert_eq!(events.len(), 3);
    assert!(events[0].tool == "edit_file" && !events[0].ok);
    assert!(events[1..]
        .iter()
        .all(|event| event.tool == "read_file" && event.ok));
    for (file, contents) in [
        ("target.txt", "before"),
        ("first.txt", "first"),
        ("second.txt", "second"),
    ] {
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(file)).unwrap(),
            contents
        );
    }
    let requests = server.received_requests().await.unwrap();
    let fourth = body_json(&requests[3]);
    assert!(fourth["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "write_file"));
    assert!(
        fourth["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("3 read-only rounds so far"))
            }),
        "the refused edit is not a completed write: {fourth}"
    );
}
