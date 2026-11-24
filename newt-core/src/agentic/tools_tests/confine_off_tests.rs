//! Missing backend at the shell boundary: no user command/backend, permission
//! prompt/grant, or scratch allocation. Earlier L2 and native operations keep
//! their own boundaries. The probe fakes I/O, never the production refusal.

use super::*;
use crate::agentic::tools::command_probe::{self, Probe};
use crate::ExecOutcome;
use std::sync::{atomic::Ordering, Arc, OnceLock};

const REFUSAL: &str = "error: this build has no confined shell; this invocation cannot run on the host route. No command ran.";

async fn run(
    command: &str,
    bypass: bool,
    full_access: bool,
    floor: Option<&Scope<String>>,
) -> (String, ExecOutcome, Arc<Probe>, MockGate) {
    let _env = crate::process_env::lock();
    let _routing = crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_NO_ROUTE", "1");
    let _authority = crate::launch_authority::ScopedAuthority::new(full_access, bypass);
    let workspace = if cfg!(windows) {
        "C:\\p12-virtual"
    } else {
        "/p12-virtual"
    };
    // The launch signal must not invent write authority. No Git-root lookup is needed.
    let caveats = Caveats {
        fs_write: Scope::none(),
        ..Caveats::top()
    };
    let probe = Arc::new(Probe {
        io_only: true,
        ..Probe::default()
    });
    let execution = OnceLock::new();
    let retry = OnceLock::new();
    let mut pending = None;
    let mut gate = MockGate::new(true, &caveats);
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 20, false);
    let text = command_probe::IO
        .scope(
            probe.clone(),
            execute_tool_inner(
                &mut display,
                "run_command",
                &serde_json::json!({"command": command, "cwd": workspace}),
                workspace,
                false,
                20,
                &caveats,
                &mut NoMcp,
                ToolCollaborators {
                    command_probe: Some(&probe),
                    permission_gate: Some(&mut gate),
                    exec_floor: floor,
                    execution: Some(&execution),
                    retry_authorized: Some(&retry),
                    pending_rerun: Some(&mut pending),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
            ),
        )
        .await;
    assert!(pending.is_none(), "missing-authority rerun slot: {text}");
    assert!(retry.get().is_none(), "retry grant: {text}");
    let outcome = *execution
        .get()
        .unwrap_or_else(|| panic!("no typed outcome: {text}"));
    (text, outcome, probe, gate)
}

fn refused((text, outcome, probe, gate): (String, ExecOutcome, Arc<Probe>, MockGate)) {
    assert_eq!(probe.host.load(Ordering::SeqCst), 0, "host backend");
    assert_eq!(probe.confined.load(Ordering::SeqCst), 0, "confined backend");
    // This entry precedes build-directory discovery, all scratch operations and prompts.
    assert_eq!(
        probe.build.load(Ordering::SeqCst),
        0,
        "specialized build entry"
    );
    assert!(gate.asks.is_empty());
    assert!(gate.full_access.is_empty());
    assert_eq!(gate.authority_calls, 0, "permission gate contact");
    assert_eq!(text, REFUSAL);
    assert_eq!(outcome, ExecOutcome::Unavailable);
}

fn host((text, outcome, probe, gate): (String, ExecOutcome, Arc<Probe>, MockGate)) {
    assert_eq!(outcome, ExecOutcome::Passed, "{text}");
    assert_eq!(probe.host.load(Ordering::SeqCst), 1);
    assert_eq!(probe.confined.load(Ordering::SeqCst), 0);
    assert_eq!(probe.build.load(Ordering::SeqCst), 0);
    assert_eq!(
        probe.exec_records.load(Ordering::SeqCst),
        1,
        "active exec recorder"
    );
    assert!(gate.asks.is_empty());
    assert!(gate.full_access.is_empty());
    assert_eq!(gate.authority_calls, 0);
}

#[tokio::test(start_paused = true)]
async fn without_confine_run_command_is_refused_before_anything_runs() {
    for command in [
        "printf p12",
        "git push --force origin",
        "gh pr create --web",
    ] {
        for full_access in [false, true] {
            for floor in [None, Some(&Scope::none())] {
                refused(run(command, false, full_access, floor).await);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn without_confine_the_exec_floor_still_stops_the_host_bypass() {
    let allowed = Scope::only(["printf".into()]);
    host(run("printf p12", true, false, None).await);
    host(run("printf p12", true, false, Some(&allowed)).await);
    refused(run("printf p12", true, false, Some(&Scope::none())).await);
    refused(run("printf p12; printf again", true, false, Some(&allowed)).await);
}

#[tokio::test(start_paused = true)]
async fn without_confine_a_build_command_is_refused_like_any_other() {
    host(run("cargo check", true, false, None).await);
    host(
        run(
            "cargo check",
            true,
            false,
            Some(&Scope::only(["cargo".into()])),
        )
        .await,
    );
    for bypass in [false, true] {
        for floor in [None, Some(&Scope::none())] {
            if bypass && floor.is_none() {
                continue;
            }
            let result = run("cargo check", bypass, false, floor).await;
            assert_eq!(
                result.2.build.load(Ordering::SeqCst),
                0,
                "specialized build entry"
            );
            refused(result);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn without_confine_a_refused_command_is_not_recorded_as_unconfined() {
    // Positive control proves the injected recorder is active without an environment/file sink.
    host(run("printf p12", true, true, None).await);
    let result = run("printf p12", false, true, None).await;
    assert_eq!(
        result.2.exec_records.load(Ordering::SeqCst),
        0,
        "refused exec was recorded"
    );
    refused(result);
}
