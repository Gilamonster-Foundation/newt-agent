//! The **sandboxed-host shell engine** (ADR 0019, #194): full-shell semantics
//! with the guarantee entirely on L3.
//!
//! Unlike the safe-subset [`ShellTool`](crate::ShellTool), this engine does
//! **no L2 parsing**. It spawns the OS shell (`/bin/sh -c <cmd>`, or an
//! embedder-configured shell) with the whole process tree inside the L3 jail via
//! [`ConfinedCommand`], which admission-checks `exec`, **fails closed** when a
//! restricted `fs_write` cannot be enforced, applies Landlock (Linux) / Seatbelt
//! (macOS), and reports the honest [`SandboxKind`]. The engine contributes zero
//! enforcement; the jail is everything.
//!
//! Honesty posture (ADR 0019 D2): it serves *fs restricted (kernel-jailed),
//! exec/net unrestricted*. A restricted `exec` or `net` grant is **refused** with
//! a structured "engine unavailable for this grant" denial — inside a full shell
//! the engine cannot bound the shell's forked children (a single `check_exec` on
//! `/bin/sh` says nothing about what it runs), so claiming to confine them would
//! be a lie (I9). Restricted-exec/net requests belong on the safe-subset engine.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use agent_bridle_core::{
    Caveats, ConfinedCommand, Denial, DenialKind, Disclosure, Invocation, SandboxKind,
    SandboxPolicy, Scope, Tool, ToolContext, ToolEnvelope, ToolError, ToolResult,
};
use async_trait::async_trait;

use crate::output_observer::{drain_capped, output_session};

/// The engine identity stamped on the disclosure (ADR 0019 D4).
const ENGINE_NAME: &str = "sandbox-host";

/// Default cap on captured output bytes (mirrors the safe-subset engine's
/// [`LimitsPolicy`](agent_bridle_core::LimitsPolicy) default; kept local so the
/// engine has no extra config surface for the MVP).
const DEFAULT_MAX_OUTPUT: usize = 64 * 1024;

/// The engine's input schema, parsed once from the embedded data file. The
/// schema is **knowledge**, so it lives in `host_shell.schema.json` (plain-text
/// data), not inline in [`Tool::schema`] — three-Cs: knowledge in data, not
/// logic. It is the *default* for the tool's overridable `schema` property
/// ([`HostShellTool::with_schema`]); `include_str!` binds the data at compile
/// time, so a malformed file fails the build's tests, never a live dispatch.
static DEFAULT_SCHEMA: LazyLock<Arc<serde_json::Value>> = LazyLock::new(|| {
    Arc::new(
        serde_json::from_str(include_str!("host_shell.schema.json"))
            .expect("embedded host_shell.schema.json must be valid JSON"),
    )
});

/// The sandboxed-host shell engine — a [`Tool`] that runs a free-form command
/// through the OS shell inside the L3 jail (ADR 0019). Registered under
/// `"shell"` so it is a construction-time engine choice (the embedder builds the
/// registry with this instead of [`ShellTool`](crate::ShellTool)); the two honor
/// the same `Tool` / `Caveats` / envelope contract.
#[derive(Clone)]
pub struct HostShellTool {
    shell: String,
    max_output: usize,
    timeout: std::time::Duration,
    sandbox: Arc<SandboxPolicy>,
    /// The tool's input schema — a *property* of this engine instance, not a
    /// hard-coded literal in [`Tool::schema`]. Defaults to the embedded
    /// [`DEFAULT_SCHEMA`] data file; overridable via
    /// [`HostShellTool::with_schema`] (three-Cs: Configuration).
    schema: Arc<serde_json::Value>,
    output_observer: Option<Arc<dyn crate::ShellOutputObserver>>,
    execution_lease: Option<crate::ExecutionLease>,
}

impl std::fmt::Debug for HostShellTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostShellTool")
            .field("shell", &self.shell)
            .finish_non_exhaustive()
    }
}

