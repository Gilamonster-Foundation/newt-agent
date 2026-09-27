//! #2274 production Brush routing, native fence, and managed evidence tests.
//! The test image handles the real private-worker entry before fixture setup.

use std::process::{Command, Stdio};
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
use std::time::Duration;

use agent_bridle_core::{Caveats, Gate, Tool, ToolContext, ToolEnvelope};
use agent_bridle_tool_shell::BrushShellTool;

#[path = "named_host_root_real/observation.rs"]
mod observation;

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
#[path = "named_host_root_real/lifecycle.rs"]
mod lifecycle;

#[path = "named_host_root_real/fixture.rs"]
mod fixture;
use fixture::{probe, Fixture};

fn main() {
    if let Some(code) = agent_bridle_tool_shell::maybe_dispatch() {
        std::process::exit(code);
    }
    if std::env::args().nth(1).as_deref() == Some("--host-root-probe") {
        std::process::exit(probe());
    }
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.block_on(run_cases());
}

fn context(caveats: &Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&BrushShellTool::new(), caveats)
        .unwrap()
}

fn selected(name: &str) -> bool {
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    filters.is_empty() || filters.iter().any(|filter| name.contains(filter))
}

async fn run_cases() {
    #[cfg(all(target_os = "linux", feature = "linux-landlock"))]
    {
        assert!(
            agent_bridle_core::landlock_is_supported(),
            "native suite requires actual Landlock"
        );
        assert!(
            agent_bridle_core::landlock_net_is_supported(),
            "native suite requires network confinement"
        );
        for name in [
            "named_root_real_producer_envelope_round_trip_verifies",
            "named_root_outside_read_has_an_unconfined_fixture_control",
            "named_root_disabled_and_valid_fallback_preserve_brush_runtime",
            "named_root_timeout_retains_terminal_and_stops_descendants",
            "named_root_cancel_drops_the_managed_tree",
            "named_root_observer_is_live_and_matches_bounded_output",
            "named_root_cancellation_does_not_block_current_thread",
        ] {
            if !selected(name) {
                continue;
            }
            eprintln!("test {name} ...");
            match name {
                "named_root_real_producer_envelope_round_trip_verifies" => {
                    producer_round_trip().await
                }
                "named_root_outside_read_has_an_unconfined_fixture_control" => {
                    fence_control().await
                }
                "named_root_disabled_and_valid_fallback_preserve_brush_runtime" => {
                    ordinary_fallback().await
                }
                "named_root_timeout_retains_terminal_and_stops_descendants" => timeout_tree().await,
                "named_root_cancel_drops_the_managed_tree" => cancel_tree().await,
                "named_root_observer_is_live_and_matches_bounded_output" => {
                    observation::observer_is_live_and_matches_bounded_output().await
                }
                "named_root_cancellation_does_not_block_current_thread" => {
                    lifecycle::cancellation_does_not_block_current_thread()
                }
                _ => unreachable!(),
            }
            eprintln!("test {name} ... ok");
        }
    }
    #[cfg(not(all(target_os = "linux", feature = "linux-landlock")))]
    if selected("named_root_unsupported_backend_refuses_before_root_side_effect") {
        eprintln!("test named_root_unsupported_backend_refuses_before_root_side_effect ...");
        let fixture = Fixture::new();
        // Establish that this executable and workspace can perform the exact
        // side effect, so unsupported refusal cannot pass due to a bad image.
        let control = Command::new(&fixture.root)
            .args(["--host-root-probe", "inside"])
            .current_dir(&fixture.workspace)
            .env_clear()
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            control.status.success(),
            "unconfined fixture must run: {control:?}"
        );
        assert!(fixture.workspace.join("inside-result").exists());
        std::fs::remove_file(fixture.workspace.join("inside-result")).unwrap();
        let value = fixture
            .tool(true)
            .invoke(fixture.arguments("inside"), &context(&fixture.caveats()))
            .await
            .unwrap();
        assert_eq!(
            value["denied"], true,
            "unsupported backend must refuse: {value}"
        );
        assert!(
            value.get("execution_started").is_none(),
            "no child may have started: {value}"
        );
        assert_eq!(value["sandbox_kind"], "none");
        assert!(
            value.get("enforcement").is_none(),
            "a refusal has no native report"
        );
        assert!(
            value.get("exit_code").is_none(),
            "a refusal has no child exit"
        );
        let envelope: ToolEnvelope = serde_json::from_value(value).unwrap();
        assert!(envelope
            .verify_named_root_execution(
                &fixture.root.display().to_string(),
                &fixture.caveats(),
                &fixture.protected_roots()
            )
            .unwrap()
            .is_none());
        assert!(matches!(
            envelope.execution.unwrap().kind,
            agent_bridle_core::ExecutionEventKind::Denied { .. }
        ));
        assert!(!fixture.workspace.join("inside-result").exists());
        eprintln!("test named_root_unsupported_backend_refuses_before_root_side_effect ... ok");
    }
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
fn verify(value: &serde_json::Value, fixture: &Fixture) -> ToolEnvelope {
    assert!(
        value.get("denied").is_none(),
        "no interceptor denial: {value}"
    );
    assert_eq!(value["sandbox_kind"], "landlock");
    for axis in ["fs_read", "fs_write", "net"] {
        assert_eq!(value["enforcement"][axis], "kernel");
    }
    assert_eq!(value["enforcement"]["exec"], "interceptor");
    let envelope: ToolEnvelope = serde_json::from_value(value.clone()).unwrap();
    assert!(envelope
        .verify_named_root_execution(
            &fixture.root.display().to_string(),
            &fixture.caveats(),
            &fixture.protected_roots()
        )
        .unwrap()
        .is_some());
    envelope
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
async fn producer_round_trip() {
    let fixture = Fixture::new();
    let value = fixture
        .tool(true)
        .invoke(
            fixture.arguments("environment"),
            &context(&fixture.caveats()),
        )
        .await
        .unwrap();
    assert_eq!(value["exit_code"], 0, "{value}");
    assert_eq!(value["stdout"], "ENV:explicit-value\nPATH:explicit-path\n");
    assert_eq!(value["stderr"], "ROOT_STDERR\n");
    let mut envelope = verify(&value, &fixture);
    let started = envelope.execution_started.as_ref().unwrap();
    let terminal = envelope.execution.as_ref().unwrap();
    assert_eq!(started.execution, terminal.execution);
    assert!(started.sequence < terminal.sequence);
    if let agent_bridle_core::ExecutionEventKind::Started { fence, .. } =
        &mut envelope.execution_started.as_mut().unwrap().kind
    {
        fence.admitted.as_mut().unwrap().root = fixture.child.display().to_string();
    } else {
        panic!("actual Started required");
    }
    assert!(envelope
        .verify_named_root_execution(
            &fixture.root.display().to_string(),
            &fixture.caveats(),
            &fixture.protected_roots()
        )
        .is_err());
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
async fn fence_control() {
    let fixture = Fixture::new();
    let caveats = fixture.caveats();
    let inside = fixture
        .tool(true)
        .invoke(fixture.arguments("forward inside"), &context(&caveats))
        .await
        .unwrap();
    assert_eq!(
        inside["exit_code"], 0,
        "descendant positive control: {inside}"
    );
    assert!(fixture.workspace.join("inside-result").exists());
    verify(&inside, &fixture);
    let arguments = fixture.arguments("forward outside");
    let fenced = fixture
        .tool(true)
        .invoke(arguments.clone(), &context(&caveats))
        .await
        .unwrap();
    verify(&fenced, &fixture);
    assert_eq!(
        fenced["exit_code"], 43,
        "known read must reach the native denial: {fenced}"
    );
    assert!(!fenced["stdout"]
        .as_str()
        .unwrap()
        .contains("known-readable-outside-value"));
    assert!(fenced["stderr"]
        .as_str()
        .unwrap()
        .contains("OUTSIDE_READ_ERROR:PermissionDenied"));

    // Fixture-positive control: identical root, argv, cwd, env and null stdin
    // through std::Command. This establishes that the sentinel is readable and
    // the fixture works, but is not a mutation of the production OS-apply path.
    // That stricter mutation remains a separate acceptance gate.
    let (root, argv) =
        agent_bridle_tool_shell::lower_named_host_root_command(arguments["cmd"].as_str().unwrap())
            .unwrap()
            .unwrap();
    let control = Command::new(root)
        .args(argv)
        .current_dir(&fixture.workspace)
        .env_clear()
        .envs(
            arguments["env"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key, value.as_str().unwrap())),
        )
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        control.status.success(),
        "disabled-fence control: {control:?}"
    );
    assert!(
        String::from_utf8_lossy(&control.stdout).contains("OUTSIDE:known-readable-outside-value")
    );
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
async fn ordinary_fallback() {
    let fixture = Fixture::new();
    let caveats = fixture.caveats();
    let mut arguments = fixture.arguments("literal first");
    let ordinary = fixture
        .tool(false)
        .invoke(arguments.clone(), &context(&caveats))
        .await
        .unwrap();
    assert_eq!(
        ordinary["exit_code"], 0,
        "default literal control: {ordinary}"
    );
    assert_eq!(ordinary["enforcement"]["exec"], "interceptor");
    assert!(ordinary.get("execution").is_none());
    for command in [
        format!(
            "'{}' --host-root-probe literal first; '{}' --host-root-probe literal second",
            fixture.root.display(),
            fixture.root.display()
        ),
        format!(
            "'{}' --host-root-probe literal \"$ROOT_TOKEN\"",
            fixture.root.display()
        ),
    ] {
        arguments["cmd"] = command.into();
        let default = fixture
            .tool(false)
            .invoke(arguments.clone(), &context(&caveats))
            .await
            .unwrap();
        let opt_in = fixture
            .tool(true)
            .invoke(arguments.clone(), &context(&caveats))
            .await
            .unwrap();
        assert_eq!(
            default, opt_in,
            "valid noncandidate must use identical ordinary behavior"
        );
        assert_eq!(opt_in["exit_code"], 0, "{opt_in}");
        assert!(opt_in.get("execution").is_none());
        assert!(opt_in.get("execution_started").is_none());
    }
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
async fn timeout_tree() {
    let fixture = Fixture::new();
    let value = fixture
        .tool(true)
        .with_timeout(Duration::from_millis(250))
        .invoke(
            fixture.arguments("wait-child"),
            &context(&fixture.caveats()),
        )
        .await
        .unwrap();
    assert!(
        fixture.workspace.join("root-started").exists(),
        "timeout must reach a live descendant"
    );
    assert_eq!(value["exit_code"], 124, "{value}");
    assert_eq!(value["timed_out"], true);
    verify(&value, &fixture);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !fixture.workspace.join("late-marker").exists(),
        "terminal must quiesce descendants"
    );
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
async fn cancel_tree() {
    let fixture = Fixture::new();
    let arguments = fixture.arguments("wait-child");
    let cx = context(&fixture.caveats());
    let tool = fixture.tool(true);
    let invocation = tokio::spawn(async move { tool.invoke(arguments, &cx).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.workspace.join("root-started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a real child must start before cancellation");
    invocation.abort();
    assert!(invocation.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !fixture.workspace.join("late-marker").exists(),
        "dropping the adapter must cancel its managed tree"
    );
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
