//! An explicit plan exit yields to approval instead of spending more rounds.
use super::*;
use crate::agentic::plan_mode::{PlanDraft, PlanDraftSink, PlanModeControl};
use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};

#[derive(Default)]
struct ApprovalControl {
    active: AtomicBool,
    requested: AtomicBool,
    reject_exit: bool,
}

impl PlanModeControl for ApprovalControl {
    fn is_plan_mode(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn set_plan_mode(&self, active: bool) -> Result<(), String> {
        self.active.store(active, Ordering::Release);
        Ok(())
    }

    fn request_exit(&self) -> Result<(), String> {
        if self.reject_exit {
            return Err("approval route unavailable".into());
        }
        self.requested.store(true, Ordering::Release);
        Ok(())
    }

    fn exit_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    fn take_exit_requested(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }
}

#[derive(Default)]
struct DraftSlot(Mutex<Option<PlanDraft>>);

impl PlanDraftSink for DraftSlot {
    fn save_draft(&self, markdown: String) -> Result<u32, String> {
        let mut slot = self.0.lock().unwrap();
        let revision = slot.as_ref().map_or(1, |draft| draft.revision + 1);
        *slot = Some(PlanDraft { revision, markdown });
        Ok(revision)
    }

    fn latest_draft(&self) -> Option<PlanDraft> {
        self.0.lock().unwrap().clone()
    }
}

/// Plan-before-act, the Plan-disposition half: a turn that may not act, whose
/// multi-step plan was recorded this turn, hands off to the operator after
/// one idle round; a round that added evidence keeps going, an Act turn is
/// never exited here (the executor arm owns that), and a plan carried over
/// from an earlier turn asks nothing.
#[test]
fn a_plan_turn_whose_plan_is_recorded_hands_off_after_one_idle_round() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
    let ledger = SessionStepLedger::default();
    let before = ledger.snapshot();
    ledger.set_plan(&["inspect".to_string(), "repair".to_string()]);
    let facts = |disposition, progressed, start| PlanRoundFacts {
        disposition,
        step_ledger: Some(&ledger as &dyn StepLedger),
        plan_at_turn_start: start,
        progressed,
    };

    let control = ApprovalControl::default();
    let mut reason = None;
    let mut slot = Some(&mut reason);
    let kept_going = pending_plan_approval_handoff(
        Some(&control),
        facts(PromptDisposition::Plan, true, &before),
        None,
        None,
        &mut slot,
    )
    .unwrap();
    assert!(
        kept_going.is_none(),
        "a round with new evidence keeps the turn"
    );
    assert!(!control.exit_requested());

    let handed_off = pending_plan_approval_handoff(
        Some(&control),
        facts(PromptDisposition::Plan, false, &before),
        None,
        None,
        &mut slot,
    )
    .unwrap();
    assert!(
        handed_off.is_some(),
        "an idle round with the plan recorded ends the turn"
    );
    assert!(control.exit_requested(), "requested on the model's behalf");
    assert_eq!(
        slot.as_deref().cloned(),
        Some(Some(crate::TurnEndReason::AwaitingOperator))
    );

    let act = ApprovalControl::default();
    assert!(pending_plan_approval_handoff(
        Some(&act),
        facts(PromptDisposition::Act, false, &before),
        None,
        None,
        &mut None,
    )
    .unwrap()
    .is_none());
    assert!(
        !act.exit_requested(),
        "an Act turn is the executor arm's to pause"
    );

    let carried = ledger.snapshot();
    let later = ApprovalControl::default();
    assert!(pending_plan_approval_handoff(
        Some(&later),
        facts(PromptDisposition::Plan, false, &carried),
        None,
        None,
        &mut None,
    )
    .unwrap()
    .is_none());
    assert!(
        !later.exit_requested(),
        "a plan from an earlier turn asks nothing"
    );
}

