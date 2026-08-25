//! Process-level regressions for `newt solve`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const NEMOTRON_MODEL: &str = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16";

fn has_one_round_action_nudge(body: &serde_json::Value) -> bool {
    body["messages"].as_array().is_some_and(|messages| {
        messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.starts_with("[1 read-only rounds so far."))
        })
    })
}

struct ReadThenFinishOnNudge {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    sequence: AtomicUsize,
}

struct CaptureThenFinish {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Respond for CaptureThenFinish {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("chat request is JSON");
        self.requests
            .lock()
            .expect("request capture lock")
            .push(body);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": NEMOTRON_MODEL,
            "choices": [{
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }]
        }))
    }
}

fn contract_from(path: &std::path::Path) -> serde_json::Value {
    let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .expect("read solve events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    let mut contracts = records
        .into_iter()
        .filter(|record| record.get("contract_version").is_some());
    let contract = contracts.next().expect("one solve contract");
    assert!(contracts.next().is_none(), "exactly one solve contract");
    contract
}

fn advertised_tool(body: &serde_json::Value, name: &str) -> bool {
    body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["function"]["name"].as_str() == Some(name))
    })
}

impl Respond for ReadThenFinishOnNudge {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("chat request is JSON");
        self.requests
            .lock()
            .expect("request capture lock")
            .push(body.clone());

        if has_one_round_action_nudge(&body) || body.get("tools").is_none() {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": NEMOTRON_MODEL,
                "choices": [{
                    "message": {"role": "assistant", "content": "nudge received; done"},
                    "finish_reason": "stop"
                }]
            }));
        }

        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": NEMOTRON_MODEL,
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": format!("read_{sequence}"),
                        "type": "function",
                        "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
    }
}

/// Grounds the in-process tenacity/unit-loop tests with a real `newt solve`
/// subprocess, an explicitly loaded TOML file, a real temporary workspace, and
/// the real `list_dir` dispatch. The inference service alone is mocked so the
/// test can inspect the second request and prove the TYPED card-family
/// attribution (#1819: an exact catalog card declaring `family = "nemotron"`,
/// associated through the SelectedModel principal — never a model-name
/// substring) changed runtime behavior as well as the emitted contract.
#[tokio::test(flavor = "multi_thread")]
async fn explicit_config_applies_nemotron_tenacity_to_runtime_and_contract() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ReadThenFinishOnNudge {
            requests: requests.clone(),
            sequence: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().expect("temporary solve workspace");
    let config_path = workspace.path().join("benchmark.toml");
    let instruction_path = workspace.path().join("instruction.md");
    let events_path = workspace.path().join("events.jsonl");
    // The family arrives TYPED: an exact catalog card in the config's
    // sibling models/ dir declares `family = "nemotron"`, bound to this
    // backend's declared model — the model NAME is deliberately an alias
    // the old substring matcher would also have caught, so this fixture
    // proves the typed path carries it now.
    let models_dir = workspace.path().join("models");
    std::fs::create_dir_all(&models_dir).expect("models dir");
    std::fs::write(
        models_dir.join("nemo-run.toml"),
        format!(
            "name = \"nemo-run\"\nbackend = \"vllm\"\nfamily = \"nemotron\"\n\n[vllm]\nserved_name = \"{NEMOTRON_MODEL}\"\n"
        ),
    )
    .expect("write family card");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "nemotron"

[[backends]]
name = "nemotron"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
card = "nemo-run"

[tenacity]
default = "relaxed"

[tenacity.families]
nemotron = "relentless"
"#,
            server.uri()
        ),
    )
    .expect("write explicit solve config");
    std::fs::write(
        &instruction_path,
        "Inspect the workspace, then complete the task.\n",
    )
    .expect("write solve instruction");
    std::fs::write(workspace.path().join("tool-ground-truth.txt"), "present\n")
        .expect("write list_dir ground-truth marker");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["solve", "--cwd"])
        .arg(workspace.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "2"])
        .assert()
        .success();

    let requests = requests.lock().expect("request capture lock");
    assert!(requests.len() >= 2, "expected at least two chat requests");
    let second_messages = requests[1]["messages"]
        .as_array()
        .expect("second request messages");
    let second_user_messages: Vec<&str> = second_messages
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        has_one_round_action_nudge(&requests[1]),
        "relentless must inject its action nudge after the first read-only round: \
         {second_user_messages:?}"
    );
    let tool_results: Vec<&str> = second_messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        tool_results
            .iter()
            .any(|result| result.contains("tool-ground-truth.txt")),
        "the real list_dir round must succeed against the temporary workspace: {tool_results:?}"
    );
    drop(requests);

    let records: Vec<serde_json::Value> = std::fs::read_to_string(&events_path)
        .expect("read solve events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    let contracts: Vec<&serde_json::Value> = records
        .iter()
        .filter(|record| record.get("contract_version").is_some())
        .collect();
    assert_eq!(
        contracts.len(),
        1,
        "solve emits exactly one contract record"
    );
    assert_eq!(contracts[0]["effective_config"]["tenacity"], "relentless");
}

