use super::super::http_loop_tests::{
    assert_no_impossible_verification_pressure, readonly_count_workspace, READONLY_COUNT_ANSWER,
    READONLY_COUNT_DATA, READONLY_COUNT_FILE, READONLY_COUNT_TASK,
};
use super::*;

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn confined_act_anthropic_recovers_with_original_usage_and_no_edit_pressure() {
    for stream in [false, true] {
        run_confined_anthropic_completion(stream, false).await;
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn confined_act_command_only_anthropic_json_recovers_with_nudges_off() {
    run_confined_anthropic_completion(false, true).await;
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn confined_act_command_only_anthropic_sse_recovers_with_nudges_off() {
    run_confined_anthropic_completion(true, true).await;
}

async fn run_confined_anthropic_completion(stream: bool, command_authority: bool) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let _env = test_env(stream);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |request: &Request| {
            let body = body_json(request);
            let text = if body
                .to_string()
                .contains("Your reply promised another step")
            {
                "There are two branches."
            } else {
                "Let me check the current implementation and identify any gaps."
            };
            if stream {
                sse_text_reply(&[text], 10, 5)
            } else {
                json_reply(
                    "end_turn",
                    serde_json::json!([{"type": "text", "text": text}]),
                    10,
                    5,
                )
            }
        })
        .mount(&server)
        .await;
    let task = "please count the branches in this repo (newt-agent repo)";
    let messages = vec![MemMessage::user(task)];
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
    c.task = task;
    c.action_nudges = false;
    c.persona_tools = None;
    c.end_reason = Some(&mut end_reason);
    let (reply, _, usage, _) = chat_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(reply, "There are two branches.");
    assert_eq!(end_reason, Some(crate::TurnEndReason::Completed));
    assert_eq!(usage.unwrap().output_tokens, 10, "both generations count");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "one bounded quality retry");
    for request in requests {
        let body = body_json(&request);
        for message in body["messages"].as_array().unwrap() {
            if message["role"] == "user" {
                let content = message["content"].to_string();
                for forbidden in ["edit_file", "write_file", "request_permissions"] {
                    assert!(!content.contains(forbidden), "{content}");
                }
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn confined_act_anthropic_json_stale_blocker_respects_scoped_read_authority() {
    for full_authority in [false, true] {
        run_anthropic_stale_blocker(false, full_authority).await;
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn confined_act_anthropic_sse_stale_blocker_respects_scoped_read_authority() {
    for full_authority in [false, true] {
        run_anthropic_stale_blocker(true, full_authority).await;
    }
}

async fn run_anthropic_stale_blocker(stream: bool, full_authority: bool) {
    use super::super::http_loop_tests::stale_authority::{
        assert_scoped_stale_guidance, stale_workspace, StaleCatalogGit, STALE_BLOCKER,
        STALE_RECOVERED,
    };
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let _env = test_env(stream);
    let (workspace, scoped) = stale_workspace();
    let caveats = if full_authority {
        Caveats::top()
    } else {
        scoped
    };
    let original = caveats.clone();
    let server = MockServer::start().await;
    let round = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let round_seen = round.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_: &Request| {
            let index = round_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = if index == 0 {
                STALE_BLOCKER
            } else {
                STALE_RECOVERED
            };
            if stream {
                sse_text_reply(&[text], 10, 5)
            } else {
                json_reply(
                    "end_turn",
                    serde_json::json!([{"type": "text", "text": text}]),
                    10,
                    5,
                )
            }
        })
        .mount(&server)
        .await;
    let task = "Inspect lib.rs and report what is blocking the task.";
    let messages = vec![MemMessage::user(task)];
    let uri = server.uri();
    let workspace_path = workspace.path().to_string_lossy();
    let mut c = ctx(&uri, &messages, &caveats);
    c.task = task;
    c.workspace = &workspace_path;
    c.persona_tools = None;
    c.git_tool = Some(&StaleCatalogGit);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
    assert!(
        reply == STALE_BLOCKER || reply == STALE_RECOVERED,
        "{reply}"
    );
    let rounds = round.load(std::sync::atomic::Ordering::SeqCst);
    let requests = server.received_requests().await.unwrap();
    if full_authority {
        assert_eq!(rounds, 2, "full authority retains stale-file recovery");
        assert_eq!(reply, STALE_RECOVERED);
    } else {
        assert!((1..=2).contains(&rounds));
        assert_scoped_stale_guidance(&requests);
    }
    assert_eq!(caveats, original);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("lib.rs")).unwrap(),
        "// unchanged\n"
    );
}

/// Real Cargo metadata grounds the verification scanner while scripted model
/// replies pin that neither a manifest nor Act disposition grants execution.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn confined_act_anthropic_verification_requires_actual_command_authority() {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let _self_verify = EnvGuard::set("NEWT_SELF_VERIFY", "1");
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
    )
    .unwrap();
    let hidden = vec!["read_file".to_string()];
    for stream in [false, true] {
        let _env = test_env(stream);
        for (exec, persona, expected_rounds) in [
            (crate::Scope::none(), None, 1),
            (crate::Scope::only(["pwd".to_string()]), None, 1),
            (
                crate::Scope::only(["cargo".to_string()]),
                Some(hidden.as_slice()),
                1,
            ),
            (crate::Scope::only(["cargo".to_string()]), None, 3),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(move |_: &Request| {
                    if stream {
                        sse_text_reply(&["There are two branches."], 10, 5)
                    } else {
                        json_reply("end_turn", serde_json::json!([{"type": "text", "text": "There are two branches."}]), 10, 5)
                    }
                })
                .mount(&server).await;
            let task = "please count the branches in this repo (newt-agent repo)";
            let messages = vec![MemMessage::user(task)];
            let caveats = Caveats {
                exec,
                ..tools::plan_phase_clamp()
            };
            let uri = server.uri();
            let workspace_path = workspace.path().to_string_lossy();
            let mut c = ctx(&uri, &messages, &caveats);
            c.task = task;
            c.workspace = &workspace_path;
            c.persona_tools = persona;
            let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
            assert_eq!(reply, "There are two branches.");
            let requests = server.received_requests().await.unwrap();
            assert_eq!(
                requests.len(),
                expected_rounds,
                "exec={:?}, persona={persona:?}",
                caveats.exec
            );
            for request in requests {
                let body = body_json(&request);
                for message in body["messages"].as_array().unwrap() {
                    if message["role"] == "user" {
                        let content = message["content"].to_string();
                        for forbidden in [
                            "edit_file",
                            "write_file",
                            "request_permissions",
                            "FIX the code",
                        ] {
                            assert!(!content.contains(forbidden), "{content}");
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn confined_act_native_read_finishes_without_impossible_verification_anthropic_json() {
    assert_native_read_finishes_without_impossible_verification(false).await;
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn confined_act_native_read_finishes_without_impossible_verification_anthropic_sse() {
    assert_native_read_finishes_without_impossible_verification(true).await;
}

/// The JSON and SSE loops must accept a grounded native read under the same
/// read-only caveats that performed it. A real manifest keeps the verification
/// scanner armed, so a quiet gate proves authority handling, not absent checks.
async fn assert_native_read_finishes_without_impossible_verification(stream: bool) {
    let _self_verify = EnvGuard::set("NEWT_SELF_VERIFY", "1");
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let _env = test_env(stream);
    let (workspace, caveats) = readonly_count_workspace();
    let original = caveats.clone();
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    let calls = round.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_: &Request| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                if stream {
                    return sse(&[
                        serde_json::json!({"type": "message_start", "message": {
                                "model": "claude-test", "usage": {"input_tokens": 10}}}),
                        serde_json::json!({"type": "content_block_start", "index": 0,
                                "content_block": {"type": "tool_use", "id": "count-read",
                                    "name": "read_file", "input": {}}}),
                        serde_json::json!({"type": "content_block_delta", "index": 0,
                                "delta": {"type": "input_json_delta", "partial_json":
                                    serde_json::json!({"path": READONLY_COUNT_FILE}).to_string()}}),
                        serde_json::json!({"type": "content_block_stop", "index": 0}),
                        serde_json::json!({"type": "message_delta", "delta": {
                                "stop_reason": "tool_use"}, "usage": {"output_tokens": 5}}),
                        serde_json::json!({"type": "message_stop"}),
                    ]);
                }
                return json_reply(
                    "tool_use",
                    serde_json::json!([{
                        "type": "tool_use", "id": "count-read", "name": "read_file",
                        "input": {"path": READONLY_COUNT_FILE}
                    }]),
                    10,
                    5,
                );
            }
            if stream {
                sse_text_reply(&[READONLY_COUNT_ANSWER], 10, 5)
            } else {
                json_reply(
                    "end_turn",
                    serde_json::json!([{
                        "type": "text", "text": READONLY_COUNT_ANSWER
                    }]),
                    10,
                    5,
                )
            }
        })
        .mount(&server)
        .await;
    let uri = server.uri();
    let workspace_path = workspace.path().to_string_lossy();
    let messages = vec![MemMessage::user(READONLY_COUNT_TASK)];
    let mut tool_events = Vec::new();
    let mut end_reason = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.workspace = &workspace_path;
    c.task = READONLY_COUNT_TASK;
    c.persona_tools = None;
    c.action_nudges = true;
    c.tool_events = Some(&mut tool_events);
    c.end_reason = Some(&mut end_reason);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(reply, READONLY_COUNT_ANSWER);
    assert_eq!(tool_events.len(), 1);
    assert!(tool_events[0].ok && tool_events[0].tool == "read_file");
    let requests = server.received_requests().await.unwrap();
    let second = body_json(&requests[1]);
    let read_result = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == "count-read")
        .expect("the successful native read reaches the answering round");
    for line in READONLY_COUNT_DATA.lines() {
        assert!(read_result["content"].as_str().unwrap().contains(line));
    }
    assert_eq!(
        round.load(Ordering::SeqCst),
        2,
        "stream={stream}: one read, then its grounded answer"
    );
    assert_eq!(end_reason, Some(crate::TurnEndReason::Completed));
    assert_eq!(caveats, original);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(READONLY_COUNT_FILE)).unwrap(),
        READONLY_COUNT_DATA
    );
    assert_no_impossible_verification_pressure(&requests);
}
