//! [`CaveatInterceptor`] — the worker-local capability hook for free-form shell.
//!
//! Static [`CmdExecFilter`] and [`FileOpenFilter`] policies gate the final
//! external command and every shell-originated file open. Execution admission
//! follows command transformations and checks the final program, not its source
//! spelling or argv[0]. A simple-command filter observes cancellation even for
//! builtin-only loops. Admitted external programs do not re-enter these filters
//! for their own opens or descendants; their inherited L3 boundary confines them.
//!
//! [`CaveatInterceptor`] carries one invocation's **effective** caveats (the
//! `ToolContext` minted by the gate) and delegates every decision to the
//! *shared* leash logic on [`ToolContext`] — [`ToolContext::check_exec`],
//! [`ToolContext::check_path_read`], [`ToolContext::check_path_write`]. It does
//! **not** duplicate the canonicalizing path check; the one in
//! `agent-bridle-core` (realpath, reject symlink/`..` escapes) is the single
//! source of truth.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_bridle_core::{Denial, DenialKind, ToolContext};
use brush_core::extensions::ShellExtensions;
use brush_core::filter::{
    CmdExecFilter, ExternalCmdParams, FileOpenAccess, FileOpenFilter, FileOpenParams,
    PreFilterResult, SimpleCmdOutput, SimpleCmdParams,
};

/// Whether a path names the process descriptor namespace rather than a normal
/// filesystem object.
///
/// These paths can duplicate already-open descriptors. They are therefore not
/// ordinary `fs_read`/`fs_write` authority: while private worker/carried
/// authentication is in flight, admitting one could expose a control endpoint
/// that was intentionally never placed in model-visible data.
pub(crate) fn is_private_descriptor_path(path: &Path) -> bool {
    use std::path::Component;

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => parts.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
        }
    }
    match parts.as_slice() {
        [dev, fd, ..] if dev == "dev" && fd == "fd" => true,
        [proc, owner, fd, ..]
            if proc == "proc"
                && fd == "fd"
                && (owner == "self"
                    || owner == "thread-self"
                    || owner.bytes().all(|byte| byte.is_ascii_digit())) =>
        {
            true
        }
        _ => false,
    }
}

/// The shared, per-invocation denial sink.
///
/// brush clones the [`CaveatInterceptor`] internally (the trait requires
/// `Clone`), so to see *every* denial the shell hit we cannot store the log in
/// the struct by value — each clone would get its own copy. The `Arc<Mutex<_>>`
/// makes all clones write to one vec. The `ShellTool` creates a *fresh* sink per
/// `invoke`, so two concurrent invocations never share one — that is what keeps
/// denials from cross-contaminating across invocations.
pub(crate) type DenialSink = Arc<Mutex<Vec<Denial>>>;

/// The shared, per-invocation **allow memo** (B1.3).
///
/// A confined loop re-spawns the same program thousands of times
/// (`while read f; do wc -l "$f"; done` → N identical `/usr/bin/wc` admissions),
/// and [`ToolContext::check_exec`] recomputes the identical answer every time.
/// This set records programs already admitted **under this invocation's `cx`** so
/// the second and later admissions short-circuit to `Allow`.
///
/// # Security invariant (the one thing to enforce in review)
///
/// This is a **pure memo, not an authority**. It is sound only because:
///
/// 1. **The key is the whole admission input.** `check_exec` is a function of
///    exactly `(cx.effective.exec, program)`. `cx` is fixed for the invocation
///    (`ToolContext::effective` is private with no setter — a running tool cannot
///    widen its own caveats), so for a fixed `cx` the answer is a function of
///    `program` alone. Memoizing therefore returns the *same* decision the
///    recomputation would, never a different one.
/// 2. **One cache belongs to exactly one `cx`.** The cache is created *inside*
///    [`CaveatInterceptor::new`], which is the only place a `cx` is installed, and
///    there is no constructor, setter, or accessor that lets a caller inject or
///    share a cache. So a cache cannot outlive, or be reused across, the
///    invocation whose caveats minted it — an `Allow` can never bleed across
///    leashes. The [`Default`] (fail-closed) interceptor gets `None` and caches
///    nothing.
/// 3. **Only `Allow` is memoized.** Denials are recomputed and re-recorded every
///    time, exactly as before, so the denial log and its telemetry are unchanged.
/// 4. **Cancellation is checked before the memo.** Both the simple-command
///    filter and final authorization observe the invocation's cancellation flag.
///
/// `Arc<Mutex<_>>` for the same reason as [`DenialSink`]: brush clones the
/// interceptor internally, and every clone must consult the one shared memo.
pub(crate) type AllowCache = Arc<Mutex<HashSet<String>>>;

