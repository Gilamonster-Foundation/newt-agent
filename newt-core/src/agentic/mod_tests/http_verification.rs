use super::*;

/// Regression for the live full-access incident: the model narrated an exec
/// denial without ever calling the advertised tool. The harness must spend one
/// bounded round demanding a real probe instead of presenting that invention
/// as the final answer.
#[tokio::test]
async fn openai_unverified_run_command_blocker_gets_ground_truth_retry() {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            last_content: Default::default(),
            round: round.clone(),
            script: vec![
                serde_json::json!({
                    "content": "I hit a capability wall: run_command is permission-denied; exec not granted."
                }),
                serde_json::json!({
                    "content": "I will inspect an actual run_command result before reporting a denial."
                }),
            ],
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = "you should have a \"gh\" command ... test \"gh auth status\" now to tell me if you can use it?";
    c.max_tool_rounds = 2;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");

    assert_eq!(round.load(Ordering::SeqCst), 2, "one corrective retry");
    assert!(
        reply.contains("actual run_command result"),
        "final reply: {reply}"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    let second = body_json(&requests[1]).to_string();
    assert!(
        second.contains("no returned run_command result this turn contains an exec denial"),
        "corrective request: {second}"
    );
    assert!(
        second.contains("Report a denial only when an actual returned result contains one"),
        "corrective request: {second}"
    );
}

/// [`run_openai_script`] against a workspace the caller chooses, so a test can
/// point the loop at a directory that actually affords a verification.
async fn run_openai_script_in(script: Vec<serde_json::Value>, workspace: &str) -> (String, usize) {
    let (reply, rounds, _) = run_openai_script_in_with_authority(
        script,
        workspace,
        "do the thing",
        &Caveats::top(),
        None,
        None,
    )
    .await;
    (reply, rounds)
}

pub(in crate::agentic) async fn run_openai_script_in_with_authority(
    script: Vec<serde_json::Value>,
    workspace: &str,
    task: &str,
    caveats: &Caveats,
    persona_tools: Option<&[String]>,
    exec_floor: Option<&crate::Scope<String>>,
) -> (String, usize, Vec<Request>) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
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
    let messages = vec![MemMessage::system("you are a test"), MemMessage::user(task)];
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, caveats);
    c.kind = BackendKind::Openai;
    c.workspace = workspace;
    c.task = task;
    c.persona_tools = persona_tools;
    c.exec_floor = exec_floor;
    let (reply, _s, _u, _h) = openai_chat_complete(c, &mut NoMcp).await.expect("dispatch");
    (
        reply,
        round.load(Ordering::SeqCst),
        server.received_requests().await.unwrap(),
    )
}

/// A real Cargo manifest grounds the scanner's mocked verification affordance.
/// Its presence must not become exec or write authority after an evidence task.
#[tokio::test]
#[serial_test::serial(newt_self_verify_env)]
async fn confined_act_self_verification_requires_authorized_and_exposed_command() {
    let _self_verify = super::super::anthropic_loop_tests::EnvGuard::set("NEWT_SELF_VERIFY", "1");
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
    )
    .unwrap();
    let hidden = vec!["read_file".to_string()];
    for (exec, persona, expected_rounds) in [
        (crate::Scope::none(), None, 1),
        (crate::Scope::only(["pwd".to_string()]), None, 1),
        (
            crate::Scope::only(["cargo".to_string()]),
            Some(hidden.as_slice()),
            1,
        ),
        // A matching exec grant still permits bounded verification recovery,
        // but no write grant means a failure must not trigger edit pressure.
        (crate::Scope::only(["cargo".to_string()]), None, 3),
    ] {
        let caveats = Caveats {
            exec,
            ..tools::plan_phase_clamp()
        };
        let original = caveats.clone();
        let (reply, rounds, requests) = run_openai_script_in_with_authority(
            vec![serde_json::json!({"content": "There are two branches."})],
            &workspace.path().to_string_lossy(),
            "please count the branches in this repo (newt-agent repo)",
            &caveats,
            persona,
            None,
        )
        .await;
        assert_eq!(reply, "There are two branches.");
        assert_eq!(
            rounds, expected_rounds,
            "exec={:?}, persona={persona:?}",
            caveats.exec
        );
        assert_eq!(
            caveats, original,
            "verification steering cannot grant authority"
        );
        for request in requests {
            let body = body_json(&request);
            for message in body["messages"].as_array().unwrap() {
                if message["role"] == "user" {
                    let content = message["content"].as_str().unwrap_or_default();
                    for forbidden in [
                        "edit_file",
                        "write_file",
                        "request_permissions",
                        "FIX the code",
                    ] {
                        assert!(!content.contains(forbidden), "{content}");
                    }
                    if expected_rounds == 1 {
                        assert!(
                            !content.contains("verification this task ships"),
                            "{content}"
                        );
                    }
                }
            }
        }
    }
}

