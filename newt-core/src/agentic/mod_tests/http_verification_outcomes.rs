//! #2315: the opt-in result-aware verification policy (`NEWT_VERIFY_OUTCOMES`),
//! driven through real loops and the real shell.
//!
//! The task names its own check in backticks, so the gate detects it without
//! fixture files, and the model's `run_command` runs that exact command. These
//! ground the pure decision tests in `self_verify`: the class the decision
//! reads is the one PR1 records at the per-tool-result funnel.
use super::*;
use crate::agentic::smart_harness::{AdjudicationSettings, SmartHarness};
use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};

const FAILING_CHECK: &str = "sh -c 'exit 1'";
const PASSING_CHECK: &str = "sh -c 'exit 0'";

fn instruction(check: &str) -> String {
    format!("Fix the bug. You can run `{check}` to verify.")
}

/// One scripted step: a `run_command` call, or the final text `done`.
#[derive(Clone, Copy)]
enum Step {
    Run(&'static str),
    Done,
}

fn reply(wire: &str, step: Step) -> ResponseTemplate {
    let call = match step {
        Step::Run(command) => Some(("run_command", serde_json::json!({"command": command}))),
        Step::Done => None,
    };
    let value = match (wire, call) {
        ("ollama", Some((name, args))) => {
            serde_json::json!({"message":{"role":"assistant","content":"",
            "tool_calls":[{"function":{"name":name,"arguments":args}}]},"done":true})
        }
        ("ollama", None) => {
            serde_json::json!({"message":{"role":"assistant","content":"done"},"done":true})
        }
        ("anthropic", Some((name, args))) => serde_json::json!({"id":"msg_1","type":"message",
            "role":"assistant","model":"test-model","stop_reason":"tool_use",
            "content":[{"type":"tool_use","id":"call_1","name":name,"input":args}]}),
        ("anthropic", None) => serde_json::json!({"id":"msg_1","type":"message",
            "role":"assistant","model":"test-model","stop_reason":"end_turn",
            "content":[{"type":"text","text":"done"}],"usage":{"input_tokens":10,"output_tokens":5}}),
        ("responses", Some((name, args))) => serde_json::json!({"id":"resp_1","status":"completed",
            "output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":name,
            "arguments":args.to_string()}]}),
        ("responses", None) => serde_json::json!({"id":"resp_1","status":"completed",
            "model":"test-model","output":[{"type":"message","id":"msg_1","role":"assistant",
            "status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}]}),
        (_, Some((name, args))) => {
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":"",
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":name,
            "arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        (_, None) => {
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":"done"},
            "finish_reason":"stop"}]})
        }
    };
    ResponseTemplate::new(200).set_body_json(value)
}

struct Run {
    /// The turn's end reason, as it serializes on the trace line.
    reason: serde_json::Value,
    bodies: Vec<String>,
    /// The behavior signals the turn recorded for the solve trace.
    signals: Vec<observability::BehaviorSignal>,
}

/// The model runs `check` once, then answers `done` every time it is asked.
async fn run(wire: &str, smart: bool, outcomes: bool, check: &'static str) -> Run {
    run_script(wire, smart, outcomes, check, &[Step::Run(check)], None, 8).await
}

/// The model follows `script`, then answers `done` every time it is asked.
async fn run_script(
    wire: &str,
    smart: bool,
    outcomes: bool,
    check: &str,
    script: &[Step],
    cancel: Option<&std::sync::atomic::AtomicBool>,
    max_tool_rounds: usize,
) -> Run {
    let _lock = env_lock().await;
    let _self_verify = EnvVar::set("NEWT_SELF_VERIFY", "1");
    let _confined = EnvVar::unset("NEWT_DISABLE_OCAP");
    let _no_anthropic_stream = EnvVar::set("NEWT_ANTHROPIC_STREAM", "off");

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (wire_owned, script) = (wire.to_string(), script.to_vec());
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            let i = calls.fetch_add(1, Ordering::SeqCst);
            reply(&wire_owned, script.get(i).copied().unwrap_or(Step::Done))
        })
        .mount(&server)
        .await;
    let harness = SmartHarness::new(
        agent_harness::Session::new(Default::default()).unwrap(),
        Arc::new(|_| Box::pin(async { Ok(("\"answer\"".to_string(), None)) })),
        AdjudicationSettings::default(),
    )
    .unwrap();

    let ws = tempfile::TempDir::new().unwrap();
    let workspace = ws.path().to_string_lossy().into_owned();
    let task = instruction(check);
    let (uri, messages, caveats) = (server.uri(), msgs(), Caveats::top());
    let mut context = ctx(&uri, &messages, &caveats);
    context.workspace = &workspace;
    context.task = &task;
    context.max_tool_rounds = max_tool_rounds;
    context.verify_outcomes = outcomes;
    let mut obs = observability::SolveObservation::default();
    context.solve_obs = Some(&mut obs);
    context.smart_harness = smart.then_some(&harness);
    context.kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    context.cancel = cancel;
    let mut reason = None;
    context.end_reason = Some(&mut reason);
    if wire == "responses" {
        openai_responses_complete(context, &mut NoMcp).await
    } else {
        chat_complete(context, &mut NoMcp).await
    }
    .expect("the turn ends without an error");
    let bodies = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect();
    Run {
        reason: serde_json::to_value(reason).unwrap(),
        bodies,
        signals: obs.behavior_signals,
    }
}

