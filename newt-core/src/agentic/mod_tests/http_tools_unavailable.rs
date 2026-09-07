use super::*;

const PROMISE: &str = "Let me check the current implementation and identify any gaps.";
const BLOCKER: &str =
    "This backend cannot use tools, so I cannot count the repository branches in this turn.";

#[tokio::test]
async fn confined_act_openai_tools_rejection_removes_action_pressure() {
    run_tools_unavailable("openai").await;
}

#[tokio::test]
async fn confined_act_ollama_tools_rejection_removes_action_pressure() {
    run_tools_unavailable("ollama").await;
}

#[tokio::test]
async fn confined_act_responses_tools_rejection_removes_action_pressure() {
    run_tools_unavailable("responses").await;
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn confined_act_anthropic_tools_rejection_removes_action_pressure() {
    let _env = super::super::anthropic_loop_tests::EnvGuard::set("NEWT_ANTHROPIC_STREAM", "off");
    run_tools_unavailable("anthropic").await;
}

/// Real native reads ground a late tools-fallback transition: edit forcing was
/// actually sent before rejection, but cannot survive into tool-less retries.
#[tokio::test]
async fn confined_act_ollama_late_tools_rejection_removes_existing_action_pressure() {
    run_late_tools_rejection(false).await;
}

/// Real operator inbox delivery and native reads distinguish literal user text
/// from generated steering when the provider later loses tool support.
#[tokio::test]
async fn confined_act_ollama_tools_rejection_preserves_literal_prefix_operator_input() {
    run_late_tools_rejection(true).await;
}

async fn run_late_tools_rejection(literal_operator_prefixes: bool) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let workspace = tempfile::tempdir().unwrap();
    let files = [
        ("first.txt", "FIRST_COUNT_EVIDENCE"),
        ("second.txt", "SECOND_COUNT_EVIDENCE"),
        ("third.txt", "THIRD_COUNT_EVIDENCE"),
    ];
    for (name, contents) in files {
        std::fs::write(workspace.path().join(name), contents).unwrap();
    }
    let scope = crate::Scope::only([workspace.path().to_string_lossy().into_owned()]);
    let caveats = Caveats {
        fs_read: scope.clone(),
        fs_write: scope,
        ..tools::plan_phase_clamp()
    };
    let original = caveats.clone();
    let server = MockServer::start().await;
    let tool_requests = Arc::new(AtomicUsize::new(0));
    let candidates = Arc::new(AtomicUsize::new(0));
    let requested = tool_requests.clone();
    let served = candidates.clone();
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |request: &Request| {
            if body_json(request).get("tools").is_some() {
                let round = requested.fetch_add(1, Ordering::SeqCst);
                if let Some((file, _)) = files.get(round) {
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {"tool_calls": [{"function": {
                            "name": "read_file", "arguments": {"path": file}
                        }}]}, "done": true
                    }));
                }
                return ResponseTemplate::new(400)
                    .set_body_string("this model does not support tools");
            }
            let text = if !is_stream(request) && served.fetch_add(1, Ordering::SeqCst) == 0 {
                PROMISE
            } else {
                BLOCKER
            };
            ndjson(&[serde_json::json!({
                "message": {"role": "assistant", "content": text}, "done": true
            })])
        })
        .mount(&server)
        .await;
    let summary = format!(
        "{} Preserved prior count evidence.",
        compress::SUMMARY_PREFIX
    );
    let mut messages = vec![
        MemMessage::user(&summary),
        MemMessage::user(READONLY_COUNT_TASK),
    ];
    let inbox = SessionSteeringInbox::new();
    let mut literal_operator_input = Vec::new();
    if literal_operator_prefixes {
        for prefix in [
            compress::LOOP_GUIDANCE_PREFIX,
            compress::CONTINUATION_PREFIX,
        ] {
            let initial = format!("{prefix} Preserve this literal initial operator message.");
            messages.insert(0, MemMessage::user(&initial));
            literal_operator_input.push(initial);
            let steered = format!("{prefix} Preserve this literal mid-turn operator correction.");
            inbox.submit(&steered);
            literal_operator_input.push(steered);
        }
    }
    let uri = server.uri();
    let workspace_path = workspace.path().to_string_lossy();
    let mut events = Vec::new();
    let mut c = ctx(&uri, &messages, &caveats);
    c.workspace = &workspace_path;
    c.task = READONLY_COUNT_TASK;
    c.tool_events = Some(&mut events);
    c.steering = Some(&inbox);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(reply, BLOCKER);
    assert_eq!(tool_requests.load(Ordering::SeqCst), 4);
    assert_eq!(candidates.load(Ordering::SeqCst), 2);
    assert_eq!(events.len(), 3);
    assert_eq!(
        inbox.pending(),
        0,
        "operator steering must actually be consumed"
    );
    assert!(events
        .iter()
        .all(|event| event.tool == "read_file" && event.ok));
    assert_eq!(caveats, original);
    for (file, contents) in files {
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(file)).unwrap(),
            contents
        );
    }
    let requests = server.received_requests().await.unwrap();
    let rejected = body_json(&requests[3]);
    for literal in &literal_operator_input {
        assert!(
            rejected["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["role"] == "user" && message["content"] == literal.as_str()
                }),
            "literal operator text must be delivered before fallback: {rejected}"
        );
    }
    assert!(rejected["tools"].to_string().contains("write_file"));
    assert!(
        rejected["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "user"
                    && message["content"].as_str().is_some_and(|text| {
                        text.contains("3 read-only rounds so far") && text.contains("edit_file")
                    })
            }),
        "the rejected request must actually carry edit forcing: {rejected}"
    );
    for request in requests.iter().skip(4) {
        let body = body_json(request);
        assert!(body.get("tools").is_none());
        let messages = body["messages"].as_array().unwrap();
        for preserved in [READONLY_COUNT_TASK, summary.as_str()]
            .into_iter()
            .chain(literal_operator_input.iter().map(String::as_str))
        {
            assert!(
                messages
                    .iter()
                    .any(|message| message["role"] == "user" && message["content"] == preserved),
                "lost operator input or summary: {body}"
            );
        }
        for (_, evidence) in files {
            assert!(
                messages.iter().any(|message| message["role"] == "tool"
                    && message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains(evidence))),
                "lost actual tool evidence: {body}"
            );
        }
        for message in messages.iter().filter(|message| message["role"] == "user") {
            let text = message["content"].to_string();
            for forbidden in [
                "edit_file",
                "write_file",
                "request_permissions",
                "tool call",
            ] {
                assert!(
                    !text.contains(forbidden),
                    "stale action pressure after rejection: {text}"
                );
            }
        }
    }
}

