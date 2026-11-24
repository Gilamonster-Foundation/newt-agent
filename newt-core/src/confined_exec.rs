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
//! - **#9** an empty `net` fence requires deny-all enforcement. A platform whose
//!   complete network authority is unknown refuses the spawn. Seatbelt admits
//!   deny-all only with its audited, unprivileged kernel network witness,
//! - **#10** where the required kernel enforcement is unavailable the spawn is
//!   **refused**, never silently downgraded to an unconfined run.
//!
//! Linux is the normative fully-supported platform; on a platform that cannot
//! kernel-confine, an `AgentInfluenced` spawn fails closed rather than pretend.
//! ([`ExecOrigin::TrustedInfra`] carries the default floor for the small set of
//! fixed-argv internal helpers that are *not* attacker-influenced; it is here
//! for signature completeness and is not used by the agent-exec paths.)
//!
//! # The per-axis exception (`ExecOrigin::BrokerMediated`, issue #2693)
//!
//! `AgentInfluenced`'s **scalar** `Kernel` floor demands `Kernel` on *every*
//! axis, including `exec`. Under Landlock a restricted `exec` axis can only
//! ever report `Interceptor` — the loader/interpreter trampoline is a
//! documented, permanent residual (agent-bridle `exec-behavior-bound` /
//! ADR 0011 D7), not a bug — so an `AgentInfluenced` spawn with a restricted
//! `exec` scope refuses unconditionally on Linux, for every caller, forever.
//! `NativeGitBroker` hit exactly this: its own
//! host-side re-dispatch of `git` (`exec: Scope::only(["git"])`) always
//! refused, so a governed commit could never land in a default-permission
//! session (#2693).
//!
//! [`ExecOrigin::BrokerMediated`] is the honest per-axis floor for that case:
//! `agent_bridle::EnforcementFloor::CONFINED` (`fs_read`/`fs_write`/`net` =
//! `Kernel`, `exec` = `Interceptor`). It is **not** a widening of
//! `AgentInfluenced` — the filesystem and network axes still fail closed
//! exactly as before. It accepts the Interceptor-level exec residual ONLY
//! where a host-side broker (not the model) already decides, by its own
//! policy, which fixed program and argv may run — `NativeGitBroker`'s
//! `RepositoryProbe` re-dispatch, never a model-chosen command. Recorded as a
//! named deviation: `docs/security/ocap-deviations.md` §
//! `native-git-broker-exec-floor`.
//!
//! # Without the `confine` feature (#3160)
//!
//! A build of newt-core without its `confine` feature does not compile the executor. The
//! names that code outside this module imports stay, as the refusals in `executor_off`:
//! [`ConstrainedExecutor`] refuses every call with [`ExecRefused::ConfinementUnenforceable`]
//! before a scratch directory or a process exists, and the three fence probes report no
//! fence. [`BACKEND_COMPILED`] says which build this is.

use std::path::{Path, PathBuf};
use std::time::Duration;

use newt_policy::caveats::{Caveats, Scope};

#[cfg(feature = "confine")]
mod executor;
#[cfg(not(feature = "confine"))]
mod executor_off;
#[cfg(feature = "confine")]
mod sandbox_policy;

#[cfg(feature = "confine")]
pub use executor::{
    cgroup_subtree_kill_available, kernel_fs_fence_available, net_guard_available,
    ConstrainedExecutor,
};
#[cfg(not(feature = "confine"))]
pub use executor_off::{
    cgroup_subtree_kill_available, kernel_fs_fence_available, net_guard_available,
    ConstrainedExecutor,
};
#[cfg(feature = "confine")]
pub(crate) use sandbox_policy::runtime_sandbox_policy;

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
    /// A fixed-program, policy-mediated re-dispatch issued by a HOST-SIDE
    /// broker that already bounds which program/argv may run — not a raw
    /// model `run_command` (see the module doc's "per-axis exception").
    /// Minted under `agent_bridle::EnforcementFloor::CONFINED`: `fs`/`net`
    /// still require `Kernel` (fail-closed, #10 unchanged), `exec` accepts
    /// the `Interceptor` tier Landlock actually provides. Currently used only
    /// by `crate::native_git_broker::NativeGitBroker`'s own `git`
    /// re-dispatch.
    BrokerMediated,
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
    /// **Deny all egress**, by the strongest kernel mechanism the platform has:
    ///
    /// * **Linux** — the child is wrapped in `newt-net-guard`, which installs
    ///   the seccomp `socket()`-family deny (TCP/UDP/DNS/raw) *in addition to*
    ///   the inherited Landlock fs fence.
    /// * **macOS** — the child's `net` caveat is narrowed to deny-all. Seatbelt
    ///   must supply Bridle's kernel network witness for the audited pipe/null,
    ///   unprivileged launch shape. Missing backend or witness refuses before
    ///   spawn; backend availability alone never establishes the floor.
    ///
    /// On a platform with neither floor the spawn is **refused**
    /// (`ExecRefused::ConfinementUnenforceable`), never run with a weaker net
    /// floor — and macOS refuses the same way if the Seatbelt backend
    /// (`sandbox-exec`) is unavailable.
    DenyAll,
}

