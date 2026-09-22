//! Process-level regressions for `newt headless`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

mod common;

const NEMOTRON_MODEL: &str = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16";

/// Grounds frame-directory admission with the real headless entrypoint. The
/// deliberately invalid auxiliary placement proves isolation is checked first,
/// while an isolated store proceeds to the independent auxiliary validation.
#[test]
fn smart_solve_admits_private_storage_before_loading_the_auxiliary() {
    for (exposed, extra_grant, unsafe_exec) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (false, false, false),
    ] {
        let mut command = common::newt();
        let home = command.home().to_path_buf();
        common::isolate_loopback_chat(&mut *command, &home);
        let workspace = home.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let frame = if exposed {
            workspace.join("frame")
        } else {
            home.join("private-frame")
        };
        let config = home.join("config.toml");
        std::fs::write(
            &config,
            r#"default_backend = "fixture"
[[backends]]
name = "fixture"
endpoint = "http://127.0.0.1:1"
model = "fixture"
kind = "openai"
[smart_harness]
device = "cuda"
"#,
        )
        .unwrap();
        let instruction = workspace.join("task.md");
        std::fs::write(&instruction, "Read the workspace.").unwrap();
        command
            .arg("--config")
            .arg(&config)
            .args(["headless", "--smart-harness", "--cwd"])
            .arg(&workspace)
            .arg("--instruction-file")
            .arg(&instruction)
            .arg("--frame-dir")
            .arg(&frame);
        if extra_grant {
            command.env("NEWT_READ_PATHS", &home);
        }
        if unsafe_exec {
            command.arg("--unsafe-host-exec");
        }
        let expected = if unsafe_exec {
            "requires confined launch authority"
        } else if !cfg!(any(target_os = "linux", target_os = "macos"))
            || !newt_core::ocap_l3_backend().1
        {
            "requires object-bound filesystem tools and a supported kernel sandbox"
        } else if exposed || extra_grant {
            "frame storage overlaps model filesystem authority"
        } else {
            // The all-clear case reaches the auxiliary stage: `device = "cuda"`
            // with no external backend is refused there (embedded is cpu-only),
            // which proves storage admission ran first without loading a model.
            "requires an external backend"
        };
        command
            .assert()
            .failure()
            .stderr(predicates::str::contains(expected));
        assert!(!frame.exists(), "rejected launch must not create a frame");
    }
}

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
        // The primary request uses SSE. Every generation reports 7 output tokens, so a contract that counts
        // two generations for one answer reads 14 (#2372).
        if streaming {
            let frame = serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]});
            let usage = serde_json::json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 7}});
            let sse = format!("data: {frame}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
            return ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream");
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": NEMOTRON_MODEL,
            "choices": [{
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 7}
        }))
    }
}

fn contract_from(path: &std::path::Path) -> serde_json::Value {
    let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .expect("read headless events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    let mut contracts = records
        .into_iter()
        .filter(|record| record.get("contract_version").is_some());
    let contract = contracts.next().expect("one headless contract");
    assert!(contracts.next().is_none(), "exactly one headless contract");
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

/// Grounds the in-process initiative/unit-loop tests with a real `newt headless`
/// subprocess, an explicitly loaded TOML file, a real temporary workspace, and
/// the real `list_dir` dispatch. The inference service alone is mocked so the
/// test can inspect the second request and prove the TYPED card-family
/// attribution (#1819: an exact catalog card declaring `family = "nemotron"`,
/// associated through the SelectedModel principal — never a model-name
/// substring) changed runtime behavior as well as the emitted contract.
///
/// The config is deliberately in the PRE-SPLIT shape (`[tenacity]` default
/// and families): this is the importer's end-to-end regression (slice 1b).
/// Without the importer on the load path the table is ignored, the family
/// default never engages, and no nudge fires within two rounds. An explicit
/// `--config` is not the operator's own config, so it is translated in memory
/// with a warning and left unchanged on disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_pre_split_config_applies_nemotron_initiative_to_runtime_and_contract() {
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

    let workspace = tempfile::tempdir().expect("temporary headless workspace");
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
    .expect("write explicit headless config");
    let config_before = std::fs::read_to_string(&config_path).expect("read config back");
    std::fs::write(
        &instruction_path,
        "Inspect the workspace, then complete the task.\n",
    )
    .expect("write headless instruction");
    std::fs::write(workspace.path().join("tool-ground-truth.txt"), "present\n")
        .expect("write list_dir ground-truth marker");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(workspace.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "2"])
        .assert()
        .success()
        .stderr(predicates::str::contains("uses old psyche labels"));
    assert_eq!(
        std::fs::read_to_string(&config_path).expect("read config back"),
        config_before,
        "an explicit config is migrated in memory, never rewritten"
    );

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
        "eager must inject its action nudge after the first read-only round: \
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
        .expect("read headless events")
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
        "headless emits exactly one contract record"
    );
    assert_eq!(contracts[0]["contract_version"], "2");
    assert_eq!(contracts[0]["effective_config"]["initiative"], "eager");
    assert_eq!(
        contracts[0]["effective_config"]["tenacity"], "normal",
        "a family sets initiative, never tenacity"
    );
}

/// The anti-substring negative: the SAME nemotron-looking model alias with
/// NO card gets NO family attribution — the `[initiative.families]` default
/// must not engage from the model NAME, so the contract records the
/// config default.
#[tokio::test(flavor = "multi_thread")]
async fn a_cardless_nemotron_looking_alias_gets_no_family_initiative() {
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

    let workspace = tempfile::tempdir().expect("temporary headless workspace");
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

[initiative]
default = "patient"

[initiative.families]
nemotron = "eager"
"#,
            server.uri()
        ),
    )
    .expect("write explicit headless config");
    std::fs::write(&instruction_path, "Complete the task.\n").expect("write instruction");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(workspace.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "2"])
        .assert()
        .success();

    let records: Vec<serde_json::Value> = std::fs::read_to_string(&events_path)
        .expect("read headless events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    let contract = records
        .iter()
        .find(|record| record.get("contract_version").is_some())
        .expect("headless emits a contract record");
    assert_eq!(
        contract["effective_config"]["initiative"], "patient",
        "a model-name alias is a LABEL — with no exact card family, the \
         per-family default must not engage"
    );
    assert_eq!(contract["effective_config"]["tenacity"], "normal");
}

