//! #2732: ordinary shell commands never inherit the build executor budget.

use std::time::Duration;

fn build_wall() -> Duration {
    super::shell::LIFECYCLE_BUILD_TIMEOUT
}

fn default_wall() -> Duration {
    Duration::from_secs(agent_bridle::LimitsPolicy::default().default_timeout_secs)
}

/// #2732: a compound build-bearing shell keeps the command budget; explicit
/// build_exec/lifecycle calls own the longer build budget.
#[test]
fn a_compound_cargo_command_keeps_the_command_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall(r#"cargo test -j 4 -p newt-git; echo "EXIT=$?""#),
        default_wall()
    );
}

/// `just <recipe>` is the other program in #2533's build-lane table.
#[test]
fn a_just_command_keeps_the_command_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(super::shell::dispatch_wall("just test"), default_wall());
}

/// An ordinary command is unaffected — the default wall still applies.
#[test]
fn a_plain_command_keeps_the_default_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(super::shell::dispatch_wall("ls -la"), default_wall());
}

/// Mentioning Cargo does not widen an ordinary command budget.
#[test]
fn cargo_mentioned_later_does_not_widen_the_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(super::shell::dispatch_wall("echo cargo"), default_wall());
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

/// A working-directory prefix does not change the ordinary command budget.
#[test]
fn a_cd_prefix_keeps_the_default_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall("cd newt-git && cargo test"),
        default_wall()
    );
}

#[test]
fn an_env_prefix_keeps_the_command_wall() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall("RUSTC_WRAPPER= cargo test -p newt-git"),
        default_wall()
    );
}

/// #2732: a version probe must not lend a runaway filesystem search a build budget.
#[test]
fn issue_2732_probe_then_search_keeps_command_budget() {
    let _env = crate::process_env::lock();
    assert_eq!(
        super::shell::dispatch_wall("cargo --version; find / -maxdepth 6 -name camino"),
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
    );
    assert_eq!(outcome, crate::ExecOutcome::TimedOut);
    assert!(text.contains("17s wall"), "{text}");
    assert!(text.contains("killed"), "{text}");
    assert!(!text.contains("17s build-lane"), "{text}");
}