/// A brush command and file-open policy that enforces an invocation's effective
/// caveats inside the dedicated Brush worker **and records each denial it
/// makes** into a shared sink.
///
/// Holds an `Option<ToolContext>`:
///
/// - `Some(cx)` — enforce `cx`'s effective caveats (the normal, constructed
///   case). Built per-invocation via [`CaveatInterceptor::new`].
/// - `None` — the [`Default`] value. **Conservatively denies everything.** The
///   trait requires `Default`, but a `ToolContext` is un-forgeable (no public
///   constructor by design), so the default cannot carry caveats; denying is the
///   only safe behavior. A default-constructed interceptor must never reach a
///   live shell — but if one ever did, it is fail-closed, not allow-all.
///
/// Every `Deny` is appended to the [`DenialSink`] so the shell tool can read a
/// **structured** signal after the run instead of string-matching stderr. An
/// `Allow` records nothing — so a permitted command that exits non-zero on its
/// own (e.g. exit 126) is never mistaken for a leash denial.
#[derive(Debug, Clone, Default)]
pub struct CaveatInterceptor {
    /// The minted context whose effective caveats gate this shell, or `None`
    /// for the fail-closed default.
    cx: Option<ToolContext>,
    /// Shared sink every denial is recorded into. `None` only for the
    /// [`Default`] interceptor (which still denies, just records nothing —
    /// it never reaches a live shell).
    sink: Option<DenialSink>,
    /// Per-run cancellation flag (FIX 2). When an outer caller (the wall-clock
    /// timeout, or a future interrupt) trips this, the next `pre_simple_cmd` —
    /// or `pre_open_file`, for a redirect opened outside any command — terminates
    /// the run. `None` (the default) means "no cancellation wired".
    cancel: Option<Arc<AtomicBool>>,
    /// Per-invocation memo of programs already admitted under `cx` (B1.3).
    /// `None` for the [`Default`] interceptor, which denies everything and so
    /// has nothing to memoize. See [`AllowCache`] for the security invariant.
    allow_cache: Option<AllowCache>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    broker: Option<Arc<crate::broker_runtime::WorkerBroker>>,
}

impl CaveatInterceptor {
    /// Build an interceptor that enforces `cx`'s effective caveats and records
    /// every denial it makes into `sink`.
    ///
    /// The allow memo (B1.3) is minted **here**, together with `cx` — that is what
    /// structurally ties one cache to exactly one caveat set. There is
    /// deliberately no way to pass one in: see [`AllowCache`].
    #[must_use]
    pub(crate) fn new(cx: ToolContext, sink: DenialSink) -> Self {
        Self {
            cx: Some(cx),
            sink: Some(sink),
            cancel: None,
            allow_cache: Some(Arc::new(Mutex::new(HashSet::new()))),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            broker: None,
        }
    }

    /// Whether `program` was already admitted under this invocation's `cx`.
    fn is_memoized_allow(&self, program: &str) -> bool {
        self.allow_cache.as_ref().is_some_and(|c| {
            c.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(program)
        })
    }

    /// Memoize an `Allow` for `program`. Only ever called after a real
    /// [`ToolContext::check_exec`] returned `Ok`.
    fn memoize_allow(&self, program: &str) {
        if let Some(cache) = &self.allow_cache {
            cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(program.to_string());
        }
    }

    /// Wire a per-run cancellation flag (FIX 2). When tripped, the next
    /// `pre_simple_cmd` terminates the run.
    #[must_use]
    pub(crate) fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn with_broker(mut self, broker: Arc<crate::broker_runtime::WorkerBroker>) -> Self {
        self.broker = Some(broker);
        self
    }

