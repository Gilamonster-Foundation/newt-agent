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

/// One scripted step: a `run_command` call, a batch the loop rejects before
/// executing any of it, or the final text `done`.
#[derive(Clone, Copy)]
enum Step {
    Run(&'static str),
    /// A real non-shell operation, retaining typed producer behavior.
    Tool(&'static str, &'static str),
    /// Multiple valid calls in one real model response.
    Tools(&'static [(&'static str, &'static str)]),
    /// A `run_command` of this command beside a call whose arguments are not a
    /// JSON object, so validation rejects the whole batch.
    RejectedBatch(&'static str),
    Done,
    Refusal,
    HttpError(u16),
}

fn rejected_batch(wire: &str, command: &str) -> ResponseTemplate {
    let run = serde_json::json!({ "command": command });
    let value = match wire {
        "anthropic" => serde_json::json!({"id":"msg_1","type":"message","role":"assistant",
            "model":"test-model","stop_reason":"tool_use","content":[
            {"type":"tool_use","id":"call_1","name":"run_command","input":run},
            {"type":"tool_use","id":"call_2","name":"run_command","input":"not an object"}]}),
        "ollama" => serde_json::json!({"message":{"role":"assistant","content":"","tool_calls":[
            {"function":{"name":"run_command","arguments":run}},
            {"function":{"name":"run_command","arguments":"not an object"}}]},"done":true}),
        "responses" => serde_json::json!({"id":"resp_1","status":"completed","output":[
            {"type":"function_call","id":"fc_1","call_id":"call_1","name":"run_command","arguments":run.to_string()},
            {"type":"function_call","id":"fc_2","call_id":"call_2","name":"run_command","arguments":"\"not an object\""}]}),
        _ => serde_json::json!({"choices":[{"message":{"role":"assistant","content":"",
            "tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"run_command",
                "arguments":run.to_string()}},
            {"id":"call_2","type":"function","function":{"name":"run_command",
                "arguments":"{\"command\":"}}]},"finish_reason":"tool_calls"}]}),
    };
    ResponseTemplate::new(200).set_body_json(value)
}

fn tool_batch(wire: &str, calls: &[(&str, &str)]) -> ResponseTemplate {
    let calls: Vec<_> = calls.iter().enumerate().map(|(i, (name, arguments))| {
        let args: serde_json::Value = serde_json::from_str(arguments).expect("fixture arguments");
        let id = format!("call_{i}");
        match wire {
            "anthropic" => serde_json::json!({"type":"tool_use","id":id,"name":name,"input":args}),
            "ollama" => serde_json::json!({"function":{"name":name,"arguments":args}}),
            "responses" => serde_json::json!({"type":"function_call","id":format!("fc_{i}"),"call_id":id,"name":name,"arguments":args.to_string()}),
            _ => serde_json::json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}),
        }
    }).collect();
    let value = match wire {
        "anthropic" => {
            serde_json::json!({"id":"msg_1","type":"message","role":"assistant","model":"test-model","stop_reason":"tool_use","content":calls})
        }
        "ollama" => {
            serde_json::json!({"message":{"role":"assistant","content":"","tool_calls":calls},"done":true})
        }
        "responses" => serde_json::json!({"id":"resp_1","status":"completed","output":calls}),
        _ => {
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":"","tool_calls":calls},"finish_reason":"tool_calls"}]})
        }
    };
    ResponseTemplate::new(200).set_body_json(value)
}

