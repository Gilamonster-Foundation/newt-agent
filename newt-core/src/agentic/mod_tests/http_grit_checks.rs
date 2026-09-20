//! #2449: actual configured checks retain their own outcome and tree freshness.
use super::*;

async fn configured_check(smart: bool, failed: bool, stale: bool, normal: bool) {
    let _tenacity = crate::tenacity::scoped_tenacity_settings(
        if normal {
            crate::Tenacity::Normal
        } else {
            crate::Tenacity::Resolute
        },
        crate::tenacity::TenacityBudgets::default(),
    );
    let workspace = tempfile::tempdir().unwrap();
    let check = if failed {
        "printf checked > check-marker; exit 1"
    } else {
        "printf checked > check-marker; test -f notes.txt"
    };
    let mut caveats = Caveats::top();
    caveats.fs_write = crate::caveats::Scope::Only(
        [workspace.path().to_string_lossy().into_owned()]
            .into_iter()
            .collect(),
    );
    let mut script = vec![Step::Tool(
        "write_file",
        r#"{"path":"notes.txt","content":"requested change\n"}"#,
    )];
    if stale {
        script.push(Step::Run("sh -c 'printf changed > notes.txt'"));
    }
    script.push(Step::Done);
    let run = run_turn_configured(
        Turn {
            wire: "openai",
            smart,
            outcomes: false,
            check: "",
            script: &script,
            cancel: None,
            max_tool_rounds: 8,
            caveats,
            env: &[
                ("NEWT_SELF_VERIFY", "0"),
                ("NEWT_SHELL_ENGINE", "safe-subset"),
            ],
            workspace_task: Some((workspace.path(), "Write the requested notes file.")),
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
            ctx.build_check_cmd = Some(check.to_string());
        },
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("check-marker")).unwrap(),
        "checked"
    );
    assert_eq!(
        run.notes_bytes.as_deref(),
        Some(if stale {
            "changed"
        } else {
            "requested change\n"
        })
    );
    assert_eq!(
        run.reason,
        if normal || (!failed && !stale) {
            "completed"
        } else if failed {
            "repair_exhausted"
        } else {
            "verification_incomplete"
        }
    );
    assert_eq!(
        run.bodies.len(),
        if normal || (!failed && !stale) {
            2
        } else if failed {
            4
        } else {
            6
        }
    );
    if !normal {
        let wanted = if failed {
            self_verify::CheckStatus::Failed
        } else if stale {
            self_verify::CheckStatus::Stale
        } else {
            self_verify::CheckStatus::Passed
        };
        assert!(
            run.signals.iter().any(|signal| matches!(signal,
            observability::BehaviorSignal::Verification { report, .. }
            if report.checks.iter().any(|check| check.status == wanted))),
            "configured check must supply structured {wanted:?} evidence: {:?}",
            run.signals
        );
    }
}

macro_rules! check_case {
    ($name:ident, $smart:literal, $failed:literal, $stale:literal, $normal:literal) => {
        #[cfg(target_os = "linux")]
        #[tokio::test]
        #[ignore = "requires confined-exec helper; executed explicitly in Grit validation"]
        #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
        async fn $name() {
            configured_check($smart, $failed, $stale, $normal).await;
        }
    };
}
check_case!(grit_2449_configured_pass, false, false, false, false);
check_case!(grit_2449_configured_pass_smart, true, false, false, false);
check_case!(grit_2449_configured_failed, false, true, false, false);
check_case!(grit_2449_configured_failed_smart, true, true, false, false);
check_case!(grit_2449_configured_stale, false, false, true, false);
check_case!(grit_2449_configured_stale_smart, true, false, true, false);
check_case!(grit_2449_configured_normal, false, true, false, true);
check_case!(grit_2449_configured_normal_smart, true, true, false, true);