/// Grounds the headless crew gate with a real `newt headless` subprocess. The
/// backend is mocked only to retain request #1; the CLI must construct the real
/// `LocalCrewRunner`, the shared driver must advertise its tools, and the
/// contract must report the same resolved posture that actually hit the wire.
#[tokio::test(flavor = "multi_thread")]
async fn headless_crew_and_obsessive_postures_reach_wire_and_contract() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let fixture = tempfile::tempdir().expect("temporary headless fixture");
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
    .expect("write explicit headless config");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");

    // Slice 1b: obsessive leaves initiative alone, so every case reports
    // the default `measured` (before the split obsessive's relentless
    // tenacity also meant nudge-after-one).
    let cases = [
        ("bare", false, "default", "normal"),
        ("crew", true, "default", "normal"),
        ("obsessive", true, "meticulous", "relentless"),
    ];
    for (name, crew, cognition, tenacity) in cases {
        let obsessive = name == "obsessive";
        let workspace = fixture.path().join(format!("ws-{name}"));
        std::fs::create_dir(&workspace).expect("create headless workspace");
        let events_path = fixture.path().join(format!("events-{name}.jsonl"));
        let mut command = Command::cargo_bin("newt").expect("newt binary");
        command.env_remove("NEWT_TEAM");
        if obsessive {
            command.arg("--obsessive");
        } else if crew {
            command.env("NEWT_TEAM", "1");
        }
        command
            .arg("--config")
            .arg(&config_path)
            .args(["headless", "--cwd"])
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
        assert_eq!(advertised_tool(&body, "crew"), crew, "{name}: {body}");
        assert_eq!(
            advertised_tool(&body, "compose_roster"),
            crew,
            "{name}: {body}"
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
        assert_eq!(
            contract["effective_config"]["crew"],
            if crew { "on" } else { "off" }
        );
        assert_eq!(contract["effective_config"]["tenacity"], tenacity);
        assert_eq!(
            contract["effective_config"]["initiative"], "measured",
            "{name}: obsessive leaves initiative alone"
        );

        // #2314: the feature receipt agrees with the wire body above in BOTH
        // arms. These runs supply no scratchpad seed and headless builds no
        // retrieval index, so both stay off the wire and are never reported active.
        let features = &contract["receipt"]["features"];
        let crew_state = if crew { "instantiated" } else { "unavailable" };
        assert_eq!(features["crew"]["state"], crew_state, "{name}: {contract}");
        assert!(!advertised_tool(&body, "code_search"));
        assert!(!advertised_tool(&body, "state_set"));
        // `</state>`, not `<state>`: tool descriptions name the opening tag,
        // only a rendered block (scratchpad.rs) closes it.
        assert!(!body.to_string().contains("</state>"));
        assert_eq!(features["code_search"]["state"], "unsupported");
        assert_eq!(features["scratchpad"]["state"], "unavailable");
    }
}

/// #2312: `newt headless` refuses an invalid output allowance at admission, like a
/// required feature it cannot supply: no model request, no events file.
#[tokio::test(flavor = "multi_thread")]
async fn headless_refuses_an_invalid_output_allowance_before_inference() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let fixture = tempfile::tempdir().expect("temporary headless fixture");
    let instruction_path = fixture.path().join("instruction.md");
    let events_path = fixture.path().join("events.jsonl");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");
    for (extra, expected) in [
        (vec!["--output-allowance", "0"], "output_allowance 0 permits no output"),
        (
            vec!["--output-allowance", "40000", "--context-window", "32768"],
            "output_allowance 40000 leaves no input room in the declared 32768-token context window",
        ),
    ] {
        Command::cargo_bin("newt")
            .expect("newt binary")
            .env_remove("NEWT_TEAM")
            .args(["--backend-endpoint", &server.uri()])
            .args(["--backend-model", NEMOTRON_MODEL])
            .args(["--backend-kind", "openai"])
            .args(["headless", "--cwd"])
            .arg(fixture.path())
            .arg("--instruction-file")
            .arg(&instruction_path)
            .arg("--events")
            .arg(&events_path)
            .args(extra)
            .assert()
            .failure()
            .stderr(predicates::str::contains(expected));
    }
    assert!(
        requests.lock().expect("request capture lock").is_empty(),
        "a refused allowance must not reach the model"
    );
    assert!(!events_path.exists(), "a refused run records no trace");
}

/// #2312: a headless run can set its output allowance, and the contract says
/// whether the server was told (`max_tokens` on the wire) or newt only reserved
/// it locally. Every contract read is paired with the captured bodies, so an
/// `enforced` predicted from configuration rather than the request cannot pass.
#[tokio::test(flavor = "multi_thread")]
async fn headless_output_allowance_reports_server_or_local_enforcement_from_the_wire() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let fixture = tempfile::tempdir().expect("temporary headless fixture");
    let instruction_path = fixture.path().join("instruction.md");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");

    // A cognition-projecting endpoint takes the cap from the CLI flag; an
    // unknown compatible endpoint takes it from `[[model_tuning]]`.
    let projecting = format!(
        r#"default_backend = "nemotron"

[[backends]]
name = "nemotron"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
api = "chat_completions"

[backends.capability.chat_completions]
cognition = true
"#,
        server.uri()
    );
    let unknown = format!(
        r#"default_backend = "plain"

[[backends]]
name = "plain"
endpoint = "{}"
model = "plain-model"
kind = "openai"
api = "chat_completions"

[[model_tuning]]
model = "plain-model"
output_allowance = 12000
"#,
        server.uri()
    );
    for (name, config, flag, enforced) in [
        ("projecting", projecting, true, "server"),
        ("unknown", unknown, false, "local"),
    ] {
        let config_path = fixture.path().join(format!("{name}.toml"));
        let events_path = fixture.path().join(format!("events-{name}.jsonl"));
        std::fs::write(&config_path, config).expect("write headless config");
        let mut command = Command::cargo_bin("newt").expect("newt binary");
        command
            .env_remove("NEWT_TEAM")
            .arg("--config")
            .arg(&config_path)
            .args(["headless", "--cwd"])
            .arg(fixture.path())
            .arg("--instruction-file")
            .arg(&instruction_path)
            .arg("--events")
            .arg(&events_path)
            .args(["--max-rounds", "1"]);
        if flag {
            command.args(["--output-allowance", "12000"]);
        }
        command.assert().success();

        let bodies = std::mem::take(&mut *requests.lock().expect("request capture lock"));
        assert!(!bodies.is_empty(), "{name}: the run reached the model");
        for body in &bodies {
            let sent = body.get("max_tokens");
            if enforced == "server" {
                assert_eq!(sent, Some(&serde_json::json!(12000)), "{name}: {body}");
            } else {
                assert_eq!(sent, None, "{name}: no cap may be sent: {body}");
            }
        }
        assert_eq!(
            contract_from(&events_path)["effective_config"]["output_allowance"],
            serde_json::json!({"tokens": 12000, "enforced": enforced}),
            "{name}"
        );
    }
}

