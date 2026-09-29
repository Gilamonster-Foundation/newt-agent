//! F20: a build command gets the build lane's wall even when `run_command`'s
//! own #2533 routing refuses to route it (a compound command, e.g. the model's
//! habitual `cargo test …; echo "EXIT=$?"`). Three campaigns died at the 60s
//! wall before this: the router correctly refuses a compound argv, but that
//! left the confined SHELL lane — where the refused call actually runs —
//! still applying the ordinary 60s wall to a build. `dispatch_wall` reads only
//! the command's leading program (`shell::leading_program`, `routing::
//! is_build_tool_program` — the SAME table #2533's routing already uses, not
//! a second list); it never touches authority.

use std::time::Duration;

fn build_wall() -> Duration {
    super::shell::LIFECYCLE_BUILD_TIMEOUT
}

fn default_wall() -> Duration {
    Duration::from_secs(agent_bridle::LimitsPolicy::default().default_timeout_secs)
}

/// The measured failure mode itself: a compound cargo command — the shape the
/// model always writes — gets the 30-minute build wall, not the 60s one that
/// killed it before.
#[test]
fn a_compound_cargo_command_gets_the_build_wall() {
    assert_eq!(
        super::shell::dispatch_wall(r#"cargo test -j 4 -p newt-git; echo "EXIT=$?""#),
        build_wall()
    );
}

/// `just <recipe>` is the other program in #2533's build-lane table.
#[test]
fn a_just_command_gets_the_build_wall() {
    assert_eq!(super::shell::dispatch_wall("just test"), build_wall());
}

/// An ordinary command is unaffected — the default wall still applies.
#[test]
fn a_plain_command_keeps_the_default_wall() {
    assert_eq!(super::shell::dispatch_wall("ls -la"), default_wall());
}

/// `cargo` mentioned anywhere OTHER than the leading program does not widen
/// the wall — only the program actually being run does.
#[test]
fn cargo_mentioned_later_does_not_widen_the_wall() {
    assert_eq!(super::shell::dispatch_wall("echo cargo"), default_wall());
}

/// A timed-out result names the wall that actually applied, so a reader can
/// tell why a call ran for minutes instead of assuming the ordinary 60s.
#[test]
fn a_timed_out_build_command_names_the_build_wall_in_its_result() {
    let envelope = serde_json::json!({
        "exit_code": 124,
        "stdout": "partial",
        "stderr": "command timed out\n",
        "timed_out": true,
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
    let build = super::shell::shell_limits(build_wall());
    assert_eq!(build.default_timeout_secs, build_wall().as_secs());
    assert!(build.max_timeout_secs >= build.default_timeout_secs);
    let ordinary = super::shell::shell_limits(default_wall());
    assert_eq!(ordinary.default_timeout_secs, default_wall().as_secs());
}

/// Pins the known gap: only the LEADING program counts, so a `cd x &&`
/// prefix keeps the default wall (the model should pass `cwd=` instead).
#[test]
fn a_cd_prefix_keeps_the_default_wall() {
    assert_eq!(
        super::shell::dispatch_wall("cd newt-git && cargo test"),
        default_wall()
    );
}

#[test]
fn an_env_prefix_still_gets_the_build_wall() {
    assert_eq!(
        super::shell::dispatch_wall("RUSTC_WRAPPER= cargo test -p newt-git"),
        build_wall()
    );
}