fn approval_response(wire: &str, first: bool) -> ResponseTemplate {
    let calls = [
        ("enter_plan_mode", serde_json::json!({})),
        (
            "render_report",
            serde_json::json!({"title":"Refactor plan","body":"Extract one cohesive module, then run its tests."}),
        ),
        ("exit_plan_mode", serde_json::json!({})),
        (
            "write_file",
            serde_json::json!({"path":"unapproved.txt","content":"must remain denied"}),
        ),
    ];
    let value = match wire {
        "ollama" => {
            let calls: Vec<_> = calls
                .iter()
                .map(|(name, args)| serde_json::json!({"function":{"name":name,"arguments":args}}))
                .collect();
            serde_json::json!({"message":{"role":"assistant","content":if first { "" } else { "Done." },
                "tool_calls":if first { calls } else { vec![] }},"done":true,
                "prompt_eval_count":10,"eval_count":5})
        }
        "anthropic" => {
            let content: Vec<_> = if first {
                calls.iter().enumerate().map(|(i, (name, args))| {
                    serde_json::json!({"type":"tool_use","id":format!("call_{i}"),"name":name,"input":args})
                }).collect()
            } else {
                vec![serde_json::json!({"type":"text","text":"Done."})]
            };
            serde_json::json!({"id":"msg_1","type":"message","role":"assistant","model":"test-model",
                "stop_reason":if first { "tool_use" } else { "end_turn" },"content":content,
                "usage":{"input_tokens":10,"output_tokens":5}})
        }
        "responses" => {
            let output: Vec<_> = if first {
                calls
                    .iter()
                    .enumerate()
                    .map(|(i, (name, args))| {
                        serde_json::json!({"type":"function_call","id":format!("fc_{i}"),
                        "call_id":format!("call_{i}"),"name":name,"arguments":args.to_string()})
                    })
                    .collect()
            } else {
                vec![
                    serde_json::json!({"type":"message","id":"msg_1","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"Done.","annotations":[]}]}),
                ]
            };
            serde_json::json!({"id":"resp_1","status":"completed","model":"test-model","output":output,
                "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}})
        }
        _ => {
            let calls: Vec<_> = calls
                .iter()
                .enumerate()
                .map(|(i, (name, args))| {
                    serde_json::json!({"id":format!("call_{i}"),"type":"function",
                    "function":{"name":name,"arguments":args.to_string()}})
                })
                .collect();
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":if first { "" } else { "Done." },
                "tool_calls":if first { calls } else { vec![] }},"finish_reason":if first { "tool_calls" } else { "stop" }}],
                "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})
        }
    };
    ResponseTemplate::new(200).set_body_json(value)
}

async fn check_approval_handoff(wire: &'static str, smart: bool, reject_exit: bool) {
    let _lock = env_lock().await;
    let _stream = EnvVar::set("NEWT_ANTHROPIC_STREAM", "off");
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let served = requests.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let first = served.fetch_add(1, Ordering::SeqCst) == 0;
            if !first && wire == "openai" && is_stream(request) {
                sse_replay("Done.")
            } else {
                approval_response(wire, first)
            }
        })
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().to_string_lossy().into_owned();
    let control = ApprovalControl {
        reject_exit,
        ..Default::default()
    };
    let draft = DraftSlot::default();
    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let harness = crate::agentic::smart_harness::SmartHarness::new(
        agent_harness::Session::new(crate::test_guard::unbudgeted_session_config()).unwrap(),
        Arc::new(|_| Box::pin(async { Ok(("\"answer\"".to_string(), None)) })),
        Default::default(),
    )
    .unwrap();
    let (uri, messages, caveats) = (server.uri(), msgs(), Caveats::top());
    let mut context = ctx(&uri, &messages, &caveats);
    context.workspace = &root;
    context.task = "Refactor the module.";
    context.kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    context.max_tool_rounds = 2;
    context.action_nudges = false;
    context.smart_harness = smart.then_some(&harness);
    context.plan_mode_control = Some(&control);
    context.step_ledger = Some(&ledger);
    context.plan_draft_sink = Some(&draft);
    let (mut reason, mut at_cap, mut events) = (None, false, Vec::new());
    context.end_reason = Some(&mut reason);
    context.round_cap_hit = Some(&mut at_cap);
    context.tool_events = Some(&mut events);
    let result = if wire == "responses" {
        openai_responses_complete(context, &mut NoMcp).await
    } else {
        chat_complete(context, &mut NoMcp).await
    }
    .unwrap();

    assert!(
        control.is_plan_mode(),
        "the loop must never grant approval itself"
    );
    assert!(!workspace.path().join("unapproved.txt").exists());
    assert_eq!(events.len(), 4, "record every call in the completed batch");
    assert!(!events[3].ok, "a post-exit write remains clamped");
    assert!(draft
        .latest_draft()
        .unwrap()
        .markdown
        .contains("Extract one cohesive module"));
    assert!(!at_cap, "an operator handoff is not round-cap exhaustion");
    if reject_exit {
        assert!(!control.take_exit_requested());
        assert_ne!(reason, Some(crate::TurnEndReason::AwaitingOperator));
        assert!(
            requests.load(Ordering::SeqCst) > 1,
            "a failed request must not invent a pending approval"
        );
    } else {
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "yield immediately after the approval-request batch: {wire}, smart={smart}"
        );
        assert_eq!(reason, Some(crate::TurnEndReason::AwaitingOperator));
        assert!(
            result.0.contains("approval"),
            "the result identifies the handoff"
        );
        assert_eq!(
            result.2,
            Some(crate::TokenUsage {
                input_tokens: 10,
                output_tokens: 5
            })
        );
        assert!(
            control.take_exit_requested(),
            "the caller must still receive the request"
        );
        assert!(
            !control.take_exit_requested(),
            "approval request is consumed exactly once"
        );
    }
}

macro_rules! approval_test {
    ($name:ident, $wire:literal, $smart:literal) => {
        #[tokio::test]
        async fn $name() {
            check_approval_handoff($wire, $smart, false).await;
        }
    };
}

approval_test!(ollama_plan_exit_yields, "ollama", false);
approval_test!(smart_ollama_plan_exit_yields, "ollama", true);
approval_test!(openai_plan_exit_yields, "openai", false);
approval_test!(smart_openai_plan_exit_yields, "openai", true);
approval_test!(anthropic_plan_exit_yields, "anthropic", false);
approval_test!(smart_anthropic_plan_exit_yields, "anthropic", true);
approval_test!(responses_plan_exit_yields, "responses", false);
approval_test!(smart_responses_plan_exit_yields, "responses", true);

#[tokio::test]
async fn rejected_plan_exit_does_not_invent_an_operator_handoff() {
    check_approval_handoff("openai", true, true).await;
}
