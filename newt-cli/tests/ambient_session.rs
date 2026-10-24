//! #978: `--ambient` end to end. A real `newt` chat loop, a recording provider that asks
//! for one `run_command`, and what the command printed as the witness:
//!
//! - under `--ambient` the command ran in the operator's login shell (`$SHELL`), saw the
//!   operator's `DISPLAY`, and the interactive-shell noise did not reach the model;
//! - under `--yolo --full-access` (the pair `--ambient` implies) the same command still
//!   runs `bash -c`: nothing changes outside the new flag;
//! - the session banner says plainly what `--ambient` is.
//!
//! Run: `cargo test -p newt-agent --test ambient_session`.

#![cfg(unix)]

mod common;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "ambient-fixture";
const TASK: &str = "Show your shell.";
/// `$0` is how the shell was invoked: `bash` for the bypass lane's `bash -c`, the full
/// `$SHELL` path for the login shell. `DISPLAY` is the operator's.
const PROBE: &str = r#"printf '[%s][%s]' "$0" "$DISPLAY""#;

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

/// One session with `flags`; returns (the run_command result the model saw, the chat's
/// stdout).
async fn session(flags: &[&str]) -> (String, String) {
    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    std::fs::create_dir_all(&config_dir).expect("isolated config dir");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": MODEL, "context_length": 131072}]
        })))
        .mount(&server)
        .await;
    let result = Arc::new(Mutex::new(String::new()));
    let capture = Arc::clone(&result);
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
            let results: Vec<&str> = messages[task + 1..]
                .iter()
                .filter(|m| m["role"] == "tool")
                .filter_map(|m| m["content"].as_str())
                .collect();
            if results.is_empty() {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "model": MODEL,
                    "choices": [{"message": {"role": "assistant", "content": null,
                        "tool_calls": [{"id": "probe", "type": "function", "function": {
                            "name": "run_command",
                            "arguments": json!({"command": PROBE}).to_string()}}]},
                        "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 32, "completion_tokens": 8}
                }));
            }
            *capture.lock().expect("capture lock") = results.join("\n");
            answer(streaming, "Recorded.")
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
"#,
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
        .args(flags)
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
        // The operator's login shell and display, as the real environment would have them.
        .env("SHELL", "/bin/bash")
        .env("DISPLAY", ":77")
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
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let seen = result.lock().expect("capture lock").clone();
    (seen, shown)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_runs_the_login_shell_with_the_operators_display() {
    let (result, shown) = session(&["--ambient"]).await;
    assert!(
        result.contains("[/bin/bash][:77]"),
        "{result}\n--- session ---\n{shown}"
    );
    assert!(
        !result.contains("no job control") && !result.contains("terminal process group"),
        "interactive-shell noise leaked to the model: {result}"
    );
    for needle in [
        "AMBIENT",
        "login shell /bin/bash -l -i -c",
        "FULL environment",
        "no confinement",
    ] {
        assert!(shown.contains(needle), "banner lacks `{needle}`:\n{shown}");
    }
}

/// `--yolo --full-access` is what `--ambient` implies, and it is unchanged: `bash -c`,
/// no login shell, no ambient banner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_yolo_full_access_pair_still_runs_bash_dash_c() {
    let (result, shown) = session(&["--yolo", "--full-access"]).await;
    assert!(
        result.contains("[bash][:77]"),
        "{result}\n--- session ---\n{shown}"
    );
    assert!(!shown.contains("AMBIENT (--ambient)"), "{shown}");
}
