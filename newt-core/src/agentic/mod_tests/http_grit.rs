//! #2449 cumulative Grit: actual failed checks share a two-admission cap.
use super::*;

async fn grit_2449_failed_check_is_bounded(wire: &str, smart: bool) {
    // Relentless already exists on the old path and includes every lower
    // tenacity obligation; this proves behavior without a compile-only red.
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Relentless);
    const CHECK: &str = "sh -c 'echo attempted > notes.txt; exit 1'";
    let run = run_turn_configured(
        Turn {
            wire,
            smart,
            outcomes: false,
            check: CHECK,
            script: &[Step::Run(CHECK)],
            cancel: None,
            max_tool_rounds: 8,
            caveats: Caveats::top(),
            env: &[
                ("NEWT_SELF_VERIFY", "0"),
                ("NEWT_SHELL_ENGINE", "safe-subset"),
            ],
            workspace_task: None,
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
        },
    )
    .await;
    assert_eq!(
        run.notes_bytes.as_deref(),
        Some("attempted\n"),
        "real check ran in its own workspace"
    );
    assert_eq!(
        run.reason, "repair_exhausted",
        "failed-check classification survives"
    );
    assert_eq!(run.bodies.len(), 4, "initial tool response + concluding response + two corrective responses; verification's third nudge cannot buy an extra Grit retry");
    assert_eq!(
        repair_ordinals(&run),
        ["1/3", "2/3"].map(String::from).into()
    );
}

macro_rules! wire_case {
    ($name:ident, $wire:literal, $smart:literal) => {
        /// #2449: cumulative recovery must not spend verification's third unit.
        #[cfg(unix)]
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
        async fn $name() {
            grit_2449_failed_check_is_bounded($wire, $smart).await;
        }
    };
}
wire_case!(grit_2449_chat_ordinary_bound, "openai", false);
wire_case!(grit_2449_chat_smart_bound, "openai", true);
wire_case!(grit_2449_responses_ordinary_bound, "responses", false);
wire_case!(grit_2449_responses_smart_bound, "responses", true);
wire_case!(grit_2449_anthropic_ordinary_bound, "anthropic", false);
wire_case!(grit_2449_anthropic_smart_bound, "anthropic", true);
wire_case!(grit_2449_ollama_ordinary_bound, "ollama", false);
wire_case!(grit_2449_ollama_smart_bound, "ollama", true);

async fn grit_2449_noncheck_failure_is_reviewed(wire: &str, smart: bool, step: Step) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Relentless);
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "operator content\n").unwrap();
    let task = instruction(PASSING_CHECK);
    let run = run_turn_configured(
        Turn {
            wire,
            smart,
            outcomes: false,
            check: PASSING_CHECK,
            script: &[Step::Run(PASSING_CHECK), step, Step::Done],
            cancel: None,
            max_tool_rounds: 8,
            caveats: Caveats::top(),
            env: &[
                ("NEWT_SELF_VERIFY", "0"),
                ("NEWT_SHELL_ENGINE", "safe-subset"),
            ],
            workspace_task: Some((workspace.path(), &task)),
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
        },
    )
    .await;
    assert_eq!(
        run.notes_bytes.as_deref(),
        Some("operator content\n"),
        "failure and harness reasoning never replay or manufacture a write"
    );
    assert_eq!(
        run.reason, "completed",
        "passing check stays fresh when failed operation changed no bytes"
    );
    assert_eq!(run.bodies.len(), 4, "one corrective reasoning continuation follows the observed non-check failure before completion");
}

