//! #2274 native 0.8 acceptance. The old and candidate operation share the
//! identical request and effective caveats. The unfenced control runs that
//! same executable/argv/env/cwd and is test-only.
#![cfg(all(target_os = "linux", feature = "linux-landlock"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agent_bridle_core::{
    Caveats, ChildNetworkPolicy, CountBound, EnforcementFloor, ExecutionEvent, ExecutionEventKind,
    ExecutionRequest, ExitEvidence, Gate, LocalExecutionBackend, SandboxKind, SandboxPolicy, Scope,
    Tool, ToolContext, ToolEnvelope, ToolResult,
};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    workspace: PathBuf,
    root: PathBuf,
    child: PathBuf,
    grandchild: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_root_under_private_marker(false)
    }

    fn with_root_under_private_marker(private: bool) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "bridle-2274-native-{}-{}",
            std::process::id(),
            FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).expect("new private fixture directory");
        let directory = directory.canonicalize().unwrap();
        let workspace = directory.join("workspace");
        let tools = directory.join("tools");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&tools).unwrap();
        let source = std::env::current_exe().unwrap();
        let root = if private {
            std::fs::create_dir(tools.join(".newt")).unwrap();
            tools.join(".newt/admitted-root")
        } else {
            tools.join("admitted-root")
        };
        let child = tools.join("child-image");
        let grandchild = tools.join("grandchild-image");
        for executable in [&root, &child, &grandchild] {
            std::fs::copy(&source, executable).unwrap();
        }
        let outside = directory.join("outside-secret");
        std::fs::write(&outside, b"outside-control-sentinel").unwrap();
        std::fs::write(workspace.join("source"), b"inside-control-sentinel").unwrap();
        Self {
            directory,
            workspace,
            root,
            child,
            grandchild,
            outside,
        }
    }

    fn caveats(&self) -> Caveats {
        Caveats {
            fs_read: Scope::only([
                self.workspace.display().to_string(),
                self.child.parent().unwrap().display().to_string(),
            ]),
            fs_write: Scope::only([self.workspace.display().to_string()]),
            exec: Scope::only([self.root.display().to_string()]),
            net: Scope::none(),
            ..Caveats::top()
        }
    }

    fn request(&self, action: &str) -> ExecutionRequest {
        ExecutionRequest::new(self.root.display().to_string())
            .args(["--exact", "native_child_entry", "--nocapture"])
            .cwd(self.workspace.clone())
            .env("BRIDLE_HOSTROOT_ROLE", "root")
            .env("BRIDLE_HOSTROOT_ACTION", action)
            .env("BRIDLE_HOSTROOT_CHILD", self.child.display().to_string())
            .env(
                "BRIDLE_HOSTROOT_GRANDCHILD",
                self.grandchild.display().to_string(),
            )
            .env(
                "BRIDLE_HOSTROOT_WORKSPACE",
                self.workspace.display().to_string(),
            )
            .env(
                "BRIDLE_HOSTROOT_OUTSIDE",
                self.outside.display().to_string(),
            )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

struct HarnessTool;

#[async_trait::async_trait]
impl Tool for HarnessTool {
    fn name(&self) -> &str {
        "named_host_root_native"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn invoke(
        &self,
        _args: serde_json::Value,
        _cx: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

fn protected_roots() -> std::collections::BTreeSet<String> {
    ["/private-operator-state/store".to_owned()].into()
}

fn context(caveats: &Caveats) -> ToolContext {
    Gate::with_budget(0, CountBound::Unlimited)
        .with_enforcement_floor(EnforcementFloor::CONFINED)
        .authorize(&HarnessTool, caveats)
        .unwrap()
}

struct Run {
    stdout: String,
    stderr: String,
    exit: Option<ExitEvidence>,
    started: bool,
    denial: Option<String>,
    started_event: Option<ExecutionEvent>,
    terminal: Option<ExecutionEvent>,
}

fn run_current_operation(cx: &ToolContext, request: ExecutionRequest) -> Run {
    run_operation(cx, request, true)
}

fn run_operation(cx: &ToolContext, request: ExecutionRequest, named: bool) -> Run {
    run_with_inventory(cx, request, named, protected_roots())
}

fn run_with_inventory(
    cx: &ToolContext,
    request: ExecutionRequest,
    named: bool,
    protected: std::collections::BTreeSet<String>,
) -> Run {
    let policy = SandboxPolicy {
        child_network: ChildNetworkPolicy::DenyDirect,
        named_root_protected_roots: Some(protected),
        ..SandboxPolicy::default()
    };
    let backend = LocalExecutionBackend::with_sandbox_policy(Arc::new(policy));
    // Only the operation changes from the measured existing-0.8 red; the
    // effective read grants, request and controls remain identical.
    let mut handle = if named {
        backend.start_named_root(cx, request)
    } else {
        backend.start(cx, request)
    }
    .unwrap();
    let mut run = Run {
        stdout: String::new(),
        stderr: String::new(),
        exit: None,
        started: false,
        denial: None,
        started_event: None,
        terminal: None,
    };
    while let Some(event) = handle.next_event() {
        if matches!(event.kind, ExecutionEventKind::Started { .. }) {
            run.started_event = Some(event.clone());
        }
        if event.is_terminal() {
            run.terminal = Some(event.clone());
        }
        match event.kind {
            ExecutionEventKind::Started { fence, .. } => {
                assert_eq!(fence.sandbox_kind, SandboxKind::Landlock);
                run.started = true;
            }
            ExecutionEventKind::Stdout(bytes) => {
                run.stdout.push_str(&String::from_utf8_lossy(&bytes))
            }
            ExecutionEventKind::Stderr(bytes) => {
                run.stderr.push_str(&String::from_utf8_lossy(&bytes))
            }
            ExecutionEventKind::Exited(evidence) => run.exit = Some(*evidence),
            ExecutionEventKind::Denied { denial } => run.denial = Some(denial.reason),
            ExecutionEventKind::Failed { message } => run.stderr.push_str(&message),
            _ => {}
        }
    }
    let _ = handle.wait();
    run
}

fn unfenced_control(request: &ExecutionRequest) -> std::process::Output {
    let mut command = Command::new(&request.executable);
    command
        .args(&request.argv)
        .env_clear()
        .envs(request.env.iter().map(|(key, value)| (key, value)));
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    command.output().unwrap()
}

fn helper_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).expect("explicit helper path"))
}

fn spawn_helper(executable: &Path, role: &str) -> i32 {
    match Command::new(executable)
        .args(["--exact", "native_child_entry", "--nocapture"])
        .env_clear()
        .envs(std::env::vars())
        .env("BRIDLE_HOSTROOT_ROLE", role)
        .status()
    {
        Ok(status) => status.code().unwrap_or(45),
        Err(error) => {
            eprintln!("DESCENDANT_SPAWN_ERROR:{:?}:{error}", error.kind());
            42
        }
    }
}

#[test]
fn native_child_entry() {
    let Ok(role) = std::env::var("BRIDLE_HOSTROOT_ROLE") else {
        return;
    };
    let code = match role.as_str() {
        "root" => spawn_helper(&helper_path("BRIDLE_HOSTROOT_CHILD"), "child"),
        "child" => spawn_helper(&helper_path("BRIDLE_HOSTROOT_GRANDCHILD"), "grandchild"),
        "grandchild" => {
            let workspace = helper_path("BRIDLE_HOSTROOT_WORKSPACE");
            assert_eq!(
                std::fs::read(workspace.join("source")).unwrap(),
                b"inside-control-sentinel"
            );
            std::fs::write(workspace.join("grandchild-output"), b"inside-write").unwrap();
            println!("GRANDCHILD_INSIDE_OK");
            match std::env::var("BRIDLE_HOSTROOT_ACTION").unwrap().as_str() {
                "inside" => 0,
                "outside-read" => match std::fs::read(helper_path("BRIDLE_HOSTROOT_OUTSIDE")) {
                    Ok(bytes) => {
                        println!("OUTSIDE_READ_OK:{}", String::from_utf8_lossy(&bytes));
                        0
                    }
                    Err(error) => {
                        eprintln!("OUTSIDE_READ_ERROR:{:?}:{error}", error.kind());
                        43
                    }
                },
                "outside-write" => match std::fs::write(
                    helper_path("BRIDLE_HOSTROOT_OUTSIDE"),
                    b"outside-write-control",
                ) {
                    Ok(()) => {
                        println!("OUTSIDE_WRITE_OK");
                        0
                    }
                    Err(error) => {
                        eprintln!("OUTSIDE_WRITE_ERROR:{:?}:{error}", error.kind());
                        44
                    }
                },
                "network" => match std::net::UdpSocket::bind("127.0.0.1:0") {
                    Ok(_) => {
                        println!("NETWORK_SOCKET_OK");
                        0
                    }
                    Err(error) => {
                        eprintln!("NETWORK_SOCKET_ERROR:{:?}:{error}", error.kind());
                        46
                    }
                },
                _ => panic!("unknown helper action"),
            }
        }
        _ => panic!("unknown helper role"),
    };
    std::process::exit(code);
}

#[test]
fn named_root_runs_distinct_descendants_with_unchanged_read_grants() {
    assert!(
        agent_bridle_core::landlock_is_supported(),
        "this run needs actual Landlock"
    );
    assert!(
        agent_bridle_core::landlock_net_is_supported(),
        "this run needs network confinement"
    );
    let fixture = Fixture::new();
    let request = fixture.request("inside");
    let control = unfenced_control(&request);
    assert!(
        control.status.success(),
        "unfenced fixture failed: {control:?}"
    );
    assert!(String::from_utf8_lossy(&control.stdout).contains("GRANDCHILD_INSIDE_OK"));
    std::fs::remove_file(fixture.workspace.join("grandchild-output")).unwrap();

    let caveats = fixture.caveats();
    let run = run_current_operation(&context(&caveats), request);
    eprintln!(
        "native candidate result: started={}, denied={:?}, exit={:?}, stdout={:?}, stderr={:?}",
        run.started,
        run.denial,
        run.exit.as_ref().and_then(|e| e.code),
        run.stdout,
        run.stderr
    );
    assert!(run.started, "must reach the actual root: {:?}", run.denial);
    assert!(
        run.denial.is_none(),
        "must not manufacture an interceptor denial"
    );
    assert_eq!(
        run.exit.as_ref().and_then(|e| e.code),
        Some(0),
        "exact-root admission must permit child/grandchild images under the same fs/net fence"
    );
    assert!(run.stdout.contains("GRANDCHILD_INSIDE_OK"));
    assert!(fixture.workspace.join("grandchild-output").is_file());
}

fn envelope(run: &Run, caveats: &Caveats) -> ToolEnvelope {
    let mut result = ToolEnvelope::new(if run.started {
        SandboxKind::Landlock
    } else {
        SandboxKind::None
    });
    result.stdout = Some(run.stdout.clone());
    result.stderr = Some(run.stderr.clone());
    result.exit_code = run.exit.as_ref().and_then(|e| e.code);
    result.execution_started = run.started_event.clone();
    result.execution = run.terminal.clone();
    if let Some(ExecutionEvent {
        kind: ExecutionEventKind::Started { fence, .. },
        ..
    }) = &run.started_event
    {
        result.enforcement = agent_bridle_core::enforcement_report(
            caveats,
            fence.admitted.as_ref().unwrap().mechanism,
        );
    }
    result
}

#[test]
fn named_root_inherits_read_write_and_socket_denial_with_positive_controls() {
    for (action, code, success, failure) in [
        (
            "outside-read",
            43,
            "OUTSIDE_READ_OK:outside-control-sentinel",
            "OUTSIDE_READ_ERROR:PermissionDenied",
        ),
        (
            "outside-write",
            44,
            "OUTSIDE_WRITE_OK",
            "OUTSIDE_WRITE_ERROR:PermissionDenied",
        ),
        (
            "network",
            46,
            "NETWORK_SOCKET_OK",
            "NETWORK_SOCKET_ERROR:PermissionDenied",
        ),
    ] {
        let fixture = Fixture::new();
        let request = fixture.request(action);
        let control = unfenced_control(&request);
        assert!(control.status.success(), "{action}: {control:?}");
        assert!(String::from_utf8_lossy(&control.stdout).contains(success));
        let caveats = fixture.caveats();
        let run = run_current_operation(&context(&caveats), request);
        eprintln!(
            "native {action}: started={} code={:?} stdout={:?} stderr={:?}",
            run.started,
            run.exit.as_ref().and_then(|e| e.code),
            run.stdout,
            run.stderr
        );
        assert!(
            run.stdout.contains("GRANDCHILD_INSIDE_OK"),
            "{action}: {}",
            run.stderr
        );
        assert_eq!(run.exit.as_ref().and_then(|e| e.code), Some(code));
        assert!(run.stderr.contains(failure), "{}", run.stderr);
        let result = envelope(&run, &caveats);
        result
            .verify_named_root_execution(
                fixture.root.to_str().unwrap(),
                &caveats,
                &protected_roots(),
            )
            .unwrap()
            .unwrap();
        let wire = result.into_json();
        assert!(
            wire.get("denied").is_none(),
            "native refusal is not an interceptor denial"
        );
        assert_eq!(wire["sandbox_kind"], "landlock");
        for axis in ["fs_read", "fs_write", "net"] {
            assert_eq!(wire["enforcement"][axis], "kernel");
        }
        assert_eq!(wire["enforcement"]["exec"], "interceptor");
    }
}

#[test]
fn ordinary_operation_retains_the_measured_descendant_refusal() {
    let fixture = Fixture::new();
    let caveats = fixture.caveats();
    let run = run_operation(&context(&caveats), fixture.request("inside"), false);
    assert!(run.started);
    assert_eq!(run.exit.as_ref().and_then(|e| e.code), Some(42));
    assert!(run
        .stderr
        .contains("DESCENDANT_SPAWN_ERROR:PermissionDenied"));
    let fence = &run.exit.unwrap().fence;
    assert!(
        fence.admitted.is_none(),
        "ordinary proof domain remains unchanged"
    );
    assert_eq!(
        agent_bridle_core::enforcement_report(
            &caveats,
            agent_bridle_core::ConfinementMechanism::new(
                SandboxKind::Landlock,
                ChildNetworkPolicy::DenyDirect
            )
        )
        .exec,
        Some(agent_bridle_core::AxisEnforcement::Interceptor)
    );
}

#[test]
fn actual_producer_pair_round_trip_verifies_and_tampering_refuses() {
    let fixture = Fixture::new();
    let caveats = fixture.caveats();
    let run = run_current_operation(&context(&caveats), fixture.request("inside"));
    let actual = envelope(&run, &caveats);
    let wire = actual.clone().into_json();
    let decoded: ToolEnvelope = serde_json::from_value(wire.clone()).unwrap();
    let root = fixture.root.to_str().unwrap();
    decoded
        .verify_named_root_execution(root, &caveats, &protected_roots())
        .unwrap()
        .unwrap();
    for mutation in [
        "root",
        "domain",
        "projection",
        "effective",
        "backend",
        "id",
        "fence_id",
        "inventory",
        "order",
        "kind",
        "exit_fence",
    ] {
        let mut tampered = actual.clone();
        let event = tampered.execution_started.as_mut().unwrap();
        match mutation {
            "id" => event.execution = agent_bridle_core::ExecutionId::next(),
            "order" => event.sequence = tampered.execution.as_ref().unwrap().sequence,
            "kind" => event.kind = ExecutionEventKind::Accepted,
            "exit_fence" => {
                if let ExecutionEventKind::Exited(exit) =
                    &mut tampered.execution.as_mut().unwrap().kind
                {
                    exit.fence.sandbox_kind = SandboxKind::None;
                }
            }
            _ => {
                let ExecutionEventKind::Started { fence, .. } = &mut event.kind else {
                    unreachable!()
                };
                let body = fence.admitted.as_mut().unwrap();
                match mutation {
                    "root" => body.root = fixture.child.display().to_string(),
                    "domain" => {
                        body.mechanism = agent_bridle_core::ConfinementMechanism::new(
                            SandboxKind::Landlock,
                            ChildNetworkPolicy::DenyDirect,
                        )
                    }
                    "projection" => {
                        body.projection.resolved.fs_write =
                            agent_bridle_core::ResolvedScope::Unbounded
                    }
                    "effective" => body.effective.exec = Scope::All,
                    "backend" => fence.sandbox_kind = SandboxKind::None,
                    "fence_id" => {
                        let changed: agent_bridle_core::AdmittedFenceId = serde_json::from_str(
                            r#""bafyr4ieamrtjdm7e5blaezbyyz6lmbwtkw6ifxmppvwqqwubnvmlxqcvci""#,
                        )
                        .unwrap();
                        fence.fence_id = changed.clone();
                        if let ExecutionEventKind::Exited(exit) =
                            &mut tampered.execution.as_mut().unwrap().kind
                        {
                            exit.fence.fence_id = changed;
                        }
                        assert!(
                            fence
                                .verify_named_root(root, &caveats, &protected_roots())
                                .is_err(),
                            "CID-only corruption must fail without relying on pair inequality"
                        );
                    }
                    "inventory" => {
                        body.protected_roots = ["/different-protected-state".to_owned()].into()
                    }
                    _ => unreachable!(),
                }
            }
        }
        assert!(
            tampered
                .verify_named_root_execution(root, &caveats, &protected_roots())
                .is_err(),
            "{mutation}"
        );
    }
    // Existing event types also preserve a real Started when the terminal is
    // Failed. This terminal substitution is SYNTHETIC transport coverage, not
    // evidence that the native reaper actually failed in this successful run.
    let mut failed = actual.clone();
    failed.execution.as_mut().unwrap().kind = ExecutionEventKind::Failed {
        message: "synthetic finalizer failure".to_owned(),
    };
    assert!(failed
        .verify_named_root_execution(root, &caveats, &protected_roots())
        .unwrap()
        .is_some());
    failed.execution_started = None;
    failed.sandbox_kind = SandboxKind::None;
    failed.enforcement = Default::default();
    assert!(failed
        .verify_named_root_execution(root, &caveats, &protected_roots())
        .unwrap()
        .is_none());

    let mut different = caveats.clone();
    different.exec = Scope::only([root.to_owned(), fixture.child.display().to_string()]);
    let other_run = run_current_operation(&context(&different), fixture.request("inside"));
    assert!(
        envelope(&other_run, &different)
            .verify_named_root_execution(root, &caveats, &protected_roots())
            .is_err(),
        "different valid authority cannot stand in for this invocation"
    );
    let mut other_request = fixture.request("inside");
    other_request.executable = fixture.child.display().to_string();
    let other_run = run_current_operation(&context(&different), other_request);
    assert!(
        envelope(&other_run, &different)
            .verify_named_root_execution(root, &different, &protected_roots())
            .is_err(),
        "same-grant different actual root cannot stand in for this invocation"
    );
}

#[test]
fn actual_no_start_denied_and_failed_terminals_carry_no_execution_proof() {
    let fixture = Fixture::new();
    let caveats = fixture.caveats();
    let mut request = fixture.request("inside");
    request.executable = fixture.child.display().to_string();
    let denied = run_current_operation(&context(&caveats), request);
    assert!(matches!(
        denied.terminal.as_ref().unwrap().kind,
        ExecutionEventKind::Denied { .. }
    ));
    assert!(envelope(&denied, &caveats)
        .verify_named_root_execution(
            fixture.child.to_str().unwrap(),
            &caveats,
            &protected_roots()
        )
        .unwrap()
        .is_none());
    let missing = fixture.root.parent().unwrap().join("missing-root");
    let mut missing_grant = caveats;
    missing_grant.exec = Scope::only([missing.display().to_string()]);
    let mut request = fixture.request("inside");
    request.executable = missing.display().to_string();
    let failed = run_current_operation(&context(&missing_grant), request);
    assert!(
        matches!(
            failed.terminal.as_ref().unwrap().kind,
            ExecutionEventKind::Failed { .. }
        ),
        "{:?}",
        failed.terminal
    );
    assert!(envelope(&failed, &missing_grant)
        .verify_named_root_execution(
            missing.to_str().unwrap(),
            &missing_grant,
            &protected_roots()
        )
        .unwrap()
        .is_none());
}

#[test]
fn explicitly_read_and_exec_granted_root_under_private_marker_runs() {
    let fixture = Fixture::with_root_under_private_marker(true);
    let caveats = fixture.caveats();
    let protected = [fixture.root.parent().unwrap().display().to_string()].into();
    let run = run_with_inventory(
        &context(&caveats),
        fixture.request("inside"),
        true,
        protected,
    );
    eprintln!(
        "explicit private root: started={} denied={:?} exit={:?} stderr={:?}",
        run.started,
        run.denial,
        run.exit.as_ref().and_then(|e| e.code),
        run.stderr
    );
    assert!(run.started, "{:?}", run.denial);
    assert_eq!(run.exit.as_ref().and_then(|e| e.code), Some(0));
    assert!(run.stdout.contains("GRANDCHILD_INSIDE_OK"));
}

#[test]
fn fence_disabled_positive_control_reads_the_known_outside_sentinel() {
    let fixture = Fixture::new();
    let control = unfenced_control(&fixture.request("outside-read"));
    assert!(
        control.status.success(),
        "unfenced control failed: {control:?}"
    );
    assert!(String::from_utf8_lossy(&control.stdout)
        .contains("OUTSIDE_READ_OK:outside-control-sentinel"));
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
