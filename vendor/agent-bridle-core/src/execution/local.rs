//! The real `LocalExecutionBackend` and the managed owner behind it (#370).
//!
//! # What the owner owns
//!
//! One started execution owns, for exactly as long as it lives: the child and
//! its process tree, the stdout/stderr drainer threads, the stdin writer, the
//! egress proxy that fences the child's network, the fence identity the spawn
//! was admitted under, and the cached terminal result. Nothing is detached —
//! dropping the handle terminates and *joins* the tree rather than letting it
//! outlive its observer.
//!
//! # Why the terminal is honest
//!
//! `Exited` is published only after **all** of the following are true:
//!
//! 1. the direct child has been reaped;
//! 2. no descendant still holds an inherited stdout/stderr writer — if one
//!    does, the tree is force-terminated and the fact is recorded as
//!    [`ExitDisposition::DescendantsReaped`] rather than papered over;
//! 3. both drainer threads have observed EOF and been joined;
//! 4. any egress proxy has been driven to quiescence through
//!    [`crate::ProxyHandle::shutdown_and_join`] (#374).
//!
//! A proxy that cannot be finalized produces `Failed`, never an `Exited` whose
//! egress evidence would be provisional while claiming to be final.
//!
//! # Why none of this runs on a reactor
//!
//! Every blocking step above — `wait(2)`, pipe reads, thread joins, and the
//! proxy's joining finalizer — happens on dedicated `std::thread` workers. An
//! async host may start an execution from inside a Tokio runtime without any of
//! this occupying a reactor worker.

use std::io::{ErrorKind, Read, Write};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use super::contract::{
    ExecutionId, ExecutionLimits, ExecutionRequest, ExecutionStdin, ExecutionTerminal,
    ExitDisposition, ExitEvidence, FenceEvidence,
};
use super::stream::{
    execution_stream, ExecutionControl, ExecutionEventSink, ExecutionHandle, OutputStream,
};
use super::tree::{containment, signal_tree, TreeContainment, TreeSignal};
use crate::{
    ConfinedCommand, Denial, DenialKind, ManagedSpawn, SandboxPolicy, ToolContext, ToolError,
    ToolResult,
};

/// Executes on this host, through the existing audited Bridle spawn funnel.
///
/// This is the only execution backend implemented. The event/handle contract is
/// backend-neutral so a remote provider can implement it later, but a remote
/// fence needs the sandbox-grain identity/provenance binding from RFC 5b before
/// it may exist at all — so there is deliberately no remote variant to select,
/// and `ConfinedCommand` carries no execution-location axis.
#[derive(Clone, Default)]
pub struct LocalExecutionBackend {
    sandbox_policy: Option<Arc<SandboxPolicy>>,
}

impl LocalExecutionBackend {
    /// A backend using the built-in sandbox mechanism policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A backend using an explicit sandbox mechanism policy.
    ///
    /// This is mechanism configuration (read/exec allow-lists), not authority —
    /// authority still comes from the [`ToolContext`] presented at start.
    #[must_use]
    pub fn with_sandbox_policy(policy: Arc<SandboxPolicy>) -> Self {
        Self {
            sandbox_policy: Some(policy),
        }
    }

    /// Start one execution and return its live event stream and lifecycle
    /// handle.
    ///
    /// Authority comes from `cx` and only from `cx`: the request carries none.
    /// The spawn goes through `ConfinedCommand` — the same exec admission,
    /// `AdmittedFence::admit`, sandbox selection/application, and
    /// `verify_applied` every other confined spawn uses. There is no second
    /// mint path and no way to reach a child around it.
    ///
    /// A refusal is reported *on the stream* as a `Denied` terminal after
    /// `Accepted`, with no `Started` event and no process ever created.
    pub fn start(
        &self,
        cx: &ToolContext,
        request: ExecutionRequest,
    ) -> ToolResult<ExecutionHandle> {
        self.start_with_boundary(cx, request, crate::ExecBoundary::ProcessTree)
    }

    /// Admit one exact absolute root and permit descendant executable identities
    /// only under its unchanged inherited filesystem/network fence.
    pub fn start_named_root(
        &self,
        cx: &ToolContext,
        request: ExecutionRequest,
    ) -> ToolResult<ExecutionHandle> {
        self.start_with_boundary(cx, request, crate::ExecBoundary::NamedRoot)
    }