/// The anti-substring negative: the SAME nemotron-looking model alias with
/// NO card gets NO family attribution — the `[tenacity.families]` default
/// must not engage from the model NAME, so the contract records the
/// config default.
#[tokio::test(flavor = "multi_thread")]
async fn a_cardless_nemotron_looking_alias_gets_no_family_tenacity() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ReadThenFinishOnNudge {
            requests: requests.clone(),
            sequence: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().expect("temporary solve workspace");
    let config_path = workspace.path().join("benchmark.toml");
    let instruction_path = workspace.path().join("instruction.md");
    let events_path = workspace.path().join("events.jsonl");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "nemotron"

[[backends]]
name = "nemotron"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"

[tenacity]
default = "relaxed"

[tenacity.families]
nemotron = "relentless"
"#,
            server.uri()
        ),
    )
    .expect("write explicit solve config");
    std::fs::write(&instruction_path, "Complete the task.\n").expect("write instruction");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["solve", "--cwd"])
        .arg(workspace.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "2"])
        .assert()
        .success();

    let records: Vec<serde_json::Value> = std::fs::read_to_string(&events_path)
        .expect("read solve events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    let contract = records
        .iter()
        .find(|record| record.get("contract_version").is_some())
        .expect("solve emits a contract record");
    assert_eq!(
        contract["effective_config"]["tenacity"], "relaxed",
        "a model-name alias is a LABEL — with no exact card family, the \
         per-family default must not engage"
    );
}

/// Grounds the headless crew gate with a real `newt solve` subprocess. The
/// backend is mocked only to retain request #1; the CLI must construct the real
/// `LocalCrewRunner`, the shared driver must advertise its tools, and the
/// contract must report the same resolved posture that actually hit the wire.
#[tokio::test(flavor = "multi_thread")]
async fn solve_crew_and_obsessive_postures_reach_wire_and_contract() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let fixture = tempfile::tempdir().expect("temporary solve fixture");
    let config_path = fixture.path().join("benchmark.toml");
    let instruction_path = fixture.path().join("instruction.md");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "nemotron"

[[backends]]
name = "nemotron"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
api = "chat_completions"

[backends.capability]
reasoning_replay_scope = "current_user_turn"

