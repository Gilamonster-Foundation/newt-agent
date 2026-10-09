//! [`ShellTool`] — the confined shell, **argv + safe-subset engine** (ADR 0005).
//!
//! Per ADR 0005, the object-capability *boundary* is L3 (kernel) and this engine
//! is the L2 *convenience*: `agent-bridle` is the exec funnel — it parses the
//! request itself (see [`crate::parse`]), checks the `exec`/`fs` leash, spawns
//! the program(s) directly, and **refuses the dynamic constructs by design**.
//! When effective caveats engage an available native backend, children spawn
//! inside that L3 boundary and inherit its scope-shaped filesystem/exec/network
//! rules. Landlock, Seatbelt, and AppContainer cover different axis shapes; the
//! result's `sandbox_kind` plus per-axis enforcement report state exactly what
//! held. When no backend engages, `sandbox_kind` is [`SandboxKind::None`] rather
//! than overclaiming (I9). A restricted filesystem axis fails closed if it
//! cannot be kernel-enforced.
//!
//! The engine (agent-bridle#34 Track A + #45): a sequence of pipelines joined by
//! `&&`/`||`/`;`, each pipeline simple commands with quoted arguments,
//! redirections (`> out`, `>> out`, `< in`, `2> err`, `2>&1`), filename globbing
//! (`*`/`?`/`[…]`), and **allowlisted `$VAR` expansion**. Because `agent-bridle`
//! performs each redirect's open and each glob's directory listing itself, those
//! filesystem touches are leash-checked (`fs_write`/`fs_read`) *before any stage
//! spawns*; a `$VAR` is expanded only if its name is on a small secret-free
//! allowlist (the configured `var_allowlist`), checked before any spawn — a real enforcement
//! point, unlike a spawned program's own opens (L3's job). `2>&1` uses a shared
//! `std::io::pipe()` writer cloned into both stdout and stderr. Process spawning
//! is behind a [`Spawner`] seam (mocked in unit tests; real path in
//! `tests/real_spawn.rs`).

use std::collections::{BTreeMap, HashSet};
use std::io::{PipeReader, PipeWriter, Read};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use agent_bridle_core::{
    admit, best_available_sandbox, effective_sandbox_kind, enforcement_report, human_gate,
    is_unbridled, unenforceable_axis, AdmissionDecision, AdmittedFence, BackendProjection, Caveats,
    ConfinedAxis, ConfinementMechanism, Denial, DenialKind, Disclosure, EnforcementReport,
    Invocation, LimitsPolicy, ResolvedScope, RuntimeClosure, SandboxKind, SandboxPolicy, Scope,
    StdioPosture, Tool, ToolContext, ToolEnvelope, ToolError, ToolResult,
};
use async_trait::async_trait;

use crate::net_proxy;
use crate::output_observer::{output_session, OutputEmitter};
use crate::parse::{
    classify, seg_literal, Arg, Command, Redirect, Refusal, Script, ScriptItem, Seg, Sep, StderrTo,
};

/// What a finished pipeline produced (the last stage's exit code; concatenated
/// output). The unit of the [`Spawner`] seam. The captured output is bounded by
/// the configured cap ([`LimitsPolicy::max_output_bytes`]) so a chatty command
/// cannot return unbounded output. A configured observer receives a bounded
/// live view while pipes are drained. For multi-stage stderr, live delivery
/// follows reader scheduling while this value is assembled in stage order, so
/// the completed envelope remains the authoritative capture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Captured {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Whether stdout was clipped at the configured output cap (more was produced).
    pub stdout_truncated: bool,
    /// Whether stderr was clipped at the configured output cap (more was produced).
    pub stderr_truncated: bool,
    /// #196: structured `net` denials observed DURING the run — one per
    /// out-of-allow-list host the egress proxy refused. Unlike `exec`/`open`
    /// denials (decided at pre-spawn admission), a net refusal is only known
    /// after the child has run, so it rides back on the capture and is attached
    /// to the result envelope by the caller. Empty on the common path.
    pub net_denials: Vec<Denial>,
    /// AB-006 (#269): the run exceeded its timeout and its process group was
    /// killed + reaped. Descendants that escape the owned process group are
    /// outside this guarantee (agent-bridle#420).
    pub timed_out: bool,
}

/// The pipeline-execution seam.
///
/// The real implementation ([`OsSpawner`]) spawns processes (and expands globs
/// against the real filesystem); tests inject a mock so the parse + leash +
/// sequencing logic is verified without real subprocesses (the workspace norm:
/// no real process/fs in unit tests). A `Spawner` only ever receives a pipeline
/// that already passed the `exec` **and** `fs` (redirect + glob-dir) leash —
/// admission happens in [`ShellTool::invoke`] *before* the spawner runs.
/// Per-invocation spawn *mechanism* config, threaded from `ShellTool`'s fields to
/// the spawner. It rides an explicit parameter, **never** `ToolContext` (which
/// carries only authority — authority≠mechanism, ADR 0017 D2). Bundles the tuning
/// knobs so the `Spawner` seam takes one config, not a growing list of scalars.
pub(crate) struct SpawnCfg {
    /// Captured stdout/stderr cap ([`LimitsPolicy::max_output_bytes`]).
    pub max_output: usize,
    /// Egress audit sink path ([`LimitsPolicy::audit_sink`]; `None` = off).
    pub audit_sink: Option<String>,
    /// Sandbox read/exec allow-lists + ABI floors ([`SandboxPolicy`]).
    pub sandbox: Arc<SandboxPolicy>,
    pub held_read_roots: Vec<agent_bridle_core::HeldReadRoot>,
    /// Explicit private-host approvals (#385/#386, forward-ported from the 0.7
    /// line), independently intersected with the invocation's ordinary network
    /// scope by the shared egress proxy.
    pub private_hosts: HashSet<String>,
    /// Process is **unbridled** (ADR 0018): drop the L3 OS sandbox and run the
    /// pipeline natively. The L2 grant checks in `invoke` still gate (advisory);
    /// only the kernel mechanism is skipped. Read once from the process marker.
    pub unbridled: bool,
    /// Bounded presentation observer for this invocation (authority-neutral).
    pub output: OutputEmitter,
    /// One deadline for the invocation, shared through every script/pipeline
    /// stage. Expiry kills/reaps owned groups before the worker returns.
    pub deadline: crate::supervisor::Deadline,
}

pub(crate) trait Spawner: Send + Sync {
    /// Whether this spawner crosses the native process boundary and therefore
    /// must pass the backend's ruleset-grain authority admission. Test spawners
    /// that execute no process return `false`; every production spawner must
    /// return `true`.
    fn requires_backend_admission(&self) -> bool;

    /// Run one leash-approved pipeline to completion, capturing its output. The
    /// effective `caveats` are passed so the real spawner can apply the selected
    /// L3 OS sandbox before spawning; the mock ignores them. `env` is the
    /// host/operator-supplied environment (the env seam, newt #783): the real
    /// spawner sets these vars on each spawned child (additive over the inherited
    /// ambient env). `env` is structured host input, never model-authored command
    /// text, so it grants no new authority — the exec/fs leash is unaffected.
    /// `cfg` carries the mechanism tuning (output cap, audit sink, sandbox policy).
    fn run(
        &self,
        stages: &[Command],
        cwd: Option<&str>,
        caveats: &Caveats,
        env: &BTreeMap<String, String>,
        cfg: &SpawnCfg,
    ) -> ToolResult<Captured>;
}

/// The real spawner: a `std::process` pipeline wired with OS pipes + redirects,
/// expanding globs against the real filesystem, optionally inside an L3 sandbox.
struct OsSpawner;

impl Spawner for OsSpawner {
    fn requires_backend_admission(&self) -> bool {
        true
    }

    fn run(
        &self,
        stages: &[Command],
        cwd: Option<&str>,
        caveats: &Caveats,
        env: &BTreeMap<String, String>,
        cfg: &SpawnCfg,
    ) -> ToolResult<Captured> {
        // Unbridled (ADR 0018): the operator explicitly dropped the L3 mechanism —
        // run natively, no OS sandbox and no egress proxy. The L2 grant checks in
        // `invoke` already gated this run (advisory); confinement is off by consent.
        if cfg.unbridled {
            return run_pipeline(
                stages,
                cwd,
                &[],
                env,
                cfg.max_output,
                cfg.output.clone(),
                &cfg.deadline,
                // Unbridled: dropping the mechanism is what the operator
                // acknowledged — redirect opens stay unbounded (#351).
                &Scope::All,
                &Scope::All,
            );
        }
        // A general remote-host `net` allow-list that cannot be named in SBPL is
        // enforced by the loopback egress proxy (#124, ADR 0016): fence the child
        // to loopback and route it through the proxy. Self-gating — `Some` only
        // where the fence is actually emittable (macOS + seatbelt).
        if let Some((allow_hosts, fenced)) = egress_proxy_plan(caveats, &cfg.sandbox) {
            return run_with_egress_proxy(stages, cwd, &fenced, env, allow_hosts, cfg);
        }
        // When a native OS sandbox will actually confine this run, apply its
        // thread- or wrapper-based launch path (ADR 0005 L3 / ADR 0006 D4).
        // Otherwise run directly — no need to spend a thread.
        if intended_sandbox_kind(caveats, &cfg.sandbox) == SandboxKind::None {
            run_pipeline(
                stages,
                cwd,
                &[],
                env,
                cfg.max_output,
                cfg.output.clone(),
                &cfg.deadline,
                &caveats.fs_read,
                &caveats.fs_write,
            )
        } else {
            run_confined(stages, cwd, caveats, env, cfg)
        }
    }
}

/// The egress-proxy plan for `caveats`, or `None` to fall through to the ordinary
/// confinement paths (#124, ADR 0016). Since #257 this is the SHARED core
/// decision ([`agent_bridle_core::egress_proxy_plan`]) — the same one
/// `ConfinedCommand::spawn_tokio` routes through — kept as a thin local alias so
/// the spawn routing ([`OsSpawner::run`]) and the reported `sandbox_kind`
/// ([`ShellTool::invoke`]) keep one call-shape and cannot disagree.
fn egress_proxy_plan(
    caveats: &Caveats,
    sandbox: &Arc<SandboxPolicy>,
) -> Option<(Vec<String>, Caveats)> {
    agent_bridle_core::egress_proxy_plan(caveats, sandbox)
}

/// Run the pipeline under the loopback egress proxy (#124, ADR 0016). Mirrors
/// [`run_confined`] but, before spawning: (1) starts a loopback forward proxy
/// bound to the `allow_hosts` — **fail-closed** if it cannot bind; (2) computes
/// the fence prefix from the loopback-`fenced` caveats — fail-closed if the
/// wrapper is missing; (3) injects `*_PROXY` into a clone of the env-seam map so
/// the child routes its HTTP/HTTPS out through the proxy. The [`ProxyHandle`] is
/// held until the confined child has been reaped, then explicitly finalized via
/// [`agent_bridle_core::net_proxy::ProxyHandle::shutdown_and_join`] (#372) — so
/// the proxy's lifetime brackets the child's, and the `net_denials` this
/// function reports come from FROZEN evidence, not a live snapshot that could
/// still be mutating.
fn run_with_egress_proxy(
    stages: &[Command],
    cwd: Option<&str>,
    fenced: &Caveats,
    env: &BTreeMap<String, String>,
    allow_hosts: Vec<String>,
    cfg: &SpawnCfg,
) -> ToolResult<Captured> {
    // (1) Fence prefix first (pure, cheap) — fail-closed if the wrapper is gone.
    let prefix = best_available_sandbox(&cfg.sandbox).command_prefix(fenced)?;
    // (2) Start the proxy — fail-closed if it cannot bind loopback (never spawn
    //     an unfenced child that would then egress freely). Audit is opt-in via the
    //     configured audit sink (observability only; off = zero overhead).
    let proxy = net_proxy::start_with_private_hosts(
        allow_hosts,
        cfg.private_hosts.iter().cloned(),
        Arc::new(net_proxy::StdResolver),
        net_audit_sink(cfg.audit_sink.as_deref()),
    )
    .map_err(ToolError::Exec)?;
    // (3) Point the child at the proxy via the env seam (a clone — never mutate
    //     the caller's map).
    let mut env = env.clone();
    for (k, v) in proxy.proxy_env() {
        env.insert(k, v);
    }

    let stages = stages.to_vec();
    let cwd = cwd.map(str::to_string);
    let fenced = fenced.clone();
    let max_output = cfg.max_output;
    let output = cfg.output.clone();
    let sandbox = cfg.sandbox.clone();
    let held = cfg.held_read_roots.clone();
    let deadline = cfg.deadline.clone();
    let captured = std::thread::Builder::new()
        .name("agent-bridle-confined".to_string())
        .spawn(move || {
            best_available_sandbox(&sandbox).apply_with_held_roots(&fenced, &held)?;
            run_pipeline(
                &stages,
                cwd.as_deref(),
                &prefix,
                &env,
                max_output,
                output,
                &deadline,
                &fenced.fs_read,
                &fenced.fs_write,
            )
        })
        .map_err(ToolError::Exec)?
        .join()
        .map_err(|_| {
            ToolError::Exec(std::io::Error::other("confined execution thread panicked"))
        })?;
    // #372: the child is reaped, but that does NOT mean every proxy connection
    // is complete — a connection worker can still be finishing (or, for an idle
    // tunnel, blocked until force-closed). Finalize explicitly: this blocks
    // until every proxy worker is joined and returns FROZEN evidence, never a
    // live snapshot that could race a worker still recording its decision.
    // Finalize regardless of the child's own outcome (never leak the proxy on a
    // child failure), but let a child failure take precedence over a
    // finalization failure when surfacing the error.
    let finalized = proxy.shutdown_and_join();
    let mut captured = captured?;
    let evidence = finalized.map_err(|e| {
        ToolError::Exec(std::io::Error::other(format!(
            "egress proxy finalization failed: {e}"
        )))
    })?;
    captured.net_denials = evidence
        .refused_hosts
        .into_iter()
        .map(|host| Denial {
            kind: DenialKind::Net,
            reason: format!("net does not permit '{host}'"),
            target: host,
        })
        .collect();
    Ok(captured)
}

/// Build the egress audit sink from the configured audit path (#124, ADR 0016;
/// `LimitsPolicy::audit_sink`, which the config loader maps from the legacy
/// `BRIDLE_NET_AUDIT` setting — I6, #145). `None`/empty → **no audit** (the
/// default; zero overhead). A path → append each proxied connection as one JSON
/// line (host, port, decision, bytes, duration) for `bridle-netmon` to render
/// live. Audit is **observability only** — it never changes an enforcement
/// decision — so a path that cannot be opened falls back to the null sink rather
/// than failing the run.
fn net_audit_sink(configured: Option<&str>) -> Arc<dyn net_proxy::AuditSink> {
    match configured {
        Some(path) if !path.is_empty() => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|f| Arc::new(net_proxy::JsonlSink::new(f)) as Arc<dyn net_proxy::AuditSink>)
            .unwrap_or_else(|_| Arc::new(net_proxy::NullSink)),
        _ => Arc::new(net_proxy::NullSink),
    }
}

/// The L3 `SandboxKind` that will actually be enforced for these caveats in this
/// build, on this host — the value reported in the result envelope (I9 / ADR
/// 0006 D3). [`effective_sandbox_kind`] is the shared honesty rule: the strongest
/// available backend's kind when these caveats engage one of its governed axis
/// shapes, else `None`. The same rule backs the subprocess primitive in core.
fn intended_sandbox_kind(caveats: &Caveats, sandbox: &Arc<SandboxPolicy>) -> SandboxKind {
    effective_sandbox_kind(best_available_sandbox(sandbox).kind(), caveats)
}

