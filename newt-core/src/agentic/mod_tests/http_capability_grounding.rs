//! Fresh capability claims must be grounded in this turn, on every provider wire.
use super::*;
use crate::agentic::smart_harness::{AdjudicationSettings, SmartHarness};
use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};

const CLAIMS: [&str; 2] = [
    "The capability probes came back. cargo --version: command not found (exit 127).",
    "I've re-probed directly this round — the toolchain is still absent.",
];
const NO_PROBES: &str = "No fresh capability checks were performed this turn.";
const WIRES: [&str; 4] = ["ollama", "openai", "anthropic", "responses"];

fn response(wire: &str, text: &str, tool: Option<&str>) -> ResponseTemplate {
    let args = match tool {
        Some("run_command") => serde_json::json!({"command":"cargo --version"}),
        Some("lifecycle") => serde_json::json!({"phase":"test", "action":"run"}),
        Some("git") => serde_json::json!({"op":"status"}),
        _ => serde_json::json!({}),
    };
    let value = match (wire, tool) {
        ("ollama", Some(name)) => serde_json::json!({"message":{"role":"assistant","content":"",
            "tool_calls":[{"function":{"name":name,"arguments":args}}]},"done":true}),
        ("ollama", None) => {
            serde_json::json!({"message":{"role":"assistant","content":text},"done":true})
        }
        ("anthropic", Some(name)) => serde_json::json!({"id":"msg_1","type":"message",
            "role":"assistant","model":"test-model","stop_reason":"tool_use",
            "content":[{"type":"tool_use","id":"call_1","name":name,"input":args}]}),
        ("anthropic", None) => serde_json::json!({"id":"msg_1","type":"message",
            "role":"assistant","model":"test-model","stop_reason":"end_turn",
            "content":[{"type":"text","text":text}],"usage":{"input_tokens":10,"output_tokens":5}}),
        ("responses", Some(name)) => serde_json::json!({"id":"resp_1","status":"completed",
            "output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":name,
            "arguments":args.to_string()}]}),
        ("responses", None) => serde_json::json!({"id":"resp_1","status":"completed",
            "model":"test-model","output":[{"type":"message","id":"msg_1","role":"assistant",
            "status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}]}),
        (_, Some(name)) => {
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":"",
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":name,
            "arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        (_, None) => serde_json::json!({"choices":[{"message":{"role":"assistant","content":text},
            "finish_reason":"stop"}]}),
    };
    ResponseTemplate::new(200).set_body_json(value)
}

struct FailingGit;
impl crate::agentic::GitTool for FailingGit {
    fn dispatch(
        &self,
        _op: &str,
        _args: &serde_json::Value,
        _caps: &crate::git_caveats::GitCaveats,
        _session: &Caveats,
    ) -> Result<String, String> {
        Err("capability denied: git status unavailable in this fixture".into())
    }
}

struct Scenario<'a> {
    wire: &'a str,
    smart: bool,
    text: &'a str,
    cap: usize,
    first_tool: Option<&'a str>,
    reject_tools: bool,
    record_events: bool,
    streaming: bool,
    refusal: bool,
}

impl<'a> Scenario<'a> {
    fn new(wire: &'a str, text: &'a str) -> Self {
        Self {
            wire,
            text,
            smart: false,
            cap: 4,
            first_tool: None,
            reject_tools: false,
            record_events: true,
            streaming: false,
            refusal: false,
        }
    }
}

async fn run(scenario: Scenario<'_>) -> (String, Vec<Request>, Vec<crate::ToolEvent>) {
    let (text, _, requests, events) = run_with_stream_state(scenario).await;
    (text, requests, events)
}

