//! The carried **brush engine** (agent-bridle#20 / Track 2): a bash-in-Rust
//! shell run in a dedicated worker behind the L3-aware spawn funnel and the
//! worker-local static command and file-open L2 policies.
//!
//! Unlike [`HostShellTool`](crate::HostShellTool) — which *refuses* a restricted
//! `exec`/`net` grant because it cannot bound a real `/bin/sh`'s forked children
//! — this engine's interceptor fires on every resolved program name and path
//! opened through Brush itself (`authorize_external_cmd` at Brush's external-spawn funnel,
//! `pre_open_file` at `Shell::open_file`). It records each denial into the worker
//! response surfaced as structured `denials` on the envelope. A permitted
//! external child's own syscalls — including carried-uutils file opens and
//! delegated descendants such as `find -exec` — do not re-enter those hooks and
//! rely on the inherited L3 boundary.
//!
//! It uses the `brush-ocap-*` fork based on the upstream static-filter design
//! (reubeno/brush#1314). The worker is born through `SandboxedWorker`; when an
//! effective native backend engages, carried commands and descendants inherit
//! it. The curated builtin set removes `exec`: replacing the authenticated
//! worker would destroy its framed response protocol.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use agent_bridle_core::{
    default_exec_path, enforcement_report, human_gate, is_unbridled, ConfinementMechanism,
    Disclosure, SandboxPolicy, SandboxedWorker, SandboxedWorkerChild, Scope, Tool, ToolContext,
    ToolEnvelope, ToolError, ToolResult,
};
use async_trait::async_trait;
use brush_builtins::{default_builtins, BuiltinSet};
use brush_core::builtins::Registration;
use brush_core::extensions::{DefaultErrorFormatter, ShellExtensionsImpl};
use brush_core::filter::NoOpSourceFilter;
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::variables::ShellVariable;
use brush_core::{Shell, ShellFd};

use crate::brush_protocol::{read_stream, stream_limits, WorkerOutcome};
use crate::brush_worker::WorkerPayload;
use crate::caveat_interceptor::CaveatInterceptor;
use crate::output_observer::{drain_capped, output_session, OutputEmitter};

/// The engine identity stamped on the disclosure (ADR 0005 D2 / ADR 0019 D4).
const ENGINE_NAME: &str = "brush";
/// Default cap on captured output bytes (mirrors the other engines' default).
const DEFAULT_MAX_OUTPUT: usize = 64 * 1024;
/// Minimal, standard `PATH` used when `exec` is RESTRICTED: external commands
/// must still *resolve* so they reach the interceptor's `authorize_external_cmd` gate
/// (which then denies the out-of-scope ones). Under full-access the full ambient
/// path is used instead (see [`BrushShellTool::invoke`]).
const RESTRICTED_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// `exec` replaces the authenticated worker process and prevents it from
/// completing the private framed response protocol. The fork authorizes exec,
/// but this embedding cannot let its worker be replaced.
const REMOVED_BUILTINS: &[&str] = &["exec"];

/// Default wall-clock ceiling for a confined run (FIX 3). Sourced from the shared
/// shell-limits contract — [`LimitsPolicy::default_timeout_secs`](agent_bridle_core::LimitsPolicy)
/// (60s) — so the brush path bounds itself exactly like the safe-subset and host
/// engines instead of running unbounded.
fn default_timeout() -> Duration {
    Duration::from_secs(agent_bridle_core::LimitsPolicy::default().default_timeout_secs)
}

/// The brush [`ShellExtensions`](brush_core::extensions::ShellExtensions) carried
/// by this engine: the default error formatter plus the capability interceptor.
type LeashedExtensions = ShellExtensionsImpl<
    DefaultErrorFormatter,
    CaveatInterceptor,
    NoOpSourceFilter,
    CaveatInterceptor,
>;

/// The engine's input schema (shared `cmd`/`env`/`cwd` contract with the other
/// engines), parsed once from the embedded data file — knowledge in data, not an
/// inline literal (three-Cs).
static DEFAULT_SCHEMA: LazyLock<Arc<serde_json::Value>> = LazyLock::new(|| {
    Arc::new(
        serde_json::from_str(include_str!("brush_shell.schema.json"))
            .expect("embedded brush_shell.schema.json must be valid JSON"),
    )
});

/// The carried brush engine — a [`Tool`] that runs a free-form command string
/// through a worker-hosted bash-in-Rust shell with a worker-local
/// static-filter leash and any L3 boundary engaged by the effective
/// caveats. Registered under `"shell"` (the ADR 0005 D2 seam), a peer of
/// [`ShellTool`](crate::ShellTool) / [`HostShellTool`](crate::HostShellTool).
#[derive(Clone)]
pub struct BrushShellTool {
    max_output: usize,
    schema: Arc<serde_json::Value>,
    output_observer: Option<Arc<dyn crate::ShellOutputObserver>>,
    execution_lease: Option<crate::ExecutionLease>,
    command_broker: Option<Arc<dyn crate::CommandBroker>>,
    /// Wall-clock ceiling for one run (FIX 3). A run that exceeds it is stopped
    /// and reported `timed_out:true` with exit 124.
    timeout: Duration,
    sandbox_policy: Arc<SandboxPolicy>,
    named_host_roots: bool,
}

impl std::fmt::Debug for BrushShellTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrushShellTool").finish_non_exhaustive()
    }
}