/// Run the pipeline on a dedicated thread that first applies the OS sandbox.
///
/// Two confinement mechanisms, honored uniformly (ADR 0006): a thread-confining
/// backend (Landlock) restricts this very thread in `apply` — per-thread,
/// irreversible, inherited across `fork`/`execve`, so it must run on a throwaway
/// thread (never the shared blocking pool) immediately before spawning the
/// children. Wrapper backends (Seatbelt/AppContainer) return an argv prefix from
/// `command_prefix`, prepended to every stage so the child is
/// spawned already confined. Both are fail-closed (ADR 0006 D4): if confinement
/// cannot be established the run errors rather than proceeding unconfined.
fn run_confined(
    stages: &[Command],
    cwd: Option<&str>,
    caveats: &Caveats,
    env: &BTreeMap<String, String>,
    cfg: &SpawnCfg,
) -> ToolResult<Captured> {
    // Computed before the spawn so a fail-closed wrapper error aborts the run.
    let prefix = best_available_sandbox(&cfg.sandbox).command_prefix(caveats)?;
    let stages = stages.to_vec();
    let cwd = cwd.map(str::to_string);
    let caveats = caveats.clone();
    let env = env.clone();
    let max_output = cfg.max_output;
    let output = cfg.output.clone();
    let sandbox = cfg.sandbox.clone();
    let held = cfg.held_read_roots.clone();
    let deadline = cfg.deadline.clone();
    std::thread::Builder::new()
        .name("agent-bridle-confined".to_string())
        .spawn(move || {
            best_available_sandbox(&sandbox).apply_with_held_roots(&caveats, &held)?;
            run_pipeline(
                &stages,
                cwd.as_deref(),
                &prefix,
                &env,
                max_output,
                output,
                &deadline,
                &caveats.fs_read,
                &caveats.fs_write,
            )
        })
        .map_err(ToolError::Exec)?
        .join()
        .map_err(|_| ToolError::Exec(std::io::Error::other("confined execution thread panicked")))?
}

/// The tool's input schema, parsed once from the embedded `shell_tool.schema.json`
/// data file — the schema is *knowledge*, so it lives in plain-text data, not an
/// inline `json!` literal (three-Cs: knowledge in data, not logic). `include_str!`
/// binds it at compile time, so a malformed edit fails the build's tests, never a
/// live dispatch. The per-instance `timeout_secs` ceiling is injected by
/// [`Tool::schema`] over this base.
static SHELL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("shell_tool.schema.json"))
        .expect("embedded shell_tool.schema.json must be valid JSON")
});

/// The confined shell tool.
///
/// Registers under `"shell"`. Accepts either argv form (`program` + `args`) or a
/// free-form `cmd` string parsed by the safe-subset engine. Leash refusals
/// (out-of-scope `exec`/`fs`, a refused construct) are returned as a **structured
/// denied envelope** (`denied: true`), not a hard error.
#[derive(Clone)]
pub struct ShellTool {
    spawner: Arc<dyn Spawner>,
    env: Arc<dyn EnvProvider>,
    lister: Arc<dyn DirLister>,
    limits: LimitsPolicy,
    /// Sandbox mechanism policy (read/exec allow-lists, ABI floors) the L3 backend
    /// enforces (I5-B, #144). Rides the tool, not the `ToolContext`.
    sandbox: Arc<SandboxPolicy>,
    private_hosts: HashSet<String>,
    output_observer: Option<Arc<dyn crate::ShellOutputObserver>>,
    execution_lease: Option<crate::ExecutionLease>,
    held_read_roots: Vec<agent_bridle_core::HeldReadRoot>,
}

impl std::fmt::Debug for ShellTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ShellTool")
    }
}

impl ShellTool {
    /// Construct the tool with the real OS spawner, environment, and dir lister,
    /// and the default [`LimitsPolicy`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(LimitsPolicy::default())
    }

    /// Construct with the real seams and a caller-supplied [`LimitsPolicy`] — the
    /// configurability seam (agent-bridle#143): tune timeouts / output / glob caps.
    #[must_use]
    pub fn with_config(limits: LimitsPolicy) -> Self {
        Self {
            spawner: Arc::new(OsSpawner),
            env: Arc::new(RealEnv),
            lister: Arc::new(RealDirLister),
            limits,
            sandbox: Arc::new(SandboxPolicy::default()),
            private_hosts: HashSet::new(),
            output_observer: None,
            execution_lease: None,
            held_read_roots: Vec::new(),
        }
    }

    /// Attach a presentation-only observer for bounded stdout/stderr chunks.
    ///
    /// The observer is queued only after leash admission. It receives at most
    /// the configured output cap per stream and cannot change authorization or
    /// the final result envelope. Delivery may finish asynchronously after the
    /// invocation returns; `on_finish` marks the queue-drained boundary.
    #[must_use]
    pub fn with_output_observer(mut self, observer: Arc<dyn crate::ShellOutputObserver>) -> Self {
        self.output_observer = Some(observer);
        self
    }

    /// Retain resources until the blocking execution owner finishes. Dropping
    /// the async waiter does not release the lease early; the safe-subset
    /// engine's existing execution deadline still governs worker shutdown.
    #[must_use]
    pub fn with_execution_lease(mut self, lease: crate::ExecutionLease) -> Self {
        self.execution_lease = Some(lease);
        self
    }

    /// Carry already admitted read roots to the kernel fence by descriptor.
    /// This changes the mechanism, never the tool's filesystem authority.
    #[must_use]
    pub fn with_held_read_roots(mut self, roots: Vec<agent_bridle_core::HeldReadRoot>) -> Self {
        self.held_read_roots = roots;
        self
    }

    /// Set the sandbox mechanism policy (read/exec allow-lists, ABI floors) the L3
    /// backend enforces (I5-B, #144). The default is today's built-in allow-lists.
    #[must_use]
    pub fn with_sandbox_policy(mut self, sandbox: SandboxPolicy) -> Self {
        self.sandbox = Arc::new(sandbox);
        self
    }

    /// Approve exact names for RFC1918/ULA resolution by the existing fenced
    /// egress proxy (#385/#386, forward-ported from the 0.7 line). The owning
    /// harness supplies explicit operator approvals; command text and remote
    /// metadata are not authority. The invocation's ordinary network scope must
    /// independently allow every requested host.
    ///
    /// Empty by default. This does not start a proxy where no kernel fence
    /// exists, permit forbidden address ranges, or change filesystem/exec scope.
    pub fn with_private_hosts(
        mut self,
        hosts: impl IntoIterator<Item = String>,
    ) -> std::io::Result<Self> {
        self.private_hosts = net_proxy::canonical_private_hosts(hosts)?;
        Ok(self)
    }

    /// Construct with an injected spawner; real environment + dir lister (tests).
    #[cfg(test)]
    fn with_spawner(spawner: Arc<dyn Spawner>) -> Self {
        Self {
            spawner,
            env: Arc::new(RealEnv),
            lister: Arc::new(RealDirLister),
            limits: LimitsPolicy::default(),
            sandbox: Arc::new(SandboxPolicy::default()),
            private_hosts: HashSet::new(),
            output_observer: None,
            execution_lease: None,
            held_read_roots: Vec::new(),
        }
    }

    /// Construct with an injected spawner **and** a fake environment (tests only),
    /// so the `$VAR` allowlist + expansion + the resolved-path leash are
    /// exercised without touching the real process environment.
    #[cfg(test)]
    fn with_spawner_and_env(spawner: Arc<dyn Spawner>, env: Arc<dyn EnvProvider>) -> Self {
        Self {
            spawner,
            env,
            lister: Arc::new(RealDirLister),
            limits: LimitsPolicy::default(),
            sandbox: Arc::new(SandboxPolicy::default()),
            private_hosts: HashSet::new(),
            output_observer: None,
            execution_lease: None,
            held_read_roots: Vec::new(),
        }
    }

    /// Construct with all three seams injected (tests only): a fake spawner, env,
    /// and directory lister, so glob expansion + the per-directory `fs_read`
    /// leash are exercised without a real filesystem (#47).
    #[cfg(test)]
    fn with_seams(
        spawner: Arc<dyn Spawner>,
        env: Arc<dyn EnvProvider>,
        lister: Arc<dyn DirLister>,
    ) -> Self {
        Self {
            spawner,
            env,
            lister,
            limits: LimitsPolicy::default(),
            sandbox: Arc::new(SandboxPolicy::default()),
            private_hosts: HashSet::new(),
            output_observer: None,
            execution_lease: None,
            held_read_roots: Vec::new(),
        }
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn schema(&self) -> serde_json::Value {
        // Structure + descriptions live in the `shell_tool.schema.json` data
        // file (knowledge in data, not an inline literal). The `timeout_secs`
        // ceiling is a per-instance property — the configured `LimitsPolicy`
        // (`with_config`) — so it is injected over the data-file base here rather
        // than baked into the file, keeping the bound's source of truth in Rust.
        let mut schema = SHELL_SCHEMA.clone();
        schema["properties"]["timeout_secs"]["maximum"] =
            serde_json::Value::from(self.limits.max_timeout_secs);
        schema
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

    /// The shell's public contract represents a **pre-execution** leash refusal
    /// as an in-band `Ok` envelope (`deny`/`refused_envelope`), not a hard
    /// `Err`. So it reports the typed [`Invocation`] itself — `Denied` for those
    /// pre-run refusals, `Ran` for any envelope produced after the child spawned
    /// (success, timeout, or a *mid-run* egress-proxy denial) — so the registry
    /// charges the call budget correctly without scraping the envelope JSON.
    async fn invoke_accounted(
        &self,
        args: serde_json::Value,
        cx: &ToolContext,
    ) -> ToolResult<Invocation> {
        let parsed = ShellArgs::parse(&args, &self.limits)?;
        // Unbridled (ADR 0018): the operator dropped the L3 mechanism. Report
        // `None` (no OS sandbox) — the per-axis report then honestly shows the
        // remaining L2/interceptor or advisory strength, never `kernel`.
        // Authority is unchanged; only the mechanism is off. Every envelope
        // discloses `unbridled` (D5).
        let unbridled = is_unbridled();
        // Honest reporting (ADR 0005 D1 / I9 / ADR 0006 D3): report the L3 kind
        // that will actually be enforced for these caveats on this host and
        // backend. `OsSpawner` applies exactly this decision, fail-closed.
        //
        // For a remote-host allow-list on a profile-capable host,
        // `egress_proxy_plan` derives candidate loopback-fenced backend caveats.
        // Profile emission is not authority evidence: the ruleset-grain admission
        // below projects these exact candidate caveats and rejects an Unknown net
        // axis before `OsSpawner` can run. The candidate kind/report are retained
        // for the structured refusal. If a future backend establishes a faithful
        // bounded projection, this same derivation becomes the executable route.
        let backend_caveats = if unbridled {
            cx.caveats().clone()
        } else {
            match egress_proxy_plan(cx.caveats(), &self.sandbox) {
                Some((_, fenced)) => fenced,
                None => cx.caveats().clone(),
            }
        };
        let sandbox_kind = if unbridled {
            SandboxKind::None
        } else {
            intended_sandbox_kind(&backend_caveats, &self.sandbox)
        };
        // Axis-granular honesty (ADR 0004 D1 / #30): every envelope this run
        // returns carries the per-axis report alongside the coarse sandbox_kind.
        // The mechanism governing this run: reported backend kind + the
        // child-network policy on `self.sandbox` (the same policy the engine
        // confines with). The per-axis report and the fail-closed guard below both
        // read THIS, so neither over-claims the net axis (a Landlock `net:none` is
        // Kernel only under `DenyDirect`).
        let mechanism = ConfinementMechanism::new(sandbox_kind, self.sandbox.child_network);
        let enforcement = enforcement_report(cx.caveats(), mechanism);

        // Resolve to a script (sequence of pipelines), or surface a refusal.
        let mut script = match parsed.script() {
            Ok(s) => s,
            Err(refusal) => {
                return Ok(refused_envelope(
                    sandbox_kind,
                    enforcement,
                    &refusal,
                    parsed.cmd.as_deref(),
                ));
            }
        };

        // Lower `$VAR` (#46) through the env seam so the RESOLVED value is what
        // the fs leash checks below and the spawner opens — never a literal
        // `$VAR`. Glob+variable words (`$DIR/*.rs`) lower to a resolved glob (with
        // the re-injection guard); redirect targets (`> $TMPDIR/out`) to a literal
        // path. A non-allowlisted (or basename-injected) variable denies pre-spawn.
        for item in &mut script {
            for stage in &mut item.pipeline {
                // Expand globs (and glob+var words) to literal matches, leash-
                // checking EVERY directory the walk lists (#47) — multi-segment
                // (`*/foo.rs`) and recursive (`**/*.rs`), all before any spawn.
                // argv[0] is left intact; the program-position check below refuses
                // a glob/var program (we never exec a pattern).
                let mut new_argv: Vec<Arg> = Vec::with_capacity(stage.argv.len());
                for (i, arg) in stage.argv.drain(..).enumerate() {
                    let pattern: Option<String> = if i == 0 {
                        None
                    } else {
                        match &arg {
                            Arg::Glob(p) => Some(p.clone()),
                            Arg::VarGlob(segs) => {
                                match expand_varglob(segs, &*self.env, &self.limits.var_allowlist) {
                                    Ok(p) => Some(p),
                                    Err((target, e)) => {
                                        return Ok(deny(
                                            sandbox_kind,
                                            enforcement,
                                            DenialKind::Exec,
                                            &target,
                                            &e,
                                        ));
                                    }
                                }
                            }
                            _ => None,
                        }
                    };
                    match pattern {
                        Some(p) => {
                            let mut leash = |dir: &Path| cx.check_path_read(dir);
                            match expand_glob_walk(
                                &p,
                                parsed.cwd.as_deref(),
                                &*self.lister,
                                &mut leash,
                                self.limits.max_glob_depth,
                                self.limits.max_glob_matches,
                            ) {
                                Ok(ms) => new_argv.extend(ms.into_iter().map(Arg::Lit)),
                                Err(e) => {
                                    return Ok(deny(
                                        sandbox_kind,
                                        enforcement,
                                        DenialKind::Open,
                                        &p,
                                        &e,
                                    ));
                                }
                            }
                        }
                        None => new_argv.push(arg),
                    }
                }
                stage.argv = new_argv;
                for redirect in &mut stage.redirects {
                    let segs = match redirect {
                        Redirect::Stdout { path, .. }
                        | Redirect::Stderr { path, .. }
                        | Redirect::Stdin { path } => path,
                        Redirect::StderrToStdout => continue,
                    };
                    match expand_redirect_target(segs, &*self.env, &self.limits.var_allowlist) {
                        Ok(resolved) => *segs = vec![Seg::Lit(resolved)],
                        Err((target, e)) => {
                            return Ok(deny(
                                sandbox_kind,
                                enforcement,
                                DenialKind::Open,
                                &target,
                                &e,
                            ));
                        }
                    }
                }
            }
        }

        // Atomic admission (ADR 0001): across the WHOLE script, every program
        // (`exec`), every redirect target (`fs_write`/`fs_read`), and every glob's
        // listed directory (`fs_read`) — all filesystem touches bridle performs —
        // must pass *before any stage spawns*. One out-of-scope element denies the
        // whole script with no partial side effects.
        for item in &script {
            for stage in &item.pipeline {
                match stage.argv.first() {
                    Some(Arg::Lit(program)) => {
                        if let Err(e) = cx.check_exec(program) {
                            return Ok(deny(
                                sandbox_kind,
                                enforcement,
                                DenialKind::Exec,
                                program,
                                &e,
                            ));
                        }
                    }
                    Some(Arg::Glob(pattern)) => {
                        return Ok(deny(
                            sandbox_kind,
                            enforcement,
                            DenialKind::Exec,
                            pattern,
                            &ToolError::denied("a glob pattern is not allowed as a program name"),
                        ));
                    }
                    Some(Arg::Var(_segs)) => {
                        return Ok(deny(
                            sandbox_kind,
                            enforcement,
                            DenialKind::Exec,
                            "$VAR",
                            &ToolError::denied("a variable is not allowed as a program name"),
                        ));
                    }
                    // A glob+var word lowers to `Arg::Glob` above; this arm is for
                    // exhaustiveness and mirrors the glob-program refusal.
                    Some(Arg::VarGlob(_)) => {
                        return Ok(deny(
                            sandbox_kind,
                            enforcement,
                            DenialKind::Exec,
                            "$VAR/glob",
                            &ToolError::denied("a glob pattern is not allowed as a program name"),
                        ));
                    }
                    None => {} // the parser guarantees a non-empty stage
                }
                for arg in &stage.argv {
                    match arg {
                        // Every variable referenced must be on the env allowlist
                        // (no secret leak), checked by name before any spawn.
                        Arg::Var(segs) => {
                            for seg in segs {
                                if let Seg::Var(name) = seg {
                                    if !is_allowed_var(name, &self.limits.var_allowlist) {
                                        return Ok(deny(
                                            sandbox_kind,
                                            enforcement,
                                            DenialKind::Exec,
                                            &format!("${name}"),
                                            &ToolError::denied(format!(
                                                "variable ${name} is not in the confined shell's allowlist"
                                            )),
                                        ));
                                    }
                                }
                            }
                        }
                        // Globs / glob+var words were expanded to literals (with
                        // the per-directory fs_read leash) in the pass above.
                        Arg::Glob(_) => unreachable!("glob expanded at admission"),
                        Arg::VarGlob(_) => unreachable!("VarGlob expanded at admission"),
                        Arg::Lit(_) => {}
                    }
                }
                for redirect in &stage.redirects {
                    // Redirect targets were lowered above, so each path is a
                    // single resolved literal — leash-check that resolved path.
                    let (path, checked) = match redirect {
                        Redirect::Stdout { path, .. } | Redirect::Stderr { path, .. } => {
                            let p = seg_literal(path).expect("redirect target lowered");
                            (p, cx.check_path_write(Path::new(p)))
                        }
                        Redirect::Stdin { path } => {
                            let p = seg_literal(path).expect("redirect target lowered");
                            (p, cx.check_path_read(Path::new(p)))
                        }
                        // `2>&1` opens no file — nothing to leash-check.
                        Redirect::StderrToStdout => continue,
                    };
                    if let Err(e) = checked {
                        return Ok(deny(sandbox_kind, enforcement, DenialKind::Open, path, &e));
                    }
                }
            }
        }
        // Leash: a provided cwd must be within fs_read scope.
        if let Some(cwd) = &parsed.cwd {
            if let Err(e) = cx.check_path_read(Path::new(cwd)) {
                return Ok(deny(sandbox_kind, enforcement, DenialKind::Open, cwd, &e));
            }
        }

        // Fail closed (ADR 0012 D4) — AFTER L2 admission (so a specific
        // out-of-scope glob/redirect/exec denial is reported first) but before any
        // spawn: require the same ruleset-grain authority admission used by
        // `ConfinedCommand`. Strength reporting alone is insufficient: a backend
        // may honestly report Advisory while its conservative authority projection
        // is Unknown, which must refuse independently of the principal's floor.
        // Project the exact caveats `OsSpawner` will apply (including the egress
        // proxy's loopback fence), so the admission and wrapper cannot equivocate.
        // Unbridled skips this fail-closed guard by consent: dropping the L3
        // mechanism is *exactly* what the operator acknowledged (ADR 0018 D1). The
        // L2 grant checks above still ran (advisory), and every axis reports
        // advisory + `disclosure.unbridled` — honest, not silent.
        if !unbridled {
            // Production spawners must pass backend admission even when no native
            // sandbox engages. The no-op backend projects unbounded authority, so
            // a restricted grant is refused here instead of spawning fail-open.
            if self.spawner.requires_backend_admission() {
                let sandbox = best_available_sandbox(&self.sandbox);
                // `Unaudited`: this route builds each pipeline stage's stdio with
                // raw `std::process::Stdio` (file redirects, OS pipes between
                // stages), never `agent_bridle_core::ConfinedStdio` — so it is
                // never the "only production caller with actual, per-channel
                // knowledge of what it configured" `StdioPosture::Audited` requires
                // (report.rs's `ConfinementMechanism::with_stdio_posture` doc
                // comment; `ConfinedCommand::spawn` is that caller, this is not).
                // Passing anything else here would assert a per-channel audit this
                // builder genuinely cannot vouch for (agent-bridle#416 round 3).
                let mut resolved =
                    sandbox.resolved_authority(&backend_caveats, StdioPosture::Unaudited);
                // AppContainer cannot express a non-empty exec allowlist in its
                // native policy, but this route has already atomically checked
                // every pipeline stage through the ShellTool exec interceptor.
                // Intersect that route-local bound with the active AppContainer
                // projection without changing the backend's native claim or its
                // runtime closure. The EFFECTIVE-kind guard is load-bearing:
                // exec-only caveats do not engage AppContainer and must retain the
                // no-backend Unbounded refusal rather than borrow this interceptor.
                if sandbox_kind == SandboxKind::AppContainer
                    && matches!(
                        &backend_caveats.exec,
                        Scope::Only(programs) if !programs.is_empty()
                    )
                {
                    resolved.exec = ResolvedScope::from_scope(&backend_caveats.exec);
                }
                let projection = BackendProjection {
                    resolved,
                    runtime_closure: sandbox.runtime_closure(&backend_caveats),
                };
                let rejected_axis = match admit(
                    &projection.resolved,
                    &backend_caveats,
                    &projection.runtime_closure,
                ) {
                    AdmissionDecision::Admit => None,
                    AdmissionDecision::Reject(reject) => Some(reject.axis),
                };

                if let Err(error) = AdmittedFence::admit(
                    &backend_caveats,
                    RuntimeClosure::empty(),
                    mechanism,
                    cx.strength_floor(),
                    |_| projection,
                ) {
                    let rejected_axis = rejected_axis.or_else(|| {
                        unenforceable_axis(cx.caveats(), mechanism, cx.strength_floor())
                            .map(|unmet| unmet.axis)
                    });
                    let (kind, target) = match rejected_axis {
                        Some(ConfinedAxis::FsRead | ConfinedAxis::FsWrite) => {
                            (DenialKind::Open, "filesystem")
                        }
                        Some(ConfinedAxis::Net) => (DenialKind::Net, "network"),
                        Some(ConfinedAxis::Exec) | None => (DenialKind::Exec, "confinement"),
                    };
                    return Ok(deny(sandbox_kind, enforcement, kind, target, &error));
                }
            }

            if let Some(unmet) = unenforceable_axis(cx.caveats(), mechanism, cx.strength_floor()) {
                let (kind, target) = match unmet.axis {
                    ConfinedAxis::FsRead | ConfinedAxis::FsWrite => {
                        (DenialKind::Open, "filesystem")
                    }
                    ConfinedAxis::Net => (DenialKind::Net, "network"),
                    ConfinedAxis::Exec => (DenialKind::Exec, "confinement"),
                };
                return Ok(deny(
                    sandbox_kind,
                    enforcement,
                    kind,
                    target,
                    &ToolError::denied(format!("{unmet}; refusing to run unconfined")),
                ));
            }
        }

        // All pipelines share one clock, including time queued before the
        // blocking owner starts. Timeout cancels and joins that owner.
        let spawner = Arc::clone(&self.spawner);
        let cwd = parsed.cwd.clone();
        let timeout = parsed.timeout;
        let deadline = crate::supervisor::Deadline::new(timeout);
        let (output_guard, output) =
            output_session(self.output_observer.clone(), self.limits.max_output_bytes);
        let cfg = SpawnCfg {
            max_output: self.limits.max_output_bytes,
            audit_sink: self.limits.audit_sink.clone(),
            sandbox: Arc::clone(&self.sandbox),
            held_read_roots: self.held_read_roots.clone(),
            private_hosts: self.private_hosts.clone(),
            unbridled,
            output,
            deadline: deadline.clone(),
        };
        // Disclosed on every envelope this run returns (ADR 0018 D5/D11 / I11).
        let disclosure = Disclosure {
            unbridled,
            human_gate: human_gate(),
            ..Disclosure::default()
        };
        // Host/operator-supplied environment (the env seam, newt #783): carried
        // through to the child processes. Empty when the dispatch omits `env`.
        // AB-004: strip loader/interpreter/hook vars (LD_PRELOAD, PYTHONPATH,
        // GIT_SSH_COMMAND, BASH_ENV, …) before they reach ANY engine — they
        // hijack what an allowed program actually executes, regardless of the
        // exec leash. Fenced once here so all three engines get a clean env.
        let (env, _dropped_env) =
            agent_bridle_core::fence_env(&parsed.env, &self.limits.env_denylist);
        let caveats = cx.caveats().clone();
        for root in &self.held_read_roots {
            if !matches!(&caveats.fs_read, Scope::All)
                && !matches!(&caveats.fs_read, Scope::Only(paths) if paths.contains(root.provenance()))
            {
                return Err(ToolError::denied("held read root is not admitted"));
            }
        }
        if !self.held_read_roots.is_empty()
            && best_available_sandbox(&self.sandbox).kind() != SandboxKind::Landlock
        {
            return Err(ToolError::denied(
                "this sandbox cannot anchor held read roots",
            ));
        }
        let execution_lease = self.execution_lease.clone();
        let mut run = tokio::task::spawn_blocking(move || {
            let _execution_lease = execution_lease;
            run_script(&*spawner, &script, cwd.as_deref(), &caveats, &env, &cfg)
        });
        let mut output_guard = Some(output_guard);
        let (joined, outer_timeout) = match tokio::time::timeout(timeout, &mut run).await {
            Ok(joined) => (joined, false),
            Err(_) => {
                deadline.cancel();
                drop(output_guard.take());
                // Do not report termination while the blocking script can still
                // spawn. Its shared deadline kills/reaps active groups first.
                (run.await, true)
            }
        };
        let mut captured =
            joined.map_err(|e| ToolError::Other(anyhow::anyhow!("shell task panicked: {e}")))??;
        captured.timed_out |= outer_timeout;
        if captured.timed_out {
            captured.exit_code = 124;
            captured.stderr = cap_string(
                format!(
                    "command timed out after {}s\n{}",
                    timeout.as_secs(),
                    captured.stderr
                ),
                self.limits.max_output_bytes,
            );
        }
        let envelope = ToolEnvelope::new(sandbox_kind)
            .with_enforcement(enforcement)
            .with_disclosure(disclosure)
            .with_exit_code(captured.exit_code)
            .with_truncation(captured.stdout_truncated, captured.stderr_truncated)
            .with_stdout(captured.stdout)
            .with_stderr(captured.stderr)
            .with_denials(captured.net_denials)
            .with_timed_out(captured.timed_out)
            .into_json();
        if !captured.timed_out {
            if let Some(guard) = output_guard {
                guard.finish();
            }
        }
        Ok(Invocation::Ran(envelope))
    }
}

/// Execute a [`Script`] with `&&`/`||`/`;` short-circuit semantics, concatenating
/// the output of the pipelines that actually run. The script's exit code is that
/// of the last pipeline that ran (bash AND-OR-list semantics).
fn run_script(
    spawner: &dyn Spawner,
    script: &[ScriptItem],
    cwd: Option<&str>,
    caveats: &Caveats,
    env: &BTreeMap<String, String>,
    cfg: &SpawnCfg,
) -> ToolResult<Captured> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut status: i32 = 0;
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    // #196: net denials accumulate across every pipeline stage that runs.
    let mut net_denials: Vec<Denial> = Vec::new();
    let mut timed_out = false;

    for item in script {
        if cfg.deadline.remaining().is_zero() {
            timed_out = true;
            status = 124;
            break;
        }
        let run_it = match item.sep {
            Sep::Seq => true,
            Sep::And => status == 0,
            Sep::Or => status != 0,
        };
        if run_it {
            let captured = spawner.run(&item.pipeline, cwd, caveats, env, cfg)?;
            stdout.push_str(&captured.stdout);
            stderr.push_str(&captured.stderr);
            stdout_truncated |= captured.stdout_truncated;
            stderr_truncated |= captured.stderr_truncated;
            net_denials.extend(captured.net_denials);
            status = captured.exit_code;
            // A pipeline that hit its deadline was killed + reaped; stop the
            // script there rather than starting further work past the deadline.
            if captured.timed_out || cfg.deadline.remaining().is_zero() {
                status = 124;
                timed_out = true;
                break;
            }
        }
    }

    // The concatenation across pipelines may itself exceed the cap; flag that.
    let stdout_truncated = stdout_truncated || stdout.len() > cfg.max_output;
    let stderr_truncated = stderr_truncated || stderr.len() > cfg.max_output;

    Ok(Captured {
        exit_code: status,
        stdout: cap_string(stdout, cfg.max_output),
        stderr: cap_string(stderr, cfg.max_output),
        net_denials,
        stdout_truncated,
        stderr_truncated,
        timed_out,
    })
}

