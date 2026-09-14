use super::*;

fn mid_sized_pair_task(label: &str) -> String {
    format!("{label} {}", "x".repeat(6_000))
}

#[test]
fn accepted_prompt_cannot_raise_budget_past_declared_ceiling() {
    assert_eq!(capped_accepted_prompt_tokens(61_221, Some(54_394)), 54_394);
    assert_eq!(capped_accepted_prompt_tokens(8_734, None), 8_734);
}

#[test]
fn authoritative_zero_input_budget_is_not_erased() {
    assert_eq!(authoritative_request_budget(Some(0), true, None), Some(0));
    assert_eq!(authoritative_request_budget(Some(0), false, None), None);
}

#[test]
fn accepted_prompt_proof_suppresses_count_fallback_only_while_it_covers_current() {
    assert!(count_guard_has_headroom(16_261, None, Some(23_799)));
    assert!(!count_guard_has_headroom(24_000, None, Some(23_799)));
    assert!(count_guard_has_headroom(24_000, Some(800_000), None));
}

/// Prove the regression fixture isolates the live-tail duplicate — the
/// protected recovery copy and schemas fit, but the irreducible complete
/// request (recovery copy + newest user presentation) does not — and
/// RETURN the `safe_context` budget the run should use.
///
/// The budget is DERIVED from the live catalog, not pinned: both `one_copy`
/// (protected head + advertised schemas) and `complete` (+ the duplicated
/// live-tail presentation) already track the catalog, and the gap between
/// them is one catalog-independent ~1.5k-token task copy. Sizing the budget
/// at their midpoint keeps `one_copy <= budget < complete` under any catalog
/// growth, so the fixture always exercises "one copy fits, the irreducible
/// pair does not → refuse". Returning it makes the guard below and the
/// actual `chat_complete` run agree on the same number.
fn mid_sized_pair_budget(task: &str, responses_wire: bool) -> usize {
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "base policy"}),
        serde_json::json!({"role": "user", "content": task}),
    ];
    prompt_read::ensure_active_prompt_card(
        &mut messages,
        prompt_read::PromptReadContext::new(None, task, None),
        None,
    );
    let head = protected_prompt_head_len(&messages, prompt_read::ACTIVE_PROMPT_PREFIX);
    let chat_tools = merged_tool_definitions(
        &NoMcp, false, false, false, None, false, false, false, false, false, false, false, false,
    );
    let tools = if responses_wire {
        serde_json::Value::Array(tools_to_responses(&chat_tools))
    } else {
        chat_tools
    };
    let estimation = crate::tokens::TokenEstimation::default();
    let one_copy = estimate_request_tokens(&messages[..head], Some(&tools), estimation);
    let complete = estimate_request_tokens(&messages, Some(&tools), estimation);
    // Strictly between one_copy and complete (their gap is the ~1.5k-token
    // live-tail task copy), so one protected copy fits but the pair cannot.
    let budget = (one_copy + complete) / 2;
    assert!(
        one_copy <= budget,
        "fixture invalid: one protected copy needs {one_copy} tokens, budget {budget}"
    );
    assert!(
        complete > budget,
        "fixture invalid: the irreducible pair needs {complete} tokens, budget {budget}"
    );
    budget
}

fn assert_irreducible_refusal(error: &anyhow::Error) {
    let message = error.to_string();
    assert!(
        message.contains("refusing before inference dispatch"),
        "{message}"
    );
    assert!(
        message.contains("operator prompt was not truncated"),
        "{message}"
    );
}