async fn run_with_stream_state(
    scenario: Scenario<'_>,
) -> (String, bool, Vec<Request>, Vec<crate::ToolEvent>) {
    let _lock = env_lock().await;
    let _verify = EnvVar::set("NEWT_SELF_VERIFY", "0");
    let _stream = EnvVar::set(
        "NEWT_ANTHROPIC_STREAM",
        if scenario.streaming { "on" } else { "off" },
    );
    let _confined = EnvVar::unset("NEWT_DISABLE_OCAP");
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let wire = scenario.wire.to_owned();
    let text = scenario.text.to_owned();
    let first_tool = scenario.first_tool.map(str::to_owned);
    let reject_tools = scenario.reject_tools;
    let streaming = scenario.streaming;
    let refusal = scenario.refusal;
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            if reject_tools && index == 0 {
                return ResponseTemplate::new(400)
                    .set_body_string("this model does not support tools");
            }
            if wire == "anthropic" && streaming {
                let frames = [
                    serde_json::json!({"type":"message_start","message":{"model":"test-model","usage":{"input_tokens":10}}}),
                    serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
                    serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                    serde_json::json!({"type":"content_block_stop","index":0}),
                    serde_json::json!({"type":"message_delta","delta":{"stop_reason":if refusal {"refusal"} else {"end_turn"}},"usage":{"output_tokens":5}}),
                    serde_json::json!({"type":"message_stop"}),
                ];
                let body: String = frames.iter().map(|frame| format!("data: {frame}\n\n")).collect();
                return ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream");
            }
            if wire == "responses" && refusal {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id":"resp_refused", "status":"completed", "output":[{
                        "type":"message", "role":"assistant", "content":[{
                            "type":"refusal", "refusal":text
                        }]
                    }]
                }));
            }
            response(
                &wire,
                &text,
                (index == 0).then_some(first_tool.as_deref()).flatten(),
            )
        })
        .mount(&server)
        .await;
    let harness = SmartHarness::new(
        agent_harness::Session::new(Default::default()).unwrap(),
        Arc::new(|_| Box::pin(async { Ok(("\"answer\"".to_string(), None)) })),
        AdjudicationSettings::default(),
    )
    .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    if scenario.first_tool == Some("lifecycle") {
        std::fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname = \"capability-probe-fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
    }
    let workspace_path = workspace.path().to_string_lossy();
    let (uri, messages) = (server.uri(), msgs());
    let mut caveats = Caveats::top();
    // A real returned refusal grounds an attempt; no host compiler is spawned.
    caveats.exec = crate::Scope::none();
    let mut context = ctx(&uri, &messages, &caveats);
    context.workspace = &workspace_path;
    context.task = "Continue the implementation and check the available toolchain.";
    context.max_tool_rounds = scenario.cap;
    context.narration_nudge_cap = 0;
    context.smart_harness = scenario.smart.then_some(&harness);
    context.kind = match scenario.wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    let mut events = Vec::new();
    context.tool_events = scenario.record_events.then_some(&mut events);
    context.git_tool = Some(&FailingGit);
    let result = if scenario.wire == "responses" {
        openai_responses_complete(context, &mut NoMcp).await
    } else {
        chat_complete(context, &mut NoMcp).await
    }
    .expect("scripted capability turn");
    (
        result.0,
        result.1,
        server.received_requests().await.unwrap(),
        events,
    )
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn persistent_fresh_probe_claim_gets_one_correction_on_every_wire() {
    for wire in WIRES {
        for smart in [false, true] {
            for claim in CLAIMS {
                let mut scenario = Scenario::new(wire, claim);
                scenario.smart = smart;
                let (reply, requests, events) = run(scenario).await;
                assert!(reply.contains(NO_PROBES), "{wire} smart={smart}: {reply}");
                assert_eq!(
                    requests.len(),
                    2,
                    "one bounded correction: {wire} smart={smart}"
                );
                assert!(events.is_empty(), "prose never mints a tool event");
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn last_round_annotates_without_an_extra_dispatch() {
    for wire in WIRES {
        for smart in [false, true] {
            let mut scenario = Scenario::new(wire, CLAIMS[0]);
            scenario.smart = smart;
            scenario.cap = 1;
            let (reply, requests, events) = run(scenario).await;
            assert!(reply.contains(NO_PROBES), "{wire} smart={smart}: {reply}");
            assert_eq!(requests.len(), 1, "round limit: {wire} smart={smart}");
            assert!(events.is_empty());
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn tools_unsupported_annotates_without_requesting_an_impossible_probe() {
    for wire in WIRES {
        let mut scenario = Scenario::new(wire, CLAIMS[1]);
        scenario.reject_tools = true;
        let (reply, requests, events) = run(scenario).await;
        assert!(reply.contains(NO_PROBES), "{wire}: {reply}");
        assert_eq!(requests.len(), 2, "unsupported-tools retry only: {wire}");
        assert!(events.is_empty());
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn returned_probe_failure_counts_even_without_an_external_event_recorder() {
    for wire in WIRES {
        for tool in ["run_command", "lifecycle", "git"] {
            for record_events in [false, true] {
                let mut scenario = Scenario::new(wire, CLAIMS[1]);
                scenario.first_tool = Some(tool);
                scenario.record_events = record_events;
                let (reply, requests, events) = run(scenario).await;
                assert!(!reply.contains(NO_PROBES), "{wire} {tool}: {reply}");
                assert_eq!(requests.len(), 2, "one tool and one answer: {wire} {tool}");
                if record_events {
                    assert!(
                        events.iter().any(|event| event.tool == tool && !event.ok),
                        "{events:?}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn retrieving_old_context_does_not_corroborate_a_fresh_probe() {
    for wire in WIRES {
        let mut scenario = Scenario::new(wire, CLAIMS[0]);
        scenario.first_tool = Some("resume_context");
        let (reply, requests, events) = run(scenario).await;
        assert!(events.iter().any(|event| event.tool == "resume_context"));
        assert!(reply.contains(NO_PROBES), "{wire}: {reply}");
        assert_eq!(
            requests.len(),
            3,
            "context read plus bounded correction: {wire}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn history_intent_and_explicit_nonverification_are_not_fresh_probe_claims() {
    for text in [
        "The previous session reported missing cargo; I have not re-probed this turn.",
        "I have not re-probed directly this round.",
        "I will re-probe the toolchain before claiming it is absent.",
        "I will do a direct re-probe of cargo next.",
        "The earlier capability probes came back in the previous session, not this turn.",
    ] {
        for wire in ["ollama", "openai"] {
            let (reply, requests, events) = run(Scenario::new(wire, text)).await;
            assert!(!reply.contains(NO_PROBES), "{wire}: {reply}");
            assert_eq!(requests.len(), 1, "no capability correction for {text}");
            assert!(events.is_empty());
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn tools_disabled_cap_exit_summary_still_checks_fresh_probe_claims() {
    for wire in WIRES {
        let mut scenario = Scenario::new(wire, CLAIMS[1]);
        scenario.cap = 1;
        scenario.first_tool = Some("resume_context");
        let (reply, requests, events) = run(scenario).await;
        assert!(events.iter().any(|event| event.tool == "resume_context"));
        assert!(reply.contains(NO_PROBES), "{wire}: {reply}");
        assert_eq!(
            requests.len(),
            2,
            "one tool round plus final summary: {wire}"
        );
        let summary = body_json(requests.last().unwrap());
        assert!(
            summary
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
            "cap summary cannot dispatch more tools: {summary}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn anthropic_streamed_answers_do_not_hide_appended_evidence_warnings() {
    for refusal in [false, true] {
        let mut scenario = Scenario::new("anthropic", CLAIMS[1]);
        scenario.streaming = true;
        scenario.refusal = refusal;
        scenario.cap = 1;
        let (reply, streamed, requests, events) = run_with_stream_state(scenario).await;
        assert!(reply.contains(NO_PROBES), "{reply}");
        assert!(
            !streamed,
            "the caller must render the appended warning, including a refusal"
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(body_json(&requests[0])["stream"], true);
        assert!(events.is_empty());
    }
    // Prove the fixture really exercised streaming: an unchanged answer retains
    // the already-displayed marker and is not needlessly printed twice.
    let mut scenario = Scenario::new("anthropic", "The task remains unfinished.");
    scenario.streaming = true;
    scenario.cap = 1;
    let (reply, streamed, _, _) = run_with_stream_state(scenario).await;
    assert_eq!(reply, "The task remains unfinished.");
    assert!(streamed);
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn responses_smart_refusal_is_annotated_without_action_pressure() {
    for smart in [false, true] {
        let mut scenario = Scenario::new("responses", CLAIMS[0]);
        scenario.smart = smart;
        scenario.refusal = true;
        let (reply, requests, events) = run(scenario).await;
        assert!(reply.contains(NO_PROBES), "smart={smart}: {reply}");
        assert_eq!(
            requests.len(),
            1,
            "a refusal cannot trigger a probing retry"
        );
        assert!(events.is_empty());
    }
}