impl Default for HostShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl HostShellTool {
    /// The engine with the default shell (`/bin/sh`) and built-in sandbox policy.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shell: "/bin/sh".to_string(),
            max_output: DEFAULT_MAX_OUTPUT,
            timeout: std::time::Duration::from_secs(
                agent_bridle_core::LimitsPolicy::default().default_timeout_secs,
            ),
            sandbox: Arc::new(SandboxPolicy::default()),
            schema: DEFAULT_SCHEMA.clone(),
            output_observer: None,
            execution_lease: None,
        }
    }

    /// Bound this invocation using the shared process-group supervisor.
    #[must_use]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Attach a presentation-only observer for bounded stdout/stderr chunks.
    ///
    /// The observer is queued only for a shell that passed admission and cannot
    /// change the final result envelope. Delivery may finish asynchronously
    /// after the invocation returns; `on_finish` marks the queue-drained boundary.
    #[must_use]
    pub fn with_output_observer(mut self, observer: Arc<dyn crate::ShellOutputObserver>) -> Self {
        self.output_observer = Some(observer);
        self
    }

    /// Retain resources until the blocking process owner finishes, including
    /// when its async waiter is dropped. This does not add host-shell
    /// cancellation support; the configured timeout still governs shutdown.
    #[must_use]
    pub fn with_execution_lease(mut self, lease: crate::ExecutionLease) -> Self {
        self.execution_lease = Some(lease);
        self
    }

    /// Override the tool's input schema (three-Cs: Configuration). Defaults to
    /// the embedded `host_shell.schema.json`; an embedder presenting a different
    /// `cmd`/`env`/`cwd` contract sets its own schema here rather than forking
    /// the engine.
    #[must_use]
    pub fn with_schema(mut self, schema: serde_json::Value) -> Self {
        self.schema = Arc::new(schema);
        self
    }

    /// Set the shell binary the engine spawns (embedder-configured, ADR 0019
    /// D5.3 — never a per-dispatch model input). Default `/bin/sh`.
    #[must_use]
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = shell.into();
        self
    }

    /// Set the sandbox mechanism policy the L3 backend enforces.
    #[must_use]
    pub fn sandbox_policy(mut self, policy: Arc<SandboxPolicy>) -> Self {
        self.sandbox = policy;
        self
    }

    /// A structured "engine unavailable for this grant" denial (ADR 0019 D5.2) —
    /// `denied: true`, so a consumer can adapt (reshape, or select the
    /// safe-subset engine), never a silent fallback.
    fn engine_unavailable(&self, kind: DenialKind, axis: &str) -> serde_json::Value {
        ToolEnvelope::new(SandboxKind::None)
            .with_disclosure(self.disclosure())
            .with_denials(vec![Denial {
                kind,
                target: format!("sandbox-host engine ({axis} restricted)"),
                reason: format!(
                    "the sandbox-host engine does not serve a restricted `{axis}` grant: \
                     inside a full shell it cannot bound the shell's forked children, so it \
                     refuses rather than claim confinement it does not have (ADR 0019 D2). \
                     Use the safe-subset engine for a restricted `{axis}` grant."
                ),
            }])
            .into_json()
    }

    fn disclosure(&self) -> Disclosure {
        Disclosure {
            engine: Some(ENGINE_NAME.to_string()),
            ..Disclosure::default()
        }
    }
}

/// Is a scope restricted (an allow-list), i.e. NOT the ambient `All`? A
/// restricted `exec`/`net` is the case the sandbox-host engine cannot honestly
/// serve (ADR 0019 D2).
fn is_restricted<T: Ord + Clone>(scope: &Scope<T>) -> bool {
    !matches!(scope, Scope::All)
}

