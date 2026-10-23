//! #2933 PR4 review (#2991): a config that goes invalid AFTER startup must neither
//! widen the tool policy nor let a component write claim success.
//!
//! Real `newt` sessions against a recording provider, with a user config that
//! allows only `read_file` and a project config that turns invalid mid-session.
//!
//! - plain command path: `/settings system-prompt preset default` refuses, creates
//!   no receipt, and the next request keeps the accepted prompt and tool list;
//! - rich panel path (PTY): applying the components panel after the config broke
//!   keeps the accepted config; the next request still carries one tool.
//!
//! Run: `cargo test -p newt-agent --test prompt_component_refresh`.

#![cfg(unix)]

mod common;

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
#[cfg(feature = "rich-tui")]
use tests_pty::Pty;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BUDGET: Duration = Duration::from_secs(60);
const MODEL: &str = "refresh-fixture";
#[cfg(feature = "rich-tui")]
const READY: &str = "REFRESH_READY> ";
/// A project layer whose `[prompt]` section does not resolve.
const BROKEN_PROJECT: &str = "[prompt]\npreset = \"nope\"\n";
const THREE_CS: &str = "Seek ground truth";

/// One chat request as the provider saw it.
#[derive(Clone, Debug)]
struct Seen {
    user: String,
    tools: Vec<String>,
    system: String,
}

fn answer(streaming: bool, content: &str) -> ResponseTemplate {
    if streaming {
        let frame = json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]});
        let usage = json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}});
        ResponseTemplate::new(200).set_body_raw(
            format!("data: {frame}\n\ndata: {usage}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "model": MODEL,
            "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }))
    }
}

struct Fixture {
    server: MockServer,
    root: tempfile::TempDir,
    config_dir: std::path::PathBuf,
    workspace: std::path::PathBuf,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// A provider recording each turn request; when a user message equals `break_on`,
/// the project layer turns invalid before the answer goes out.
async fn fixture(user_config_tail: &str, break_on: Option<&'static str>) -> Fixture {
    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    let workspace = root.path().join("ws");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(workspace.join(".newt")).unwrap();
    let project = workspace.join(".newt").join("config.toml");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": [{"id": MODEL, "object": "model"}]
        })))
        .mount(&server)
        .await;
    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let capture = Arc::clone(&seen);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("chat JSON");
            let streaming = body["stream"].as_bool().unwrap_or(false);
            let messages = body["messages"].as_array().expect("messages");
            let user = messages
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .and_then(|m| m["content"].as_str())
                .unwrap_or("")
                .to_string();
            if user.starts_with("TURN_") {
                let tools = body["tools"]
                    .as_array()
                    .map(|t| {
                        t.iter()
                            .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let system = messages
                    .iter()
                    .filter(|m| m["role"] == "system")
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                capture.lock().unwrap().push(Seen {
                    user: user.clone(),
                    tools,
                    system,
                });
            }
            if break_on == Some(user.as_str()) {
                std::fs::write(&project, BROKEN_PROJECT).unwrap();
            }
            answer(streaming, &format!("ECHO[{user}]"))
        })
        .mount(&server)
        .await;
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "default_backend = \"fixture\"\n\n[[backends]]\nname = \"fixture\"\nendpoint = \"{}\"\nmodel = \"{MODEL}\"\nkind = \"openai\"\n\n[tui.permissions]\npreset = \"read_only\"\nprompt = false\nextra_exec = []\nnet = []\n\n[tool_exposure]\nallow_list = [\"read_file\"]\n\n{user_config_tail}",
            server.uri()
        ),
    )
    .unwrap();
    Fixture {
        server,
        root,
        config_dir,
        workspace,
        seen,
    }
}

fn command(f: &Fixture) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate(&mut cmd, f.root.path());
    cmd.current_dir(&f.workspace)
        .arg("--config-dir")
        .arg(&f.config_dir)
        .args([
            "--no-splash",
            "--ephemeral",
            "--no-agents-file",
            "--no-prompt-for-permissions",
        ])
        .env("NEWT_NO_MODEL_PULL", "1")
        .env("NEWT_NO_COCKPIT", "1")
        .env("NEWT_NUDGE", "off")
        .kill_on_drop(true);
    cmd
}

fn turn(seen: &[Seen], user: &str) -> Seen {
    seen.iter()
        .find(|s| s.user == user)
        .unwrap_or_else(|| panic!("no request carried {user}: {seen:?}"))
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_component_write_after_the_config_broke_refuses_and_keeps_the_accepted_prompt() {
    let f = fixture(
        "[prompt]\ncomponents = { off = [\"habit-ground-truth\"] }\n",
        Some("TURN_ONE"),
    )
    .await;
    let mut cmd = command(&f);
    cmd.args(["--plain"])
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn the real CLI");
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"TURN_ONE\n/settings system-prompt preset default\nTURN_TWO\n/quit\n")
        .await
        .unwrap();
    drop(input);
    let output = tokio::time::timeout(BUDGET, child.wait_with_output())
        .await
        .expect("the chat exits")
        .expect("output");
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The write refused: no success line, the resolver error named, no receipt.
    assert!(
        !shown.contains("system-prompt preset: default (every component on)"),
        "a failed resolution must not report success:\n{shown}"
    );
    assert!(shown.contains("config does not resolve"), "{shown}");
    let receipts = std::fs::read_to_string(f.config_dir.join("receipts.jsonl")).unwrap_or_default();
    assert!(
        !receipts.contains("system-prompt"),
        "no receipt for a write that did not happen:\n{receipts}"
    );
    // The live prompt and the tool list are the accepted ones.
    let seen = f.seen.lock().unwrap().clone();
    let (one, two) = (turn(&seen, "TURN_ONE"), turn(&seen, "TURN_TWO"));
    assert!(
        !one.system.contains(THREE_CS),
        "habit-ground-truth starts off"
    );
    assert!(
        !two.system.contains(THREE_CS),
        "still off after the refused write"
    );
    assert_eq!(two.tools, ["read_file"], "{shown}");
    drop(f.server);
}

