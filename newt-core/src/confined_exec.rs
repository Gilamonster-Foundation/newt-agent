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

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// The network floor a confined child runs under. The `Caveats.net` allow-list
/// is a hostname-level intent; this decides the *kernel* mechanism that backs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetGrant {
    /// The child's `Caveats.net` is honored by whatever the fs/net sandbox
    /// provides (Landlock TCP-deny, Seatbelt SBPL, …) with no extra floor. The
    /// historical default — kept for spawns that are not yet migrated to the
    /// full egress floor.
    #[default]
    Unrestricted,
    /// **Deny all egress**: the child is wrapped in `newt-net-guard`, which
    /// installs the seccomp `socket()`-family deny (TCP/UDP/DNS/raw) *in addition
    /// to* the inherited Landlock fs fence. On a platform without the seccomp
    /// floor the spawn is **refused** (`ExecRefused::ConfinementUnenforceable`),
    /// never run with a weaker net floor.
    DenyAll,
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
    /// The kernel network floor (see [`NetGrant`]).
    net_grant: NetGrant,
    /// Explicit path to the `newt-net-guard` wrapper (tests set this; production
    /// resolves it next to the running executable). Only consulted for
    /// [`NetGrant::DenyAll`].
    net_guard_bin: Option<PathBuf>,
    /// A wall-clock bound on the child. `None` = unbounded (the historical
    /// behavior, kept for long legitimate builds). When set, the executor
    /// SIGKILLs the child's whole process group at the deadline so a hostile
    /// child cannot hang the harness indefinitely.
    timeout: Option<Duration>,
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
            timeout: None,
            net_grant: NetGrant::Unrestricted,
            net_guard_bin: None,
        }
    }

    /// Set the kernel network floor. [`NetGrant::DenyAll`] wraps the child in
    /// `newt-net-guard` (seccomp egress deny); if that floor cannot be
    /// established the spawn is refused.
    #[must_use]
    pub fn net_grant(mut self, grant: NetGrant) -> Self {
        self.net_grant = grant;
        self
    }

    /// Override the `newt-net-guard` binary path (tests point this at the
    /// Cargo-built guard; production resolves it next to the executor binary).
    #[must_use]
    pub fn net_guard_bin(mut self, path: impl Into<PathBuf>) -> Self {
        self.net_guard_bin = Some(path.into());
        self
    }

    /// Bound the child to `duration` of wall-clock time. At the deadline the
    /// executor SIGKILLs the child's entire process group and returns with
    /// [`ConfinedOutput::timed_out`] set — a hostile child that hangs (or spins)
    /// cannot stall the harness. Unset by default (unbounded, for long builds).
    #[must_use]
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self
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
    /// True iff the child hit its [`ExecRequest::timeout`] and was killed. A
    /// timed-out run is never reported as `success`.
    pub timed_out: bool,
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
        // For a guarded (DenyAll) child, place it in a fresh cgroup-v2 subtree so
        // the whole descendant TREE — including a setsid / double-fork daemon that
        // escapes the process group — can be terminated with one `cgroup.kill`.
        // Best-effort: `None` where cgroup delegation is unavailable (killpg
        // fallback + the b1 residual).
        #[cfg(target_os = "linux")]
        let cgroup = if req.net_grant == NetGrant::DenyAll {
            CgroupHandle::create()
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let cgroup_procs = cgroup.as_ref().map(CgroupHandle::procs);
        #[cfg(not(target_os = "linux"))]
        let cgroup_procs: Option<PathBuf> = None;

        // Apply the network floor: NetGrant::DenyAll rewrites the child to run
        // under `newt-net-guard` (seccomp egress deny) with the guard's dir added
        // to the fs fence's read set so it can be exec'd. Refuses if the floor
        // cannot be established (never a weaker net floor).
        let (program, args, caveats) = Self::resolve_net_floor(req, cgroup_procs.as_deref())?;
        let cx = mint_context(req.origin, &caveats)?;

        let mut cmd = ConfinedCommand::new(&program)
            .args(&args)
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

        let mut child = cmd.spawn(&cx).map_err(|e| match e {
            // A `Denied` from spawn is the fail-closed refusal: the fence could
            // not be kernel-enforced (confinement_unenforceable) — do NOT fall
            // back to an unconfined run.
            ToolError::Denied { .. } => ExecRefused::ConfinementUnenforceable(e.to_string()),
            other => ExecRefused::Authorize(other.to_string()),
        })?;

        let sandbox_kind = child.sandbox_kind;
        // The child leads its own process group (`new_process_group`, pgid ==
        // pid), so one `killpg` reaps the child AND any descendants it left in
        // the group — the child-lifetime containment the raw `Command`s lacked.
        let pgid = child.child.id();

        // Drain stdout/stderr on their own threads so a child that fills a pipe
        // buffer cannot deadlock the wait (matches `wait_with_output`).
        let out_pipe = child.child.stdout.take();
        let err_pipe = child.child.stderr.take();
        let out_thread = std::thread::spawn(move || drain_pipe(out_pipe));
        let err_thread = std::thread::spawn(move || drain_pipe(err_pipe));

        // Bounded wait: poll for exit until the optional deadline. At the
        // deadline (leader still alive → pgid valid, no pid-reuse race) SIGKILL
        // the whole group so a hostile child cannot hang the harness.
        let deadline = req.timeout.map(|t| Instant::now() + t);
        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.child.try_wait().map_err(ExecRefused::Spawn)? {
                break s;
            }
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    #[cfg(target_os = "linux")]
                    if let Some(cg) = &cgroup {
                        cg.kill();
                    }
                    kill_process_group(pgid);
                    timed_out = true;
                    break child.child.wait().map_err(ExecRefused::Spawn)?;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        // Sweep the whole descendant tree. `cgroup.kill` catches a setsid /
        // double-fork daemon that escaped the process group; `killpg` is the
        // fallback where cgroup delegation was unavailable.
        #[cfg(target_os = "linux")]
        if let Some(cg) = &cgroup {
            cg.kill();
        }
        kill_process_group(pgid);

        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();

        Ok(ConfinedOutput {
            // A killed/timed-out run is never a success even if the reaped status
            // is a signal-death the OS reports oddly.
            success: status.success() && !timed_out,
            code: status.code(),
            stdout,
            stderr,
            sandbox_kind,
            timed_out,
        })
    }

    /// Compute the effective `(program, args, caveats)` after applying the
    /// request's [`NetGrant`]. [`NetGrant::DenyAll`] wraps the real program in
    /// `newt-net-guard` and grants the fs fence read+exec on the guard's dir; if
    /// the seccomp floor cannot be established it refuses (fail-closed). When
    /// `cgroup_procs` is `Some`, the guard is told to join that cgroup (its
    /// `cgroup.procs` is added to the write fence) before it execs.
    fn resolve_net_floor(
        req: &ExecRequest,
        cgroup_procs: Option<&Path>,
    ) -> Result<(String, Vec<String>, Caveats), ExecRefused> {
        // Only the Linux `DenyAll` arm reads `cgroup_procs`; on other platforms
        // that arm is `#[cfg]`'d out and `DenyAll` fails closed, so the parameter
        // is genuinely unused there. Consume it to keep `-D warnings` happy
        // without weakening the Linux signature both callers share.
        #[cfg(not(target_os = "linux"))]
        let _ = cgroup_procs;
        match req.net_grant {
            NetGrant::Unrestricted => {
                Ok((req.program.clone(), req.args.clone(), req.caveats.clone()))
            }
            #[cfg(target_os = "linux")]
            NetGrant::DenyAll => {
                // `(guard_program, prefix_args)`: a standalone `newt-net-guard`
                // (tests) has no prefix; the production self-exec is
                // `current_exe __net-guard`, so the guard rides in a released
                // `newt`. Either way the child inherits the fs fence + gets the
                // seccomp egress floor before the real program runs.
                let (guard, prefix) = Self::resolve_net_guard(req)?;
                let mut caveats = req.caveats.clone();
                if let Some(dir) = Path::new(&guard).parent() {
                    extend_read_root(&mut caveats, dir.to_string_lossy().into_owned());
                }
                let mut args: Vec<String> = prefix;
                if let Some(procs) = cgroup_procs {
                    // Grant write to the single cgroup.procs file so the guard can
                    // join the subtree under Landlock (it cannot escape to another
                    // cgroup — only its own procs file is writable).
                    let procs = procs.to_string_lossy().into_owned();
                    extend_write_root(&mut caveats, procs.clone());
                    args.push("--cgroup-procs".to_string());
                    args.push(procs);
                }
                args.push("--".to_string());
                args.push(req.program.clone());
                args.extend(req.args.iter().cloned());
                Ok((guard, args, caveats))
            }
            #[cfg(not(target_os = "linux"))]
            NetGrant::DenyAll => Err(ExecRefused::ConfinementUnenforceable(
                "NetGrant::DenyAll needs the seccomp egress floor, unavailable on this platform"
                    .into(),
            )),
        }
    }

    /// Resolve how to invoke the child-side network guard as `(program,
    /// prefix_args)`, in three tiers:
    ///
    /// 1. an explicit `net_guard_bin` override (the crate's real-resource tests
    ///    point it at the Cargo-built `newt-net-guard`) — standalone, no prefix;
    /// 2. a co-located `newt-net-guard` helper next to the running exe or one dir
    ///    up (the cargo dev/test layout builds it there) — standalone, no prefix.
    ///    Preferred in tests/dev so the guard is always a *real, correct* binary,
    ///    never a re-exec of a non-`newt` harness;
    /// 3. **self-exec** `current_exe __net-guard …` — production, where only
    ///    `newt` ships and carries the guard, so nothing separate can fall out of
    ///    the release archive / `cargo install` / package payload.
    ///
    /// Tier 3 self-exec fires **only when the running exe is `newt`**: re-exec'ing
    /// an arbitrary binary that lacks the `__net-guard` dispatch (a test harness,
    /// say) would run neither the guard nor the intended child — so when the exe
    /// is not `newt` and no helper was found, refuse (fail-closed) rather than
    /// re-exec something that cannot enforce the floor.
    #[cfg(target_os = "linux")]
    fn resolve_net_guard(req: &ExecRequest) -> Result<(String, Vec<String>), ExecRefused> {
        // Tier 1: explicit override.
        if let Some(p) = &req.net_guard_bin {
            return if p.is_file() {
                Ok((p.to_string_lossy().into_owned(), Vec::new()))
            } else {
                Err(ExecRefused::ConfinementUnenforceable(format!(
                    "net-guard binary not found at {}",
                    p.display()
                )))
            };
        }
        let exe = std::env::current_exe().map_err(|e| {
            ExecRefused::ConfinementUnenforceable(format!(
                "cannot resolve current executable to locate the net guard: {e}"
            ))
        })?;
        // Tier 2: a sibling `newt-net-guard` (exe dir, or one dir up for the
        // cargo `deps/` test layout).
        for cand in [
            exe.parent().map(|d| d.join("newt-net-guard")),
            exe.parent()
                .and_then(Path::parent)
                .map(|d| d.join("newt-net-guard")),
        ]
        .into_iter()
        .flatten()
        {
            if cand.is_file() {
                return Ok((cand.to_string_lossy().into_owned(), Vec::new()));
            }
        }
        // Tier 3: self-exec — only if the running exe actually IS `newt` (the
        // binary that carries the hidden `__net-guard` dispatch).
        if exe.file_name().and_then(|n| n.to_str()) == Some("newt") {
            return Ok((
                exe.to_string_lossy().into_owned(),
                vec!["__net-guard".to_string()],
            ));
        }
        Err(ExecRefused::ConfinementUnenforceable(format!(
            "no net guard: no `newt-net-guard` next to {} and it is not the `newt` \
             binary, so the `__net-guard` self-exec dispatch is unavailable",
            exe.display()
        )))
    }
}