impl Default for BrushShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BrushShellTool {
    /// The engine with default output cap and the embedded schema.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_output: DEFAULT_MAX_OUTPUT,
            schema: DEFAULT_SCHEMA.clone(),
            output_observer: None,
            execution_lease: None,
            command_broker: None,
            timeout: default_timeout(),
            sandbox_policy: Arc::new(SandboxPolicy::default()),
            named_host_roots: false,
        }
    }

    /// Set the maximum captured bytes per output stream. The shared worker
    /// protocol rejects zero and values beyond its finite capture ceiling.
    /// Output beyond this limit is reported by the envelope's truncation flags.
    pub fn with_max_output_bytes(mut self, max_output: usize) -> ToolResult<Self> {
        stream_limits(max_output)?;
        self.max_output = max_output;
        Ok(self)
    }

    /// Override the wall-clock ceiling (three-Cs: Configuration). A run that
    /// exceeds `timeout` has its worker process group terminated on Unix (the
    /// worker process on other targets) and is reported `timed_out:true` with
    /// exit 124.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Retain caller-owned execution resources through worker shutdown.
    ///
    /// Dropping an invocation signals cancellation, but process termination
    /// and reaping happen in its blocking supervisor. A clone of this lease
    /// stays with that owner until cleanup finishes, including on cancellation
    /// and errors. The lease conveys no filesystem or execution authority.
    #[must_use]
    pub fn with_execution_lease(mut self, lease: crate::ExecutionLease) -> Self {
        self.execution_lease = Some(lease);
        self
    }

    /// Prepare selected native commands through a private, scoped host broker.
    ///
    /// The command still executes under the original worker fence and stdio.
    /// The broker cannot replace the invocation's authority. Unsupported
    /// platforms refuse this optional facility before executing shell code.
    #[must_use]
    pub fn with_command_broker(mut self, broker: Arc<dyn crate::CommandBroker>) -> Self {
        self.command_broker = Some(broker);
        self
    }

    /// Select the OS-sandbox mechanism policy for the private worker.
    #[must_use]
    pub fn with_sandbox_policy(mut self, policy: Arc<SandboxPolicy>) -> Self {
        self.sandbox_policy = policy;
        self
    }

    /// Enable exact, absolute, literal host roots in the trusted parent.
    ///
    /// A candidate still needs an exact effective exec grant and an admitted
    /// inherited filesystem/network fence. Its descendants may choose their
    /// executable identities within that fence; root exec admission reports
    /// Interceptor strength. Ordinary and dynamic Brush commands keep their
    /// existing execution mechanism. The sandbox policy must also supply its
    /// explicit [`SandboxPolicy::named_root_protected_roots`] inventory; omission
    /// refuses the new operation. This option defaults off.
    #[must_use]
    pub fn with_named_host_roots(mut self) -> Self {
        self.named_host_roots = true;
        self
    }

    /// Attach a presentation-only observer for bounded stdout/stderr chunks.
    ///
    /// The observer receives only output captured by an admitted invocation and
    /// cannot change the interceptor, authority, or final result envelope.
    /// Delivery may finish asynchronously after the invocation returns;
    /// `on_finish` marks the queue-drained boundary.
    #[must_use]
    pub fn with_output_observer(mut self, observer: Arc<dyn crate::ShellOutputObserver>) -> Self {
        self.output_observer = Some(observer);
        self
    }

    /// Override the tool's input schema (three-Cs: Configuration).
    #[must_use]
    pub fn with_schema(mut self, schema: serde_json::Value) -> Self {
        self.schema = Arc::new(schema);
        self
    }

    fn disclosure(&self) -> Disclosure {
        Disclosure {
            unbridled: is_unbridled(),
            engine: Some(ENGINE_NAME.to_string()),
            human_gate: human_gate(),
            ..Disclosure::default()
        }
    }
}