    fn start_with_boundary(
        &self,
        cx: &ToolContext,
        request: ExecutionRequest,
        boundary: crate::ExecBoundary,
    ) -> ToolResult<ExecutionHandle> {
        request.validate()?;
        let id = ExecutionId::next();
        let limits = request.limits;
        let control = Arc::new(LocalControl::new(limits));
        let (sink, handle) = execution_stream(id, limits, Arc::clone(&control) as Arc<_>);

        // `Accepted` precedes every spawn attempt, so an observer can always
        // tell "refused before we tried" from "never reached the backend".
        sink.accepted()?;

        match self.spawn(cx, &request, boundary) {
            Ok(spawned) => {
                self.attach(spawned, request, sink, &control)?;
                Ok(handle)
            }
            Err(ToolError::Denied { reason }) => {
                let denial = Denial {
                    kind: DenialKind::Exec,
                    target: request.executable.clone(),
                    reason,
                };
                control.publish(&sink, ExecutionTerminal::Denied { denial })?;
                Ok(handle)
            }
            Err(other) => {
                control.publish(
                    &sink,
                    ExecutionTerminal::Failed {
                        message: other.to_string(),
                    },
                )?;
                Ok(handle)
            }
        }
    }

    fn spawn(
        &self,
        cx: &ToolContext,
        request: &ExecutionRequest,
        boundary: crate::ExecBoundary,
    ) -> ToolResult<ManagedSpawn> {
        let mut cmd = ConfinedCommand::new(request.executable.clone())
            .args(request.argv.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Bounded tree ownership: the child leads its own process group, so
            // a tree-wide signal can reach its descendants and *only* its
            // descendants.
            .new_process_group();
        if let Some(policy) = &self.sandbox_policy {
            cmd = cmd.sandbox_policy(Arc::clone(policy));
        }
        for (k, v) in &request.env {
            cmd = cmd.env(k, v);
        }
        if let Some(dir) = &request.cwd {
            cmd = cmd.current_dir(dir);
        }
        match boundary {
            crate::ExecBoundary::ProcessTree => cmd.spawn_managed(cx),
            crate::ExecBoundary::NamedRoot => cmd.spawn_named_root_managed(cx),
        }
    }

    /// Wire a freshly spawned tree to its drainers, stdin writer, and reaper.
    fn attach(
        &self,
        spawned: ManagedSpawn,
        request: ExecutionRequest,
        sink: ExecutionEventSink,
        control: &Arc<LocalControl>,
    ) -> ToolResult<()> {
        let ManagedSpawn {
            mut child,
            sandbox_kind,
            fence_id,
            admitted,
            proxy,
        } = spawned;
        let pid = child.id();
        let fence = FenceEvidence {
            fence_id,
            sandbox_kind,
            egress_proxied: proxy.is_some(),
            admitted: admitted.map(Box::new),
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

        control.arm(pid);
        // `Started` means a real process tree exists — it is emitted here,
        // after the spawn returned a live child, never before.
        //
        // A live child now exists, so from here until the reaper thread owns it
        // every early return must terminate and reap it. `Child::drop` does NOT
        // kill, so a bare `?` would leave a detached process behind an error the
        // caller sees as "nothing started".
        if let Err(error) = sink.started(pid, fence.clone()) {
            abandon_unattached(&mut child, pid, proxy);
            return Err(error);
        }

        let limits = request.limits;
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let mut drainers = Vec::new();
        if let Some(out) = stdout {
            drainers.push(spawn_drainer(
                out,
                OutputStream::Stdout,
                sink.clone(),
                limits.max_output_chunk_bytes(),
                done_tx.clone(),
            ));
        }
        if let Some(err) = stderr {
            drainers.push(spawn_drainer(
                err,
                OutputStream::Stderr,
                sink.clone(),
                limits.max_output_chunk_bytes(),
                done_tx.clone(),
            ));
        }
        drop(done_tx);
        let expected_drainers = drainers.len();

        let stdin_thread = stdin.map(|mut pipe| {
            let bytes = match request.stdin {
                ExecutionStdin::Null => Vec::new(),
                ExecutionStdin::Bytes(bytes) => bytes,
            };
            std::thread::spawn(move || {
                // A child that exits without reading gives us EPIPE; that is a
                // normal race, not a failure of the execution.
                let _ = pipe.write_all(&bytes);
                let _ = pipe.flush();
                drop(pipe);
            })
        });

        let reaper_control = Arc::clone(control);
        let reaper = std::thread::spawn(move || {
            reap(
                &mut child,
                Reaping {
                    sink,
                    control: reaper_control,
                    proxy,
                    fence,
                    limits,
                    drainers,
                    expected_drainers,
                    done_rx,
                    stdin_thread,
                },
            );
        });
        control.set_reaper(reaper);
        Ok(())
    }
}

/// Everything the reaper thread needs to reach a quiescent terminal.
struct Reaping {
    sink: ExecutionEventSink,
    control: Arc<LocalControl>,
    proxy: Option<crate::net_proxy::ProxyHandle>,
    fence: FenceEvidence,
    limits: ExecutionLimits,
    drainers: Vec<JoinHandle<()>>,
    expected_drainers: usize,
    done_rx: mpsc::Receiver<()>,
    stdin_thread: Option<JoinHandle<()>>,
}

fn spawn_drainer<R: Read + Send + 'static>(
    mut reader: R,
    stream: OutputStream,
    sink: ExecutionEventSink,
    chunk: usize,
    done: mpsc::Sender<()>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = vec![0_u8; chunk];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                // The sink never blocks: if the bounded queue is full the chunk
                // is counted as dropped and we keep reading, so a stalled
                // consumer can never fill the child's pipe buffer and wedge it.
                //
                // A sink ERROR is likewise not a reason to stop reading. The
                // drainer's other job is to keep the pipe empty; abandoning it
                // would let the child block on a full pipe buffer and never
                // exit, converting a reporting fault into a hang.
                Ok(n) => {
                    let _ = sink.output(stream, &buf[..n]);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = done.send(());
    })
}