/// Build a structured `denied` envelope for a leash refusal. Returned as an
/// [`Invocation::Denied`]: this is a **pre-execution** refusal (nothing ran), so
/// the registry charges zero calls even though the envelope surfaces to the
/// caller as an ordinary `Ok` value.
fn deny(
    sandbox_kind: SandboxKind,
    enforcement: EnforcementReport,
    kind: DenialKind,
    target: &str,
    err: &ToolError,
) -> Invocation {
    Invocation::Denied(
        ToolEnvelope::new(sandbox_kind)
            .with_enforcement(enforcement)
            .with_disclosure(unbridle_disclosure())
            .with_denials(vec![Denial {
                kind,
                target: target.to_string(),
                reason: err.to_string(),
            }])
            .into_json(),
    )
}

/// The disclosure block stamped on **every** envelope (ADR 0018 D5): reads the
/// process-level unbridle marker so a denied/refused result is as honest about
/// the posture as a successful one.
fn unbridle_disclosure() -> Disclosure {
    Disclosure {
        unbridled: is_unbridled(),
        human_gate: human_gate(),
        ..Disclosure::default()
    }
}

/// Build a structured `denied` envelope for a parser [`Refusal`]. Like
/// [`deny`], this is a **pre-execution** refusal, so it is returned as an
/// [`Invocation::Denied`] (charges zero calls).
fn refused_envelope(
    sandbox_kind: SandboxKind,
    enforcement: EnforcementReport,
    refusal: &Refusal,
    cmd: Option<&str>,
) -> Invocation {
    let envelope = ToolEnvelope::new(sandbox_kind)
        .with_enforcement(enforcement)
        .with_disclosure(unbridle_disclosure())
        .with_denials(vec![Denial {
            kind: DenialKind::Exec,
            target: refusal.construct(),
            reason: refusal.to_string(),
        }])
        .into_json();

    // A dynamic safe-subset refusal is a parser/mechanism boundary, not an
    // executable denial. When the carried Brush parser is present, attach its
    // pure source inspection so an embedder can review an exact source string
    // and a flattened, source-bound inventory before selecting a full-grammar
    // engine. Inspection performs no expansion or execution; failure simply
    // retains the legacy fail-closed envelope.
    #[cfg(feature = "brush")]
    {
        let mut envelope = envelope;
        if matches!(refusal, Refusal::Dynamic(_)) {
            if let Some(cmd) = cmd {
                if let Ok(inspection) = crate::inspect_shell(cmd) {
                    if let Ok(value) = serde_json::to_value(inspection) {
                        envelope["shell_inspection"] = value;
                    }
                }
            }
        }
        Invocation::Denied(envelope)
    }
    #[cfg(not(feature = "brush"))]
    {
        let _ = cmd;
        Invocation::Denied(envelope)
    }
}

/// Parsed, validated `shell` arguments.
struct ShellArgs {
    program: Option<String>,
    args: Vec<String>,
    cmd: Option<String>,
    cwd: Option<String>,
    /// Host/operator-supplied environment for the spawned child(ren) (the env
    /// seam, newt #783). Empty when the dispatch omits `env` (back-compat). Only
    /// string values are taken; non-string entries are ignored.
    env: BTreeMap<String, String>,
    timeout: Duration,
}

impl ShellArgs {
    fn parse(v: &serde_json::Value, limits: &LimitsPolicy) -> ToolResult<Self> {
        let obj = v
            .as_object()
            .ok_or_else(|| ToolError::denied("shell args must be a JSON object"))?;

        let program = obj
            .get("program")
            .and_then(|x| x.as_str())
            .map(String::from);
        let cmd = obj.get("cmd").and_then(|x| x.as_str()).map(String::from);
        let args = obj
            .get("args")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let cwd = obj.get("cwd").and_then(|x| x.as_str()).map(String::from);
        // The env seam (newt #783): a `"env": { "KEY": "VALUE", … }` object whose
        // string values are set on the spawned child(ren). Absent → empty map
        // (back-compat). Non-string values are dropped (the schema is string-only).
        let env = obj
            .get("env")
            .and_then(|x| x.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect::<BTreeMap<String, String>>()
            })
            .unwrap_or_default();
        let timeout_secs = obj
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(limits.default_timeout_secs)
            .clamp(1, limits.max_timeout_secs);

        match (&program, &cmd) {
            (Some(_), Some(_)) => {
                return Err(ToolError::denied(
                    "provide exactly one of `program` or `cmd`, not both",
                ));
            }
            (None, None) => return Err(ToolError::denied("provide one of `program` or `cmd`")),
            _ => {}
        }
        if program.is_none() && !args.is_empty() {
            return Err(ToolError::denied(
                "`args` may only be used together with `program`",
            ));
        }

