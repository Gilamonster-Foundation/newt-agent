//! #2933 PR5: switching the fence components off stops TELLING the model about the
//! fence and nothing else. A write outside the workspace is still refused, and a
//! write inside it is still allowed, with `fs-confinement` and `runtime-authority`
//! off exactly as with them on.
//!
//! Real `newt` chat loop on piped stdin, a recording provider that asks for one
//! write inside and one outside the workspace, and the files on disk as the witness.
//!
//! Run: `cargo test -p newt-agent --test prompt_component_fence`.

mod common;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "fence-fixture";
const TASK: &str = "Write the two canaries.";

/// What one session showed: the turn's system text, the tool results the model got
/// back, and whether each canary exists afterwards.
struct Run {
    system: String,
    tool_results: Vec<String>,
    inside: bool,
    outside: bool,
}

fn answer(streaming: bool, content: &str) -> ResponseTemplate {
    if streaming {
        let frame = json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]});
        ResponseTemplate::new(200).set_body_raw(
            format!("data: {frame}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "model": MODEL,
            "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 32, "completion_tokens": 8}
        }))
    }
}

async fn session(prompt_tail: &str) -> (std::process::Output, Run) {
    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    std::fs::create_dir_all(&config_dir).expect("isolated config dir");
    let inside = root.path().join("inside-canary.txt");
    let outside_dir = tempfile::tempdir().expect("owned out-of-workspace fixture");
    let outside = outside_dir.path().join("outside-canary.txt");
    assert!(!outside.starts_with(root.path()));

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": MODEL, "context_length": 131072}]
        })))
        .mount(&server)
        .await;

    let seen = Arc::new(Mutex::new((String::new(), Vec::<String>::new())));
    let capture = Arc::clone(&seen);
    let (inside_arg, outside_arg) = (inside.clone(), outside.clone());
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("chat JSON");
            let streaming = body["stream"].as_bool().unwrap_or(false);
            let messages = body["messages"].as_array().expect("messages");
            let Some(task) = messages
                .iter()
                .rposition(|m| m["role"] == "user" && m["content"].as_str() == Some(TASK))
            else {
                return answer(streaming, "ok");
            };
            let results: Vec<String> = messages[task + 1..]
                .iter()
                .filter(|m| m["role"] == "tool")
                .filter_map(|m| m["content"].as_str().map(str::to_string))
                .collect();
            let mut seen = capture.lock().expect("capture lock");
            if results.is_empty() {
                seen.0 = messages
                    .iter()
                    .filter(|m| m["role"] == "system")
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let call = |id: &str, file: &std::path::Path| {
                    json!({"id": id, "type": "function", "function": {
                        "name": "write_file",
                        "arguments": json!({"path": file.to_string_lossy(), "content": "CANARY\n"}).to_string()
                    }})
                };
                return ResponseTemplate::new(200).set_body_json(json!({
                    "model": MODEL,
                    "choices": [{"message": {"role": "assistant", "content": null,
                        "tool_calls": [call("inside", &inside_arg), call("outside", &outside_arg)]},
                        "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 32, "completion_tokens": 8}
                }));
            }
            seen.1 = results;
            answer(streaming, "The tool results are recorded.")
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
{prompt_tail}"#,
            server.uri()
        ),
    )
    .expect("isolated config");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate(&mut cmd, root.path());
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
    let mut child = cmd.spawn().expect("spawn the real CLI");
    let mut input = child.stdin.take().expect("piped stdin");
    input
        .write_all(format!("{TASK}\n/quit\n").as_bytes())
        .await
        .expect("feed the task");
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("the chat exits")
        .expect("output");
    let (system, tool_results) = seen.lock().expect("capture lock").clone();
    let run = Run {
        system,
        tool_results,
        inside: inside.exists(),
        outside: outside.exists(),
    };
    (output, run)
}

async fn run(prompt_tail: &str) -> Run {
    let (output, run) = session(prompt_tail).await;
    assert!(
        output.status.success(),
        "chat failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    run
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fence_holds_with_its_description_switched_off() {
    let told = run("").await;
    let silent =
        run("[prompt]\ncomponents = { off = [\"fs-confinement\", \"runtime-authority\"] }\n").await;

    // The description is what changed...
    assert!(
        told.system.contains("# Filesystem confinement"),
        "{}",
        told.system
    );
    assert!(
        !silent.system.contains("# Filesystem confinement"),
        "the model is no longer told: {}",
        silent.system
    );
    // ...and enforcement did not: same refusal outside, same write inside.
    for (name, r) in [("told", &told), ("silent", &silent)] {
        assert!(
            !r.outside,
            "{name}: the write outside the fence must not happen"
        );
        assert!(
            r.inside,
            "{name}: the write inside the workspace still happens"
        );
        assert!(
            r.tool_results
                .iter()
                .any(|t| t.contains("capability denied")),
            "{name}: the outside write is refused: {:?}",
            r.tool_results
        );
    }
}

/// `card` off sends no active-prompt card, and only that: the rest of the turn is
/// untouched. Valid because the allow list hides prompt_read and artifact_read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_off_sends_no_active_prompt_card_when_its_tools_are_hidden() {
    let hide = "[tool_exposure]\nallow_list = [\"write_file\"]\n";
    let with_card = run(&format!(
        "{hide}[prompt]\ncomponents = {{ on = [\"card\"] }}\n"
    ))
    .await;
    let without = run(&format!(
        "{hide}[prompt]\ncomponents = {{ off = [\"card\"] }}\n"
    ))
    .await;
    assert!(
        with_card.system.contains("[NEWT ACTIVE PROMPT v1]"),
        "{}",
        with_card.system
    );
    assert!(
        !without.system.contains("[NEWT ACTIVE PROMPT v1]"),
        "{}",
        without.system
    );
    // The fence is untouched by it, too.
    assert!(without.inside && !without.outside);
}

/// With the card's tools still visible, `card` off refuses the session: no
/// implicit coupling, and the message names the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_off_with_its_tools_visible_refuses_the_session() {
    let (output, run) = session("[prompt]\ncomponents = { off = [\"card\"] }\n").await;
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{shown}");
    assert!(shown.contains("/settings tools"), "{shown}");
    assert!(
        run.system.is_empty(),
        "no inference request was made: {}",
        run.system
    );
}