/// A fully-specified request to run one confined subprocess. Every field is
/// explicit: there is no ambient default that could widen the child's
/// authority. The child starts env-EMPTY — [`env`](Self::env) grants are the
/// child's *entire* environment.
#[derive(Debug, Clone)]
// Without `confine` no executor reads `origin`, `program`, `args`, `cwd`, `net_grant`,
// `net_guard_bin` or `timeout`. The unit tests read `origin`, `args`, `net_grant` and
// `timeout`, so `program`, `cwd` and `net_guard_bin` are unread in a test build too: the
// expectation holds in every build without `confine`.
#[cfg_attr(
    not(feature = "confine"),
    expect(dead_code, reason = "only the executor reads the spawn description")
)]
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
    /// Per-run scratch directories (a build's `TMPDIR`): created fresh and
    /// owner-only just before spawning, removed when the run ends, so building
    /// a request never touches the filesystem.
    scratch_dirs: Vec<PathBuf>,
    /// Read roots the caller holds as directory descriptors: the Landlock
    /// fence anchors on them instead of re-opening the root by path.
    held_read_roots: Vec<crate::sandbox_seam::HeldReadRoot>,
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
            scratch_dirs: Vec::new(),
            held_read_roots: Vec::new(),
        }
    }

    /// Anchor the fence's read rules for these roots on the descriptors the
    /// caller holds ([`crate::sandbox_seam::HeldReadRoot`]), so a root swapped at
    /// its pathname after the caller's check is outside the fence. Each root
    /// must also be in `caveats.fs_read`; this changes how the fence is
    /// built, never what is admitted.
    #[must_use]
    pub fn held_read_roots(
        mut self,
        roots: impl IntoIterator<Item = crate::sandbox_seam::HeldReadRoot>,
    ) -> Self {
        self.held_read_roots.extend(roots);
        self
    }

    /// Add validated build metadata reads together with their held objects.
    /// Plain caveats alone cannot carry descriptor-bound authority.
    #[must_use]
    pub fn with_build_git_reads(mut self, workspace: &Path) -> Self {
        let (paths, held) = crate::git_hardening::build_reads::build_git_reads(workspace);
        if let Scope::Only(reads) = &mut self.caveats.fs_read {
            reads.extend(paths);
            self.held_read_roots.extend(held);
        }
        self
    }

    #[cfg(feature = "confine")]
    pub(crate) fn build_held_read_roots(&self) -> Vec<crate::sandbox_seam::HeldReadRoot> {
        self.held_read_roots.clone()
    }

    /// Give the run a private scratch `dir`: created just before spawning
    /// (owner-only, refused if anything already exists there, symlink
    /// included) and removed when the run ends. See `ScratchDir`.
    #[must_use]
    pub fn scratch_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.scratch_dirs.push(dir.into());
        self
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

    /// The exact prepared fence, for admission before this request is spawned.
    #[must_use]
    pub fn caveats(&self) -> &Caveats {
        &self.caveats
    }

    /// Acquire this request's private scratch only after admission. The guards
    /// keep the same cleanup lifetime for native argv and confined shell runs.
    #[cfg(feature = "confine")]
    pub(crate) fn prepare_scratch(&self) -> std::io::Result<Vec<ScratchDir>> {
        self.scratch_dirs
            .iter()
            .map(|dir| {
                crate::scratch::validate_build_grant(dir, &self.caveats.fs_write)?;
                ScratchDir::create(dir)
            })
            .collect()
    }
}

/// Why a confined spawn did not run. Every variant is a refusal to weaken the
/// claim: the executor never falls back to an unconfined run.
#[derive(Debug)]
pub enum ExecRefused {
    /// The fence could not be kernel-enforced on this platform/kernel, so the
    /// spawn was refused rather than run unconfined (#10). Carries the honest
    /// reason from `agent-bridle`; a build without the `confine` feature carries
    /// `CONFINE_OFF_REASON` instead.
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

/// Whether this build compiled the confinement backend: newt-core's `confine` feature is on,
/// as it is by default. Without it [`ConstrainedExecutor`] refuses every call (#3160).
pub const BACKEND_COMPILED: bool = cfg!(feature = "confine");

/// The reason a build without the backend gives: in the refusal [`ConstrainedExecutor`]
/// returns and in the OCAP verdict on the executor.
pub(crate) const CONFINE_OFF_REASON: &str =
    "this build has no confinement backend (cargo feature `confine` is off)";

/// The refusal [`ConstrainedExecutor`] returns without the backend. The one place it is built.
pub(crate) fn off_refusal() -> ExecRefused {
    ExecRefused::ConfinementUnenforceable(CONFINE_OFF_REASON.to_owned())
}

/// Refuse before a caller requests authority or prepares files for an unavailable backend.
pub(crate) fn require_backend() -> Result<(), ExecRefused> {
    if BACKEND_COMPILED {
        Ok(())
    } else {
        Err(off_refusal())
    }
}

/// The result of a completed confined run: the child's exit outcome, its
/// captured output, and the OS sandbox that was ACTUALLY applied (honest — never
/// over-claimed; [`crate::sandbox_seam::SandboxKind::None`] means advisory only).
#[derive(Debug)]
pub struct ConfinedOutput {
    /// True iff the child exited with a success status.
    pub success: bool,
    /// The exit code, if the process exited with one.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The OS sandbox actually applied to the child.
    pub sandbox_kind: crate::sandbox_seam::SandboxKind,
    /// True iff the child hit its [`ExecRequest::timeout`] and was killed. A
    /// timed-out run is never reported as `success`.
    pub timed_out: bool,
}

#[cfg(target_os = "windows")]
pub(crate) struct WindowsJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(target_os = "windows")]
impl WindowsJob {
    #[cfg(feature = "confine")]
    fn for_child(child: &std::process::Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        Self::for_process(child.as_raw_handle())
    }

    pub(crate) fn for_process(process: std::os::windows::io::RawHandle) -> Option<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: all pointers are null or point to initialized Win32 structs for
        // the duration of the call. The returned handle is owned by `WindowsJob`.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` has the exact type requested by
        // `JobObjectExtendedLimitInformation`.
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } != 0;
        if !configured {
            // SAFETY: `handle` is a live handle returned by CreateJobObjectW.
            unsafe {
                CloseHandle(handle);
            }
            return None;
        }

        let process = process as HANDLE;
        // SAFETY: both handles are live. Failure is possible when the host has
        // already assigned the process to a non-breakaway job; in that case the
        // executor keeps the immediate-child kill fallback.
        let assigned = unsafe { AssignProcessToJobObject(handle, process) } != 0;
        if !assigned {
            // SAFETY: `handle` is a live handle returned by CreateJobObjectW.
            unsafe {
                CloseHandle(handle);
            }
            return None;
        }

        Some(Self { handle })
    }

    #[cfg(feature = "confine")]
    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: `handle` is owned by this RAII wrapper. Termination is
        // best-effort; an already-empty job simply reports failure, which is fine.
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // KILL_ON_JOB_CLOSE makes a dropped job object a final cleanup sweep.
        // SAFETY: `handle` is owned by this wrapper.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// SIGKILL an entire process group (`pgid`). Best-effort: `ESRCH` (the group is
/// already gone) is fine. On non-unix this is a no-op — the confined executor is
/// the Linux-normative path; other platforms fail closed before reaching here.
/// Public so the eval harness's unconfined runner reaps with the same call.
#[cfg(unix)]
pub fn kill_process_group(pgid: u32) {
    // SAFETY: `killpg` with a valid pgid and SIGKILL has no memory effects; a
    // stale pgid returns ESRCH which we ignore.
    unsafe {
        let _ = libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
}

/// The Xcode binaries a bare-name exec grant already reaches: for each granted
/// name whose `/usr/bin/<name>` is present, `<developer>/usr/bin/<name>`.
///
/// On macOS `/usr/bin/python3`, `/usr/bin/git`, `/usr/bin/clang` and the rest
/// are xcrun shims that only forward to that binary, and the shell lane puts
/// `<developer>/usr/bin` first on the child's PATH so the shim's per-user xcrun
/// cache is never needed. A bare grant resolves only against the system dirs,
/// though, so without the twin the kernel refused the very binary PATH chose
/// (`execvp() of 'python3' failed: Operation not permitted`). Admitting it is
/// the grant the operator already made, as agent-bridle admits `/bin/bash` for
/// a granted `/bin/sh`. Empty off macOS or with no selected developer dir.
#[must_use]
pub(crate) fn developer_exec_twins<'a>(exec: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    #[cfg(target_os = "macos")]
    if let Some(developer) = selected_developer_directory() {
        return exec
            .into_iter()
            .filter(|name| !name.contains('/') && Path::new("/usr/bin").join(name).is_file())
            .map(|name| Path::new(developer).join("usr/bin").join(name))
            .filter_map(|twin| twin.canonicalize().ok())
            .filter(|twin| twin.is_file())
            .flat_map(|twin| {
                // A framework build's `Versions/<v>/bin/<name>` is itself a
                // launcher that re-spawns a companion inside its bundle.
                let companions: Vec<PathBuf> = twin
                    .parent()
                    .and_then(Path::parent)
                    .map(|version| {
                        FRAMEWORK_LAUNCHER_COMPANIONS
                            .iter()
                            .map(|relative| version.join(relative))
                            .filter(|companion| companion.is_file())
                            .collect()
                    })
                    .unwrap_or_default();
                std::iter::once(twin).chain(companions)
            })
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
    }
    let _ = exec;
    Vec::new()
}

