//! #978: `--ambient` end to end. A real `newt` chat loop, a recording provider that asks
//! for one `run_command`, and what the command printed as the witness:
//!
//! - under `--yolo` (= `--disable-ocap --full-access --ambient`) the command ran in the
//!   operator's login shell (`$SHELL`), saw the operator's `DISPLAY`, and the
//!   interactive-shell noise did not reach the model; the banner lists what is off;
//! - the granular flags keep their narrow meaning: `--disable-ocap --full-access` still
//!   runs `bash -c` (no ambient banner), `--ambient` alone claims only its own axis, and
//!   `--disable-ocap --ambient` composes the host shell with the login shell.
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
async fn yolo_runs_the_login_shell_with_the_operators_display_and_lists_what_is_off() {
    let (result, shown) = session(&["--yolo"]).await;
    assert!(
        result.contains("[/bin/bash][:77]"),
        "{result}\n--- session ---\n{shown}"
    );
    assert!(
        !result.contains("no job control") && !result.contains("terminal process group"),
        "interactive-shell noise leaked to the model: {result}"
    );
    for needle in [
        "YOLO (--yolo)",
        "approvals OFF (--disable-ocap)",
        "confinement OFF (--full-access)",
        "environment OFF (--ambient)",
        "login shell /bin/bash -l -i -c",
        "FULL environment",
    ] {
        assert!(shown.contains(needle), "banner lacks `{needle}`:\n{shown}");
    }
}

/// `--disable-ocap --full-access` is two of the three, and unchanged: `bash -c`, no
/// ambient banner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_granular_pair_still_runs_bash_dash_c() {
    let (result, shown) = session(&["--disable-ocap", "--full-access"]).await;
    assert!(
        result.contains("[bash][:77]"),
        "{result}\n--- session ---\n{shown}"
    );
    assert!(!shown.contains("(--ambient)"), "{shown}");
}

/// `--ambient` alone is its own axis: the banner names the environment and does not
/// claim approvals or confinement are off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_alone_claims_only_the_environment() {
    let (result, shown) = session(&["--ambient"]).await;
    // Confinement is untouched: run_command still ran in the confined shell, which has
    // neither the login shell nor the operator's display.
    assert!(
        !result.contains("/bin/bash") && !result.contains(":77"),
        "{result}"
    );
    assert!(
        shown.contains("AMBIENT (--ambient): environment OFF"),
        "{shown}"
    );
    assert!(
        !shown.contains("approvals OFF") && !shown.contains("confinement OFF"),
        "{shown}"
    );
    assert!(
        !shown.contains("ocap DISABLED") && !shown.contains("FULL ACCESS"),
        "{shown}"
    );
}

/// The three flags spelled out are `--yolo`: same login shell, same display.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_three_granular_flags_together_are_yolo() {
    let (result, shown) = session(&["--disable-ocap", "--full-access", "--ambient"]).await;
    assert!(
        result.contains("[/bin/bash][:77]"),
        "{result}\n--- session ---\n{shown}"
    );
    assert!(shown.contains("YOLO (--yolo)"), "{shown}");
}