#[async_trait]
impl Tool for HostShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn schema(&self) -> serde_json::Value {
        // The schema is a property of the engine (default: the embedded
        // `host_shell.schema.json` data file), not a hard-coded literal here.
        (*self.schema).clone()
    }

    async fn invoke(
        &self,
        args: serde_json::Value,
        cx: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        self.invoke_accounted(args, cx)
            .await
            .map(Invocation::into_value)
    }

    /// Like [`ShellTool`](crate::ShellTool), this engine returns a **pre-run**
    /// authority refusal (`engine_unavailable`) as an in-band `Ok` envelope, so
    /// it reports the typed [`Invocation`] itself — `Denied` for the pre-run
    /// engine-unavailable refusals, `Ran` for the spawned command's envelope —
    /// so the call budget is charged only for a run that actually happened.
    async fn invoke_accounted(
        &self,
        args: serde_json::Value,
        cx: &ToolContext,
    ) -> ToolResult<Invocation> {
        let cmd = args
            .get("cmd")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::denied("sandbox-host: missing required `cmd` string"))?
            .to_string();

        // ADR 0019 D2 — honesty. The engine only serves fs-restricted; a
        // restricted exec/net grant is refused (the shell forks children this
        // engine cannot bound). fs restriction is handled by ConfinedCommand's
        // fail-closed spawn below.
        let caveats: &Caveats = cx.caveats();
        if is_restricted(&caveats.exec) {
            return Ok(Invocation::Denied(
                self.engine_unavailable(DenialKind::Exec, "exec"),
            ));
        }
        if is_restricted(&caveats.net) {
            return Ok(Invocation::Denied(
                self.engine_unavailable(DenialKind::Net, "net"),
            ));
        }

        let mut env: BTreeMap<String, String> = args
            .get("env")
            .and_then(serde_json::Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        // Full-access PATH seeding (Track 1a / parity). We only reach here when
        // `exec` is unrestricted (a restricted exec/net was refused above), so the
        // child is already permitted to run anything on the host. Seed a usable
        // `PATH` — the ambient `$PATH`, else the conventional search dirs — so bare
        // program names (`grep`/`ls`/`find`, and tools in non-standard dirs like
        // `~/.cargo/bin` / `/opt/homebrew/bin`) resolve like the host shell,
        // instead of leaning on the shell's fragile compiled `_CS_PATH` fallback.
        // `ConfinedCommand`'s `env_clear` still scrubs everything else; a
        // caller-provided `PATH` wins.
        env.entry("PATH".to_string())
            .or_insert_with(agent_bridle_core::default_exec_path);
        let cwd = args.get("cwd").and_then(serde_json::Value::as_str);
        let max_output = self.max_output;

        // Build the confined spawn: `/bin/sh -c <cmd>`, empty env + granted vars,
        // piped output. ConfinedCommand admission-checks exec (the shell binary,
        // permitted under exec=All), fail-closes on unenforceable fs, applies the
        // OS jail, and reports the honest kind.
        let mut command = ConfinedCommand::new(&self.shell)
            .new_process_group()
            .arg("-c")
            .arg(crate::with_posix_command_discovery(&cmd))
            .sandbox_policy(Arc::clone(&self.sandbox))
            .stdin(agent_bridle_core::ConfinedStdio::Null)
            .stdout(agent_bridle_core::ConfinedStdio::Piped)
            .stderr(agent_bridle_core::ConfinedStdio::Piped);
        for (k, v) in &env {
            command = command.env(k, v);
        }
        if let Some(dir) = cwd {
            command = command.current_dir(dir);
        }

        // The spawn + blocking wait runs off the async runtime. `spawn` itself
        // enforces the fail-closed fs check and applies the jail on the spawn
        // thread, so the child inherits confinement.
        let confined = command.spawn(cx)?;
        let sandbox_kind = confined.sandbox_kind;
        let mut child = confined.child;
        let stdout_reader = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Exec(std::io::Error::other("stdout pipe missing")))?;
        let stderr_reader = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Exec(std::io::Error::other("stderr pipe missing")))?;
        let (output_guard, output) = output_session(self.output_observer.clone(), max_output);

        let timeout = self.timeout;
        let execution_lease = self.execution_lease.clone();
        let captured = tokio::task::spawn_blocking(move || {
            let _execution_lease = execution_lease;
            let stdout_output = output.clone();
            let stdout = std::thread::spawn(move || {
                drain_capped(
                    stdout_reader,
                    max_output,
                    &stdout_output,
                    crate::ShellOutputStream::Stdout,
                )
            });
            let stderr = std::thread::spawn(move || {
                drain_capped(
                    stderr_reader,
                    max_output,
                    &output,
                    crate::ShellOutputStream::Stderr,
                )
            });

            let status = crate::supervisor::supervise(std::slice::from_mut(&mut child), timeout);
            // A shell can exit with background descendants still holding pipes.
            // Terminate its group before joining the bounded output readers.
            crate::kill_child_tree(&mut child);
            let stdout = stdout.join().map_err(|_| {
                ToolError::Exec(std::io::Error::other("stdout reader thread panicked"))
            });
            let stderr = stderr.join().map_err(|_| {
                ToolError::Exec(std::io::Error::other("stderr reader thread panicked"))
            });

            let status = status?;
            let (stdout, stdout_truncated) = stdout??;
            let (stderr, stderr_truncated) = stderr??;
            Ok::<_, ToolError>((status, stdout, stdout_truncated, stderr, stderr_truncated))
        })
        .await
        .map_err(|e| ToolError::Exec(std::io::Error::other(format!("join: {e}"))))??;

        let ((exit_code, timed_out), stdout, stdout_truncated, stderr, stderr_truncated) = captured;
        let stdout = cap_utf8(&stdout, max_output, stdout_truncated);
        let stderr = if timed_out {
            format!(
                "command timed out after {}s; process group terminated",
                timeout.as_secs()
            )
        } else {
            cap_utf8(&stderr, max_output, stderr_truncated)
        };
        let envelope = ToolEnvelope::new(sandbox_kind)
            .with_disclosure(self.disclosure())
            .with_exit_code(exit_code)
            .with_stdout(stdout)
            .with_stderr(stderr)
            .with_timed_out(timed_out)
            .into_json();
        if !timed_out {
            output_guard.finish();
        }
        Ok(Invocation::Ran(envelope))
    }
}

/// Lossy-decode captured bytes and cap at `max` bytes (on a char boundary).
fn cap_utf8(bytes: &[u8], max: usize, truncated: bool) -> String {
    let s = String::from_utf8_lossy(bytes);
    if !truncated && s.len() <= max {
        return s.into_owned();
    }
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default schema loads from the embedded `host_shell.schema.json` data
    /// file (not an inline literal) and has the `cmd`/`env`/`cwd` shape. Guards
    /// the data file against corruption — a bad edit fails here, not in prod.
    #[test]
    fn default_schema_loads_from_data_file_with_expected_shape() {
        let s = HostShellTool::new().schema();
        assert_eq!(s["type"], "object");
        assert_eq!(s["required"], serde_json::json!(["cmd"]));
        for key in ["cmd", "env", "cwd"] {
            assert!(
                s["properties"].get(key).is_some(),
                "schema is missing the `{key}` property: {s}"
            );
        }
    }

    /// The schema is a configurable *property* of the engine (three-Cs), so an
    /// embedder can override it at construction.
    #[test]
    fn with_schema_overrides_the_property() {
        let custom = serde_json::json!({ "type": "object", "properties": {} });
        let s = HostShellTool::new().with_schema(custom.clone()).schema();
        assert_eq!(s, custom);
    }
}