        Ok(Self {
            program,
            args,
            cmd,
            cwd,
            env,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    /// Resolve to a script. Argv form is a one-pipeline, one-stage script whose
    /// args are all **literal** (no globbing/parsing); free-form is parsed by the
    /// safe-subset engine.
    fn script(&self) -> Result<Script, Refusal> {
        if let Some(program) = &self.program {
            let mut argv = Vec::with_capacity(1 + self.args.len());
            argv.push(Arg::Lit(program.clone()));
            argv.extend(self.args.iter().cloned().map(Arg::Lit));
            Ok(vec![ScriptItem {
                sep: Sep::Seq,
                pipeline: vec![Command {
                    argv,
                    redirects: Vec::new(),
                }],
            }])
        } else {
            classify(self.cmd.as_deref().unwrap_or(""))
        }
    }
}

// ── variable expansion (allowlist) ──────────────────────────────────────────

/// The environment variables the confined engine will expand (ADR 0005 D3,
/// allowlist-only). Deliberately small and secret-free: no `PATH`, no tokens.
/// A `$VAR` outside this set is denied — so a confined run can never splice a
/// secret (e.g. `$AWS_SECRET_KEY`) into an argument, even when `exec` is tight.
/// Whether `name` may be expanded from the environment, against the configured
/// allowlist ([`LimitsPolicy::var_allowlist`]).
fn is_allowed_var(name: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|v| v == name)
}

/// The environment seam (#46): the engine reads `$VAR` values through this, so the
/// allowlist + expansion + the resolved-path `fs` leash stay unit-testable
/// without touching the real process environment (a fake map in tests). Only
/// allowlisted names (the configured `var_allowlist`) are ever read.
pub(crate) trait EnvProvider: Send + Sync {
    /// The value of `name`, or `None` if unset.
    fn get(&self, name: &str) -> Option<String>;
}

/// The real process environment (`std::env::var`).
pub(crate) struct RealEnv;
impl EnvProvider for RealEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Expand a redirect target's segments to a literal path, reading allowlisted
/// `$VAR` through the env seam. Single-literal substitution: the value is **not**
/// re-split or re-globbed (no re-injection). `Err((target, reason))` names a
/// non-allowlisted variable for a structured denial.
fn expand_redirect_target(
    segs: &[Seg],
    env: &dyn EnvProvider,
    allowlist: &[String],
) -> Result<String, (String, ToolError)> {
    let mut out = String::new();
    for seg in segs {
        match seg {
            Seg::Lit(s) => out.push_str(s),
            Seg::Var(name) => {
                if !is_allowed_var(name, allowlist) {
                    return Err((
                        format!("${name}"),
                        ToolError::denied(format!(
                            "variable ${name} is not in the confined shell's allowlist"
                        )),
                    ));
                }
                out.push_str(&env.get(name).unwrap_or_default());
            }
        }
    }
    Ok(out)
}

/// Expand a glob+variable word (e.g. `$DIR/*.rs`) into a resolved glob pattern,
/// reading allowlisted `$VAR` through the env seam.
///
/// **Re-injection guard:** a variable may only contribute to the directory
/// *prefix* (everything up to the last `/`), never to the glob *basename* — so a
/// var value can never inject a glob metachar that widens the match. The existing
/// single-segment globber then treats the (var-derived) directory as a literal
/// path and globs only the source-literal basename. A variable in the basename is
/// refused. `Err((target, reason))` names a non-allowlisted var or the refusal.
fn expand_varglob(
    segs: &[Seg],
    env: &dyn EnvProvider,
    allowlist: &[String],
) -> Result<String, (String, ToolError)> {
    let mut out = String::new();
    let mut last_var_byte: Option<usize> = None; // byte index of the last var-origin char
    let mut last_slash_byte: Option<usize> = None; // byte index of the last '/'
    for seg in segs {
        match seg {
            Seg::Lit(s) => {
                for ch in s.chars() {
                    if ch == '/' {
                        last_slash_byte = Some(out.len());
                    }
                    out.push(ch);
                }
            }
            Seg::Var(name) => {
                if !is_allowed_var(name, allowlist) {
                    return Err((
                        format!("${name}"),
                        ToolError::denied(format!(
                            "variable ${name} is not in the confined shell's allowlist"
                        )),
                    ));
                }
                for ch in env.get(name).unwrap_or_default().chars() {
                    if ch == '/' {
                        last_slash_byte = Some(out.len());
                    }
                    last_var_byte = Some(out.len());
                    out.push(ch);
                }
            }
        }
    }
    // A var char in the basename (at/after the char following the last '/') could
    // inject a glob metachar from its value — refuse (re-injection guard).
    let basename_start = last_slash_byte.map_or(0, |i| i + 1);
    if last_var_byte.is_some_and(|v| v >= basename_start) {
        return Err((
            "$VAR".to_string(),
            ToolError::denied(
                "a variable in a glob's basename is not supported (re-injection guard); \
                 put the variable in the directory prefix, e.g. $DIR/*.rs",
            ),
        ));
    }
    Ok(out)
}

// ── glob expansion (multi-segment + recursive `**`) ─────────────────────────

/// One directory entry the glob walker sees: a name and whether it is a directory
/// (needed to recurse for `**`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlobEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Lists a directory's entries — the filesystem seam for the glob walker, so unit
/// tests drive multi-segment / `**` expansion without a real filesystem (#47).
pub(crate) trait DirLister: Send + Sync {
    /// The entries of `dir` (names + is-dir), or empty if it cannot be read.
    fn list(&self, dir: &Path) -> Vec<GlobEntry>;
}

/// The real filesystem lister.
pub(crate) struct RealDirLister;
impl DirLister for RealDirLister {
    fn list(&self, dir: &Path) -> Vec<GlobEntry> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| {
                    let e = e.ok()?;
                    let name = e.file_name().into_string().ok()?;
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    Some(GlobEntry { name, is_dir })
                })
                .collect()
            })
            .unwrap_or_default()
    }
}

/// Append `name` to the result path `rel`, preserving the pattern's form
/// (relative vs absolute).
fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else if rel == "/" {
        format!("/{name}")
    } else {
        format!("{rel}/{name}")
    }
}

/// Collect every descendant directory of `(real, rel)` (bounded depth),
/// leash-checking + listing each — the `**` expansion. Hidden directories are not
/// descended (bash globstar default).
fn descend_all(
    real: &Path,
    rel: &str,
    list: &dyn DirLister,
    leash: &mut dyn FnMut(&Path) -> ToolResult<()>,
    depth: usize,
    max_matches: usize,
    out: &mut Vec<(PathBuf, String)>,
) -> ToolResult<()> {
    if depth == 0 || out.len() >= max_matches {
        return Ok(());
    }
    leash(real)?;
    let mut entries = list.list(real);
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries {
        if e.is_dir && !e.name.starts_with('.') {
            let child_real = real.join(&e.name);
            let child_rel = join_rel(rel, &e.name);
            out.push((child_real.clone(), child_rel.clone()));
            if out.len() >= max_matches {
                break;
            }
            descend_all(
                &child_real,
                &child_rel,
                list,
                leash,
                depth - 1,
                max_matches,
                out,
            )?;
        }
    }
    Ok(())
}

