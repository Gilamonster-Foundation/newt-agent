use super::*;

/// Real file reads supply fresh evidence on each round. The backend completes
/// after three reads, so a one-round cap needs two renewals and a final answer
/// round. This grounds the progress policy in all four production HTTP loops.
struct ToolsThenAnswer {
    wire: &'static str,
    calls: Arc<AtomicUsize>,
    tools: Vec<(&'static str, serde_json::Value)>,
    answer: &'static str,
}

impl Respond for ToolsThenAnswer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let tool = self.tools.get(n);
        let call = request_has_tools(req) && tool.is_some();
        let (name, args) = tool.cloned().unwrap_or(("", serde_json::Value::Null));
        let answer = if tool.is_none() {
            self.answer
        } else {
            "Premature stop."
        };
        let body = match self.wire {
            "ollama" => serde_json::json!({"message": {
                "role":"assistant", "content": if call { "" } else { answer },
                "tool_calls": if call { vec![serde_json::json!({"function": {
                    "name":name, "arguments":args
                }})] } else { vec![] }
            }, "done":true}),
            "anthropic" => serde_json::json!({"id":format!("msg_{n}"),"type":"message",
                "role":"assistant","model":"test-model", "stop_reason":if call {"tool_use"} else {"end_turn"},
                "content":if call { vec![serde_json::json!({"type":"tool_use","id":format!("call_{n}"),
                    "name":name,"input":args})] } else {vec![serde_json::json!({"type":"text","text":answer})]}
            }),
            "responses" => serde_json::json!({"id":format!("resp_{n}"),"status":"completed",
                "output":if call {vec![serde_json::json!({"type":"function_call","id":format!("fc_{n}"),
                    "call_id":format!("call_{n}"),"name":name,"arguments":args.to_string()})]}
                    else {vec![serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":answer}]})]}
            }),
            _ => serde_json::json!({"choices":[{"message":{"role":"assistant",
                "content":if call {""} else {answer},
                "tool_calls":if call {vec![serde_json::json!({"type":"function","id":format!("call_{n}"),
                    "function":{"name":name,"arguments":args.to_string()}})]} else {vec![]}},
                "finish_reason":if call {"tool_calls"} else {"stop"}}]}),
        };
        ResponseTemplate::new(200).set_body_json(body)
    }
}

async fn complete_read_assignment(
    wire: &'static str,
    grace: usize,
    smart: bool,
    calls_allowed: Option<u32>,
) -> (anyhow::Result<String>, usize) {
    let _env = crate::agentic::anthropic_loop_tests::test_env(false);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(ToolsThenAnswer {
            wire,
            calls: calls.clone(),
            tools: (0..3)
                .map(|n| {
                    (
                        "read_file",
                        serde_json::json!({"path":format!("evidence-{n}.txt")}),
                    )
                })
                .collect(),
            answer: "All three files read.",
        })
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    for n in 0..3 {
        std::fs::write(
            workspace.path().join(format!("evidence-{n}.txt")),
            format!("Fact {n}.\n"),
        )
        .unwrap();
    }
    let messages = vec![MemMessage::user(
        "Read the three evidence files and report their contents.",
    )];
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    let uri = server.uri();
    let harness = crate::agentic::smart_harness::SmartHarness::new(
        agent_harness::Session::new(crate::test_guard::unbudgeted_session_config()).unwrap(),
        Arc::new(|_| Box::pin(async { Ok(("\"answer\"".into(), None)) })),
        crate::agentic::smart_harness::AdjudicationSettings::default(),
    )
    .unwrap();
    let kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "Read the three evidence files and report their contents.",
        kind,
    );
    ctx.api_key = Some("sk-test");
    ctx.workspace = workspace.path().to_str().unwrap();
    ctx.safe_context = None;
    ctx.max_tool_rounds = 1;
    ctx.workflow_grace_rounds = grace;
    ctx.action_nudges = false;
    ctx.smart_harness = smart.then_some(&harness);
    let allowance = calls_allowed.map(crate::agentic::run_allowance::RunAllowance::new);
    ctx.run_allowance = allowance.as_ref();
    let result = if wire == "responses" {
        openai_responses_complete(ctx, &mut NoMcp).await
    } else {
        chat_complete(ctx, &mut NoMcp).await
    };
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| !String::from_utf8_lossy(&r.body).contains("<workflow_state>")),
        "cap renewal must not inject a new model-facing protocol"
    );
    (result.map(|result| result.0), calls.load(Ordering::SeqCst))
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn fresh_evidence_renews_rounds_without_nudges_on_every_provider() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        for smart in [false, true] {
            let (answer, calls) = complete_read_assignment(wire, 1, smart, None).await;
            let answer = answer.unwrap_or_else(|error| panic!("{wire}: {error:#}"));
            assert_eq!(
                answer, "All three files read.",
                "{wire}, smart={smart}: {answer}"
            );
            assert_eq!(
                calls, 4,
                "{wire}, smart={smart}: three tools and their final answer"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn zero_grace_preserves_operator_hard_cap_on_every_provider() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let (answer, calls) = complete_read_assignment(wire, 0, false, None).await;
        let answer = answer.unwrap_or_else(|error| panic!("{wire}: {error:#}"));
        assert!(!answer.contains("All three files read."), "{wire}");
        assert_eq!(calls, 2, "{wire}: one tool and the cap summary");
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn automatic_renewal_preserves_explicit_inference_call_budget() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let (result, calls) = complete_read_assignment(wire, 1, false, Some(2)).await;
        let error = result.expect_err("progress cannot enlarge an operator's call budget");
        assert!(
            format!("{error:#}").contains("run allowance is exhausted"),
            "{wire}: {error:#}"
        );
        assert_eq!(
            calls, 2,
            "{wire}: the refused third request must not reach the provider"
        );
    }
}

