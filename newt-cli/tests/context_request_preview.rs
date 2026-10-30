//! #2959 / PR #2985 r1: `/context request` previews what the NEXT ordinary
//! request sends. Each case drives the production binary through the real
//! slash handler and a real following turn against a recording mock, then
//! compares the previewed tool names with the tool names that turn sent.
//!
//! Grounds the mocked `tool_set` parity tests in `newt-core`: those feed both
//! sides the same disposition by hand, so only a real session can catch the
//! command resolving a different mode, disposition or allow list than the
//! ordinary turn does.
#![cfg(target_os = "linux")]

mod common;

use serde_json::{json, Value};
use std::os::unix::fs::DirBuilderExt as _;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TASK: &str = "Create the task checkout and implement the approval guard.";

struct Session {
    transcript: String,
    sent_tools: Vec<String>,
    sent_system: Vec<String>,
    requests_before_task: usize,
}

fn tool_names(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    t["function"]["name"]
                        .as_str()
                        .or_else(|| t["name"].as_str())
                })
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The previewed table rows between `## Tools (` and `Total`.
fn previewed_tools(transcript: &str) -> Vec<String> {
    let mut names: Vec<String> = transcript
        .lines()
        .map(|l| l.trim_start_matches('>').trim())
        .skip_while(|l| !l.contains("## Tools ("))
        .skip(4)
        .take_while(|l| l.starts_with("| "))
        .filter_map(|l| l.trim_start_matches("| ").split(" |").next())
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

async fn session(extra_config: &str, script: &str) -> Session {
    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&config_dir)
        .unwrap();
    let config = config_dir.join("config.toml");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":"preview-fixture","context_length":131072}]})),
        )
        .mount(&server)
        .await;
    let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let capture = Arc::clone(&bodies);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            capture
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&req.body).unwrap());
            ResponseTemplate::new(200).set_body_json(json!({
                "model":"preview-fixture",
                "choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":32,"completion_tokens":8}
            }))
        })
        .mount(&server)
        .await;
    std::fs::write(
        &config,
        format!(
            r#"default_backend = "fixture"
[[backends]]
name = "fixture"
endpoint = "{}"
model = "preview-fixture"
kind = "openai"
api = "chat_completions"
[tui]
max_tool_rounds = 2
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
scheduled = true
{extra_config}
"#,
            server.uri()
        ),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate(&mut cmd, root.path());
    cmd.arg("--config-dir")
        .arg(&config_dir)
        .arg("--config")
        .arg(&config)
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
    // #2972: a new session has no controlling terminal, so the transcript is
    // never re-wrapped to a narrow PTY width.
    // SAFETY: `setsid` is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let mut child = cmd.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(format!("{script}\n{TASK}\n/quit\n").as_bytes())
        .await
        .unwrap();
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output())
        .await
        .expect("session watchdog; child is killed on drop")
        .unwrap();
    let transcript = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{transcript}");
    let bodies = bodies.lock().unwrap();
    let task_request = bodies
        .iter()
        .find(|b| {
            b["messages"].as_array().is_some_and(|m| {
                m.iter().any(|m| {
                    m["role"] == "user" && m["content"].as_str().is_some_and(|s| s.contains(TASK))
                })
            })
        })
        .unwrap_or_else(|| panic!("the task request was never sent: {transcript}"));
    Session {
        sent_tools: tool_names(task_request),
        sent_system: task_request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str().map(str::to_owned))
            .collect(),
        requests_before_task: bodies
            .iter()
            .position(|b| b == task_request)
            .expect("task request is in the list"),
        transcript,
    }
}

fn assert_parity(s: &Session) {
    assert!(
        s.transcript
            .contains("next request (preview; nothing sent)"),
        "{}",
        s.transcript
    );
    let previewed = previewed_tools(&s.transcript);
    assert!(!s.sent_tools.is_empty(), "{}", s.transcript);
    assert_eq!(
        previewed, s.sent_tools,
        "previewed tools differ from the ordinary request's\n{}",
        s.transcript
    );
    // The preview never sends: only the task turn reaches the server.
    assert_eq!(s.requests_before_task, 0, "{}", s.transcript);
}

/// Plan mode narrows an action-shaped turn to the read-only catalog; the
/// preview must show that narrowed set, not the Act set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_preview_matches_the_next_request() {
    let s = session("", "/mode plan\n/context request").await;
    assert_parity(&s);
    assert!(
        !s.sent_tools.contains(&"write_file".to_string()),
        "{}",
        s.transcript
    );
}

/// Plan mode plus an allow list: both ceilings apply to the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_with_an_allow_list_preview_matches_the_next_request() {
    let s = session(
        "[tool_exposure]\nallow_list = [\"read_file\", \"write_file\"]",
        "/mode plan\n/context request",
    )
    .await;
    assert_parity(&s);
    assert_eq!(
        s.sent_tools,
        vec!["read_file".to_string()],
        "{}",
        s.transcript
    );
}

/// Read-only exposure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_preview_matches_the_next_request() {
    let s = session("[tool_exposure]\nread_only = true", "/context request").await;
    assert_parity(&s);
}

/// Auto: the preview names the working style the next action turn gets
/// (`configured="auto" effective="dev"`), and a second preview still shows it
/// (the pending selection is peeked, never consumed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_mode_preview_names_the_effective_style_the_next_request_uses() {
    let s = session("", "/mode auto\n/context request\n/context request").await;
    assert_parity(&s);
    let effective = |text: &str| {
        text.lines()
            .find(|l| l.contains("configured=\"auto\""))
            .map(str::to_owned)
    };
    let sent = s
        .sent_system
        .iter()
        .find_map(|t| effective(t))
        .unwrap_or_else(|| panic!("no auto line in the sent system: {}", s.transcript));
    assert!(
        s.transcript.contains(sent.trim()),
        "the preview must carry the same mode line as the request: {sent}\n{}",
        s.transcript
    );
}

/// The protected active-prompt card rides the real request after the leading
/// system messages, so the preview must list it as a labelled component, and
/// must not call anything "as sent".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_includes_the_active_prompt_card_and_is_labelled_partial() {
    let s = session("", "/context request").await;
    assert!(
        s.sent_system
            .iter()
            .any(|t| t.contains("[NEWT ACTIVE PROMPT v1]")),
        "{}",
        s.transcript
    );
    assert!(s.transcript.contains("| card |"), "{}", s.transcript);
    assert!(
        s.transcript.contains("(preview, partial)"),
        "{}",
        s.transcript
    );
    assert!(s.transcript.contains("Total (partial)"), "{}", s.transcript);
    assert!(!s.transcript.contains("as sent"), "{}", s.transcript);
}
