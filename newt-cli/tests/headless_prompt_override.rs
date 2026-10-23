//! #2938: the system message on the wire is the operator's file, byte for
//! byte, when an override is set — and newt's assembled prompt when not.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

mod common;

struct Capture(Arc<Mutex<Vec<serde_json::Value>>>);

impl Respond for Capture {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let streaming = body["stream"].as_bool().unwrap_or(false);
        self.0.lock().unwrap().push(body);
        if streaming {
            let frame = serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]});
            let sse = format!("data: {frame}\n\ndata: [DONE]\n\n");
            return ResponseTemplate::new(200).set_body_raw(sse.into_bytes(), "text/event-stream");
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}]
        }))
    }
}

/// Run `newt headless` once against a capturing peer, with `extra` args and an
/// optional config body; return the first request's system message.
async fn system_messages(
    extra: &[&std::ffi::OsStr],
    config_tail: &str,
) -> Result<Vec<String>, String> {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Capture(requests.clone()))
        .mount(&server)
        .await;
    let fixture = tempfile::tempdir().unwrap();
    let instruction = fixture.path().join("instruction.md");
    std::fs::write(&instruction, "Finish without calling a tool.\n").unwrap();
    let config = fixture.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "default_backend = \"fixture\"\n[[backends]]\nname = \"fixture\"\nendpoint = \"{}\"\nmodel = \"fixture\"\nkind = \"openai\"\n{config_tail}",
            server.uri()
        ),
    )
    .unwrap();
    let out = common::newt()
        .env_remove("NEWT_TEAM")
        .env_remove("NEWT_SYSTEM_PROMPT_FILE")
        .arg("--config")
        .arg(&config)
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction)
        .args(["--max-rounds", "1"])
        .args(extra)
        .output()
        .unwrap();
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let body = requests.lock().unwrap().pop().expect("one request");
    assert_eq!(body["messages"][0]["role"], "system", "{body}");
    Ok(body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect())
}

/// The first system message (the plain-mode tests' view).
async fn system_message(extra: &[&std::ffi::OsStr], config_tail: &str) -> Result<String, String> {
    system_messages(extra, config_tail)
        .await
        .map(|all| all[0].clone())
}

const PRUNED: &str = "You are terse.\n\n  Keep this spacing.\n";

#[tokio::test(flavor = "multi_thread")]
async fn unset_sends_the_assembled_prompt() {
    let sent = system_message(&[], "").await.unwrap();
    assert!(sent.contains("You are newt"), "{sent}");
    assert!(!sent.starts_with("Workspace root: "), "{sent}");
}

/// The bare message stays reachable for evals, by name.
#[tokio::test(flavor = "multi_thread")]
async fn the_headless_preset_sends_the_bare_workspace_message() {
    let sent = system_message(&["--prompt-preset".as_ref(), "headless".as_ref()], "")
        .await
        .unwrap();
    assert!(sent.starts_with("Workspace root: "), "{sent}");
    assert!(!sent.contains("You are newt"), "{sent}");
}

/// An unknown preset is refused before any request goes out.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_preset_is_refused() {
    let err = system_message(&["--prompt-preset".as_ref(), "nope".as_ref()], "")
        .await
        .unwrap_err();
    assert!(err.contains("nope") && err.contains("presets:"), "{err}");
}

/// One session's config and workspace, shared by a TUI run and a headless run.
struct Parity {
    root: tempfile::TempDir,
    workspace: std::path::PathBuf,
    server: MockServer,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn parity(config_tail: &str) -> Parity {
    let root = common::isolated_root();
    let workspace = root.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("README.md"), "# fixture\n").unwrap();
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list", "data": [{"id": "fixture", "object": "model"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Capture(seen.clone()))
        .mount(&server)
        .await;
    std::fs::create_dir_all(root.path().join(".newt")).unwrap();
    std::fs::write(
        root.path().join(".newt").join("config.toml"),
        format!(
            "default_backend = \"fixture\"\n[[backends]]\nname = \"fixture\"\nendpoint = \"{}\"\nmodel = \"fixture\"\nkind = \"openai\"\n[tui.permissions]\npreset = \"read_only\"\nprompt = false\nextra_exec = []\nnet = []\n[tool_exposure]\nallow_list = [\"read_file\"]\n{config_tail}",
            server.uri()
        ),
    )
    .unwrap();
    Parity {
        root,
        workspace,
        server,
        seen,
    }
}