/// Grounds the no-exec steering regression after an actual native read, not
/// merely a scripted answer: a manifest cannot turn that evidence into an
/// obligation to run tests or request broader authority.
#[tokio::test]
#[serial_test::serial(newt_self_verify_env)]
async fn confined_act_native_read_finishes_without_impossible_verification_openai() {
    let _self_verify = super::super::anthropic_loop_tests::EnvGuard::set("NEWT_SELF_VERIFY", "1");
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let (workspace, caveats) = readonly_count_workspace();
    let original = caveats.clone();
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            last_content: Default::default(),
            script: vec![
                serde_json::json!({"tool_calls": [{
                    "id": "count-read", "type": "function",
                    "function": {"name": "read_file", "arguments":
                        serde_json::json!({"path": READONLY_COUNT_FILE}).to_string()}
                }]}),
                serde_json::json!({"content": READONLY_COUNT_ANSWER}),
            ],
        })
        .mount(&server)
        .await;
    let uri = server.uri();
    let workspace_path = workspace.path().to_string_lossy();
    let messages = vec![MemMessage::user(READONLY_COUNT_TASK)];
    let mut tool_events = Vec::new();
    let mut end_reason = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.workspace = &workspace_path;
    c.task = READONLY_COUNT_TASK;
    c.action_nudges = true;
    c.tool_events = Some(&mut tool_events);
    c.end_reason = Some(&mut end_reason);
    let (reply, _, _, _) = openai_chat_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(reply, READONLY_COUNT_ANSWER);
    assert_eq!(tool_events.len(), 1);
    assert!(tool_events[0].ok && tool_events[0].tool == "read_file");
    let requests = server.received_requests().await.unwrap();
    let second = body_json(&requests[1]);
    let read_result = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "count-read")
        .expect("the successful native read reaches the answering round");
    for line in READONLY_COUNT_DATA.lines() {
        assert!(read_result["content"].as_str().unwrap().contains(line));
    }
    assert_eq!(
        round.load(Ordering::SeqCst),
        2,
        "one read, then its grounded answer"
    );
    assert_eq!(end_reason, Some(crate::TurnEndReason::Completed));
    assert_eq!(caveats, original);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(READONLY_COUNT_FILE)).unwrap(),
        READONLY_COUNT_DATA
    );
    assert_no_impossible_verification_pressure(&requests);
}

/// A real manifest grounds the scanner's affordance detection: metadata
/// outside fs_read must not influence the mocked model's follow-up request.
/// The allowed-root twin proves that absence of guidance is not a dark gate.
#[tokio::test]
#[serial_test::serial(newt_self_verify_env)]
async fn confined_act_verification_cannot_use_an_unauthorized_manifest() {
    let _self_verify = super::super::anthropic_loop_tests::EnvGuard::set("NEWT_SELF_VERIFY", "1");
    let workspace = tempfile::tempdir().unwrap();
    let readable = workspace.path().join("readable");
    std::fs::create_dir(&readable).unwrap();
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = 'private_fixture'\nversion = '0.1.0'\n",
    )
    .unwrap();
    for (fs_read, expected_rounds) in [
        (crate::Scope::none(), 1),
        (
            crate::Scope::only([readable.to_string_lossy().into_owned()]),
            1,
        ),
        (
            crate::Scope::only([workspace.path().to_string_lossy().into_owned()]),
            3,
        ),
    ] {
        let caveats = Caveats {
            fs_read,
            exec: crate::Scope::only(["cargo".to_string()]),
            ..tools::plan_phase_clamp()
        };
        let original = caveats.clone();
        let (reply, rounds, requests) = run_openai_script_in_with_authority(
            vec![serde_json::json!({"content": "There are two branches."})],
            &workspace.path().to_string_lossy(),
            "please count the branches in this repo (newt-agent repo)",
            &caveats,
            None,
            None,
        )
        .await;
        assert_eq!(reply, "There are two branches.");
        assert_eq!(rounds, expected_rounds, "fs_read={:?}", caveats.fs_read);
        assert_eq!(caveats, original);
        let verification_guidance = requests.iter().any(|request| {
            let body = body_json(request);
            body["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("verification this task ships"))
            })
        });
        assert_eq!(verification_guidance, expected_rounds > 1);
    }
}

