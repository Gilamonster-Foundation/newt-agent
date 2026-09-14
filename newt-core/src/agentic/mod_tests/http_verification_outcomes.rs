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

/// One scripted reply: `Some(command)` is a `run_command` call, `None` the
/// final text `done`.
fn reply(wire: &str, command: Option<&str>) -> ResponseTemplate {
    let value = match (wire, command) {
        ("ollama", Some(c)) => serde_json::json!({"message":{"role":"assistant","content":"",
            "tool_calls":[{"function":{"name":"run_command","arguments":{"command":c}}}]},"done":true}),
        ("ollama", None) => {
            serde_json::json!({"message":{"role":"assistant","content":"done"},"done":true})
        }
        ("anthropic", Some(c)) => serde_json::json!({"id":"msg_1","type":"message",
            "role":"assistant","model":"test-model","stop_reason":"tool_use",
            "content":[{"type":"tool_use","id":"call_1","name":"run_command","input":{"command":c}}]}),
        ("anthropic", None) => serde_json::json!({"id":"msg_1","type":"message",
            "role":"assistant","model":"test-model","stop_reason":"end_turn",
            "content":[{"type":"text","text":"done"}],"usage":{"input_tokens":10,"output_tokens":5}}),
        ("responses", Some(c)) => serde_json::json!({"id":"resp_1","status":"completed",
            "output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"run_command",
            "arguments":serde_json::json!({"command":c}).to_string()}]}),
        ("responses", None) => serde_json::json!({"id":"resp_1","status":"completed",
            "model":"test-model","output":[{"type":"message","id":"msg_1","role":"assistant",
            "status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}]}),
        (_, Some(c)) => serde_json::json!({"choices":[{"message":{"role":"assistant","content":"",
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"run_command",
            "arguments":serde_json::json!({"command":c}).to_string()}}]},"finish_reason":"tool_calls"}]}),
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
}

/// The model runs `check` once, then answers `done` every time it is asked.
async fn run(wire: &str, smart: bool, outcomes: bool, check: &str) -> Run {
    let _lock = env_lock().await;
    let _self_verify = EnvVar::set("NEWT_SELF_VERIFY", "1");
    let _outcomes = if outcomes {
        EnvVar::set("NEWT_VERIFY_OUTCOMES", "1")
    } else {
        EnvVar::unset("NEWT_VERIFY_OUTCOMES")
    };
    let _confined = EnvVar::unset("NEWT_DISABLE_OCAP");
    let _no_anthropic_stream = EnvVar::set("NEWT_ANTHROPIC_STREAM", "off");

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (wire_owned, check_owned) = (wire.to_string(), check.to_string());
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            reply(&wire_owned, first.then_some(check_owned.as_str()))
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
    context.max_tool_rounds = 8;
    context.smart_harness = smart.then_some(&harness);
    context.kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
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
    }
}
