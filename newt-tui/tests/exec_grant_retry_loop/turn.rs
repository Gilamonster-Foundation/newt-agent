#[path = "../../../newt-core/tests/support/chat_ctx.rs"]
mod fixture;

use newt_core::{
    agentic::{openai_chat_complete, ChatCtx},
    commit_review::NoOpReaders,
    BackendKind, Caveats, MemMessage, NoMcp,
};
use newt_policy::PermissionGate;
use std::sync::atomic::AtomicBool;

pub async fn run(
    url: &str,
    workspace: &str,
    caveats: &Caveats,
    gate: &mut dyn PermissionGate,
    cancel: &AtomicBool,
) {
    let messages = vec![
        MemMessage::system("Execute the scripted tool calls."),
        MemMessage::user("Run the script"),
    ];
    openai_chat_complete(
        ChatCtx {
            model: "grant-retry-test",
            kind: BackendKind::Openai,
            api_key: Some("sk-test"),
            workspace,
            max_tool_rounds: 5,
            inference_timeout_secs: 120,
            reader_source: Some(&NoOpReaders),
            permission_gate: Some(gate),
            cancel: Some(cancel),
            ..fixture::chat_ctx(url, &messages, caveats)
        },
        &mut NoMcp,
    )
    .await
    .expect("scripted provider turn");
}