/// **The wiring #1943 arms, proved end to end through the loop.**
///
/// Every other test in this file points at [`NO_CHECKS_WORKSPACE`] so the gate
/// stays out of their way — which would leave the armed gate exactly as
/// unexercised as the env var left it, and that is the failure this whole PR
/// is about. So one test points the loop at a workspace that DOES ship a
/// verification and holds it to firing.
///
/// `.` under `cargo test -p newt-core` is this crate's directory, which ships
/// a `Cargo.toml`. That is a deliberate real-filesystem dependency in exactly
/// one test, and it is what **grounds** `self_verify`'s mocked scanner tests:
/// those encode a belief about what `read_dir` yields, and this is the test
/// that would fail if the belief were wrong.
#[tokio::test]
async fn an_armed_self_verify_gate_adds_a_round_when_the_workspace_ships_a_check() {
    // The model answers without ever running a command, three times running.
    // Armed, the gate hands it another round each time — and then STOPS at
    // `SELF_VERIFY_CAP` (2), so a model that will not verify still ends its
    // turn. Pinning the exact count pins the cap with it: a gate that could
    // nudge forever would hang the turn it was meant to improve.
    let script = vec![
        serde_json::json!({ "content": "Done — the fix is in place." }),
        serde_json::json!({ "content": "Still done." }),
        serde_json::json!({ "content": "Confirmed complete." }),
    ];
    let (_, rounds) = run_openai_script_in(script.clone(), ".").await;
    assert_eq!(
        rounds, 3,
        "an unverified conclusion in a workspace shipping `cargo test` costs a round per nudge, capped at SELF_VERIFY_CAP = 2 — that is #1943"
    );

    // The anti-vacuous twin, in the same test so the two can never drift: the
    // extra round is the GATE, not something else in the loop. The same script
    // against a workspace that affords nothing concludes in one.
    let (_, rounds) = run_openai_script_in(script, NO_CHECKS_WORKSPACE).await;
    assert_eq!(
        rounds, 1,
        "with nothing to verify the gate must stay silent — otherwise the assertion above is measuring some other nudge"
    );
}

/// #1259: `request_user_input` is a legitimate escalation in an **Explain**
/// turn — the boxed-in model formally asks the human instead of being forced
/// into penalized narration (the #1257 double-bind). Headless (no gate), the
/// dispatch returns the recoverable no-human message — never the
/// disposition-refusal, never a hang — and the turn completes normally.
/// Contrast pin in the same run: an Act-only tool (`run_command`) under the
/// same Explain turn still gets the disposition refusal (the boundary holds).
#[tokio::test]
async fn explain_turn_request_user_input_dispatches_and_completes() {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            last_content: Default::default(),
            round: round.clone(),
            script: vec![
                serde_json::json!({
                    "content": null,
                    "tool_calls": [{
                        "id": "c1", "type": "function",
                        "function": { "name": "request_user_input",
                                       "arguments": "{\"question\":\"Which directory should I size?\"}" }
                    }]
                }),
                serde_json::json!({
                    "content": null,
                    "tool_calls": [{
                        "id": "c2", "type": "function",
                        "function": { "name": "run_command",
                                       "arguments": "{\"command\":\"du -sh .\"}" }
                    }]
                }),
                serde_json::json!({ "content": "Understood — proceeding with the workspace root." }),
            ],
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.prompt_disposition = PromptDisposition::Explain;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp)
        .await
        .expect("an Explain turn asking the human completes, never errors");
    assert!(
        reply.contains("proceeding with the workspace root"),
        "the turn ends on the final answer: {reply}"
    );

    // Wire-level: what the loop fed back for each tool call.
    let requests = server.received_requests().await.expect("recorded");
    let bodies: Vec<String> = requests
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    let all = bodies.join("\n---\n");
    // The escalation DISPATCHED: its result is the recoverable headless
    // message, not the disposition refusal.
    assert!(
        all.contains("no human available this session"),
        "request_user_input must dispatch (headless => the recoverable no-human message): {all}"
    );
    assert!(
        !all.contains("Tool `request_user_input` is not available for this request"),
        "request_user_input must NOT be disposition-refused in an Explain turn"
    );
    // The boundary still holds for Act-only tools in the SAME turn.
    assert!(
        all.contains("Tool `run_command` is not available for this request"),
        "run_command must stay disposition-refused in an Explain turn: {all}"
    );
}