    /// Whether the run has been cancelled. `false` when no flag is wired.
    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
    }

    /// Record a cancelled run's refusal as a structured denial and return the
    /// reason to hand back as a `Deny`. This is the fail-closed direction — it
    /// refuses, never permits — so the OCAP guarantee is preserved.
    fn cancelled(&self, kind: DenialKind, target: impl Into<String>) -> String {
        const REASON: &str = "run cancelled (timeout or interrupt)";
        self.record(kind, target, REASON);
        REASON.to_string()
    }

    /// Record a denial into the shared sink (a no-op if there is no sink).
    fn record(&self, kind: DenialKind, target: impl Into<String>, reason: impl Into<String>) {
        if let Some(sink) = &self.sink {
            // A poisoned mutex would only happen if a brush callback panicked
            // mid-record; recover the inner vec rather than poison-propagate so
            // a single bad record cannot lose the rest of the denial log.
            let mut guard = sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.push(Denial {
                kind,
                target: target.into(),
                reason: reason.into(),
            });
        }
    }
}

impl CaveatInterceptor {
    /// **The cancellation seam.** Fires once per command — builtin, function,
    /// and external alike — so a run can be stopped wherever it is, and returns
    /// a `Deny` that *terminates* the run rather than becoming an exit status an
    /// enclosing loop would shrug off (`brush_core::error::Error::is_terminating`).
    ///
    /// This is what bounds a PURE-BUILTIN runaway (`while true; do :; done`),
    /// which reaches neither the external-spawn funnel (`authorize_external_cmd`) nor the
    /// file-open one (`pre_open_file`) and so had no observation point at all.
    ///
    /// It makes **no capability decision**: admission stays in `authorize_external_cmd` /
    /// `pre_open_file`, which have the resolved program path and the canonicalized
    /// file path this hook does not. Uncancelled runs always `Allow` — the hot
    /// path is one relaxed atomic load, no allocation.
    fn check_cancelled(&self, name: &str) -> Result<(), brush_core::Error> {
        if self.is_cancelled() {
            return Err(cancelled_error(self.cancelled(DenialKind::Exec, name)));
        }
        Ok(())
    }

    /// Deny unless the effective `exec` caveat allows `program`.
    ///
    /// `program` is what brush is about to spawn: for `PATH`-resolved commands
    /// the resolved absolute path, and for path-separator commands the path as
    /// written (`/bin/rm`, `./x`). We hand that string to
    /// [`ToolContext::check_exec`], which allows it if the `exec` scope contains
    /// it verbatim OR contains its basename — so a bare-name grant (`["git"]`)
    /// matches the resolved `/usr/bin/git`, while `/bin/rm` is denied whenever
    /// neither `rm` nor `/bin/rm` is granted (the path-separator bypass stays
    /// closed at the funnel).
    ///
    /// Cancellation is checked before the memo even when a custom carried
    /// dispatch calls final authorization directly.
    fn check_exec(&self, program: &str, args: &[String]) -> Result<(), brush_core::Error> {
        self.check_cancelled(program)?;
        #[cfg(feature = "carried-coreutils")]
        let logical = crate::coreutils_dispatch::logical_carried_command(program, args);
        #[cfg(feature = "carried-coreutils")]
        let program = logical.as_deref().unwrap_or(program);
        #[cfg(not(feature = "carried-coreutils"))]
        let _ = args;
        if self.is_memoized_allow(program) {
            return Ok(());
        }
        match &self.cx {
            Some(cx) => match cx.check_exec(program) {
                Ok(()) => {
                    // Memoize only the affirmative decision; denials are
                    // recomputed and re-recorded every time (below), so the
                    // denial log is bit-for-bit what it was before B1.3.
                    self.memoize_allow(program);
                    Ok(())
                }
                Err(e) => {
                    let reason = e.to_string();
                    // Record the denial as a structured signal BEFORE returning.
                    self.record(DenialKind::Exec, program, &reason);
                    Err(permission_error(reason))
                }
            },
            // Fail-closed default: no caveats means no authority.
            None => {
                let reason = "no effective caveats (default interceptor); exec denied".to_string();
                self.record(DenialKind::Exec, program, &reason);
                Err(permission_error(reason))
            }
        }
    }