/// The real response decoder still returns usable evidence after a local
/// interrupt, but normal completion forensics must not overwrite that interrupt.
#[tokio::test]
async fn confined_act_responses_cancelled_final_text_is_not_completed() {
    let server = MockServer::start().await;
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_after_dispatch = cancel.clone();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |_: &Request| {
            cancel_after_dispatch.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "completed", "output": [{
                    "type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": READONLY_COUNT_ANSWER}]
                }]
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    let messages = vec![MemMessage::user(READONLY_COUNT_TASK)];
    let caveats = tools::plan_phase_clamp();
    let uri = server.uri();
    let mut reason = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.task = READONLY_COUNT_TASK;
    c.cancel = Some(&cancel);
    c.end_reason = Some(&mut reason);
    let (reply, _, _, _) = openai_responses_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(reply, READONLY_COUNT_ANSWER, "preserve returned evidence");
    assert!(cancel.load(Ordering::SeqCst));
    assert_eq!(
        reason, None,
        "an interrupted return must not be stamped Completed"
    );
}

/// The real provider loops must stop treating the original catalog as usable
/// after the endpoint rejects tools. These wiremock cases exercise transport
/// fallback and steering, not a claim about a real model's tool support.
async fn run_tools_unavailable(wire: &'static str) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    assert_eq!(
        crate::NudgeClassifier::builtin().classify(PROMISE).class,
        crate::NudgeClass::DeferredAnswer
    );
    for action_nudges in [false, true] {
        for repeated in [false, true] {
            let server = MockServer::start().await;
            let rejections = Arc::new(AtomicUsize::new(0));
            let candidates = Arc::new(AtomicUsize::new(0));
            let rejected = rejections.clone();
            let served = candidates.clone();
            let last = Arc::new(Mutex::new(String::new()));
            Mock::given(method("POST"))
                .and(path(match wire {
                    "openai" => "/v1/chat/completions",
                    "ollama" => "/api/chat",
                    "responses" => "/v1/responses",
                    "anthropic" => "/v1/messages",
                    _ => unreachable!(),
                }))
                .respond_with(move |request: &Request| {
                    let body = body_json(request);
                    if body.get("tools").is_some() {
                        rejected.fetch_add(1, Ordering::SeqCst);
                        return ResponseTemplate::new(400)
                            .set_body_string("this model does not support tools");
                    }
                    // An accepted final-answer streaming replay is not another
                    // candidate round; mirror the parent's scripted responders.
                    let text = if is_stream(request) {
                        last.lock().unwrap().clone()
                    } else {
                        let round = served.fetch_add(1, Ordering::SeqCst);
                        let text = if round == 0 || repeated {
                            PROMISE
                        } else {
                            BLOCKER
                        };
                        *last.lock().unwrap() = text.to_string();
                        text.to_string()
                    };
                    match wire {
                        "openai" if is_stream(request) => sse_replay(&text),
                        "openai" => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                            "choices": [{"message": {"role": "assistant", "content": text}}]
                        })),
                        "ollama" => ndjson(&[serde_json::json!({
                            "message": {"role": "assistant", "content": text}, "done": true,
                            "prompt_eval_count": 4, "eval_count": 2
                        })]),
                        "responses" => {
                            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                                "status": "completed", "output": [{
                                    "type": "message", "role": "assistant",
                                    "content": [{"type": "output_text", "text": text}]
                                }]
                            }))
                        }
                        "anthropic" => {
                            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                                "model": "claude-test", "stop_reason": "end_turn",
                                "content": [{"type": "text", "text": text}],
                                "usage": {"input_tokens": 4, "output_tokens": 2}
                            }))
                        }
                        _ => unreachable!(),
                    }
                })
                .mount(&server)
                .await;
            let messages = vec![MemMessage::user(READONLY_COUNT_TASK)];
            // Broad grants deliberately leave the stale catalog action-capable;
            // the endpoint's tools rejection is the actual loss of usability.
            let caveats = Caveats::top();
            let original = caveats.clone();
            let uri = server.uri();
            let mut reason = None;
            let mut events = Vec::new();
            let mut c = ctx(&uri, &messages, &caveats);
            c.kind = match wire {
                "ollama" => BackendKind::Ollama,
                "anthropic" => BackendKind::Anthropic,
                _ => BackendKind::Openai,
            };
            if wire == "anthropic" {
                c.api_key = Some("sk-ant-test");
            }
            c.task = READONLY_COUNT_TASK;
            c.prompt_disposition = PromptDisposition::Act;
            c.action_nudges = action_nudges;
            c.end_reason = Some(&mut reason);
            c.tool_events = Some(&mut events);
            let (reply, _, _, _) = match wire {
                "openai" => openai_chat_complete(c, &mut NoMcp).await,
                "ollama" | "anthropic" => chat_complete(c, &mut NoMcp).await,
                "responses" => openai_responses_complete(c, &mut NoMcp).await,
                _ => unreachable!(),
            }
            .unwrap();
            assert_eq!(rejections.load(Ordering::SeqCst), 1, "one schema fallback");
            assert_eq!(
                candidates.load(Ordering::SeqCst),
                2,
                "one bounded quality retry"
            );
            assert!(events.is_empty(), "no tool was actually usable or called");
            assert_eq!(caveats, original, "fallback must not alter grants");
            if repeated {
                assert!(reply.contains(PROMISE), "{reply}");
                assert!(reply.contains("appears unfinished"), "{reply}");
                assert_eq!(reason, Some(crate::TurnEndReason::NarrationCapExhausted));
            } else {
                assert_eq!(reply, BLOCKER);
                assert_eq!(reason, Some(crate::TurnEndReason::Completed));
            }
            let requests = server.received_requests().await.unwrap();
            assert!(
                requests.len() <= 4,
                "fallback, two candidates, at most one replay"
            );
            let catalog = body_json(&requests[0])["tools"].to_string();
            for name in ["run_command", "edit_file", "write_file"] {
                assert!(
                    catalog.contains(name),
                    "fixture must initially expose {name}"
                );
            }
            for request in requests.iter().skip(1) {
                let body = body_json(request);
                assert!(body.get("tools").is_none());
                assert!(body.get("tool_choice").is_none());
                let messages = body
                    .get("messages")
                    .or_else(|| body.get("input"))
                    .and_then(serde_json::Value::as_array)
                    .unwrap();
                for message in messages.iter().filter(|message| message["role"] == "user") {
                    let text = message["content"].to_string();
                    for forbidden in [
                        "run_command",
                        "edit_file",
                        "write_file",
                        "request_permissions",
                        "tool call",
                    ] {
                        assert!(
                            !text.contains(forbidden),
                            "impossible action pressure: {text}"
                        );
                    }
                }
            }
        }
    }
}
