use super::*;

/// First round: empty content with token usage near the safe-context
/// ceiling → the loop must emit the overflow notice, trim, and retry.
/// Second round: a real answer.
struct OverflowThenRecover {
    probes: Arc<AtomicUsize>,
    /// Reported prompt size of the empty overflow round — set ≥85% of the
    /// safe-context window so the silent-overflow gate fires.
    overflow_prompt: u32,
}
impl Respond for OverflowThenRecover {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let overflow_prompt = self.overflow_prompt;
        if is_stream(req) {
            // Streams mirror the probe sequence: empty first, content after.
            if self.probes.load(Ordering::SeqCst) <= 1 {
                ndjson(&[serde_json::json!({
                    "message": {"content": ""}, "done": true,
                    "prompt_eval_count": overflow_prompt, "eval_count": 1
                })])
            } else {
                ndjson(&[
                    serde_json::json!({"message": {"content": "recovered "}, "done": false}),
                    serde_json::json!({
                        "message": {"content": "after trim"}, "done": true,
                        "prompt_eval_count": 12, "eval_count": 4
                    }),
                ])
            }
        } else {
            let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": ""},
                    "prompt_eval_count": overflow_prompt, "eval_count": 1,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered after trim"},
                    "prompt_eval_count": 12, "eval_count": 4,
                }))
            }
        }
    }
}

#[tokio::test]
async fn context_overflow_trims_and_retries_then_recovers() {
    let server = MockServer::start().await;
    // Derive the safe window from the live catalog: the exact active prompt and
    // expanded tool catalog must fit, so reserve ~311 tokens of headroom above
    // the catalog (a catalog-INDEPENDENT figure for the tiny messages/card) as
    // the window. The empty round's reported prompt is then pinned at 88% of
    // that window — comfortably ≥85% — so the silent-overflow gate keeps firing
    // as the catalog grows. (Reproduces the historical 4,096 window / ~3,600
    // report at today's catalog size.)
    // (Step 18.1: the check compares the largest single prompt against the
    // window — the old multi-round sum, 180 here, inflated past 85% after
    // two rounds on EVERY long turn, firing spurious overflow retries.)
    let safe_context = (builtin_catalog_tokens(PromptDisposition::Act)
        + prompt_read::response_repository_policy_tokens()
        + 311) as u32;
    let overflow_prompt = safe_context * 88 / 100; // ≥85% of the window
    let probes = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OverflowThenRecover {
            probes: probes.clone(),
            overflow_prompt,
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.safe_context = Some(safe_context);
    let (reply, streamed, usage, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert_eq!(
        probes.load(Ordering::SeqCst),
        2,
        "overflow must trigger exactly one trim-and-retry probe"
    );
    assert_eq!(reply, "recovered after trim");
    assert!(streamed);
    assert_eq!(
        usage
            .expect("accumulated usage survives the retry")
            .input_tokens,
        overflow_prompt,
        "largest single prompt across the overflowed + recovered rounds"
    );
}

/// Tool calls every round with a tiny trim threshold: the mid-loop
/// compression must fire — observable as the compaction marker (NOT the
/// old amputation placeholder) reaching the model. With no summarizer
/// injected, this is the static-fallback path (Step 18.4).
struct TrimObservingResponder {
    marker_seen: Arc<AtomicBool>,
    old_placeholder_seen: Arc<AtomicBool>,
}
impl Respond for TrimObservingResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let contains = |needle: &str| {
            body["messages"]
                .as_array()
                .map(|m| {
                    m.iter().any(|msg| {
                        msg["content"]
                            .as_str()
                            .map(|c| c.contains(needle))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        };
        if contains(SUMMARY_PREFIX) && contains("Summary generation was unavailable.") {
            self.marker_seen.store(true, Ordering::SeqCst);
        }
        if body.get("tools").is_some() && contains("earlier tool-call messages omitted") {
            self.old_placeholder_seen.store(true, Ordering::SeqCst);
        }
        if body.get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]}
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": "final after trim"}}))
        }
    }
}

#[tokio::test]
async fn mid_loop_compression_fires_when_message_list_grows() {
    let server = MockServer::start().await;
    let marker_seen = Arc::new(AtomicBool::new(false));
    let old_placeholder_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(TrimObservingResponder {
            marker_seen: marker_seen.clone(),
            old_placeholder_seen: old_placeholder_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 3;
    c.mid_loop_trim_threshold = 4;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(
        marker_seen.load(Ordering::SeqCst),
        "the static compaction marker must have reached the model mid-loop"
    );
    assert!(
        !old_placeholder_seen.load(Ordering::SeqCst),
        "the pre-18.4 amputation placeholder must never be emitted"
    );
    assert_eq!(reply, "final after trim");
}

/// OpenAI-path transcript regression for the 2026-07-16 amnesia failure:
/// after turn A completed, turn B grew large enough to compact mid-loop. The
/// continuation must point at the authoritative protected prompt B, never
/// rediscover the first user message (A) from retained conversation history.
struct OpenAiTaskAnchoringResponder {
    directive_seen: Arc<AtomicBool>,
    current_task_in_directive: Arc<AtomicBool>,
    historical_task_in_directive: Arc<AtomicBool>,
}

impl Respond for OpenAiTaskAnchoringResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let messages = body["messages"].as_array();
        let has_summary = messages.is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(SUMMARY_PREFIX))
            })
        });

        if has_summary {
            let directive = messages.and_then(|messages| {
                messages.iter().find_map(|message| {
                    message["content"]
                        .as_str()
                        .filter(|content| content.starts_with(compress::CONTINUATION_PREFIX))
                })
            });
            if let Some(directive) = directive {
                self.directive_seen.store(true, Ordering::SeqCst);
                if directive.contains("prompt_read") {
                    self.current_task_in_directive.store(true, Ordering::SeqCst);
                }
                if directive.contains(HISTORICAL_TASK) {
                    self.historical_task_in_directive
                        .store(true, Ordering::SeqCst);
                }
            }
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "continued the current task"}}]
            }));
        }

        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "definitely_not_a_real_tool",
                        "arguments": "{}"
                    }
                }]
            }}]
        }))
    }
}