/// #2993 review: `/settings system-prompt off readme` edits a component, not the
/// preset, so a decorated `preset = "default"` keeps its comments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_component_edit_leaves_a_decorated_default_preset_alone() {
    let f = fixture(
        "[prompt]\n# which prompt\npreset = \"default\" # explicit on purpose\n",
        None,
    )
    .await;
    let mut cmd = command(&f);
    cmd.args(["--plain"])
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn the real CLI");
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"/settings system-prompt off readme\n/quit\n")
        .await
        .unwrap();
    drop(input);
    let output = tokio::time::timeout(BUDGET, child.wait_with_output())
        .await
        .expect("the chat exits")
        .expect("output");
    let shown = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        shown.contains("system-prompt component readme: off"),
        "{shown}"
    );
    let saved = std::fs::read_to_string(f.config_dir.join("config.toml")).unwrap();
    assert!(
        saved.contains("# which prompt")
            && saved.contains("preset = \"default\" # explicit on purpose")
            && saved.contains("off = [\"readme\"]"),
        "the edit must keep the preset line and its comments:\n{saved}"
    );
    drop(f.server);
}

/// #2933 PR7: `soul.md` is retired. A file in the user config directory is not read,
/// the session says so once (naming `[prompt] file`), and the prompt is the one built
/// from components.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_soul_md_is_ignored_with_a_notice_pointing_at_prompt_file() {
    let f = fixture("", None).await;
    std::fs::write(f.config_dir.join("soul.md"), "You are a Django expert.").unwrap();
    let mut cmd = command(&f);
    cmd.args(["--plain"])
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn the real CLI");
    let mut input = child.stdin.take().unwrap();
    // Several turns and a settings write (which re-resolves the config) in one session.
    input
        .write_all(b"TURN_ONE\n/settings system-prompt off readme\nTURN_TWO\n/quit\n")
        .await
        .unwrap();
    drop(input);
    let output = tokio::time::timeout(BUDGET, child.wait_with_output())
        .await
        .expect("the chat exits")
        .expect("output");
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        shown.contains("soul.md") && shown.contains("[prompt] file"),
        "the retired soul gets a notice:\n{shown}"
    );
    assert_eq!(
        shown.matches("ignoring").count(),
        1,
        "one notice per source per session, not one per config read:\n{shown}"
    );
    let seen = f.seen.lock().unwrap().clone();
    let one = turn(&seen, "TURN_ONE");
    assert!(!one.system.contains("Django"), "{}", one.system);
    assert!(one
        .system
        .contains("You are newt, a free, friendly, local agentic coder."));
    drop(f.server);
}

#[cfg(feature = "rich-tui")]
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

#[cfg(feature = "rich-tui")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(prompt_component_refresh_pty)]
async fn the_components_panel_keeps_the_accepted_config_when_it_breaks_mid_panel() {
    let f = fixture("", None).await;
    let pty = Pty::open();
    let mut cmd = command(&f);
    cmd.env("NEWT_EDIT_MODE", "emacs")
        .env("NEWT_PROMPT", READY)
        .env("NEWT_FOOTER", "off")
        .env("TERM", "xterm-256color")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(pty.slave_stdio());
    let mut child = cmd.spawn().expect("spawn the real CLI on the owned PTY");
    drop(cmd);
    assert!(pty.wait_for_screen(READY, BUDGET), "{:?}", pty.screen());

    say(&pty, "TURN_ONE", "ECHO[TURN_ONE]");
    pty.type_in("/settings system-prompt components\r");
    assert!(
        pty.wait_for_screen("presentation only", BUDGET),
        "the panel never opened: {:?}",
        pty.screen()
    );
    // The config breaks while the panel is open; then the operator applies a change.
    std::fs::write(
        f.workspace.join(".newt").join("config.toml"),
        BROKEN_PROJECT,
    )
    .unwrap();
    pty.type_in("G");
    pty.type_in(" ");
    pty.type_in("\r");
    assert!(
        pty.wait_for_screen("keeping the last accepted configuration", BUDGET),
        "the refresh must keep the accepted config, not fall to defaults: {:?}",
        pty.screen()
    );
    say(&pty, "TURN_TWO", "ECHO[TURN_TWO]");
    pty.type_in("/exit\r");
    let _ = tokio::time::timeout(BUDGET, child.wait()).await;

    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(turn(&seen, "TURN_ONE").tools, ["read_file"]);
    assert_eq!(
        turn(&seen, "TURN_TWO").tools,
        ["read_file"],
        "a broken config must not widen the tool list"
    );
}