fn reply(wire: &str, step: Step) -> ResponseTemplate {
    if matches!(step, Step::Refusal) {
        let value = match wire {
            "anthropic" => serde_json::json!({"id":"msg_1","type":"message","role":"assistant",
                "model":"test-model","stop_reason":"refusal","content":[{"type":"text","text":"I decline this request."}]}),
            "responses" => serde_json::json!({"id":"resp_1","status":"completed","output":[
                {"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"I decline this request."}]}]}),
            _ => unreachable!(
                "typed refusal fixture only covers providers with an existing refusal branch"
            ),
        };
        return ResponseTemplate::new(200).set_body_json(value);
    }
    let call = match step {
        Step::Run(command) => Some(("run_command", serde_json::json!({"command": command}))),
        Step::Tool(name, args) => {
            Some((name, serde_json::from_str(args).expect("fixture arguments")))
        }
        Step::Tools(calls) => return tool_batch(wire, calls),
        Step::RejectedBatch(command) => return rejected_batch(wire, command),
        Step::Done => None,
        Step::Refusal => unreachable!(),
        Step::HttpError(status) => return ResponseTemplate::new(status),
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
    /// The loop reported that the turn ended at its round cap.
    at_cap: bool,
    answer: String,
    notes_bytes: Option<String>,
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
    run_turn(Turn {
        wire,
        smart,
        outcomes,
        check,
        script,
        cancel,
        max_tool_rounds,
        caveats: Caveats::top(),
        env: &[],
        workspace_task: None,
    })
    .await
}

/// One scripted turn, for a case that also narrows the session's authority or
/// pins env the shell reads.
struct Turn<'a> {
    wire: &'a str,
    smart: bool,
    outcomes: bool,
    check: &'a str,
    script: &'a [Step],
    cancel: Option<&'a std::sync::atomic::AtomicBool>,
    max_tool_rounds: usize,
    caveats: Caveats,
    /// Set under the env lock for the turn, after the defaults below.
    env: &'a [(&'static str, &'static str)],
    workspace_task: Option<(&'a std::path::Path, &'a str)>,
}

async fn run_turn(turn: Turn<'_>) -> Run {
    run_turn_configured(turn, |_| {}).await
}

async fn run_turn_configured(turn: Turn<'_>, configure: impl FnOnce(&mut ChatCtx<'_>)) -> Run {
    run_turn_with_auxiliary(turn, configure, None, None).await
}

async fn run_turn_with_auxiliary(
    turn: Turn<'_>,
    configure: impl FnOnce(&mut ChatCtx<'_>),
    auxiliary: Option<Arc<SummarizeFn>>,
    allowance: Option<&run_allowance::RunAllowance>,
) -> Run {
    let Turn {
        wire,
        smart,
        outcomes,
        check,
        script,
        cancel,
        max_tool_rounds,
        caveats,
        env,
        workspace_task,
    } = turn;
    let _lock = env_lock().await;
    let _self_verify = EnvVar::set("NEWT_SELF_VERIFY", "1");
    let _confined = EnvVar::unset("NEWT_DISABLE_OCAP");
    let _no_anthropic_stream = EnvVar::set("NEWT_ANTHROPIC_STREAM", "off");
    let _pinned: Vec<EnvVar> = env
        .iter()
        .map(|(key, value)| EnvVar::set(key, value))
        .collect();

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
        auxiliary.unwrap_or_else(|| {
            Arc::new(|_| Box::pin(async { Ok(("\"answer\"".to_string(), None)) }))
        }),
        AdjudicationSettings::default(),
    )
    .unwrap();

    let ws = tempfile::TempDir::new().unwrap();
    let workspace = workspace_task
        .map_or(ws.path(), |(path, _)| path)
        .to_string_lossy()
        .into_owned();
    let task = workspace_task.map_or_else(|| instruction(check), |(_, task)| task.to_string());
    let (uri, messages) = (server.uri(), msgs());
    let attempt_ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut context = ctx(&uri, &messages, &caveats);
    context.workspace = &workspace;
    context.run_allowance = allowance;
    context.attempt_ledger = allowance.map(|_| &attempt_ledger);
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
    let mut at_cap = false;
    context.round_cap_hit = Some(&mut at_cap);
    configure(&mut context);
    let (answer, _, _, _) = if wire == "responses" {
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
        at_cap,
        answer,
        notes_bytes: std::fs::read_to_string(std::path::Path::new(&workspace).join("notes.txt"))
            .ok(),
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

/// Grounds verification eligibility in real files and the existing shell:
/// an external report does not change the code workspace, while a local source
/// change still requires its checks. Both ordinary gates and all smart wires
/// must make the same decision in attempted and result-aware modes.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn unchanged_workspace_does_not_turn_an_external_report_into_a_coding_task() {
    for (wire, smart) in [
        ("openai", false),
        ("anthropic", false),
        ("openai", true),
        ("anthropic", true),
        ("ollama", true),
        ("responses", true),
    ] {
        for outcomes in [false, true] {
            for source_change in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let workspace = root.path().join("code");
                std::fs::create_dir(&workspace).unwrap();
                std::fs::write(workspace.join("Cargo.toml"), "[workspace]\n").unwrap();
                std::fs::write(workspace.join("package.json"), "{}\n").unwrap();
                std::fs::create_dir(workspace.join("tests")).unwrap();
                let command = if source_change {
                    "sh -c 'printf changed > source.rs'"
                } else {
                    "sh -c 'printf report > ../dashboard.md'"
                };
                let run = run_turn(Turn {
                    wire,
                    smart,
                    outcomes,
                    check: "",
                    script: &[Step::Run(command)],
                    cancel: None,
                    max_tool_rounds: 8,
                    caveats: Caveats::top(),
                    env: &[],
                    workspace_task: Some((&workspace, "Complete the requested file update.")),
                })
                .await;
                let label =
                    format!("{wire} smart={smart} outcomes={outcomes} source={source_change}");
                let output = if source_change {
                    workspace.join("source.rs")
                } else {
                    root.path().join("dashboard.md")
                };
                assert_eq!(
                    std::fs::read_to_string(output).unwrap(),
                    if source_change { "changed" } else { "report" },
                    "{label}"
                );
                let nudged = run
                    .bodies
                    .iter()
                    .any(|body| body.contains("Before you finish"));
                assert_eq!(nudged, source_change, "{label}");
                if !source_change {
                    assert_eq!(run.reason, "completed", "{label}");
                    assert_eq!(run.bodies.len(), 2, "{label}: no unrelated test rounds");
                }
            }
        }
    }
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
        assert!(!run.at_cap, "{wire} smart={smart}");
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
        // Round three, item 7: every cap exit says so, whatever it reports, so
        // the TUI keeps the pause affordance for a repair_exhausted at the cap.
        assert!(run.at_cap, "{wire} smart={smart}");
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

/// Round three, item 2: a check created after the turn's first scan is still
/// seen. The model runs a command (the first scan finds nothing to verify),
/// writes a `Cargo.toml`, and runs `cargo test`, which fails on the broken
/// manifest at the round limit. The cap exit must see that check and end
/// `repair_exhausted`, not `round_cap`. The task names no check, so only the
/// workspace scan can find it.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn a_check_created_after_the_first_scan_decides_the_cap_exit() {
    let script = [
        Step::Run(PASSING_CHECK),
        Step::Run("sh -c 'echo broken > Cargo.toml'"),
        Step::Run("cargo test"),
    ];
    for (wire, smart) in [("openai", false), ("anthropic", true)] {
        let run = run_script(wire, smart, true, "", &script, None, 3).await;
        assert_eq!(run.reason, "repair_exhausted", "{wire} smart={smart}");
        // The cap exit's trace names the check the scan found after the write,
        // and the failed run it decided on.
        let checks: Vec<_> = run
            .signals
            .iter()
            .filter_map(|signal| match signal {
                observability::BehaviorSignal::Verification { report, .. } => Some(
                    report
                        .checks
                        .iter()
                        .map(|check| (check.label.clone(), check.status))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(
            checks,
            [vec![(
                "`cargo test`".to_string(),
                crate::agentic::self_verify::CheckStatus::Failed
            )]],
            "{wire} smart={smart}"
        );
    }
}

/// #2315 acceptance: each case the issue names ends the turn within the
/// declared allowance, and the trace names what decided it. The ordinary gate
/// and SmartHarness reach the same ending in every case.
/// - requested but unexecuted: the batch holding the check is rejected before
///   anything runs, so the check has no execution and cannot pass;
/// - denied: no exec authority, so the confined shell refuses the check;
/// - unavailable: the check's program does not exist;
/// - timed out: the host lane's wall-clock ceiling kills the check;
/// - failed with substantial output: the result the model reads is collapsed,
///   and the check still classifies failed;
/// - no checks: nothing to verify, and the trace says so.
///
/// Traps: a bound that holds only because a case never ran (each case asserts
/// the class its check recorded), and a round count that cannot fail (rounds
/// are counted exactly, excluding only a request that replays the previous
/// history).
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn every_verification_case_ends_within_its_allowance() {
    use crate::agentic::self_verify::{CheckStatus, VERIFY_REPAIR_ALLOWANCE};
    const ABSENT: &str = "newt-absent-binary-2315-check --verify";
    const SLOW: &str = "sleep 5";
    const LOUD_FAILURE: &str = "sh -c 'seq 1 20000; exit 1'";
    const EXHAUSTED: &[&str] = &["nudge", "nudge", "nudge", "repair_exhausted"];
    struct Case {
        name: &'static str,
        check: &'static str,
        step: Step,
        caveats: Caveats,
        env: &'static [(&'static str, &'static str)],
        reason: &'static str,
        /// Primary model rounds: the scripted step, one answer per nudge, and
        /// the final answer.
        rounds: usize,
        /// Every verification decision the trace records, in order.
        decisions: &'static [&'static str],
        /// The class the check recorded at the last decision.
        status: Option<CheckStatus>,
    }
    let no_exec = Caveats {
        exec: crate::caveats::Scope::none(),
        ..Caveats::top()
    };
    let cases = [
        Case {
            name: "requested but unexecuted",
            check: PASSING_CHECK,
            step: Step::RejectedBatch(PASSING_CHECK),
            caveats: Caveats::top(),
            env: &[],
            reason: "verification_incomplete",
            rounds: 2,
            decisions: &["verification_incomplete"],
            status: Some(CheckStatus::Unexecuted),
        },
        Case {
            name: "denied",
            check: PASSING_CHECK,
            step: Step::Run(PASSING_CHECK),
            caveats: no_exec,
            env: &[("NEWT_SHELL_ENGINE", "safe-subset")],
            reason: "verification_incomplete",
            rounds: 2,
            decisions: &["verification_incomplete"],
            status: Some(CheckStatus::Denied),
        },
        Case {
            name: "unavailable",
            check: ABSENT,
            step: Step::Run(ABSENT),
            caveats: Caveats::top(),
            env: &[],
            reason: "verification_incomplete",
            rounds: 2,
            decisions: &["verification_incomplete"],
            status: Some(CheckStatus::Unavailable),
        },
        Case {
            name: "timed out",
            check: SLOW,
            step: Step::Run(SLOW),
            caveats: Caveats::top(),
            env: &[
                ("NEWT_DISABLE_OCAP", "1"),
                ("NEWT_HOST_EXEC_TIMEOUT_SECS", "1"),
            ],
            reason: "repair_exhausted",
            rounds: 5,
            decisions: EXHAUSTED,
            status: Some(CheckStatus::TimedOut),
        },
        Case {
            name: "failed with substantial output",
            check: LOUD_FAILURE,
            step: Step::Run(LOUD_FAILURE),
            caveats: Caveats::top(),
            env: &[],
            reason: "repair_exhausted",
            rounds: 5,
            decisions: EXHAUSTED,
            status: Some(CheckStatus::Failed),
        },
        Case {
            name: "no checks",
            check: "",
            step: Step::Done,
            caveats: Caveats::top(),
            env: &[],
            reason: "completed",
            rounds: 1,
            decisions: &["no_checks"],
            status: None,
        },
    ];
    for case in &cases {
        for (wire, smart) in [("openai", false), ("anthropic", true)] {
            let label = format!("{} on {wire} smart={smart}", case.name);
            let run = run_turn(Turn {
                wire,
                smart,
                outcomes: true,
                check: case.check,
                script: &[case.step],
                cancel: None,
                max_tool_rounds: 8,
                caveats: case.caveats.clone(),
                env: case.env,
                workspace_task: None,
            })
            .await;
            assert_eq!(run.reason, case.reason, "{label}");
            // Primary rounds only: a request whose messages equal the previous
            // request's replays that round rather than starting a new one.
            let histories: Vec<serde_json::Value> = run
                .bodies
                .iter()
                .map(|body| {
                    serde_json::from_str::<serde_json::Value>(body).unwrap()["messages"].clone()
                })
                .collect();
            let rounds = histories
                .iter()
                .enumerate()
                .filter(|(i, history)| *i == 0 || histories[i - 1] != **history)
                .count();
            assert_eq!(rounds, case.rounds, "{label}: primary model rounds");
            assert!(rounds <= VERIFY_REPAIR_ALLOWANCE + 2, "{label}");
            let signals: Vec<_> = run
                .signals
                .iter()
                .filter_map(|signal| match signal {
                    observability::BehaviorSignal::Verification {
                        decision, report, ..
                    } => Some((decision.as_str(), report)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                signals
                    .iter()
                    .map(|(decision, _)| *decision)
                    .collect::<Vec<_>>(),
                case.decisions,
                "{label}: the decision sequence"
            );
            assert_eq!(
                signals.last().map(|(_, report)| report
                    .checks
                    .iter()
                    .map(|check| check.status)
                    .collect::<Vec<_>>()),
                Some(case.status.into_iter().collect()),
                "{label}: the class the check recorded"
            );
            if case.check == LOUD_FAILURE {
                assert!(
                    run.bodies[1].contains("error: command exited 1")
                        && !run.bodies[1].contains("\\n10000\\n"),
                    "{label}: the model reads a collapsed failure"
                );
            }
            if case.check == SLOW {
                assert!(
                    run.bodies[1..]
                        .iter()
                        .any(|body| body.contains("timed out")),
                    "{label}: the repair names the timeout"
                );
            }
        }
    }
}

#[path = "http_resolute.rs"]
mod resolute;

#[path = "http_grit.rs"]
mod grit;