/// The distinct repair ordinals the model was shown, e.g. `["1/3", "2/3"]`.
fn repair_ordinals(run: &Run) -> std::collections::BTreeSet<String> {
    run.bodies
        .iter()
        .flat_map(|body| {
            body.match_indices("verification repair ")
                .map(move |(at, m)| &body[at + m.len()..])
        })
        .filter_map(|rest| rest.split(')').next())
        .map(str::to_string)
        .collect()
}

/// A4/A10/A11: a failing check is repaired up to the declared allowance, then
/// the turn ends `repair_exhausted`. The ordinary gates and SmartHarness reach
/// the same decision (A12). Trap: a repair test whose check never ran; the
/// model's second request must carry the real failing result.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn a_failing_check_is_repaired_to_the_allowance_then_exhausts() {
    for (wire, smart) in [
        ("openai", false),
        ("anthropic", false),
        ("openai", true),
        ("anthropic", true),
        ("ollama", true),
        ("responses", true),
    ] {
        let run = run(wire, smart, true, FAILING_CHECK).await;
        assert!(
            run.bodies[1].contains("error: command exited 1"),
            "{wire} smart={smart}: the check really ran and failed"
        );
        assert_eq!(run.reason, "repair_exhausted", "{wire} smart={smart}");
        assert_eq!(
            repair_ordinals(&run),
            ["1/3", "2/3", "3/3"].map(String::from).into(),
            "{wire} smart={smart}: exactly the declared allowance"
        );
        // Finding 10: the trace signal bench analysis reads. One decision per
        // concluding answer: three repair nudges, then the stop.
        let decisions: Vec<_> = run
            .signals
            .iter()
            .filter_map(|signal| match signal {
                observability::BehaviorSignal::Verification {
                    decision,
                    repairs_used,
                    allowance,
                    report,
                    ..
                } => Some((decision.as_str(), *repairs_used, *allowance, report.basis)),
                _ => None,
            })
            .collect();
        assert_eq!(
            decisions,
            [
                (
                    "nudge",
                    0,
                    3,
                    crate::agentic::self_verify::StateBasis::MutationChain
                ),
                (
                    "nudge",
                    1,
                    3,
                    crate::agentic::self_verify::StateBasis::MutationChain
                ),
                (
                    "nudge",
                    2,
                    3,
                    crate::agentic::self_verify::StateBasis::MutationChain
                ),
                (
                    "repair_exhausted",
                    3,
                    3,
                    crate::agentic::self_verify::StateBasis::MutationChain
                ),
            ],
            "{wire} smart={smart}"
        );
    }
}

/// A1: the same run with the policy off keeps #1961's attempted-check contract
/// (the failing attempt satisfies the gate) and never mentions repair.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn with_the_policy_off_a_failing_attempt_still_satisfies_the_gate() {
    for (wire, smart) in [("openai", false), ("anthropic", false), ("responses", true)] {
        let run = run(wire, smart, false, FAILING_CHECK).await;
        assert_eq!(run.reason, "completed", "{wire} smart={smart}");
        assert!(repair_ordinals(&run).is_empty(), "{wire} smart={smart}");
    }
}

/// A7: a passing check that is still fresh ends the turn as completed, with no
/// verification nudge at all.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn a_fresh_passing_check_completes_without_a_nudge() {
    for (wire, smart) in [("openai", false), ("anthropic", false), ("responses", true)] {
        let run = run(wire, smart, true, PASSING_CHECK).await;
        assert_eq!(run.reason, "completed", "{wire} smart={smart}");
        assert!(
            !run.bodies
                .iter()
                .any(|body| body.contains("[loop-guidance] Before you finish")),
            "{wire} smart={smart}"
        );
        // Review F: the pass recorded a tree state and the conclusion decided on
        // it (basis `tree`), not on the mutation-chain fallback.
        let bases: Vec<_> = run
            .signals
            .iter()
            .filter_map(|signal| match signal {
                observability::BehaviorSignal::Verification {
                    decision, report, ..
                } => Some((decision.as_str(), report.basis)),
                _ => None,
            })
            .collect();
        assert_eq!(
            bases,
            [("accept", crate::agentic::self_verify::StateBasis::Tree)],
            "{wire} smart={smart}"
        );
    }
}

