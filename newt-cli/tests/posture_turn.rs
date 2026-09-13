//! Real chat-loop grounding for the mocked posture command/settings tests.

mod common;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "posture-fixture";
const FRAMING: &str = "POSTURE_FIXTURE_LOCKED_AUTHORITY";
const LOCKED_TASK: &str = "Create posture-canary.txt containing LOCKED_ATTEMPT.";
const RELEASED_TASK: &str = "Create posture-canary.txt containing RELEASED_ATTEMPT.";

fn final_response(streaming: bool) -> ResponseTemplate {
    if streaming {
        let frame = json!({"choices": [{"delta": {"content": "The tool result is recorded."}, "finish_reason": "stop"}]});
        ResponseTemplate::new(200).set_body_raw(
            format!("data: {frame}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "model": MODEL,
            "choices": [{
                "message": {"role": "assistant", "content": "The tool result is recorded."},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 32, "completion_tokens": 8}
        }))
    }
}

/// Grounds the command/settings state tests in the actual `run_chat` accepted
/// turn: settings must affect BOTH the next system prompt and real filesystem
/// dispatch, and `/posture off` must release only that clamp. Piped stdin uses
/// the real lean surface and slash dispatcher without PTY rendering timing.
///
/// Action nudges are explicitly off to isolate posture enforcement from the
/// separate authority-aware steering repair. This is not combined live-model
/// acceptance. No real inference, model provisioning, permission prompts, or
/// command authority is used. The model-requested writes target only owned
/// disposable canaries: one inside the workspace and one outside, which must
/// remain denied after the posture clamp is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_posture_reaches_accepted_turn_and_off_releases_the_clamp() {
    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    std::fs::create_dir_all(&config_dir).expect("isolated config dir");
    let canary = root.path().join("posture-canary.txt");
    let outside = tempfile::tempdir().expect("owned out-of-workspace fixture");
    let outside_canary = outside.path().join("outside-canary.txt");
    assert!(!outside_canary.starts_with(root.path()));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": MODEL, "context_length": 131072}]
        })))
        .mount(&server)
        .await;

    // Capture the actual request and real canary state at its arrival, rather
    // than infer an earlier denial from the final file (the second turn writes
    // the same path). Ignore startup probes; assertions below require both
    // operator tasks and their exact tool-call results to have reached the wire.
    let observed = Arc::new(Mutex::new(Vec::<(bool, Value, bool, bool, bool)>::new()));
    let pending_replay = Mutex::new(None::<Vec<u8>>);
    let capture = Arc::clone(&observed);
    let inspected_canary = canary.clone();
    let inspected_outside = outside_canary.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("chat request JSON");
            let streaming = body["stream"].as_bool().unwrap_or(false);
            // Only a matching terminal answer arms one optional display copy.
            // A different request consumes that eligibility as a new round.
            let replaying = pending_replay
                .lock()
                .expect("replay lock")
                .take()
                .is_some_and(|prior| streaming && prior == request.body);
            let messages = body["messages"].as_array().expect("chat messages");
            let active_task = messages.iter().rposition(|message| {
                message["role"] == "user"
                    && message["content"].as_str().is_some_and(|text| {
                        text.contains(LOCKED_TASK) || text.contains(RELEASED_TASK)
                    })
            });
            let Some(task_index) = active_task else {
                return final_response(streaming);
            };
            let released = messages[task_index]["content"]
                .as_str()
                .unwrap()
                .contains(RELEASED_TASK);
            let call_id = if released {
                "released_write"
            } else {
                "locked_write"
            };
            let has_result = messages[task_index + 1..]
                .iter()
                .any(|message| message["role"] == "tool" && message["tool_call_id"] == call_id);
            capture.lock().expect("capture lock").push((
                released,
                body.clone(),
                inspected_canary.exists(),
                inspected_outside.exists(),
                replaying,
            ));
            if has_result {
                if !replaying {
                    *pending_replay.lock().expect("replay lock") = Some(request.body.clone());
                }
                // Preserve the primary's original JSON usage and the optional
                // display's SSE content; both requests now use stream:true.
                return final_response(replaying && streaming);
            }
            let mut calls = vec![json!({
                "id": call_id, "type": "function",
                "function": {
                    "name": "write_file",
                    "arguments": json!({
                        "path": "posture-canary.txt",
                        "content": if released { "RELEASED_ATTEMPT\n" } else { "LOCKED_ATTEMPT\n" }
                    }).to_string()
                }
            })];
            if released {
                // Releasing the posture must not widen the original workspace
                // grant. This second tool call is a negative authority control.
                calls.push(json!({
                    "id": "outside_write", "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": json!({
                            "path": inspected_outside.to_string_lossy(),
                            "content": "OUTSIDE_ATTEMPT\n"
                        }).to_string()
                    }
                }));
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "model": MODEL,
                "choices": [{
                    "message": {"role": "assistant", "content": null, "tool_calls": calls},
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 32, "completion_tokens": 8}
            }))
        })
        .mount(&server)
        .await;

    let config_path = config_dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "fixture"