/// Guarantees a terminal exists once the reaper is gone.
///
/// `wait()` blocks until a terminal is published, so a reaper that dies without
/// publishing one — a panic in any step, a poisoned lock turned fatal — would
/// leave every waiter blocked forever. This guard runs on unwind as well as on
/// the normal path and publishes a `Failed` terminal if nothing else did.
/// `publish` keeps the FIRST terminal, so this can never overwrite a real one.
pub(super) struct TerminalGuard {
    sink: ExecutionEventSink,
    control: Arc<LocalControl>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.control.publish(
            &self.sink,
            ExecutionTerminal::Failed {
                message: "the execution owner terminated without publishing a result".to_string(),
            },
        );
    }
}

/// Terminate and reap a child that was spawned but never handed to a reaper.
fn abandon_unattached(child: &mut Child, pid: u32, proxy: Option<crate::net_proxy::ProxyHandle>) {
    signal_tree(pid, TreeSignal::Forced);
    let _ = child.kill();
    let _ = child.wait();
    // `ProxyHandle::drop` force-closes and blocks until its workers are joined
    // (#372), so the proxy is quiesced here rather than detached.
    drop(proxy);
}

fn reap(child: &mut Child, mut ctx: Reaping) {
    let guard = TerminalGuard {
        sink: ctx.sink.clone(),
        control: Arc::clone(&ctx.control),
    };
    let status = child.wait();
    // The leader is reaped: no later signal may target this pid as a group id.
    ctx.control.leader_reaped();

    // Bounded wait for both pipes to reach EOF. A descendant that inherited
    // stdout/stderr keeps them open after the leader exits — the case where a
    // naive implementation either hangs or detaches the drain and calls the
    // result final anyway.
    let deadline = Instant::now() + ctx.limits.reap_grace();
    let mut finished = 0;
    while finished < ctx.expected_drainers {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match ctx.done_rx.recv_timeout(remaining) {
            Ok(()) => finished += 1,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let mut descendants_reaped = false;
    if finished < ctx.expected_drainers {
        // Someone in this execution's group still holds a writer. Terminate the
        // group so the pipes close; the tree belongs to this execution, so this
        // can reach nothing else. The pid is still a valid group id here: the
        // group is non-empty precisely because those descendants are alive.
        descendants_reaped = true;
        ctx.control.force_tree();
        while finished < ctx.expected_drainers {
            match ctx.done_rx.recv() {
                Ok(()) => finished += 1,
                Err(_) => break,
            }
        }
    }

    // Joined, not detached: after this loop no drainer thread survives.
    for drainer in ctx.drainers.drain(..) {
        let _ = drainer.join();
    }
    if let Some(stdin) = ctx.stdin_thread.take() {
        let _ = stdin.join();
    }

    // #374: live proxy counters are provisional. The frozen, joinable evidence
    // comes only from the consuming finalizer, and it runs *here*, on this
    // dedicated thread, before any terminal becomes observable.
    let mut proxy_evidence = None;
    let mut proxy_failure = None;
    if let Some(proxy) = ctx.proxy.take() {
        match proxy.shutdown_and_join() {
            Ok(evidence) => proxy_evidence = Some(evidence),
            Err(e) => proxy_failure = Some(e),
        }
    }

    let terminal = compose_terminal(
        status
            .as_ref()
            .map(|s| (s.code(), exit_signal(s)))
            .map_err(|e| e.to_string()),
        proxy_evidence,
        proxy_failure.map(|e| e.to_string()),
        ctx.control.disposition(descendants_reaped),
        ctx.fence.clone(),
        ctx.sink.dropped(),
    );

    let _ = ctx.control.publish(&ctx.sink, terminal);
    // The real terminal is published; the guard's fallback is now a no-op
    // because `publish` keeps the first record.
    drop(guard);
}

/// Decide the terminal record from a quiescent execution's parts.
///
/// Pure, and deliberately separated from [`reap`] so the load-bearing rule —
/// *a proxy that could not be finalized is a failure, never an apparently
/// successful exit* — is provable on every platform, including hosts where the
/// egress proxy never engages because their backend cannot address-fence
/// loopback.
pub(super) fn compose_terminal(
    status: Result<(Option<i32>, Option<i32>), String>,
    proxy_evidence: Option<crate::net_proxy::ProxyFinalEvidence>,
    proxy_failure: Option<String>,
    disposition: ExitDisposition,
    fence: FenceEvidence,
    dropped: super::contract::DroppedEvidence,
) -> ExecutionTerminal {
    if let Some(failure) = proxy_failure {
        // Egress evidence that could not be made quiescent is a failed
        // execution. Reporting `Exited` here would label a provisional
        // finalization as final.
        return ExecutionTerminal::Failed {
            message: format!("egress proxy finalization failed: {failure}"),
        };
    }
    match status {
        Ok((code, signal)) => ExecutionTerminal::Exited(Box::new(ExitEvidence {
            code,
            signal,
            disposition,
            fence,
            dropped,
            proxy: proxy_evidence,
        })),
        Err(e) => ExecutionTerminal::Failed {
            message: format!("could not reap the execution's process tree: {e}"),
        },
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// State shared between the handle, the drainers, and the reaper.
struct TreeState {
    /// The direct child's pid, and on Unix its process-group id. `None` until a
    /// child actually exists, so a denied execution can never signal anything.
    pid: Option<u32>,
    /// Cleared once the direct child has been reaped.
    leader_alive: bool,
    /// How termination was requested, if it was.
    requested: Option<ExitDisposition>,
    /// A graceful-cancel escalation timer is already armed.
    escalating: bool,
}

pub(super) struct LocalControl {
    limits: ExecutionLimits,
    /// Shared so the bounded cancel-escalation timer can re-check liveness
    /// *under the same lock* that gates `leader_reaped` before it signals.
    tree: Arc<Mutex<TreeState>>,
    terminal: Mutex<Option<ExecutionTerminal>>,
    terminal_ready: Condvar,
    reaper: Mutex<Option<JoinHandle<()>>>,
    abandoned: AtomicBool,
}

impl LocalControl {
    fn new(limits: ExecutionLimits) -> Self {
        Self {
            limits,
            tree: Arc::new(Mutex::new(TreeState {
                pid: None,
                leader_alive: false,
                requested: None,
                escalating: false,
            })),
            terminal: Mutex::new(None),
            terminal_ready: Condvar::new(),
            reaper: Mutex::new(None),
            abandoned: AtomicBool::new(false),
        }
    }

    fn lock_tree(&self) -> std::sync::MutexGuard<'_, TreeState> {
        self.tree
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn arm(&self, pid: u32) {
        let mut tree = self.lock_tree();
        tree.pid = Some(pid);
        tree.leader_alive = true;
    }

    fn set_reaper(&self, handle: JoinHandle<()>) {
        *self
            .reaper
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    /// Mark the direct child reaped, under the same lock a signal takes — so a
    /// concurrent `kill` can never deliver to a pid that has just been recycled.
    fn leader_reaped(&self) {
        self.lock_tree().leader_alive = false;
    }

    /// Force-terminate the tree from inside the reaper, where the group is
    /// known to be non-empty (surviving descendants keep the group id valid).
    fn force_tree(&self) {
        let tree = self.lock_tree();
        if let Some(pid) = tree.pid {
            signal_tree(pid, TreeSignal::Forced);
        }
    }

    fn disposition(&self, descendants_reaped: bool) -> ExitDisposition {
        let tree = self.lock_tree();
        match tree.requested {
            Some(requested) => requested,
            None if descendants_reaped => ExitDisposition::DescendantsReaped,
            None => ExitDisposition::Natural,
        }
    }

    pub(super) fn publish(
        &self,
        sink: &ExecutionEventSink,
        terminal: ExecutionTerminal,
    ) -> ToolResult<()> {
        {
            let mut slot = self
                .terminal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_some() {
                // A terminal is already cached: this is the guard's fallback
                // arriving after the real result. Never overwrite it, and never
                // emit a second terminal event.
                return Ok(());
            }
            *slot = Some(terminal.clone());
            // Wake `wait()` before the event becomes visible, so a consumer that
            // sees the terminal event and then calls `wait()` never races.
            self.terminal_ready.notify_all();
        }
        sink.publish_terminal(terminal)
    }
}

#[cfg(test)]
impl LocalControl {
    /// A control with no process attached, for exercising terminal publication
    /// without spawning anything.
    pub(super) fn for_test() -> Self {
        Self::new(ExecutionLimits::default())
    }
}

#[cfg(test)]
impl TerminalGuard {
    pub(super) fn for_test(sink: ExecutionEventSink, control: Arc<LocalControl>) -> Self {
        Self { sink, control }
    }
}

impl ExecutionControl for LocalControl {
    fn wait(&self) -> ToolResult<ExecutionTerminal> {
        let mut slot = self
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(terminal) = slot.as_ref() {
                // Idempotent by construction: every caller gets a clone of the
                // one cached record, so repeated waits cannot disagree.
                return Ok(terminal.clone());
            }
            slot = self
                .terminal_ready
                .wait(slot)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn cancel(&self) -> ToolResult<()> {
        {
            let mut tree = self.lock_tree();
            if !tree.leader_alive {
                return Ok(());
            }
            tree.requested.get_or_insert(ExitDisposition::Cancelled);
            if tree.escalating {
                // Idempotent: a second cancel re-sends the graceful signal but
                // never arms a second escalation timer.
                if let Some(pid) = tree.pid {
                    signal_tree(pid, TreeSignal::Graceful);
                }
                return Ok(());
            }
            tree.escalating = true;
            if let Some(pid) = tree.pid {
                // Graceful first, while still holding the lock that gates
                // `leader_reaped`, so the signal cannot land after the reap.
                signal_tree(pid, TreeSignal::Graceful);
            }
        }
        // Escalation is bounded and asynchronous: `cancel` returns promptly, and
        // a tree that ignores the graceful request is killed anyway.
        let tree = Arc::clone(&self.tree);
        let grace = self.limits.cancel_grace();
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            let tree = tree
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Re-checked under the lock: if the leader was already reaped, the
            // pid is no longer ours to signal and we must not touch it.
            if !tree.leader_alive {
                return;
            }
            if let Some(pid) = tree.pid {
                signal_tree(pid, TreeSignal::Forced);
            }
        });
        Ok(())
    }

    fn kill(&self) -> ToolResult<()> {
        let mut tree = self.lock_tree();
        if !tree.leader_alive {
            return Ok(());
        }
        tree.requested = Some(ExitDisposition::Killed);
        if let Some(pid) = tree.pid {
            signal_tree(pid, TreeSignal::Forced);
        }
        Ok(())
    }

    fn abandon(&self) {
        if self.abandoned.swap(true, Ordering::AcqRel) {
            return;
        }
        // Fail-closed: terminate the tree, then *join* the reaper so this
        // returns only once the child, its descendants, the drainers, and the
        // proxy are all finished. Nothing is left detached behind a dropped
        // handle.
        let _ = self.kill();
        let reaper = self
            .reaper
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(reaper) = reaper {
            let _ = reaper.join();
        }
    }
}

/// The tree containment this platform proves for managed executions.
///
/// Reported rather than assumed: a caller that needs group-grain containment
/// can check it instead of trusting that every platform behaves like Linux.
#[must_use]
pub fn local_tree_containment() -> LocalTreeContainment {
    match containment() {
        TreeContainment::ProcessGroup => LocalTreeContainment::ProcessGroup,
        TreeContainment::PlatformTreeKill => LocalTreeContainment::PlatformTreeKill,
        TreeContainment::DirectChildOnly => LocalTreeContainment::DirectChildOnly,
    }
}

/// How completely this platform can terminate a managed execution's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalTreeContainment {
    /// Unix: the execution leads its own process group and the group is
    /// signalled as one unit.
    ProcessGroup,
    /// Windows: `taskkill /T` walks the parent/child relation. A real tree
    /// kill, but not job-object containment.
    PlatformTreeKill,
    /// No tree primitive; only the direct child can be signalled.
    DirectChildOnly,
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