/// What a macOS framework launcher (`<Name>.framework/Versions/<v>/bin/<tool>`)
/// re-spawns, relative to its `Versions/<v>` directory. Python's framework build
/// runs its interpreter from an app bundle so it gets a bundle identity.
#[cfg(target_os = "macos")]
const FRAMEWORK_LAUNCHER_COMPANIONS: &[&str] = &["Resources/Python.app/Contents/MacOS/Python"];

/// The system's selected toolchain, resolved once without repository env/cwd.
#[cfg(target_os = "macos")]
pub(crate) fn selected_developer_directory() -> Option<&'static str> {
    static DEVELOPER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DEVELOPER
        .get_or_init(|| {
            let output = std::process::Command::new("/usr/bin/xcode-select")
                .arg("-p")
                .env_clear()
                .current_dir("/")
                .stdin(std::process::Stdio::null())
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let path = std::path::PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
            if !path.is_absolute() || !path.is_dir() {
                return None;
            }
            path.canonicalize().ok()?.to_str().map(str::to_owned)
        })
        .as_deref()
}

/// The selected toolchain's tool directory, where the real developer tools live
/// that Apple's locator stubs (`/usr/bin/git`, `python3`, ...) only forward to.
/// Derived from [`selected_developer_directory`]: the host's own `xcode-select`
/// answer, never a fixed path and never read from the confined child (#3156).
#[cfg(target_os = "macos")]
pub(crate) fn developer_tool_directory() -> Option<PathBuf> {
    selected_developer_directory().map(|developer| Path::new(developer).join("usr/bin"))
}

/// A `Caveats` fence for an attacker-influenced subprocess confined to
/// `workspace`: it may read and write **only** beneath the workspace root, run
/// any program (an interpreter's *identity* is admitted; its behavior is bounded
/// by the fs/net axes — see `agent_bridle::AxisEnforcement::Kernel`'s doc), and reach **no**
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
        max_calls: newt_policy::caveats::CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// Prepare a build under the existing calibrated fence. Resolve operator
/// tool homes before replacing HOME; no shell credentials or global target /
/// compiler-cache authority are inherited. Cargo build scripts and tests inherit
/// the same kernel fence as the compiler.
/// `network` is the caller's existing operator-granted scope, never authority
/// acquired by approving a build. Host lists narrow to deny-all for the child,
/// just as in ordinary dispatch; the caller's session authority is unchanged.
#[must_use]
pub fn build_tool_request(
    workspace: &Path,
    cwd: &Path,
    program: impl Into<String>,
    args: impl IntoIterator<Item = impl Into<String>>,
    network: &Scope<String>,
) -> ExecRequest {
    let mut caveats = build_tool_caveats(workspace);
    caveats.net = newt_policy::caveats::spawn_net_scope(network);
    let net_grant = if caveats.net == Scope::none() {
        NetGrant::DenyAll
    } else {
        NetGrant::Unrestricted
    };
    toolchain_request(
        ExecOrigin::AgentInfluenced,
        workspace,
        cwd,
        program,
        args,
        caveats,
    )
    .net_grant(net_grant)
    .env("CARGO_NET_OFFLINE", "true")
}

/// A build's unique temporary directory under a private managed base outside
/// Git worktrees, or the explicit operator override. The Build gate admits its
/// workspace-specific root before the executor creates anything. The existing
/// execution lease retains scratch through completion and cancellation.
#[must_use]
pub fn build_scratch_dir(workspace: &Path) -> PathBuf {
    scratch_dir_under(workspace, Some(crate::scratch::build_scratch_base()))
}

/// The stable per-workspace scratch root used by Build requests and their run
/// directories. This only plans a path; allocation remains after admission.
/// An explicit operator Build scratch override is reflected in this path and
/// still needs coverage by the caller's authority.
#[must_use]
pub fn build_scratch_root(workspace: &Path) -> PathBuf {
    operator_build_scratch_root(workspace, Some(crate::scratch::build_scratch_base()))
        .expect("the Build scratch base is always configured or managed")
}

fn scratch_dir_under(workspace: &Path, base: Option<PathBuf>) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    operator_build_scratch_root(workspace, base)
        .unwrap_or_else(|| workspace.join("target").join("tmp"))
        .join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
}

/// Stable per-workspace root, so the fence can grant it before any run exists.
/// Managed defaults use a compact locator; explicit bases retain the full CID.
fn operator_build_scratch_root(workspace: &Path, base: Option<PathBuf>) -> Option<PathBuf> {
    base.map(|base| {
        let identity =
            content_addressable::RawContentId::from_content(workspace.to_string_lossy().as_bytes());
        crate::scratch::managed_build_partition(&base, &identity)
            .unwrap_or_else(|| base.join(identity.to_string()))
    })
}

/// A run's private scratch directory. [`ScratchDir::create`] makes it owner-only
/// and refuses if the path already exists (so a pre-planted directory or
/// symlink is never followed or reused); `Drop` removes exactly that directory,
/// then the `tmp` and `target` parents only if they are left empty.
#[cfg(any(test, feature = "confine"))]
pub(crate) struct ScratchDir {
    path: PathBuf,
    /// `target/` did not exist before this run created it.
    created_target: bool,
    managed: Option<crate::scratch::ManagedRun>,
}

