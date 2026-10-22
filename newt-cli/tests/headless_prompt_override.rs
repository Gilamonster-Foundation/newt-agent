//! #2938: the system message on the wire is the operator's file, byte for
//! byte, when an override is set — and newt's assembled prompt when not.

use std::sync::{Arc, Mutex};

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
async fn system_message(extra: &[&std::ffi::OsStr], config_tail: &str) -> Result<String, String> {
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
    Ok(body["messages"][0]["content"].as_str().unwrap().to_string())
}

const PRUNED: &str = "You are terse.\n\n  Keep this spacing.\n";

#[tokio::test(flavor = "multi_thread")]
async fn unset_sends_the_assembled_prompt() {
    let sent = system_message(&[], "").await.unwrap();
    assert!(sent.starts_with("Workspace root: "), "{sent}");
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