#[async_trait]
impl Tool for BrushShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn schema(&self) -> serde_json::Value {
        (*self.schema).clone()
    }

    async fn invoke(
        &self,
        args: serde_json::Value,
        cx: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        let cmd = args
            .get("cmd")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::denied("brush: missing required `cmd` string"))?
            .to_string();
        let cwd = args
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir().map_err(ToolError::from)?)
            .canonicalize()
            .map_err(|error| ToolError::denied(format!("brush cwd is invalid: {error}")))?;
        cx.check_path_read(&cwd)?;

        // The schema's `env` seam: the DELIBERATE import surface across the
        // confinement boundary (this engine runs `do_not_inherit_env(true)`, so
        // nothing ambient leaks in). String values only, mirroring the host and
        // safe-subset engines. Before this, brush silently DROPPED `env` even
        // though the schema advertised it — losing HOME/USER/VIRTUAL_ENV and
        // re-opening the #783-class `~`-expansion bug under this engine.
        let env: BTreeMap<String, String> = args
            .get("env")
            .and_then(serde_json::Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // PATH parity: the FULL ambient path when `exec` is unrestricted (so the
        // agent's own tools — `~/.cargo/bin`, `/opt/homebrew/bin`, … — resolve
        // like a host shell); a minimal standard path when `exec` is RESTRICTED,
        // so externals still resolve to reach the `authorize_external_cmd` gate that denies
        // the out-of-scope ones. The host env is never inherited otherwise.
        let path_value = if matches!(cx.caveats().exec, Scope::All) {
            default_exec_path()
        } else {
            RESTRICTED_PATH.to_string()
        };

        if let Some((root, argv)) = crate::named_host_root::select_named_host_root(
            self.named_host_roots && self.command_broker.is_none(),
            &cmd,
            cx.caveats(),
        )
        .map_err(|error| ToolError::denied(format!("brush: {error}")))?
        {
            let mut request = agent_bridle_core::ExecutionRequest::new(root)
                .args(argv)
                .cwd(cwd);
            let mut env = env;
            env.entry("PATH".to_string()).or_insert(path_value);
            request.env = env.into_iter().collect();
            return crate::named_host_root_execution::invoke_named_host_root(
                cx,
                request,
                Arc::clone(&self.sandbox_policy),
                self.timeout,
                self.max_output,
                self.output_observer.clone(),
                self.disclosure(),
                self.execution_lease.clone(),
            )
            .await;
        }

        let max_output = self.max_output;
        let (output_guard, output) = output_session(self.output_observer.clone(), max_output);
        let mut nonce_bytes = [0_u8; 32];
        getrandom::getrandom(&mut nonce_bytes)
            .map_err(|error| ToolError::denied(format!("cannot create worker nonce: {error}")))?;
        let nonce = nonce_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if self.command_broker.is_some() {
            return Err(ToolError::denied(
                "authenticated native command broker is unavailable on this platform",
            ));
        }
        let resources = match &self.command_broker {
            Some(broker) => broker.read_only_resources(cx)?,
            None => Vec::new(),
        };
        let mut payload = WorkerPayload::new(
            cmd,
            Some(cwd.to_string_lossy().into_owned()),
            path_value,
            env,
            max_output,
        );
        payload.broker = self.command_broker.is_some();
        let confined = SandboxedWorker::brush()
            .sandbox_policy(Arc::clone(&self.sandbox_policy))
            .read_only_resources(resources)
            .spawn(cx, &nonce, &cwd)?;
        let sandbox_kind = confined.sandbox_kind;
        let caveats = cx.caveats().clone();
        // The mechanism governing the worker: reported kind + the child-network
        // policy the worker was spawned under (same `self.sandbox_policy`). Keeps
        // the reported net witness honest (Landlock `net:none` is Kernel only under
        // `DenyDirect`), never over-claiming from the kind alone.
        let mechanism = ConfinementMechanism::new(sandbox_kind, self.sandbox_policy.child_network);
        let timeout = self.timeout;
        let worker_output = output.clone();
        let cancellation =
            crate::named_host_root_execution::CancelOnDrop(Arc::new(AtomicBool::new(false)));
        let cancelled = Arc::clone(&cancellation.0);
        let execution_lease = self.execution_lease.clone();
        let command_broker = self.command_broker.clone();
        let context = cx.clone();
        let supervised = tokio::task::spawn_blocking(move || {
            // Keep resources in the process owner's lifetime, not just the
            // cancellable async waiter's stack.
            let _execution_lease = execution_lease;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let (broker_runtime, endpoint) = match command_broker {
                Some(broker) => {
                    // OwnedWorker below must reap before this runtime drops.
                    // If broker setup fails, reap the newly spawned worker too.
                    match crate::broker_runtime::BrokerRuntime::start(
                        broker,
                        context,
                        confined.child.id(),
                        cancelled.clone(),
                        std::time::Instant::now() + timeout,
                    ) {
                        Ok((runtime, endpoint)) => (Some(runtime), Some(endpoint)),
                        Err(error) => {
                            drop(OwnedWorker {
                                confined,
                                reaped: false,
                            });
                            return Err(error);
                        }
                    }
                }
                None => (None, None),
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let endpoint = {
                let _ = (command_broker, context);
                None
            };
            let result = supervise_worker(
                confined,
                &payload,
                timeout,
                max_output,
                worker_output,
                &cancelled,
                endpoint,
            );
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            drop(broker_runtime);
            result
        })
        .await
        .map_err(|error| ToolError::Exec(std::io::Error::other(format!("join: {error}"))))??;
        match supervised {
            Supervised::Complete(outcome) => {
                let WorkerOutcome {
                    response,
                    stdout,
                    stderr,
                    dropped,
                } = outcome;
                if let Some(error) = response.error {
                    return Err(ToolError::denied(format!(
                        "brush worker refused request: {error}"
                    )));
                }
                // The transcript is the accumulation of the frames an observer
                // already saw live. The terminal contributes status, denials,
                // and drop accounting — never a second copy of the output that
                // could disagree with what was displayed.
                let envelope = ToolEnvelope::new(sandbox_kind)
                    .with_enforcement(enforcement_report(&caveats, mechanism))
                    .with_disclosure(self.disclosure())
                    .with_exit_code(response.exit_code)
                    .with_stdout(String::from_utf8_lossy(&stdout).into_owned())
                    .with_stderr(String::from_utf8_lossy(&stderr).into_owned())
                    .with_truncation(
                        response.dropped.stdout_bytes > 0 || dropped.stdout_bytes > 0,
                        response.dropped.stderr_bytes > 0 || dropped.stderr_bytes > 0,
                    )
                    .with_timed_out(false)
                    .with_denials(response.denials)
                    .into_json();
                output_guard.finish();
                Ok(envelope)
            }
            Supervised::TimedOut => {
                drop(output_guard);
                Ok(ToolEnvelope::new(sandbox_kind)
                    .with_enforcement(enforcement_report(&caveats, mechanism))
                    .with_disclosure(self.disclosure())
                    .with_exit_code(124)
                    .with_stderr(format!("command timed out after {}s", timeout.as_secs()))
                    .with_timed_out(true)
                    .into_json())
            }
        }
    }
}

enum Supervised {
    Complete(WorkerOutcome),
    TimedOut,
}

fn supervise_worker(
    confined: SandboxedWorkerChild,
    payload: &WorkerPayload,
    timeout: Duration,
    max_output: usize,
    output: OutputEmitter,
    cancelled: &AtomicBool,
    endpoint: Option<crate::brush_worker::WorkerEndpoint>,
) -> ToolResult<Supervised> {
    let mut worker = OwnedWorker {
        confined,
        reaped: false,
    };
    if cancelled.load(Ordering::Acquire) {
        return Err(ToolError::denied("brush invocation cancelled"));
    }
    let auth_started = std::time::Instant::now();
    if let Err(error) =
        authenticate_worker(&mut worker.confined, payload, timeout, cancelled, endpoint)
    {
        worker.terminate()?;
        let stdout = worker
            .confined
            .child
            .stdout
            .take()
            .ok_or_else(|| ToolError::denied("brush worker stdout was not piped"))?;
        let stderr = worker
            .confined
            .child
            .stderr
            .take()
            .ok_or_else(|| ToolError::denied("brush worker stderr was not piped"))?;
        let protocol_cap = max_output.saturating_mul(4).saturating_add(1024 * 1024);
        let stdout_reader = std::thread::spawn(move || read_capped(stdout, protocol_cap));
        let stderr_reader =
            std::thread::spawn(move || read_capped(stderr, max_output.saturating_add(4096)));

        let stdout = stdout_reader
            .join()
            .map_err(|_| ToolError::denied("brush worker stdout reader panicked"))??;
        let stderr = stderr_reader
            .join()
            .map_err(|_| ToolError::denied("brush worker stderr reader panicked"))??;
        let reason = brush_worker_handshake_error_message(&error.to_string(), &stdout, &stderr);
        return Err(ToolError::denied(format!(
            "brush worker authentication handshake failed: {reason}"
        )));
    }

    let stdout = worker
        .confined
        .child
        .stdout
        .take()
        .ok_or_else(|| ToolError::denied("brush worker stdout was not piped"))?;
    let stderr = worker
        .confined
        .child
        .stderr
        .take()
        .ok_or_else(|| ToolError::denied("brush worker stderr was not piped"))?;

    let limits = stream_limits(max_output)?;
    let stdout_reader = std::thread::spawn(move || {
        read_stream(stdout, limits, |stream, chunk| {
            output.emit(stream, chunk);
        })
    });
    let stderr_reader =
        std::thread::spawn(move || read_capped(stderr, max_output.saturating_add(4096)));

    let deadline = auth_started + timeout;
    let status = loop {
        if cancelled.load(Ordering::Acquire) {
            break None;
        }
        if let Some(status) = worker.try_reap()? {
            break Some(status);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            break None;
        }
        std::thread::park_timeout(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(10)),
        );
    };
    if status.is_none() {
        worker.terminate()?;
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        if cancelled.load(Ordering::Acquire) {
            return Err(ToolError::denied("brush invocation cancelled"));
        }
        return Ok(Supervised::TimedOut);
    }
    let outcome = stdout_reader
        .join()
        .map_err(|_| ToolError::denied("brush worker stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| ToolError::denied("brush worker stderr reader panicked"))??;
    let response = &outcome.response;
    if response.error.is_some() && !stderr.is_empty() {
        // Keep the authenticated terminal error authoritative. Worker stderr is
        // diagnostic-only and must never become a second result channel.
        eprintln!(
            "brush worker diagnostic stderr: {}",
            String::from_utf8_lossy(&stderr)
        );
    }
    Ok(Supervised::Complete(outcome))
}

/// Every exit path owns cleanup. In particular, a protocol/reader error must
/// not drop an unreaped child while the invocation's resource lease is freed.
struct OwnedWorker {
    confined: SandboxedWorkerChild,
    reaped: bool,
}

impl OwnedWorker {
    fn terminate(&mut self) -> ToolResult<()> {
        crate::kill_child_tree(&mut self.confined.child);
        self.confined.child.wait().map_err(ToolError::from)?;
        self.reaped = true;
        Ok(())
    }

    fn try_reap(&mut self) -> ToolResult<Option<std::process::ExitStatus>> {
        let status = reap_worker_tree_if_exited(&mut self.confined.child)?;
        self.reaped = status.is_some();
        Ok(status)
    }
}

impl Drop for OwnedWorker {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.terminate();
        }
    }
}

