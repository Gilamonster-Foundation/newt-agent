//! Output capacity must be visible before the model attempts a large rewrite.
use super::*;

/// The configured allowance reaches labelled harness guidance on every wire.
/// A pinned extraction is measured from authorized source bytes, not line count.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn output_budget_planning_tracks_cap_and_measures_pinned_range() {
    // PR #2860: the Anthropic fixture emits SSE; pin its process-wide mode
    // under the same lock as the JSON and streaming provider fixtures.
    let _env = crate::agentic::anthropic_loop_tests::test_env(true);
    let _settings = default_loop_settings();
    for wire in ["openai", "responses", "ollama", "anthropic"] {
        for (cap, lines, oversized) in [(1200, "lines 1-2", false), (2400, "lines 1-80", true)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(move |_: &Request| {
                    super::output_limit_recovery::reply(wire, 1, false, false)
                })
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("src")).unwrap();
            std::fs::write(
                dir.path().join("src/data.rs"),
                "x".repeat(100) + "\n" + &("x".repeat(100) + "\n").repeat(99),
            )
            .unwrap();
            let ledger = scheduled::SessionStepLedger::default();
            let pin = selected_target::PinnedTarget::revise(
                &serde_json::json!({
                    "path":"src/data.rs", "scope":format!("extract {lines}"),
                    "evidence":"read source", "revision":"initial"
                }),
                None,
            )
            .unwrap();
            scheduled::StepLedger::restore(
                &ledger,
                &scheduled::PlanSnapshot {
                    target: Some(pin),
                    steps: vec![],
                },
            );
            let uri = server.uri();
            let messages = msgs();
            let caveats = crate::confined_exec::workspace_confined_caveats(dir.path());
            let mut context = ctx(&uri, &messages, &caveats);
            context.kind = match wire {
                "ollama" => BackendKind::Ollama,
                "anthropic" => BackendKind::Anthropic,
                _ => BackendKind::Openai,
            };
            if wire == "responses" {
                context.openai_api = crate::OpenAiApi::Responses;
            }
            context.output_allowance = Some(cap);
            context.step_ledger = Some(&ledger);
            context.workspace = dir.path().to_str().unwrap();
            context.action_nudges = false;
            let result = chat_complete(context, &mut NoMcp).await.unwrap();
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let body = body_json(&requests[0]);
            let prompt = body.to_string();
            assert!(
                prompt.contains(&format!("Per-reply output budget: {cap} tokens")),
                "{wire}: {prompt}"
            );
            assert!(
                prompt.contains(&format!("at most {} tokens", cap / 4)),
                "{wire}: {prompt}"
            );
            assert!(
                prompt.contains("copy_from")
                    && prompt.contains("start_line")
                    && prompt.contains("end_line")
            );
            assert_eq!(
                prompt.contains("Planned extraction exceeds"),
                oversized,
                "{wire}: {prompt}"
            );
            let field = if wire == "responses" {
                "input"
            } else {
                "messages"
            };
            for message in body[field].as_array().unwrap() {
                if message.to_string().contains("Per-reply output budget") {
                    assert_ne!(message["role"], "assistant");
                }
            }
            assert!(
                !result.0.contains("Per-reply output budget"),
                "guidance is not assistant speech"
            );
        }
    }
}
