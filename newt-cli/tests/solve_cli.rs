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
        let streaming = body["stream"].as_bool().unwrap_or(false);
        self.requests
            .lock()
            .expect("request capture lock")
            .push(body);
        // #123: the accepted round is re-issued with `stream: true` and served
        // as SSE. Captured like any other request — it goes to the same
        // endpoint and carries the same wire controls, so the assertions about
        // both apply to it too.
        if streaming {
            let frame = serde_json::json!({"choices": [{"delta": {"content": "done"}}]});
            let sse = format!("data: {frame}\n\ndata: [DONE]\n\n");
            return ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream");
        }
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
    // The probe round plus its #123 streaming re-issue — both to the CLI
    // endpoint, both naming the CLI-overridden model.
    assert_eq!(requests.len(), 2, "the CLI endpoint served the turn");
    for request in requests.iter() {
        assert_eq!(request["model"], "operator-model");
    }
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
    // The probe round plus its #123 streaming re-issue. Neither may carry
    // cognition controls: a dial is an intent, not evidence the endpoint
    // supports the wire fields, and the streamed round is the same wire.
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert!(request.get("max_tokens").is_none());
        assert!(request.get("chat_template_kwargs").is_none());
    }
    drop(requests);

    let contract = contract_from(&events_path);
    assert_eq!(contract["effective_config"]["cognition"], "default");
}

// ── Phase 27 BAT/UAT: the round-cap honesty gate ──────────────────────────
//
// Replays the SHAPE of the 2026-09-07 newt-on-newt run (issue #2212) against a
// SCRIPTED model: writes land early, then the run grinds read-only calls until
// the tool-round cap ends it. Measured from that trajectory — `edit_file` at
// calls 25, 26 and 27 of 128, then 101 further calls with no write among them,
// so 79% of the run happened after the work was done. It exited
// `end_reason: Some(RoundCap)` and the summary line still said
// `"status": "completed"`.
//
// THE ASSERTION IS ABOUT THE HARNESS, NOT THE MODEL. A model that over-elaborates
// is not a defect this gate can fix. Reporting that run as `completed` is.
//
//   If the turn ended at the tool-round cap, the emitted summary must not
//   report the run as completed.
//
// Both fields already exist and are already emitted — `TurnEndReason::RoundCap`
// (newt-core/src/metrics.rs) and the solve_result line's `status`. So this gate
// invents no vocabulary and binds no field that is still being designed: it
// pins the RELATIONSHIP between two shipped fields. Whatever names the 27.5
// honesty fix chooses, `status` must stop saying "completed" here.
//
// Deliberately ONE reason to be red. Naming the dominant failure mode
// (write-complete-then-grind) needs a typed vocabulary that does not exist in
// `TurnEndReason` or the solve contract today; asserting it as a substring of
// prose would be a gate the fix could satisfy without fixing anything. That is
// specified as a follow-up against #2212, not smuggled in here.
//
// Why real tools, not hallucinated ones: `resolve_tool_alias`
// (agentic/tools/catalog.rs) may now correct an unknown name, and
// `RepeatCallGuard` (agentic/mod.rs) short-circuits an EXACT repeat before
// dispatch. This fixture therefore calls only real tools and gives every
// read-only round a DISTINCT path, so the rounds are burned the way the
// captured run burned them — by legitimate, succeeding, redundant work — and
// not by a mechanism that already has its own guard.
const CAP_ROUNDS: usize = 8;
const EARLY_WRITES: usize = 3;

/// Reads the ONE `solve_result` line. Sibling of [`contract_from`], which
/// filters for the contract record; this filters for its complement.
fn solve_result_from(path: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).expect("read solve events");
    let mut results = raw
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("event line is JSON"))
        .filter(|record| record.get("kind").and_then(|k| k.as_str()) == Some("solve_result"));
    let result = results.next().expect("one solve_result record");
    assert!(results.next().is_none(), "exactly one solve_result record");
    result
}

/// Writes on the first [`EARLY_WRITES`] rounds, then grinds distinct read-only
/// calls forever — never volunteering a final answer.
struct WritesEarlyThenGrindsReadOnly {
    round: AtomicUsize,
    rounds_served: Arc<AtomicUsize>,
}

impl Respond for WritesEarlyThenGrindsReadOnly {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("chat request is JSON");

