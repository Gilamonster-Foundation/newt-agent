//! BAT: a private review URL recovers through the connected MCP catalog.
//!
//! The model backend and MCP server are simulated, while the production
//! agentic loop, `web_fetch` SSRF guard, `tool_search`, leash, and tool-event
//! recorder all run unchanged. The adaptive model deliberately emits the two
//! field-observed dead ends when the preceding tool result does not contain a
//! usable authenticated-source route, so this test goes red if the recovery
//! contract stops composing through the live loop.
// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:45 EDT | Date: 2026-08-12

use crate::agentic::{chat_complete, ChatCtx, LeasedMcpCall, McpTools, PromptIntake};
use crate::{Caveats, MemMessage, ToolEvent};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const REVIEW_TOOL: &str = "review_source__get_review";
const USER_REVIEW_URL: &str = "https://reviews.example.test/reviews/42";
// Numeric loopback is the deterministic simulated DNS result for the private
// review host. `Caveats::top()` intentionally does not opt it into private-IP
// access, so the real SSRF guard (`crate::web_fetch`) refuses it before any connection.
const PRIVATE_FETCH_TARGET: &str = "http://127.0.0.1/reviews/42";
const MCP_RESULT: &str = "authenticated review 42 loaded";

struct ReviewPermission(usize);

impl newt_policy::PermissionGate for ReviewPermission {
    fn ask_dependency_fetch(
        &mut self,
        _prepared: &newt_policy::caveats::Caveats,
        _request: &newt_policy::permissions::PermissionRequest,
    ) -> newt_policy::permissions::PermissionDecision {
        newt_policy::permissions::PermissionDecision::Deny
    }
    fn ask_question(&mut self, _question: &str) -> newt_policy::HumanQuestionOutcome {
        newt_policy::HumanQuestionOutcome::Unavailable
    }

    fn ask(
        &mut self,
        requests: &[newt_policy::PermissionRequest],
    ) -> newt_policy::PermissionDecision {
        if requests.len() == 1
            && requests[0].kind == newt_policy::DenialKind::RemoteTool
            && requests[0].target == REVIEW_TOOL
        {
            self.0 += 1;
            newt_policy::PermissionDecision::Allow(Caveats::top())
        } else {
            newt_policy::PermissionDecision::Deny
        }
    }
}

fn last_tool_result(body: &serde_json::Value) -> Option<&str> {
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")?
        .get("content")?
        .as_str()
}

fn ollama_tool_call(name: &str, arguments: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "message": {
            "content": "",
            "tool_calls": [{
                "function": {
                    "name": name,
                    "arguments": arguments
                }
            }]
        }
    }))
}

/// Adaptive simulated inference: obey the authenticated-source recovery when
/// it is present; otherwise reproduce the operator-blocking fallback.
struct PrivateReviewModel;

impl Respond for PrivateReviewModel {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        match last_tool_result(&body) {
            None => ollama_tool_call(
                "web_fetch",
                serde_json::json!({"url": PRIVATE_FETCH_TARGET}),
            ),
            Some(result) if result.contains(MCP_RESULT) => ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "message": {"content": "Review evidence loaded through the connector."}
                })),
            Some(result)
                if result.contains("Authenticated-source recovery")
                    && result.contains(REVIEW_TOOL) =>
            {
                ollama_tool_call("tool_search", serde_json::json!({"query": REVIEW_TOOL}))
            }
            Some(result) if result.contains("Tools matching") && result.contains(REVIEW_TOOL) => {
                ollama_tool_call(REVIEW_TOOL, serde_json::json!({"url": USER_REVIEW_URL}))
            }
            Some(result)
                if result.contains("run_command")
                    && (result.contains("active persona")
                        || result.contains("unavailable for this")) =>
            {
                ollama_tool_call(
                    "request_user_input",
                    serde_json::json!({
                        "question": "Please provide the local checkout server configuration."
                    }),
                )
            }
            Some(result) if result.contains("no human available this session") => {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "Blocked waiting for local client configuration."}
                }))
            }
            // This is the field-observed failure mode. If the web-fetch result
            // loses either discovery guidance or the callable MCP candidate,
            // the production loop records this tool and the BAT fails.
            Some(_) => ollama_tool_call(
                "run_command",
                serde_json::json!({
                    "command": "curl -fsSL https://reviews.example.test/reviews/42"
                }),
            ),
        }
    }
}