    /// Deny unless the effective `fs_read`/`fs_write` caveat allows `path`.
    ///
    /// `write` selects the axis. Both checks canonicalize first (realpath) and
    /// reject paths that escape the granted scope via `..` or a symlink — that
    /// logic is the shared one in `agent-bridle-core`, reused here, not copied.
    ///
    /// Checks cancellation before any special-path allowance: a redirect on a
    /// COMPOUND command (`while …; done > f`) is opened by the interpreter's
    /// redirect setup, outside any command dispatch, so `pre_simple_cmd` is not
    /// provably ahead of this hook. Checked before the /dev/null allowance below
    /// so cancellation wins outright. Fail-closed and cheap.
    fn check_open(&self, path: &Path, write: bool) -> Result<(), brush_core::Error> {
        if self.is_cancelled() {
            return Err(cancelled_error(
                self.cancelled(DenialKind::Open, path.to_string_lossy()),
            ));
        }
        // newt#969: the standard sinks are ALWAYS-permitted write targets.
        // `2>/dev/null` is the most common idiom in shell training data, and
        // writing to /dev/null|stdout|stderr is not a filesystem mutation in
        // any capability sense — no data persists, nothing is created. A
        // closed 3-item whitelist, not a general redirect grant.
        if write
            && matches!(
                path.to_str(),
                Some("/dev/null" | "/dev/stdout" | "/dev/stderr")
            )
        {
            return Ok(());
        }
        if is_private_descriptor_path(path) {
            let reason =
                "raw process descriptor paths are reserved for private control".to_string();
            self.record(DenialKind::Open, path.to_string_lossy(), &reason);
            return Err(permission_error(reason));
        }
        let Some(cx) = &self.cx else {
            let reason = "no effective caveats (default interceptor); open denied".to_string();
            self.record(DenialKind::Open, path.to_string_lossy(), &reason);
            return Err(permission_error(reason));
        };
        let result = if write {
            cx.check_path_write(path)
        } else {
            cx.check_path_read(path)
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                let reason = e.to_string();
                self.record(DenialKind::Open, path.to_string_lossy(), &reason);
                Err(permission_error(reason))
            }
        }
    }
}

fn permission_error(reason: String) -> brush_core::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, reason).into()
}

fn cancelled_error(reason: String) -> brush_core::Error {
    brush_core::Error::from(std::io::Error::new(std::io::ErrorKind::Interrupted, reason))
        .into_terminating()
}

impl CmdExecFilter for CaveatInterceptor {
    async fn pre_external_cmd<'a, SE: ShellExtensions>(
        &self,
        params: ExternalCmdParams<'a, SE>,
    ) -> PreFilterResult<ExternalCmdParams<'a, SE>, brush_core::filter::ExternalCmdOutput> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let mut params = params;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(broker) = &self.broker {
            if let Err(error) = broker.prepare(&mut params) {
                return PreFilterResult::Return(Err(
                    permission_error(error.to_string()).into_terminating()
                ));
            }
        }
        PreFilterResult::Continue(params)
    }

    async fn external_cmd_spawned(
        &self,
        command: &brush_core::filter::ExternalCommand,
        pid: Option<u32>,
    ) -> Result<(), brush_core::Error> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(broker) = &self.broker {
            broker
                .spawned(command, pid)
                .map_err(|error| permission_error(error.to_string()).into_terminating())?;
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = (command, pid);
        Ok(())
    }
    async fn pre_simple_cmd<'a, SE: ShellExtensions>(
        &self,
        params: SimpleCmdParams<'a, SE>,
    ) -> PreFilterResult<SimpleCmdParams<'a, SE>, SimpleCmdOutput> {
        match self.check_cancelled(params.command_name()) {
            Ok(()) => PreFilterResult::Continue(params),
            Err(error) => PreFilterResult::Return(Err(error)),
        }
    }

    async fn authorize_external_cmd<SE: ShellExtensions>(
        &self,
        params: &ExternalCmdParams<'_, SE>,
    ) -> Result<(), brush_core::Error> {
        // A transform can replace program, cwd, argv, and environment. Admit
        // this final launch state; original_command() and argv0 are metadata.
        self.check_cancelled(&params.original_command().to_string_lossy())?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(broker) = &self.broker {
            broker
                .check_delegations(&params.command)
                .map_err(|error| permission_error(error.to_string()))?;
        } else if !params.command.delegated_fds().is_empty() {
            return Err(permission_error(
                "no broker declared these private descriptors".to_owned(),
            ));
        }
        let deny = |reason: &str| {
            self.record(
                DenialKind::Exec,
                params.command.program().to_string_lossy(),
                reason,
            );
            permission_error(reason.to_owned())
        };
        let cwd = params
            .command
            .current_dir()
            .filter(|cwd| cwd.is_absolute())
            .ok_or_else(|| {
                deny("final executable requires an explicit absolute working directory")
            })?;
        self.check_open(cwd, false)?;
        if !params.command.env_clear() {
            return Err(deny(
                "final executable must not inherit the worker's ambient environment",
            ));
        }
        let supplied = Path::new(params.command.program());
        let program = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else if supplied.components().count() > 1 {
            cwd.join(supplied).components().collect()
        } else {
            return Err(deny(
                "final executable is unresolved; an explicit program path is required",
            ));
        };
        let program = program
            .to_str()
            .ok_or_else(|| deny("executable path is not representable by the exec grant"))?;
        // Only the carried protocol's fixed ASCII fields are inspected here.
        // Actual OsString arguments and the prepared environment are forwarded
        // unchanged by the launch path after authorization.
        let args: Vec<String> = params
            .command
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.check_exec(program, &args)
    }
}

