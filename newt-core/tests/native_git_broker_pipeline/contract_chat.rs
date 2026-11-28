//! Scripted inference-loop fixture: one run_command call, then a final answer.
//! Only the host-side mock model uses loopback HTTP; child tools retain net:none.
//! Adapted from agentic/mod_tests/openai_stream_loop.rs and http_loop.rs.
#[path = "../support/chat_ctx.rs"]
mod fixture;

use newt_core::{
    agentic::{chat_complete, GitTool},
    worktree_adoption::WorktreeSession,
    BackendKind, Caveats, ChatCtx, MemMessage, NoMcp,
};
use newt_policy::PermissionGate;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

pub async fn dispatch(
    session: &WorktreeSession,
    workspace: &Path,
    caveats: &Caveats,
    command: &str,
    git_tool: Option<&dyn GitTool>,
) -> String {
    dispatch_gated(session, workspace, caveats, command, git_tool, None).await
}

/// [`dispatch`] with an operator to ask: a git op that destroys work is confirmed with
/// them first (#3145), and refused when there is none.
pub async fn dispatch_gated(
    session: &WorktreeSession,
    workspace: &Path,
    caveats: &Caveats,
    command: &str,
    git_tool: Option<&dyn GitTool>,
    gate: Option<&mut dyn PermissionGate>,
) -> String {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    let seen = round.clone();
    let args = serde_json::json!({"command": command}).to_string();
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            let mut message = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                serde_json::json!({"role":"assistant", "tool_calls":[{"id":"contract-call", "type":"function", "function":{"name":"run_command", "arguments":args}}]})
            } else { serde_json::json!({"role":"assistant", "content":"Fixture complete."}) };
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            if body["stream"] != true {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices":[{"message":message}]}));
            }
            let finish = if let Some(calls) = message["tool_calls"].as_array_mut() {
                calls[0]["index"] = serde_json::json!(0);
                "tool_calls"
            } else { "stop" };
            let chunk = serde_json::json!({"choices":[{"index":0,"delta":message}]});
            let done = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]});
            ResponseTemplate::new(200).set_body_raw(format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n"), "text/event-stream")
        }).mount(&server).await;
    let messages = vec![
        MemMessage::system("Execute the requested fixture command once."),
        MemMessage::user(command),
    ];
    let uri = server.uri();
    let mut context = ctx(&uri, &messages, caveats);
    context.workspace = workspace.to_str().unwrap();
    context.worktree_session = Some(session);
    context.git_tool = git_tool;
    // `&mut dyn` is invariant in its object lifetime; the coercion shortens it to the context's.
    context.permission_gate = gate.map(|gate| gate as &mut dyn PermissionGate);
    chat_complete(context, &mut NoMcp)
        .await
        .expect("scripted ChatCtx turn");
    let requests = server.received_requests().await.unwrap();
    requests
        .iter()
        .rev()
        .find_map(|req| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            body["messages"]
                .as_array()?
                .iter()
                .rev()
                .find(|message| message["role"] == "tool")?["content"]
                .as_str()
                .map(str::to_owned)
        })
        .expect("the model must receive the actual tool result")
}

fn ctx<'a>(server_uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    ChatCtx {
        kind: BackendKind::Openai,
        workspace: "",
        action_nudges: false,
        ..fixture::chat_ctx(server_uri, messages, caveats)
    }
}
