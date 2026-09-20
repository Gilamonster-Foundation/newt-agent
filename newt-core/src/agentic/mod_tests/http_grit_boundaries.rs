//! #2449: completion boundaries retain all typed recovery obligations.
use super::*;

async fn grit_2449_missing_check_and_tool_failure_share_admission(smart: bool) {
    let _tenacity = crate::tenacity::scoped_tenacity_settings(
        crate::Tenacity::Resolute,
        crate::tenacity::TenacityBudgets { grit_retries: 0 },
    );
    let run = run_turn_configured(
        Turn {
            wire: "openai",
            smart,
            outcomes: false,
            check: PASSING_CHECK,
            script: &[
                Step::Tool("read_file", r#"{"path":"missing.txt"}"#),
                Step::Done,
            ],
            cancel: None,
            max_tool_rounds: 8,
            caveats: Caveats::top(),
            env: &[("NEWT_SELF_VERIFY", "0")],
            workspace_task: None,
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
        },
    )
    .await;
    assert_eq!(run.bodies.len(), 2,
        "missing-check guidance also addresses the observed read failure and cannot bypass zero Grit");
    assert_eq!(run.reason, "failed");
    assert!(run.answer.contains("recovery incomplete"));
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn grit_2449_missing_check_noncheck_overlap_ordinary() {
    grit_2449_missing_check_and_tool_failure_share_admission(false).await;
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn grit_2449_missing_check_noncheck_overlap_smart() {
    grit_2449_missing_check_and_tool_failure_share_admission(true).await;
}

/// #2449: an automatic configured check is a real failed operation, even
/// though write_file itself succeeded. Assert both effects before recovery.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the confined-exec helper; executed explicitly in Grit validation"]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn grit_2449_automatic_build_failure_reaches_recovery() {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Grit);
    let workspace = tempfile::tempdir().unwrap();
    const CHECK: &str = "printf checked > check-marker; exit 1";
    let mut caveats = Caveats::top();
    caveats.fs_write = crate::caveats::Scope::Only(
        [workspace.path().to_string_lossy().into_owned()]
            .into_iter()
            .collect(),
    );
    let run = run_turn_configured(
        Turn {
            wire: "openai",
            smart: false,
            outcomes: false,
            check: "",
            script: &[
                Step::Tool(
                    "write_file",
                    r#"{"path":"notes.txt","content":"requested change\n"}"#,
                ),
                Step::Done,
            ],
            cancel: None,
            max_tool_rounds: 8,
            caveats,
            env: &[("NEWT_SELF_VERIFY", "0")],
            workspace_task: Some((workspace.path(), "Write notes.txt as requested.")),
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
            ctx.build_check_cmd = Some(CHECK.to_string());
        },
    )
    .await;
    assert_eq!(run.notes_bytes.as_deref(), Some("requested change\n"));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("check-marker")).unwrap(),
        "checked",
        "the confined automatic check actually executed"
    );
    assert_eq!(
        run.bodies.len(),
        3,
        "typed automatic check failure admits one corrective assessment"
    );
    assert_eq!(
        run.reason, "completed",
        "Grit may explain an expected failure"
    );
}

async fn grit_2449_final_failure_stops_without_summary(wire: &str, smart: bool) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Grit);
    let run = run_turn_configured(
        Turn {
            wire,
            smart,
            outcomes: false,
            check: "",
            script: &[Step::Tool("read_file", r#"{"path":"missing.txt"}"#)],
            cancel: None,
            max_tool_rounds: 1,
            caveats: Caveats::top(),
            env: &[("NEWT_SELF_VERIFY", "0")],
            workspace_task: None,
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
        },
    )
    .await;
    assert_eq!(
        run.bodies.len(),
        1,
        "a final-round failure cannot admit corrective summary work"
    );
    assert_eq!(run.reason, "failed");
    assert!(run.answer.contains("recovery incomplete"));
}

macro_rules! final_failure_case {
    ($name:ident, $wire:literal, $smart:literal) => {
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
        async fn $name() {
            grit_2449_final_failure_stops_without_summary($wire, $smart).await;
        }
    };
}
final_failure_case!(grit_2449_final_failure_chat, "openai", false);
final_failure_case!(grit_2449_final_failure_chat_smart, "openai", true);
final_failure_case!(grit_2449_final_failure_responses, "responses", false);
final_failure_case!(grit_2449_final_failure_responses_smart, "responses", true);
final_failure_case!(grit_2449_final_failure_anthropic, "anthropic", false);
final_failure_case!(grit_2449_final_failure_anthropic_smart, "anthropic", true);
final_failure_case!(grit_2449_final_failure_ollama, "ollama", false);
final_failure_case!(grit_2449_final_failure_ollama_smart, "ollama", true);
