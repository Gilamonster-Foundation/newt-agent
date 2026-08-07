//! `ConstrainedExecutor` — the single confined-subprocess seam (P4 /
//! `p4-constrained-executor`).
//!
//! # Why this exists
//!
//! Attacker-influenced subprocesses — a model-authored `run_command`, a
//! repository-configured `build_check_cmd`, a roadmap `verify` string, a crew
//! formatter — historically each reached the OS through a *raw* `sh -c` /
//! `Command::spawn` that inherited newt's full environment and ran with no
//! kernel confinement. That is one whole class of confused-deputy escapes, one
//! per spawn site. This module makes the escape *unrepresentable* by giving
//! every such spawn ONE typed door.
//!
//! An [`ExecRequest`] declares, explicitly and up front, everything the child
//! is allowed: its `program` + `args`, its `cwd`, the [`Caveats`] fence
//! (fs / net / exec authority), the exact environment grants (nothing else — the
//! child starts env-EMPTY), and its [`ExecOrigin`] trust class. The executor
//! then mints the confinement through the SAME audited path
//! [`newt-mcp-client`](../../newt_mcp_client) already uses for MCP stdio servers
//! —  `agent_bridle::ConfinedCommand` + the `Gate` — so a single, reviewed
//! implementation confines them all.
//!
//! # The fail-closed law (targets #7–#10)
//!
//! An [`ExecOrigin::AgentInfluenced`] request is minted under a **`Kernel`
//! strength floor**. `agent_bridle::ConfinedCommand::spawn` then refuses
//! (`confinement_unenforceable`) whenever a restricted fs axis cannot be
//! *kernel*-enforced — on Linux without Landlock, or on any platform without a
//! kernel fs backend at all. So:
//!
//! - **#7** no attacker-influenced child runs outside this executor (raw spawns
//!   are migrated away and inventory-gated),
//! - **#8** the child cannot inherit credentials or authority switches — its
//!   environment is EMPTY plus only the explicit grants,
//! - **#9** an empty `net` fence becomes a kernel deny-all (Landlock ABI-v4 /
//!   Seatbelt), so untrusted code has no network without an explicit grant,
//! - **#10** where the required kernel enforcement is unavailable the spawn is
//!   **refused**, never silently downgraded to an unconfined run.
//!
//! Linux is the normative fully-supported platform; on a platform that cannot
//! kernel-confine, an `AgentInfluenced` spawn fails closed rather than pretend.
//! ([`ExecOrigin::TrustedInfra`] carries the default floor for the small set of
//! fixed-argv internal helpers that are *not* attacker-influenced; it is here
//! for signature completeness and is not used by the agent-exec paths.)

use std::path::{Path, PathBuf};

use agent_bridle::{AxisEnforcement, ConfinedCommand, Gate, Tool, ToolContext, ToolError};

use crate::caveats::{Caveats, Scope};

/// The trust class of the code being executed — the axis that decides how hard
/// the executor must confine before it will run anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecOrigin {
    /// The program, its argv, or its behavior is influenced by the model or by
    /// repository-authored content (a build-check command, a formatter, a
    /// roadmap verify string, a model `run_command`). Minted under a `Kernel`
    /// strength floor: the spawn is **refused** if the fs fence cannot be
    /// kernel-enforced (fail-closed, #10).
    AgentInfluenced,
    /// A fixed-argv internal helper the operator authored (not model/repo
    /// influenced). Carries the default strength floor. Reserved for helpers the
    /// spawn inventory classifies `trusted-*`; the attacker-influenced paths
    /// always use [`ExecOrigin::AgentInfluenced`].
    TrustedInfra,
}

impl ExecOrigin {
    /// The `Gate` an origin authorizes through — `AgentInfluenced` raises the
    /// enforcement floor to `Kernel` so an un-kernel-enforceable fence refuses.
    fn gate(self) -> Gate {
        match self {
            Self::AgentInfluenced => Gate::new(0).with_strength_floor(AxisEnforcement::Kernel),
            Self::TrustedInfra => Gate::new(0),
        }
    }
}