/// Capture the actual provider requests after enough distinct real reads to
/// trigger initiative reminders and cross more than one renewable boundary.
async fn read_only_nudge_requests(wire: &'static str, grace: usize) -> (usize, Vec<String>) {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(ToolsThenAnswer {
            wire,
            calls: calls.clone(),
            tools: (0..7)
                .map(|n| {
                    (
                        "read_file",
                        serde_json::json!({"path":format!("evidence-{n}.txt")}),
                    )
                })
                .collect(),
            answer: "All seven files read.",
        })
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    for n in 0..7 {
        std::fs::write(
            workspace.path().join(format!("evidence-{n}.txt")),
            format!("Fact {n}.\n"),
        )
        .unwrap();
    }
    let task = "Read the evidence files and report their contents.";
    let messages = vec![MemMessage::user(task)];
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    let uri = server.uri();
    let kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, kind);
    ctx.api_key = Some("sk-test");
    ctx.workspace = workspace.path().to_str().unwrap();
    ctx.safe_context = None;
    ctx.max_tool_rounds = 4;
    ctx.workflow_grace_rounds = grace;
    ctx.action_nudges = true;
    let result = if wire == "responses" {
        openai_responses_complete(ctx, &mut NoMcp).await
    } else {
        chat_complete(ctx, &mut NoMcp).await
    }
    .unwrap_or_else(|error| panic!("{wire}, grace={grace}: {error:#}"));
    if grace > 0 {
        assert_eq!(result.0, "All seven files read.", "{wire}");
    }
    let requests = server.received_requests().await.unwrap();
    (
        calls.load(Ordering::SeqCst),
        requests
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body).into_owned())
            .collect(),
    )
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn renewable_read_only_nudges_do_not_invent_a_final_round_countdown() {
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    crate::initiative::set_initiative_config(Default::default());
    crate::initiative::set_cli_initiative(crate::initiative::Initiative::Measured);
    let _env = crate::agentic::anthropic_loop_tests::test_env(false);
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let (calls, requests) = read_only_nudge_requests(wire, 2).await;
        assert_eq!(calls, 8, "{wire}: seven reads and their final answer");
        let reminders: Vec<_> = requests
            .iter()
            .filter(|request| request.contains("read-only rounds so far"))
            .collect();
        assert_eq!(reminders.is_empty(), wire == "responses", "{wire}");
        assert!(
            reminders
                .iter()
                .all(|request| !request.contains("round(s) left")
                    && !request.contains("configured hard limit")),
            "{wire}: renewable reminders must not claim a final deadline: {reminders:#?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn zero_grace_read_only_nudges_describe_only_rounds_after_the_current_round() {
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    crate::initiative::set_initiative_config(Default::default());
    crate::initiative::set_cli_initiative(crate::initiative::Initiative::Measured);
    let _env = crate::agentic::anthropic_loop_tests::test_env(false);
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let (calls, requests) = read_only_nudge_requests(wire, 0).await;
        assert_eq!(calls, 5, "{wire}: four tool rounds and the cap summary");
        let final_tool_request = &requests[3];
        if wire == "responses" {
            assert!(!final_tool_request.contains("read-only rounds so far"));
        } else {
            assert!(final_tool_request.contains(
                "After this round, at most 0 further tool rounds remain under the configured hard limit."
            ), "{wire}: the currently admitted round remains usable: {final_tool_request}");
        }
    }
}