#[cfg(any(test, feature = "confine"))]
impl ScratchDir {
    fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(managed) = crate::scratch::acquire_build_run(path)? {
            return Ok(Self {
                path: path.to_owned(),
                created_target: false,
                managed: Some(managed),
            });
        }
        let tmp = path.parent().unwrap_or(path);
        let target = tmp.parent().unwrap_or(tmp);
        let created_target = !target.exists();
        // `create_dir`, not `create_dir_all`: the workspace (or the operator's
        // base) must already exist. A missing one is a refusal, never created.
        match std::fs::create_dir(target) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        Self::private_dir(tmp, false)?;
        Self::private_dir(path, true)?;
        Ok(Self {
            path: path.to_path_buf(),
            created_target,
            managed: None,
        })
    }

    /// Make one directory, owner-only on unix. `exclusive`: fail if it exists.
    fn private_dir(dir: &Path, exclusive: bool) -> std::io::Result<()> {
        #[cfg(unix)]
        let builder = {
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        match builder.create(dir) {
            Err(error) if !exclusive && error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }
}

#[cfg(any(test, feature = "confine"))]
impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.managed.is_some() {
            return;
        }
        // Never follow a link: only a real directory is removed.
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.is_dir()) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
        if let Some(tmp) = self.path.parent() {
            let _ = std::fs::remove_dir(tmp);
            if self.created_target {
                if let Some(target) = tmp.parent() {
                    let _ = std::fs::remove_dir(target);
                }
            }
        }
    }
}

/// The crates.io hosts `cargo fetch` reaches: the sparse index and the
/// download CDN. These are the destinations the dependency-fetch gate asks for.
pub const CRATES_IO_FETCH_HOSTS: &[&str] = &["index.crates.io", "static.crates.io"];

/// Fill the operator's Cargo package cache with the crates `Cargo.lock` pins, so
/// the offline build lane ([`build_tool_request`]) can resolve them.
///
/// `cargo fetch --locked` compiles nothing and runs no build script, and
/// `--locked` binds every download to the lockfile's checksums. The argv is
/// fixed here, never model-supplied, so it runs as [`ExecOrigin::TrustedInfra`]:
/// host restrictions remain subject to Bridle's platform admission checks.
/// The fetch path is registered as `dependency-fetch-egress` in
/// `docs/security/ocap-deviations.md`; callers must hold the operator's `net`
/// grant for [`CRATES_IO_FETCH_HOSTS`] before running this. `network` carries
/// that authority (including an explicit unrestricted grant); this builder
/// never grants a host the caller did not provide.
#[must_use]
pub fn dependency_fetch_request(
    workspace: &Path,
    cwd: &Path,
    network: &Scope<String>,
) -> ExecRequest {
    let writes: Vec<String> = operator_tool_home("CARGO_HOME", ".cargo")
        .map(|home| cargo_home_fetch_write_roots(&home))
        .unwrap_or_default();
    let mut caveats = build_tool_caveats_with_writes(workspace, &writes);
    caveats.net = network.clone();
    toolchain_request(
        ExecOrigin::TrustedInfra,
        workspace,
        cwd,
        "cargo",
        ["fetch", "--locked"],
        caveats,
    )
}

/// What a fetch writes under `$CARGO_HOME`: the registry (index cache, `.crate`
/// files, extracted sources) and cargo's package-cache lock files. Not the
/// whole directory, which holds `credentials.toml`.
fn cargo_home_fetch_write_roots(cargo_home: &str) -> Vec<String> {
    let base = Path::new(cargo_home);
    [
        "registry",
        ".package-cache",
        ".package-cache-mutate",
        ".global-cache",
        ".global-cache-journal",
        ".global-cache-wal",
        ".global-cache-shm",
    ]
    .into_iter()
    .map(|entry| base.join(entry).to_string_lossy().into_owned())
    .collect()
}

/// The environment every toolchain child shares: an env-empty start, a private
/// scratch HOME, a workspace target, operator tool homes, and a PATH that finds
/// the real compiler. HOME and temporary files share the admitted run lease;
/// developer-tool caches and Linux dot directories cannot dirty the checkout.
/// Network policy is the caller's.
fn toolchain_request(
    origin: ExecOrigin,
    workspace: &Path,
    cwd: &Path,
    program: impl Into<String>,
    args: impl IntoIterator<Item = impl Into<String>>,
    caveats: Caveats,
) -> ExecRequest {
    let scratch = build_scratch_dir(workspace);
    let mut request = ExecRequest::new(origin, program, args, cwd, caveats)
        .env("HOME", scratch.to_string_lossy())
        .env("TMPDIR", scratch.to_string_lossy())
        .env("TMP", scratch.to_string_lossy())
        .env("TEMP", scratch.to_string_lossy())
        .scratch_dir(scratch)
        .env(
            "CARGO_TARGET_DIR",
            workspace.join("target").to_string_lossy(),
        )
        // Operator Cargo config may name an out-of-fence compiler cache daemon.
        // A confined build uses the compiler directly, not that ambient deputy.
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "");
    for (name, default) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
        if let Some(path) = operator_tool_home(name, default) {
            request = request.env(name, path);
        }
    }
    let mut paths = Vec::new();
    #[cfg(target_os = "macos")]
    if let Some(developer) = selected_developer_directory() {
        // Xcode's Developer/usr/bin/cc is another xcrun shim. Prefer its
        // actual compiler, which needs no per-user xcrun cache/frameworks.
        paths.push(PathBuf::from(developer).join("Toolchains/XcodeDefault.xctoolchain/usr/bin"));
        paths.push(PathBuf::from(developer).join("usr/bin"));
        if let Some(sdk) = selected_macos_sdk() {
            request = request.env("SDKROOT", sdk);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(path) = std::env::join_paths(paths) {
        request = request.env("PATH", path.to_string_lossy());
    }
    request.with_build_git_reads(workspace)
}

fn operator_tool_home(name: &str, default: &str) -> Option<String> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(default)))
        .filter(|path| path.is_absolute())
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn selected_macos_sdk() -> Option<String> {
    let developer = Path::new(selected_developer_directory()?);
    [
        "Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk",
        "SDKs/MacOSX.sdk",
    ]
    .into_iter()
    .filter_map(|relative| developer.join(relative).canonicalize().ok())
    .find(|path| path.is_dir() && path.starts_with(developer))
    .map(|path| path.to_string_lossy().into_owned())
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
/// Writes cover the workspace and its private build scratch partition, never
/// the shared cache or shared temporary parent. The baseline denies networking;
/// build requests narrow the caller's network scope using ordinary spawn policy.
/// Descendants inherit the same filesystem fence.
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
    build_caveats_with_scratch(
        workspace,
        extra_write_roots,
        Some(crate::scratch::build_scratch_base()),
    )
}

