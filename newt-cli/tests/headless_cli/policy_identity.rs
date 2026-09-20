//! Actual wire requests and captured verification ground producer policy identity.
use super::*;
use serde_json::{json, Value};

fn primary_path(wire: &str) -> &'static str {
    match wire {
        "ollama" => "/api/chat",
        "anthropic" => "/v1/messages",
        "responses" => "/v1/responses",
        _ => "/v1/chat/completions",
    }
}

struct WireReply {
    wire: &'static str,
    calls: AtomicUsize,
}
impl Respond for WireReply {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let text = format!(
            "Completed response {}",
            self.calls.fetch_add(1, Ordering::SeqCst)
        );
        let response = match self.wire {
            "ollama" => {
                json!({"model":"fixture-model","message":{"role":"assistant","content":text},"done":true})
            }
            "anthropic" => {
                json!({"id":"msg_1","type":"message","role":"assistant","model":"fixture-model","stop_reason":"end_turn","content":[{"type":"text","text":text}],"usage":{"input_tokens":10,"output_tokens":5}})
            }
            "responses" => {
                json!({"id":"resp_1","status":"completed","model":"fixture-model","output":[{"type":"message","content":[{"type":"output_text","text":text}]}]})
            }
            _ if body["stream"] == true => {
                let frame = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
                return ResponseTemplate::new(200).set_body_raw(
                    format!("data: {frame}\n\ndata: [DONE]\n\n"),
                    "text/event-stream",
                );
            }
            _ => {
                json!({"model":"fixture-model","choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
            }
        };
        ResponseTemplate::new(200).set_body_json(response)
    }
}

struct WireFixture {
    root: tempfile::TempDir,
    primary: MockServer,
    auxiliary: MockServer,
    wire: &'static str,
    /// Extra `[backends.capability.*]` TOML, placed directly under the backend.
    declaration: String,
}
struct RunEvidence {
    contract: Value,
    result: Value,
    request: Value,
    request_path: String,
}
impl WireFixture {
    async fn new(wire: &'static str) -> Self {
        let primary = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(primary_path(wire)))
            .respond_with(WireReply {
                wire,
                calls: AtomicUsize::new(0),
            })
            .mount(&primary)
            .await;
        let auxiliary = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices":[{"message":{"role":"assistant","content":"\"answer\""},"finish_reason":"stop"}]
            })))
            .mount(&auxiliary).await;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        Self {
            root,
            primary,
            auxiliary,
            wire,
            declaration: String::new(),
        }
    }

    fn declaring(mut self, declaration: &str) -> Self {
        self.declaration = declaration.to_owned();
        self
    }

    /// Write the config and run the real binary once; the caller judges the outcome.
    async fn launch(
        &self,
        name: &str,
        smart: bool,
        navigation_calls: usize,
    ) -> (std::process::Output, std::path::PathBuf) {
        let root = self.root.path();
        let workspace = root.join("workspace");
        let instruction = workspace.join("instruction.md");
        let config = root.join("config.toml");
        let events = root.join(format!("{name}.jsonl"));
        let kind = match self.wire {
            "ollama" => "ollama",
            "anthropic" => "anthropic",
            _ => "openai",
        };
        let api = if self.wire == "responses" {
            "responses"
        } else {
            "chat_completions"
        };
        std::fs::write(&config, format!(
            "default_backend = \"fixture\"\n\
             [initiative.rounds]\nmeasured = 7\n\
             [[backends]]\nname = \"fixture\"\nendpoint = \"{}\"\n\
             model = \"fixture-model\"\nkind = \"{kind}\"\napi = \"{api}\"\n{declaration}\
             [smart_harness]\ndevice = \"cpu\"\nmax_navigation_calls = {navigation_calls}\n\
             [smart_harness.backend]\nendpoint = \"{}\"\nmodel = \"auxiliary-fixture\"\nkind = \"openai\"\n",
            self.primary.uri(), self.auxiliary.uri(), declaration = self.declaration
        )).unwrap();
        std::fs::write(
            &instruction,
            "Implement the requested change and run its test.\n",
        )
        .unwrap();
        let mut command = Command::cargo_bin("newt").unwrap();
        common::isolate_loopback_chat(&mut command, root);
        command
            .timeout(std::time::Duration::from_secs(30))
            .env("NEWT_ANTHROPIC_STREAM", "off")
            .args([
                "--tenacity",
                "resolute",
                "--initiative",
                "measured",
                "--cognition",
                "meticulous",
                "--config",
            ])
            .arg(&config)
            .args(["headless", "--cwd"])
            .arg(&workspace)
            .arg("--instruction-file")
            .arg(&instruction)
            .arg("--events")
            .arg(&events)
            .args(["--max-rounds", "1", "--context-window", "32768"]);
        if smart {
            command
                .arg("--smart-harness")
                .arg("--frame-dir")
                .arg(root.join("private-frame"));
        }
        (command.output().unwrap(), events)
    }

    async fn run(&self, name: &str, smart: bool, navigation_calls: usize) -> RunEvidence {
        let before = self
            .primary
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == primary_path(self.wire))
            .count();
        let auxiliary_before = self.auxiliary.received_requests().await.unwrap().len();
        let (output, events) = self.launch(name, smart, navigation_calls).await;
        let calls = self.primary.received_requests().await.unwrap();
        let diagnostic_requests: Vec<_> = calls
            .iter()
            .map(|request| {
                (
                    request.url.path().to_owned(),
                    String::from_utf8_lossy(&request.body).into_owned(),
                )
            })
            .collect();
        assert!(
            output.status.success(),
            "{} smart={smart}: status={} stderr={} stdout={} requests={diagnostic_requests:?}",
            self.wire,
            output.status,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        // Token-count probes get the mock server's normal unsupported404;
        // count actual primary inference separately from that admission probe.
        let calls: Vec<_> = calls
            .iter()
            .filter(|request| request.url.path() == primary_path(self.wire))
            .collect();
        assert_eq!(calls.len(), before + 1, "one real primary request");
        let aux_calls = self.auxiliary.received_requests().await.unwrap().len();
        if smart {
            assert!(
                aux_calls > auxiliary_before,
                "the real auxiliary must classify the reply"
            );
        } else {
            assert_eq!(aux_calls, auxiliary_before, "disabled Smart does not infer");
        }
        RunEvidence {
            contract: contract_from(&events),
            result: solve_result_from(&events),
            request: serde_json::from_slice(&calls[before].body).unwrap(),
            request_path: calls[before].url.path().to_owned(),
        }
    }
}