        // No tools advertised => this is the cap-exit summary request, not a
        // tool round. Answer it plausibly: the point of the gate is that a
        // WELL-FORMED summary is still reported dishonestly.
        if body.get("tools").is_none() {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": NEMOTRON_MODEL,
                "choices": [{
                    "message": {"role": "assistant", "content": "I made the three edits and verified them."},
                    "finish_reason": "stop"
                }]
            }));
        }

        let n = self.rounds_served.fetch_add(1, Ordering::SeqCst);
        let _ = self.round.fetch_add(1, Ordering::SeqCst);

        let call = if n < EARLY_WRITES {
            // The work itself — real writes, counted by `write_calls`.
            serde_json::json!({
                "id": format!("write_{n}"),
                "type": "function",
                "function": {
                    "name": "write_file",
                    "arguments": serde_json::to_string(&serde_json::json!({
                        "path": format!("src/produced_{n}.rs"),
                        "content": format!("pub const PRODUCED_{n}: u32 = {n};\n"),
                    })).expect("write args serialize")
                }
            })
        } else {
            // The grind — succeeding, legitimate, redundant, and DISTINCT each
            // round so the repeat guard does not absorb it.
            serde_json::json!({
                "id": format!("read_{n}"),
                "type": "function",
                "function": {
                    "name": "read_file",
                    "arguments": serde_json::to_string(&serde_json::json!({
                        "path": format!("src/seed_{n}.rs"),
                    })).expect("read args serialize")
                }
            })
        };

        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": NEMOTRON_MODEL,
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [call]},
                "finish_reason": "tool_calls"
            }]
        }))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn round_cap_exit_is_not_reported_as_a_completed_run() {
    let server = MockServer::start().await;
    let rounds_served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(WritesEarlyThenGrindsReadOnly {
            round: AtomicUsize::new(0),
            rounds_served: rounds_served.clone(),
        })
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().expect("temporary solve workspace");
    let src = workspace.path().join("src");
    std::fs::create_dir_all(&src).expect("workspace src dir");
    // One readable seed per grinding round, so every read SUCCEEDS. A grind
    // made of failures would be a different defect (thrash), already covered by
    // `uat_thrash_run_gets_honest_cap_exit_not_raise_the_limit`.
    for n in 0..(CAP_ROUNDS + 4) {
        std::fs::write(
            src.join(format!("seed_{n}.rs")),
            format!("pub const SEED_{n}: u32 = {n};\n"),
        )
        .expect("write seed file");
    }

    let config_path = workspace.path().join("solve.toml");
    let instruction_path = workspace.path().join("instruction.md");
    let events_path = workspace.path().join("events.jsonl");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "capped"

[[backends]]
name = "capped"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
"#,
            server.uri()
        ),
    )
    .expect("write solve config");
    std::fs::write(
        &instruction_path,
        "Add the three produced constants, then verify them.\n",
    )
    .expect("write solve instruction");

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
        .args(["--max-rounds", &CAP_ROUNDS.to_string()])
        .assert()
        .success();

    let result = solve_result_from(&events_path);

    // ── anti-vacuity ──────────────────────────────────────────────────────
    // A replay harness that silently ran zero rounds must not be able to pass.
    // Three INDEPENDENT counters: the scripted model's own count, the harness's
    // dispatch count, and the early-write count that makes this THIS scenario
    // rather than any capped run.
    let served = rounds_served.load(Ordering::SeqCst);
    assert_eq!(
        served, CAP_ROUNDS,
        "the scripted model must have served exactly {CAP_ROUNDS} tool rounds; \
         served {served} — the replay did not run the trajectory it claims to"
    );
    assert_eq!(
        result["tool_calls"], CAP_ROUNDS as u64,
        "the harness must have dispatched every scripted round: {result}"
    );
    assert_eq!(
        result["write_calls"], EARLY_WRITES as u64,
        "the early-write half of the shape must have happened — without it this \
         is a generic capped run, not the write-complete-then-grind case: {result}"
    );

    // ── the precondition ──────────────────────────────────────────────────
    // If the run did not actually end at the cap, the fixture failed to
    // reproduce the scenario and the honesty assertion below would be vacuous.
    assert_eq!(
        result["end_reason"], "Some(RoundCap)",
        "fixture must reproduce a cap exit, else the honesty assertion is vacuous: {result}"
    );

    // ── the honesty clause (Phase 27.5) ───────────────────────────────────
    // RED against current main: newt-cli/src/solve.rs derives `status` from
    // `outcome.error.is_none()` alone and never consults `end_reason`.
    assert_ne!(
        result["status"], "completed",
        "a run that ended at the tool-round cap must not report itself completed \
         — end_reason says RoundCap on the same line: {result}"
    );

    // The contract line carries the SAME claim through a second derivation
    // (`solve_contract::outcome_label`), so it can drift from `status` unless
    // both are pinned. Asserted in the same relationship form — NOT against a
    // literal value — so this stays correct under any naming the honesty fix
    // chooses. `contract_from` also pins that exactly one contract record
    // exists, so a run that emitted none cannot pass here.
    let contract = contract_from(&events_path);
    assert_ne!(
        contract["outcome"], "completed",
        "the contract record must not report a cap exit as completed either: {contract}"
    );
}