fn build_caveats_with_scratch(
    workspace: &Path,
    extra_write_roots: &[String],
    scratch_base: Option<PathBuf>,
) -> Caveats {
    let scratch = operator_build_scratch_root(workspace, scratch_base)
        .map(|root| root.to_string_lossy().into_owned());
    let mut write_roots = vec![workspace.to_string_lossy().into_owned()];
    write_roots.extend(extra_write_roots.iter().cloned());
    write_roots.extend(scratch.clone());
    let mut read_roots = build_tool_read_roots(workspace);
    read_roots.extend(scratch);
    Caveats {
        // Calibrated reads: workspace + the toolchain/package-cache roots build
        // tools need — NOT all of `$HOME` (so ~/.ssh etc. stay unreadable). The
        // system dirs (/usr, /lib, loaders) are covered by the sandbox backend's
        // base read paths; here we add the workspace and the per-user caches.
        fs_read: Scope::only(read_roots),
        fs_write: Scope::only(write_roots),
        exec: Scope::All,
        net: Scope::none(),
        max_calls: newt_policy::caveats::CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// The read roots a build tool needs beyond the sandbox backend's base system
/// paths: the workspace and per-user toolchain / package caches — resolved
/// from the operator's environment, never
/// `$HOME` as a whole. Deliberately excludes credential dirs (`~/.ssh`, `~/.aws`,
/// `~/.gnupg`): a build tool has no reason to read those, and granting them would
/// reopen the read-then-disclose path.
fn build_tool_read_roots(workspace: &Path) -> Vec<String> {
    let mut roots = vec![workspace.to_string_lossy().into_owned()];
    roots.extend(toolchain_read_roots());
    roots
}

/// The per-user toolchain / package-cache roots alone (no workspace), derived
/// from the operator's environment. Shared by [`build_tool_read_roots`] and by
/// [`newt_policy::permissions::widen_caveats`]'s build-tool exec arm — an
/// `exec` grant for `cargo`/`just`/`make` carries the same calibrated reads a
/// lifecycle build gets, one derivation, not two (dec1-build-grant, F35).
///
/// dec1-build-grant round 3 (architect review, PR #2579): every root here is
/// non-secret by construction — narrow this list rather than the callers, and
/// every caller inherits the fix. `$CARGO_HOME` is narrowed to
/// [`cargo_home_read_roots`] (never the whole directory — that grant let
/// `cargo --version; cat $CARGO_HOME/credentials.toml` read the secret
/// straight through it), and `$XDG_CACHE_HOME` is dropped entirely: nothing
/// in this repo has shown a build needs it, and it can hold arbitrary
/// per-tool state (an ML CLI's auth token cache, for one). If a real build
/// ever needs a specific cache subdirectory, grant that subdirectory with a
/// comment naming why — never the whole cache root.
#[must_use]
pub fn toolchain_read_roots() -> Vec<String> {
    let mut roots = Vec::new();
    if let Some(home) = operator_tool_home("CARGO_HOME", ".cargo") {
        roots.extend(cargo_home_read_roots(&home));
    }
    if let Some(path) = operator_tool_home("RUSTUP_HOME", ".rustup") {
        roots.push(path);
    }
    // System C toolchain headers: world-readable OS files with zero
    // disclosure value (P0 U6e) — this is a read-then-disclose fence, and
    // headers carry nothing worth disclosing. macOS is covered by the
    // SDK/Developer roots that `runtime_sandbox_policy` adds instead.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        roots.push("/usr/include".to_string());
        roots.push("/usr/local/include".to_string());
    }
    // The exact `~/.local/bin` root only — never a wider `~/.local` or
    // `$HOME` grant. `$XDG_BIN_HOME`, if set, overrides it; XDG_BIN_HOME is
    // a proposed, not ratified, XDG variable, so this is deliberately not
    // presented as a standard default.
    if let Some(path) = operator_tool_home("XDG_BIN_HOME", ".local/bin") {
        roots.push(path);
    }
    roots
}

/// `$CARGO_HOME`'s narrow, non-secret read set (dec1-build-grant round 3):
/// `bin/` (cached toolchain binaries), `registry/` (downloaded crate
/// sources), `git/` (vendored git dependencies) — what `cargo build`/`cargo
/// test` reads to resolve dependencies with the network denied — plus the
/// config file, if the operator has one. NEVER `credentials.toml` /
/// `credentials`: those hold registry auth tokens, and the whole-directory
/// grant this replaces handed them to ANY confined command whose leading
/// token merely looked like a build tool. A subdir that doesn't exist yet is
/// a harmless no-op grant, so this never probes the filesystem — same style
/// as the rest of this module's root derivation.
fn cargo_home_read_roots(cargo_home: &str) -> Vec<String> {
    let base = Path::new(cargo_home);
    let mut roots: Vec<String> = ["bin", "registry", "git"]
        .into_iter()
        .map(|subdir| base.join(subdir).to_string_lossy().into_owned())
        .collect();
    roots.push(base.join("config.toml").to_string_lossy().into_owned());
    roots.push(base.join("config").to_string_lossy().into_owned());
    roots
}

/// Build-tool exec names whose grant carries the toolchain read roots and, in
/// the TUI danger table, is covered by an existing session build grant —
/// pure DATA (three-Cs), not a `match` arm, so a new build tool is a data
/// edit. Compared against the target's file-name component, so a pathful
/// `/usr/bin/cargo` matches too.
pub const BUILD_TOOL_EXEC: &[&str] = &["cargo", "just", "make"];

/// Is `target` a build-tool exec name (see [`BUILD_TOOL_EXEC`])? Compares the
/// file-name component, matching the danger table's `is_interpreter` convention.
#[must_use]
pub fn is_build_tool_exec(target: &str) -> bool {
    let trimmed = target.trim();
    let name = Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(trimmed);
    BUILD_TOOL_EXEC.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dec1-build-grant: the build-tool predicate matches a bare name and a
    /// pathful target on its file-name component, and rejects an unrelated
    /// command — the same convention `DangerTable::is_interpreter` uses.
    #[test]
    fn is_build_tool_exec_matches_known_names_only() {
        assert!(is_build_tool_exec("cargo"));
        assert!(is_build_tool_exec("just"));
        assert!(is_build_tool_exec("make"));
        assert!(is_build_tool_exec("/usr/bin/cargo"));
        assert!(!is_build_tool_exec("rm"));
        assert!(!is_build_tool_exec("bash"));
    }

    /// RAII guard that restores an env var to its value at construction on
    /// drop, so a failed assertion mid-test does not leak the fake `HOME` (or
    /// a cleared `XDG_BIN_HOME`) into every later test in the process.
    struct EnvRestore {
        key: &'static str,
        saved: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                saved: std::env::var_os(key),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            crate::process_env::set_or_remove(
                self.key,
                self.saved.as_deref().and_then(|v| v.to_str()),
            );
        }
    }

    /// Regression for the P0 U6e build-lane read fence: the lane could not
    /// compile any crate with a C build script (`stdc-predef.h: Permission
    /// denied`) or resolve `just` from `~/.local/bin` (`Permission denied`).
    ///
    /// Deterministic (holds `process_env::lock`, pins `HOME` to a fake path,
    /// clears `XDG_BIN_HOME` for the default case) rather than reading the
    /// ambient environment — a runner with `XDG_BIN_HOME` set or no `HOME`
    /// must not flip or skip this assertion.
    ///
    /// The forbidden check reuses [`newt_policy::caveats::permits_path`] — the same
    /// containment logic the fence itself enforces — so it is an
    /// ancestor-or-equal check, not string equality: a root of `$HOME` itself
    /// would pass an equality check against `$HOME/.newt` but must fail this
    /// one, since it would still disclose everything under it.
    #[test]
    fn build_tool_read_roots_include_c_headers_and_local_bin() {
        let _env = crate::process_env::lock();
        let _restore_home = EnvRestore::capture("HOME");
        let _restore_xdg_bin = EnvRestore::capture("XDG_BIN_HOME");
        // Built from temp_dir so it is absolute on every platform: a bare
        // `/fake-…` has no drive prefix on Windows, and operator_tool_home
        // (correctly) drops a non-absolute root.
        let fake_home = std::env::temp_dir().join("fake-home-for-build-tool-read-roots-test");
        let abs_bin_home = std::env::temp_dir()
            .join("abs-bin-home")
            .to_string_lossy()
            .into_owned();
        crate::process_env::set_var("HOME", fake_home.to_str().unwrap());
        crate::process_env::remove_var("XDG_BIN_HOME");

        let roots = build_tool_read_roots(Path::new("/workspace"));
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            assert!(roots.iter().any(|r| r == "/usr/include"));
            assert!(roots.iter().any(|r| r == "/usr/local/include"));
        }
        let local_bin = fake_home.join(".local/bin").to_string_lossy().into_owned();
        assert!(roots.contains(&local_bin));

        // Ancestor-or-equal, not equality: no root may CONTAIN these HOME
        // subtrees (or HOME itself) via the same prefix logic the fs fence
        // enforces in production.
        let read_scope = Scope::only(roots.clone());
        for forbidden in [
            fake_home.clone(),
            fake_home.join(".newt"),
            fake_home.join(".config"),
            fake_home.join(".local/share"),
            fake_home.join(".ssh"),
        ] {
            let forbidden = forbidden.to_string_lossy().into_owned();
            assert!(
                !newt_policy::caveats::permits_path(&read_scope, &forbidden),
                "a build-lane root must not grant read over {forbidden}"
            );
        }

        // An absolute XDG_BIN_HOME override replaces the default.
        crate::process_env::set_var("XDG_BIN_HOME", &abs_bin_home);
        let roots_abs = build_tool_read_roots(Path::new("/workspace"));
        assert!(roots_abs.contains(&abs_bin_home));
        assert!(!roots_abs.contains(&local_bin));

        // A relative XDG_BIN_HOME must be skipped, per operator_tool_home,
        // not silently fall back to the default `~/.local/bin`.
        crate::process_env::set_var("XDG_BIN_HOME", "relative/bin");
        let roots_relative = build_tool_read_roots(Path::new("/workspace"));
        assert!(!roots_relative.iter().any(|r| r == "relative/bin"));
        assert!(!roots_relative.contains(&local_bin));
    }

    /// dec1-build-grant round 3 (architect review, PR #2579), red test (a):
    /// the whole `$CARGO_HOME` directory is no longer a root — a hostile
    /// `cargo --version; cat $CARGO_HOME/credentials.toml` read the secret
    /// straight through that grant. `toolchain_read_roots` now narrows
    /// `$CARGO_HOME` to `bin/`/`registry/`/`git/` + a config file, and drops
    /// `$XDG_CACHE_HOME` entirely. Would fail before the fix: `roots`
    /// contained the bare `$CARGO_HOME` and `$XDG_CACHE_HOME` strings, which
    /// `permits_path` (ancestor-or-equal, the same check the fence itself
    /// uses) would have found to cover `credentials.toml` and
    /// `huggingface/token`.
    #[test]
    fn toolchain_read_roots_narrows_cargo_home_and_drops_the_cache_dir() {
        let _env = crate::process_env::lock();
        let _restore_home = EnvRestore::capture("HOME");
        let _restore_cargo = EnvRestore::capture("CARGO_HOME");
        let _restore_xdg_cache = EnvRestore::capture("XDG_CACHE_HOME");
        let fake_home = std::env::temp_dir().join("fake-home-for-toolchain-read-roots-test");
        crate::process_env::set_var("HOME", fake_home.to_str().unwrap());
        crate::process_env::remove_var("CARGO_HOME");
        crate::process_env::remove_var("XDG_CACHE_HOME");

        let roots = toolchain_read_roots();
        let cargo_home = fake_home.join(".cargo");

        // The narrow subdirs a real build needs ARE granted.
        for subdir in ["bin", "registry", "git"] {
            let expected = cargo_home.join(subdir).to_string_lossy().into_owned();
            assert!(roots.contains(&expected), "missing {expected} in {roots:?}");
        }
        for config in ["config.toml", "config"] {
            let expected = cargo_home.join(config).to_string_lossy().into_owned();
            assert!(roots.contains(&expected), "missing {expected} in {roots:?}");
        }
        // The whole CARGO_HOME directory itself is never a root — a
        // directory grant would also cover credentials.toml.
        assert!(!roots
            .iter()
            .any(|r| r == cargo_home.to_string_lossy().as_ref()));

        // The credential files are never covered — ancestor-or-equal check,
        // the same containment logic the fence itself enforces.
        let read_scope = Scope::only(roots.clone());
        for secret in ["credentials.toml", "credentials"] {
            let forbidden = cargo_home.join(secret).to_string_lossy().into_owned();
            assert!(
                !newt_policy::caveats::permits_path(&read_scope, &forbidden),
                "toolchain_read_roots must never grant read over {forbidden}"
            );
        }

        // $XDG_CACHE_HOME is dropped entirely — not a root, and no path
        // beneath the default cache dir is covered either.
        let default_cache = fake_home.join(".cache").to_string_lossy().into_owned();
        assert!(!roots.iter().any(|r| r == &default_cache));
        let hf_token = fake_home
            .join(".cache/huggingface/token")
            .to_string_lossy()
            .into_owned();
        assert!(!newt_policy::caveats::permits_path(&read_scope, &hf_token));
    }

    #[test]
    fn dependency_fetch_is_online_to_crates_io_only_and_never_writes_credentials() {
        let _env = crate::process_env::lock();
        let network = Scope::only(CRATES_IO_FETCH_HOSTS.iter().map(|host| (*host).to_owned()));
        let request = dependency_fetch_request(Path::new("/ws"), Path::new("/ws/sub"), &network);
        assert_eq!(request.origin, ExecOrigin::TrustedInfra);
        assert_eq!(request.net_grant, NetGrant::Unrestricted);
        assert_eq!(request.args, ["fetch", "--locked"]);
        assert_eq!(
            request.caveats.net,
            Scope::only(CRATES_IO_FETCH_HOSTS.iter().map(|host| (*host).to_owned()))
        );
        assert!(request
            .env_grants()
            .iter()
            .all(|(name, _)| name != "CARGO_NET_OFFLINE"));
        if let Some(home) = operator_tool_home("CARGO_HOME", ".cargo") {
            let home = Path::new(&home);
            let registry = home.join("registry").to_string_lossy().into_owned();
            let credentials = home.join("credentials.toml").to_string_lossy().into_owned();
            assert!(newt_policy::caveats::permits_path(
                &request.caveats.fs_write,
                &registry
            ));
            assert!(!newt_policy::caveats::permits_path(
                &request.caveats.fs_write,
                &credentials
            ));
            assert!(!newt_policy::caveats::permits_path(
                &request.caveats.fs_read,
                &credentials
            ));
        }
    }

    #[test]
    fn dependency_fetch_network_grant_does_not_expand_filesystem_authority() {
        let _env = crate::process_env::lock();
        let ws = Path::new("/ws");
        let narrow = Scope::only(CRATES_IO_FETCH_HOSTS.iter().map(|host| (*host).to_owned()));
        let baseline = dependency_fetch_request(ws, ws, &narrow);
        let request = dependency_fetch_request(ws, ws, &Scope::All);
        assert_eq!(request.caveats.net, Scope::All);
        assert_eq!(request.caveats.fs_read, baseline.caveats.fs_read);
        assert_eq!(request.caveats.fs_write, baseline.caveats.fs_write);
        assert_eq!(request.args, baseline.args);
        let denied = dependency_fetch_request(ws, ws, &Scope::none());
        assert_eq!(denied.caveats.net, Scope::none());
    }

    #[test]
    fn build_request_discovers_toolchain_before_replacing_home() {
        let _env = crate::process_env::lock();
        let request = build_tool_request(
            Path::new("/ws"),
            Path::new("/ws"),
            "cargo",
            ["test"],
            &Scope::none(),
        );
        let env: std::collections::BTreeMap<_, _> = request.env_grants().iter().cloned().collect();
        assert_eq!(
            env.get("HOME").map(Path::new),
            Some(request.scratch_dirs[0].as_path())
        );
        assert_eq!(
            env.get("CARGO_TARGET_DIR").map(std::path::Path::new),
            Some(Path::new("/ws").join("target").as_path())
        );
        // `RUSTUP_HOME` env matches an operator override and is fully
        // readable (never a secret; toolchains only).
        if let Some(path) = operator_tool_home("RUSTUP_HOME", ".rustup") {
            assert_eq!(env.get("RUSTUP_HOME"), Some(&path));
            assert!(newt_policy::caveats::permits_path(
                &request.caveats.fs_read,
                &path
            ));
            assert!(!newt_policy::caveats::permits_path(
                &request.caveats.fs_write,
                &path
            ));
        }
        // `CARGO_HOME` env still resolves to the whole home (cargo itself
        // needs that to find its own layout) — but the READ grant is the
        // narrowed `cargo_home_read_roots` set (round 3, PR #2579): the
        // subdirs a build needs are readable, `credentials.toml` is not.
        if let Some(path) = operator_tool_home("CARGO_HOME", ".cargo") {
            assert_eq!(env.get("CARGO_HOME"), Some(&path));
            for subdir in ["bin", "registry", "git"] {
                assert!(newt_policy::caveats::permits_path(
                    &request.caveats.fs_read,
                    &Path::new(&path).join(subdir).to_string_lossy()
                ));
            }
            assert!(!newt_policy::caveats::permits_path(
                &request.caveats.fs_read,
                &Path::new(&path).join("credentials.toml").to_string_lossy()
            ));
            assert!(!newt_policy::caveats::permits_path(
                &request.caveats.fs_write,
                &path
            ));
        }
        assert!(env.keys().all(|name| [
            "HOME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "PATH",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "CARGO_TARGET_DIR",
            "CARGO_NET_OFFLINE",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "SDKROOT"
        ]
        .contains(&name.as_str())));
        assert_eq!(request.net_grant, NetGrant::DenyAll);
        assert_eq!(request.caveats.exec, Scope::All);
        // The proposed Build fence includes only this workspace's private
        // scratch partition, not its shared temporary parent.
        assert_eq!(
            request.caveats.fs_write,
            build_tool_caveats(Path::new("/ws")).fs_write
        );
        let scratch = &request.scratch_dirs[0];
        assert_eq!(request.scratch_dirs.len(), 1);
        assert_eq!(env.get("TMPDIR").map(Path::new), Some(scratch.as_path()));
        assert!(!scratch.starts_with("/ws"), "{scratch:?}");
        assert!(newt_policy::caveats::permits_path(
            &request.caveats.fs_write,
            &scratch.to_string_lossy()
        ));
        assert!(newt_policy::caveats::permits_path(
            &request.caveats.fs_read,
            &scratch.to_string_lossy()
        ));
    }

    /// Build and fetch must isolate HOME writes without losing toolchain homes.
    #[test]
    fn toolchain_requests_keep_home_outside_the_workspace() {
        let _env = crate::process_env::lock();
        let workspace = Path::new("/ws");
        for request in [
            build_tool_request(workspace, workspace, "cargo", ["check"], &Scope::none()),
            dependency_fetch_request(workspace, workspace, &Scope::none()),
        ] {
            let env: std::collections::BTreeMap<_, _> =
                request.env_grants().iter().cloned().collect();
            let home = Path::new(&env["HOME"]);
            assert!(
                !home.starts_with(workspace),
                "HOME must not pollute the repo"
            );
            assert_eq!(home, request.scratch_dirs[0]);
            assert_eq!(env["HOME"], env["TMPDIR"]);
            for (name, default) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
                assert_eq!(env.get(name).cloned(), operator_tool_home(name, default));
            }
            assert!(newt_policy::caveats::permits_path(
                &request.caveats.fs_write,
                &home.to_string_lossy()
            ));
            assert!(!newt_policy::caveats::permits_path(
                &request.caveats.fs_write,
                &home.parent().unwrap().parent().unwrap().to_string_lossy()
            ));
        }
    }

    #[test]
    fn each_build_scratch_dir_is_unique() {
        let ws = Path::new("/ws");
        assert_ne!(build_scratch_dir(ws), build_scratch_dir(ws));
    }

    #[test]
    fn build_network_authority_narrows_hosts_and_preserves_filesystem_fence() {
        let _env = crate::process_env::lock();
        let ws = Path::new("/ws");
        let baseline = build_tool_caveats(ws);
        // #2729: derive the native grant from the effective child scope.
        for (network, expected) in [
            (Scope::none(), Scope::none()),
            (Scope::only(["example.test".into()]), Scope::none()),
            (Scope::All, Scope::All),
        ] {
            let request = build_tool_request(ws, ws, "cargo", ["test"], &network);
            assert_eq!(request.caveats.net, expected);
            assert_eq!(request.caveats.fs_read, baseline.fs_read);
            assert_eq!(request.caveats.fs_write, baseline.fs_write);
            assert_eq!(request.caveats.exec, baseline.exec);
            assert_eq!(request.origin, ExecOrigin::AgentInfluenced);
            assert_eq!(
                request.net_grant,
                if expected == Scope::none() {
                    NetGrant::DenyAll
                } else {
                    NetGrant::Unrestricted
                }
            );
            assert!(request
                .env_grants()
                .contains(&("CARGO_NET_OFFLINE".into(), "true".into())));
        }
    }

    #[cfg(unix)]
    #[test]
    fn scratch_dir_is_owner_only_and_gone_after_the_run() {
        use std::os::unix::fs::PermissionsExt;
        let ws = tempfile::tempdir().unwrap();
        let path = build_scratch_dir(ws.path());
        let guard = ScratchDir::create(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o700);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        std::fs::write(path.join("cc.tmp"), b"x").unwrap();
        drop(guard);
        assert!(!path.exists(), "scratch dir must be removed");
        assert!(
            !ws.path().join("target").exists(),
            "an empty target/ this run created is removed too"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scratch_cleanup_deletes_nothing_else() {
        let ws = tempfile::tempdir().unwrap();
        let target = ws.path().join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::write(target.join("debug/keep"), b"k").unwrap();
        let other = ScratchDir::create(&build_scratch_dir(ws.path())).unwrap();
        let path = build_scratch_dir(ws.path());
        drop(ScratchDir::create(&path).unwrap());
        assert!(target.join("debug/keep").is_file());
        assert!(other.path.is_dir(), "a concurrent run's dir is untouched");
        drop(other);
        assert!(target.join("debug/keep").is_file());
    }

    /// A pre-existing symlink (or anything) at the scratch path is refused,
    /// never followed into another place, and its target is left alone.
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_scratch_path_is_refused() {
        let ws = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let path = scratch_dir_under(ws.path(), None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &path).unwrap();
        assert!(ScratchDir::create(&path).is_err());
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(elsewhere.path().is_dir());
    }

    /// A missing workspace is a refusal: the scratch guard creates `target/`
    /// and below, never the workspace itself or its ancestors.
    #[test]
    fn a_missing_workspace_is_refused_not_created() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("missing").join("ws");
        assert!(ScratchDir::create(&scratch_dir_under(&ws, None)).is_err());
        assert!(!root.path().join("missing").exists());
    }

    /// Pure policy guard for the fetch broker; real rewrite refusal grounds it
    /// in the ignored weekly/release tier, including native macOS admission.
    #[cfg(feature = "confine")]
    #[test]
    fn fetch_copy_requires_deny_all_even_with_ambient_session_network() {
        let baseline = Caveats {
            net: Scope::All,
            ..workspace_confined_caveats(Path::new("/ws"))
        };
        let request = ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            "git",
            ["fetch"],
            "/ws",
            baseline.clone(),
        );
        let request = crate::git_staging::confined::fetch_copy_request(request);
        assert_eq!(
            request.net_grant,
            NetGrant::DenyAll,
            "fetch copy lacks a fail-closed network floor"
        );
        assert_eq!(request.caveats.fs_read, baseline.fs_read);
        assert_eq!(request.caveats.fs_write, baseline.fs_write);
        assert_eq!(request.caveats.exec, baseline.exec);
        assert!(request
            .env_grants()
            .iter()
            .any(|(k, v)| k == "GIT_ALLOW_PROTOCOL" && v == "file"));
        for (key, value) in [
            ("core.hooksPath", "/dev/null"),
            ("core.fsmonitor", ""),
            ("protocol.ext.allow", "never"),
        ] {
            let (env_key, _) = request
                .env_grants()
                .iter()
                .find(|(k, v)| k.starts_with("GIT_CONFIG_KEY_") && v == key)
                .unwrap();
            let value_key = env_key.replace("_KEY_", "_VALUE_");
            assert!(request
                .env_grants()
                .iter()
                .any(|(k, v)| k == &value_key && v == value));
        }
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
        use newt_policy::caveats::ScopeExt;
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
            "no write to shared /tmp — only the private workspace scratch partition"
        );
        assert!(
            !cav.net.permits(&"evil.example".to_string()),
            "no network — the exfil channel is closed"
        );
    }

    /// #2604 escape hatch: an operator build scratch base moves the run out of
    /// the repo, to `<base>/<workspace id>/<run>`, and the fence grants exactly
    /// that per-workspace root. With no base, nothing is added.
    #[test]
    fn an_operator_build_scratch_base_moves_the_run_and_is_granted() {
        let ws = Path::new("/ws");
        let base = std::env::temp_dir().join("operator-build-scratch");
        let root = operator_build_scratch_root(ws, Some(base.clone())).unwrap();
        assert_eq!(root.parent(), Some(base.as_path()));
        assert_eq!(
            Some(root.clone()),
            operator_build_scratch_root(ws, Some(base.clone()))
        );
        assert_ne!(
            Some(root.clone()),
            operator_build_scratch_root(Path::new("/other"), Some(base.clone()))
        );

        let run = scratch_dir_under(ws, Some(base.clone()));
        assert_eq!(run.parent(), Some(root.as_path()));
        assert!(
            !run.starts_with(ws),
            "an operator base leaves the workspace"
        );

        let root = root.to_string_lossy().into_owned();
        let cav = build_caveats_with_scratch(ws, &[], Some(base));
        assert!(newt_policy::caveats::permits_path(
            &cav.fs_write,
            &run.to_string_lossy()
        ));
        assert!(newt_policy::caveats::permits_path(&cav.fs_read, &root));

        let default = build_caveats_with_scratch(ws, &[], None);
        assert_eq!(default.fs_write, Scope::only(["/ws".to_owned()]));
        assert!(scratch_dir_under(ws, None).starts_with("/ws/target/tmp"));
    }

    #[test]
    fn build_tool_caveats_with_writes_grants_only_the_named_extra_root() {
        use newt_policy::caveats::ScopeExt;
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
        use newt_policy::caveats::ScopeExt;
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
    fn refusal_displays_are_honest_about_fail_closed() {
        let r = ExecRefused::ConfinementUnenforceable("no Landlock".into());
        assert!(r.to_string().contains("fail-closed"));
    }
}

#[cfg(all(test, target_os = "linux", feature = "confine"))]
#[path = "confined_exec_git_tests.rs"]
mod git_read_race_tests;