[backends.capability.chat_completions]
cognition = true
chat_template_kwargs = true
parallel_tool_calls = false
bounded_reasoning_continuation = true
"#,
            server.uri()
        ),
    )
    .expect("write explicit solve config");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write solve instruction");

    let cases = [
        ("crew", false, "default", "standard"),
        ("obsessive", true, "contemplating", "relentless"),
    ];
    for (name, obsessive, cognition, tenacity) in cases {
        let workspace = fixture.path().join(format!("ws-{name}"));
        std::fs::create_dir(&workspace).expect("create solve workspace");
        let events_path = fixture.path().join(format!("events-{name}.jsonl"));
        let mut command = Command::cargo_bin("newt").expect("newt binary");
        command.env_remove("NEWT_TEAM");
        if obsessive {
            command.arg("--obsessive");
        } else {
            command.env("NEWT_TEAM", "1");
        }
        command
            .arg("--config")
            .arg(&config_path)
            .args(["solve", "--cwd"])
            .arg(&workspace)
            .arg("--instruction-file")
            .arg(&instruction_path)
            .arg("--events")
            .arg(&events_path)
            .args(["--max-rounds", "1"])
            .assert()
            .success();

        let body = requests
            .lock()
            .expect("request capture lock")
            .pop()
            .expect("one captured request");
        assert!(
            advertised_tool(&body, "crew") && advertised_tool(&body, "compose_roster"),
            "{name} must advertise the real crew surface: {body}"
        );
        if obsessive {
            assert_eq!(body["max_tokens"], 16000);
            assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        } else {
            assert!(
                body.get("chat_template_kwargs").is_none(),
                "default cognition means Newt sends no thinking selection: {body}"
            );
        }

        let contract = contract_from(&events_path);
        assert_eq!(contract["effective_config"]["cognition"], cognition);
        assert_eq!(contract["effective_config"]["crew"], "on");
        assert_eq!(contract["effective_config"]["tenacity"], tenacity);
    }
}

/// An explicit config file selects the configuration source, but it must not
/// defeat the higher-precedence per-invocation backend pin. This real process
/// test leaves the file's endpoint unreachable: success therefore proves the
/// request used the CLI endpoint, not merely that the contract was rewritten.
#[tokio::test(flavor = "multi_thread")]
async fn explicit_config_still_honors_cli_backend_override() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let stale_server = MockServer::start().await;
    let stale_requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: stale_requests.clone(),
        })
        .mount(&stale_server)
        .await;

    let fixture = tempfile::tempdir().expect("temporary solve fixture");
    let config_path = fixture.path().join("benchmark.toml");
    let instruction_path = fixture.path().join("instruction.md");
    let events_path = fixture.path().join("events.jsonl");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "stale"

[[backends]]
name = "stale"
endpoint = "{}"
model = "stale-model"
kind = "openai"
"#,
            stale_server.uri()
        ),
    )
    .expect("write explicit solve config");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write solve instruction");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .args(["--backend-endpoint", &server.uri()])
        .args(["--backend-model", "operator-model"])
        .args(["--backend-kind", "openai"])
        .arg("--config")
        .arg(&config_path)
        .args(["solve", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "1"])
        .assert()
        .success();

    let requests = requests.lock().expect("request capture lock");
    assert_eq!(requests.len(), 1, "the CLI endpoint served the turn");
    assert_eq!(requests[0]["model"], "operator-model");
    drop(requests);
    assert!(
        stale_requests
            .lock()
            .expect("stale request lock")
            .is_empty(),
        "the explicit file's stale backend must not receive the turn"
    );

    let contract = contract_from(&events_path);
    assert_eq!(contract["requested_model"], "operator-model");
    assert_eq!(contract["backend"]["name"], "cli");
}

/// A cognition dial is an intent, not evidence that a backend received the
/// corresponding wire controls. Unknown Chat Completions endpoints retain the
/// historical request shape, so their contract must say `default` as well.
#[tokio::test(flavor = "multi_thread")]
async fn unsupported_chat_cognition_is_default_on_wire_and_contract() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let fixture = tempfile::tempdir().expect("temporary solve fixture");
    let config_path = fixture.path().join("benchmark.toml");
    let instruction_path = fixture.path().join("instruction.md");
    let events_path = fixture.path().join("events.jsonl");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "unknown-chat"

[[backends]]
name = "unknown-chat"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
api = "chat_completions"
"#,
            server.uri()
        ),
    )
    .expect("write explicit solve config");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write solve instruction");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .args(["--cognition", "contemplating"])
        .arg("--config")
        .arg(&config_path)
        .args(["solve", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "1"])
        .assert()
        .success();

    let requests = requests.lock().expect("request capture lock");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].get("max_tokens").is_none());
    assert!(requests[0].get("chat_template_kwargs").is_none());
    drop(requests);

    let contract = contract_from(&events_path);
    assert_eq!(contract["effective_config"]["cognition"], "default");
}
