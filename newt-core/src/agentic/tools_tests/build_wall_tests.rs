//! #2732: builds retain their budget while ordinary commands and probes stay bounded.

use std::time::Duration;

fn build_wall() -> Duration {
    super::shell::LIFECYCLE_BUILD_TIMEOUT
}

fn default_wall() -> Duration {
    Duration::from_secs(agent_bridle::LimitsPolicy::default().default_timeout_secs)
}

/// #2732: prepared compound builds retain the build executor budget.
#[test]
fn a_compound_cargo_command_keeps_the_build_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall(
            r#"cargo test -j 4 -p newt-git; echo "EXIT=$?""#,
            Default::default(),
        ),
        build_wall()
    );
}

/// `just <recipe>` is the other program in #2533's build-lane table.
#[test]
fn a_just_command_keeps_the_build_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall("just test", Default::default(),),
        build_wall()
    );
}

/// An ordinary command is unaffected — the default wall still applies.
#[test]
fn a_plain_command_keeps_the_default_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall("ls -la", Default::default(),),
        default_wall()
    );
}

/// Mentioning Cargo does not widen an ordinary command budget.
#[test]
fn cargo_mentioned_later_does_not_widen_the_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall("echo cargo", Default::default(),),
        default_wall()
    );
}

/// A timed-out result names the wall that actually applied, so a reader can
/// tell why a call ran for minutes instead of assuming the ordinary 60s.
#[test]
fn a_timed_out_build_command_names_the_build_wall_in_its_result() {
    let _env = crate::process_env::lock();
    let envelope = serde_json::json!({
        "exit_code": 124,
        "stdout": "partial",
        "stderr": "command timed out\n",
        "timed_out": true,
        "timeout_secs": build_wall().as_secs(),
    });
    let (text, class) = super::shell::confined_result(
        r#"cargo test; echo "EXIT=$?""#,
        &envelope,
        &crate::caveats::Caveats::top(),
        false,
        |envelope| {
            format!(
                "{}{}",
                envelope["stdout"].as_str().unwrap_or_default(),
                envelope["stderr"].as_str().unwrap_or_default()
            )
        },
        Default::default(),
    );
    assert_eq!(class, crate::ExecOutcome::TimedOut);
    assert!(
        text.contains(&format!("{}s", build_wall().as_secs())),
        "missing the build wall in: {text}"
    );
    assert!(
        text.contains(&format!("not the default {}s", default_wall().as_secs())),
        "missing the default-wall callout in: {text}"
    );
}

/// F20 regression: the model sends no `timeout_secs`, so `ShellTool` uses
/// `default_timeout_secs`. Raising only `max_timeout_secs` left a build
/// command on the SafeSubset engine killed at 60s.
#[test]
fn the_wall_is_the_default_timeout_not_just_the_ceiling() {
    let _env = crate::process_env::lock();
    let build = super::shell::shell_limits(build_wall());
    assert_eq!(build.default_timeout_secs, build_wall().as_secs());
    assert!(build.max_timeout_secs >= build.default_timeout_secs);
    let ordinary = super::shell::shell_limits(default_wall());
    assert_eq!(ordinary.default_timeout_secs, default_wall().as_secs());
}

/// A working-directory prefix preserves the classified build budget.
#[test]
fn a_cd_prefix_keeps_the_build_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall("cd newt-git && cargo test", Default::default(),),
        build_wall()
    );
}

#[test]
fn an_env_prefix_keeps_the_build_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall("RUSTC_WRAPPER= cargo test -p newt-git", Default::default(),),
        build_wall()
    );
}

/// #2732: a version probe must not lend a runaway filesystem search a build budget.
#[test]
fn issue_2732_probe_then_search_keeps_command_budget() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall(
            "cargo --version; find / -maxdepth 6 -name camino",
            Default::default(),
        ),
        default_wall()
    );
}

/// #2732: expose the existing executor's bounded per-call budget to the model.
#[test]
fn issue_2732_command_catalog_has_capped_override() {
    let _env = crate::process_env::lock();
    let definitions = crate::agentic::tools::tool_definitions();
    let command = definitions
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["function"]["name"] == "run_command")
        .unwrap();
    assert_eq!(
        command["function"]["parameters"]["properties"]["timeout_secs"]["minimum"],
        1
    );
    assert_eq!(
        command["function"]["parameters"]["properties"]["timeout_secs"]["maximum"],
        300
    );
}

/// #2732: injected configuration and per-call budgets cannot disable or exceed the ceiling.
#[test]
fn issue_2732_budget_configuration_is_bounded() {
    let _env = crate::process_env::lock();
    use super::shell::command_budget_seconds as budget;
    assert_eq!(budget(None, None), 60);
    assert_eq!(budget(Some("90"), None), 90);
    assert_eq!(budget(Some("90"), Some(10)), 10);
    assert_eq!(budget(Some("90"), Some(0)), 90);
    for raw in ["", "bad", "0", "-1"] {
        assert_eq!(budget(Some(raw), None), 60);
    }
    assert_eq!(budget(Some("999999"), None), 300);
    assert_eq!(budget(None, Some(u64::MAX)), 300);
}

