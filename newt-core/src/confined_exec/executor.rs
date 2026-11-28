//! The confined spawn: [`ConstrainedExecutor`], the gate that mints its context, the
//! fence, network-guard and cgroup probes, the network floor, and the cleanup of the
//! child's process tree. Outside comments and tests, only this file and its sibling
//! `sandbox_policy.rs`, the sandbox policy it installs, name agent-bridle in
//! `confined_exec`. The request and result types it reads are in the parent module, whose
//! doc states the fail-closed law and the per-axis exception this code enforces. Moved
//! from `confined_exec.rs` unchanged (#3160).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_bridle::{
    AxisEnforcement, ConfinedCommand, EnforcementFloor, Gate, Tool, ToolContext, ToolError,
};

use newt_policy::caveats::Caveats;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use newt_policy::caveats::Scope;

#[cfg(unix)]
use super::kill_process_group;
#[cfg(target_os = "macos")]
use super::runtime_sandbox_policy;
#[cfg(target_os = "windows")]
use super::WindowsJob;
use super::{ConfinedOutput, ExecOrigin, ExecRefused, ExecRequest, NetGrant};

impl ExecOrigin {
    /// The `Gate` an origin authorizes through — `AgentInfluenced` raises the
    /// enforcement floor to `Kernel` so an un-kernel-enforceable fence refuses;
    /// `BrokerMediated` raises it to the per-axis `CONFINED` floor instead,
    /// which keeps fs/net at `Kernel` but accepts `Interceptor` for exec.
    fn gate(self) -> Gate {
        match self {
            Self::AgentInfluenced => Gate::new(0).with_strength_floor(AxisEnforcement::Kernel),
            Self::TrustedInfra => Gate::new(0),
            Self::BrokerMediated => Gate::new(0).with_enforcement_floor(EnforcementFloor::CONFINED),
        }
    }
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
///
/// Linux uses Landlock; macOS uses Seatbelt (A3, operator-accepted 2026-09-30,
/// verified on a remote macOS test runner — see `docs/security/ocap-deviations.md`).
#[must_use]
pub fn kernel_fs_fence_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        agent_bridle::landlock_is_supported()
    }
    #[cfg(target_os = "macos")]
    {
        agent_bridle::seatbelt_is_supported()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
        Self::run_until_cancelled(req, None)
    }

    /// The same executor for model-visible async tools. Dropping this future
    /// signals the existing wait loop to kill/reap the confined process group;
    /// dropping a bare spawn_blocking handle alone would leave it running.
    pub async fn run_async(req: ExecRequest) -> Result<ConfinedOutput, ExecRefused> {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        struct CancelOnDrop(Arc<AtomicBool>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
        let flag = Arc::clone(&cancel.0);
        let result =
            tokio::task::spawn_blocking(move || Self::run_until_cancelled(&req, Some(&flag)))
                .await
                .map_err(|error| ExecRefused::Spawn(std::io::Error::other(error)))?;
        drop(cancel);
        result
    }

    fn run_until_cancelled(
        req: &ExecRequest,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<ConfinedOutput, ExecRefused> {
        if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
            return Err(ExecRefused::Spawn(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "confined execution cancelled before spawn",
            )));
        }
        req.require_toolchain_admission()?;
        let scratch = req.prepare_scratch().map_err(ExecRefused::Spawn)?;
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
            .private_build_scratch(req.private_build_scratch(&scratch))
            .sandbox_policy(std::sync::Arc::new(req.sandbox_policy()))
            .args(&args)
            .current_dir(&req.cwd)
            // A fresh process group so a supervisor could kill the whole
            // descendant tree (the executor's own cancellation is a follow-up;
            // the group is set up now so it is available without a re-spawn).
            .new_process_group()
            // `ConfinedStdio::Null`/`Piped` (not a bare `Stdio`) so this
            // `AgentInfluenced`/`Kernel`-floor spawn can honestly claim the
            // audited stdio shape ADR 0015 amendment E6 requires for the
            // Seatbelt net:none Kernel witness (agent-bridle#416 round-2
            // review, item 1) — this is the exact route
            // `seatbelt_net_deny_all_runs_kernel_denied` exercises.
            .stdin(agent_bridle::ConfinedStdio::Null)
            .stdout(agent_bridle::ConfinedStdio::Piped)
            .stderr(agent_bridle::ConfinedStdio::Piped)
            .held_read_roots(req.build_held_read_roots());
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

        let sandbox_kind = crate::sandbox_seam::SandboxKind::from(child.sandbox_kind);
        // The child leads its own process group (`new_process_group`, pgid ==
        // pid), so one `killpg` reaps the child AND any descendants it left in
        // the group — the child-lifetime containment the raw `Command`s lacked.
        let pgid = child.child.id();
        #[cfg(target_os = "windows")]
        let windows_job = WindowsJob::for_child(&child.child);

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
            let deadline_reached = deadline.is_some_and(|dl| Instant::now() >= dl);
            let cancelled =
                cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire));
            if deadline_reached || cancelled {
                #[cfg(target_os = "linux")]
                if let Some(cg) = &cgroup {
                    cg.kill();
                }
                #[cfg(target_os = "windows")]
                kill_child_tree(&mut child.child, pgid, windows_job.as_ref());
                #[cfg(not(target_os = "windows"))]
                kill_child_tree(&mut child.child, pgid);
                timed_out = deadline_reached;
                break child.child.wait().map_err(ExecRefused::Spawn)?;
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
        #[cfg(target_os = "windows")]
        kill_child_tree(&mut child.child, pgid, windows_job.as_ref());
        #[cfg(not(target_os = "windows"))]
        kill_child_tree(&mut child.child, pgid);

        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();

        Ok(ConfinedOutput {
            toolchains: req.toolchains.clone(),
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
        // that arm is `#[cfg]`'d out, so the parameter is unused there.
        // Consume it to keep `-D warnings` happy
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
                if req.sandbox_policy().child_network
                    == agent_bridle::ChildNetworkPolicy::PrivateBuild
                {
                    // The spawn funnel installs either private loopback or both
                    // deny-all mechanisms before this helper can run.
                    args.push("--inherit-build-network".to_string());
                }
                args.push("--".to_string());
                args.push(req.program.clone());
                args.extend(req.args.iter().cloned());
                Ok((guard, args, caveats))
            }
            #[cfg(target_os = "macos")]
            NetGrant::DenyAll => {
                // Use Bridle's witness for the same pipe/null stdio configured
                // below; it includes the deputy audit and caller privilege.
                // Admission checks the actual spawn again, so this preflight
                // cannot replace or weaken the kernel floor.
                let caveats = deny_all_net(&req.caveats);
                let mechanism = agent_bridle::ConfinementMechanism::new(
                    agent_bridle::SandboxKind::Seatbelt,
                    runtime_sandbox_policy().child_network,
                )
                .with_stdio_posture(agent_bridle::StdioPosture::Audited);
                seatbelt_deny_all_policy(
                    agent_bridle::seatbelt_is_supported(),
                    agent_bridle::enforcement_report(&caveats, mechanism).net,
                )?;
                Ok((req.program.clone(), req.args.clone(), caveats))
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            NetGrant::DenyAll => Err(ExecRefused::ConfinementUnenforceable(
                "NetGrant::DenyAll needs a kernel egress floor (Linux seccomp guard or \
                 macOS Seatbelt), unavailable on this platform"
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

/// Narrow a caveat set's `net` axis to deny-all (a pure attenuation — never
/// widens). This is the macOS [`NetGrant::DenyAll`] floor: an empty net scope
/// makes the Seatbelt profile `(deny network*)`.
#[cfg(any(target_os = "macos", test))]
fn deny_all_net(caveats: &Caveats) -> Caveats {
    let mut narrowed = caveats.clone();
    narrowed.net = Scope::only([] as [String; 0]);
    narrowed
}

/// Conditional macOS admission; the witness comes from Bridle's audited
/// pipe/null launch report, including its deputy audit and caller privilege.
/// Bridle checks the actual command again before spawning it.
#[cfg(any(target_os = "macos", test))]
fn seatbelt_deny_all_policy(
    backend_available: bool,
    net_witness: Option<AxisEnforcement>,
) -> Result<(), ExecRefused> {
    if backend_available && net_witness == Some(AxisEnforcement::Kernel) {
        Ok(())
    } else {
        Err(ExecRefused::ConfinementUnenforceable(
            "NetGrant::DenyAll requires Seatbelt and a kernel network witness for an audited, \
             unprivileged launch"
                .into(),
        ))
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

#[cfg(target_os = "windows")]
fn kill_child_tree(child: &mut std::process::Child, _pgid: u32, job: Option<&WindowsJob>) {
    if let Some(job) = job {
        job.terminate();
    }
    // If job assignment failed because the host runner already placed the child
    // in a job object, still kill the immediate launcher so the wait is bounded.
    let _ = child.kill();
}

#[cfg(not(target_os = "windows"))]
fn kill_child_tree(_child: &mut std::process::Child, pgid: u32) {
    #[cfg(unix)]
    kill_process_group(pgid);
    #[cfg(not(unix))]
    {
        let _ = pgid;
        let _ = _child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::confined_exec::build_tool_request;
    use crate::confined_exec::workspace_confined_caveats;

    #[test]
    fn already_cancelled_request_never_attempts_spawn() {
        let request = ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            "missing-program",
            ["unused"],
            "/missing",
            workspace_confined_caveats(Path::new("/missing")),
        );
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert!(
            matches!(ConstrainedExecutor::run_until_cancelled(&request, Some(&cancel)), Err(ExecRefused::Spawn(error)) if error.kind() == std::io::ErrorKind::Interrupted)
        );
    }

    /// End to end: the child's TMPDIR is the scratch dir, and it is gone once
    /// the lane returns, whether or not this host can enforce the fence.
    #[cfg(unix)]
    #[test]
    fn a_build_lane_run_leaves_no_scratch_behind() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let req = build_tool_request(
            &ws,
            &ws,
            "sh",
            ["-c", "printf %s \"$TMPDIR\"; touch \"$TMPDIR/x\""],
            &Scope::none(),
        );
        let scratch = req.scratch_dirs[0].clone();
        if let Ok(out) = ConstrainedExecutor::run(&req) {
            if out.success {
                assert_eq!(
                    String::from_utf8_lossy(&out.stdout),
                    scratch.to_string_lossy()
                );
            }
        }
        assert!(!scratch.exists());
    }

    #[test]
    fn agent_influenced_mints_under_a_kernel_strength_floor() {
        // #10 foundation: an AgentInfluenced context carries the Kernel floor,
        // which is what makes `spawn` fail closed when the fence can't be
        // kernel-enforced. TrustedInfra carries the default (advisory) floor.
        let caveats = workspace_confined_caveats(Path::new("/ws"));
        let agent_cx = mint_context(ExecOrigin::AgentInfluenced, &caveats).unwrap();
        assert_eq!(agent_cx.strength_floor().exec(), AxisEnforcement::Kernel);
        assert_eq!(agent_cx.strength_floor().net(), AxisEnforcement::Kernel);
        let trusted_cx = mint_context(ExecOrigin::TrustedInfra, &caveats).unwrap();
        assert_ne!(trusted_cx.strength_floor().exec(), AxisEnforcement::Kernel);
        assert_ne!(trusted_cx.strength_floor().net(), AxisEnforcement::Kernel);
    }

    /// #2693: `BrokerMediated` is the per-axis fix for `NativeGitBroker`'s own
    /// git re-dispatch — fs/net stay at the SAME `Kernel` floor `AgentInfluenced`
    /// requires (no widening there), but exec is accepted at `Interceptor`
    /// instead of demanding `Kernel` (which Landlock's restricted-exec axis can
    /// never report — see the module doc). Mutation: if this ever regressed to
    /// `AgentInfluenced`'s scalar floor, `exec()` here would equal `Kernel` and
    /// this assertion would fail.
    #[test]
    fn broker_mediated_mints_under_the_per_axis_confined_floor() {
        let caveats = workspace_confined_caveats(Path::new("/ws"));
        let agent_cx = mint_context(ExecOrigin::AgentInfluenced, &caveats).unwrap();
        let broker_cx = mint_context(ExecOrigin::BrokerMediated, &caveats).unwrap();
        assert_eq!(
            broker_cx.strength_floor().fs_read(),
            agent_cx.strength_floor().fs_read()
        );
        assert_eq!(
            broker_cx.strength_floor().fs_write(),
            agent_cx.strength_floor().fs_write()
        );
        assert_eq!(
            broker_cx.strength_floor().net(),
            agent_cx.strength_floor().net()
        );
        assert_eq!(
            broker_cx.strength_floor().fs_read(),
            AxisEnforcement::Kernel
        );
        assert_eq!(broker_cx.strength_floor().net(), AxisEnforcement::Kernel);
        assert_eq!(
            broker_cx.strength_floor().exec(),
            AxisEnforcement::Interceptor
        );
        assert_ne!(
            broker_cx.strength_floor().exec(),
            agent_cx.strength_floor().exec()
        );
    }

    #[test]
    fn deny_all_net_is_a_pure_attenuation_of_the_net_axis_only() {
        // The macOS DenyAll floor rests on this narrowing: the net axis becomes
        // deny-all (→ Seatbelt `(deny network*)`), and NOTHING else moves — a
        // widened fs/exec axis here would be authority the caller never asked
        // for. Grounded by `macos_seatbelt_adversarial::
        // seatbelt_net_deny_all_runs_kernel_denied` (the real Seatbelt run).
        let base = workspace_confined_caveats(Path::new("/ws"));
        let narrowed = deny_all_net(&base);
        assert_eq!(narrowed.net, Scope::only([] as [String; 0]));
        assert_eq!(narrowed.fs_read, base.fs_read);
        assert_eq!(narrowed.fs_write, base.fs_write);
        assert_eq!(narrowed.exec, base.exec);
        // Idempotent, and already-deny-all stays deny-all.
        assert_eq!(deny_all_net(&narrowed), narrowed);
    }

    /// The fetch copy must admit a witnessed launch and refuse every missing
    /// witness before spawn. Native fetch transfer and rewrite grounds exercise
    /// the admitted arm with real Seatbelt; this guard also covers unavailable
    /// backends and privileged/unaudited launches without running a process.
    #[test]
    fn seatbelt_deny_all_policy_requires_a_kernel_witness() {
        for backend in [false, true] {
            for witness in [
                None,
                Some(AxisEnforcement::Advisory),
                Some(AxisEnforcement::Interceptor),
                Some(AxisEnforcement::Kernel),
            ] {
                let result = seatbelt_deny_all_policy(backend, witness);
                if backend && witness == Some(AxisEnforcement::Kernel) {
                    assert!(result.is_ok(), "witnessed launch refused: {result:?}");
                } else {
                    assert!(
                        matches!(result, Err(ExecRefused::ConfinementUnenforceable(_))),
                        "unwitnessed launch admitted: backend={backend}, witness={witness:?}"
                    );
                }
            }
        }
    }

    /// Grounds `build_scratch_dir` against a real build-lane child: its
    /// `TMPDIR` is writable under the fence and lies outside the workspace.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_build_lane_child_gets_a_writable_scratch_dir_outside_the_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let script = "import os, tempfile\n\
            d = tempfile.mkdtemp(); open(os.path.join(d, 'x'), 'w').write('ok'); print(d)\n";
        // Exercise scratch filesystem confinement; macOS cannot enforce restricted net.
        let req = build_tool_request(&ws, &ws, "python3", ["-c", script], &Scope::All);
        let out = ConstrainedExecutor::run(&req).expect("the build lane spawns");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.success, "stdout: {stdout}\nstderr: {stderr}");
        assert!(
            !std::path::Path::new(stdout.trim()).starts_with(&ws),
            "{stdout}"
        );
    }

    #[test]
    fn exec_spawn_tool_declares_no_special_ceiling() {
        // The throwaway tool must not narrow authority: it declares `top()` so
        // the effective caveats are exactly the request's fence.
        let tool = ExecSpawnTool;
        assert_eq!(tool.name(), "confined_exec");
        assert_eq!(tool.required(), Caveats::top());
    }
}