const HISTORICAL_TASK: &str = "OLD TASK: probe every ambient MCP server and report its health";
const CURRENT_TASK: &str =
    "CURRENT TASK: modify the newt-agent source code and implement MCP management";

#[tokio::test]
async fn openai_mid_loop_compaction_anchors_the_current_turn_not_historical_prompt() {
    let server = MockServer::start().await;
    let directive_seen = Arc::new(AtomicBool::new(false));
    let current_task_in_directive = Arc::new(AtomicBool::new(false));
    let historical_task_in_directive = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiTaskAnchoringResponder {
            directive_seen: directive_seen.clone(),
            current_task_in_directive: current_task_in_directive.clone(),
            historical_task_in_directive: historical_task_in_directive.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user(HISTORICAL_TASK),
        MemMessage::assistant("Ten ambient MCP servers are reachable."),
        MemMessage::user(CURRENT_TASK),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = CURRENT_TASK;
    c.max_tool_rounds = 3;
    // The protected active-prompt pair and four historical messages fit on
    // round 0; the first tool exchange grows the list past seven, forcing the
    // regression's MID-TURN compaction.
    c.mid_loop_trim_threshold = 7;

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai loop should continue after compaction");

    assert_eq!(reply, "continued the current task");
    assert!(
        directive_seen.load(Ordering::SeqCst),
        "the post-compaction directive must reach the wire"
    );
    assert!(
        current_task_in_directive.load(Ordering::SeqCst),
        "the wire continuation must point back to the protected current prompt"
    );
    assert!(
        !historical_task_in_directive.load(Ordering::SeqCst),
        "the wire continuation must not relabel historical task A as current"
    );
}

#[test]
fn post_compaction_continuation_reanchors_on_ground_truth() {
    // Regression #1163 (2026-07-14 Opus session): after a mid-turn
    // compaction the model saw an empty `git diff`, concluded "the
    // worktree is clean, start fresh", DISOWNED its own branch+commit and
    // repeated finished work. The continuation directive must order a
    // ground-truth re-anchor and state the clean-tree≠no-work rule.
    let d = post_compaction_continuation(None, prompt_read::PromptReadContext::new(None, "", None));
    assert!(d.contains("re-anchor on ground truth"), "{d}");
    assert!(d.contains("git branch"), "{d}");
    assert!(
        d.contains("clean working tree does NOT mean no work happened"),
        "{d}"
    );
    assert!(d.contains("artifact_read {\"address\":\"root\"}"), "{d}");
    assert!(d.contains("do not repeat work"), "{d}");
}

#[test]
fn post_compaction_continuation_reinjects_the_full_plan_advance_not_rewrite() {
    // #1163 (F): the corporate-box repro showed the model REWRITE its own
    // plan post-compaction (dropping the implement steps for "stop
    // implementation"). The directive must re-inject the WHOLE plan (every
    // step + status) and order advance-not-rewrite, so the plan is an
    // anchor the model continues from.
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
    let ledger = SessionStepLedger::default();
    ledger.set_plan(&[
        "verify state".to_string(),
        "wire the lazy-emission guard".to_string(),
        "wire nudger profiles".to_string(),
    ]);
    ledger.advance(); // step 1 done, step 2 active
    let d = post_compaction_continuation(
        Some(&ledger),
        prompt_read::PromptReadContext::new(None, "", None),
    );
    // The full plan is present — including the not-yet-reached step.
    assert!(
        d.contains("wire the lazy-emission guard"),
        "active step: {d}"
    );
    assert!(
        d.contains("wire nudger profiles"),
        "future step present: {d}"
    );
    assert!(d.contains("verify state"), "done step present: {d}");
    // The advance-not-rewrite instruction.
    assert!(
        d.contains("NEVER to \\\n                 replace") || d.contains("NEVER to replace"),
        "{d}"
    );
    assert!(d.contains("advance"), "{d}");
    // No plan → no plan clause (and no panic).
    let empty =
        post_compaction_continuation(None, prompt_read::PromptReadContext::new(None, "", None));
    assert!(!empty.contains("active plan is below"), "{empty}");
}

#[test]
fn post_compaction_continuation_points_to_the_immutable_prompt_without_quoting_it() {
    // Regression #1163 (second repro, corporate-box Opus 2026-07-14):
    // compaction summarized the middle and the model re-derived a WRONG
    // task ("deliver a report") from the summary, dropping the operator's
    // actual instruction and confabulating. The exact instruction now lives
    // in a protected user-priority pair, so this directive points to its
    // immutable receipt rather than injecting a truncated user-role quote.
    let task = "make a plan, make a branch, write me a commit for each suggestion";
    let turn = crate::TurnPromptContext::ephemeral_operator("conv", task, task);
    let d = post_compaction_continuation(
        None,
        prompt_read::PromptReadContext::new(Some(&turn), task, None),
    );
    assert!(d.contains(&turn.active().id().to_string()), "{d}");
    assert!(d.contains(turn.active().model_digest()), "{d}");
    assert!(d.contains("prompt_read"), "{d}");
    assert!(!d.contains(task), "must not duplicate operator text: {d}");
    assert!(d.contains("do not narrow the task"), "{d}");
}

#[test]
fn post_compaction_uses_the_current_turn_task_not_the_first_conversation_prompt() {
    // Regression (2026-07-16 Opus transcript): turn A asked Newt to probe
    // ambient MCP servers. Turn B asked it to implement MCP management.
    // After a mid-turn compaction the harness rediscovered turn A with
    // `find()` and injected it as "the instruction for this turn", causing
    // the model to abandon the repository work and repeat the old probes.
    let old_task = "can you access any of the ambient MCP servers?";
    let current_task = "modify the newt-agent source code and implement MCP management";
    let turn = crate::TurnPromptContext::ephemeral_operator("conv", current_task, current_task);
    let prompt_context = prompt_read::PromptReadContext::new(Some(&turn), current_task, None);
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "you are newt"}),
        serde_json::json!({"role": "user", "content": old_task}),
        serde_json::json!({"role": "assistant", "content": "Ten servers are reachable."}),
        serde_json::json!({"role": "user", "content": current_task}),
        serde_json::json!({"role": "assistant", "content": "I will inspect the repository."}),
    ];
    let mut nudges = 1usize;

    apply_post_compaction_continuation(
        &mut messages,
        &mut nudges,
        CompressAction::Summarized,
        None,
        prompt_context,
        true,
        true,
    );

    let directive = messages
        .last()
        .and_then(|message| message["content"].as_str())
        .expect("post-compaction continuation");
    assert!(
        directive.contains(&turn.active().id().to_string()),
        "{directive}"
    );
    assert!(
        directive.contains(turn.active().model_digest()),
        "{directive}"
    );
    assert!(!directive.contains(current_task), "{directive}");
    assert!(
        !directive.contains(old_task),
        "the first conversation prompt must not be relabeled as current: {directive}"
    );
}

