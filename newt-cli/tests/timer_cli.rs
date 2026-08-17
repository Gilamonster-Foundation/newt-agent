//! Process-level regressions for `newt timer` — the self-scheduled wake-up
//! substrate (PR #1747).
//!
//! Two blockers are grounded here against the real `newt` binary:
//!
//! 1. `--every 0` must not create a repeating job. It would make
//!    `advance_repeat` spin forever once the job is due. The CLI must reject
//!    it cleanly with a non-zero exit and persist nothing.
//! 2. The documented wake-up path must actually reach the Newt solve entry
//!    point. `newt timer fire --run` drains a due timer and drives a real
//!    headless `newt solve` (in-process) against a mocked backend; the
//!    scheduled prompt must appear on the wire as the solve instruction.
//!
//! The inference backend is mocked with `wiremock` (the same tier
//! `solve_cli.rs` uses) so a scheduled prompt can be proven to reach the
//! solve entry point without a live model.

use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const NEMOTRON_MODEL: &str = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16";

/// Capture the chat request body, then finish the turn with no tool calls so
/// the headless solve completes in one round.
struct CaptureThenFinish {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Respond for CaptureThenFinish {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("chat request is JSON");
        self.requests
            .lock()
            .expect("request capture lock")
            .push(body);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": NEMOTRON_MODEL,
            "choices": [{
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }]
        }))
    }
}

/// Regression (#1747 blocker 1): `--every 0` must not create a repeating job.
/// It must fail cleanly with a non-zero exit and a clear message, and persist
/// nothing — `advance_repeat` would otherwise spin forever once the job is due.
#[test]
fn every_zero_is_rejected_cleanly() {
    let fixture = tempfile::tempdir().expect("timer fixture");
    Command::cargo_bin("newt")
        .expect("newt binary")
        .args(["timer", "schedule", "5m", "watch the pipeline"])
        .arg("--every")
        .arg("0")
        .arg("--dir")
        .arg(fixture.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("greater than zero"));
    // Nothing was persisted: validation runs before the store is opened.
    let timers: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(fixture.path().join("timers.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    assert!(timers.is_empty(), "no timer persisted for --every 0");
}

/// Regression (#1747 blocker 2): a scheduled prompt must reach the Newt solve
/// entry point. `newt timer fire --run` drains a due timer and drives a real
/// headless `newt solve` (in-process) against a mocked backend; the scheduled
/// prompt appears on the wire as the solve instruction. This is the supported
/// wake-up path — `newt solve "$prompt"` does not work (it requires
/// `--instruction-file`), so the scheduler invokes the solve entry point
/// instead of changing `solve` semantics.
#[tokio::test(flavor = "multi_thread")]
async fn fire_run_reaches_solve_entry_point() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let fixture = tempfile::tempdir().expect("timer + solve fixture");
    let config_path = fixture.path().join("timer.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "nemotron"

[[backends]]
name = "nemotron"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
api = "chat_completions"

[backends.capability]
reasoning_replay_scope = "current_user_turn"

[backends.capability.chat_completions]
cognition = true
chat_template_kwargs = true
parallel_tool_calls = false
bounded_reasoning_continuation = true
"#,
            server.uri()
        ),
    )
    .expect("write explicit solve config");

    let prompt = "check PR #1747 CI is green";
    // Schedule a timer due immediately. A zero *delay* (0s) is allowed — only a
    // zero *repeat* is rejected.
    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["timer", "schedule", "0s", prompt])
        .arg("--dir")
        .arg(fixture.path())
        .assert()
        .success();

    // Fire + run: the due prompt must drive a real solve against the mock.
    // current_dir is the solve workspace (cwd "." resolves here).
    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["timer", "fire", "--run"])
        .arg("--dir")
        .arg(fixture.path())
        .current_dir(fixture.path())
        .assert()
        .success();

    let requests = requests.lock().expect("request capture lock");
    assert_eq!(
        requests.len(),
        1,
        "the scheduled prompt must reach the solve entry point (one chat turn)"
    );
    let user_content: Vec<&str> = requests[0]["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(
        user_content.iter().any(|c| c.contains(prompt)),
        "scheduled prompt must reach the wire as the solve instruction: {user_content:?}"
    );
}

/// Regression (#1747 req 7/12): `fire --run` must drain EVERY due timer — a
/// failure earlier in the beat must not silently lose later due timers. On the
/// success path each due prompt reaches the solve entry point, so the mock
/// backend sees one chat turn per scheduled timer. (The failure path — failed
/// timer stays pending, later timers remain unclaimed — is covered by the
/// pure-core `failed_execution_keeps_later_timers_pending` unit test.)
#[tokio::test(flavor = "multi_thread")]
async fn fire_run_drains_all_due_timers() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let fixture = tempfile::tempdir().expect("timer + solve fixture");
    let config_path = fixture.path().join("timer.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_backend = "nemotron"

[[backends]]
name = "nemotron"
endpoint = "{}"
model = "{NEMOTRON_MODEL}"
kind = "openai"
api = "chat_completions"

[backends.capability]
reasoning_replay_scope = "current_user_turn"

[backends.capability.chat_completions]
cognition = true
chat_template_kwargs = true
parallel_tool_calls = false
bounded_reasoning_continuation = true
"#,
            server.uri()
        ),
    )
    .expect("write explicit solve config");

    let prompts = ["check build A", "check build B", "check build C"];
    for p in &prompts {
        Command::cargo_bin("newt")
            .expect("newt binary")
            .env_remove("NEWT_TEAM")
            .arg("--config")
            .arg(&config_path)
            .args(["timer", "schedule", "0s", p])
            .arg("--dir")
            .arg(fixture.path())
            .assert()
            .success();
    }

    Command::cargo_bin("newt")
        .expect("newt binary")
        .env_remove("NEWT_TEAM")
        .arg("--config")
        .arg(&config_path)
        .args(["timer", "fire", "--run"])
        .arg("--dir")
        .arg(fixture.path())
        .current_dir(fixture.path())
        .assert()
        .success();

    let requests = requests.lock().expect("request capture lock");
    assert_eq!(
        requests.len(),
        prompts.len(),
        "every due timer must reach the solve entry point — none silently lost"
    );
    let seen: Vec<String> = requests
        .iter()
        .flat_map(|r| {
            r["messages"]
                .as_array()
                .expect("messages array")
                .iter()
                .filter(|m| m["role"] == "user")
                .filter_map(|m| m["content"].as_str().map(String::from))
        })
        .collect();
    for p in &prompts {
        assert!(
            seen.iter().any(|c| c.contains(p)),
            "prompt {p:?} must reach the wire: {seen:?}"
        );
    }

    // After a successful drain, all one-shot timers are removed: the queue is
    // empty. This proves the success side of claim → execute → acknowledge.
    let timers: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(fixture.path().join("timers.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    assert!(
        timers.is_empty(),
        "one-shots removed after success: {timers:?}"
    );
}