/// #2732: result text names the actual budget, not a command-name heuristic.
#[test]
fn issue_2732_timeout_reports_selected_budget() {
    let _env = crate::process_env::lock();
    let envelope = serde_json::json!({"exit_code": 124, "timed_out": true, "timeout_secs": 17});
    let (text, outcome) = super::shell::confined_result(
        "cargo --version; find /",
        &envelope,
        &crate::caveats::Caveats::top(),
        false,
        |_| "partial".into(),
        Default::default(),
    );
    assert_eq!(outcome, crate::ExecOutcome::TimedOut);
    assert!(text.contains("17s wall"), "{text}");
    assert!(text.contains("killed"), "{text}");
    assert!(!text.contains("17s build-lane"), "{text}");
}

/// #2732 round 2: a prepared compound build must survive the ordinary deadline.
#[test]
fn round2_compound_build_retains_build_budget() {
    let _env = crate::process_env::lock();
    let wall = super::shell::dispatch_wall(
        "cargo check --workspace && echo checked",
        Default::default(),
    );
    assert_eq!(wall, build_wall());
    let elapsed = Duration::from_secs(61);
    assert!(elapsed < wall);
    assert!(
        elapsed
            >= super::shell::dispatch_wall(
                "cargo --version; find / -maxdepth 6 -name camino",
                Default::default(),
            )
    );
}

/// #2732 round 2: probe/help commands stay ordinary, including wrappers and paths.
#[test]
fn round2_only_classified_build_work_gets_long_budget() {
    let _env = crate::process_env::lock();
    for command in [
        "cargo --version; find / -maxdepth 6",
        "cargo check --help; find /",
        "just --list; find /",
        "make --version; find /",
    ] {
        assert_eq!(
            super::shell::dispatch_wall(command, Default::default(),),
            default_wall(),
            "{command}"
        );
    }
    for command in [
        "cargo +stable check --workspace && echo checked",
        "cargo --offline --color never check --workspace && echo checked",
        "just test integration && echo checked",
        "timeout 100 cargo check --workspace && echo checked",
        "/usr/bin/cargo check --workspace && echo checked",
        "make all && echo checked",
    ] {
        assert_eq!(
            super::shell::dispatch_wall(command, Default::default(),),
            build_wall(),
            "{command}"
        );
    }
    assert_eq!(
        super::shell::command_wall("cargo check && echo checked", Some(20), Default::default(),),
        Duration::from_secs(20)
    );
}

/// #2747: catalog, ordinary dispatcher, safe-subset limits, and timeout notes
/// use the same captured configuration; per-call/build exceptions stay bounded.
#[test]
fn command_budget_2747_catalog_and_dispatch_agree() {
    for value in ["1", "17", "900"] {
        let budget = crate::RunCommandBudget::from_configured(Some(value));
        let seconds = budget.seconds(None);
        let defs = super::catalog::tool_definitions_with_budget(budget);
        let description = defs[0]["function"]["description"].as_str().unwrap();
        assert!(description.contains(&format!("killed after {seconds} seconds")));
        let wall = super::shell::command_wall("echo ready", None, budget);
        assert_eq!(wall.as_secs(), seconds);
        assert_eq!(
            super::shell::shell_limits(wall).default_timeout_secs,
            seconds
        );
        assert!(super::shell::timed_out_note(wall, budget).contains(&format!("{seconds}s wall")));
        assert_eq!(
            super::shell::command_wall("echo ready", Some(0), budget),
            wall
        );
        assert_eq!(
            super::shell::command_wall("echo ready", Some(999), budget).as_secs(),
            300
        );
        assert_eq!(
            super::shell::command_wall("cargo check", None, budget),
            build_wall()
        );
        assert_eq!(
            super::shell::command_wall("cargo check", Some(17), budget).as_secs(),
            17
        );
        assert_eq!(
            super::shell::command_wall("cargo --version", None, budget),
            wall
        );
    }
}

/// #2747: ground the pure budget selection in a host-dispatch envelope without
/// measuring elapsed time or changing the process-wide timeout environment.
#[cfg(unix)]
#[tokio::test]
async fn command_budget_2747_host_dispatch_carries_captured_budget() {
    let budget = crate::RunCommandBudget::from_configured(Some("17"));
    let envelope = super::shell::host_shell_dispatch(
        super::shell::ShellRoute::BashSh,
        "printf ready",
        ".",
        None,
        None,
        budget,
    )
    .await
    .unwrap();
    assert_eq!(envelope["timeout_secs"], 17);
    assert_eq!(envelope["exit_code"], 0);
    assert_eq!(envelope["stdout"], "ready");
}
