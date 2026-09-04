use super::*;
use newt_core::agentic::openai_chat_complete;
use newt_core::caveats::Caveats;
use newt_core::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ]
}

/// Grounds the parse-only recovery callback contract: cache ownership
/// remains in the observation hook exercised by the integration below.
#[test]
fn context_window_400_hook_returns_the_full_window() {
    let err = anyhow::anyhow!("prompt is too long: 42000 tokens > 32768 maximum");
    let recovered = recover_context_window_400(&err, "cw-hook-model", "2026-08-01");
    assert_eq!(recovered, Some(32_768));

    let vllm = anyhow::anyhow!(
            "This model's maximum context length is 32768 tokens. However, you requested 16000 output tokens and your prompt contains 20000 input tokens, for a total of 36000 tokens (20000 + 16000 = 36000 > 32768). Please reduce the length of the input prompt or the number of requested output tokens."
        );
    assert_eq!(
        recover_context_window_400(&vllm, "cw-hook-model", "2026-08-01"),
        Some(32_768),
    );
}

/// Regression for issue #223: a hard context-window 400 must NOT kill the
/// session. The loop parses the model's real limit from the error body,
/// tightens the budget, trims, retries, and returns a real answer — and
/// persists the discovered limit so future sessions start tightened.
///
/// Before the fix, the 400 propagated out of `with_backoff_notify(...).await?`
/// and the whole turn died with `error: inference endpoint 400: …`.
#[serial_test::serial(real_fs)]
#[test]
fn openai_loop_recovers_from_context_window_400() {
    struct CwResponder {
        calls: Arc<AtomicUsize>,
        final_answer: String,
    }
    impl Respond for CwResponder {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First dispatch overflows the context window using the
                // exact vLLM 0.19 output-plus-prompt validation wording.
                ResponseTemplate::new(400).set_body_string(
                        "This model's maximum context length is 1000000 tokens. However, you requested 16000 output tokens and your prompt contains 5960028 input tokens, for a total of 5976028 tokens (5960028 + 16000 = 5976028 > 1000000). Please reduce the length of the input prompt or the number of requested output tokens.",
                    )
            } else {
                // After trim+retry, answer with no tool calls so the loop ends.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": self.final_answer } }],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 2, "total_tokens": 102}
                }))
            }
        }
    }

    // Isolate cache persistence to a temp dir via the thread-local cache
    // override — NOT a global $HOME swap. The swap raced every HOME-reading
    // test in this binary (#507: ~20 tests intermittently failed writing
    // `~/.newt/...` when their thread saw this test's transient HOME). The
    // override is thread-local, so no other test thread is affected and no env
    // write guard is needed.
    let tmp = tempfile::tempdir().unwrap();
    probe::set_cache_dir_override(Some(tmp.path().to_path_buf()));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (result, calls_made, recovered_window, accepted_observed, persisted) = rt.block_on(async {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(CwResponder {
                calls: calls.clone(),
                final_answer: "recovered answer".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let today = "2026-08-01";
        let mut cap_cache = probe::load_cache();
        let mut recovered_window = None;
        let mut accepted_observed = false;
        let out = {
            let mut on_obs = |obs: newt_core::RoundObservation| {
                if matches!(obs, newt_core::RoundObservation::Accepted { .. }) {
                    accepted_observed = true;
                }
                if let newt_core::RoundObservation::ContextWindow400 { context_window } = obs {
                    recovered_window = Some(context_window);
                }
                let dirty = {
                    let entry = cap_cache
                        .entry(probe::cap_key(
                            newt_core::Serving::Multiplexer,
                            "",
                            "cw-test-model",
                        ))
                        .or_default();
                    probe::apply_observation(entry, &obs, today)
                };
                if dirty {
                    probe::save_cache(&cap_cache);
                }
            };
            openai_chat_complete(
                ChatCtx {
                    rewrites_history: true,
                    url: &server.uri(),
                    model: "cw-test-model",
                    kind: BackendKind::Openai,
                    emits_leading_reasoning: false,
                    api_key: Some("sk-test"),
                    messages: &messages,
                    task: "do the thing",
                    workspace: ".",
                    color: false,
                    markdown: false,
                    tool_offload: false,
                    spill_store: None,
                    disclosure: None,
                    compaction_store: None,
                    scratchpad: false,
                    scratchpad_store: None,
                    code_search: None,
                    where_is: None,
                    nav: None,
                    exposure: Default::default(),
                    experience_store: None,
                    step_ledger: None,
                    caveats: &caveats,
                    persona_tools: None,
                    cognition: None,
                    chat_completions_capability: Default::default(),
                    reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope::Never,
                    max_tool_rounds: 5,
                    narration_nudge_cap: 1,
                    action_nudges: true,
                    prompt_disposition: newt_core::agentic::PromptDisposition::Act,
                    prompt_intake: None,
                    workflow_grace_rounds: 0,
                    tool_output_lines: 20,
                    debug: false,
                    trace: false,
                    num_ctx: None,
                    input_ceiling_pct: 80,
                    low_budget_pct: 15,
                    connect_timeout_secs: 5,
                    inference_timeout_secs: 120,
                    mid_loop_trim_threshold: 40,
                    compaction_trigger_policy: newt_core::CompactionTriggerPolicy::HeadroomAware,
                    mid_loop_trim_tokens: None,
                    max_ok_input: None,
                    build_check_cmd: None,
                    safe_context: None,
                    // Parse-only recovery reports the hard window through the
                    // same observation owner as the successful retry.
                    recover_cw_400: Some(recover_context_window_400),
                    note_sink: None,
                    note_nudge: None,
                    recall_source: None,
                    memory_source: None,
                    summarizer: None,
                    compress_state: None,
                    tool_events: None,
                    phantom_reaches: None,
                    end_reason: None,
                    solve_obs: None,
                    permission_gate: None,
                    on_round_usage: Some(&mut on_obs),
                    estimate_ratio: None,
                    estimation: newt_core::TokenEstimation::default(),
                    summary_input_cap_floor_chars: 8_192,
                    exec_floor: None,
                    write_ledger: None,
                    attribution: None,
                    cancel: None,
                    live_tool_output: None,
                    git_tool: None,
                    crew_runner: None,
                    operating_mode_control: None,
                    plan_mode_control: None,
                    disposition_request_control: None,
                    steering: None,
                    completed_spill_renderer: None,
                },
                &mut Mcp::empty(),
            )
            .await
        };
        // Read the persisted facts after both the 400 and accepted retry.
        let persisted = probe::load_cache()
            .get(&probe::cap_key(
                newt_core::Serving::Multiplexer,
                "",
                "cw-test-model",
            ))
            .map(|e| (e.context_window, e.max_ok_input, e.safe_context));
        (
            out,
            calls.load(Ordering::SeqCst),
            recovered_window,
            accepted_observed,
            persisted,
        )
    });

    // Clear the thread-local cache override before any assertion can unwind.
    probe::set_cache_dir_override(None);

    let (reply, _streamed, _usage, _hallu) =
        result.expect("loop must recover from the 400, not propagate it");
    assert_eq!(reply, "recovered answer");
    assert!(
        calls_made >= 2,
        "expected at least one retry after the 400, got {calls_made} call(s)"
    );
    assert_eq!(recovered_window, Some(1_000_000));
    assert!(accepted_observed, "the successful retry must emit Accepted");
    // Persistence (issue #223 req 4): the full window and its generic 80%
    // caps survive the Accepted observation emitted by the retry.
    assert_eq!(
        persisted,
        Some((Some(1_000_000), Some(800_000), Some(800_000)))
    );
}