/// #2313: `newt headless --run-allowance 0` configures an already-exhausted
/// budget, so the first (and only) dispatch attempt is refused before any
/// wire bytes are sent — the model never sees a request.
#[tokio::test(flavor = "multi_thread")]
async fn headless_with_an_exhausted_run_allowance_refuses_dispatch_before_any_request() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let fixture = tempfile::tempdir().expect("temporary headless fixture");
    let instruction_path = fixture.path().join("instruction.md");
    let events_path = fixture.path().join("events.jsonl");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .args(["--backend-endpoint", &server.uri()])
        .args(["--backend-model", NEMOTRON_MODEL])
        .args(["--backend-kind", "openai"])
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--run-allowance", "0"])
        // Changed ON PURPOSE (was `.failure()`): exhaustion is now a TYPED, clean
        // stop filed like the round cap (exit 0, `timeout`/`incomplete`), with a
        // harness-written notice — not an untyped `harness_error`.
        .assert()
        .success()
        .stdout(predicates::str::contains("run allowance is exhausted"));

    assert!(
        requests.lock().expect("request capture lock").is_empty(),
        "an exhausted run allowance must not reach the model"
    );
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&events_path)
        .expect("events")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    let result = lines
        .iter()
        .find(|r| r["kind"] == "solve_result")
        .expect("solve_result");
    assert_eq!(result["end_reason"], "Some(RunAllowance)", "{result}");
    assert_eq!(result["status"], "incomplete");
    let contract = lines
        .iter()
        .find(|r| r.get("contract_version").is_some())
        .expect("contract");
    assert_eq!(contract["outcome"], "timeout");
}

/// #2313: a configured, non-exhausted run allowance is reported in the
/// contract's effective_config, mirroring output_allowance's own reporting —
/// and a run with NO run allowance configured is completely unaffected: no
/// key in effective_config, and the request still reaches the model.
#[tokio::test(flavor = "multi_thread")]
async fn headless_run_allowance_is_reported_when_set_and_absent_when_not() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let fixture = tempfile::tempdir().expect("temporary headless fixture");
    let instruction_path = fixture.path().join("instruction.md");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");

    for (name, extra, expect_key) in [
        (
            "configured",
            vec!["--run-allowance".to_string(), "5".to_string()],
            true,
        ),
        ("unconfigured", vec![], false),
    ] {
        let events_path = fixture.path().join(format!("events-{name}.jsonl"));
        Command::cargo_bin("newt")
            .expect("newt binary")
            .env_remove("NEWT_TEAM")
            .args(["--backend-endpoint", &server.uri()])
            .args(["--backend-model", NEMOTRON_MODEL])
            .args(["--backend-kind", "openai"])
            .args(["headless", "--cwd"])
            .arg(fixture.path())
            .arg("--instruction-file")
            .arg(&instruction_path)
            .arg("--events")
            .arg(&events_path)
            .args(["--max-rounds", "1"])
            .args(&extra)
            .assert()
            .success();

        assert!(
            !requests.lock().expect("request capture lock").is_empty(),
            "{name}: the run reached the model"
        );
        requests.lock().expect("request capture lock").clear();

        let effective_config = contract_from(&events_path)["effective_config"].clone();
        if expect_key {
            assert_eq!(
                effective_config["run_allowance"],
                serde_json::json!(5),
                "{name}"
            );
        } else {
            assert!(
                effective_config.get("run_allowance").is_none(),
                "{name}: no configured allowance must report no key: {effective_config}"
            );
        }
    }
}

/// #2314: a required feature this run cannot supply stops the headless before any
/// model request, naming the feature and why. `.failure()` alone would also
/// pass for an unknown flag, so each refusal asserts its stderr text; the twin
/// shows the same guard admitting a requirement the run does supply.
#[tokio::test(flavor = "multi_thread")]
async fn a_required_feature_that_cannot_be_supplied_fails_before_inference() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let fixture = tempfile::tempdir().expect("temporary headless fixture");
    let instruction_path = fixture.path().join("instruction.md");
    let seed_path = fixture.path().join("seed.json");
    let events_path = fixture.path().join("events.jsonl");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");
    std::fs::write(&seed_path, r#"{"k": "v"}"#).expect("write scratchpad seed");
    let headless = |extra: &[&std::ffi::OsStr]| {
        let mut command = Command::cargo_bin("newt").expect("newt binary");
        command
            .env_remove("NEWT_TEAM")
            .args(["--backend-endpoint", &server.uri()])
            .args(["--backend-model", NEMOTRON_MODEL])
            .args(["--backend-kind", "openai"])
            .args(["headless", "--cwd"])
            .arg(fixture.path())
            .arg("--instruction-file")
            .arg(&instruction_path)
            .arg("--events")
            .arg(&events_path)
            .args(["--max-rounds", "1"])
            .args(extra);
        command
    };

    for (feature, state) in [
        ("code_search", "unsupported"),
        ("crew", "unavailable"),
        ("scratchpad", "unavailable"),
    ] {
        headless(&["--require-feature".as_ref(), feature.as_ref()])
            .assert()
            .failure()
            .stderr(predicates::str::contains(format!(
                "required feature `{feature}` is {state}"
            )));
    }
    assert!(
        requests.lock().expect("request capture lock").is_empty(),
        "a refused requirement must not reach the model"
    );
    assert!(!events_path.exists(), "a refused run records no trace");

    headless(&[
        "--require-feature".as_ref(),
        "scratchpad".as_ref(),
        "--scratchpad-state".as_ref(),
        seed_path.as_os_str(),
    ])
    .assert()
    .success();
    let body = requests
        .lock()
        .expect("request capture lock")
        .pop()
        .expect("the admitted run reached the model");
    assert!(advertised_tool(&body, "state_set"), "{body}");
    let scratchpad = &contract_from(&events_path)["receipt"]["features"]["scratchpad"];
    assert_eq!(scratchpad["state"], "instantiated", "{scratchpad}");
    assert_eq!(scratchpad["required"], true, "{scratchpad}");
}

/// #2314: `--scratchpad-state` is the headless opt-in. The seeded state must
/// reach the model as the literal `<state>` block with the state tools
/// advertised, and the receipt must name the fresh scope and the seed it was
/// built from — a seed of `{}` would inject no block, so it is never used here.
#[tokio::test(flavor = "multi_thread")]
async fn headless_scratchpad_state_reaches_wire_and_receipt() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let fixture = tempfile::tempdir().expect("temporary headless fixture");
    let instruction_path = fixture.path().join("instruction.md");
    let seed_path = fixture.path().join("seed.json");
    let events_path = fixture.path().join("events.jsonl");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");
    std::fs::write(&seed_path, r#"{"k": "v"}"#).expect("write scratchpad seed");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .args(["--backend-endpoint", &server.uri()])
        .args(["--backend-model", NEMOTRON_MODEL])
        .args(["--backend-kind", "openai"])
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--scratchpad-state")
        .arg(&seed_path)
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
    for tool in ["state_set", "state_get", "state_clear"] {
        assert!(advertised_tool(&body, tool), "{tool} advertised: {body}");
    }
    assert_eq!(body["messages"][0]["role"], "system", "{body}");
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .is_some_and(|c| c.contains("<state>\nk: v\n</state>")),
        "the seeded state rides message[0]: {body}"
    );

    let scratchpad = &contract_from(&events_path)["receipt"]["features"]["scratchpad"];
    assert_eq!(scratchpad["state"], "instantiated", "{scratchpad}");
    assert_eq!(scratchpad["scope"], "fresh");
    assert!(
        scratchpad["seed"]
            .as_str()
            .is_some_and(|s| s.starts_with('b')),
        "the seed is a content id: {scratchpad}"
    );
}