/// Expand a glob pattern (multi-segment and recursive `**`) against the
/// filesystem via `list`, **leash-checking every directory before listing it**
/// (`leash`) — so the whole walk stays within `fs_read` scope, *before any stage
/// spawns* (atomic admission). Per-component matching uses [`fnmatch`]
/// (`*`/`?`/`[…]` do not cross `/`); `**` matches zero or more directory levels.
/// Bounded by depth + match count. nullglob-off: no match → the literal pattern.
/// A `leash` `Err` (an out-of-scope directory) propagates and denies the command.
fn expand_glob_walk(
    pattern: &str,
    cwd: Option<&str>,
    list: &dyn DirLister,
    leash: &mut dyn FnMut(&Path) -> ToolResult<()>,
    max_depth: usize,
    max_matches: usize,
) -> ToolResult<Vec<String>> {
    let absolute = pattern.starts_with('/');
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();

    let base_real = if absolute {
        PathBuf::from("/")
    } else {
        cwd.map_or_else(|| PathBuf::from("."), PathBuf::from)
    };
    let base_rel = if absolute {
        "/".to_string()
    } else {
        String::new()
    };
    let mut frontier: Vec<(PathBuf, String)> = vec![(base_real, base_rel)];

    for seg in &segments {
        let mut next: Vec<(PathBuf, String)> = Vec::new();
        if *seg == "**" {
            for (real, rel) in &frontier {
                next.push((real.clone(), rel.clone())); // `**` matches zero levels too
                descend_all(real, rel, list, leash, max_depth, max_matches, &mut next)?;
            }
        } else {
            let seg_hidden = seg.starts_with('.');
            for (real, rel) in &frontier {
                leash(real)?;
                let mut entries = list.list(real);
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                for e in entries {
                    if (seg_hidden || !e.name.starts_with('.')) && fnmatch(seg, &e.name) {
                        next.push((real.join(&e.name), join_rel(rel, &e.name)));
                        if next.len() >= max_matches {
                            break;
                        }
                    }
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }

    let mut matches: Vec<String> = frontier.into_iter().map(|(_, rel)| rel).collect();
    matches.retain(|m| !m.is_empty()); // drop the "zero-levels" cwd self-match
    matches.sort();
    matches.dedup();
    if matches.is_empty() {
        Ok(vec![pattern.to_string()])
    } else {
        Ok(matches)
    }
}

/// Glob match: `*` (any run), `?` (one char), `[…]` (class with ranges and
/// `!`/`^` negation). `*`/`?`/`[` do not cross `/` (single-segment matching).
fn fnmatch(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fnmatch_inner(&p, &n)
}

fn fnmatch_inner(p: &[char], n: &[char]) -> bool {
    match p.first() {
        None => n.is_empty(),
        Some('*') => fnmatch_inner(&p[1..], n) || (!n.is_empty() && fnmatch_inner(p, &n[1..])),
        Some('?') => !n.is_empty() && fnmatch_inner(&p[1..], &n[1..]),
        Some('[') => {
            if n.is_empty() {
                return false;
            }
            match match_class(&p[1..], n[0]) {
                Some((matched, rest)) => matched && fnmatch_inner(rest, &n[1..]),
                // Malformed class (no closing `]`): treat `[` as a literal.
                None => n[0] == '[' && fnmatch_inner(&p[1..], &n[1..]),
            }
        }
        Some(&c) => !n.is_empty() && c == n[0] && fnmatch_inner(&p[1..], &n[1..]),
    }
}

/// Match a `[...]` class against `c`. `p` begins just after `[`. Returns
/// `(matched, remaining pattern after ])`, or `None` if there is no closing `]`.
fn match_class(p: &[char], c: char) -> Option<(bool, &[char])> {
    let mut i = 0;
    let negate = matches!(p.first(), Some('!' | '^'));
    if negate {
        i = 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((matched ^ negate, &p[i + 1..]));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if c >= p[i] && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if c == p[i] {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

// ── process execution ───────────────────────────────────────────────────────

/// Kill (and reap) any stages already spawned, so a mid-pipeline error does not
/// orphan processes.
fn kill_all(children: &mut [Child]) {
    for child in children.iter_mut() {
        crate::kill_child_tree(child);
        let _ = child.wait();
    }
}

/// Lower a stage's [`Arg`] list into a concrete argv: literals as-is, globs
/// expanded against the real filesystem, and (allowlisted) variables read from
/// the environment as a single literal (no re-split / no re-glob of the value).
/// The allowlist is enforced earlier in [`ShellTool::invoke`].
fn expand_stage_argv(stage: &Command, _cwd: Option<&str>) -> Vec<String> {
    let mut argv = Vec::with_capacity(stage.argv.len());
    for arg in &stage.argv {
        match arg {
            Arg::Lit(s) => argv.push(s.clone()),
            // Concatenate the segments: literals as-is, variables (already
            // allowlisted in `invoke`) read from the env as a single literal —
            // no re-split / no re-glob of the value.
            Arg::Var(segs) => {
                let mut word = String::new();
                for seg in segs {
                    match seg {
                        Seg::Lit(s) => word.push_str(s),
                        Seg::Var(name) => word.push_str(&std::env::var(name).unwrap_or_default()),
                    }
                }
                argv.push(word);
            }
            // Globs (and glob+var words) are expanded to literal matches at
            // admission (with the per-directory fs_read leash), so the spawner
            // never sees them.
            Arg::Glob(_) => unreachable!("glob expanded at admission"),
            Arg::VarGlob(_) => unreachable!("VarGlob lowered/expanded at admission"),
        }
    }
    argv
}

/// Spawn a pipeline of commands wired with OS pipes and file redirections,
/// capturing the last stage's stdout (unless it is redirected to a file) and
/// every stage's stderr. The pipeline's exit code is the last stage's (bash
/// semantics without `pipefail`).
///
/// Deadlock-free: every stage's stderr and the last stage's stdout are drained by
/// their own threads, so no pipe can fill while we `wait()` the children.
///
/// `wrap` is the OS-sandbox command prefix (macOS Seatbelt's `sandbox-exec -p
/// <profile>`), prepended to **every** stage so each spawned program is confined;
/// it is empty for thread-confining (Landlock) and unconfined runs.
#[allow(clippy::too_many_arguments)] // house precedent (shell_inspect/gate): flat args over a one-off bag
fn run_pipeline(
    stages: &[Command],
    cwd: Option<&str>,
    wrap: &[String],
    env: &BTreeMap<String, String>,
    max_output: usize,
    output: OutputEmitter,
    deadline: &crate::supervisor::Deadline,
    // #351: the effective fs axes bound the PARENT-side redirect opens below —
    // `open_scoped_*` resolves-and-opens beneath the granted roots in one
    // kernel-checked step, so a component swapped for a symlink after the leash
    // check cannot steer the open (the check→open TOCTOU).
    fs_read: &Scope<String>,
    fs_write: &Scope<String>,
) -> ToolResult<Captured> {
    debug_assert!(!stages.is_empty(), "the parser guarantees ≥1 stage");
    let n = stages.len();
    let last = n - 1;

    let mut children: Vec<Child> = Vec::with_capacity(n);
    // The read end feeding the NEXT stage's stdin (from the prior stage's stdout).
    let mut prev_stdin: Option<PipeReader> = None;
    // The read end capturing final stdout (last stage, when not redirected).
    let mut stdout_capture: Option<PipeReader> = None;
    // Reader threads for stages whose stderr is captured separately. Each yields
    // (captured bytes ≤ cap, truncated?).
    let mut stderr_threads: Vec<std::thread::JoinHandle<(Vec<u8>, bool)>> = Vec::new();

    for (i, stage) in stages.iter().enumerate() {
        if deadline.remaining().is_zero() {
            break;
        }
        let is_last = i == last;
        let stage_argv = expand_stage_argv(stage, cwd);
        // Prepend the sandbox wrapper (Seatbelt) so the program is spawned
        // confined: `sandbox-exec -p <profile> <program> <args…>`. Empty wrap is
        // the identity. `sandbox-exec` forwards stdio + cwd to the child, so the
        // pipe/redirect plumbing below is unchanged.
        let argv: Vec<String> = if wrap.is_empty() {
            stage_argv
        } else {
            wrap.iter().cloned().chain(stage_argv).collect()
        };
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        // Each stage leads its own process group so a timeout can SIGKILL the
        // whole group — stage plus any descendants (AB-006, #269).
        #[cfg(unix)]
        cmd.process_group(0);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // Host/operator-supplied environment (the env seam, newt #783): set the
        // provided vars on the child, additive over the inherited ambient env.
        // The values are structured host input (never model-authored command
        // text), so they grant no new authority — the exec/fs leash that already
        // admitted this stage checked the *real* program (argv[0]), not env. When
        // a Seatbelt `wrap` prefix is present, `sandbox-exec` forwards its own
        // environment to the wrapped program, so setting it here still reaches the
        // confined child.
        // AB-016 / #323: do NOT inherit the parent's ambient environment (which
        // may carry provider secrets — `OPENAI_API_KEY` etc.) — start empty with a
        // fixed, minimal baseline, then apply only the (already loader-fenced)
        // caller env. Brings this engine to the env_clear posture the Brush
        // (`do_not_inherit_env`) and Host (`ConfinedCommand`) engines already have.
        //
        // #323 (a 0.8 blocker): this was Unix-only, so on Windows the ambient env
        // leaked all the way to the AppContainer child via
        // `dispatch_bridled_shell → ShellTool → aclaunch → AppContainer` (aclaunch
        // forwards its own env, exactly as Seatbelt's `sandbox-exec` does). Clear on
        // every platform; the child gets the baseline + only the delegated vars.
        cmd.env_clear();
        cmd.env("PATH", agent_bridle_core::default_exec_path());
        #[cfg(unix)]
        cmd.env("LC_ALL", "C");
        #[cfg(windows)]
        {
            // Minimal Windows execution baseline — well-known non-secret platform
            // paths, NOT ambient authority (a provider secret like `OPENAI_API_KEY`
            // is never in this set):
            //   - `SystemRoot`   — process / DLL initialisation.
            //   - `LOCALAPPDATA` — the AppContainer runtime's per-profile storage /
            //     redirection root. WITHOUT it, `CreateProcessW` for an AppContainer
            //     child fails with `ERROR_ENVVAR_NOT_FOUND (203)` — so clearing the
            //     env without it would break every confined Windows spawn.
            //   - `PATHEXT`      — executable resolution.
            for var in ["SystemRoot", "LOCALAPPDATA"] {
                if let Some(v) = std::env::var_os(var) {
                    cmd.env(var, v);
                }
            }
            cmd.env(
                "PATHEXT",
                std::env::var_os("PATHEXT")
                    .unwrap_or_else(|| std::ffi::OsString::from(".COM;.EXE;.BAT;.CMD")),
            );
        }
        for (k, v) in env {
            cmd.env(k, v);
        }

        // ── stdin: a `< file` redirect wins over the incoming pipe ──────────
        if let Some(path) = stage.stdin_path() {
            // #351: bounded open beneath the granted fs_read roots.
            let file = ok_or_kill_tool(
                agent_bridle_core::open_scoped_read(fs_read, Path::new(path)),
                &mut children,
            )?;
            cmd.stdin(Stdio::from(file));
            prev_stdin = None;
        } else {
            cmd.stdin(match prev_stdin.take() {
                Some(reader) => Stdio::from(reader),
                None => Stdio::null(),
            });
        }

        // ── stdout (+ the handle stderr clones for `2>&1`) ──────────────────
        // A `> file` redirect goes to the file; otherwise a `std::io::pipe()` is
        // used so its writer can be cloned for `2>&1` in any position.
        let dup_source: DupSource;
        if let Some((path, append)) = stage.stdout_redirect() {
            // #351: bounded open beneath the granted fs_write roots.
            let file = ok_or_kill_tool(
                agent_bridle_core::open_scoped_write(fs_write, Path::new(path), append),
                &mut children,
            )?;
            let clone = ok_or_kill(file.try_clone(), &mut children)?;
            cmd.stdout(Stdio::from(file));
            dup_source = DupSource::File(clone);
        } else {
            let (reader, writer) = ok_or_kill(std::io::pipe(), &mut children)?;
            let clone = ok_or_kill(writer.try_clone(), &mut children)?;
            cmd.stdout(Stdio::from(writer));
            if is_last {
                stdout_capture = Some(reader);
            } else {
                prev_stdin = Some(reader);
            }
            dup_source = DupSource::Pipe(clone);
        }

        // ── stderr ──────────────────────────────────────────────────────────
        match stage.stderr_disposition() {
            // `2>&1`: stderr writes to the stdout destination (the dup is moved
            // into the child; nothing captured separately).
            StderrTo::Stdout => match dup_source {
                DupSource::File(f) => {
                    cmd.stderr(Stdio::from(f));
                }
                DupSource::Pipe(w) => {
                    cmd.stderr(Stdio::from(w));
                }
            },
            // `2> file`: stderr to its own file.
            StderrTo::File { path, append } => {
                // #351: bounded open beneath the granted fs_write roots.
                let file = ok_or_kill_tool(
                    agent_bridle_core::open_scoped_write(fs_write, Path::new(&path), append),
                    &mut children,
                )?;
                cmd.stderr(Stdio::from(file));
                // `dup_source` is dropped here (unused) — never retain a writer.
            }
            // Default: capture stderr separately via a piped fd.
            StderrTo::Capture => {
                cmd.stderr(Stdio::piped());
            }
        }

        // #319: close ambient descriptors the parent left open so a confined
        // stage does not inherit un-delegated object capabilities (the descriptor
        // analog of the `env_clear` above). Encapsulated in `agent-bridle-fdguard`
        // (the single `unsafe` seam); enforced on Linux + macOS (the macOS leg
        // refuses the spawn when the descriptor universe cannot be bounded, #352),
        // no-op elsewhere. Runs
        // last so it applies regardless of the stdio disposition chosen above.
        #[cfg(unix)]
        agent_bridle_fdguard::deny_inherited_fds(&mut cmd);

        if deadline.remaining().is_zero() {
            break;
        }
        let mut child = ok_or_kill(cmd.spawn(), &mut children)?;

        if matches!(stage.stderr_disposition(), StderrTo::Capture) {
            let err = child.stderr.take().expect("stderr is piped");
            let output = output.clone();
            stderr_threads.push(std::thread::spawn(move || {
                read_capped_observed(err, max_output, &output, crate::ShellOutputStream::Stderr)
            }));
        }
        children.push(child);
    }

    // The parent now holds no pipe writers, so a captured reader sees EOF once
    // the child(ren) exit. Read stdout (bounded by the cap) concurrently with
    // waiting; a child producing past the cap is cut off via EPIPE.
    let stdout_thread = stdout_capture.map(|reader| {
        std::thread::spawn(move || {
            read_capped_observed(
                reader,
                max_output,
                &output,
                crate::ShellOutputStream::Stdout,
            )
        })
    });

    // Supervise every stage to the deadline (AB-006, #269). Poll with `try_wait`
    // (the reader threads keep the pipes draining, so no stage blocks on a full
    // buffer); on the deadline, SIGKILL each stage's process group and reap, so
    // nothing — child or descendant — outlives the timeout. The pipeline's exit
    // code is the last stage's.
    let (exit_code, timed_out) = crate::supervisor::supervise_until(&mut children, deadline)?;
    // A completed stage can leave descendants holding its output pipes.
    // End owned groups before reader joins, as in the host engine. Escaped
    // pipe holders remain outside this mechanism (agent-bridle#420).
    for child in &mut children {
        crate::kill_child_tree(child);
    }

    let (stdout, stdout_truncated) =
        stdout_thread.map_or((Vec::new(), false), |h| h.join().unwrap_or_default());
    let mut stderr = Vec::new();
    let mut stderr_truncated = false;
    for h in stderr_threads {
        let (buf, trunc) = h.join().unwrap_or_default();
        stderr.extend(buf);
        stderr_truncated |= trunc;
    }
    // Concatenated stderr across stages may itself exceed the cap; `capped_utf8`
    // clips it and we flag that too.
    let stderr_truncated = stderr_truncated || stderr.len() > max_output;

    Ok(Captured {
        exit_code,
        stdout: capped_utf8(&stdout, max_output),
        stderr: capped_utf8(&stderr, max_output),
        stdout_truncated,
        stderr_truncated,
        // #196: net denials are attached by run_with_egress_proxy (which owns the
        // proxy handle), not here — a bare pipeline run observes no proxy refusals.
        net_denials: Vec::new(),
        timed_out,
    })
}

/// What a stage's stderr clones from for `2>&1` (the stdout destination).
enum DupSource {
    File(std::fs::File),
    Pipe(PipeWriter),
}

/// Map an `io::Result`, killing already-spawned children on error so a failure
/// mid-pipeline never orphans processes.
fn ok_or_kill<T>(result: std::io::Result<T>, children: &mut [Child]) -> ToolResult<T> {
    result.map_err(|e| {
        kill_all(children);
        ToolError::Exec(e)
    })
}

/// [`ok_or_kill`] for results that already carry a [`ToolError`] (the mediated
/// redirect opens, #351): kill spawned stages, keep the denial/error as-is.
fn ok_or_kill_tool<T>(result: ToolResult<T>, children: &mut [Child]) -> ToolResult<T> {
    result.inspect_err(|_| kill_all(children))
}

/// Read **at most** `max_output` bytes from `reader` into memory, then probe one
/// more byte to decide whether the source had more. Returns the captured bytes
/// (≤ cap) and whether it was truncated.
///
/// Crucially, peak buffering is bounded by the cap **regardless of how much the
/// child produces** — closing the DoS where a fast producer (`yes`,
/// `cat /dev/zero`) balloons host memory up to the timeout window (#73). The
/// remainder is **not** drained: dropping `reader` closes the pipe read end, so a
/// still-writing child gets `EPIPE`/`SIGPIPE` on its next write (the `| head`
/// model) rather than blocking us — and we never read past `cap + 1` bytes.
fn read_capped_observed(
    mut reader: impl Read,
    max_output: usize,
    output: &OutputEmitter,
    stream: crate::ShellOutputStream,
) -> (Vec<u8>, bool) {
    let mut buf = Vec::with_capacity(max_output.min(8 * 1024));
    let mut chunk = [0u8; 8 * 1024];
    while buf.len() < max_output {
        let remaining = max_output - buf.len();
        let read_len = remaining.min(chunk.len());
        match reader.read(&mut chunk[..read_len]) {
            Ok(0) => return (buf, false),
            Ok(n) => {
                output.emit(stream, &chunk[..n]);
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return (buf, false),
        }
    }
    let mut probe = [0u8; 1];
    let truncated = loop {
        match reader.read(&mut probe) {
            Ok(n) => break n > 0,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break false,
        }
    };
    (buf, truncated)
}

#[cfg(test)]
fn read_capped(reader: impl Read, max_output: usize) -> (Vec<u8>, bool) {
    read_capped_observed(
        reader,
        max_output,
        &OutputEmitter::default(),
        crate::ShellOutputStream::Stdout,
    )
}

/// Lossy-decode captured output (already bounded to ≤ `max_output` by
/// [`read_capped`]). The `min` is a defensive belt-and-suspenders. Truncation at
/// a byte boundary is safe: [`String::from_utf8_lossy`] replaces any partial
/// trailing sequence rather than panicking.
fn capped_utf8(bytes: &[u8], max_output: usize) -> String {
    let slice = &bytes[..bytes.len().min(max_output)];
    String::from_utf8_lossy(slice).into_owned()
}

/// Cap an already-decoded string to `max_output` at a char boundary
/// (used for the concatenated output of a multi-pipeline script).
fn cap_string(mut s: String, max_output: usize) -> String {
    if s.len() > max_output {
        let mut end = max_output;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridle_core::{Caveats, Gate, Scope};
    use std::collections::HashMap;
    use std::sync::{mpsc, Mutex};

    /// The schema loads from the embedded `shell_tool.schema.json` data file (not
    /// an inline literal) with the expected shape. Guards the data file against
    /// corruption — a bad edit fails here, not in prod.
    #[test]
    fn schema_loads_from_data_file_with_expected_shape() {
        let s = ShellTool::new().schema();
        assert_eq!(s["type"], "object");
        assert_eq!(s["additionalProperties"], false);
        for key in ["program", "args", "cmd", "cwd", "env", "timeout_secs"] {
            assert!(
                s["properties"].get(key).is_some(),
                "schema is missing the `{key}` property: {s}"
            );
        }
    }

    /// The `timeout_secs.maximum` is a per-instance property injected over the
    /// data-file base — it tracks the configured `LimitsPolicy`, so `with_config`
    /// changes the advertised ceiling.
    #[test]
    fn schema_timeout_maximum_tracks_the_configured_limits() {
        let limits = agent_bridle_core::LimitsPolicy {
            max_timeout_secs: 7,
            ..agent_bridle_core::LimitsPolicy::default()
        };
        let s = ShellTool::with_config(limits).schema();
        assert_eq!(s["properties"]["timeout_secs"]["maximum"], 7);
        // The data file itself carries no `maximum` — the bound is Rust-owned.
        assert!(SHELL_SCHEMA["properties"]["timeout_secs"]
            .get("maximum")
            .is_none());
    }

    /// A safe-subset `$()` refusal carries Brush's parse-only, source-bound
    /// inventory. Inspection is metadata only: not even the outer `ls` reaches
    /// the mock spawner.
    #[cfg(feature = "brush")]
    #[tokio::test]
    async fn dynamic_refusal_attaches_non_executing_shell_inspection() {
        let cmd = r#"ls -1 $(find . -name "*.rs" -type f -exec wc -l {} + 2>/dev/null | sort -nr | head -10)"#;
        let mock = Arc::new(MockSpawner::default());
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(serde_json::json!({"cmd": cmd}), &ctx(Caveats::top()))
            .await
            .expect("structured refusal");

        assert_eq!(out["denied"], true);
        assert_eq!(out["denials"][0]["target"], "command substitution `$(`");
        assert_eq!(out["shell_inspection"]["source"], cmd);
        assert_eq!(
            out["shell_inspection"]["constructs"][0]["kind"],
            "command_substitution"
        );
        assert_eq!(
            out["shell_inspection"]["constructs"][0]["inspection"]["commands"][0]
                ["descendant_execs"][0]["program"],
            "wc"
        );
        assert!(
            calls(&mock).is_empty(),
            "inspection must not execute any stage: {out}"
        );

        let arithmetic_cmd = r#"echo "$((1 + 2))""#;
        let arithmetic = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": arithmetic_cmd}),
                &ctx(Caveats::top()),
            )
            .await
            .expect("structured arithmetic refusal");

        assert_eq!(
            arithmetic["denials"][0]["target"],
            "arithmetic expansion `$((`"
        );
        assert_eq!(
            arithmetic["shell_inspection"]["constructs"][0]["kind"],
            "arithmetic_expansion"
        );
        assert!(
            calls(&mock).is_empty(),
            "arithmetic inspection must not execute any stage: {arithmetic}"
        );

        let runtime_arithmetic = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "echo $((runtime_value))"}),
                &ctx(Caveats::top()),
            )
            .await
            .expect("structured runtime arithmetic refusal");
        assert_eq!(
            runtime_arithmetic["denials"][0]["target"],
            "arithmetic expansion `$((`"
        );
        assert!(
            runtime_arithmetic.get("shell_inspection").is_none(),
            "an incomplete runtime-state projection must not be attached: {runtime_arithmetic}"
        );
        assert!(
            calls(&mock).is_empty(),
            "runtime arithmetic inspection must not execute any stage: {runtime_arithmetic}"
        );
    }

    /// A spawner that records every pipeline it runs and returns a canned exit
    /// code per program (argv0), default 0 — no real processes.
    #[derive(Default)]
    struct MockSpawner {
        calls: Mutex<Vec<Vec<Command>>>,
        /// The env map handed to each `run` call (parallel to `calls`), so the env
        /// seam (newt #783) is verified without a real process.
        envs: Mutex<Vec<BTreeMap<String, String>>>,
        /// #385/#386: private-host approvals forwarded to each `run` call, and the
        /// granted `net` scope alongside them — so a test can assert the two travel
        /// independently rather than one being inferred from the other.
        private_hosts: Mutex<Vec<Vec<String>>>,
        net_scopes: Mutex<Vec<Scope<String>>>,
        exit_by_program: HashMap<String, i32>,
        block_ms: u64,
        /// #196: net denials the spawner reports back — the shape
        /// `run_with_egress_proxy` produces from the proxy's refused hosts, so the
        /// Captured→envelope wiring is verified without a real proxy/child.
        net_denials: Vec<Denial>,
    }

    impl MockSpawner {
        fn with_exit(program: &str, code: i32) -> Self {
            let mut m = Self::default();
            m.exit_by_program.insert(program.to_string(), code);
            m
        }

        /// #196: a mock whose `run` reports these net denials (as the real proxy
        /// path would for refused hosts).
        fn with_net_denials(denials: Vec<Denial>) -> Self {
            Self {
                net_denials: denials,
                ..Self::default()
            }
        }
    }

    /// A stage's program word (argv[0]) for test assertions. (A variable in the
    /// program position is denied in `invoke`, so it never reaches the spawner.)
    fn prog(stage: &Command) -> &str {
        match stage.argv.first() {
            Some(Arg::Lit(s) | Arg::Glob(s)) => s,
            Some(Arg::Var(_) | Arg::VarGlob(_)) | None => "",
        }
    }

    impl Spawner for MockSpawner {
        fn requires_backend_admission(&self) -> bool {
            false
        }

        fn run(
            &self,
            stages: &[Command],
            _cwd: Option<&str>,
            caveats: &Caveats,
            env: &BTreeMap<String, String>,
            cfg: &SpawnCfg,
        ) -> ToolResult<Captured> {
            self.calls.lock().unwrap().push(stages.to_vec());
            self.envs.lock().unwrap().push(env.clone());
            let mut hosts: Vec<_> = cfg.private_hosts.iter().cloned().collect();
            hosts.sort();
            self.private_hosts.lock().unwrap().push(hosts);
            self.net_scopes.lock().unwrap().push(caveats.net.clone());
            if self.block_ms > 0 {
                std::thread::sleep(Duration::from_millis(self.block_ms));
            }
            Ok(Captured {
                exit_code: self
                    .exit_by_program
                    .get(prog(&stages[0]))
                    .copied()
                    .unwrap_or(0),
                stdout: String::new(),
                stderr: String::new(),
                net_denials: self.net_denials.clone(),
                ..Default::default()
            })
        }
    }

    /// #2732 round 2: short pipelines share one invocation deadline. A later
    /// side-effect sentinel must never start after the cumulative budget expires.
    #[test]
    fn round2_successive_stages_share_one_deadline() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct ClockSpawner {
            ticks: Arc<AtomicU64>,
            calls: Mutex<Vec<String>>,
        }
        impl Spawner for ClockSpawner {
            fn requires_backend_admission(&self) -> bool {
                false
            }
            fn run(
                &self,
                stages: &[Command],
                _: Option<&str>,
                _: &Caveats,
                _: &BTreeMap<String, String>,
                _: &SpawnCfg,
            ) -> ToolResult<Captured> {
                self.calls.lock().unwrap().push(prog(&stages[0]).to_owned());
                self.ticks.fetch_add(30, Ordering::SeqCst);
                Ok(Captured::default())
            }
        }
        let ticks = Arc::new(AtomicU64::new(0));
        let clock = ticks.clone();
        let spawner = ClockSpawner {
            ticks,
            calls: Mutex::new(Vec::new()),
        };
        let cfg = SpawnCfg {
            max_output: 100,
            held_read_roots: Vec::new(),
            audit_sink: None,
            sandbox: Arc::new(SandboxPolicy::default()),
            private_hosts: HashSet::new(),
            unbridled: false,
            output: OutputEmitter::default(),
            deadline: crate::supervisor::Deadline::with_clock(Duration::from_secs(60), move || {
                Duration::from_secs(clock.load(Ordering::SeqCst))
            }),
        };
        let script = classify("first; second; sentinel").unwrap();
        let captured = run_script(
            &spawner,
            &script,
            None,
            &Caveats::top(),
            &BTreeMap::new(),
            &cfg,
        )
        .unwrap();
        assert_eq!(*spawner.calls.lock().unwrap(), ["first", "second"]);
        assert!(captured.timed_out);
        assert_eq!(captured.exit_code, 124);
    }

    struct CoordinatedSpawner {
        proceed: Mutex<mpsc::Receiver<()>>,
        finished: mpsc::Sender<()>,
    }

    impl Spawner for CoordinatedSpawner {
        fn requires_backend_admission(&self) -> bool {
            false
        }

        fn run(
            &self,
            _stages: &[Command],
            _cwd: Option<&str>,
            _caveats: &Caveats,
            _env: &BTreeMap<String, String>,
            cfg: &SpawnCfg,
        ) -> ToolResult<Captured> {
            cfg.output.emit(crate::ShellOutputStream::Stdout, b"first");
            let _ = self
                .proceed
                .lock()
                .expect("proceed lock")
                .recv_timeout(cfg.deadline.remaining());
            cfg.output.emit(crate::ShellOutputStream::Stdout, b"second");
            self.finished.send(()).expect("test observes completion");
            Ok(Captured {
                exit_code: 0,
                stdout: "firstsecond".to_string(),
                ..Default::default()
            })
        }
    }

    fn coordinated_spawner() -> (
        Arc<CoordinatedSpawner>,
        mpsc::Sender<()>,
        mpsc::Receiver<()>,
    ) {
        let (proceed_tx, proceed_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        (
            Arc::new(CoordinatedSpawner {
                proceed: Mutex::new(proceed_rx),
                finished: finished_tx,
            }),
            proceed_tx,
            finished_rx,
        )
    }

    struct BlockingObserver {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        finished: mpsc::Sender<()>,
    }

    impl crate::ShellOutputObserver for BlockingObserver {
        fn on_output(
            &self,
            _invocation: crate::ShellInvocationId,
            _stream: crate::ShellOutputStream,
            _chunk: &[u8],
        ) {
            self.entered.send(()).expect("observer entered callback");
            self.release
                .lock()
                .expect("observer release lock")
                .recv()
                .expect("test releases blocked observer");
        }

        fn on_finish(&self, _invocation: crate::ShellInvocationId) {
            self.finished.send(()).expect("record unexpected finish");
        }
    }

    struct TemporalPipelineSpawner;

    impl Spawner for TemporalPipelineSpawner {
        fn requires_backend_admission(&self) -> bool {
            false
        }

        fn run(
            &self,
            stages: &[Command],
            _cwd: Option<&str>,
            _caveats: &Caveats,
            _env: &BTreeMap<String, String>,
            cfg: &SpawnCfg,
        ) -> ToolResult<Captured> {
            assert_eq!(stages.len(), 2, "the test request is one pipeline");
            // Stage two becomes readable first, but the final envelope is
            // assembled in pipeline-stage order by the real spawner.
            cfg.output
                .emit(crate::ShellOutputStream::Stderr, b"second-stage");
            cfg.output
                .emit(crate::ShellOutputStream::Stderr, b"first-stage");
            Ok(Captured {
                exit_code: 0,
                stderr: "firs".to_string(),
                stderr_truncated: true,
                ..Default::default()
            })
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum PipelineObserverEvent {
        Output(crate::ShellInvocationId, crate::ShellOutputStream, Vec<u8>),
        Finish(crate::ShellInvocationId),
    }

    struct PipelineObserver(mpsc::Sender<PipelineObserverEvent>);

    impl crate::ShellOutputObserver for PipelineObserver {
        fn on_output(
            &self,
            invocation: crate::ShellInvocationId,
            stream: crate::ShellOutputStream,
            chunk: &[u8],
        ) {
            self.0
                .send(PipelineObserverEvent::Output(
                    invocation,
                    stream,
                    chunk.to_vec(),
                ))
                .expect("record pipeline output");
        }

        fn on_finish(&self, invocation: crate::ShellInvocationId) {
            self.0
                .send(PipelineObserverEvent::Finish(invocation))
                .expect("record pipeline finish");
        }
    }

    #[tokio::test]
    async fn observer_receives_output_before_invoke_completes() {
        let (spawner, proceed, finished) = coordinated_spawner();
        let (seen_tx, seen_rx) = mpsc::channel();
        let seen_rx = Arc::new(Mutex::new(seen_rx));
        let observer = Arc::new(move |invocation, stream, chunk: &[u8]| {
            seen_tx
                .send((invocation, stream, chunk.to_vec()))
                .expect("test receives observer callback");
        });
        let tool = ShellTool::with_spawner(spawner).with_output_observer(observer);
        let context = ctx(exec_only(&["anything"]));

        let invoke = tokio::spawn(async move {
            tool.invoke(serde_json::json!({"program": "anything"}), &context)
                .await
        });
        let first_rx = Arc::clone(&seen_rx);
        let first = tokio::task::spawn_blocking(move || {
            first_rx
                .lock()
                .expect("observer receiver lock")
                .recv_timeout(Duration::from_secs(2))
        })
        .await
        .expect("receiver task")
        .expect("live callback before completion");
        let invocation = first.0;
        assert_eq!(
            first,
            (
                invocation,
                crate::ShellOutputStream::Stdout,
                b"first".to_vec()
            )
        );
        assert!(!invoke.is_finished(), "the tool must still be running");

        proceed.send(()).expect("release spawner");
        finished
            .recv_timeout(Duration::from_secs(2))
            .expect("spawner completion");
        let out = invoke.await.expect("invoke task").expect("invoke result");
        assert_eq!(out["stdout"], "firstsecond");
        assert_eq!(
            seen_rx
                .lock()
                .expect("observer receiver lock")
                .recv_timeout(Duration::from_secs(2))
                .expect("second callback"),
            (
                invocation,
                crate::ShellOutputStream::Stdout,
                b"second".to_vec()
            )
        );
    }

    #[tokio::test]
    async fn pipeline_stderr_live_cap_is_temporal_but_envelope_is_authoritative() {
        let (events_tx, events_rx) = mpsc::channel();
        let mut tool = ShellTool::with_spawner(Arc::new(TemporalPipelineSpawner));
        tool.limits.max_output_bytes = 4;
        let tool = tool.with_output_observer(Arc::new(PipelineObserver(events_tx)));

        let out = tool
            .invoke(
                serde_json::json!({"cmd": "first | second"}),
                &ctx(exec_only(&["first", "second"])),
            )
            .await
            .expect("invoke pipeline");

        assert_eq!(out["stderr"], "firs");
        assert_eq!(out["stderr_truncated"], true);
        let first = events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("live stderr event");
        let invocation = match first {
            PipelineObserverEvent::Output(id, crate::ShellOutputStream::Stderr, bytes) => {
                assert_eq!(bytes, b"seco", "the live cap follows enqueue order");
                id
            }
            other => panic!("unexpected first observer event: {other:?}"),
        };
        assert_eq!(
            events_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("queue-drained finish"),
            PipelineObserverEvent::Finish(invocation)
        );
        assert!(
            events_rx.try_recv().is_err(),
            "the later stage-order bytes are outside the live cap"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_does_not_wait_for_a_blocked_observer_or_deliver_late_output() {
        let (spawner, proceed, finished) = coordinated_spawner();
        let (seen_tx, seen_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_observer_tx, release_observer_rx) = mpsc::channel();
        let release_observer_rx = Mutex::new(release_observer_rx);
        let observer = Arc::new(move |invocation, stream, chunk: &[u8]| {
            seen_tx
                .send((invocation, stream, chunk.to_vec()))
                .expect("observer receiver remains alive");
            entered_tx.send(()).expect("observer entered callback");
            release_observer_rx
                .lock()
                .expect("observer release lock")
                .recv()
                .expect("test releases blocked observer");
        });
        let tool = ShellTool::with_spawner(spawner).with_output_observer(observer);
        let context = ctx(exec_only(&["anything"]));

        let mut invoke = tokio::spawn(async move {
            tool.invoke(serde_json::json!({"program": "anything"}), &context)
                .await
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("observer is blocked in its first callback");
        let first = seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first callback");
        assert_eq!(first.1, crate::ShellOutputStream::Stdout);
        assert_eq!(first.2, b"first");

        invoke.abort();
        let cancelled = tokio::time::timeout(Duration::from_millis(500), &mut invoke).await;
        let _ = proceed.send(());
        release_observer_tx
            .send(())
            .expect("release presentation callback");
        finished
            .recv_timeout(Duration::from_secs(2))
            .expect("detached worker attempted its late write");
        let cancelled = cancelled.expect("cancellation must not wait for observer code");
        assert!(
            cancelled.expect_err("invoke is cancelled").is_cancelled(),
            "the invocation future was cancelled"
        );
        assert!(
            seen_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "output emitted by the detached worker after cancellation is ignored"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_does_not_wait_for_a_blocked_observer_or_finish_the_session() {
        let (spawner, proceed, worker_finished) = coordinated_spawner();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let observer = Arc::new(BlockingObserver {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            finished: finish_tx,
        });
        let tool = ShellTool::with_spawner(spawner).with_output_observer(observer);
        let context = ctx(exec_only(&["anything"]));

        let mut invoke = tokio::spawn(async move {
            tool.invoke(
                serde_json::json!({"program": "anything", "timeout_secs": 1}),
                &context,
            )
            .await
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("observer is blocked in its first callback");

        let result = tokio::time::timeout(Duration::from_secs(2), &mut invoke).await;
        if result.is_err() {
            invoke.abort();
        }
        let _ = proceed.send(());
        release_tx.send(()).expect("release presentation callback");
        worker_finished
            .recv_timeout(Duration::from_secs(2))
            .expect("detached worker attempted its late write");

        let output = result
            .expect("tool timeout must not wait for observer code")
            .expect("invoke task")
            .expect("timeout envelope");
        assert_eq!(output["timed_out"], true);
        assert!(
            finish_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "a timed-out observer session must not report ordinary completion"
        );
    }

    fn ctx(granted: Caveats) -> ToolContext {
        Gate::new(0)
            .authorize(&ShellTool::new(), &granted)
            .expect("authorize")
    }

    /// A context for a **strong** principal (fence-strength floor = `Kernel`):
    /// any restricted axis the real backend can't kernel-confine fails closed.
    fn ctx_strong(granted: Caveats) -> ToolContext {
        Gate::new(0)
            .with_strength_floor(agent_bridle_core::AxisEnforcement::Kernel)
            .authorize(&ShellTool::new(), &granted)
            .expect("authorize")
    }

    fn exec_only(names: &[&str]) -> Caveats {
        Caveats {
            exec: Scope::only(names.iter().map(|s| (*s).to_string())),
            ..Caveats::top()
        }
    }

    fn calls(mock: &Arc<MockSpawner>) -> Vec<Vec<Command>> {
        mock.calls.lock().unwrap().clone()
    }

    /// The env map handed to each `run` call, in order (the env seam, newt #783).
    fn envs(mock: &Arc<MockSpawner>) -> Vec<BTreeMap<String, String>> {
        mock.envs.lock().unwrap().clone()
    }

    /// #385/#386: forward-port of `66960bb`'s `ShellTool` builder tests — the
    /// approval set is empty by default and rejects anything that is not an
    /// exact host (wildcard, URL, port, path/socket form).
    #[test]
    fn private_hosts_are_empty_by_default_and_reject_nonexact_names() {
        assert!(ShellTool::new().private_hosts.is_empty());
        assert!(ShellTool::default().private_hosts.is_empty());
        for invalid in [
            "*",
            "*.example.test",
            "https://service.test",
            "service.test:443",
            "unix:/tmp/service.sock",
        ] {
            let error = ShellTool::new()
                .with_private_hosts([invalid.to_string()])
                .expect_err("private-host approval must be an exact host");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    /// The approved set is canonicalized (lowercased, trailing-dot stripped,
    /// deduped) and reaches the spawner via `SpawnCfg::private_hosts`, without
    /// widening or narrowing the invocation's own `net` scope.
    #[tokio::test]
    async fn private_hosts_reach_spawner_without_changing_net_authority() {
        let mock = Arc::new(MockSpawner::default());
        let granted = Caveats {
            net: Scope::only(["other.test".to_string()]),
            ..Caveats::top()
        };
        let out = ShellTool::with_spawner(mock.clone())
            .with_private_hosts(["Service.Test.".to_string(), "service.test".to_string()])
            .expect("canonical exact approval")
            .invoke(serde_json::json!({"cmd": "echo hi"}), &ctx(granted.clone()))
            .await
            .expect("invoke");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(
            *mock.private_hosts.lock().unwrap(),
            vec![vec!["service.test".to_string()]]
        );
        assert_eq!(*mock.net_scopes.lock().unwrap(), vec![granted.net]);
    }

    /// Granting ordinary `net` scope for a host does NOT itself approve it as a
    /// private-host target — the two grants are independent (#385/#386).
    #[tokio::test]
    async fn private_hosts_are_not_inferred_from_net_authority() {
        let mock = Arc::new(MockSpawner::default());
        let granted = Caveats {
            net: Scope::only(["service.test".to_string()]),
            ..Caveats::top()
        };
        ShellTool::with_spawner(mock.clone())
            .invoke(serde_json::json!({"cmd": "echo hi"}), &ctx(granted))
            .await
            .expect("invoke");
        assert_eq!(
            *mock.private_hosts.lock().unwrap(),
            vec![Vec::<String>::new()]
        );
    }

    /// ADR 0012 D4/D8 + ADR 0014: a STRONG principal (floor = `Kernel`) refuses to
    /// run unconfined when a restricted axis cannot be kernel-confined on this host.
    ///
    /// For the `exec` axis the outcome is **backend-dependent** since ADR 0014
    /// closed #57 for macOS: under an active Seatbelt backend `exec` is
    /// kernel-confined via `process-exec*`, so the strong principal *runs*
    /// (reporting `exec → kernel`); under Landlock or a Noop host the exec axis is
    /// still held (#31/#57), so it fails closed *before any spawn*. The default
    /// (permissive, Advisory-floor) principal runs in either case. This closes the
    /// shell's run-unconfined gap and matches `ConfinedCommand`'s fail-closed
    /// posture. The test's expectation is derived from the *same* honesty rule the
    /// production path uses (`intended_sandbox_kind` + `enforcement_report`), so the
    /// two cannot disagree across platforms/features.
    #[tokio::test]
    async fn strong_principal_fails_closed_on_unenforceable_exec() {
        let granted = exec_only(&["echo"]);
        // Does the backend that will actually govern this run kernel-confine `exec`?
        // Seatbelt does (`process-exec*`, ADR 0014); Landlock/Noop do not (#31/#57).
        let exec_is_kernel_confined = enforcement_report(
            &granted,
            intended_sandbox_kind(&granted, &Arc::new(SandboxPolicy::default())),
        )
        .exec
            == Some(agent_bridle_core::AxisEnforcement::Kernel);

        let mock = Arc::new(MockSpawner::default());
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "echo hi"}),
                &ctx_strong(granted.clone()),
            )
            .await
            .expect("invoke");
        if exec_is_kernel_confined {
            // Seatbelt confines `exec` in the kernel, so the strong principal runs —
            // kernel-confined, not refused.
            assert_ne!(
                out["denied"],
                serde_json::json!(true),
                "kernel-confined exec must run for a strong principal: {out}"
            );
            assert_eq!(
                out["enforcement"]["exec"], "kernel",
                "exec is reported kernel-confined: {out}"
            );
            assert_eq!(ran_programs(&mock), ["echo"], "the program spawned: {out}");
        } else {
            // The exec axis is held (Landlock/Noop): a Kernel floor cannot be met, so
            // refuse before any spawn.
            assert_eq!(
                out["denied"], true,
                "strong principal must fail closed on unenforceable exec: {out}"
            );
            assert!(ran_programs(&mock).is_empty(), "nothing may spawn: {out}");
        }

        // The default (permissive, Advisory-floor) principal runs the same command
        // regardless of backend.
        let mock = Arc::new(MockSpawner::default());
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(serde_json::json!({"cmd": "echo hi"}), &ctx(granted))
            .await
            .expect("invoke");
        assert_ne!(
            out["denied"],
            serde_json::json!(true),
            "default principal still runs: {out}"
        );
    }

    /// #196: a net refusal reported by the spawner (the shape
    /// `run_with_egress_proxy` produces from the proxy's refused hosts) reaches
    /// the result envelope as a structured `net` denial with `denied: true` — the
    /// exact signal a consumer (newt) lifts into a per-host prompt. Unlike an
    /// `exec`/`open` refusal, the command still RAN (the refusal is observed
    /// during the run, not at pre-spawn admission).
    #[tokio::test]
    async fn net_refusal_surfaces_as_a_net_denial_in_the_envelope() {
        let mock = Arc::new(MockSpawner::with_net_denials(vec![Denial {
            kind: DenialKind::Net,
            target: "github.com".to_string(),
            reason: "net does not permit 'github.com'".to_string(),
        }]));
        let out = ShellTool::with_spawner(mock)
            .invoke(
                serde_json::json!({ "cmd": "echo hi" }),
                &ctx(exec_only(&["echo"])),
            )
            .await
            .expect("invoke");
        assert_eq!(
            out["denied"],
            serde_json::json!(true),
            "a net denial sets denied: {out}"
        );
        assert_eq!(out["denials"][0]["kind"], "net");
        assert_eq!(out["denials"][0]["target"], "github.com");
        // The command still executed — a success envelope (has exit_code), not a
        // pre-spawn refused envelope.
        assert!(out.get("exit_code").is_some(), "command still ran: {out}");
    }

    fn ran_programs(mock: &Arc<MockSpawner>) -> Vec<String> {
        calls(mock)
            .iter()
            .map(|pipeline| prog(&pipeline[0]).to_string())
            .collect()
    }

    // ── the env seam (newt #783) ────────────────────────────────────────────

    /// A dispatch carrying `"env": { "FOO": "bar" }` reaches the spawner with that
    /// var on the child's environment map — the seam newt #783 needs so it can
    /// pass the venv environment as real env instead of an `export …;` prefix.
    #[tokio::test]
    async fn env_map_is_passed_to_the_spawner() {
        let mock = Arc::new(MockSpawner::default());
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({
                    "program": "echo",
                    "args": ["hi"],
                    "env": { "FOO": "bar", "VIRTUAL_ENV": "/venv" },
                }),
                &ctx(exec_only(&["echo"])),
            )
            .await
            .expect("invoke");
        assert_ne!(out["denied"], serde_json::json!(true), "must run: {out}");
        let envs = envs(&mock);
        assert_eq!(envs.len(), 1, "one pipeline ran");
        assert_eq!(envs[0].get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(
            envs[0].get("VIRTUAL_ENV").map(String::as_str),
            Some("/venv"),
            "every env entry reaches the child: {:?}",
            envs[0]
        );
    }

    /// The env map is NEVER part of the leash decision: the leash still checks the
    /// real program. A compound command (`hostname; uname`) with `env` set must
    /// check `hostname` first — never `export`/`env`/an env KEY. This is the exact
    /// newt #783 root cause: prepending `export VIRTUAL_ENV=…;` made the first
    /// stage's argv[0] the literal `export` builtin, which the leash denied. With
    /// env carried as a real map there is no `export` stage at all.
    #[tokio::test]
    async fn env_does_not_change_the_program_the_leash_checks() {
        let mock = Arc::new(MockSpawner::default());
        // Grant exactly the two real programs; `export`/`env`/the env keys are NOT
        // granted, so if any of them were checked the run would be denied.
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({
                    "cmd": "hostname; uname -s",
                    "env": { "FOO": "bar" },
                }),
                &ctx(exec_only(&["hostname", "uname"])),
            )
            .await
            .expect("invoke");
        assert_ne!(out["denied"], serde_json::json!(true), "must run: {out}");
        // The FIRST program the spawner saw is the real `hostname`, not `export`.
        let programs = ran_programs(&mock);
        assert_eq!(
            programs,
            vec!["hostname".to_string(), "uname".to_string()],
            "the leash/spawner see the real programs, never `export`/env keys: {programs:?}"
        );
        // And the env still reached each child.
        for e in envs(&mock) {
            assert_eq!(e.get("FOO").map(String::as_str), Some("bar"));
        }
    }

    /// `ShellArgs::parse`: the `env` field is populated from the dispatch JSON
    /// `"env"` object when present, and is empty (back-compat) when absent.
    #[test]
    fn parse_env_field_present_and_absent() {
        // Present → populated (string values only).
        let parsed = ShellArgs::parse(
            &serde_json::json!({
                "program": "echo",
                "env": { "FOO": "bar", "BAZ": "qux" },
            }),
            &agent_bridle_core::LimitsPolicy::default(),
        )
        .expect("parse");
        assert_eq!(parsed.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(parsed.env.get("BAZ").map(String::as_str), Some("qux"));
        assert_eq!(parsed.env.len(), 2);

        // Absent → empty map (existing dispatches are unaffected).
        let parsed = ShellArgs::parse(
            &serde_json::json!({ "program": "echo" }),
            &agent_bridle_core::LimitsPolicy::default(),
        )
        .expect("parse");
        assert!(parsed.env.is_empty(), "absent env defaults to empty");
    }

    /// #143: the timeout is bounded/defaulted by the configured `LimitsPolicy`,
    /// not the old hard-coded 300/60. A tuned policy clamps and defaults to its
    /// own values.
    #[test]
    fn parse_timeout_uses_configured_limits() {
        let limits = agent_bridle_core::LimitsPolicy {
            max_timeout_secs: 5,
            default_timeout_secs: 3,
            ..agent_bridle_core::LimitsPolicy::default()
        };
        // A request over the configured max is clamped to it.
        let over = ShellArgs::parse(
            &serde_json::json!({ "program": "echo", "timeout_secs": 9999 }),
            &limits,
        )
        .expect("parse");
        assert_eq!(over.timeout, std::time::Duration::from_secs(5));
        // No timeout specified → the configured default.
        let dflt =
            ShellArgs::parse(&serde_json::json!({ "program": "echo" }), &limits).expect("parse");
        assert_eq!(dflt.timeout, std::time::Duration::from_secs(3));
    }

    /// A fake environment for the `$VAR` tests — exercises the allowlist +
    /// expansion + resolved-path leash without touching the real process env.
    struct FakeEnv(HashMap<String, String>);
    impl EnvProvider for FakeEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }
    fn fake_env(pairs: &[(&str, &str)]) -> Arc<dyn EnvProvider> {
        Arc::new(FakeEnv(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        ))
    }

    /// A fake directory lister keyed by path string — drives the glob walker
    /// without a real filesystem (#47).
    struct MapLister(HashMap<String, Vec<GlobEntry>>);
    impl DirLister for MapLister {
        fn list(&self, dir: &Path) -> Vec<GlobEntry> {
            // Normalize to forward slashes so test maps written with `/` work on
            // Windows where PathBuf::join produces `\`-separated paths.
            let key = dir.to_string_lossy().replace('\\', "/");
            self.0.get(&key).cloned().unwrap_or_default()
        }
    }
    fn ent(name: &str, is_dir: bool) -> GlobEntry {
        GlobEntry {
            name: name.to_string(),
            is_dir,
        }
    }
    fn map_lister(dirs: &[(&str, Vec<GlobEntry>)]) -> Arc<dyn DirLister> {
        Arc::new(MapLister(
            dirs.iter()
                .map(|(d, es)| ((*d).to_string(), es.clone()))
                .collect(),
        ))
    }

    // ── $VAR in redirect targets (#46, via the env seam) ────────────────────

    /// `> $TMPDIR/out` expands the allowlisted var through the seam and the
    /// spawner receives the RESOLVED path (never a literal `$VAR`); the resolved
    /// path is what the fs leash checked.
    #[tokio::test]
    async fn redirect_var_is_expanded_and_reaches_spawner_resolved() {
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let mock = Arc::new(MockSpawner::default());
        let tool = ShellTool::with_spawner_and_env(mock.clone(), fake_env(&[("TMPDIR", &tmp)]));
        // fs_write is All (default), so the resolved path passes the leash.
        let out = tool
            .invoke(
                serde_json::json!({"cmd": "echo hi > $TMPDIR/out"}),
                &ctx(exec_only(&["echo"])),
            )
            .await
            .expect("invoke");
        assert_ne!(
            out["denied"],
            serde_json::json!(true),
            "in-scope var: {out}"
        );
        let redir = &calls(&mock)[0][0].redirects[0];
        assert_eq!(
            *redir,
            Redirect::Stdout {
                path: vec![Seg::Lit(format!("{tmp}/out"))],
                append: false,
            }
        );
    }

    /// A non-allowlisted variable in a redirect target denies before any spawn.
    #[tokio::test]
    async fn redirect_var_not_in_allowlist_is_denied() {
        let mock = Arc::new(MockSpawner::default());
        let tool = ShellTool::with_spawner_and_env(mock.clone(), fake_env(&[("SECRET", "/x")]));
        let out = tool
            .invoke(
                serde_json::json!({"cmd": "echo hi > $SECRET"}),
                &ctx(exec_only(&["echo"])),
            )
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true, "non-allowlisted redirect var: {out}");
        assert!(
            ran_programs(&mock).is_empty(),
            "no spawn on a denied redirect"
        );
        assert!(out["denials"][0]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("SECRET"));
    }

    // ── glob + variable in one word (#46, $DIR/*.rs) ────────────────────────

    /// The re-injection guard, unit-tested directly: a `*` in the VAR VALUE stays
    /// in the (literal) directory prefix and never globs; a variable in the glob
    /// BASENAME is refused.
    #[test]
    fn expand_varglob_keeps_value_metachars_literal_and_refuses_basename_var() {
        // TMPDIR is allowlisted; give it a value containing a glob metachar.
        let env = FakeEnv(HashMap::from([("TMPDIR".to_string(), "/a*b".to_string())]));
        let allow = agent_bridle_core::LimitsPolicy::default().var_allowlist;
        // `$TMPDIR/*.rs` → "/a*b/*.rs": the var's `*` is in the dir prefix
        // (literal); only the source `*.rs` basename globs.
        let pattern = expand_varglob(
            &[Seg::Var("TMPDIR".into()), Seg::Lit("/*.rs".into())],
            &env,
            &allow,
        )
        .unwrap();
        assert_eq!(pattern, "/a*b/*.rs");
        // A variable in the glob basename is refused (would re-inject metachars).
        let err = expand_varglob(
            &[Seg::Var("TMPDIR".into()), Seg::Lit("*.rs".into())],
            &env,
            &allow,
        );
        assert!(err.is_err(), "var in glob basename must be refused");
    }

    /// `$DIR/*.rs` lowers the var (env seam) AND expands the glob at admission
    /// (per-directory fs_read leash), so the spawner receives the literal matches.
    #[tokio::test]
    async fn glob_var_expands_to_resolved_matches_before_spawn() {
        let mock = Arc::new(MockSpawner::default());
        let lister = map_lister(&[
            (".", vec![ent("proj", true)]),
            ("./proj", vec![ent("a.rs", false), ent("b.rs", false)]),
        ]);
        let tool = ShellTool::with_seams(mock.clone(), fake_env(&[("TMPDIR", "proj")]), lister);
        let out = tool
            .invoke(
                serde_json::json!({"cmd": "ls $TMPDIR/*.rs"}), // fs_read All by default
                &ctx(exec_only(&["ls"])),
            )
            .await
            .expect("invoke");
        assert_ne!(
            out["denied"],
            serde_json::json!(true),
            "in-scope glob var: {out}"
        );
        assert_eq!(
            calls(&mock)[0][0].argv,
            vec![
                Arg::Lit("ls".into()),
                Arg::Lit("proj/a.rs".into()),
                Arg::Lit("proj/b.rs".into())
            ]
        );
    }

    /// A variable in the glob basename (`$PREFIX*.rs`) is refused at admission.
    #[tokio::test]
    async fn glob_var_in_basename_is_denied() {
        let mock = Arc::new(MockSpawner::default());
        let tool = ShellTool::with_spawner_and_env(mock.clone(), fake_env(&[("PREFIX", "foo")]));
        let out = tool
            .invoke(
                serde_json::json!({"cmd": "ls $PREFIX*.rs"}),
                &ctx(exec_only(&["ls"])),
            )
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true, "var in glob basename refused: {out}");
        assert!(ran_programs(&mock).is_empty());
    }

    /// A non-allowlisted variable in a glob word denies before any spawn.
    #[tokio::test]
    async fn glob_var_not_in_allowlist_is_denied() {
        let mock = Arc::new(MockSpawner::default());
        let tool = ShellTool::with_spawner_and_env(mock.clone(), fake_env(&[("SECRET", "/s")]));
        let out = tool
            .invoke(
                serde_json::json!({"cmd": "ls $SECRET/*.rs"}),
                &ctx(exec_only(&["ls"])),
            )
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true, "non-allowlisted glob var: {out}");
        assert!(ran_programs(&mock).is_empty());
    }

    /// The RESOLVED redirect path is leash-checked: an allowlisted var whose value
    /// lands outside `fs_write` scope denies (proving the leash sees the resolved
    /// path, not the literal `$VAR`).
    #[tokio::test]
    async fn redirect_var_resolved_path_out_of_fs_write_scope_denied() {
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let mock = Arc::new(MockSpawner::default());
        let tool = ShellTool::with_spawner_and_env(mock.clone(), fake_env(&[("TMPDIR", &tmp)]));
        let granted = Caveats {
            exec: Scope::only(["echo".to_string()]),
            fs_write: Scope::only(["/nonexistent-grant-root".to_string()]),
            ..Caveats::top()
        };
        let out = tool
            .invoke(
                serde_json::json!({"cmd": "echo hi > $TMPDIR/out"}),
                &ctx(granted),
            )
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true, "resolved path outside fs_write: {out}");
        assert!(ran_programs(&mock).is_empty());
    }

    // ── sequencing / leash (carried from earlier increments) ────────────────

    #[tokio::test]
    async fn and_short_circuits_on_failure() {
        let mock = Arc::new(MockSpawner::with_exit("false", 1));
        ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "false && echo hi"}),
                &ctx(exec_only(&["false", "echo"])),
            )
            .await
            .expect("invoke");
        assert_eq!(ran_programs(&mock), vec!["false"], "echo must be skipped");
    }

    #[tokio::test]
    async fn out_of_scope_anywhere_denies_the_whole_script() {
        let mock = Arc::new(MockSpawner::default());
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "echo ok ; rm -rf x"}),
                &ctx(exec_only(&["echo"])),
            )
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true);
        assert!(ran_programs(&mock).is_empty());
    }

    // ── globbing (increment 5) ──────────────────────────────────────────────

    /// A glob arg is EXPANDED at admission (with the per-directory fs_read leash)
    /// to its literal matches before the spawner runs (#47).
    #[tokio::test]
    async fn glob_arg_expanded_to_matches_before_spawn() {
        let mock = Arc::new(MockSpawner::default());
        let lister = map_lister(&[(
            ".",
            vec![ent("a.rs", false), ent("b.rs", false), ent("c.txt", false)],
        )]);
        ShellTool::with_seams(mock.clone(), fake_env(&[]), lister)
            .invoke(
                serde_json::json!({"cmd": "ls *.rs"}), // fs_read is All by default
                &ctx(exec_only(&["ls"])),
            )
            .await
            .expect("invoke");
        assert_eq!(
            calls(&mock)[0][0].argv,
            vec![
                Arg::Lit("ls".into()),
                Arg::Lit("a.rs".into()),
                Arg::Lit("b.rs".into())
            ]
        );
    }

    /// A glob in the program position is refused (we never exec a pattern).
    #[tokio::test]
    async fn glob_as_program_name_denied() {
        let mock = Arc::new(MockSpawner::default());
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(serde_json::json!({"cmd": "*.sh foo"}), &ctx(Caveats::top()))
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true);
        assert!(ran_programs(&mock).is_empty());
    }

    /// The directory a glob lists is an `fs_read`; out of scope ⇒ denied, no spawn.
    #[tokio::test]
    async fn glob_dir_out_of_fs_read_scope_denied() {
        let mock = Arc::new(MockSpawner::default());
        let granted = Caveats {
            exec: Scope::only(["echo".to_string()]),
            // fs_read restricted to the temp dir; the cwd glob lists elsewhere.
            fs_read: Scope::only([std::env::temp_dir().to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(serde_json::json!({"cmd": "echo *"}), &ctx(granted))
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true);
        assert_eq!(out["denials"][0]["kind"], "open");
        assert!(ran_programs(&mock).is_empty());
    }

    // ── variable expansion / allowlist (increment 6) ────────────────────────

    /// An allowlisted variable reaches the spawner as an (unexpanded) `Var`.
    #[tokio::test]
    async fn allowlisted_var_reaches_spawner() {
        let mock = Arc::new(MockSpawner::default());
        ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "echo $HOME"}),
                &ctx(exec_only(&["echo"])),
            )
            .await
            .expect("invoke");
        let c = calls(&mock);
        assert_eq!(
            c[0][0].argv,
            vec![
                Arg::Lit("echo".into()),
                Arg::Var(vec![Seg::Var("HOME".into())]),
            ]
        );
    }

    /// A variable NOT on the allowlist is denied — the spawner is never called,
    /// so a secret like `$AWS_SECRET_KEY` can never be spliced into an argument.
    #[tokio::test]
    async fn non_allowlisted_var_denied() {
        let mock = Arc::new(MockSpawner::default());
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "echo $AWS_SECRET_KEY"}),
                &ctx(Caveats::top()),
            )
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true);
        assert_eq!(out["denials"][0]["target"], "$AWS_SECRET_KEY");
        assert!(ran_programs(&mock).is_empty());
    }

    /// A variable in the program position is refused (we never exec a variable).
    #[tokio::test]
    async fn var_as_program_name_denied() {
        let mock = Arc::new(MockSpawner::default());
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "$HOME foo"}),
                &ctx(Caveats::top()),
            )
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true);
        assert!(ran_programs(&mock).is_empty());
    }

    // ── stderr redirects / 2>&1 (issue #45) ─────────────────────────────────

    /// A `2> file` target is leash-checked (`fs_write`) before any spawn.
    #[tokio::test]
    async fn stderr_to_file_out_of_scope_denied() {
        let mock = Arc::new(MockSpawner::default());
        let granted = Caveats {
            exec: Scope::only(["cmd".to_string()]),
            fs_write: Scope::only([std::env::temp_dir().to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let out = ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "cmd 2> /etc/passwd"}),
                &ctx(granted),
            )
            .await
            .expect("invoke");
        assert_eq!(out["denied"], true);
        assert_eq!(out["denials"][0]["kind"], "open");
        assert_eq!(out["denials"][0]["target"], "/etc/passwd");
        assert!(ran_programs(&mock).is_empty());
    }

    /// `2>&1` parses to a merge and reaches the spawner (no separate file open).
    #[tokio::test]
    async fn stderr_merge_reaches_spawner() {
        let mock = Arc::new(MockSpawner::default());
        ShellTool::with_spawner(mock.clone())
            .invoke(
                serde_json::json!({"cmd": "cmd 2>&1"}),
                &ctx(exec_only(&["cmd"])),
            )
            .await
            .expect("invoke");
        let c = calls(&mock);
        assert_eq!(c[0][0].stderr_disposition(), StderrTo::Stdout);
    }

    #[tokio::test]
    async fn both_program_and_cmd_is_a_hard_error() {
        let res = ShellTool::new()
            .invoke(
                serde_json::json!({"program": "echo", "cmd": "echo hi"}),
                &ctx(Caveats::top()),
            )
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn timeout_is_reported() {
        let mock = Arc::new(MockSpawner {
            block_ms: 1500,
            ..Default::default()
        });
        let out = ShellTool::with_spawner(mock)
            .invoke(
                serde_json::json!({"program": "anything", "timeout_secs": 1}),
                &ctx(exec_only(&["anything"])),
            )
            .await
            .expect("invoke");
        assert_eq!(out["timed_out"], true);
    }

    // ── pure glob matching / expansion (no real fs) ─────────────────────────

    #[test]
    fn fnmatch_basics() {
        assert!(fnmatch("*.rs", "a.rs"));
        assert!(!fnmatch("*.rs", "a.txt"));
        assert!(fnmatch("a?c", "abc"));
        assert!(!fnmatch("a?c", "ac"));
        assert!(fnmatch("*", ""));
        assert!(fnmatch("a*", "a"));
        assert!(fnmatch("[abc]x", "bx"));
        assert!(!fnmatch("[abc]x", "dx"));
        assert!(fnmatch("[!abc]x", "dx"));
        assert!(fnmatch("[a-c]", "b"));
        assert!(!fnmatch("[a-c]", "d"));
        assert!(fnmatch("foo*bar", "fooXYbar"));
    }

    /// #73 regression: `read_capped` bounds peak buffering to the cap and flags
    /// truncation, without slurping the whole stream. The reader panics if asked
    /// for far more than the cap — which `read_to_end` (the old path) would do on
    /// an endless producer.
    #[test]
    fn read_capped_bounds_buffering_and_flags_truncation() {
        // The default output cap (LimitsPolicy::max_output_bytes == 1 MiB).
        const CAP: usize = 1 << 20;
        // An endless 'x' source that asserts it is never asked for more than the
        // cap plus a small probe/pipe slack.
        struct Endless {
            served: usize,
        }
        impl Read for Endless {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.served = self.served.saturating_add(b.len());
                assert!(
                    self.served <= CAP + 64 * 1024,
                    "read_capped over-read {} bytes (cap {CAP})",
                    self.served
                );
                b.fill(b'x');
                Ok(b.len())
            }
        }
        let (buf, truncated) = read_capped(Endless { served: 0 }, CAP);
        assert_eq!(buf.len(), CAP, "peak buffering bounded by the cap");
        assert!(
            truncated,
            "a source longer than the cap is flagged truncated"
        );

        // A short source is captured whole and NOT flagged.
        let (buf2, trunc2) = read_capped(&b"hello"[..], CAP);
        assert_eq!(buf2, b"hello");
        assert!(!trunc2, "a sub-cap source is not truncated");
    }

    #[test]
    fn read_capped_retries_an_interrupted_read() {
        struct InterruptedOnce {
            interrupted: bool,
            inner: std::io::Cursor<Vec<u8>>,
        }

        impl Read for InterruptedOnce {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
                }
                self.inner.read(buf)
            }
        }

        let reader = InterruptedOnce {
            interrupted: false,
            inner: std::io::Cursor::new(b"abcdef".to_vec()),
        };
        let (captured, truncated) = read_capped(reader, 4);

        assert_eq!(captured, b"abcd");
        assert!(truncated);
    }

    #[test]
    fn glob_walk_single_segment_and_subpath() {
        let lister = map_lister(&[
            (
                ".",
                vec![
                    ent("a.rs", false),
                    ent("b.rs", false),
                    ent("c.txt", false),
                    ent(".hidden.rs", false),
                    ent("src", true),
                ],
            ),
            ("./src", vec![ent("a.rs", false), ent("b.rs", false)]),
        ]);
        let mut allow = |_d: &Path| Ok(());
        // *.rs matches the two .rs files (sorted), hidden excluded.
        assert_eq!(
            expand_glob_walk("*.rs", None, &*lister, &mut allow, 64, 4096).unwrap(),
            vec!["a.rs", "b.rs"]
        );
        // No match → the literal pattern (nullglob off).
        assert_eq!(
            expand_glob_walk("zzz*", None, &*lister, &mut allow, 64, 4096).unwrap(),
            vec!["zzz*"]
        );
        // Sub-path keeps the directory prefix on each match.
        assert_eq!(
            expand_glob_walk("src/*.rs", None, &*lister, &mut allow, 64, 4096).unwrap(),
            vec!["src/a.rs", "src/b.rs"]
        );
    }

    #[test]
    fn glob_walk_multi_segment_and_recursive() {
        let lister = map_lister(&[
            (
                ".",
                vec![ent("a", true), ent("b", true), ent("x.rs", false)],
            ),
            ("./a", vec![ent("foo.rs", false), ent("sub", true)]),
            ("./b", vec![ent("bar.rs", false)]),
            ("./a/sub", vec![ent("deep.rs", false)]),
        ]);
        let mut allow = |_d: &Path| Ok(());
        // Multi-segment: `*/foo.rs` matches only where foo.rs exists.
        assert_eq!(
            expand_glob_walk("*/foo.rs", None, &*lister, &mut allow, 64, 4096).unwrap(),
            vec!["a/foo.rs"]
        );
        // Recursive `**`: `*.rs` at every level (cwd + all subdirs).
        assert_eq!(
            expand_glob_walk("**/*.rs", None, &*lister, &mut allow, 64, 4096).unwrap(),
            vec!["a/foo.rs", "a/sub/deep.rs", "b/bar.rs", "x.rs"]
        );
    }

    #[test]
    fn glob_walk_leashes_every_directory_and_denies_out_of_scope() {
        let lister = map_lister(&[
            (".", vec![ent("a", true), ent("x.rs", false)]),
            ("./a", vec![ent("secret.rs", false)]),
        ]);
        // A leash that refuses to read `./a` denies the whole recursive walk
        // (every directory the walk lists is fs_read-checked before listing).
        let mut deny_a = |d: &Path| {
            if d.to_string_lossy().contains("a") {
                Err(ToolError::denied("out of fs_read scope"))
            } else {
                Ok(())
            }
        };
        assert!(expand_glob_walk("**/*.rs", None, &*lister, &mut deny_a, 64, 4096).is_err());
    }

    /// #143: the total-match cap is config-driven, not a hard-coded const — a
    /// `max_matches` of 2 truncates a 4-match single-segment glob.
    #[test]
    fn glob_walk_respects_configured_match_cap() {
        let lister = map_lister(&[(
            ".",
            vec![
                ent("a.rs", false),
                ent("b.rs", false),
                ent("c.rs", false),
                ent("d.rs", false),
            ],
        )]);
        let mut allow = |_d: &Path| Ok(());
        let got = expand_glob_walk("*.rs", None, &*lister, &mut allow, 64, 2).unwrap();
        assert_eq!(got.len(), 2, "match cap of 2 must bound the result set");
    }

    /// #143: the `**` recursion-depth cap is config-driven — a `max_depth` of 1
    /// descends a single level and never reaches the deeper `sub` directory.
    #[test]
    fn glob_walk_respects_configured_depth_cap() {
        let lister = map_lister(&[
            (".", vec![ent("a", true), ent("x.rs", false)]),
            ("./a", vec![ent("foo.rs", false), ent("sub", true)]),
            ("./a/sub", vec![ent("deep.rs", false)]),
        ]);
        let mut allow = |_d: &Path| Ok(());
        // depth 1: cwd + one level of dirs; `a/sub/deep.rs` is out of reach.
        let got = expand_glob_walk("**/*.rs", None, &*lister, &mut allow, 1, 4096).unwrap();
        assert!(
            !got.iter().any(|m| m.contains("deep.rs")),
            "depth cap of 1 must not reach a/sub/deep.rs; got {got:?}"
        );
    }

    /// #143: the variable allowlist is config-driven — a name absent from the
    /// default set is expandable when configured, and a default name is denied
    /// when configured out. Proves `is_allowed_var` reads the passed allowlist.
    #[test]
    fn var_allowlist_is_config_driven() {
        // A custom var (not in the default set) is allowed when configured.
        let allow_custom = vec!["MY_CUSTOM_VAR".to_string()];
        let env = FakeEnv(HashMap::from([(
            "MY_CUSTOM_VAR".to_string(),
            "/data".to_string(),
        )]));
        let out = expand_redirect_target(&[Seg::Var("MY_CUSTOM_VAR".into())], &env, &allow_custom)
            .unwrap();
        assert_eq!(out, "/data");
        // A default-allowlisted name (HOME) is denied when configured out.
        assert!(!is_allowed_var("HOME", &["PWD".to_string()]));
        assert!(is_allowed_var("PWD", &["PWD".to_string()]));
    }

    /// #145 (I6): the egress audit sink is built from the configured path
    /// (`LimitsPolicy::audit_sink`), not a direct `BRIDLE_NET_AUDIT` env read.
    /// `None` ⇒ the null sink (no file); `Some(path)` ⇒ a JSONL sink writing to
    /// exactly that path. Would fail on the old env-only path.
    #[test]
    fn net_audit_sink_is_config_driven() {
        use crate::net_proxy::{NetAuditEvent, NetDecision, NetKind};
        let ev = NetAuditEvent {
            ts_ms: 0,
            host: "example.test".to_string(),
            port: 443,
            kind: NetKind::Connect,
            decision: NetDecision::Allowed,
            bytes_up: 1,
            bytes_down: 2,
            dur_ms: 3,
        };
        // None → null sink: records silently, no file.
        net_audit_sink(None).record(&ev);

        // Some(path) → JSONL sink appends the event to that exact path.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ab-audit-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create isolated audit test directory");
        let path = dir.join("audit.jsonl");
        let sink = net_audit_sink(path.to_str());
        sink.record(&ev);
        drop(sink);
        let contents = std::fs::read_to_string(&path).expect("configured audit file written");
        assert!(
            contents.contains("example.test"),
            "the configured sink must write the event: {contents}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #138 (audit robustness): a *bad* audit path must degrade to the null sink so
    /// the run continues — a broken audit config can never break confinement. The
    /// sink records without panic and no file is created at the unopenable path.
    #[test]
    fn net_audit_sink_bad_path_degrades_to_null() {
        use crate::net_proxy::{NetAuditEvent, NetDecision, NetKind};
        let ev = NetAuditEvent {
            ts_ms: 0,
            host: "example.test".to_string(),
            port: 443,
            kind: NetKind::Http,
            decision: NetDecision::Allowed,
            bytes_up: 1,
            bytes_down: 2,
            dur_ms: 3,
        };
        // A path under a nonexistent directory can't be created → NullSink fallback.
        let bad = std::env::temp_dir()
            .join(format!("ab-nope-{}", std::process::id()))
            .join("does/not/exist/audit.jsonl");
        let sink = net_audit_sink(bad.to_str());
        sink.record(&ev); // must not panic
        assert!(
            !bad.exists(),
            "a bad audit path must not create a file (degraded to null): {bad:?}"
        );
    }
}