macro_rules! noncheck_case {
    ($name:ident, $wire:literal, $smart:literal, $step:expr) => {
        /// #2449: ordinary failure evidence cannot disappear behind a passing check.
        #[cfg(unix)]
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
        async fn $name() {
            grit_2449_noncheck_failure_is_reviewed($wire, $smart, $step).await;
        }
    };
}
noncheck_case!(
    grit_2449_chat_ordinary_shell,
    "openai",
    false,
    Step::Run(FAILING_CHECK)
);
noncheck_case!(
    grit_2449_chat_ordinary_read,
    "openai",
    false,
    Step::Tool("read_file", r#"{"path":"absent.txt"}"#)
);
noncheck_case!(
    grit_2449_chat_ordinary_edit,
    "openai",
    false,
    Step::Tool(
        "edit_file",
        r#"{"path":"notes.txt","old_string":"absent text","new_string":"must not be written"}"#
    )
);
noncheck_case!(
    grit_2449_chat_smart_shell,
    "openai",
    true,
    Step::Run(FAILING_CHECK)
);
noncheck_case!(
    grit_2449_chat_smart_read,
    "openai",
    true,
    Step::Tool("read_file", r#"{"path":"absent.txt"}"#)
);
noncheck_case!(
    grit_2449_chat_smart_edit,
    "openai",
    true,
    Step::Tool(
        "edit_file",
        r#"{"path":"notes.txt","old_string":"absent text","new_string":"must not be written"}"#
    )
);
noncheck_case!(
    grit_2449_responses_ordinary_shell,
    "responses",
    false,
    Step::Run(FAILING_CHECK)
);
noncheck_case!(
    grit_2449_responses_ordinary_read,
    "responses",
    false,
    Step::Tool("read_file", r#"{"path":"absent.txt"}"#)
);
noncheck_case!(
    grit_2449_responses_ordinary_edit,
    "responses",
    false,
    Step::Tool(
        "edit_file",
        r#"{"path":"notes.txt","old_string":"absent text","new_string":"must not be written"}"#
    )
);
noncheck_case!(
    grit_2449_responses_smart_shell,
    "responses",
    true,
    Step::Run(FAILING_CHECK)
);
noncheck_case!(
    grit_2449_responses_smart_read,
    "responses",
    true,
    Step::Tool("read_file", r#"{"path":"absent.txt"}"#)
);
noncheck_case!(
    grit_2449_responses_smart_edit,
    "responses",
    true,
    Step::Tool(
        "edit_file",
        r#"{"path":"notes.txt","old_string":"absent text","new_string":"must not be written"}"#
    )
);
noncheck_case!(
    grit_2449_anthropic_ordinary_shell,
    "anthropic",
    false,
    Step::Run(FAILING_CHECK)
);
noncheck_case!(
    grit_2449_anthropic_ordinary_read,
    "anthropic",
    false,
    Step::Tool("read_file", r#"{"path":"absent.txt"}"#)
);
noncheck_case!(
    grit_2449_anthropic_ordinary_edit,
    "anthropic",
    false,
    Step::Tool(
        "edit_file",
        r#"{"path":"notes.txt","old_string":"absent text","new_string":"must not be written"}"#
    )
);
noncheck_case!(
    grit_2449_anthropic_smart_shell,
    "anthropic",
    true,
    Step::Run(FAILING_CHECK)
);
noncheck_case!(
    grit_2449_anthropic_smart_read,
    "anthropic",
    true,
    Step::Tool("read_file", r#"{"path":"absent.txt"}"#)
);
noncheck_case!(
    grit_2449_anthropic_smart_edit,
    "anthropic",
    true,
    Step::Tool(
        "edit_file",
        r#"{"path":"notes.txt","old_string":"absent text","new_string":"must not be written"}"#
    )
);
noncheck_case!(
    grit_2449_ollama_ordinary_shell,
    "ollama",
    false,
    Step::Run(FAILING_CHECK)
);
noncheck_case!(
    grit_2449_ollama_ordinary_read,
    "ollama",
    false,
    Step::Tool("read_file", r#"{"path":"absent.txt"}"#)
);
noncheck_case!(
    grit_2449_ollama_ordinary_edit,
    "ollama",
    false,
    Step::Tool(
        "edit_file",
        r#"{"path":"notes.txt","old_string":"absent text","new_string":"must not be written"}"#
    )
);
noncheck_case!(
    grit_2449_ollama_smart_shell,
    "ollama",
    true,
    Step::Run(FAILING_CHECK)
);
noncheck_case!(
    grit_2449_ollama_smart_read,
    "ollama",
    true,
    Step::Tool("read_file", r#"{"path":"absent.txt"}"#)
);
noncheck_case!(
    grit_2449_ollama_smart_edit,
    "ollama",
    true,
    Step::Tool(
        "edit_file",
        r#"{"path":"notes.txt","old_string":"absent text","new_string":"must not be written"}"#
    )
);

async fn actual_auxiliary() -> (MockServer, Arc<SummarizeFn>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("\"answer\""))
        .mount(&server)
        .await;
    let url = server.uri();
    let complete: Arc<SummarizeFn> = Arc::new(move |prompt| {
        let url = url.clone();
        Box::pin(async move {
            let text = reqwest::Client::new()
                .post(url)
                .body(prompt)
                .send()
                .await?
                .text()
                .await?;
            Ok((text, None))
        })
    });
    (server, complete)
}

async fn grit_2449_auxiliary_obeys_global_admission(wire: &str) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Relentless);
    let auxiliary = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("\"answer\""))
        .mount(&auxiliary)
        .await;
    let url = auxiliary.uri();
    let complete: Arc<SummarizeFn> = Arc::new(move |prompt| {
        let url = url.clone();
        Box::pin(async move {
            let text = reqwest::Client::new()
                .post(url)
                .body(prompt)
                .send()
                .await?
                .text()
                .await?;
            Ok((text, None))
        })
    });
    let allowance = run_allowance::RunAllowance::new(2);
    let run = run_turn_with_auxiliary(
        Turn {
            wire,
            smart: true,
            outcomes: false,
            check: PASSING_CHECK,
            script: &[Step::Run(PASSING_CHECK), Step::Done],
            cancel: None,
            max_tool_rounds: 8,
            caveats: Caveats::top(),
            env: &[
                ("NEWT_SELF_VERIFY", "0"),
                ("NEWT_SHELL_ENGINE", "safe-subset"),
            ],
            workspace_task: None,
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
        },
        Some(complete),
        Some(&allowance),
    )
    .await;
    assert_eq!(
        run.bodies.len(),
        2,
        "primary consumes the two existing global units"
    );
    assert_eq!(allowance.remaining(), 0);
    assert_eq!(auxiliary.received_requests().await.unwrap().len(), 0,
        "Smart adjudication cannot send an unbudgeted third model request for this cumulative Grit turn");
}

macro_rules! auxiliary_case {
    ($name:ident, $wire:literal) => {
        /// #2449: actual Smart auxiliary HTTP dispatch shares global admission.
        #[cfg(unix)]
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
        async fn $name() {
            grit_2449_auxiliary_obeys_global_admission($wire).await;
        }
    };
}
auxiliary_case!(grit_2449_chat_auxiliary_bound, "openai");
auxiliary_case!(grit_2449_responses_auxiliary_bound, "responses");
auxiliary_case!(grit_2449_anthropic_auxiliary_bound, "anthropic");
auxiliary_case!(grit_2449_ollama_auxiliary_bound, "ollama");

#[path = "http_grit_boundaries.rs"]
mod boundaries;

#[path = "http_grit_checks.rs"]
mod configured_checks;

#[cfg(unix)]
#[path = "http_grit_standalone.rs"]
mod standalone;

#[cfg(unix)]
#[path = "http_grit_repair.rs"]
mod repair;

#[cfg(unix)]
#[path = "http_grit_check_accounting.rs"]
mod check_accounting;