/// #2372: `newt headless` shows the model's final claim on stdout exactly once on
/// every wire. Chat Completions returns its answer unprinted, so headless prints
/// it; Anthropic streams it itself, so headless must not print it again. The
/// claim is what a transcript tail and false-completion forensics read.
#[tokio::test(flavor = "multi_thread")]
async fn headless_prints_the_final_answer_exactly_once_on_every_wire() {
    const CLAIM: &str = "FINAL-CLAIM-2372 every check passed";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"].as_bool().unwrap_or(false) {
                let frame = serde_json::json!({"choices": [{"delta": {"content": CLAIM}, "finish_reason": "stop"}]});
                let sse = format!("data: {frame}\n\ndata: [DONE]\n\n");
                return ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream");
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": CLAIM}, "finish_reason": "stop"}]
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(|_request: &Request| {
            let frames = [
                serde_json::json!({"type": "message_start", "message": {"model": "m", "usage": {"input_tokens": 5}}}),
                serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": CLAIM}}),
                serde_json::json!({"type": "content_block_stop", "index": 0}),
                serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}}),
                serde_json::json!({"type": "message_stop"}),
            ];
            let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream")
        })
        .mount(&server)
        .await;

    let fixture = tempfile::tempdir().expect("temporary headless fixture");
    let instruction_path = fixture.path().join("instruction.md");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");
    for kind in ["openai", "anthropic"] {
        let output = Command::cargo_bin("newt")
            .expect("newt binary")
            .env_remove("NEWT_TEAM")
            .env_remove("NEWT_ANTHROPIC_STREAM")
            .args(["--backend-endpoint", &server.uri()])
            .args(["--backend-model", "m"])
            .args(["--backend-kind", kind])
            .args(["headless", "--cwd"])
            .arg(fixture.path())
            .arg("--instruction-file")
            .arg(&instruction_path)
            .args(["--max-rounds", "1"])
            .output()
            .expect("run newt headless");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(stdout.matches(CLAIM).count(), 1, "{kind}: {stdout}");
    }

    // The answer is shown before anything that can fail: an unwritable
    // --events path (a directory) fails the run, and the claim is still there.
    let output = Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .args(["--backend-endpoint", &server.uri()])
        .args(["--backend-model", "m"])
        .args(["--backend-kind", "openai"])
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(fixture.path())
        .args(["--max-rounds", "1"])
        .output()
        .expect("run newt headless");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a directory is not an events file"
    );
    assert_eq!(
        stdout.matches(&format!("▸  {CLAIM}")).count(),
        1,
        "{stdout}"
    );
}

/// #2372: `▸` marks the model's claim. A reply the harness wrote itself — here
/// the empty-response note — is printed as a harness notice, never as a claim.
#[tokio::test(flavor = "multi_thread")]
async fn headless_prints_a_harness_written_reply_as_a_notice_not_a_claim() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"].as_bool().unwrap_or(false) {
                let frame = serde_json::json!({"choices": [{"delta": {"content": ""}, "finish_reason": "stop"}]});
                let sse = format!("data: {frame}\n\ndata: [DONE]\n\n");
                return ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream");
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": ""}, "finish_reason": "stop"}]
            }))
        })
        .mount(&server)
        .await;
    let fixture = tempfile::tempdir().expect("temporary headless fixture");
    let instruction_path = fixture.path().join("instruction.md");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");
    let output = Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .args(["--backend-endpoint", &server.uri()])
        .args(["--backend-model", "m"])
        .args(["--backend-kind", "openai"])
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .args(["--max-rounds", "1"])
        .output()
        .expect("run newt headless");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("⚠  newt: (model returned an empty response"),
        "the harness text is a notice: {stdout}"
    );
    assert!(
        !stdout.contains("▸  (model returned"),
        "never a claim: {stdout}"
    );
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

    let fixture = tempfile::tempdir().expect("temporary headless fixture");
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
    .expect("write explicit headless config");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .args(["--backend-endpoint", &server.uri()])
        .args(["--backend-model", "operator-model"])
        .args(["--backend-kind", "openai"])
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "1"])
        .assert()
        .success();

    let requests = requests.lock().expect("request capture lock");
    // #2372: one generation for the one answer, to the CLI endpoint, naming
    // the CLI-overridden model.
    assert_eq!(requests.len(), 1, "the CLI endpoint served the turn");
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
    // #2372: the answer was generated once, so it is counted once.
    assert_eq!(contract["timing"]["gen_tokens"], 7, "{contract}");
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

    let fixture = tempfile::tempdir().expect("temporary headless fixture");
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
    .expect("write explicit headless config");
    std::fs::write(&instruction_path, "Finish without calling a tool.\n")
        .expect("write headless instruction");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .args(["--cognition", "meticulous"])
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "1"])
        .assert()
        .success();

    let requests = requests.lock().expect("request capture lock");
    // #2372: one generation. It may not carry cognition controls: a dial is an
    // intent, not evidence the endpoint supports the wire fields.
    assert_eq!(requests.len(), 1);
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
// `TurnEndReason` or the headless contract today; asserting it as a substring of
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
    let raw = std::fs::read_to_string(path).expect("read headless events");
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

    let workspace = tempfile::tempdir().expect("temporary headless workspace");
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

    let config_path = workspace.path().join("headless.toml");
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
    .expect("write headless config");
    std::fs::write(
        &instruction_path,
        "Add the three produced constants, then verify them.\n",
    )
    .expect("write headless instruction");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
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
    // `write_calls` counts by NAME and ignores `ok` (headless.rs), so it would
    // still read 3 if every write had been DENIED — and then the ok-gated field
    // below would be `null`, failing for a permission reason unrelated to this
    // fix. Pin the writes as SUCCEEDING so the two cannot drift apart silently.
    let writes_ok = result["trajectory"]
        .as_array()
        .expect("trajectory is an array")
        .iter()
        .filter(|e| e["tool"] == "write_file" && e["ok"] == true)
        .count();
    assert_eq!(
        writes_ok, EARLY_WRITES,
        "the early writes must have SUCCEEDED, not merely been attempted: {result}"
    );

    // ── the typed grind measurement (#2214) ───────────────────────────────
    // RED against e3f42a36: the record has no such key, so this reads `null`.
    //
    // How many calls the run spent AFTER its last successful workspace write —
    // the thing that distinguishes write-complete-then-grind from thrash and
    // from a genuinely-too-small cap, all three of which say `RoundCap` today.
    //
    // The expected value is in CALLS, not rounds. It equals
    // `CAP_ROUNDS - EARLY_WRITES` only because this scripted model issues
    // exactly one call per round; a fixture that ever batches two calls into a
    // round must recompute it from the trajectory rather than from the round
    // counts. 5 is neither 0 nor `tool_calls` (8), so neither a constant-zero
    // implementation nor an off-by-the-whole-length one passes.
    assert_eq!(
        result["calls_after_last_write"],
        (CAP_ROUNDS - EARLY_WRITES) as u64,
        "the run spent its whole tail after the work was done; that must be a \
         value a gate can assert, not prose in the reply: {result}"
    );

    // ── the precondition ──────────────────────────────────────────────────
    // If the run did not actually end at the cap, the fixture failed to
    // reproduce the scenario and the honesty assertion below would be vacuous.
    assert_eq!(
        result["end_reason"], "Some(RoundCap)",
        "fixture must reproduce a cap exit, else the honesty assertion is vacuous: {result}"
    );

    // ── the honesty clause (Phase 27.5) ───────────────────────────────────
    // RED against current main: newt-cli/src/headless.rs derives `status` from
    // `outcome.error.is_none()` alone and never consults `end_reason`.
    // #2215 landed the typed vocabulary, so this binds the VALUE now rather
    // than only the relationship: `status_label` maps round_cap/empty/cancelled
    // to "incomplete". Asserting the exact token means a future change that
    // quietly reroutes a cap exit back to "completed" fails here even if it
    // keeps the two fields consistent with each other.
    assert_eq!(
        result["status"], "incomplete",
        "a run that ended at the tool-round cap must report itself incomplete \
         — end_reason says RoundCap on the same line: {result}"
    );

    // ── the contract clause (#2218) ───────────────────────────────────────
    // THIS ASSERTION WAS POINTING THE WRONG WAY. It pinned `"round_cap"` — the
    // exact value that broke the bench — so a regression gate was protecting
    // the regression, and anyone fixing the contract break would have been
    // failed by it.
    //
    // `gilamonster-bench/src/contract.rs:20` declares `Outcome` as a CLOSED
    // serde enum with no `serde(other)`, and `SUPPORTED_VERSIONS = ["1"]`. An
    // unknown value does not deserialize, so a cap-exit run did not score
    // badly — it DROPPED OUT of the matrix silently.
    //
    // `timeout` is the v1 bucket for "exhausted its budget without reaching the
    // goal": `is_real_attempt()` is `Completed | ModelError`, so it correctly
    // excludes the run from capability scoring. The honesty claim has not
    // moved — it lives in `status` above, on newt's own line.
    //
    // `contract_from` also pins that exactly one contract record exists, so a
    // run that emitted none cannot pass here.
    let contract = contract_from(&events_path);
    assert_eq!(
        contract["outcome"], "timeout",
        "a cap exit must file in a bucket the bench's closed enum can parse \
         AND that excludes it from capability scoring; which wall it hit is \
         `status` and `end_reason`'s to report: {contract}"
    );
}