fn b_system(body: &serde_json::Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What the builder assembled: the headless driver appends its own observed-facts
/// and output-planning text after it, and the TUI wraps it in per-turn context.
fn assembled(headless: &str) -> &str {
    headless
        .split("\n\n[Harness observed facts")
        .next()
        .unwrap()
        .trim_end()
}

impl Parity {
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
        common::isolate(&mut cmd, self.root.path());
        cmd.current_dir(&self.workspace)
            .arg("--config-dir")
            .arg(self.root.path().join(".newt"))
            .env("NEWT_NO_MODEL_PULL", "1")
            .env("NEWT_NO_COCKPIT", "1")
            .env("NEWT_NUDGE", "off")
            .env("NEWT_NO_AGENTS_FILE", "1")
            .kill_on_drop(true);
        cmd
    }

    /// The system text of the request answering "hello" (the TUI also sends a probe).
    fn first_system(&self) -> String {
        let seen = self.seen.lock().unwrap();
        let body = seen
            .iter()
            .find(|b| {
                b["messages"]
                    .as_array()
                    .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
                    .and_then(|m| m["content"].as_str())
                    .is_some_and(|c| c.trim() == "hello")
            })
            .expect("a request carried the turn");
        let text = b_system(body);
        // Session-specific names: the temp workspace, and the plan file's session id.
        let text = text.replace(self.workspace.to_str().unwrap(), "WS");
        regex::Regex::new(r"sessions/[^/]+/")
            .unwrap()
            .replace_all(&text, "sessions/ID/")
            .into_owned()
    }

    async fn tui(&self) -> String {
        let mut cmd = self.command();
        cmd.args([
            "--no-splash",
            "--ephemeral",
            "--plain",
            "--no-prompt-for-permissions",
        ])
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn the real CLI");
        let mut input = child.stdin.take().unwrap();
        input.write_all(b"hello\n/quit\n").await.unwrap();
        drop(input);
        let out =
            tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
                .await
                .expect("the chat exits")
                .expect("output");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.first_system()
    }

    async fn headless(&self, extra: &[&str]) -> String {
        let instruction = self.root.path().join("instruction.md");
        std::fs::write(&instruction, "hello\n").unwrap();
        let mut cmd = self.command();
        cmd.args(["headless", "--cwd"])
            .arg(&self.workspace)
            .arg("--instruction-file")
            .arg(&instruction)
            .args(["--max-rounds", "1"])
            .args(extra);
        let out = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
            .await
            .expect("headless exits")
            .expect("output");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.first_system()
    }
}

/// #2933: for the same config, the prompt `newt headless` assembles is the one the
/// TUI sends. The TUI adds per-turn context around it and headless adds the driver's
/// observed facts after it; the assembled prompt itself is the same bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_sends_the_prompt_the_tui_sends() {
    for tail in [
        "",
        "[prompt]\npreset = \"pi\"\n",
        "[prompt]\ncomponents = { off = [\"readme\"] }\n",
    ] {
        let p = parity(tail).await;
        let tui = p.tui().await;
        let headless = p.headless(&[]).await;
        let built = assembled(&headless);
        assert!(built.len() > 100, "{built}");
        assert!(
            tui.contains(built),
            "config tail {tail:?}\n--- headless ---\n{built}\n--- tui ---\n{tui}"
        );
        drop(p.server);
    }
}

/// `--prompt-preset` names a preset over the config's, exactly as `[prompt] preset` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flag_selects_a_preset_like_the_config_does() {
    let by_config = parity("[prompt]\npreset = \"pi\"\n").await;
    let want = by_config.headless(&[]).await;
    let by_flag = parity("").await;
    let got = by_flag.headless(&["--prompt-preset", "pi"]).await;
    assert_eq!(assembled(&got), assembled(&want));
    assert!(!assembled(&got).contains("How to work"), "{got}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_flag_sends_exactly_the_file() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), PRUNED).unwrap();
    let sent = system_message(
        &["--system-prompt-file".as_ref(), file.path().as_os_str()],
        "",
    )
    .await
    .unwrap();
    assert_eq!(sent, PRUNED);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_config_file_sends_exactly_the_file_and_the_flag_wins() {
    let from_config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(from_config.path(), PRUNED).unwrap();
    let tail = format!("[prompt]\nfile = {:?}\n", from_config.path());
    assert_eq!(system_message(&[], &tail).await.unwrap(), PRUNED);

    let from_flag = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(from_flag.path(), "flag wins\n").unwrap();
    let sent = system_message(
        &[
            "--system-prompt-file".as_ref(),
            from_flag.path().as_os_str(),
        ],
        &tail,
    )
    .await
    .unwrap();
    assert_eq!(sent, "flag wins\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_override_file_fails_the_run_instead_of_falling_back() {
    let err = system_message(
        &[
            "--system-prompt-file".as_ref(),
            "/no/such/prompt.md".as_ref(),
        ],
        "",
    )
    .await
    .unwrap_err();
    assert!(err.contains("/no/such/prompt.md"), "{err}");
}

/// Every provider-visible system message is the file and nothing else, also
/// with a scratchpad installed (its working-memory head used to lead message 0).
#[tokio::test(flavor = "multi_thread")]
async fn a_scratchpad_adds_nothing_to_the_system_material() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), PRUNED).unwrap();
    let seed = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(seed.path(), r#"{"k": "v"}"#).unwrap();
    let all = system_messages(
        &[
            "--system-prompt-file".as_ref(),
            file.path().as_os_str(),
            "--scratchpad-state".as_ref(),
            seed.path().as_os_str(),
        ],
        "",
    )
    .await
    .unwrap();
    assert_eq!(all, vec![PRUNED.to_string()]);
}

/// The plain control, over ALL system messages rather than the first.
#[tokio::test(flavor = "multi_thread")]
async fn the_file_is_the_only_system_message() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), PRUNED).unwrap();
    let all = system_messages(
        &["--system-prompt-file".as_ref(), file.path().as_os_str()],
        "",
    )
    .await
    .unwrap();
    assert_eq!(all, vec![PRUNED.to_string()]);
}