#[tokio::test]
async fn ollama_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
    let server = MockServer::start().await;
    let task = format!("OLLAMA-GIANT {}", "x".repeat(20_000));
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let error = chat_complete(
        hard_budget_ctx(
            &server.uri(),
            &messages,
            &caveats,
            &task,
            BackendKind::Ollama,
        ),
        &mut NoMcp,
    )
    .await
    .expect_err("giant exact prompt is irreducible");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn openai_chat_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
    let server = MockServer::start().await;
    let task = format!("OPENAI-CHAT-GIANT {}", "x".repeat(20_000));
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let error = openai_chat_complete(
        hard_budget_ctx(
            &server.uri(),
            &messages,
            &caveats,
            &task,
            BackendKind::Openai,
        ),
        &mut NoMcp,
    )
    .await
    .expect_err("giant exact prompt is irreducible");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn responses_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
    let server = MockServer::start().await;
    let task = format!("RESPONSES-GIANT {}", "x".repeat(20_000));
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let error = openai_responses_complete(
        hard_budget_ctx(
            &server.uri(),
            &messages,
            &caveats,
            &task,
            BackendKind::Openai,
        ),
        &mut NoMcp,
    )
    .await
    .expect_err("giant exact prompt is irreducible");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn ollama_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("OLLAMA-MID-PAIR");
    let budget = mid_sized_pair_budget(&task, false);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Ollama);
    ctx.safe_context = Some(budget as u32);
    let error = chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the two irreducible prompt presentations exceed the window");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn openai_chat_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("OPENAI-CHAT-MID-PAIR");
    let budget = mid_sized_pair_budget(&task, false);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = Some(budget as u32);
    let error = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the two irreducible prompt presentations exceed the window");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn openai_chat_declared_num_ctx_is_a_local_refusal_budget() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("OPENAI-CHAT-NUM-CTX");
    let budget = mid_sized_pair_budget(&task, false);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.num_ctx = Some(((budget * 100).div_ceil(80)) as u32);

    let error = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the declared local window must refuse the irreducible request");

    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn openai_chat_output_reserve_tightens_declared_window_before_dispatch() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("OPENAI-CHAT-OUTPUT-RESERVE");
    let budget = mid_sized_pair_budget(&task, false);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.cognition = Some(crate::role_profile::Cognition::Contemplating);
    ctx.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        ..Default::default()
    };
    let context_window = budget + 16_000;
    assert!(
        context_window * ctx.input_ceiling_pct as usize / 100 > budget,
        "fixture must be tightened by output reserve, not percentage"
    );
    ctx.num_ctx = Some(context_window as u32);

    let error = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the 16K output reserve must refuse the irreducible input");

    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn responses_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("RESPONSES-MID-PAIR");
    let budget = mid_sized_pair_budget(&task, true);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = Some(budget as u32);
    let error = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the two irreducible prompt presentations exceed the window");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn responses_refuses_locally_when_a_configured_window_cannot_fit() {
    // #1526 (invariant #4): a CONFIGURED context window is a local safety
    // limit even though it is never sent on the Responses wire. A window too
    // small to hold the irreducible request must be refused PRE-DISPATCH —
    // no request reaches the provider — rather than relying on a reactive
    // 400 or a silent truncation. (The previous contract wrongly let this
    // sail through; that assertion is now reversed.)
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0) // nothing may be dispatched
        .mount(&server)
        .await;

    let task = "a normal Responses request";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    // A 1-token window leaves zero input capacity → local refusal.
    ctx.num_ctx = Some(1);

    openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a 1-token configured window cannot fit the request");
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn responses_irreducible_request_refuses_before_the_proactive_guard() {
    // The B3 proactive guard sits BEHIND the pre-loop irreducible preflight: a
    // request whose protected head + newest live user alone dwarf the budget can
    // never be helped by compaction (the newest user is protected), so it is
    // refused BEFORE the round loop — the guard never runs. This pins that
    // fail-closed path to its DISTINGUISHING pre-loop message ("the operator
    // prompt was not truncated"), distinct from the in-loop preflight's "function
    // outputs were not truncated". (Honesty note per adversarial review: this
    // exercises the pre-existing irreducible gate, NOT B3's proactive path — B3's
    // guard is best-effort and DELEGATES the refusal to the authoritative
    // preflight, so there is no B3-specific fail-closed behavior to assert here.
    // B3's own new behavior — proactive compaction that lets an over-budget
    // request SUCCEED — is covered by responses_proactively_compacts_* /
    // responses_final_summary_is_proactively_compacted.)
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "UNREACHED"}]}]
        })))
        .expect(0)
        .mount(&server)
        .await;

    let task = format!("IRREDUCIBLE {}", "z".repeat(40_000));
    let messages = vec![MemMessage::system("base policy"), MemMessage::user(&task)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = Some(512);
    ctx.num_ctx = Some(512);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 1;

    let err = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("an irreducible over-budget request fails closed, never dispatched");
    let msg = err.to_string();
    assert!(
        msg.contains("operator prompt was not truncated"),
        "expected the PRE-LOOP irreducible refusal (distinct from the in-loop \
             preflight message), got: {msg}"
    );
    // `.expect(0)` verified on MockServer drop: ZERO inference dispatches.
}

struct GiantPromptReadResponder {
    openai: bool,
}