/// Drive `newt headless` against one fixed SSE reply on every POST; returns the
/// POST count, the `solve_result` line and the contract record.
async fn headless_against_stream(stream: String) -> (usize, serde_json::Value, serde_json::Value) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().expect("temporary headless workspace");
    let config_path = workspace.path().join("headless.toml");
    let instruction_path = workspace.path().join("instruction.md");
    let events_path = workspace.path().join("events.jsonl");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "strict"

[[backends]]
name = "strict"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
"#,
            server.uri()
        ),
    )
    .expect("write headless config");
    std::fs::write(&instruction_path, "Read the seed file.\n").expect("write headless instruction");

    let _ = Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(workspace.path())
        .arg("--instruction-file")
        .arg(&instruction_path)
        .arg("--events")
        .arg(&events_path)
        .assert();

    let posts = server
        .received_requests()
        .await
        .expect("journal")
        .into_iter()
        .filter(|request| request.method.as_str() == "POST")
        .count();
    (
        posts,
        solve_result_from(&events_path),
        contract_from(&events_path),
    )
}

/// A streamed `read_file` call; `id` is the raw JSON fragment for the id member
/// (empty = the key is absent).
fn streamed_read_file_call(id: &str) -> String {
    [
        format!(
            r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,{id}"type":"function","function":{{"name":"read_file","arguments":"{{}}"}}}}]}}}}]}}"#
        ),
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
        "[DONE]".to_string(),
    ]
    .iter()
    .map(|frame| format!("data: {frame}\n\n"))
    .collect()
}

/// #2318: a 2xx OpenAI stream that strict decoding rejects (here a tool call
/// whose id is not a string, which cannot be read at all) is the model's answer,
/// so the headless contract files it `model_error`. Its class used to be lost
/// and the run filed as `harness_error`, which the bench excludes from
/// capability scoring as the harness's fault. (A MISSING id used to be the
/// example; it is now the batch validator's, re-asked — see the next test.)
#[tokio::test(flavor = "multi_thread")]
async fn a_strictly_rejected_stream_files_as_model_error() {
    let (posts, result, contract) =
        headless_against_stream(streamed_read_file_call(r#""id":7,"#)).await;
    assert_eq!(posts, 1, "exactly one POST: the rejection is not retried");
    assert_eq!(result["tool_calls"], 0, "nothing ran: {result}");
    assert_eq!(result["end_reason"], "None", "{result}");
    assert_eq!(contract["outcome"], "model_error", "{contract}");
}

/// U3: a streamed tool call with NO id reaches the shared batch validator and is
/// re-asked within its bounded budget; the third id-less batch ends the turn as
/// the model's error, with nothing dispatched.
#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_idless_call_is_re_asked_then_files_as_model_error() {
    let (posts, result, contract) = headless_against_stream(streamed_read_file_call("")).await;
    assert_eq!(posts, 3, "two re-asks, then the third id-less batch aborts");
    // The re-asked rounds leave only not-ok rejection markers: nothing ran.
    let trajectory = result["trajectory"].as_array().expect("trajectory");
    assert_eq!(trajectory.len(), 2, "one marker per re-ask: {result}");
    assert!(
        trajectory
            .iter()
            .all(|e| e["tool"] == "(rejected tool-call batch)" && e["ok"] == false),
        "nothing was dispatched: {result}"
    );
    assert_eq!(contract["outcome"], "model_error", "{contract}");
}

/// #2374: the receipt's verification mode comes from the loop the run actually
/// has. With both switches on, the Chat Completions loop runs the result-aware
/// gate, and the Responses loop without SmartHarness has no gate, so it reports
/// `off`. Reverting headless to an unconditional receipt fails the second case.
#[tokio::test(flavor = "multi_thread")]
async fn headless_reports_verification_off_where_the_loop_has_no_gate() {
    for (api, expected) in [("chat", "result_aware"), ("responses", "off")] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(CaptureThenFinish {
                requests: Arc::new(Mutex::new(Vec::new())),
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_1", "status": "completed", "model": NEMOTRON_MODEL,
                "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "done", "annotations": []}]}]
            })))
            .mount(&server)
            .await;
        let fixture = tempfile::tempdir().expect("temporary headless fixture");
        let instruction_path = fixture.path().join("instruction.md");
        let events_path = fixture.path().join("events.jsonl");
        std::fs::write(&instruction_path, "Finish without calling a tool.\n")
            .expect("write headless instruction");
        Command::cargo_bin("newt")
            .expect("newt binary")
            .env_remove("NEWT_TEAM")
            .env("NEWT_SELF_VERIFY", "1")
            .env("NEWT_VERIFY_OUTCOMES", "1")
            .args(["--backend-endpoint", &server.uri()])
            .args(["--backend-model", NEMOTRON_MODEL])
            .args(["--backend-kind", "openai"])
            .args(["--backend-api", api])
            .args(["headless", "--cwd"])
            .arg(fixture.path())
            .arg("--instruction-file")
            .arg(&instruction_path)
            .arg("--events")
            .arg(&events_path)
            .args(["--max-rounds", "1"])
            .assert()
            .success();
        let verification = &contract_from(&events_path)["receipt"]["verification"];
        assert_eq!(verification["mode"], expected, "{api}: {verification}");
    }
}