/// Whether the [`NetGrant::DenyAll`] network guard can be resolved in the current
/// process WITHOUT an explicit override — mirroring tiers 2–3 of
/// [`ConstrainedExecutor::resolve_net_guard`]: a co-located `newt-net-guard`
/// helper (the cargo dev/test layout), or the running exe being `newt` (which
/// carries the `__net-guard` self-exec dispatch).
///
/// Tests gate their "the confined verify actually runs" assertions on this:
/// where the guard cannot be established the `DenyAll` spawn correctly FAILS
/// CLOSED (it never runs the repo-controlled command unconfined), so a test that
/// needs the verify to run must skip rather than assert a pass. A `-p <crate>`
/// build that does not compile the `newt-net-guard` bin and runs from a test
/// harness (not `newt`) is exactly that case. Always `false` off Linux (DenyAll
/// is unsupported there and fails closed).
#[must_use]
pub fn net_guard_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        for cand in [
            exe.parent().map(|d| d.join("newt-net-guard")),
            exe.parent()
                .and_then(Path::parent)
                .map(|d| d.join("newt-net-guard")),
        ]
        .into_iter()
        .flatten()
        {
            if cand.is_file() {
                return true;
            }
        }
        exe.file_name().and_then(|n| n.to_str()) == Some("newt")
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Add one read root to a caveats fs-read scope (the `lock_fs_to_workspace`
/// pattern); `All` stays `All`.
#[cfg(target_os = "linux")]
fn extend_read_root(caveats: &mut Caveats, dir: String) {
    caveats.fs_read = match &caveats.fs_read {
        Scope::All => Scope::All,
        Scope::Only(set) => Scope::only(set.iter().cloned().chain(std::iter::once(dir))),
    };
}

/// Add one write path to a caveats fs-write scope; `All` stays `All`.
#[cfg(target_os = "linux")]
fn extend_write_root(caveats: &mut Caveats, path: String) {
    caveats.fs_write = match &caveats.fs_write {
        Scope::All => Scope::All,
        Scope::Only(set) => Scope::only(set.iter().cloned().chain(std::iter::once(path))),
    };
}

/// A child-lifetime cgroup-v2 subtree: the confined child (and EVERY descendant,
/// including a `setsid` / double-fork daemon that escapes the process group) is
/// placed in it, so one write to `cgroup.kill` terminates the whole tree —
/// containment `killpg` alone cannot provide.
///
/// Best-effort and unprivileged: [`create`](Self::create) makes a fresh directory
/// under this process's OWN delegated cgroup and returns `None` where cgroup-v2
/// delegation is unavailable (the executor then keeps the killpg fallback and the
/// full-tree containment stays a `b1` residual). Dropping it kills + removes the
/// subtree.
#[cfg(target_os = "linux")]
pub struct CgroupHandle {
    dir: PathBuf,
}

/// Whether a delegated cgroup-v2 subtree with `cgroup.kill` can be created on
/// this host (the mechanism that contains a `setsid` / double-fork escape).
/// `false` where cgroup-v2 delegation is unavailable — e.g. an unprivileged
/// container/pod without a delegated subtree — in which case the executor falls
/// back to `killpg` and full-tree containment stays a `b1` residual. Real-resource
/// tests use this to skip the stronger assertion where the primitive is absent.
#[must_use]
pub fn cgroup_subtree_kill_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        CgroupHandle::create().is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
impl CgroupHandle {
    /// Create a fresh sub-cgroup under this process's delegated cgroup, or `None`
    /// if cgroup-v2 delegation (`cgroup.kill`) is unavailable here.
    fn create() -> Option<Self> {
        let self_cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        // The unified (v2) line is `0::<relative-path>`.
        let rel = self_cg
            .lines()
            .find_map(|l| l.strip_prefix("0::"))?
            .trim()
            .trim_start_matches('/');
        let pid = std::process::id();
        // A per-process counter disambiguates concurrent runs (no rng available).
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = PathBuf::from(format!("/sys/fs/cgroup/{rel}/newt-exec-{pid}-{n}"));
        std::fs::create_dir(&dir).ok()?;
        if dir.join("cgroup.kill").exists() {
            Some(Self { dir })
        } else {
            let _ = std::fs::remove_dir(&dir);
            None
        }
    }

    /// The `cgroup.procs` file a joining child writes its pid into.
    fn procs(&self) -> PathBuf {
        self.dir.join("cgroup.procs")
    }

    /// Kill every process in the subtree (setsid escapes included). Best-effort.
    fn kill(&self) {
        let _ = std::fs::write(self.dir.join("cgroup.kill"), "1");
    }
}

#[cfg(target_os = "linux")]
impl Drop for CgroupHandle {
    fn drop(&mut self) {
        self.kill();
        // rmdir needs the cgroup empty; the kill above drains it, but reaping is
        // asynchronous, so retry briefly.
        for _ in 0..50 {
            if std::fs::remove_dir(&self.dir).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Read a captured pipe to EOF (best-effort). Runs on its own thread so the
/// wait loop cannot deadlock behind a full pipe buffer.
fn drain_pipe(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf);
    }
    buf
}

/// SIGKILL an entire process group (`pgid`). Best-effort: `ESRCH` (the group is
/// already gone) is fine. On non-unix this is a no-op — the confined executor is
/// the Linux-normative path; other platforms fail closed before reaching here.
fn kill_process_group(pgid: u32) {
    #[cfg(unix)]
    // SAFETY: `killpg` with a valid pgid and SIGKILL has no memory effects; a
    // stale pgid returns ESRCH which we ignore.
    unsafe {
        let _ = libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pgid;
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
    fn timeout_builder_sets_the_bound_default_is_unbounded() {
        // #1598: the bound is opt-in (unbounded by default so long legitimate
        // builds are not killed), and `.timeout()` records it — the field the
        // real-resource `confined_exec_lifetime` tests exercise for real.
        let base = ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            "sh",
            ["-c", "true"],
            "/ws",
            workspace_confined_caveats(Path::new("/ws")),
        );
        assert_eq!(base.timeout, None);
        let bounded = base.timeout(Duration::from_secs(5));
        assert_eq!(bounded.timeout, Some(Duration::from_secs(5)));
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