#[test]
fn post_compaction_refunds_rescue_budget_and_appends_one_directive() {
    let directive_count = |messages: &[serde_json::Value]| {
        messages
            .iter()
            .filter(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with(compress::CONTINUATION_PREFIX))
            })
            .count()
    };
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "you are a test"}),
        serde_json::json!({"role": "user", "content": "do the thing"}),
        serde_json::json!({
            "role": "user",
            "content": format!("{} stale directive", compress::CONTINUATION_PREFIX)
        }),
    ];
    let mut nudges = 1usize;
    let prompt_context = prompt_read::PromptReadContext::new(None, "do the thing", None);

    // Prune-only passes keep the corrective text: no refund, no anchor.
    apply_post_compaction_continuation(
        &mut messages,
        &mut nudges,
        CompressAction::Pruned,
        None,
        prompt_context,
        true,
        true,
    );
    assert_eq!(nudges, 1, "prune must not refund the rescue budget");
    assert_eq!(messages.len(), 3, "prune must not touch the directive");

    // Round 0 (a FRESH turn whose between-turn growth fired the pre-send
    // compaction): no directive — "You are mid-task … do not summarize"
    // would countermand the operator's brand-new ask sitting above it.
    apply_post_compaction_continuation(
        &mut messages,
        &mut nudges,
        CompressAction::Summarized,
        None,
        prompt_context,
        false,
        true,
    );
    assert_eq!(nudges, 1, "round 0 must not touch the rescue budget");
    assert_eq!(messages.len(), 3, "round 0 must not inject the directive");

    // A MID-TURN summarization refunds the budget, drops the stale
    // directive, and appends exactly one fresh act-now anchor as the last
    // user message.
    apply_post_compaction_continuation(
        &mut messages,
        &mut nudges,
        CompressAction::Summarized,
        None,
        prompt_context,
        true,
        true,
    );
    assert_eq!(nudges, 0, "summarization refunds the rescue budget");
    assert_eq!(directive_count(&messages), 1, "at most one directive alive");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "user");
    let content = last["content"].as_str().unwrap();
    assert!(
        content.starts_with(compress::CONTINUATION_PREFIX),
        "{content}"
    );
    assert!(content.contains("tool call"), "{content}");
    assert!(!content.contains("stale directive"), "{content}");
}