fn assert_exact_identity(record: &Value) {
    assert_eq!(record["contract_version"], "3");
    let claimed: content_addressable::ContentId = record["config_digest"]
        .as_str()
        .expect("an emitted identity")
        .parse()
        .unwrap();
    // Independent decoding, outside the producer helper, retains unknown keys.
    let decoded: Value = serde_json::from_str(&record.to_string()).unwrap();
    let expected = content_addressable::ContentId::from_canonical_bytes(
        &content_addressable::canonical::to_canonical_dagcbor(&decoded["effective_config"])
            .unwrap(),
    );
    assert_eq!(claimed, expected);
}

async fn assert_wire_policy(wire: &'static str, smart: bool) {
    let fixture = WireFixture::new(wire).await;
    let run = fixture.run("wire", smart, 32).await;
    let config = &run.contract["effective_config"];
    assert_eq!(config["semantic_cognition"], "meticulous");
    let receipt = &run.contract["receipt"]["verification"];
    assert_eq!(receipt["required_by_tenacity"], true);
    assert_eq!(receipt["mode"], "result_aware");
    assert_eq!(receipt["repair_allowance"], 3);
    assert_eq!(
        run.result["status"], "incomplete",
        "no observed test execution occurred"
    );
    assert!(run.request.get("reasoning_effort").is_none());
    assert!(run.request.get("chat_template_kwargs").is_none());
    match wire {
        "responses" => {
            assert_eq!(run.request_path, "/v1/responses");
            assert_eq!(run.request["reasoning"]["effort"], "high");
            assert_eq!(config["reasoning_effort"], "high");
            assert_eq!(config["output_allowance"]["tokens"], 16_000);
        }
        "anthropic" => {
            assert_eq!(run.request_path, "/v1/messages");
            assert_eq!(run.request["max_tokens"], 8_192);
            assert!(run.request.get("reasoning").is_none());
            assert_eq!(config["output_allowance"]["tokens"], 8_192);
        }
        _ => {
            assert_eq!(
                run.request_path,
                if wire == "ollama" {
                    "/api/chat"
                } else {
                    "/v1/chat/completions"
                }
            );
            assert!(run.request.get("reasoning").is_none());
            assert!(run.request.get("max_tokens").is_none());
            assert!(config.get("output_allowance").is_none());
        }
    }
    assert_eq!(config["verification"], *receipt);
    // Captured Chat controls belong to the Chat Completions wire alone.
    if wire == "chat" {
        assert_eq!(
            config["chat_completions"],
            json!({"capability": {}, "reasoning_replay_scope": "never"})
        );
    } else {
        assert!(config.get("chat_completions").is_none(), "{wire}: {config}");
    }
    if smart {
        let launch = &config["smart_harness"];
        assert!(launch["invocation_mode"].is_string(), "{launch}");
        assert!(launch.get("starting_cid").is_some(), "{launch}");
        assert!(launch["configuration"].is_object(), "{launch}");
        let head = run.result["smart_harness"]["head"]
            .as_str()
            .expect("the trace still carries the final head");
        assert!(
            !config.to_string().contains(head),
            "the final head is execution evidence, never hashed configuration"
        );
    } else {
        assert!(
            config.get("smart_harness").is_none(),
            "Smart off: no launch manifest"
        );
        assert!(run.result.get("smart_harness").is_none());
    }
    assert_eq!(
        json!({
            "wire_api": config["wire_api"],
            "initiative_read_only_rounds": config["initiative_read_only_rounds"],
            "tool_round_limit": config["tool_round_limit"],
        }),
        json!({
            "wire_api": if wire == "chat" { "chat_completions" } else { wire },
            "initiative_read_only_rounds": 7,
            "tool_round_limit": {
                "rounds": 1, "source": "override", "configured": 40, "tenacity": "resolute"
            },
        }),
        "captured dispatch and numeric policy must match this real request"
    );
    assert_exact_identity(&run.contract);
}

