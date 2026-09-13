use super::*;
use crate::agentic::http_loop_tests::DisplayReplay;
use crate::agentic::trim::estimate_request_tokens;
use serde_json::{json, Value};
use std::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct ReportMeasuredUsage {
    requests: Arc<Mutex<Vec<Value>>>,
    usage: ReportedUsage,
    replay: DisplayReplay,
}

enum ReportedUsage {
    Absent,
    PromptOnly,
    Complete,
}

impl Respond for ReportMeasuredUsage {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if self.replay.take(request) {
            return ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"4.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                );
        }
        let estimate = estimate_request_tokens(
            body["messages"].as_array().unwrap(),
            body.get("tools"),
            Default::default(),
        );
        self.requests.lock().unwrap().push(body);
        let mut reply = json!({
            "choices": [{"finish_reason":"stop","message":{
                "role":"assistant","content":"4."
            }}]
        });
        if !matches!(self.usage, ReportedUsage::Absent) {
            reply["usage"] = json!({"prompt_tokens":estimate * 3 / 2});
            if matches!(self.usage, ReportedUsage::Complete) {
                reply["usage"]["completion_tokens"] = json!(2);
            }
        }
        self.replay.arm(request);
        ResponseTemplate::new(200).set_body_json(reply)
    }
}

async fn consecutive_requests(usage: ReportedUsage) -> Vec<Value> {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ReportMeasuredUsage {
            requests: requests.clone(),
            usage,
            replay: Default::default(),
        })
        .mount(&server)
        .await;
    let mut config = TurnDriverConfig::new(
        server.uri(),
        "calibration-fixture",
        BackendKind::Openai,
        "driver-calibration-test-no-workspace",
    );
    config.num_ctx = Some(65_536);
    config.max_tool_rounds = 2;
    config.workflow_grace_rounds = 0;
    config.mid_loop_trim_threshold = usize::MAX;
    let mut messages = vec![MemMessage::system("Answer the current question.")];
    for i in 0..60 {
        messages.push(MemMessage::user(format!(
            "old question {i} {}",
            "x".repeat(1_200)
        )));
        messages.push(MemMessage::assistant(format!(
            "old answer {i} {}",
            "y".repeat(1_200)
        )));
    }
    let task = "What is 2 + 2?";
    messages.push(MemMessage::user(task));
    let driver = TurnDriver::with_transcript(config, messages);
    // Each call receives the runtime clone a real submitted worker receives.
    // Using the shared run_one_turn seam avoids timing-dependent poll sleeps.
    for _ in 0..2 {
        let result = run_one_turn(
            &driver.config,
            &driver.runtime.clone(),
            driver.transcript(),
            task,
        )
        .await
        .unwrap();
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.reply, "4.");
    }
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    requests
}

fn raw(request: &Value) -> usize {
    estimate_request_tokens(
        request["messages"].as_array().unwrap(),
        request.get("tools"),
        Default::default(),
    )
}

#[tokio::test]
async fn consecutive_driver_turns_keep_usage_calibration_and_shrink_the_next_request() {
    assert_calibrated_shrink(&consecutive_requests(ReportedUsage::Complete).await);
}

#[tokio::test]
async fn consecutive_driver_turns_calibrate_prompt_usage_without_completion_usage() {
    assert_calibrated_shrink(&consecutive_requests(ReportedUsage::PromptOnly).await);
}

fn assert_calibrated_shrink(requests: &[Value]) {
    assert!(
        raw(&requests[0]) < 65_536 * 80 / 100,
        "cold fixture fits the declared budget"
    );
    assert!(
        raw(&requests[0]) * 3 / 2 > 65_536 * 80 / 100,
        "measured fixture needs compaction"
    );
    assert!(
        raw(&requests[1]) < raw(&requests[0]),
        "the next worker must preserve learned usage and send a smaller prompt"
    );
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["content"] == "What is 2 + 2?"));
}

#[tokio::test]
async fn consecutive_driver_turns_without_usage_preserve_the_cold_budget() {
    let requests = consecutive_requests(ReportedUsage::Absent).await;
    assert_eq!(
        raw(&requests[1]),
        raw(&requests[0]),
        "absence of usage must not invent calibration"
    );
    // The first system envelope names this turn's fresh prompt receipt; the
    // equal-size envelope is expected to differ, while history stays intact.
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .eq(requests[0]["messages"].as_array().unwrap().iter().skip(1)));
}

/// Grounds bounded overflow guesses in the real headless dispatch path: a
/// session that received no usable usage must still admit a later small prompt.
#[tokio::test]
async fn inferred_overflows_cannot_permanently_refuse_the_next_small_driver_prompt() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&request.body).unwrap());
            // A complete JSON response is also valid when the primary requests
            // SSE. No stream flag shortcut may hide a missing primary dispatch.
            ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"finish_reason":"stop","message":{
                    "role":"assistant","content":"Hello."
                }}]
            }))
        })
        .mount(&server)
        .await;
    let mut config = TurnDriverConfig::new(
        server.uri(),
        "calibration-fixture",
        BackendKind::Openai,
        "driver-calibration-test-no-workspace",
    );
    config.num_ctx = Some(65_536);
    config.max_tool_rounds = 2;
    config.workflow_grace_rounds = 0;
    config.mid_loop_trim_threshold = usize::MAX;
    let task = "Hello";
    let driver = TurnDriver::with_transcript(config, vec![MemMessage::user(task)]);
    {
        let mut state = driver.runtime.compress_state.lock().await;
        // Six turns with an initial rejection and two failed shrinks each.
        // Reuse the actual session state; no measured sample corrects it here.
        for _ in 0..18 {
            let applied = state.calibration.ratio(None);
            state.calibration.overflow(applied);
        }
    }
    let result = run_one_turn(
        &driver.config,
        &driver.runtime.clone(),
        driver.transcript(),
        task,
    )
    .await
    .unwrap();
    assert!(
        result.error.is_none(),
        "unmeasured overflow guesses must not permanently block a small prompt: {:?}",
        result.error
    );
    assert_eq!(result.reply, "Hello.");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one primary and its optional display");
    for request in requests.iter() {
        assert!(request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "user" && message["content"] == task));
    }
}