/// A fully-specified request to run one confined subprocess. Every field is
/// explicit: there is no ambient default that could widen the child's
/// authority. The child starts env-EMPTY — [`env`](Self::env) grants are the
/// child's *entire* environment.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    origin: ExecOrigin,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    caveats: Caveats,
    env: Vec<(String, String)>,
}

impl ExecRequest {
    /// A request to run `program` with `args` in `cwd`, confined by `caveats`.
    /// The environment starts empty; add grants with [`env`](Self::env).
    #[must_use]
    pub fn new(
        origin: ExecOrigin,
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<PathBuf>,
        caveats: Caveats,
    ) -> Self {
        Self {
            origin,
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            caveats,
            env: Vec::new(),
        }
    }

    /// Grant one environment variable to the child. Absent any grant the child
    /// runs with an EMPTY environment — no inherited credentials or authority
    /// switches (#8).
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.env.push((key.into(), val.into()));
        self
    }

    /// Grant several environment variables at once.
    #[must_use]
    pub fn envs(mut self, vars: impl IntoIterator<Item = (String, String)>) -> Self {
        self.env.extend(vars);
        self
    }

    /// The environment grants (test/inspection accessor). This is the child's
    /// entire environment — anything not listed here is unset in the child.
    #[must_use]
    pub fn env_grants(&self) -> &[(String, String)] {
        &self.env
    }
}

/// Why a confined spawn did not run. Every variant is a refusal to weaken the
/// claim: the executor never falls back to an unconfined run.
#[derive(Debug)]
pub enum ExecRefused {
    /// The fence could not be kernel-enforced on this platform/kernel, so the
    /// spawn was refused rather than run unconfined (#10). Carries the honest
    /// reason from `agent-bridle`.
    ConfinementUnenforceable(String),
    /// The gate refused to mint the confinement (e.g. the program is outside the
    /// exec allow-list, or the call budget is exhausted).
    Authorize(String),
    /// The program was confined successfully but the OS failed to start or wait
    /// on it (spawn error, I/O error draining its output).
    Spawn(std::io::Error),
}

impl std::fmt::Display for ExecRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfinementUnenforceable(why) => write!(
                f,
                "refused (fail-closed): confinement cannot be kernel-enforced here — {why}"
            ),
            Self::Authorize(why) => write!(f, "refused: {why}"),
            Self::Spawn(e) => write!(f, "confined spawn failed: {e}"),
        }
    }
}

impl std::error::Error for ExecRefused {}

/// The result of a completed confined run: the child's exit outcome, its
/// captured output, and the OS sandbox that was ACTUALLY applied (honest — never
/// over-claimed; [`agent_bridle::SandboxKind::None`] means advisory only).
#[derive(Debug)]
pub struct ConfinedOutput {
    /// True iff the child exited with a success status.
    pub success: bool,
    /// The exit code, if the process exited with one.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The OS sandbox actually applied to the child.
    pub sandbox_kind: agent_bridle::SandboxKind,
}

/// A throwaway [`Tool`] used only to mint the spawn [`ToolContext`] through the
/// gate. The confined spawn admission-checks the *program*, not this tool's
/// name, so the identity is immaterial. Module-scoped (mirrors
/// `newt-mcp-client`'s `McpSpawnTool`) so its trivial impl is unit-testable.
struct ExecSpawnTool;

