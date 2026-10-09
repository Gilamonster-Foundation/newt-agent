//! Production approval continuation preserves adoption, grants and ceilings.
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

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
}

fn reply(calls: Vec<Value>) -> ResponseTemplate {
    let message = if calls.is_empty() {
        json!({"role":"assistant","content":"The tool results are recorded."})
    } else {
        json!({"role":"assistant","content":null,"tool_calls":calls})
    };
    ResponseTemplate::new(200).set_body_json(json!({
        "model":"approval-fixture", "choices":[{"message":message,"finish_reason":"stop"}],
        "usage":{"prompt_tokens":32,"completion_tokens":8}
    }))
}

/// #2829: run the real session, PlanModeState, run_plan_approval and queued
/// next-turn authority reconstruction. `/mode full-auto` is the operator's
/// standing approval; no test-local phase setter or reconstructed ChatCtx is
/// involved. Signed approvals are recalled separately from launch authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_plan_approval_preserves_adoption_and_recalled_grants() {
    assert!(newt_core::confined_exec::kernel_fs_fence_available());
    let root = common::isolated_root();
    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let config_dir = root.path().join(".newt");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    let task_home = tempfile::tempdir().unwrap();
    let task = task_home.path().join("task");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&task)
        .unwrap();
    std::fs::write(root.path().join("original.txt"), "original").unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "original.txt"],
        vec![
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=",
            "commit",
            "-qm",
            "seed",
        ],
    ] {
        let mut git = Command::new("git");
        common::isolate(&mut git, root.path());
        let output = git.args(args).output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let key = newt_identity::load_or_generate(&config_dir.join("identity.pem")).unwrap();
    for entry in [
        newt_core::ocap_store::ApproveEntry::Exec {
            target: "git".into(),
        },
        newt_core::ocap_store::ApproveEntry::Exec {
            target: "pwd".into(),
        },
        newt_core::ocap_store::ApproveEntry::Fs {
            path: task.to_string_lossy().into_owned(),
            write: true,
        },
        newt_core::ocap_store::ApproveEntry::Fs {
            path: granted.path().to_string_lossy().into_owned(),
            write: true,
        },
    ] {
        newt_core::ocap_store::persist_approve(
            &config,
            entry,
            |_, _| false,
            |bytes| key.sign(bytes).to_bytes(),
        )
        .unwrap();
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":"approval-fixture","context_length":131072}]})),
        )
        .mount(&server)
        .await;
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let capture = Arc::clone(&captured);
    let grant_path = granted.path().to_path_buf();
    let original = root.path().join("original.txt");
    let outside_path = outside.path().join("forbidden");
    let task_path = task.clone();
    let sent = Mutex::new((0, false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let messages = body["messages"].as_array().unwrap();
            if !messages.iter().any(|m| {
                m["role"] == "user" && m["content"].as_str().is_some_and(|s| s.contains(TASK))
            }) {
                return reply(vec![]);
            }
            capture.lock().unwrap().push(body.clone());
            let mut sent = sent.lock().unwrap();
            let approved = messages.iter().any(|m| {
                m["role"] == "user"
                    && m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains(newt_core::agentic::PLAN_APPROVAL_PREFIX))
            });
            if approved && !sent.1 {
                sent.1 = true;
                return reply(vec![
                    call(
                        "task_write",
                        "write_file",
                        json!({"path":"approved.txt","content":"approved"}),
                    ),
                    call("task_exec", "run_command", json!({"command":"pwd"})),
                    call(
                        "grant_after",
                        "write_file",
                        json!({"path":grant_path.join("after"),"content":"retained"}),
                    ),
                    call(
                        "original_denied",
                        "write_file",
                        json!({"path":original,"content":"forbidden"}),
                    ),
                    call(
                        "outside_denied",
                        "write_file",
                        json!({"path":outside_path,"content":"forbidden"}),
                    ),
                    call("exec_denied", "run_command", json!({"command":"uname"})),
                ]);
            }
            if sent.0 == 0 {
                sent.0 = 1;
                return reply(vec![call(
                    "adopt",
                    "run_command",
                    json!({"command":format!("git worktree add -b task {}", task_path.display())}),
                )]);
            }
            if sent.0 == 1 {
                sent.0 = 2;
                return reply(vec![
                    call(
                        "grant_before",
                        "write_file",
                        json!({"path":grant_path.join("before"),"content":"retained"}),
                    ),
                    call("enter", "enter_plan_mode", json!({})),
                    call(
                        "plan_denied",
                        "write_file",
                        json!({"path":"while-planning.txt","content":"forbidden"}),
                    ),
                    call("exit", "exit_plan_mode", json!({})),
                ]);
            }
            reply(vec![])
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
model = "approval-fixture"
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
scheduled = true
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
        .env("NEWT_SHELL_ENGINE", "brush")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(format!("/mode full-auto\n{TASK}\n/quit\n").as_bytes())
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
    assert!(transcript.contains("plan auto-approved"), "{transcript}");
    let requests = captured.lock().unwrap();
    let result = |id: &str| {
        requests
            .iter()
            .flat_map(|r| r["messages"].as_array().unwrap())
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
            .unwrap_or_else(|| panic!("missing result {id}: {transcript}"))["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        result("adopt").contains("Adopted task worktree"),
        "{transcript}"
    );
    assert!(transcript.contains("entered PLAN MODE"), "{transcript}");
    assert!(
        !task.join("while-planning.txt").exists(),
        "Plan allowed a write"
    );
    assert!(
        transcript.contains("Tool `write_file` is not available for this request"),
        "{transcript}"
    );
    assert_eq!(
        std::fs::read_to_string(task.join("approved.txt")).unwrap_or_else(|e| panic!(
            "approved continuation must write in the adopted task: {e}\n{transcript}"
        )),
        "approved"
    );
    assert!(
        result("task_exec").contains(task.to_str().unwrap()),
        "{transcript}"
    );
    assert!(!result("task_exec").contains("denied"), "{transcript}");
    for name in ["before", "after"] {
        assert_eq!(
            std::fs::read_to_string(granted.path().join(name))
                .unwrap_or_else(|e| panic!("retained grant must permit {name}: {e}\n{transcript}")),
            "retained"
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("original.txt")).unwrap(),
        "original"
    );
    assert!(!outside.path().join("forbidden").exists());
    for id in ["original_denied", "outside_denied", "exec_denied"] {
        assert!(result(id).contains("denied"), "{id}: {}", result(id));
    }
}