/// A plain streamed answer, with a usage chunk (32 in, 12 out) when `measured`.
fn usage_sse_reply(measured: bool) -> ResponseTemplate {
    let mut frames = vec![serde_json::json!({
        "model": NEMOTRON_MODEL,
        "choices": [{"delta": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}]
    })
    .to_string()];
    if measured {
        frames.push(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 32, "completion_tokens": 12}})
                .to_string(),
        );
    }
    frames.push("[DONE]".to_string());
    let body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// Run one `newt headless` against `server`, with `--events` into `workspace` when
/// asked, and return the process's stdout.
fn run_usage_solve(
    server: &MockServer,
    workspace: &std::path::Path,
    pricing: bool,
    events: bool,
) -> String {
    let config_path = workspace.join("headless.toml");
    let instruction_path = workspace.join("instruction.md");
    let mut config = format!(
        r#"default_backend = "usage"

[[backends]]
name = "usage"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
"#,
        server.uri()
    );
    if pricing {
        config.push_str(&format!(
            "\n[pricing.overrides.\"{NEMOTRON_MODEL}\"]\ninput_usd_per_1k = 1.0\noutput_usd_per_1k = 1.0\n"
        ));
    }
    std::fs::write(&config_path, config).expect("write headless config");
    std::fs::write(&instruction_path, "Say done.\n").expect("write headless instruction");
    let mut command = Command::cargo_bin("newt").expect("newt binary");
    command
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(workspace)
        .arg("--instruction-file")
        .arg(&instruction_path);
    if events {
        command.arg("--events").arg(workspace.join("events.jsonl"));
    }
    let output = command.output().expect("newt headless runs");
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

/// #2313 (d): the `solve_result` line carries a `usage` stanza summed per
/// inference attempt, and `--events` carries the attempt ledger's chain, which
/// verifies against the stanza's `ledger_head`.
///
/// - Measured and priced: `in_tokens`/`out_tokens` are per-attempt sums,
///   `usage_complete` is true and `cost_usd` is priced from them.
/// - Unmeasured (no `usage` in the reply): every attempt is `usage_missing`,
///   usage is incomplete, and there is no `cost_usd` — unknown is not zero.
/// - Without `--events` there are no ledger lines, so no `ledger_head`: a head
///   with no lines to walk is unverifiable.
///
/// Counts are relative to the POSTs the server received, so the display
/// reissue (#2372) changes nothing here.
#[tokio::test(flavor = "multi_thread")]
async fn headless_reports_per_attempt_usage_and_a_verifiable_attempt_ledger() {
    use newt_core::attempts::AttemptRecord;
    use newt_core::event_journal::{verify_chain, JournalLine};

    for measured in [true, false] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(usage_sse_reply(measured))
            .mount(&server)
            .await;
        let workspace = tempfile::tempdir().expect("temporary headless workspace");
        run_usage_solve(&server, workspace.path(), measured, true);

        let posts = server.received_requests().await.expect("journal").len() as u64;
        assert!(posts > 0, "measured={measured}: the run reached the model");
        let events_path = workspace.path().join("events.jsonl");
        let result = solve_result_from(&events_path);
        let usage = &result["usage"];
        assert_eq!(usage["attempts"], posts, "measured={measured}: {usage}");
        if measured {
            assert_eq!(usage["in_tokens"], 32 * posts, "{usage}");
            assert_eq!(usage["out_tokens"], 12 * posts, "{usage}");
            assert_eq!(usage["usage_missing"], 0, "{usage}");
            assert_eq!(usage["usage_complete"], true, "{usage}");
            let cost = usage["cost_usd"]
                .as_f64()
                .expect("a complete, priced run has a cost");
            assert!((cost - 0.044 * posts as f64).abs() < 1e-9, "{usage}");
        } else {
            assert_eq!(usage["in_tokens"], 0, "{usage}");
            assert_eq!(usage["usage_missing"], posts, "{usage}");
            assert_eq!(usage["usage_complete"], false, "{usage}");
            assert!(
                usage.get("cost_usd").is_none(),
                "unknown is not zero: {usage}"
            );
        }

        let lines: Vec<JournalLine<AttemptRecord>> = std::fs::read_to_string(&events_path)
            .expect("read headless events")
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("event line is JSON")
            })
            .filter(|record| record["kind"] == "attempt")
            .map(|record| {
                serde_json::from_value(record).expect("an attempt line is a journal line")
            })
            .collect();
        let head = usage["ledger_head"]
            .as_str()
            .expect("--events carries the head");
        assert_eq!(
            verify_chain(&lines, Some(head)),
            vec![],
            "measured={measured}"
        );
        let attempts: std::collections::BTreeSet<_> =
            lines.iter().map(|line| line.node.payload().id).collect();
        assert_eq!(attempts.len() as u64, posts, "one attempt id per POST");
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(usage_sse_reply(true))
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().expect("temporary headless workspace");
    let stdout = run_usage_solve(&server, workspace.path(), false, false);
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert!(
        records.iter().all(|record| record["kind"] != "attempt"),
        "no --events, no lines"
    );
    let result = records
        .iter()
        .find(|record| record["kind"] == "solve_result")
        .expect("a solve_result line on stdout");
    assert!(result["usage"]["attempts"].as_u64() > Some(0), "{result}");
    assert!(result["usage"].get("ledger_head").is_none(), "{result}");
}

#[path = "headless_cli/cognition.rs"]
mod cognition;

/// Answers the first chat request with a `write_file` tool call and every later
/// one with a 500, so the run fails part-way AFTER one real write.
struct WriteThenFail {
    sequence: AtomicUsize,
}

impl Respond for WriteThenFail {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.sequence.fetch_add(1, Ordering::SeqCst) > 0 {
            return ResponseTemplate::new(500).set_body_string("upstream exploded");
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "m",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "w0",
                        "type": "function",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"out.txt\",\"content\":\"partial\\n\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
    }
}