fn authenticate_worker(
    confined: &mut SandboxedWorkerChild,
    payload: &WorkerPayload,
    timeout: Duration,
    cancelled: &AtomicBool,
    endpoint: Option<crate::brush_worker::WorkerEndpoint>,
) -> ToolResult<()> {
    #[cfg(unix)]
    {
        let pid = i32::try_from(confined.child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or_else(|| ToolError::denied("invalid brush worker PID"))?;
        watch_worker_authentication(pid, cancelled, || {
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let _ = endpoint;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if let Some(endpoint) = endpoint {
                return confined.send_payload_with_control_channel(payload, endpoint, timeout);
            }
            confined.send_payload(payload, timeout)
        })
    }
    // Private worker authentication is currently supported only on macOS and
    // Linux. Preserve the existing refusal on other platforms.
    #[cfg(not(unix))]
    {
        let _ = (cancelled, endpoint);
        confined.send_payload(payload, timeout)
    }
}

#[cfg(unix)]
fn watch_worker_authentication<T>(
    pid: rustix::process::Pid,
    cancelled: &AtomicBool,
    authenticate: impl FnOnce() -> ToolResult<T>,
) -> ToolResult<T> {
    // The caller owns an UNREAPED child throughout this scope. The watcher is
    // disarmed and joined before any wait/reap, so it can never signal a reused
    // PID. Killing the worker closes its private control endpoint and unblocks
    // authentication without shortening the configured startup timeout.
    std::thread::scope(|scope| {
        let (finished, completion) = std::sync::mpsc::channel::<()>();
        let watcher = std::thread::Builder::new()
            .name("brush-auth-cancellation".into())
            .spawn_scoped(scope, move || loop {
                if cancelled.load(Ordering::Acquire) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::STOP);
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                    break;
                }
                match completion.recv_timeout(Duration::from_millis(10)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            })
            .map_err(ToolError::from)?;
        let result = authenticate();
        // Disconnect also happens during unwinding, so the scoped watcher
        // cannot strand its parent if authentication panics.
        drop(finished);
        watcher
            .join()
            .map_err(|_| ToolError::denied("brush authentication watcher panicked"))?;
        result
    })
}

fn brush_worker_handshake_error_message(
    transport_error: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    if let Ok(outcome) =
        stream_limits(stdout.len().max(1)).and_then(|limits| read_stream(stdout, limits, |_, _| {}))
    {
        let response = outcome.response;
        if let Some(reason) = response.error {
            return reason;
        }
        if !response.denials.is_empty() {
            return serde_json::to_string(&response.denials)
                .unwrap_or_else(|_| String::from("unprintable denials"));
        }
        if response.exit_code != 0 {
            return format!("exit_code {}", response.exit_code);
        }
    }
    let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
    if stderr_text.is_empty() {
        transport_error.to_owned()
    } else {
        format!("{transport_error}: {stderr_text}")
    }
}

fn read_capped(reader: impl Read, cap: usize) -> ToolResult<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(cap.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ToolError::from)?;
    if bytes.len() > cap {
        return Err(ToolError::denied(
            "brush worker protocol output exceeded its cap",
        ));
    }
    Ok(bytes)
}

/// Poll for the worker's exit and, the instant it has exited, reap its **whole
/// process group** before reaping the leader.
///
/// Bounded tree ownership on the NORMAL completion path, not only on timeout: a
/// descendant the command backgrounded would otherwise outlive the worker while
/// still holding dups of its inherited pipe writers, so a terminal could be
/// reported with an output writer — and a process — still alive.
///
/// The ordering is the load-bearing part. `Child::try_wait` *reaps*, which frees
/// the pid; signalling the group afterwards means signalling a process-group id
/// that is no longer ours, and a recycled pid would take the signal. So exit is
/// detected with `waitid(WNOWAIT)`, which leaves the leader in a waitable state:
/// while it is un-reaped its pid cannot be reused, the group id is still ours to
/// signal, and only then is the leader reaped.
#[cfg(unix)]
fn reap_worker_tree_if_exited(child: &mut Child) -> ToolResult<Option<std::process::ExitStatus>> {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};

    let Ok(raw) = i32::try_from(child.id()) else {
        return child.try_wait().map_err(ToolError::from);
    };
    let Some(pid) = Pid::from_raw(raw) else {
        return child.try_wait().map_err(ToolError::from);
    };
    let exited = matches!(
        waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ),
        Ok(Some(_))
    );
    if !exited {
        return Ok(None);
    }
    crate::kill_child_tree(child);
    child.wait().map(Some).map_err(ToolError::from)
}

