//! Synthetic stream regression; this does not claim a native execution ran.

use super::*;
use agent_bridle_core::{
    empty_closure, execution_stream, AdmittedFence, BackendProjection, Caveats, ChildNetworkPolicy,
    ConfinementMechanism, EnforcementFloor, ExecutionControl, ExecutionEmit, ExecutionId,
    ExecutionLimits, ExecutionTerminal, FenceEvidence, Gate, OutputStream, ResolvedAuthority,
    ResolvedScope, Scope,
};

struct FailedControl;

impl ExecutionControl for FailedControl {
    fn wait(&self) -> ToolResult<ExecutionTerminal> {
        Ok(ExecutionTerminal::Failed {
            message: "synthetic finalizer failure".into(),
        })
    }
    fn cancel(&self) -> ToolResult<()> {
        Ok(())
    }
    fn kill(&self) -> ToolResult<()> {
        Ok(())
    }
    fn abandon(&self) {}
}

/// #2274: the reserved drop notice must survive Failed, whose shape deliberately
/// carries no ExitEvidence. Previously only Exited set the truncation flags.
#[tokio::test]
async fn named_host_root_failed_terminal_keeps_reserved_output_truncation() {
    let root = if cfg!(windows) {
        r"C:\tools\root.exe"
    } else {
        "/tools/root"
    };
    let workspace = std::env::temp_dir().join("bridle-synthetic-workspace");
    let protected: BTreeSet<String> = [std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join("bridle-synthetic-protected-state")
        .display()
        .to_string()]
    .into();
    let caveats = Caveats {
        fs_read: Scope::only([workspace.display().to_string()]),
        fs_write: Scope::only([workspace.display().to_string()]),
        exec: Scope::only([root.to_string()]),
        ..Caveats::top()
    };
    let policy = Arc::new(SandboxPolicy {
        child_network: ChildNetworkPolicy::DenyDirect,
        named_root_protected_roots: Some(protected.clone()),
        ..SandboxPolicy::default()
    });
    let mechanism =
        ConfinementMechanism::for_named_root(SandboxKind::Landlock, policy.child_network);
    let admitted = AdmittedFence::admit_named_root(
        &caveats,
        root,
        &protected,
        mechanism,
        EnforcementFloor::CONFINED,
        |effective| {
            let mut resolved = ResolvedAuthority::from_delegated(effective);
            resolved.exec = ResolvedScope::Unbounded;
            BackendProjection {
                resolved,
                runtime_closure: empty_closure(),
            }
        },
    )
    .unwrap();
    let fence = FenceEvidence {
        fence_id: admitted.fence_id().clone(),
        sandbox_kind: SandboxKind::Landlock,
        egress_proxied: false,
        admitted: admitted.admitted_body().cloned().map(Box::new),
    };
    let limits = ExecutionLimits::new(2, 16, 16, Duration::ZERO, Duration::ZERO).unwrap();
    let (sink, handle) = execution_stream(ExecutionId::next(), limits, Arc::new(FailedControl));
    sink.accepted().unwrap();
    sink.started(1, fence).unwrap();
    assert_eq!(
        sink.output(OutputStream::Stdout, b"lost").unwrap(),
        ExecutionEmit::Dropped
    );
    assert_eq!(
        sink.output(OutputStream::Stderr, b"lost").unwrap(),
        ExecutionEmit::Dropped
    );
    sink.publish_terminal(ExecutionTerminal::Failed {
        message: "synthetic finalizer failure".into(),
    })
    .unwrap();
    let collector = NamedRootCollector {
        cx: Gate::new(0)
            .authorize(&crate::BrushShellTool::new(), &caveats)
            .unwrap(),
        root: root.into(),
        policy,
        deadline: Instant::now() + Duration::from_secs(5),
        timeout: Duration::from_secs(5),
        max_output: 64,
        output: OutputEmitter::default(),
        disclosure: Disclosure::default(),
    };
    let envelope = collector.collect(handle, &AtomicBool::new(false)).unwrap();
    assert!(envelope.execution_started.is_some());
    assert!(matches!(
        envelope.execution.as_ref().unwrap().kind,
        ExecutionEventKind::Failed { .. }
    ));
    assert!(
        envelope.exit_code.is_none(),
        "a failed finalizer must not mint an exit"
    );
    assert!(
        !envelope.denied,
        "a failed finalizer is not a policy refusal"
    );
    assert!(
        envelope.stdout_truncated,
        "reserved stdout loss was discarded"
    );
    assert!(
        envelope.stderr_truncated,
        "reserved stderr loss was discarded"
    );
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