impl Respond for GiantPromptReadResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.openai {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_prompt_read",
                        "type": "function",
                        "function": {
                            "name": "prompt_read",
                            "arguments": "{\"address\":\"previous\"}"
                        }
                    }]
                }}]
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {
                        "name": "prompt_read",
                        "arguments": {"address": "previous"}
                    }
                }]}
            }))
        }
    }
}

fn giant_previous_prompt_context(
    store: &SessionPromptStore,
    conversation_id: &str,
    task: &str,
) -> crate::TurnPromptContext {
    let giant = format!("GIANT PREVIOUS OPERATOR PROMPT\n{}", "z\n".repeat(25_000));
    store
        .begin_prompt(
            conversation_id,
            crate::NewPrompt::operator(giant.as_bytes(), giant.as_bytes()),
        )
        .unwrap();
    store
        .begin_prompt(
            conversation_id,
            crate::NewPrompt::operator(task.as_bytes(), task.as_bytes()),
        )
        .unwrap()
}

#[tokio::test]
async fn ollama_giant_prompt_read_result_refuses_before_second_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(GiantPromptReadResponder { openai: false })
        .mount(&server)
        .await;
    let task = "re-read the prior prompt, then explain it";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let prompt_store = SessionPromptStore::default();
    let turn = giant_previous_prompt_context(&prompt_store, "ollama-prompt-read", task);
    let source = prompt_store.source("ollama-prompt-read");
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = Some(8_000);
    ctx.max_tool_rounds = 2;

    let error = chat_complete_with_prompt(ctx, Some(&turn), Some(&source), &mut NoMcp)
        .await
        .expect_err("the giant exact prompt_read result must block the next request");
    let message = error.to_string();
    assert!(
        message.contains("complete inference request needs"),
        "{message}"
    );
    assert!(
        message.contains("tool results were not truncated"),
        "{message}"
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    assert_eq!(requests.len(), 1, "no over-budget second dispatch");
}

#[tokio::test]
async fn openai_chat_giant_prompt_read_result_refuses_before_second_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(GiantPromptReadResponder { openai: true })
        .mount(&server)
        .await;
    let task = "re-read the prior prompt, then explain it";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let prompt_store = SessionPromptStore::default();
    let turn = giant_previous_prompt_context(&prompt_store, "openai-prompt-read", task);
    let source = prompt_store.source("openai-prompt-read");
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = Some(8_000);
    ctx.max_tool_rounds = 2;

    let error = openai_chat_complete_with_prompt(ctx, Some(&turn), Some(&source), &mut NoMcp)
        .await
        .expect_err("the giant exact prompt_read result must block the next request");
    let message = error.to_string();
    assert!(
        message.contains("complete inference request needs"),
        "{message}"
    );
    assert!(
        message.contains("tool results were not truncated"),
        "{message}"
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    let requests = super::chat_recovery::openai_generation_requests(requests);
    assert_eq!(requests.len(), 1, "no over-budget second dispatch");
}

#[tokio::test]
async fn responses_refuses_giant_function_output_before_second_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{
                "type": "function_call",
                "call_id": "call_huge",
                "name": "read_file",
                "arguments": "{\"path\":\"huge.txt\"}"
            }],
            "usage": {"input_tokens": 100, "output_tokens": 10}
        })))
        .mount(&server)
        .await;

    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("huge.txt"), "x".repeat(64_000)).unwrap();
    let task = "read the large fixture and report what it contains";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.workspace = workspace.path().to_str().unwrap();
    ctx.safe_context = Some(8_000);
    ctx.max_tool_rounds = 2;

    let error = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("giant function output must block the next request");
    // #1528 B3: the fresh giant tool output is the newest protected item, so the
    // proactive guard's best-effort compaction cannot reduce it; the authoritative
    // preflight then refuses. The headless error CHAIN (`{:#}`) carries BOTH the
    // preflight refusal (root) AND the attached proactive-compaction reason
    // (item 4) — so structured callers see the refusal was preceded by a real,
    // failed compaction attempt, not a naive over-budget send.
    let chain = format!("{error:#}");
    assert!(chain.contains("Responses request needs"), "{chain}");
    assert!(
        chain.contains("function outputs were not truncated"),
        "{chain}"
    );
    assert!(
        chain.contains("proactive compaction"),
        "the fail-closed error chain must attach the B3 compaction reason: {chain}"
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    assert_eq!(
        requests.len(),
        1,
        "the first tool call may dispatch, but its giant output must never be resent"
    );
}