/// U7: a run that dies part-way must still tell the dispatcher what it left
/// behind. The harness (not the model) writes `handback` on the solve_result
/// line: the workspace delta from the git probe, the end reason, and whether the
/// model said anything at all. Grounds the mocked `handback` unit tests with a
/// real git workspace and a real `write_file`.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_fails_after_a_write_hands_back_what_it_changed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(WriteThenFail {
            sequence: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let control = tempfile::tempdir().expect("control dir outside the workspace");
    let workspace = tempfile::tempdir().expect("temporary headless workspace");
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(workspace.path())
            .output()
            .expect("git")
            .status
            .success());
    };
    git(&["init", "-q"]);
    std::fs::write(workspace.path().join("dirty-before.txt"), "x\n").expect("pre-existing dirt");
    let config_path = control.path().join("cfg.toml");
    std::fs::write(
        &config_path,
        format!(
            "default_backend = \"b\"\n\n[[backends]]\nname = \"b\"\nendpoint = \"{}\"\nmodel = \"m\"\nkind = \"openai\"\n",
            server.uri()
        ),
    )
    .expect("write config");
    let instruction = control.path().join("task.md");
    std::fs::write(&instruction, "Write out.txt.\n").expect("write instruction");
    let events_path = control.path().join("events.jsonl");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .env("NEWT_HTTP_MAX_RETRIES", "0")
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(workspace.path())
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "4"])
        .assert()
        .failure();

    let result: serde_json::Value = std::fs::read_to_string(&events_path)
        .expect("read headless events")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("event line is JSON"))
        .find(|r| r["kind"] == "solve_result")
        .expect("a solve_result line");
    assert_eq!(result["status"], "failed", "{result}");
    let handback = &result["handback"];
    assert_eq!(
        handback["files_changed"],
        serde_json::json!(["out.txt"]),
        "{result}"
    );
    assert_eq!(handback["files_changed_source"], "git_status_delta");
    assert_eq!(handback["files_changed_truncated"], false);
    assert_eq!(handback["model_reply_present"], false);
    assert!(handback["end_reason"].is_string(), "{handback}");
}

/// One `write_file`, then `list_dir` forever: after a real write every further
/// round changes nothing and verifies nothing. Records each request body.
struct WriteThenGrind {
    sequence: AtomicUsize,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Respond for WriteThenGrind {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("chat body");
        self.requests.lock().expect("capture").push(body);
        let n = self.sequence.fetch_add(1, Ordering::SeqCst);
        let (name, arguments) = if n == 0 {
            ("write_file", "{\"path\":\"out.txt\",\"content\":\"x\\n\"}")
        } else {
            ("list_dir", "{\"path\":\".\"}")
        };
        if request.url.path() == "/v1/responses" {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": format!("resp_{n}"), "status": "completed", "model": "m",
                "output": [{"type": "function_call", "id": format!("fc_{n}"),
                    "call_id": format!("c{n}"), "name": name, "arguments": arguments,
                    "status": "completed"}]
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "m",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{"id": format!("c{n}"), "type": "function",
                        "function": {"name": name, "arguments": arguments}}]
                },
                "finish_reason": "tool_calls"
            }]
        }))
    }
}

/// U4b: a successful write followed by N rounds that change and verify nothing
/// is steered at `steer_after` and ends, typed, at `stop_after` — filed like the
/// round cap (`timeout` / `incomplete`), with a harness-written notice and NO
/// model summary call. The thresholds are configuration (`[initiative.no_progress]`).
async fn assert_write_then_no_progress_stops(api: &str) {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(WriteThenGrind {
            sequence: AtomicUsize::new(0),
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(WriteThenGrind {
            sequence: AtomicUsize::new(0),
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let control = tempfile::tempdir().expect("control dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let config_path = control.path().join("cfg.toml");
    std::fs::write(
        &config_path,
        format!(
            "default_backend = \"b\"\n\n[[backends]]\nname = \"b\"\nendpoint = \"{}\"\nmodel = \"m\"\nkind = \"openai\"\n\n[initiative.no_progress]\nsteer_after = 2\nstop_after = 3\n",
            server.uri()
        ),
    )
    .expect("config");
    let instruction = control.path().join("task.md");
    std::fs::write(&instruction, "Write out.txt, then stop.\n").expect("instruction");
    let events_path = control.path().join("events.jsonl");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["--backend-api", api])
        .args(["headless", "--cwd"])
        .arg(workspace.path())
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "40"])
        .assert()
        .success();

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&events_path)
        .expect("events")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    let result = lines
        .iter()
        .find(|r| r["kind"] == "solve_result")
        .expect("solve_result");
    assert_eq!(result["end_reason"], "Some(NoProgress)", "{result}");
    assert_eq!(result["status"], "incomplete");
    assert!(
        result["reply_chars"].as_u64().unwrap() > 0,
        "harness notice: {result}"
    );
    let contract = lines
        .iter()
        .find(|r| r.get("contract_version").is_some())
        .expect("contract");
    assert_eq!(contract["outcome"], "timeout");

    let requests = requests.lock().expect("capture");
    assert_eq!(
        requests.len(),
        4,
        "write, then three grinding rounds; no summary call"
    );
    assert!(
        requests.iter().all(|r| r.get("tools").is_some()),
        "no tools-disabled summary"
    );
    // Chat bodies carry `messages`; Responses bodies carry `input`.
    let last_messages = requests[3]
        .get("messages")
        .or_else(|| requests[3].get("input"))
        .expect("messages or input")
        .to_string();
    assert!(
        last_messages.contains("rounds have passed since your last successful change"),
        "the steer must precede the final round: {last_messages}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_then_no_progress_is_steered_then_stopped_like_a_cap() {
    assert_write_then_no_progress_stops("chat").await;
}

/// Review fix 5 (RED first): the Responses loop had no brake at all, so the same
/// stall ran to the round cap on that wire.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_then_no_progress_is_stopped_on_the_responses_wire_too() {
    assert_write_then_no_progress_stops("responses").await;
}

/// A `write_file` (with an id), then only tool calls that carry NO id: each such
/// batch is re-asked (not run, not recorded as a round outcome), and a third in
/// a row would abort the run.
struct WriteThenIdless {
    sequence: AtomicUsize,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Respond for WriteThenIdless {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("chat body");
        self.requests.lock().expect("capture").push(body);
        let n = self.sequence.fetch_add(1, Ordering::SeqCst);
        let call = if n == 0 {
            serde_json::json!({"id": "c0", "type": "function", "function": {
                "name": "write_file", "arguments": "{\"path\":\"out.txt\",\"content\":\"x\\n\"}"}})
        } else {
            serde_json::json!({"type": "function", "function": {
                "name": "list_dir", "arguments": "{\"path\":\".\"}"}})
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "m",
            "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [call]},
                "finish_reason": "tool_calls"}]
        }))
    }
}