#[tokio::test]
async fn question_turns_are_never_nudged() {
    // Regression #1152/#1162 (the 2026-07-14 Opus session): the user asked
    // a QUESTION; the model's narrated answer classified as pending-action
    // and got nudged, seeding the "I'm genuinely finished" defense loop
    // (#1158). With the intent gate, a question turn takes its narration
    // as the final answer on ROUND ONE — no rescue, no residue.
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            last_content: Default::default(),
            round: round.clone(),
            // Phrasing the classifier reads as pending-action ("Let me…").
            script: vec![
                serde_json::json!({ "content": "Let me look into the harness next." }),
                serde_json::json!({ "content": "SHOULD NEVER BE REQUESTED" }),
            ],
        })
        .mount(&server)
        .await;
    let question =
        "Give me your top 5 improvements to make LLM effectiveness better inside this harness please?";
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user(question),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = question;
    c.narration_nudge_cap = 2; // budget available — the GATE must stop it
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(
        round.load(Ordering::SeqCst),
        1,
        "a question turn must never consume a rescue round"
    );
    assert!(
        reply.contains("look into"),
        "narration IS the answer: {reply}"
    );
}

#[tokio::test]
async fn stale_file_blocker_nudges_ground_truth_check_and_continues() {
    let blocker = "\
Summary

What happened: The lib.rs file I was editing grew from ~9400 to ~16808 lines \
between reads — likely modified concurrently by another agent or tool. This \
means my old edit contexts are stale.

Why I'm blocked: I cannot safely use edit_file on lib.rs because the file has \
been modified out from under me. My old line references and context are invalid \
for an 8400-line larger file.

Final Answer / Recommendation

The operator should restore lib.rs to a known-good state (e.g., git checkout \
newt-tui/src/lib.rs).";
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({ "content": blocker }),
        serde_json::json!({ "content": "Ground truth checked; lib.rs is clean, so I am continuing." }),
    ])
    .await;
    assert_eq!(
        rounds, 2,
        "stale-file blocker should get one verification nudge"
    );
    assert!(
        reply.contains("lib.rs is clean"),
        "returns the post-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("git checkout"),
        "must not accept the unverified revert recommendation: {reply}"
    );
}

#[test]
fn looks_like_unverified_stale_file_blocker_requires_file_stale_and_blocker_cues() {
    assert!(looks_like_unverified_stale_file_blocker(
        "The lib.rs file I was editing grew from ~9400 to ~16808 lines between reads. \
         Why I'm blocked: I cannot safely use edit_file because the file has been \
         modified out from under me. The operator should restore lib.rs."
    ));
    assert!(looks_like_unverified_stale_file_blocker(
        "My old line references are invalid and the context is stale. Any edit could \
         land in the wrong place and corrupt the code; recommendation: restore the file."
    ));
    assert!(!looks_like_unverified_stale_file_blocker(
        "The cache entry is stale, so I refreshed it and continued."
    ));
    assert!(!looks_like_unverified_stale_file_blocker(
        "I checked git diff and the file is clean, so I can continue from the verified contents."
    ));
}

#[test]
fn stale_file_ground_truth_nudge_names_read_only_checks_and_revert_guard() {
    let nudge = stale_file_ground_truth_nudge(true, &Caveats::top(), None);
    assert!(nudge.contains("git status --short"), "{nudge}");
    assert!(nudge.contains("git diff -- <file>"), "{nudge}");
    assert!(nudge.contains("wc -l <file>"), "{nudge}");
    assert!(nudge.contains("re-read the exact target range"), "{nudge}");
    assert!(
        nudge.contains("Never recommend git checkout/revert"),
        "{nudge}"
    );
}
