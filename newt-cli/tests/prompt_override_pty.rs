//! Real-resource grounding for the system prompt override in the TUI (#2938).
//!
//! Run: `cargo test -p newt-agent --test prompt_override_pty`.
//!
//! The mocked tests prove the pieces (`turn_system_text`, the refresh helper,
//! the headless wire). This drives the real `newt` chat loop on an owned PTY
//! with a mocked provider and reads what the provider actually received: the
//! TUI wraps harness text around the system prompt every turn, which only a
//! captured request can show. Setting and resetting the file must take effect
//! on the very next turn, with no `/new` between.

#![cfg(unix)]

mod common;

use std::time::Duration;

use serde_json::json;
use tests_pty::Pty;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const BUDGET: Duration = Duration::from_secs(30);
const MODEL: &str = "prompt-override-fixture";
const READY: &str = "PROMPT_OVERRIDE_READY> ";
const PRUNED: &str = "You are the pruned prompt.\n\n  Keep this spacing.\n";

/// Answers every turn with `ECHO[<last user message>]`, so each turn's end is
/// recognisable on screen.
struct Echo;

impl Respond for Echo {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("chat JSON");
        let last_user = body["messages"]
            .as_array()
            .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
            .and_then(|m| m["content"].as_str())
            .unwrap_or("")
            .to_string();
        let answer = format!("ECHO[{last_user}]");
        if body["stream"].as_bool().unwrap_or(false) {
            let frame =
                json!({"choices": [{"delta": {"content": answer}, "finish_reason": "stop"}]});
            let usage = json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}});
            ResponseTemplate::new(200).set_body_raw(
                format!("data: {frame}\n\ndata: {usage}\n\ndata: [DONE]\n\n"),
                "text/event-stream",
            )
        } else {
            ResponseTemplate::new(200).set_body_json(json!({
                "model": MODEL,
                "choices": [{"message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
            }))
        }
    }
}

/// The first system message of the request that carried user message `turn`.
async fn system_message_for(server: &MockServer, turn: &str) -> String {
    let requests = server.received_requests().await.expect("captured requests");
    requests
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .find(|body| {
            body["messages"].as_array().is_some_and(|m| {
                m.iter()
                    .any(|m| m["role"] == "user" && m["content"] == turn)
            })
        })
        .and_then(|body| {
            body["messages"]
                .as_array()?
                .iter()
                .find(|m| m["role"] == "system")?["content"]
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| panic!("no request carried {turn}"))
}

fn say(pty: &Pty, line: &str, expect: &str) {
    pty.type_in(&format!("{line}\r"));
    assert!(
        pty.wait_for_screen(expect, BUDGET),
        "{line:?} never produced {expect:?}: {:?}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen_after(expect, READY, BUDGET),
        "no fresh editor after {line:?}: {:?}",
        pty.screen()
    );
}

/// A mock provider plus an isolated config; `config_tail` is appended to it.
async fn fixture(config_tail: &str) -> (MockServer, tempfile::TempDir, std::path::PathBuf) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": [{"id": MODEL, "object": "model"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Echo)
        .mount(&server)
        .await;
    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    std::fs::create_dir(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "default_backend = \"fixture\"\n\n[[backends]]\nname = \"fixture\"\nendpoint = \"{}\"\nmodel = \"{MODEL}\"\nkind = \"openai\"\n\n[tui.permissions]\npreset = \"read_only\"\nprompt = false\nextra_exec = []\nnet = []\n{config_tail}",
            server.uri()
        ),
    )
    .unwrap();
    (server, root, config)
}

fn start(root: &tempfile::TempDir, config: &std::path::Path) -> (Pty, tokio::process::Child) {
    let pty = Pty::open();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate(&mut cmd, root.path());
    cmd.args(["--no-splash", "--ephemeral", "--no-prompt-for-permissions"])
        .arg("--config")
        .arg(config)
        .env("NEWT_DISABLE_OCAP", "0")
        .env("NEWT_FULL_ACCESS", "0")
        .env("NEWT_EDIT_MODE", "emacs")
        .env("NEWT_NO_MODEL_PULL", "1")
        .env("NEWT_PROMPT", READY)
        .env("NEWT_FOOTER", "off")
        .env("NEWT_NO_COCKPIT", "1")
        .env("TERM", "xterm-256color")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(pty.slave_stdio())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn real CLI on the owned PTY");
    drop(cmd);
    assert!(pty.wait_for_screen(READY, BUDGET), "{:?}", pty.screen());
    (pty, child)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(prompt_override_pty)]
async fn the_tui_sends_exactly_the_file_and_set_and_reset_apply_at_once() {
    let (server, root, config) = fixture("").await;
    let file = root.path().join("pruned.md");
    std::fs::write(&file, PRUNED).unwrap();
    let (pty, mut child) = start(&root, &config);

    say(&pty, "TURN_ONE", "ECHO[TURN_ONE]");
    say(
        &pty,
        &format!("/settings system-prompt {}", file.display()),
        "system prompt file:",
    );
    say(&pty, "TURN_TWO", "ECHO[TURN_TWO]");
    say(
        &pty,
        "/settings system-prompt reset",
        "system prompt file: reset",
    );
    say(&pty, "TURN_THREE", "ECHO[TURN_THREE]");
    pty.type_in("/exit\r");
    let _ = tokio::time::timeout(BUDGET, child.wait()).await;

    let one = system_message_for(&server, "TURN_ONE").await;
    let two = system_message_for(&server, "TURN_TWO").await;
    let three = system_message_for(&server, "TURN_THREE").await;
    assert!(
        one.contains("Workspace: "),
        "assembled before the file: {one:.300}"
    );
    assert_eq!(two, PRUNED, "the system message must be exactly the file");
    assert!(
        three.contains("Workspace: ") && !three.contains(PRUNED),
        "reset applies at once: {three:.300}"
    );
}

/// `reset` is a session state: it suppresses a configured `[prompt] file`
/// (leaving the config alone) rather than revealing it again, and selecting an
/// unusable file is refused and leaves the previous state in force.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(prompt_override_pty)]
async fn reset_beats_a_configured_file_and_an_unusable_file_is_refused() {
    let root_for_file = common::isolated_root();
    let file = root_for_file.path().join("configured.md");
    std::fs::write(&file, PRUNED).unwrap();
    let (server, root, config) = fixture(&format!("\n[prompt]\nfile = {file:?}\n")).await;
    let (pty, mut child) = start(&root, &config);

    say(&pty, "TURN_ONE", "ECHO[TURN_ONE]");
    say(
        &pty,
        "/settings system-prompt reset",
        "system prompt file: reset",
    );
    say(&pty, "TURN_TWO", "ECHO[TURN_TWO]");
    say(
        &pty,
        "/settings system-prompt /no/such/prompt.md",
        "nothing changed",
    );
    say(&pty, "TURN_THREE", "ECHO[TURN_THREE]");
    pty.type_in("/exit\r");
    let _ = tokio::time::timeout(BUDGET, child.wait()).await;

    assert_eq!(system_message_for(&server, "TURN_ONE").await, PRUNED);
    let two = system_message_for(&server, "TURN_TWO").await;
    assert!(
        two.contains("Workspace: ") && !two.contains(PRUNED),
        "{two:.300}"
    );
    let three = system_message_for(&server, "TURN_THREE").await;
    assert!(
        three.contains("Workspace: ") && !three.contains(PRUNED),
        "{three:.300}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        PRUNED,
        "config file untouched"
    );
}
