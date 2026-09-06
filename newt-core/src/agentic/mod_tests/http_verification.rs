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
    c.workspace = workspace;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    (reply, round.load(Ordering::SeqCst))
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
    let nudge = stale_file_ground_truth_nudge();
    assert!(nudge.contains("git status --short"), "{nudge}");
    assert!(nudge.contains("git diff -- <file>"), "{nudge}");
    assert!(nudge.contains("wc -l <file>"), "{nudge}");
    assert!(nudge.contains("re-read the exact target range"), "{nudge}");
    assert!(
        nudge.contains("Never recommend git checkout/revert"),
        "{nudge}"
    );
}