/// No process groups with POSIX semantics here; fall back to the direct child.
#[cfg(not(unix))]
fn reap_worker_tree_if_exited(child: &mut Child) -> ToolResult<Option<std::process::ExitStatus>> {
    child.try_wait().map_err(ToolError::from)
}

/// What a finished brush run produced.
///
/// Deliberately no stdout/stderr: the run's output already went out over the
/// framed result channel as it was produced, and that stream is the single
/// authoritative transcript. Returning a second copy here would recreate
/// exactly the two-truths problem the protocol removes — and it would be the
/// WORSE copy, because a detached drain (below) leaves it silently short while
/// the live stream already carried the bytes.
#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) exit_code: i32,
    /// A drain thread had to be detached because a surviving writer kept the
    /// pipe open, so output produced after that point reached neither the
    /// stream nor this result. Reported so the terminal can say output was
    /// omitted instead of presenting a short transcript as complete.
    pub(crate) stdout_detached: bool,
    pub(crate) stderr_detached: bool,
}

/// Drive a brush shell to completion for one command, capturing stdout/stderr
/// via real OS pipes (an `Arc<Mutex<Vec<u8>>>` will not satisfy brush's fd
/// `Stream`; pipes are mandatory). The shell is built with the supplied
/// [`CaveatInterceptor`] so Brush-originated exec/open operations are gated.
/// IO must be enabled on the runtime (not just time): `$(...)` sets up real
/// pipes via tokio's IO driver, and with IO enabled the inner program hits the
/// `authorize_external_cmd` funnel (a legible recorded denial) rather than panicking.
pub(crate) fn run_in_brush(
    cmd: String,
    cwd: Option<String>,
    path_value: String,
    env: BTreeMap<String, String>,
    interceptor: CaveatInterceptor,
    max_output: usize,
    output: OutputEmitter,
) -> ToolResult<Captured> {
    let (out_reader, out_writer) =
        std::io::pipe().map_err(|e| ToolError::Exec(brush_io("create stdout pipe", &e)))?;
    let (err_reader, err_writer) =
        std::io::pipe().map_err(|e| ToolError::Exec(brush_io("create stderr pipe", &e)))?;

    // Drain the read ends on background threads so a chatty command cannot
    // deadlock by filling the pipe buffer before the shell exits. Each thread
    // reports its captured output over a channel (FIX 4) rather than via
    // `JoinHandle::join`: a `join` blocks the caller for the ENTIRE lifetime of
    // any background child that inherited a dup of the write pipe (brush hands
    // each child a real `dup(2)`), which would pin a
    // scarce `spawn_blocking` worker — so `collect_drained` bounded-waits then
    // DETACHES instead.
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let stdout_output = output.clone();
    std::thread::spawn(move || {
        let _ = out_tx.send(drain(
            out_reader,
            max_output,
            &stdout_output,
            crate::ShellOutputStream::Stdout,
        ));
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = err_tx.send(drain(
            err_reader,
            max_output,
            &output,
            crate::ShellOutputStream::Stderr,
        ));
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ToolError::Exec(brush_io("build shell runtime", &e)))?;

    let working_dir = cwd.map(std::path::PathBuf::from);

    let exit_code = rt.block_on(async move {
        let mut fds: HashMap<ShellFd, OpenFile> = HashMap::new();
        // FIX 1 (critical #4): seed STDIN_FD with `/dev/null` (an at-EOF reader).
        // Otherwise brush defaults STDIN_FD to the real `std::io::stdin()`
        // (`openfiles.rs` `default_files`), so a confined `cat`/`wc`/`grep`/`sort`
        // with no pipe would read the OPERATOR'S TERMINAL — hanging the turn,
        // stealing keystrokes, and corrupting MCP stdio. `openfiles::null()` is the
        // cross-platform sink (`/dev/null` on unix, `NUL` on Windows), mirroring
        // how the safe-subset engine gives spawned children `Stdio::null()`.
        fds.insert(
            OpenFiles::STDIN_FD,
            brush_core::openfiles::null()
                .map_err(|e| ToolError::Exec(brush_io("open /dev/null stdin", &e)))?,
        );
        fds.insert(OpenFiles::STDOUT_FD, OpenFile::from(out_writer));
        fds.insert(OpenFiles::STDERR_FD, OpenFile::from(err_writer));

        let mut shell: Shell<LeashedExtensions> =
            Shell::builder_with_extensions::<LeashedExtensions>()
                .cmd_exec_filter(interceptor.clone())
                .file_open_filter(interceptor)
                .builtins(confined_builtins())
                .do_not_inherit_env(true)
                .no_editing(true)
                .interactive(false)
                .kill_external_commands_on_drop(true)
                .fds(fds)
                .maybe_working_dir(working_dir)
                .build()
                .await
                .map_err(|e| ToolError::Exec(brush_io("build shell", &e)))?;

        // Register the carried coreutils shims (issue #206). They re-exec
        // `<self> --invoke-bundled <name>`, so they resolve ONLY when
        // the host binary is dispatch-capable (calls `maybe_dispatch()` in main).
        // The re-exec still funnels through the `authorize_external_cmd` interceptor.
        #[cfg(feature = "carried-coreutils")]
        {
            crate::coreutils_dispatch::install_default_providers();
            crate::coreutils_dispatch::register_shims(&mut shell);
        }

        // Every var seeded below MUST be `.export()`ed: `ShellVariable::new`
        // defaults `exported: false` (an ordinary shell variable, visible to
        // brush's own expansion but NOT propagated to a spawned child's OS
        // environment). Without this, a real external command sees NONE of
        // PATH/HOME/USER/the caller's env seam — only brush's own internal
        // variable table has them, which is indistinguishable from working
        // until the first external command needs one of these (any command
        // not in `confined_builtins()`). Found via `env | sort` inside a live
        // confined `run_command` showing only `PWD`/`SHLVL`/`_` — none of the
        // vars this function believed it had seeded.
        let mut path_var = ShellVariable::new(path_value);
        path_var.export();
        shell
            .env_mut()
            .set_global("PATH", path_var)
            .map_err(|e| ToolError::Exec(brush_io("seed PATH", &e)))?;

        // Windows: a child spawned under `do_not_inherit_env(true)` needs the
        // OS-minimal vars (`SystemRoot`, …) or `CreateProcess`/CRT init fails to
        // start it at all. These are not secrets — every Windows process needs
        // them — so seeding them keeps external commands and the carried-coreutils
        // re-exec runnable under confinement. Unix needs none of this.
        #[cfg(windows)]
        for key in [
            "SystemRoot",
            "SystemDrive",
            "windir",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "NUMBER_OF_PROCESSORS",
        ] {
            if let Ok(val) = std::env::var(key) {
                let mut var = ShellVariable::new(val);
                var.export();
                let _ = shell.env_mut().set_global(key, var);
            }
        }

        // Import the caller-provided env (the schema's `env` seam) LAST, so a
        // caller `PATH` (e.g. a venv-prepended one) wins over the exec-scope
        // seed above — matching the host and safe-subset engines. This does NOT
        // widen authority: `authorize_external_cmd` gates the RESOLVED PROGRAM against the
        // caveats regardless of `PATH` (brush_shell.schema.json). Nothing ambient
        // is inherited; only these explicitly-passed vars cross the boundary.
        for (key, val) in &env {
            let mut var = ShellVariable::new(val.clone());
            var.export();
            shell
                .env_mut()
                .set_global(key, var)
                .map_err(|e| ToolError::Exec(brush_io("seed env var", &e)))?;
        }

        // Brush expands PS4 when xtrace is enabled, including a second round of
        // command substitution when `promptvars` is on. PS4 is not part of the
        // inspected model command inventory, so allowing model text or imported
        // env to redefine it would create a hidden execution path:
        // `PS4='$(hidden-command)'; set -x`. Pin it to a literal readonly value
        // after every import. Readonly is essential because model text can turn
        // `promptvars` back on even if an embedder initially disables it.
        let mut ps4 = ShellVariable::new("+ ");
        ps4.set_readonly();
        shell
            .env_mut()
            .set_global("PS4", ps4)
            .map_err(|e| ToolError::Exec(brush_io("pin readonly PS4", &e)))?;

        let result = shell.run_dash_c_command(cmd).await.map_err(|e| {
            // A policy-marked terminating error must retain its actual reason.
            if e.is_terminating() {
                ToolError::denied(format!("brush run terminated: {e}"))
            } else {
                ToolError::Exec(brush_io("run command", &e))
            }
        })?;

        // Drop the shell so it releases its clones of the pipe writers; only then
        // do the reader threads see EOF.
        drop(shell);

        Ok::<i32, ToolError>(i32::from(u8::from(result.exit_code)))
    })?;

    // Still waited on, even though the captured text is discarded: the wait is
    // what synchronizes the drain threads' emission into the framed stream
    // before the worker writes its terminal.
    let (_, stdout_detached) = collect_drained(&out_rx, "stdout")?;
    let (_, stderr_detached) = collect_drained(&err_rx, "stderr")?;

    Ok(Captured {
        exit_code,
        stdout_detached,
        stderr_detached,
    })
}

/// Wall-clock ceiling for waiting on a drain thread before DETACHING it (FIX 4).
/// The drain finishes as soon as every writer — the shell's own clones plus any
/// dup a background child inherited — is closed; in the common case (no surviving
/// child) that is immediate after `drop(shell)`, so this bound is only ever hit
/// when a background child keeps a pipe-writer dup open. It caps how long a single
/// confined run can hold its `spawn_blocking` worker on drain, well under the
/// engine's wall-clock timeout.
const DRAIN_DETACH_DEADLINE: Duration = Duration::from_millis(500);

/// Collect a drain thread's captured output without ever pinning the (scarce)
/// `spawn_blocking` worker on it (FIX 4 / finding #7). Returns as soon as the
/// drain finishes; if a background child holds a pipe-writer dup past
/// [`DRAIN_DETACH_DEADLINE`], DETACHES the drain thread (a cheap leaked OS thread
/// that self-terminates when the child eventually exits) and returns empty — the
/// observer already received the live bytes; the worker is freed rather than hung
/// for the child's whole lifetime.
fn collect_drained(
    rx: &std::sync::mpsc::Receiver<ToolResult<String>>,
    stream: &str,
) -> ToolResult<(String, bool)> {
    use std::sync::mpsc::RecvTimeoutError;
    match rx.recv_timeout(DRAIN_DETACH_DEADLINE) {
        // The drain finished and reported its captured output (or a drain error).
        Ok(result) => result.map(|text| (text, false)),
        // A background child still holds the write pipe: detach, free the worker.
        // Reported as detached so the caller can record that output was omitted
        // rather than letting a short transcript read as a complete one.
        Err(RecvTimeoutError::Timeout) => Ok((String::new(), true)),
        // The drain thread dropped its sender without reporting — it panicked.
        Err(RecvTimeoutError::Disconnected) => Err(ToolError::denied(format!(
            "{stream} reader thread panicked"
        ))),
    }
}

/// The curated builtin set: the bash-mode default set with [`REMOVED_BUILTINS`]
/// stripped out (robust-by-construction — a removed builtin is simply gone, so a
/// confined shell running `exec` gets "command not found", never a spawn).
fn confined_builtins() -> HashMap<String, Registration<LeashedExtensions>> {
    let mut builtins = default_builtins::<LeashedExtensions>(BuiltinSet::BashMode);
    for name in REMOVED_BUILTINS {
        builtins.remove(*name);
    }
    builtins
}

/// Read a pipe to EOF (capped at `max` bytes), returning lossy UTF-8.
fn drain(
    reader: std::io::PipeReader,
    max: usize,
    output: &OutputEmitter,
    stream: crate::ShellOutputStream,
) -> ToolResult<String> {
    let (buf, _truncated) = drain_capped(reader, max, output, stream)
        .map_err(|e| ToolError::Exec(brush_io("drain pipe", &e)))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Wrap a brush/IO error with context as an [`std::io::Error`].
fn brush_io(ctx: &str, e: &impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("{ctx}: {e}"))
}

#[cfg(test)]
mod schema_tests {
    use super::*;
    #[test]
    fn output_capture_limit_is_finite_and_validated() {
        assert!(BrushShellTool::new().with_max_output_bytes(0).is_err());
        assert!(BrushShellTool::new()
            .with_max_output_bytes(crate::brush_protocol::MAX_CONFIGURED_OUTPUT + 1)
            .is_err());
        let limit = agent_bridle_core::LimitsPolicy::default().max_output_bytes;
        let tool = BrushShellTool::new().with_max_output_bytes(limit).unwrap();
        assert_eq!(tool.max_output, limit);
    }

    #[test]
    fn default_schema_describes_brush_without_host_shell_overclaims() {
        let schema = BrushShellTool::new().schema();
        let description = schema["properties"]["cmd"]["description"]
            .as_str()
            .expect("cmd description");
        assert!(description.contains("carried Brush shell"), "{description}");
        assert!(description.contains("exec caveat"), "{description}");
        assert!(description.contains("enforcement report"), "{description}");
        assert!(description.contains("sandbox_kind"), "{description}");
        assert!(!description.contains("/bin/sh -c"), "{description}");
        assert!(
            !description.contains("whole process tree inside the kernel"),
            "{description}"
        );
        assert!(
            !description.contains("Requires exec+net to be unrestricted"),
            "{description}"
        );
    }
}

/// FIX 2 cancellation-seam tests. These call `run_in_brush` directly in the test
/// process to exercise its per-run cancellation hook; production timeout
/// supervision instead terminates the dedicated worker from outside. They are
/// real-spawn by nature, with no mock. Unix-only for the fixed `/bin/*` external
/// paths that force the `authorize_external_cmd` funnel.
#[cfg(all(test, unix))]
mod cancel_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use agent_bridle_core::{Caveats, Denial, DenialKind, Gate, Scope, Tool, ToolResult};

    use crate::caveat_interceptor::DenialSink;

    #[test]
    fn authentication_cancellation_unblocks_before_the_startup_deadline() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut child = Command::new("/bin/sleep")
            .arg("5")
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .expect("start bounded authentication stand-in");
        let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        let mut channel = child.stdout.take().unwrap();
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        let result = std::thread::scope(|scope| {
            let (auth_started, ready) = std::sync::mpsc::channel();
            let cancellation = &cancelled;
            scope.spawn(move || {
                ready.recv().unwrap();
                cancellation.store(true, Ordering::Release);
            });
            watch_worker_authentication(pid, &cancelled, || {
                auth_started.send(()).unwrap();
                channel
                    .read_to_end(&mut Vec::new())
                    .map_err(ToolError::from)
            })
        });
        // The identity remains owned and unreaped until the watcher has joined.
        let status = child.wait().expect("reap authentication stand-in");
        assert!(result.is_ok());
        assert!(!status.success(), "cancellation must terminate the child");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Mint a `ToolContext` the only legitimate way — through the gate.
    fn ctx(granted: Caveats) -> agent_bridle_core::ToolContext {
        struct AnyTool;
        #[async_trait]
        impl Tool for AnyTool {
            fn name(&self) -> &str {
                "any"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn invoke(
                &self,
                _args: serde_json::Value,
                _cx: &agent_bridle_core::ToolContext,
            ) -> ToolResult<serde_json::Value> {
                Ok(serde_json::Value::Null)
            }
        }
        Gate::new(0)
            .authorize(&AnyTool, &granted)
            .expect("authorize")
    }

    /// A pre-tripped flag makes the run abort at the very first external-spawn
    /// boundary (`authorize_external_cmd`) instead of completing — and the abort is recorded
    /// as a structured `exec` denial, i.e. it REFUSED the spawn (OCAP-preserving),
    /// never allowed one.
    #[test]
    fn cancel_flag_aborts_at_the_next_external_command() {
        let cancel = Arc::new(AtomicBool::new(true));
        let sink: DenialSink = Arc::new(Mutex::new(Vec::new()));
        let interceptor = CaveatInterceptor::new(ctx(Caveats::top()), Arc::clone(&sink))
            .with_cancel(Arc::clone(&cancel));

        let res = run_in_brush(
            "/bin/echo hi".to_string(),
            None,
            RESTRICTED_PATH.to_string(),
            BTreeMap::new(),
            interceptor,
            DEFAULT_MAX_OUTPUT,
            OutputEmitter::default(),
        );

        assert!(
            res.is_err(),
            "a cancelled run must abort, not complete: {res:?}"
        );
        let recorded = sink.lock().expect("sink").clone();
        assert_eq!(recorded.len(), 1, "one cancellation denial: {recorded:?}");
        assert_eq!(recorded[0].kind, DenialKind::Exec);
    }

    /// The load-bearing recovery property, shared by every loop shape below: a
    /// loop that would spin forever is stopped PROMPTLY by tripping the flag
    /// mid-run, and the blocking worker FINISHES — no leaked, grinding thread
    /// (report open-Q #4) — returning a cancellation error rather than panicking.
    ///
    /// Hermetic by construction: the caveats grant no exec authority, so a loop
    /// body that *is* an external is refused at `authorize_external_cmd` (a cheap recorded
    /// denial, no real subprocess) while still cycling the interpreter.
    ///
    /// Returns the recorded denials so a caller can assert on them.
    fn assert_loop_is_cancellable(cmd: &str, path: &str, what: &str) -> Vec<Denial> {
        let cancel = Arc::new(AtomicBool::new(false));
        let sink: DenialSink = Arc::new(Mutex::new(Vec::new()));
        let cx = ctx(Caveats {
            exec: Scope::only(["__never_in_scope__".to_string()]),
            ..Caveats::top()
        });
        let interceptor =
            CaveatInterceptor::new(cx, Arc::clone(&sink)).with_cancel(Arc::clone(&cancel));

        let (cmd, path) = (cmd.to_string(), path.to_string());
        let worker = std::thread::spawn(move || {
            run_in_brush(
                cmd,
                None,
                path,
                BTreeMap::new(),
                interceptor,
                DEFAULT_MAX_OUTPUT,
                OutputEmitter::default(),
            )
        });

        // Let the loop get going; it is infinite, so it must still be running.
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !worker.is_finished(),
            "the {what} loop should still be spinning before cancel"
        );

        // Trip the flag: the next `pre_simple_cmd` observes it and terminates.
        cancel.store(true, Ordering::SeqCst);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() {
            assert!(
                Instant::now() < deadline,
                "cancel did not stop the {what} loop — the blocking worker leaked"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let res = worker
            .join()
            .expect("the worker must return cleanly, not panic");
        assert!(
            res.is_err(),
            "a cancelled {what} run returns a cancellation error: {res:?}"
        );
        let recorded = sink.lock().expect("sink").clone();
        recorded
    }

    /// **The headline bound.** A loop of PURE BUILTINS reaches no external-spawn
    /// and no file-open boundary, so before `pre_simple_cmd` existed it had no
    /// observation point at all: the timeout fired, the caller got exit 124, and
    /// the detached worker span a CPU until process exit. `pre_simple_cmd` fires
    /// once per command — builtins included — and its `Deny` terminates the run
    /// instead of being folded into an exit status the enclosing loop shrugs off.
    #[test]
    fn tripping_cancel_stops_a_pure_builtin_loop() {
        assert_loop_is_cancellable("while true; do :; done", "", "pure-builtin");
    }

    /// The external-command shape: unchanged behavior, now observed one hook
    /// earlier (`pre_simple_cmd` precedes `authorize_external_cmd` on every path).
    #[test]
    fn tripping_cancel_stops_a_loop_of_an_external_command() {
        assert_loop_is_cancellable(
            "while true; do /bin/true; done",
            RESTRICTED_PATH,
            "external",
        );
    }

    /// **The carried-coreutils cancellation guard.** A loop whose body is a
    /// CARRIED util (`cat`, registered as a shim builtin) must remain
    /// cancellable — the same property the shapes above pin.
    ///
    /// This now holds for the strongest reason: `pre_simple_cmd` fires for the
    /// shim itself, so cancellation no longer depends on the shim's re-exec
    /// crossing `authorize_external_cmd`. It remains a regression guard for the carried
    /// re-exec path: the denial assertion below pins that a carried utility
    /// crosses the leash.
    #[cfg(feature = "carried-coreutils")]
    #[test]
    fn tripping_cancel_stops_a_loop_of_a_carried_coreutil() {
        // `cat` resolves to the carried shim, NOT to /bin/cat: PATH is empty, so
        // nothing external could satisfy it.
        let recorded = assert_loop_is_cancellable(
            "while true; do cat /etc/hostname; done",
            "",
            "carried-util",
        );
        // The loop really did reach the admission seam every iteration (rather
        // than bypassing the worker's leash).
        assert!(
            recorded.iter().any(|d| d.kind == DenialKind::Exec),
            "the carried-util loop must register exec-axis denials: {recorded:?}"
        );
    }
}

/// FIX 4 detach-mechanism tests. The load-bearing behavior — free the scarce
/// `spawn_blocking` worker instead of pinning it for the whole lifetime of a
/// background child that holds a pipe-writer dup — lives in `collect_drained`, so
/// it is exercised DIRECTLY here. (An end-to-end `sleep 5 & echo hi` invoke would
/// route through brush's real `&` job control, which is inherently racy on the
/// per-run current-thread runtime — finding #7 / Effort B — and makes a timing
/// assertion flaky; the detach itself is deterministic.)
#[cfg(test)]
mod drain_tests {
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    /// A writer kept open (as a surviving background child would keep its dup)
    /// must NOT pin the caller: `collect_drained` bounded-waits to the deadline,
    /// then DETACHES and returns — freeing the worker while a cheap OS drain
    /// thread lingers until the writer finally closes.
    #[test]
    fn collect_drained_detaches_when_a_writer_stays_open() {
        let (reader, writer) = std::io::pipe().expect("pipe");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(drain(
                reader,
                DEFAULT_MAX_OUTPUT,
                &OutputEmitter::default(),
                crate::ShellOutputStream::Stdout,
            ));
        });

        let start = Instant::now();
        let out = collect_drained(&rx, "stdout").expect("no drain error");
        let elapsed = start.elapsed();

        assert!(
            elapsed >= DRAIN_DETACH_DEADLINE
                && elapsed < DRAIN_DETACH_DEADLINE + Duration::from_secs(2),
            "must detach ~at the deadline, not block on the held-open writer: {elapsed:?}"
        );
        assert_eq!(
            out,
            (String::new(), true),
            "detached before EOF → empty capture, and the detach is REPORTED so \
             the terminal can record that output was omitted"
        );

        // Releasing the writer lets the detached drain thread reach EOF and end.
        drop(writer);
    }

    /// The common case: once every writer is closed the drain reaches EOF and
    /// `collect_drained` returns the FULL captured output PROMPTLY — well under
    /// the detach deadline — so normal runs lose nothing to the backstop.
    #[test]
    fn collect_drained_returns_full_output_promptly_when_writers_close() {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"captured-output").expect("write");
        drop(writer); // EOF: no surviving writer dup.

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(drain(
                reader,
                DEFAULT_MAX_OUTPUT,
                &OutputEmitter::default(),
                crate::ShellOutputStream::Stdout,
            ));
        });

        let start = Instant::now();
        let out = collect_drained(&rx, "stdout").expect("no drain error");
        assert_eq!(
            out,
            ("captured-output".to_string(), false),
            "full foreground output is captured, with no detach reported"
        );
        assert!(
            start.elapsed() < DRAIN_DETACH_DEADLINE,
            "must return as soon as the drain EOFs, not wait out the detach deadline"
        );
    }
}

#[cfg(test)]
mod handshake_error_tests {
    use super::*;
    use crate::brush_protocol::{write_pre_start_terminal, WorkerResponse};
    use std::io::ErrorKind;

    #[test]
    fn handshake_error_prefers_worker_error_over_transport_error() {
        let response = WorkerResponse::failure("worker nonce mismatch");
        let mut stdout = Vec::new();
        write_pre_start_terminal(&mut stdout, &response).expect("frame terminal response");
        let reason = brush_worker_handshake_error_message("transport failed", &stdout, &[]);
        assert_eq!(reason, "worker nonce mismatch");
    }

    #[test]
    fn handshake_error_falls_back_to_stderr_when_no_structured_response() {
        let transport_error = std::io::Error::from(ErrorKind::ConnectionReset);
        let reason = brush_worker_handshake_error_message(
            &transport_error.to_string(),
            b"not-json",
            b"stderr text",
        );
        assert!(reason.starts_with("connection reset"));
        assert!(reason.contains("stderr text"));
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