/// Grounds the Lab failure in the real tool dispatcher: an invalid working
/// directory followed by a successful file observation is ordinary recovery,
/// not an instruction to manufacture a source edit. All provider requests must
/// retain both results without injecting a repair-lock or rediscovery prompt.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn recovered_command_error_does_not_invent_a_repair_task_on_any_provider() {
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    crate::initiative::set_initiative_config(Default::default());
    crate::initiative::set_cli_initiative(crate::initiative::Initiative::Measured);
    let _env = crate::agentic::anthropic_loop_tests::test_env(false);
    let command = if cfg!(windows) {
        "cmd.exe /d /c cd"
    } else {
        "pwd"
    };
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let answer =
            "Summary of findings: the cwd error is resolved; the root evidence is available.";
        Mock::given(method("POST"))
            .respond_with(ToolsThenAnswer {
                wire,
                calls: calls.clone(),
                tools: vec![
                    (
                        "run_command",
                        serde_json::json!({"command":command, "cwd":"missing-subdirectory"}),
                    ),
                    ("read_file", serde_json::json!({"path":"evidence.txt"})),
                ],
                answer,
            })
            .mount(&server)
            .await;
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("evidence.txt"),
            "Root evidence is available.\n",
        )
        .unwrap();
        let task = "Inspect the workspace and report the findings.";
        let messages = vec![MemMessage::user(task)];
        // Windows: this is a recovery-transcript fixture, not an AppContainer
        // proof. Unrestricted axes let the invalid cwd reach the operating
        // system; AppContainer evidence lives in the dedicated Windows
        // evidence lane.
        #[cfg(windows)]
        let caveats = crate::caveats::Caveats::top();
        // Everywhere else: exercise the invalid cwd under the same confined
        // caveats (network excepted) this scenario runs under in practice,
        // so the non-Windows lane still covers "confined except network"
        // rather than losing that coverage to the Windows exception above.
        #[cfg(not(windows))]
        let caveats = {
            let mut caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
            caveats.net = crate::caveats::Scope::All;
            caveats
        };
        let uri = server.uri();
        let kind = match wire {
            "ollama" => BackendKind::Ollama,
            "anthropic" => BackendKind::Anthropic,
            _ => BackendKind::Openai,
        };
        let mut events = Vec::new();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, kind);
        ctx.api_key = Some("sk-test");
        ctx.workspace = workspace.path().to_str().unwrap();
        ctx.safe_context = None;
        ctx.max_tool_rounds = crate::TuiConfig::default().max_tool_rounds;
        ctx.workflow_grace_rounds = crate::TuiConfig::default().workflow_grace_rounds;
        ctx.action_nudges = true;
        ctx.tool_events = Some(&mut events);
        let result = if wire == "responses" {
            openai_responses_complete(ctx, &mut NoMcp).await
        } else {
            chat_complete(ctx, &mut NoMcp).await
        };
        assert_eq!(result.unwrap().0, answer, "{wire}");
        assert_eq!(events.len(), 2, "{wire}: {events:?}");
        assert!(!events[0].ok, "{wire}: the invalid cwd must really fail");
        assert!(
            events[1].ok,
            "{wire}: the subsequent observation must succeed"
        );
        let requests = server.received_requests().await.unwrap();
        let request_text: Vec<_> = requests
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body))
            .collect();
        assert!(
            request_text
                .last()
                .unwrap()
                .contains("No such file or directory")
                || request_text
                    .last()
                    .unwrap()
                    .contains("cannot find the path")
                // Windows reports an absent process working directory as
                // ERROR_DIRECTORY (267), whose system message is this text.
                || request_text
                    .last()
                    .unwrap()
                    .contains("directory name is invalid"),
            "{wire}: failure must come from the absent cwd: {}",
            request_text.last().unwrap()
        );
        assert!(
            request_text.iter().all(|r| !r.contains("<workflow_state>")
                && !r.contains("You are rediscovering an error")),
            "{wire}: ordinary tool recovery must not become a repair instruction"
        );
        assert!(
            request_text
                .last()
                .unwrap()
                .contains("Root evidence is available."),
            "{wire}: the observation remains available to the model"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "{wire}: recovery does not need a forced extra turn"
        );
    }
}