impl FileOpenFilter for CaveatInterceptor {
    fn pre_open_file<SE: ShellExtensions>(
        &self,
        params: FileOpenParams<'_, SE>,
    ) -> Result<(), brush_core::Error> {
        match params.access {
            FileOpenAccess::Read => self.check_open(params.path, false),
            FileOpenAccess::Write => self.check_open(params.path, true),
            FileOpenAccess::ReadWrite => {
                self.check_open(params.path, false)?;
                self.check_open(params.path, true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridle_core::{Caveats, Gate, Scope, Tool, ToolResult};

    /// Mint a `ToolContext` the only legitimate way — through the gate — using a
    /// trivial tool with default (`top`) requirements so `effective == granted`.
    fn ctx(granted: Caveats) -> ToolContext {
        struct AnyTool;
        #[async_trait::async_trait]
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
                _cx: &ToolContext,
            ) -> ToolResult<serde_json::Value> {
                Ok(serde_json::Value::Null)
            }
        }
        Gate::new(0)
            .authorize(&AnyTool, &granted)
            .expect("authorize")
    }

    /// Build an interceptor with a fresh sink, returning both so a test can
    /// assert on what was recorded.
    fn interceptor_with_sink(granted: Caveats) -> (CaveatInterceptor, DenialSink) {
        let sink: DenialSink = Arc::new(Mutex::new(Vec::new()));
        let interceptor = CaveatInterceptor::new(ctx(granted), Arc::clone(&sink));
        (interceptor, sink)
    }

    /// Snapshot the sink's recorded denials.
    fn drain(sink: &DenialSink) -> Vec<Denial> {
        sink.lock().unwrap().clone()
    }

    #[test]
    fn default_is_fail_closed() {
        let interceptor = CaveatInterceptor::default();
        assert!(interceptor.check_exec("echo", &[]).is_err());
        assert!(interceptor.check_open(Path::new("/tmp"), false).is_err());
    }

    #[test]
    fn exec_allows_in_scope_denies_out_of_scope() {
        let (interceptor, sink) = interceptor_with_sink(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        assert!(matches!(interceptor.check_exec("echo", &[]), Ok(())));
        assert!(interceptor.check_exec("rm", &["-rf".to_string()]).is_err());
        // An Allow records nothing; only the Deny is in the sink.
        let recorded = drain(&sink);
        assert_eq!(
            recorded.len(),
            1,
            "exactly one denial expected: {recorded:?}"
        );
        assert_eq!(recorded[0].kind, DenialKind::Exec);
        assert_eq!(recorded[0].target, "rm");
        assert!(recorded[0].reason.contains("not within the granted"));
    }

    #[test]
    fn exec_denies_path_separator_spelled_command() {
        // The load-bearing case the hook exists for: `/bin/rm` is denied because
        // `/bin/rm` is not within `exec: Only{echo}` — the path-separator bypass
        // is closed, since the hook fires even for path-separator commands.
        let (interceptor, sink) = interceptor_with_sink(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        assert!(interceptor
            .check_exec("/bin/rm", &["-rf".to_string()])
            .is_err());
        // The denial records the path-separator program verbatim.
        let recorded = drain(&sink);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].kind, DenialKind::Exec);
        assert_eq!(recorded[0].target, "/bin/rm");
    }

    #[test]
    fn allow_records_nothing_in_sink() {
        // A permitted exec must NOT leave a denial — this is what keeps a
        // permitted command that exits 126 on its own from being flagged.
        let (interceptor, sink) = interceptor_with_sink(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        assert!(matches!(interceptor.check_exec("echo", &[]), Ok(())));
        assert!(drain(&sink).is_empty(), "an Allow must record nothing");
    }

    #[test]
    fn dev_null_sinks_are_always_writable() {
        // newt#969: `cmd 2>/dev/null` must never be a capability denial — the
        // sinks are not mutations. Even with NO caveats (fail-closed default
        // interceptor), the three standard sinks stay writable; everything
        // else keeps failing closed.
        let i = CaveatInterceptor::default();
        assert!(matches!(i.check_open(Path::new("/dev/null"), true), Ok(())));
        assert!(matches!(
            i.check_open(Path::new("/dev/stdout"), true),
            Ok(())
        ));
        assert!(matches!(
            i.check_open(Path::new("/dev/stderr"), true),
            Ok(())
        ));
        // Not a general /dev grant, and reads are unaffected by the whitelist.
        assert!(i.check_open(Path::new("/dev/sda"), true).is_err());
        assert!(i.check_open(Path::new("/etc/passwd"), true).is_err());
    }

    #[test]
    fn raw_descriptor_namespaces_are_denied_even_with_ambient_fs() {
        let (interceptor, sink) = interceptor_with_sink(Caveats::top());
        for path in [
            "/dev/fd/0",
            "/dev/ignored/../fd/9",
            "/proc/self/fd/3",
            "/proc/thread-self/fd/4",
            "/proc/1234/fd/5",
        ] {
            assert!(
                interceptor.check_open(Path::new(path), false).is_err(),
                "{path} must not duplicate a private descriptor"
            );
        }
        assert_eq!(drain(&sink).len(), 5);
        assert!(matches!(
            interceptor.check_open(Path::new("/proc/self/status"), false),
            Ok(())
        ));
    }

    #[test]
    fn open_write_uses_fs_write_axis() {
        let dir = std::env::temp_dir();
        let (interceptor, _sink) = interceptor_with_sink(Caveats {
            fs_write: Scope::only([dir.to_string_lossy().into_owned()]),
            ..Caveats::top()
        });
        // A new file under the allowed dir: write allowed.
        assert!(matches!(
            interceptor.check_open(&dir.join("ab-interceptor-ok.txt"), true),
            Ok(())
        ));
        // Clearly outside the allowed dir: write denied.
        assert!(interceptor
            .check_open(Path::new("/etc/shadow"), true)
            .is_err());
    }

    #[test]
    fn open_denial_is_recorded_as_open_kind() {
        let dir = std::env::temp_dir();
        let (interceptor, sink) = interceptor_with_sink(Caveats {
            fs_write: Scope::only([dir.to_string_lossy().into_owned()]),
            ..Caveats::top()
        });
        let _ = interceptor.check_open(Path::new("/etc/shadow"), true);
        let recorded = drain(&sink);
        assert_eq!(recorded.len(), 1, "one open denial expected: {recorded:?}");
        assert_eq!(recorded[0].kind, DenialKind::Open);
        assert_eq!(recorded[0].target, "/etc/shadow");
    }

    // ---- B1.3: the per-invocation allow memo -------------------------------

    /// The memo must not change any decision: repeated admissions return the
    /// same answer the uncached path computes, for both axes.
    #[test]
    fn memo_never_changes_a_decision() {
        let (interceptor, _sink) = interceptor_with_sink(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        for _ in 0..5 {
            assert!(matches!(interceptor.check_exec("echo", &[]), Ok(())));
            assert!(interceptor.check_exec("rm", &[]).is_err());
        }
    }

    /// Denials are NEVER memoized: each one is recomputed and re-recorded, so
    /// the denial log is exactly what it was before B1.3 (five attempts on the
    /// same out-of-scope program still yield five records).
    #[test]
    fn denials_are_recorded_every_time_not_memoized() {
        let (interceptor, sink) = interceptor_with_sink(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        for _ in 0..5 {
            let _ = interceptor.check_exec("rm", &[]);
        }
        let recorded = drain(&sink);
        assert_eq!(
            recorded.len(),
            5,
            "every denial must still be recorded: {recorded:?}"
        );
        assert!(recorded.iter().all(|d| d.kind == DenialKind::Exec));
    }

    /// An `Allow` memoized under one invocation's caveats MUST NOT be visible to
    /// another invocation with different caveats. This is the security invariant:
    /// the cache is minted inside `new` alongside the `cx`, so two interceptors
    /// never share one — `echo` allowed over here stays denied over there.
    #[test]
    fn memo_does_not_bleed_across_invocations_with_different_caveats() {
        let (permissive, _s1) = interceptor_with_sink(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        // Warm the permissive interceptor's memo.
        assert!(matches!(permissive.check_exec("echo", &[]), Ok(())));

        // A DIFFERENT invocation, whose caveats do not grant `echo`.
        let (restrictive, sink) = interceptor_with_sink(Caveats {
            exec: Scope::only(["ls".to_string()]),
            ..Caveats::top()
        });
        assert!(
            restrictive.check_exec("echo", &[]).is_err(),
            "a memoized Allow must never cross into an invocation with different caveats"
        );
        assert_eq!(drain(&sink).len(), 1, "and the denial is recorded");
    }

    /// brush clones the interceptor per pipeline stage; the clones must share the
    /// one memo (that is the whole point — otherwise each stage re-pays the
    /// admission). Sharing within ONE invocation is correct: same `cx`.
    #[test]
    fn clones_share_one_memo_within_an_invocation() {
        let (interceptor, _sink) = interceptor_with_sink(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        assert!(matches!(interceptor.check_exec("echo", &[]), Ok(())));
        let clone = interceptor.clone();
        assert!(
            clone.is_memoized_allow("echo"),
            "a clone must see the memo its sibling warmed"
        );
        assert!(matches!(clone.check_exec("echo", &[]), Ok(())));
    }

    /// The fail-closed default keeps no memo and still denies everything.
    #[test]
    fn default_interceptor_memoizes_nothing() {
        let interceptor = CaveatInterceptor::default();
        for _ in 0..3 {
            assert!(interceptor.check_exec("echo", &[]).is_err());
        }
        assert!(!interceptor.is_memoized_allow("echo"));
    }

    /// **The cancellation-ordering guard.** A program already memoized as
    /// allowed must not outlive a cancellation, including custom dispatch that
    /// enters final authorization directly.
    #[test]
    fn memoized_allow_does_not_outlive_cancellation() {
        let cancel = Arc::new(AtomicBool::new(false));
        let sink: DenialSink = Arc::new(Mutex::new(Vec::new()));
        let interceptor = CaveatInterceptor::new(ctx(Caveats::top()), Arc::clone(&sink))
            .with_cancel(Arc::clone(&cancel));

        // Warm the memo: `/bin/echo` is now a memoized Allow.
        assert!(matches!(interceptor.check_exec("/bin/echo", &[]), Ok(())));
        assert!(interceptor.is_memoized_allow("/bin/echo"));

        // Trip cancellation; even direct final admission must terminate before
        // consulting a previously warmed allow memo.
        cancel.store(true, Ordering::SeqCst);
        assert!(
            interceptor
                .check_exec("/bin/echo", &[])
                .unwrap_err()
                .is_terminating(),
            "a cancelled run must terminate even for a memoized-Allow program"
        );

        // The cancellation is recorded as a structured exec denial — the memo
        // did not swallow the telemetry either.
        let recorded = drain(&sink);
        assert_eq!(recorded.len(), 1, "one cancellation denial: {recorded:?}");
        assert_eq!(recorded[0].kind, DenialKind::Exec);
    }

    /// An uncancelled run's `pre_simple_cmd` makes no capability decision: it
    /// allows everything, leaving admission to `authorize_external_cmd`/`pre_open_file`.
    #[test]
    fn simple_command_allows_when_not_cancelled() {
        let (interceptor, sink) = interceptor_with_sink(Caveats::default());
        assert!(matches!(interceptor.check_cancelled("rm"), Ok(())));
        assert!(drain(&sink).is_empty(), "an Allow records no denial");
    }

    async fn policy_test_shell() -> brush_core::Shell {
        brush_core::Shell::builder()
            .do_not_inherit_env(true)
            .profile(brush_core::ProfileLoadBehavior::Skip)
            .rc(brush_core::RcLoadBehavior::Skip)
            .build()
            .await
            .expect("isolated shell")
    }

    #[tokio::test]
    async fn final_authorization_checks_final_program_and_cwd_not_source_or_argv0() {
        let shell = policy_test_shell().await;
        let cwd = std::env::temp_dir()
            .canonicalize()
            .expect("temporary directory");
        let allowed = cwd.join("allowed-program");
        let (policy, sink) = interceptor_with_sink(Caveats {
            exec: Scope::only([allowed.to_string_lossy().into_owned()]),
            fs_read: Scope::only([cwd.to_string_lossy().into_owned()]),
            ..Caveats::top()
        });
        let prepared = |program: &Path, cwd: &Path| {
            let mut command = brush_core::filter::ExternalCommand::new(program);
            command.set_current_dir(cwd);
            command.clear_env();
            command.set_argv0("allowed-program");
            ExternalCmdParams::with_original_command(&shell, "allowed-program", command)
        };
        assert!(policy
            .authorize_external_cmd(&prepared(&allowed, &cwd))
            .await
            .is_ok());
        assert!(
            policy
                .authorize_external_cmd(&prepared(Path::new("./allowed-program"), &cwd))
                .await
                .is_ok(),
            "separator-relative commands resolve against final cwd"
        );
        assert!(
            policy
                .authorize_external_cmd(&prepared(&cwd.join("denied-program"), &cwd))
                .await
                .is_err(),
            "an allowed original spelling and argv0 are not exec authority"
        );
        let outside = cwd.parent().expect("temporary directory has a parent");
        assert!(
            policy
                .authorize_external_cmd(&prepared(&allowed, outside))
                .await
                .is_err(),
            "final cwd must pass the existing fs read policy"
        );
        assert!(
            policy
                .authorize_external_cmd(&prepared(Path::new("allowed-program"), &cwd))
                .await
                .is_err(),
            "do not silently redo PATH resolution after final filters"
        );
        assert_eq!(drain(&sink).len(), 3);
    }

    #[tokio::test]
    async fn final_authorization_rejects_implicit_launch_state() {
        let shell = policy_test_shell().await;
        let cwd = std::env::temp_dir()
            .canonicalize()
            .expect("temporary directory");
        let program = cwd.join("fixture-program");
        let (policy, sink) = interceptor_with_sink(Caveats::top());
        let mut command = brush_core::filter::ExternalCommand::new(&program);
        command.clear_env();
        let params = ExternalCmdParams::new(&shell, command);
        assert!(policy
            .authorize_external_cmd(&params)
            .await
            .unwrap_err()
            .to_string()
            .contains("absolute working directory"));
        let mut command = brush_core::filter::ExternalCommand::new(&program);
        command.set_current_dir(cwd);
        let params = ExternalCmdParams::new(&shell, command);
        assert!(policy
            .authorize_external_cmd(&params)
            .await
            .unwrap_err()
            .to_string()
            .contains("ambient environment"));
        assert_eq!(drain(&sink).len(), 2);
    }

    #[tokio::test]
    async fn file_filter_read_write_requires_both_capability_axes() {
        let shell = policy_test_shell().await;
        let path = std::env::temp_dir()
            .canonicalize()
            .expect("temporary directory")
            .join("read-write-filter-fixture");
        for (read, write) in [(Scope::All, Scope::none()), (Scope::none(), Scope::All)] {
            let (policy, _) = interceptor_with_sink(Caveats {
                fs_read: read,
                fs_write: write,
                ..Caveats::top()
            });
            assert!(policy
                .pre_open_file(FileOpenParams::new(
                    &shell,
                    &path,
                    &path,
                    FileOpenAccess::ReadWrite,
                ))
                .is_err());
        }
        let (policy, _) = interceptor_with_sink(Caveats::top());
        assert!(policy
            .pre_open_file(FileOpenParams::new(
                &shell,
                &path,
                &path,
                FileOpenAccess::ReadWrite,
            ))
            .is_ok());
    }

    #[test]
    fn clones_share_one_sink() {
        // brush clones the interceptor; every clone must write to the same sink
        // (that is why the sink is Arc-shared). Two denials via two clones
        // appear in the one shared log.
        let (interceptor, sink) = interceptor_with_sink(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        let clone = interceptor.clone();
        let _ = interceptor.check_exec("rm", &[]);
        let _ = clone.check_exec("curl", &[]);
        let recorded = drain(&sink);
        assert_eq!(
            recorded.len(),
            2,
            "both clones' denials must land: {recorded:?}"
        );
        let targets: Vec<&str> = recorded.iter().map(|d| d.target.as_str()).collect();
        assert!(targets.contains(&"rm"));
        assert!(targets.contains(&"curl"));
    }
}