struct RecordingReviewMcp {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl McpTools for RecordingReviewMcp {
    fn handles(&self, name: &str) -> bool {
        name == REVIEW_TOOL
    }

    fn tool_defs(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": REVIEW_TOOL,
                "description": "Get an authenticated code review from its URL.",
                "parameters": {
                    "type": "object",
                    "properties": {"url": {"type": "string"}},
                    "required": ["url"]
                }
            }
        })]
    }

    async fn call(&mut self, leased: &LeasedMcpCall<'_>) -> String {
        self.calls.lock().unwrap().push(leased.tool().to_string());
        MCP_RESULT.to_string()
    }
}

/// User acceptance regression for the private-review incident: after raw HTTP
/// is refused, Newt must discover and call the connected authenticated source
/// before trying shell or asking for unrelated local-client configuration.
#[ignore = "simulated integration: loopback inference server; scheduled/release MCP acceptance tier"]
#[serial_test::serial(mcp_acceptance)]
#[tokio::test]
async fn private_review_fetch_recovers_through_tool_search_and_mcp() {
    let model = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(PrivateReviewModel)
        .mount(&model)
        .await;

    let task = format!("Give me a review of {USER_REVIEW_URL}");
    let intake = PromptIntake::analyze(&task);
    let messages = vec![
        MemMessage::system("Use connected authenticated sources for private URLs."),
        MemMessage::user(task.clone()),
    ];
    let caveats = Caveats::top();
    let mut permission = ReviewPermission(0);
    let persona_tools = vec![
        "web_fetch".to_string(),
        "tool_search".to_string(),
        REVIEW_TOOL.to_string(),
    ];
    let mut events: Vec<ToolEvent> = Vec::new();
    let mcp_calls = Arc::new(Mutex::new(Vec::new()));
    let mut mcp = RecordingReviewMcp {
        calls: mcp_calls.clone(),
    };

    let (reply, _, _, hallucinations) = chat_complete(
        ChatCtx {
            model: "adaptive-private-review-model",
            task: &task,
            persona_tools: Some(&persona_tools),
            max_tool_rounds: 6,
            prompt_disposition: intake.disposition(),
            tool_events: Some(&mut events),
            permission_gate: Some(&mut permission),
            ..crate::test_chat_ctx::chat_ctx(&model.uri(), &messages, &caveats)
        },
        &mut mcp,
    )
    .await
    .expect("private review recovery completes through the production loop");

    let executed: Vec<&str> = events.iter().map(|event| event.tool.as_str()).collect();
    assert_eq!(
        executed,
        vec!["web_fetch", "tool_search", REVIEW_TOOL],
        "raw fetch must recover through discovery and the connected MCP: {events:?}"
    );
    assert!(
        !executed.contains(&"run_command") && !executed.contains(&"request_user_input"),
        "private-source recovery must not fall back to shell/operator setup: {events:?}"
    );
    assert_eq!(
        mcp_calls.lock().unwrap().as_slice(),
        [REVIEW_TOOL],
        "the discovered namespaced MCP must execute exactly once"
    );
    assert_eq!(hallucinations, 0, "all three tool names are real");
    assert_eq!(
        permission.0, 1,
        "the remote tool required explicit permission"
    );
    assert!(reply.contains("connector"), "final answer: {reply}");

    let wire = model
        .received_requests()
        .await
        .expect("model requests recorded")
        .iter()
        .map(|request| String::from_utf8_lossy(&request.body))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        wire.contains("SSRF block"),
        "the raw-fetch guard must remain intact: {wire}"
    );
    assert!(
        wire.contains("Authenticated-source recovery"),
        "the recovery route must reach the next inference round: {wire}"
    );
}
