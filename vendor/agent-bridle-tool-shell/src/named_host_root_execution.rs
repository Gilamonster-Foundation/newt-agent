//! Adapt the managed execution stream without manufacturing terminal evidence.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_bridle_core::{
    enforcement_report, Disclosure, ExecutionEventKind, ExecutionHandle, ExecutionRequest,
    LocalExecutionBackend, SandboxKind, SandboxPolicy, ToolContext, ToolEnvelope, ToolError,
    ToolResult,
};

use crate::output_observer::{output_session, OutputEmitter};
use crate::{ShellOutputObserver, ShellOutputStream};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn invoke_named_host_root(
    cx: &ToolContext,
    request: ExecutionRequest,
    policy: Arc<SandboxPolicy>,
    timeout: Duration,
    max_output: usize,
    observer: Option<Arc<dyn ShellOutputObserver>>,
    disclosure: Disclosure,
    execution_lease: Option<crate::ExecutionLease>,
) -> ToolResult<serde_json::Value> {
    let (guard, output) = output_session(observer, max_output);
    let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancelled = Arc::clone(&cancellation.0);
    let collector = NamedRootCollector {
        cx: cx.clone(),
        root: request.executable.clone(),
        policy: Arc::clone(&policy),
        deadline: Instant::now() + timeout,
        timeout,
        max_output,
        output,
        disclosure,
    };
    // This owner acquires and drops the handle off the reactor. If the async
    // waiter is cancelled, its signal stops this collector and the unchanged
    // core Drop joins cleanup here. Abort acknowledgement does not mean that
    // cleanup has finished; normal completion awaits the quiescent terminal.
    let envelope = tokio::task::spawn_blocking(move || {
        let _execution_lease = execution_lease;
        check_cancelled(&cancelled)?;
        let handle = LocalExecutionBackend::with_sandbox_policy(policy)
            .start_named_root(&collector.cx, request)?;
        collector.collect(handle, &cancelled)
    })
    .await
    .map_err(|error| {
        ToolError::Exec(std::io::Error::other(format!(
            "named-root owner join: {error}"
        )))
    })??;
    if !envelope.timed_out.unwrap_or(false)
        && envelope
            .execution
            .as_ref()
            .is_some_and(|event| matches!(event.kind, ExecutionEventKind::Exited(_)))
    {
        guard.finish();
    }
    Ok(envelope.into_json())
}

/// The async waiter's drop never joins the process supervisor.
pub(crate) struct CancelOnDrop(pub(crate) Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> ToolResult<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(ToolError::denied("named-root invocation cancelled"))
    } else {
        Ok(())
    }
}

// Invocation-local configuration only; no new wire record or authority token.
struct NamedRootCollector {
    cx: ToolContext,
    root: String,
    policy: Arc<SandboxPolicy>,
    deadline: Instant,
    timeout: Duration,
    max_output: usize,
    output: OutputEmitter,
    disclosure: Disclosure,
}