[[backends]]
name = "fixture"
endpoint = "{}"
model = "{MODEL}"
kind = "openai"
api = "chat_completions"
[tui]
max_tool_rounds = 4
workflow_grace_rounds = 0
[tui.permissions]
preset = "workspace_edit"
net = []
prompt = false
[permission_presets.locked]
readonly = true
exec_allow = []
deny = ["*"]
[modes.locked]
preset = "locked"
framing = "{FRAMING}"
[memory]
provider = "rolling_window"
context_tokens = 131072
note_nudge_interval = 0
[context]
manager = "append-only"
[context.features]
semantic = false
experiential = false
scheduled = false
"#,
            server.uri()
        ),
    )
    .expect("isolated explicit config");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate(&mut cmd, root.path());
    // This test drives authority, so also remove the entire NEWT_* family:
    // inherited grant, bypass, model, and UI overrides must not change its run.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("NEWT_") {
            cmd.env_remove(key);
        }
    }
    cmd.arg("--config-dir")
        .arg(&config_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "--no-splash",
            "--plain",
            "--ephemeral",
            "--no-agents-file",
            "--no-prompt-for-permissions",
        ])
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("NEWT_NO_MODEL_PULL", "1")
        .env("NEWT_NUDGE", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn exact Cargo-built newt");
    let mut input = child.stdin.take().expect("piped stdin");
    input
        .write_all(
            format!(
                "/settings posture locked\n{LOCKED_TASK}\n/posture off\n{RELEASED_TASK}\n/quit\n"
            )
            .as_bytes(),
        )
        .await
        .expect("feed slash commands and accepted turns");
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("posture chat watchdog expired; child is killed on drop")
        .expect("collect posture chat output");
    assert!(
        output.status.success(),
        "chat failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let captured = observed.lock().expect("capture lock");
    for released in [false, true] {
        let requests: Vec<_> = captured
            .iter()
            .filter(|(phase, _, _, _, replay)| *phase == released && !*replay)
            .collect();
        assert_eq!(
            requests.len(),
            2,
            "one tool-call round and one result round per turn"
        );
        let (_, first, existed_before_call, _, _) = requests[0];
        assert!(
            !existed_before_call,
            "canary must remain absent through the locked turn and before release"
        );
        // Only CURRENT system-role content counts, never prior user/history
        // text which may legitimately mention an earlier posture.
        let system = first["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system")
            .map(|message| message["content"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(system.contains(FRAMING), !released, "current turn framing");
        let call_id = if released {
            "released_write"
        } else {
            "locked_write"
        };
        let result = requests[1].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
            .expect("actual dispatched tool result")["content"]
            .as_str()
            .expect("text tool result");
        assert_eq!(result.contains("capability denied: fs_write"), !released);
        assert_eq!(
            requests[1].2, released,
            "real write outcome at result arrival"
        );
        if released {
            let outside_result = requests[1].1["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| {
                    message["role"] == "tool" && message["tool_call_id"] == "outside_write"
                })
                .expect("actual out-of-workspace dispatch result")["content"]
                .as_str()
                .expect("text tool result");
            assert!(
                outside_result.contains("capability denied: fs_write"),
                "{outside_result}"
            );
        }
    }
    assert!(captured.iter().all(|(_, _, _, exists, _)| !exists));
    assert!(
        !outside_canary.exists(),
        "off must preserve the base write fence"
    );
    assert_eq!(
        std::fs::read_to_string(&canary).expect("released scoped write"),
        "RELEASED_ATTEMPT\n"
    );
}