/// A8, and the measurement bias the tree basis exists to avoid: a read-only
/// command after a pass changes no bytes, so the pass stays current and the
/// turn completes. Twin: a shell write after the pass (the `sed -i` shape a
/// write-tool ledger would miss) makes it stale, the model
/// is told to re-run it, and a model that never does ends
/// `verification_incomplete`. Grounds `tree_state` in the real filesystem.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn a_pass_goes_stale_on_a_write_but_not_on_a_read_only_command() {
    let read_only = run_script(
        "openai",
        false,
        true,
        PASSING_CHECK,
        &[Step::Run(PASSING_CHECK), Step::Run("ls")],
        None,
        8,
    )
    .await;
    assert_eq!(read_only.reason, "completed");
    assert!(!read_only
        .bodies
        .iter()
        .any(|b| b.contains("workspace changed after it ran")));

    let written = run_script(
        "openai",
        false,
        true,
        PASSING_CHECK,
        &[
            Step::Run(PASSING_CHECK),
            Step::Run("sh -c 'echo changed > notes.txt'"),
        ],
        None,
        8,
    )
    .await;
    assert!(
        written
            .bodies
            .iter()
            .any(|b| b.contains("workspace changed after it ran")),
        "the write made the pass stale"
    );
    assert_eq!(written.reason, "verification_incomplete");
}

/// A6: an interrupt while the check runs ends the turn `cancelled`, never a
/// pass: the funnel records nothing for a call that did not return. SmartHarness
/// is the path that stamps `cancelled` headless (the ordinary loop leaves it to
/// the TUI).
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn cancelling_a_running_check_ends_cancelled_without_a_pass() {
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setter = flag.clone();
    let interrupt = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        setter.store(true, Ordering::SeqCst);
    });
    const SLOW: &str = "sh -c 'sleep 5'";
    let run = run_script(
        "openai",
        true,
        true,
        SLOW,
        &[Step::Run(SLOW)],
        Some(&flag),
        8,
    )
    .await;
    interrupt.await.unwrap();
    assert_eq!(run.reason, "cancelled");
    assert_eq!(
        run.bodies.len(),
        1,
        "no request after the interrupted check"
    );
}

/// Finding 3: the model fixes the code and re-runs the identical failing check.
/// The repeat-call guard's failure memo described the tree before the fix, so
/// it must not short-circuit the re-run: the turn completes on the new pass.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn an_identical_rerun_after_a_fix_is_not_blocked_by_the_repeat_guard() {
    const CHECK: &str = "sh -c 'test -f fixed'";
    for (wire, smart) in [
        ("openai", false),
        ("anthropic", false),
        ("ollama", true),
        ("responses", true),
    ] {
        let run = run_script(
            wire,
            smart,
            true,
            CHECK,
            &[Step::Run(CHECK), Step::Run("touch fixed"), Step::Run(CHECK)],
            None,
            8,
        )
        .await;
        assert_eq!(run.reason, "completed", "{wire} smart={smart}");
        assert!(
            !run.bodies.iter().any(|b| b.contains("You already called")),
            "{wire} smart={smart}: the guard short-circuited the re-run"
        );
    }
}

/// Finding 4 and review B: when the rounds run out with a failed check, a turn
/// that HAS a verification gate ends `repair_exhausted` (a scored attempt) and
/// records the reclassification in the trace. A loop with no gate (Ollama or
/// Responses without SmartHarness, receipt mode `off`) keeps `round_cap`.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn a_failure_at_the_round_limit_ends_repair_exhausted_not_round_cap() {
    let repair = [
        Step::Run(FAILING_CHECK),
        Step::Done,
        Step::Run(FAILING_CHECK),
    ];
    let tools_only = [Step::Run(FAILING_CHECK); 3];
    for (wire, smart, script, expected) in [
        ("openai", false, &repair[..], "repair_exhausted"),
        ("anthropic", false, &repair[..], "repair_exhausted"),
        ("openai", true, &repair[..], "repair_exhausted"),
        ("ollama", true, &repair[..], "repair_exhausted"),
        ("responses", true, &repair[..], "repair_exhausted"),
        ("ollama", false, &tools_only[..], "round_cap"),
        ("responses", false, &tools_only[..], "round_cap"),
    ] {
        let run = run_script(wire, smart, true, FAILING_CHECK, script, None, 3).await;
        assert_eq!(run.reason, expected, "{wire} smart={smart}");
        let stops = run
            .signals
            .iter()
            .filter(|signal| {
                matches!(signal, observability::BehaviorSignal::Verification { decision, .. }
                    if decision == "repair_exhausted")
            })
            .count();
        assert_eq!(
            stops,
            usize::from(expected == "repair_exhausted"),
            "{wire} smart={smart}: the cap-exit reclassification is traced"
        );
    }
}
