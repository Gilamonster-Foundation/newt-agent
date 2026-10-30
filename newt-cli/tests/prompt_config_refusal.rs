//! #2933 review of #2990: an invalid `[prompt]` setting refuses the TUI session.
//!
//! The startup used to print "running on built-in defaults" and carry on. Defaults
//! drop `[tool_exposure] allow_list`, so a typo in `[prompt] components` sent all 41
//! tools to the provider instead of the one the operator allowed. This drives the
//! real binary against a recording provider and asserts the refusal: non-zero exit,
//! the error named, and not one inference request.
//!
//! Run: `cargo test -p newt-agent --test prompt_config_refusal`.

mod common;

use std::time::Duration;

use serde_json::json;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BUDGET: Duration = Duration::from_secs(60);
const MODEL: &str = "prompt-refusal-fixture";

/// A recording provider and an isolated config restricted to `read_file`, with
/// `prompt_tail` as the `[prompt]` section.
async fn run(prompt_tail: &str) -> (std::process::Output, Vec<serde_json::Value>) {
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
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": MODEL,
            "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;
    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    std::fs::create_dir(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "default_backend = \"fixture\"\n\n[[backends]]\nname = \"fixture\"\nendpoint = \"{}\"\nmodel = \"{MODEL}\"\nkind = \"openai\"\n\n[tool_exposure]\nallow_list = [\"read_file\"]\n\n{prompt_tail}",
            server.uri()
        ),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate(&mut cmd, root.path());
    cmd.args(["--no-splash", "--ephemeral", "--no-prompt-for-permissions"])
        .arg("--config")
        .arg(&config)
        .env("NEWT_NO_MODEL_PULL", "1")
        .env("NEWT_NO_COCKPIT", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn the real CLI");
    // One ordinary request, then quit: the session, if it starts, sends it.
    let mut stdin = child.stdin.take().expect("piped stdin");
    tokio::io::AsyncWriteExt::write_all(&mut stdin, b"say hello\n/exit\n")
        .await
        .expect("type the request");
    drop(stdin);
    let output = tokio::time::timeout(BUDGET, child.wait_with_output())
        .await
        .expect("the CLI exits")
        .expect("output");
    let requests = server
        .received_requests()
        .await
        .expect("captured requests")
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .filter_map(|r| serde_json::from_slice(&r.body).ok())
        .collect();
    (output, requests)
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_prompt_setting_refuses_the_session_and_sends_nothing() {
    for (tail, named) in [
        ("[prompt]\ncomponents = { off = [\"nope\"] }\n", "nope"),
        ("[prompt]\npreset = \"nope\"\n", "nope"),
        ("[prompt]\npreset = \"headless\"\n", "headless"),
        (
            "[prompt]\ncomponents = { on = [\"habit-ground-truth\"], off = [\"habit-ground-truth\"] }\n",
            "both",
        ),
    ] {
        let (output, requests) = run(tail).await;
        let shown = text(&output);
        assert!(
            !output.status.success(),
            "{tail}: must exit non-zero\n{shown}"
        );
        assert!(requests.is_empty(), "{tail}: no inference request\n{shown}");
        assert!(
            shown.contains("config resolution failed") && shown.contains(named),
            "{tail}: names the error\n{shown}"
        );
        assert!(
            !shown.contains("built-in defaults"),
            "{tail}: never falls back to defaults\n{shown}"
        );
    }
}

/// The control: the same restricted config with a VALID prompt setting starts, so
/// the refusal above is the setting's doing and not the fixture's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_prompt_setting_starts_the_session() {
    let (output, requests) =
        run("[prompt]\ncomponents = { off = [\"habit-ground-truth\"] }\n").await;
    let shown = text(&output);
    assert!(output.status.success(), "{shown}");
    assert!(!shown.contains("config resolution failed"), "{shown}");
    // The restriction held: the one request carried the one allowed tool.
    let tools: Vec<&str> = requests
        .iter()
        .find(|r| r.to_string().contains("say hello"))
        .and_then(|r| r["tools"].as_array())
        .map(|t| {
            t.iter()
                .filter_map(|t| t["function"]["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(tools, ["read_file"], "{shown}");
}