// Independent cases make every wire/Smart counterfactual execute on a red run.
macro_rules! wire_case {
    ($name:ident, $wire:literal, $smart:literal) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            assert_wire_policy($wire, $smart).await;
        }
    };
}
wire_case!(policy_identity_ollama_ordinary, "ollama", false);
wire_case!(policy_identity_ollama_smart, "ollama", true);
wire_case!(policy_identity_chat_ordinary, "chat", false);
wire_case!(policy_identity_chat_smart, "chat", true);
wire_case!(policy_identity_responses_ordinary, "responses", false);
wire_case!(policy_identity_responses_smart, "responses", true);
wire_case!(policy_identity_anthropic_ordinary, "anthropic", false);
wire_case!(policy_identity_anthropic_smart, "anthropic", true);

/// Ground the immutable-launch projection in real distinct Smart sessions:
/// runtime heads remain visible, but cannot alter identity of the same policy.
#[tokio::test(flavor = "multi_thread")]
async fn smart_launch_identity_excludes_final_head_and_tracks_changed_configuration() {
    let fixture = WireFixture::new("chat").await;
    let first = fixture.run("first", true, 32).await;
    let second = fixture.run("second", true, 32).await;
    assert_ne!(
        first.result["smart_harness"]["head"],
        second.result["smart_harness"]["head"]
    );
    assert!(first.result["smart_harness"]["head"].is_string());
    assert!(
        first.contract["effective_config"]["smart_harness"]
            .get("head")
            .is_none(),
        "final execution head belongs to the trace, not captured policy"
    );
    assert_exact_identity(&first.contract);
    assert_exact_identity(&second.contract);
    assert_eq!(
        first.contract["effective_config"],
        second.contract["effective_config"]
    );
    assert_eq!(
        first.contract["config_digest"],
        second.contract["config_digest"]
    );
    let changed = fixture.run("changed", true, 33).await;
    assert_exact_identity(&changed.contract);
    assert_ne!(
        first.contract["config_digest"],
        changed.contract["config_digest"]
    );
}