/// Refusal text from dispatching a hopelessly large prompt to the loop that
/// owns `path_str`, at a declared 32,768 window, with a mock that must
/// receive nothing.
async fn refused_budget_message(path_str: &str, output_allowance: Option<u32>) -> String {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(path_str))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let task = format!("OUTPUT-ALLOWANCE {}", "x".repeat(200_000));
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let kind = if path_str == "/api/chat" {
        BackendKind::Ollama
    } else {
        BackendKind::Openai
    };
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, kind);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = Some(32_768);
    ctx.cognition = Some(crate::role_profile::Cognition::Contemplating);
    ctx.output_allowance = output_allowance;
    ctx.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        ..Default::default()
    };
    let result = match path_str {
        "/v1/chat/completions" => openai_chat_complete(ctx, &mut NoMcp).await,
        "/v1/responses" => openai_responses_complete(ctx, &mut NoMcp).await,
        _ => chat_complete(ctx, &mut NoMcp).await,
    };
    let error = result.expect_err("the prompt cannot fit any budget");
    assert_no_requests(&server).await;
    error.to_string()
}

/// #2312 (A4): the output allowance is resolved ONCE and every surface that
/// decides or reports the input budget agrees on it — the generation policy,
/// the percentage/window ceiling, the Responses budget state, the public
/// reporting seam on both OpenAI wires, and the number each loop refuses
/// against. Guards the #1534 class: threading an override into the policy
/// alone leaves admission on the cognition table while the wire changes.
/// The `None` row is the twin: without an override every surface falls back
/// to the Contemplating table's 16,000 (16,768 input).
///
/// EXCLUDED surface, a known discrepancy: the Anthropic loop. With no
/// allowance it reserves nothing locally while sending the 8,192 wire default
/// as `max_tokens`; this PR keeps that default unchanged and tracks the fix
/// separately. With an explicit allowance it reserves and sends the same value.
#[tokio::test]
async fn output_allowance_resolves_once_across_every_budget_surface() {
    use crate::agentic::generation_policy::GenerationPolicy;
    use crate::role_profile::Cognition;
    let capability = crate::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        ..Default::default()
    };
    let scope = crate::model_card::ReasoningReplayScope::Never;
    // (explicit allowance, resolved allowance, input budget, Ollama budget).
    // Ollama has no cognition table, so only an explicit allowance reserves.
    for (explicit, resolved, budget, ollama_budget) in [
        (Some(20_000), 20_000, 12_768, 12_768),
        (None, 16_000, 16_768, 26_214),
    ] {
        let policy =
            GenerationPolicy::resolve(Some(Cognition::Contemplating), explicit, capability, scope);
        assert_eq!(policy.output_allowance, Some(resolved), "{explicit:?}");
        assert_eq!(policy.max_output_tokens, Some(resolved), "{explicit:?}");
        assert_eq!(
            num_ctx_input_ceiling(Some(32_768), 80, policy.output_allowance),
            Some(budget),
            "{explicit:?}"
        );
        assert_eq!(
            resolve_responses_budget(
                Some(32_768),
                None,
                None,
                None,
                80,
                Some(Cognition::Contemplating),
                explicit
            )
            .actionable_input_budget(),
            Some(budget),
            "{explicit:?}"
        );
        for api in [
            crate::OpenAiApi::ChatCompletions,
            crate::OpenAiApi::Responses,
        ] {
            assert_eq!(
                initial_context_input_budget(
                    BackendKind::Openai,
                    api,
                    Some(32_768),
                    80,
                    Some(Cognition::Contemplating),
                    explicit,
                    capability,
                    scope,
                    None,
                    None,
                ),
                Some(budget as u32),
                "{explicit:?} {api:?}"
            );
        }

        let names = |n: usize| format!("authoritative {n}-token input budget");
        let chat = refused_budget_message("/v1/chat/completions", explicit).await;
        assert!(chat.contains(&names(budget)), "{explicit:?}: {chat}");
        let responses = refused_budget_message("/v1/responses", explicit).await;
        assert!(
            responses.contains(&names(budget)),
            "{explicit:?}: {responses}"
        );
        let ollama = refused_budget_message("/api/chat", explicit).await;
        assert!(
            ollama.contains(&names(ollama_budget)),
            "{explicit:?}: {ollama}"
        );
    }
}