#[async_trait::async_trait]
impl Tool for ExecSpawnTool {
    fn name(&self) -> &str {
        "confined_exec"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn invoke(
        &self,
        _args: serde_json::Value,
        _cx: &ToolContext,
    ) -> agent_bridle::ToolResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

/// Mint the spawn [`ToolContext`] the only legitimate way — through the gate —
/// bounded by `caveats` and the origin's strength floor.
fn mint_context(origin: ExecOrigin, caveats: &Caveats) -> Result<ToolContext, ExecRefused> {
    origin
        .gate()
        .authorize(&ExecSpawnTool, caveats)
        .map_err(|e| ExecRefused::Authorize(e.to_string()))
}

/// Whether this platform can actually **kernel-enforce** the workspace fs fence
/// an [`ExecOrigin::AgentInfluenced`] spawn requires. When `false`, such a spawn
/// FAILS CLOSED ([`ExecRefused::ConfinementUnenforceable`]) rather than running
/// unconfined — the honest signal a caller (or a test) uses to know whether the
/// executor will confine or refuse on this host.
#[must_use]
pub fn kernel_fs_fence_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        agent_bridle::landlock_is_supported()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The single confined-subprocess executor. Every attacker-influenced spawn in
/// newt routes through [`run`](Self::run); there is no other confined-spawn
/// implementation to keep in sync.
pub struct ConstrainedExecutor;

impl ConstrainedExecutor {
    /// Run `req` to completion, confined. Blocks until the child exits (like the
    /// raw `Command::output` it replaces); returns [`ExecRefused`] without ever
    /// running the child unconfined.
    ///
    /// On `AgentInfluenced` origin the `Kernel` strength floor makes
    /// `agent_bridle::ConfinedCommand::spawn` refuse when the fs fence cannot be
    /// kernel-enforced — so a platform/kernel without Landlock (or any OS fs
    /// backend) fails closed here rather than running the child unconfined.
    pub fn run(req: &ExecRequest) -> Result<ConfinedOutput, ExecRefused> {
        let cx = mint_context(req.origin, &req.caveats)?;

        let mut cmd = ConfinedCommand::new(&req.program)
            .args(&req.args)
            .current_dir(&req.cwd)
            // A fresh process group so a supervisor could kill the whole
            // descendant tree (the executor's own cancellation is a follow-up;
            // the group is set up now so it is available without a re-spawn).
            .new_process_group()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // The child's ENTIRE environment: the explicit grants and nothing else
        // (ConfinedCommand starts env-empty). No inherited credentials/switches.
        for (k, v) in &req.env {
            cmd = cmd.env(k, v);
        }

        let child = cmd.spawn(&cx).map_err(|e| match e {
            // A `Denied` from spawn is the fail-closed refusal: the fence could
            // not be kernel-enforced (confinement_unenforceable) — do NOT fall
            // back to an unconfined run.
            ToolError::Denied { .. } => ExecRefused::ConfinementUnenforceable(e.to_string()),
            other => ExecRefused::Authorize(other.to_string()),
        })?;

        let sandbox_kind = child.sandbox_kind;
        // `wait_with_output` drains stdout/stderr concurrently (its own threads)
        // so a child that fills a pipe buffer cannot deadlock the wait.
        let out = child.child.wait_with_output().map_err(ExecRefused::Spawn)?;

        Ok(ConfinedOutput {
            success: out.status.success(),
            code: out.status.code(),
            stdout: out.stdout,
            stderr: out.stderr,
            sandbox_kind,
        })
    }
}

/// A `Caveats` fence for an attacker-influenced subprocess confined to
/// `workspace`: it may read and write **only** beneath the workspace root, run
/// any program (an interpreter's *identity* is admitted; its behavior is bounded
/// by the fs/net axes — see [`AxisEnforcement::Kernel`]'s doc), and reach **no**
/// network (empty `net` → kernel deny-all). Built axis-by-axis so a headless
/// dispatch path never even mentions `Caveats::top()` (the `#94` no-top-leak
/// guard).
#[must_use]
pub fn workspace_confined_caveats(workspace: &Path) -> Caveats {
    let root = workspace.to_string_lossy().into_owned();
    Caveats {
        fs_read: Scope::only([root.clone()]),
        fs_write: Scope::only([root]),
        exec: Scope::All,
        // Empty net allow-list → a kernel deny-all under Landlock ABI-v4 /
        // Seatbelt. Untrusted code has no network without an explicit grant (#9).
        net: Scope::none(),
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// A `Caveats` fence for a repo-configured **build / test / format tool** run on
/// the model's edits (`build_check_cmd`, crew formatters, roadmap `verify`).
///
/// It widens [`workspace_confined_caveats`]'s reads to the **toolchain + package
/// cache** a real build tool needs (so `cargo`/`rustc` resolve and cached deps
/// are found) but NOT `$HOME` broadly — `~/.ssh`, `~/.aws`, `/etc/shadow`, and
/// arbitrary secrets stay unreadable. That closes a read-then-disclose path: a
/// hostile `build_check_cmd = "cat ~/.ssh/id_rsa"` would otherwise surface the
/// key in the tool output the model sees, and the net fence alone (which stops
/// the *child* exfiltrating) does not close the child→output→model channel.
///
/// The dangerous halves stay fully fenced — `fs_write` is the workspace only (no
/// poisoning the shared cache, no shared `/tmp`), `net` is an empty deny-all, and
/// scratch belongs in the fence (callers point `TMPDIR` at the workspace). A
/// child a hostile build spawns inherits the same Landlock fence.
#[must_use]
pub fn build_tool_caveats(workspace: &Path) -> Caveats {
    build_tool_caveats_with_writes(workspace, &[])
}

/// [`build_tool_caveats`] plus additional writable roots — for a build tool that
/// legitimately writes OUTSIDE the workspace to an operator-configured location,
/// e.g. crew's shared `CARGO_TARGET_DIR` (one incremental target across the
/// sequential worktrees). Each extra root is an explicit, reviewed grant; `net`
/// stays denied and the read set stays the calibrated toolchain/cache set. Only
/// add roots the OPERATOR configured, not anything the repository controls.
#[must_use]
pub fn build_tool_caveats_with_writes(workspace: &Path, extra_write_roots: &[String]) -> Caveats {
    let mut write_roots = vec![workspace.to_string_lossy().into_owned()];
    write_roots.extend(extra_write_roots.iter().cloned());
    Caveats {
        // Calibrated reads: workspace + the toolchain/package-cache roots build
        // tools need — NOT all of `$HOME` (so ~/.ssh etc. stay unreadable). The
        // system dirs (/usr, /lib, loaders) are covered by the sandbox backend's
        // base read paths; here we add the workspace and the per-user caches.
        fs_read: Scope::only(build_tool_read_roots(workspace)),
        fs_write: Scope::only(write_roots),
        exec: Scope::All,
        net: Scope::none(),
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// The read roots a build tool needs beyond the sandbox backend's base system
/// paths: the workspace and the per-user toolchain / package caches (Cargo,
/// rustup, and the XDG cache) — resolved from the operator's environment, never
/// `$HOME` as a whole. Deliberately excludes credential dirs (`~/.ssh`, `~/.aws`,
/// `~/.gnupg`): a build tool has no reason to read those, and granting them would
/// reopen the read-then-disclose path.
fn build_tool_read_roots(workspace: &Path) -> Vec<String> {
    let mut roots = vec![workspace.to_string_lossy().into_owned()];
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // (env var that overrides the default, default subdir under HOME)
    for (var, default) in [
        ("CARGO_HOME", ".cargo"),
        ("RUSTUP_HOME", ".rustup"),
        ("XDG_CACHE_HOME", ".cache"),
    ] {
        if let Some(explicit) = std::env::var_os(var) {
            roots.push(explicit.to_string_lossy().into_owned());
        } else if let Some(home) = &home {
            roots.push(home.join(default).to_string_lossy().into_owned());
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_influenced_mints_under_a_kernel_strength_floor() {
        // #10 foundation: an AgentInfluenced context carries the Kernel floor,
        // which is what makes `spawn` fail closed when the fence can't be
        // kernel-enforced. TrustedInfra carries the default (advisory) floor.
        let caveats = workspace_confined_caveats(Path::new("/ws"));
        let agent_cx = mint_context(ExecOrigin::AgentInfluenced, &caveats).unwrap();
        assert_eq!(agent_cx.strength_floor(), AxisEnforcement::Kernel);
        let trusted_cx = mint_context(ExecOrigin::TrustedInfra, &caveats).unwrap();
        assert_ne!(trusted_cx.strength_floor(), AxisEnforcement::Kernel);
    }

    #[test]
    fn request_env_is_exactly_the_grants_nothing_inherited() {
        // #8: the child's environment is the explicit grants only — there is no
        // path by which an ambient credential/switch reaches it.
        let req = ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            "sh",
            ["-c", "true"],
            "/ws",
            workspace_confined_caveats(Path::new("/ws")),
        )
        .env("HOME", "/ws")
        .envs([("LANG".into(), "C".into())]);
        assert_eq!(
            req.env_grants(),
            &[
                ("HOME".to_string(), "/ws".to_string()),
                ("LANG".to_string(), "C".to_string())
            ]
        );
    }

    #[test]
    fn build_tool_caveats_calibrated_reads_fenced_writes_no_net() {
        // A build/test tool reads the workspace + toolchain caches, but NOT
        // arbitrary secrets; it must not write outside the workspace fence, and
        // must not reach the network.
        use crate::caveats::ScopeExt;
        let cav = build_tool_caveats(Path::new("/ws"));
        // Reads: the workspace is granted; an arbitrary credential path is not.
        assert!(cav.fs_read.permits(&"/ws".to_string()));
        assert!(
            !cav.fs_read
                .permits(&"/home/someone-else/.ssh/id_rsa".to_string()),
            "a build tool must not be able to READ arbitrary secrets (disclosure via output)"
        );
        assert!(
            matches!(cav.fs_read, Scope::Only(_)),
            "reads are a calibrated set, never open"
        );
        // Writes: workspace only.
        assert!(cav.fs_write.permits(&"/ws".to_string()));
        assert!(
            !cav.fs_write.permits(&"/home/user/.cargo".to_string()),
            "no write to the shared package cache (poisoning) outside the fence"
        );
        assert!(!cav.fs_write.permits(&"/etc".to_string()));
        assert!(
            !cav.fs_write.permits(&"/tmp".to_string()),
            "no write to shared /tmp — scratch goes in-workspace via TMPDIR"
        );
        assert!(
            !cav.net.permits(&"evil.example".to_string()),
            "no network — the exfil channel is closed"
        );
    }

    #[test]
    fn build_tool_caveats_with_writes_grants_only_the_named_extra_root() {
        use crate::caveats::ScopeExt;
        let cav = build_tool_caveats_with_writes(Path::new("/ws"), &["/crew/target".to_string()]);
        assert!(cav.fs_write.permits(&"/ws".to_string()));
        assert!(
            cav.fs_write.permits(&"/crew/target".to_string()),
            "the explicitly-granted extra write root is permitted"
        );
        assert!(!cav.fs_write.permits(&"/crew/other".to_string()));
    }

    #[test]
    fn workspace_caveats_fence_reads_writes_to_workspace_and_denies_net() {
        // #9: net is an empty allow-list (deny-all). fs_read/fs_write name only
        // the workspace root; exec stays open (interpreter identity, not behavior).
        use crate::caveats::ScopeExt;
        let cav = workspace_confined_caveats(Path::new("/ws"));
        assert!(cav.fs_write.permits(&"/ws".to_string()));
        assert!(!cav.fs_write.permits(&"/etc".to_string()));
        assert!(cav.fs_read.permits(&"/ws".to_string()));
        assert!(!cav.fs_read.permits(&"/etc/passwd".to_string()));
        // Empty net allow-list permits nothing.
        assert!(!cav.net.permits(&"example.com".to_string()));
        assert!(matches!(cav.net, Scope::Only(ref s) if s.is_empty()));
    }

    #[test]
    fn exec_spawn_tool_declares_no_special_ceiling() {
        // The throwaway tool must not narrow authority: it declares `top()` so
        // the effective caveats are exactly the request's fence.
        let tool = ExecSpawnTool;
        assert_eq!(tool.name(), "confined_exec");
        assert_eq!(tool.required(), Caveats::top());
    }

    #[test]
    fn refusal_displays_are_honest_about_fail_closed() {
        let r = ExecRefused::ConfinementUnenforceable("no Landlock".into());
        assert!(r.to_string().contains("fail-closed"));
    }
}