/// The interaction of the brake with #2500's id-less re-ask: a re-ask round
/// `continue`s past `record_round_outcome`, but it is a completed model round that
/// changed nothing. The gate counts it, and it does not reset the brake, so the
/// brake (`stop_after = 2`) ends the run BEFORE the third id-less batch would
/// abort it. Were re-asks uncounted, the run would fail with the correlation
/// error instead.
#[tokio::test(flavor = "multi_thread")]
async fn an_idless_reask_round_is_counted_by_the_brake_and_does_not_reset_it() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(WriteThenIdless {
            sequence: AtomicUsize::new(0),
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let control = tempfile::tempdir().expect("control dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let config_path = control.path().join("cfg.toml");
    std::fs::write(
        &config_path,
        format!(
            "default_backend = \"b\"\n\n[[backends]]\nname = \"b\"\nendpoint = \"{}\"\nmodel = \"m\"\nkind = \"openai\"\n\n[initiative.no_progress]\nsteer_after = 0\nstop_after = 2\n",
            server.uri()
        ),
    )
    .expect("config");
    let instruction = control.path().join("task.md");
    std::fs::write(&instruction, "Write out.txt, then stop.\n").expect("instruction");
    let events_path = control.path().join("events.jsonl");

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(workspace.path())
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "40"])
        .assert()
        .success();

    let result = solve_result_from(&events_path);
    assert_eq!(result["end_reason"], "Some(NoProgress)", "{result}");
    assert_eq!(result["status"], "incomplete");
    assert!(
        result["error"].is_null(),
        "not the correlation abort: {result}"
    );
    assert_eq!(
        requests.lock().expect("capture").len(),
        3,
        "the write, then two re-asked rounds; the stop precedes a third"
    );
}

/// #2524 item 1, end-to-end: a signed `~/.newt/ocap/approve.toml` `[[fs]]`
/// read grant for a path OUTSIDE the workspace is admitted into a real
/// `newt headless` run's caveats and is visible on the contract record's
/// `effective_config.durable_grants` (the observability requirement).
/// Isolated via `common::newt()` (a throwaway `$HOME`); the signing key and
/// store live ONLY under that throwaway `.newt/`, never the real one.
#[tokio::test(flavor = "multi_thread")]
async fn a_signed_ocap_fs_read_grant_is_admitted_into_headless_caveats_and_the_contract() {
    const CLAIM: &str = "FINAL-CLAIM-2524-ocap-grant";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": CLAIM}, "finish_reason": "stop"}]
        })))
        .mount(&server)
        .await;

    let mut cmd = common::newt();
    let home = cmd.home().to_path_buf();
    common::isolate_loopback_chat(&mut *cmd, &home);
    let config_dir = cmd.config_dir();

    // The disposable ROOT identity + a signed approve entry for a path
    // outside the workspace — never the real `~/.newt/identity.pem`.
    let key = newt_identity::UserKey::generate();
    let identity_path = config_dir.join("identity.pem");
    key.save(&identity_path).expect("save disposable root key");
    let ocap_dir = config_dir.join("ocap");
    std::fs::create_dir_all(&ocap_dir).expect("ocap dir");
    let granted_path = home.join("outside-canvas-token");
    std::fs::write(&granted_path, "token\n").expect("write outside file");
    let mut file = newt_core::ocap_store::PolicyFile::parse(&format!(
        // A TOML literal string: a Windows path's `\U…` is not an escape.
        "[[fs]]\npath = '{}'\n",
        granted_path.display()
    ))
    .expect("parse approve.toml");
    let (signed, refused) = newt_core::ocap_store::sign_approves(
        &mut file,
        |_, _| false,
        |payload| key.sign(payload).to_bytes(),
    );
    assert_eq!(signed, 1);
    assert!(refused.is_empty());
    std::fs::write(
        ocap_dir.join("approve.toml"),
        file.to_toml().expect("serialize approve.toml"),
    )
    .expect("write approve.toml");

    let workspace = home.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "default_backend = \"b\"\n\n[[backends]]\nname = \"b\"\nendpoint = \"{}\"\nmodel = \"m\"\nkind = \"openai\"\n",
            server.uri()
        ),
    )
    .expect("write config");
    let instruction = home.join("task.md");
    std::fs::write(&instruction, "Finish without calling a tool.\n").expect("instruction");
    let events_path = home.join("events.jsonl");

    // The default `--confined` lane (no `--smart-harness`, which needs an
    // auxiliary model this test has no business provisioning): `fs_read`
    // starts `Scope::All` there, so this proves the OBSERVABILITY half (the
    // admitted grant is on the contract record) with a real binary; the
    // fenced-widening half is proved by the mocked unit test
    // `fold_ocap_grants_widens_a_fenced_axis_but_never_touches_an_open_one`
    // (`newt-cli/src/headless.rs`), which exercises the same fenced axis the
    // smart lane narrows to, without needing a live auxiliary.
    cmd.arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(&workspace)
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "1"])
        .assert()
        .success();

    let contract = contract_from(&events_path);
    let durable_grants = contract["effective_config"]["durable_grants"]
        .as_array()
        .expect("durable_grants stanza present when a grant was admitted");
    // #2532 review, item 3: this grant folds into fs_read's already-`All`
    // confined-lane axis, so it changed nothing — the contract labels it a
    // no-op rather than implying the signature is why the read worked.
    assert_eq!(
        durable_grants,
        &vec![serde_json::json!(format!(
            "fs_read:{} (no-op: already permitted)",
            granted_path.display()
        ))],
        "{contract}"
    );
}

/// #2532 review, should-fix 1, RED FIRST: a headless run with NO existing
/// `identity.pem` in its config dir must leave none behind — reading the OCAP
/// store must never mint the operator's root key as a side effect. Before the
/// fix, `resolve_ocap_store` called `newt_identity::load_or_generate`, which
/// writes a fresh key on first read; a headless run on a bare host/CI/scratch
/// `--config-dir` would silently root every later `sign-ocap` on that host in
/// a key nobody chose.
#[tokio::test(flavor = "multi_thread")]
async fn headless_reading_the_ocap_store_never_mints_an_identity_key() {
    const CLAIM: &str = "FINAL-CLAIM-2532-no-mint";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": CLAIM}, "finish_reason": "stop"}]
        })))
        .mount(&server)
        .await;

    let mut cmd = common::newt();
    let home = cmd.home().to_path_buf();
    common::isolate_loopback_chat(&mut *cmd, &home);
    let config_dir = cmd.config_dir();
    let identity_path = config_dir.join("identity.pem");
    assert!(!identity_path.exists(), "no key before the run");

    let workspace = home.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config_path = home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "default_backend = \"b\"\n\n[[backends]]\nname = \"b\"\nendpoint = \"{}\"\nmodel = \"m\"\nkind = \"openai\"\n",
            server.uri()
        ),
    )
    .expect("write config");
    let instruction = home.join("task.md");
    std::fs::write(&instruction, "Finish without calling a tool.\n").expect("instruction");
    let events_path = home.join("events.jsonl");

    cmd.arg("--config")
        .arg(&config_path)
        .args(["headless", "--cwd"])
        .arg(&workspace)
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events_path)
        .args(["--max-rounds", "1"])
        .assert()
        .success();

    assert!(
        !identity_path.exists(),
        "reading the OCAP store must never mint identity.pem"
    );
    let contract = contract_from(&events_path);
    assert!(
        contract["effective_config"]["durable_grants"].is_null(),
        "no key on disk means no approves, not a freshly-minted empty store: {contract}"
    );
}