/// Identical semantic/numeric policy on two unsupported-cognition wires must
/// still have distinct identities for their actual configured dispatch APIs.
#[tokio::test(flavor = "multi_thread")]
async fn policy_identity_distinguishes_chat_and_ollama_dispatch() {
    let chat = WireFixture::new("chat").await.run("chat", false, 32).await;
    let ollama = WireFixture::new("ollama")
        .await
        .run("ollama", false, 32)
        .await;
    assert_eq!(chat.request_path, "/v1/chat/completions");
    assert_eq!(ollama.request_path, "/api/chat");
    assert_exact_identity(&chat.contract);
    assert_exact_identity(&ollama.contract);
    let mut chat_config = chat.contract["effective_config"].clone();
    let mut ollama_config = ollama.contract["effective_config"].clone();
    assert_eq!(
        chat_config.as_object_mut().unwrap().remove("wire_api"),
        Some(json!("chat_completions"))
    );
    assert_eq!(
        ollama_config.as_object_mut().unwrap().remove("wire_api"),
        Some(json!("ollama"))
    );
    // The captured Chat controls are wire-scoped too: Ollama has none to capture.
    assert!(chat_config
        .as_object_mut()
        .unwrap()
        .remove("chat_completions")
        .is_some());
    assert_eq!(
        chat_config, ollama_config,
        "only wire-scoped fields distinguish these policies"
    );
    assert_ne!(
        chat.contract["config_digest"],
        ollama.contract["config_digest"]
    );
}

/// Suite #2449: a REAL pre-admission failure (a declared Responses effort
/// ladder that does not advertise the selected cognition) returns an error
/// outcome before any request. The record may claim the captured launch policy
/// but no verification gate, wire effort or technique receipt that never ran.
#[tokio::test(flavor = "multi_thread")]
async fn pre_admission_failure_claims_no_verification_or_wire_evidence() {
    let fixture = WireFixture::new("responses")
        .await
        .declaring("[backends.capability.responses]\nreasoning_effort = [\"low\"]\n");
    let (output, events) = fixture.launch("refused", false, 32).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "an admission failure exits 1"
    );
    let requests = fixture.primary.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.url.path() != primary_path("responses")),
        "no primary inference may precede the admission failure"
    );
    let (contract, result) = (contract_from(&events), solve_result_from(&events));
    assert_ne!(contract["outcome"], "completed");
    assert!(
        result["error"].as_str().unwrap().contains("not advertised"),
        "{result}"
    );
    let config = &contract["effective_config"];
    for key in ["verification", "reasoning_effort", "responses_capability"] {
        assert!(
            config.get(key).is_none(),
            "{key} never instantiated: {config}"
        );
    }
    assert!(contract["receipt"]["verification"].is_null(), "{contract}");
    for (name, entry) in contract["receipt"]["features"].as_object().unwrap() {
        assert_ne!(
            entry["state"], "instantiated",
            "{name} never ran: {contract}"
        );
    }
    assert_eq!(config["semantic_cognition"], "meticulous");
    assert_exact_identity(&contract);
}

/// Suite #2449: accepted Chat Completions controls change the request body, so
/// they must change the configuration identity too; identical captured
/// controls must not.
#[tokio::test(flavor = "multi_thread")]
async fn accepted_chat_controls_are_part_of_the_configuration_identity() {
    const BASE: &str =
        "[backends.capability.chat_completions]\ncognition = true\nchat_template_kwargs = true\n";
    const NO_KWARGS: &str =
        "[backends.capability.chat_completions]\ncognition = true\nchat_template_kwargs = false\n";
    const NO_PARALLEL: &str = "[backends.capability.chat_completions]\ncognition = true\nchat_template_kwargs = true\nparallel_tool_calls = false\n";
    let mut runs = Vec::new();
    for declaration in [BASE, BASE, NO_KWARGS, NO_PARALLEL] {
        let fixture = WireFixture::new("chat").await.declaring(declaration);
        let run = fixture.run("chat", false, 32).await;
        assert_exact_identity(&run.contract);
        runs.push(run);
    }
    // Ground the claim: these controls really change what is sent.
    assert_eq!(
        runs[0].contract["effective_config"]["chat_completions"],
        json!({
            "capability": {"cognition": true, "chat_template_kwargs": true},
            "reasoning_replay_scope": "never"
        })
    );
    assert!(runs[0].request.get("chat_template_kwargs").is_some());
    assert!(runs[2].request.get("chat_template_kwargs").is_none());
    assert_eq!(runs[3].request["parallel_tool_calls"], false);
    let digest = |i: usize| runs[i].contract["config_digest"].clone();
    assert_eq!(
        digest(0),
        digest(1),
        "identical captured controls, identical identity"
    );
    assert_ne!(digest(0), digest(2), "chat_template_kwargs alone");
    assert_ne!(digest(0), digest(3), "parallel_tool_calls alone");
    assert_ne!(digest(2), digest(3));
}