impl NamedRootCollector {
    fn collect(
        self,
        mut handle: ExecutionHandle,
        cancelled: &AtomicBool,
    ) -> ToolResult<ToolEnvelope> {
        let Self {
            cx,
            root,
            policy,
            deadline,
            timeout,
            max_output,
            output,
            disclosure,
        } = self;
        let mut protected_roots = BTreeSet::new();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut stdout_truncated = false;
        let mut stderr_truncated = false;
        let mut timed_out = false;
        let mut started = None;
        let mut denials = Vec::new();

        let terminal = loop {
            check_cancelled(cancelled)?;
            if !timed_out && Instant::now() >= deadline {
                timed_out = true;
                output.cancel();
                handle.kill()?;
            }
            let Some(event) = handle.try_next_event() else {
                std::thread::park_timeout(Duration::from_millis(10));
                continue;
            };
            match &event.kind {
                ExecutionEventKind::Started { fence, .. } => {
                    protected_roots = policy.resolve_named_root_protected_roots()?;
                    fence.verify_named_root(&root, cx.caveats(), &protected_roots)?;
                    started = Some(event);
                }
                ExecutionEventKind::Stdout(bytes) => {
                    output.emit(ShellOutputStream::Stdout, bytes);
                    stdout_truncated |= append_capped(&mut stdout, bytes, max_output);
                }
                ExecutionEventKind::Stderr(bytes) => {
                    output.emit(ShellOutputStream::Stderr, bytes);
                    stderr_truncated |= append_capped(&mut stderr, bytes, max_output);
                }
                ExecutionEventKind::Denial(denial) => denials.push(denial.clone()),
                ExecutionEventKind::OutputTruncated(dropped) => {
                    // Failed has no ExitEvidence; the reserved notice is still
                    // authoritative about output omitted before that failure.
                    stdout_truncated |= dropped.stdout_bytes != 0;
                    stderr_truncated |= dropped.stderr_bytes != 0;
                }
                ExecutionEventKind::Exited(_)
                | ExecutionEventKind::Denied { .. }
                | ExecutionEventKind::Failed { .. } => break event,
                ExecutionEventKind::Accepted => {}
            }
        };

        let sandbox_kind = match &started {
            Some(event) => match &event.kind {
                ExecutionEventKind::Started { fence, .. } => fence.sandbox_kind,
                _ => unreachable!("only a real Started event is retained"),
            },
            None => SandboxKind::None,
        };
        let mut envelope = ToolEnvelope::new(sandbox_kind)
            .with_disclosure(disclosure)
            .with_timed_out(timed_out);
        match &terminal.kind {
            ExecutionEventKind::Exited(exit) => {
                envelope.exit_code = exit.code.or_else(|| exit.signal.map(|signal| 128 + signal));
                stdout_truncated |= exit.dropped.stdout_bytes != 0;
                stderr_truncated |= exit.dropped.stderr_bytes != 0;
            }
            ExecutionEventKind::Denied { denial } => {
                denials.push(denial.clone());
                envelope.stderr = Some(denial.reason.clone());
            }
            ExecutionEventKind::Failed { message } => {
                // A real Started must survive a failed proxy/reap finalizer. This
                // is a mechanism failure, not an interceptor denial or fake exit.
                envelope.stderr = Some(format!(
                    "{}execution failed: {message}",
                    String::from_utf8_lossy(&stderr),
                ));
            }
            _ => unreachable!("the loop exits only for a real terminal event"),
        }
        envelope.stdout = Some(String::from_utf8_lossy(&stdout).into_owned());
        if envelope.stderr.is_none() {
            envelope.stderr = Some(String::from_utf8_lossy(&stderr).into_owned());
        }
        if timed_out {
            envelope.exit_code = Some(124);
            envelope.stderr = Some(format!("command timed out after {}s", timeout.as_secs()));
        }
        envelope.stdout_truncated = stdout_truncated;
        envelope.stderr_truncated = stderr_truncated;
        envelope = envelope.with_denials(denials);
        envelope.execution_started = started;
        envelope.execution = Some(terminal);
        if let Some(event) = &envelope.execution_started {
            let ExecutionEventKind::Started { fence, .. } = &event.kind else {
                unreachable!("only a real Started event is retained");
            };
            let body = fence.verify_named_root(&root, cx.caveats(), &protected_roots)?;
            envelope.enforcement = enforcement_report(cx.caveats(), body.mechanism);
        }
        envelope.verify_named_root_execution(&root, cx.caveats(), &protected_roots)?;
        Ok(envelope)
    }
}

fn append_capped(capture: &mut Vec<u8>, bytes: &[u8], cap: usize) -> bool {
    let available = cap.saturating_sub(capture.len());
    capture.extend_from_slice(&bytes[..bytes.len().min(available)]);
    bytes.len() > available
}

#[cfg(test)]
#[path = "named_host_root_execution_tests.rs"]
mod tests;

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
