//! Shell execution for run_command and lifecycle: environment, confinement,
//! host-process lifetime, and shell-envelope interpretation.

use super::super::content_spill::{self, SpillStore};
use super::super::display::ToolPresentation;
use super::super::permissions::{
    DenialKind, PermissionDecision, PermissionGate, PermissionRequest,
};
use super::live_output::{LiveOutputRelay, LiveOutputSession};
use super::output_budget::{
    self, cap_model_output, cap_model_output_with_handle, max_output_tokens, output_head_tokens,
};
use super::{denial_recovery_hint, full_access_requested, ocap_disabled};

pub fn venv_cmd_prefix() -> Option<String> {
    let venv = std::env::var("NEWT_VENV")
        .or_else(|_| std::env::var("VIRTUAL_ENV"))
        .ok();
    let exec_paths = std::env::var("NEWT_EXEC_PATHS").ok();

    if venv.is_none() && exec_paths.is_none() {
        return None;
    }

    // sh single-quoting: wrap in '', escape any ' as '\''
    let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));

    // Build a list of dirs to prepend to PATH (venv/bin first, then exec-paths).
    let mut path_dirs: Vec<String> = Vec::new();
    let mut prefix = String::new();

    if let Some(ref venv) = venv {
        let venv_bin = format!("{venv}/bin");
        prefix.push_str(&format!("export VIRTUAL_ENV={}; ", q(venv)));
        path_dirs.push(venv_bin);
    }
    if let Some(ref paths) = exec_paths {
        for dir in paths.split(':') {
            if !dir.is_empty() {
                path_dirs.push(dir.to_string());
            }
        }
    }

    if !path_dirs.is_empty() {
        let quoted: Vec<String> = path_dirs.iter().map(|d| q(d)).collect();
        prefix.push_str(&format!("export PATH={}:\"$PATH\"; ", quoted.join(":")));
    }

    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Build the venv/exec-path environment as a `{KEY:VALUE}` map for the confined
/// shell's structured `env` seam (agent-bridle, newt #783).
///
/// Same inputs as [`venv_cmd_prefix`] — `NEWT_VENV` (preferred) or
/// `$VIRTUAL_ENV`, plus `NEWT_EXEC_PATHS` — but delivered as host-supplied env
/// vars set directly on the spawned child instead of `export …;` text prepended
/// to the command. The `export` form is the #783 root cause: `export` is a
/// shell builtin, not a program, so the confined safe-subset engine refuses it
/// on a compound command (`a; b | c`). Passing the vars through the env seam
/// sidesteps that entirely and never touches the command text.
///
/// `PATH` is the venv `bin` (then any `NEWT_EXEC_PATHS` dirs) *prepended* to the
/// inherited host `PATH`: the env seam sets the value additively over the
/// child's ambient environment, so we read the host `PATH` here and build the
/// full string rather than relying on a `$PATH` expansion inside the value.
/// Returns an empty map when neither input is set (no env key is sent).
pub(super) fn venv_env_map() -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();

    // Env passthrough: the confined shell has NO ambient shell variables, so
    // without this brush cannot expand `~` (it resolves `~` from its `HOME` shell
    // var, erroring "HOME not set") and the command silently used a literal
    // `~/…` path — leaving `<cwd>/~/…` debris on disk. Seed a minimal,
    // operator-configurable allow-list from the process env (default HOME+USER;
    // widened via `[shell] env_passthrough`, published as
    // NEWT_SHELL_ENV_PASSTHROUGH). Each var is set only when present, so nothing
    // is fabricated and the default stays narrow (the confined shell is a trust
    // boundary — a wide passthrough would leak secrets into a sandboxed command).
    for var in shell_env_passthrough() {
        if let Ok(val) = std::env::var(&var) {
            map.insert(var, val);
        }
    }
    // File-sourced import (#1243 Leg 2): the `~/.newt/shell-env/` drop-in dir —
    // deliberate, allowlisted tokens/support vars whose VALUES live in files,
    // never in config.toml or newt's own process env. Merged over the ambient
    // passthrough (explicit operator intent wins); the engine-critical vars
    // (SHELL/VIRTUAL_ENV/PATH) set below still win over a same-named token file.
    if let Some(config_path) = crate::Config::user_config_path() {
        map.extend(crate::shell_env::from_config_dir(&config_path));
    }
    // Identify the confined engine so `env` / scripts can tell they're in newt's
    // shell (e.g. `SHELL=safe-subset` / `brush` / `host`), not the login shell.
    map.insert("SHELL".to_string(), shell_engine().as_str().to_string());

    let venv = std::env::var("NEWT_VENV")
        .or_else(|_| std::env::var("VIRTUAL_ENV"))
        .ok();
    let exec_paths = std::env::var("NEWT_EXEC_PATHS").ok();

    // Dirs to prepend to PATH (venv/bin first, then any exec-paths), mirroring
    // venv_cmd_prefix's ordering.
    let mut path_dirs: Vec<String> = Vec::new();
    if let Some(ref venv) = venv {
        map.insert("VIRTUAL_ENV".to_string(), venv.clone());
        path_dirs.push(format!("{venv}/bin"));
    }
    if let Some(ref paths) = exec_paths {
        for dir in paths.split(':') {
            if !dir.is_empty() {
                path_dirs.push(dir.to_string());
            }
        }
    }

    if !path_dirs.is_empty() {
        let prepend = path_dirs.join(":");
        let path = match std::env::var("PATH") {
            Ok(inherited) if !inherited.is_empty() => format!("{prepend}:{inherited}"),
            _ => prepend,
        };
        map.insert("PATH".to_string(), path);
    }

    map
}

/// The confined-shell env passthrough list: `NEWT_SHELL_ENV_PASSTHROUGH`
/// (colon-separated, published from `[shell] env_passthrough`) or the minimal
/// default (`HOME`, `USER`). Empty entries are dropped.
fn shell_env_passthrough() -> Vec<String> {
    match std::env::var("NEWT_SHELL_ENV_PASSTHROUGH") {
        Ok(s) if !s.trim().is_empty() => s
            .split(':')
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect(),
        _ => crate::config::shell_env_passthrough_default(),
    }
}

/// Build the dispatch args for agent-bridle's confined `shell` tool (#783): the
/// RAW user command (free-form `cmd` mode) plus the venv carried through the
/// structured `env` seam ([`venv_env_map`]). Deliberately NO `export …;` prefix
/// on `cmd` — that is what the confined safe-subset engine refuses on a
/// compound command (the #783 root cause); the env seam sets `VIRTUAL_ENV` /
/// `PATH` on the spawned child instead. The host-bypass (`--yolo`) path keeps
/// the prefix form because it runs on a real `/bin/sh` where `export` works.
/// Resolve an optional model-supplied `cwd` (#1159) against the workspace: a
/// relative dir joins under the workspace root; an absolute one is taken as-is
/// (the confined shell's fs fence rejects any path that escapes the workspace,
/// so this never widens reach). `None` runs at the workspace root, as before.
pub(super) fn resolve_exec_cwd(workspace: &str, cwd: Option<&str>) -> String {
    match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        None => workspace.to_string(),
        Some(c) if std::path::Path::new(c).is_absolute() => c.to_string(),
        Some(c) => std::path::Path::new(workspace)
            .join(c)
            .to_string_lossy()
            .into_owned(),
    }
}

/// Split a leading `cd <path> &&` (or `cd <path> ;`) off a `run_command`
/// string so the `cd` — a shell **builtin**, not an executable — never reaches
/// the confined exec layer, which would try to `execvp("cd")` and fail (on
/// macOS: `sandbox-exec: execvp() of 'cd' failed`). The models reliably prefix
/// `cd <workspace> && <real command>` out of habit; folding the `cd` into the
/// command's cwd runs the real command where the model meant, and — as a bonus
/// — the OCAP prompt then names the *real* capability (`git checkout -b …`)
/// instead of the opaque `cd … && git …`.
///
/// Only a SINGLE leading `cd` at the very start is folded (not `cd a && cd b`,
/// and not a `cd` deeper in a pipeline — those are left for the shell engine).
/// Folding happens only when the path is followed by a sequential connective
/// (`&&` / `;`) or end-of-string; an unusual `cd x | y` is left whole.
/// Returns `(cd_path, remainder)`; `remainder` is empty for a bare `cd <path>`.
pub(super) fn split_leading_cd(cmd: &str) -> (Option<String>, String) {
    let rest = match cmd.trim_start().strip_prefix("cd") {
        Some(r) if r.starts_with(char::is_whitespace) => r.trim_start(),
        _ => return (None, cmd.to_string()),
    };
    let (path, after) = parse_cd_path(rest);
    if path.is_empty() {
        return (None, cmd.to_string());
    }
    let after = after.trim_start();
    let remainder = if let Some(r) = after.strip_prefix("&&") {
        r.trim_start().to_string()
    } else if let Some(r) = after.strip_prefix(';') {
        r.trim_start().to_string()
    } else if after.is_empty() {
        String::new()
    } else {
        // `cd <path>` followed by something we don't confidently understand
        // (a pipe, `||`, a redirection) — don't fold; hand the whole string to
        // the engine.
        return (None, cmd.to_string());
    };
    (Some(path), remainder)
}

/// Parse the first path token of a `cd` argument: a single/double-quoted string
/// (returned unquoted) or an unquoted run up to the next whitespace. Returns
/// `(path, remainder_after_the_token)`.
fn parse_cd_path(s: &str) -> (String, &str) {
    let first = s.chars().next();
    if let Some(q @ ('"' | '\'')) = first {
        if let Some(end) = s[1..].find(q) {
            return (s[1..=end].to_string(), &s[end + 2..]);
        }
    }
    match s.find(char::is_whitespace) {
        Some(i) => (s[..i].to_string(), &s[i..]),
        None => (s.to_string(), ""),
    }
}

pub(super) fn confined_dispatch_args(cmd: &str, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "cmd": cmd,
        "cwd": cwd,
        "env": venv_env_map(),
    })
}

/// The shell engine selected for this dispatch (ADR 0005 D2 seam). An explicit
/// `[shell] engine` / `--shell-engine` choice is published by the CLI through
/// `NEWT_SHELL_ENGINE`, so deep `run_command` dispatch reads it without threading
/// it through every signature. Full-access sessions retain their platform
/// default; otherwise the confined default is resolved below from the current L3
/// fence state rather than cached at startup.
pub(super) fn shell_engine() -> crate::ShellEngine {
    if let Some(engine) = std::env::var("NEWT_SHELL_ENGINE")
        .ok()
        .and_then(|s| s.parse::<crate::ShellEngine>().ok())
    {
        return engine;
    }
    // No engine was published (e.g. a non-CLI entry point that set
    // NEWT_FULL_ACCESS directly). Honor the same auto-upgrade the CLI applies so
    // `NEWT_FULL_ACCESS=1` alone still gets the full-grammar engine (`host` on
    // unix, `brush` on Windows).
    if full_access_requested() {
        return crate::full_access_default_engine();
    }
    // #1243 Leg 1: the CONFINED default is L3-gated and resolved HERE, per
    // dispatch — `dispatch_bridled_shell()` calls this on every run_command, so the
    // fence state is re-checked at exec time and never cached at startup (the
    // agent-bridle #239 TLA+ TOCTOU obligation). Brush when a kernel fence
    // enforces on this host; safe-subset's structural refusal otherwise.
    crate::confined_default_engine(crate::ocap_l3_backend().1)
}

/// agent-bridle's tool registry with the `"shell"` tool bound to the selected
/// engine (the ADR 0005 D2 seam: `safe-subset` / `host` / `brush` all honor the
/// same `Tool` contract under the `"shell"` name). `web_fetch` is added
/// unchanged. Mirrors `agent_bridle::registry()` but swaps the shell engine so
/// `[shell] engine = "host"` (or `--full-access`) routes `run_command` to the
/// full-grammar, kernel-jailed sandbox-host engine instead of the safe subset.
/// The b1 sandbox policy every `run_command` shell engine runs under: the
/// default backend confinement PLUS [`agent_bridle::ChildNetworkPolicy::DenyDirect`]
/// — the seccomp `socket()`-family egress deny (agent-bridle 0.7.15) that closes
/// the UDP/DNS/raw/packet leg the Landlock TCP-only net rule misses.
///
/// Applied unconditionally, but **inert unless the caller's `net` caveat is
/// already deny-all** (`net: none`): a granted net scope leaves it untouched (the
/// caller asked for egress), while a hostile / confined `run_command` under
/// `net: none` gets NO off-box socket of any protocol — the live attacker-exec
/// path finally inheriting the same complete egress floor as the
/// `ConstrainedExecutor` callers. Fail-closed: if the seccomp floor cannot be
/// installed, the spawn is refused rather than run with a weaker floor.
fn b1_run_command_sandbox_policy() -> agent_bridle::SandboxPolicy {
    agent_bridle::SandboxPolicy {
        child_network: agent_bridle::ChildNetworkPolicy::DenyDirect,
        ..agent_bridle::SandboxPolicy::default()
    }
}

fn bridle_registry(
    engine: crate::ShellEngine,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> agent_bridle::Registry {
    use std::sync::Arc;
    let shell: Arc<dyn agent_bridle::Tool> = match engine {
        crate::ShellEngine::SafeSubset => {
            let mut tool =
                agent_bridle::ShellTool::new().with_sandbox_policy(b1_run_command_sandbox_policy());
            if let Some(observer) = live.clone() {
                tool = tool.with_output_observer(observer);
            }
            Arc::new(tool)
        }
        crate::ShellEngine::Host => {
            let mut tool = agent_bridle::HostShellTool::new()
                .sandbox_policy(Arc::new(b1_run_command_sandbox_policy()));
            if let Some(observer) = live.clone() {
                tool = tool.with_output_observer(observer);
            }
            Arc::new(tool)
        }
        crate::ShellEngine::Brush => {
            // Cargo's library-test harness is not the `newt` executable and
            // therefore cannot service bridle's worker re-exec handshake
            // (`--agent-bridle-worker brush`). Keep unit tests on the same
            // confined Tool contract without attempting to re-exec the test
            // harness; the real binary path remains Brush unchanged.
            #[cfg(test)]
            let shell = {
                let mut tool = agent_bridle::ShellTool::new()
                    .with_sandbox_policy(b1_run_command_sandbox_policy());
                if let Some(observer) = live {
                    tool = tool.with_output_observer(observer);
                }
                Arc::new(tool) as Arc<dyn agent_bridle::Tool>
            };
            #[cfg(not(test))]
            let shell = {
                // The carried brush engine (agent-bridle 0.7): in-process bash + the
                // L2 CommandInterceptor. The cross-platform engine — and on Windows
                // the ONLY full-grammar option, since `host` needs `/bin/sh`.
                #[cfg(windows)]
                {
                    use std::sync::Once;
                    static WARN: Once = Once::new();
                    WARN.call_once(|| {
                        tracing::warn!(
                            "using the 'brush' shell engine on Windows: run_command runs a \
                         bash-in-Rust shell for internal-tooling compatibility. Native \
                         PowerShell/cmd code paths are a FUTURE release — not written yet \
                         (we are opinionated Linux developers who occasionally use a \
                         MacBook). Bash-isms work; Windows-native shell semantics do not."
                        );
                    });
                }
                let mut tool = agent_bridle::BrushShellTool::new()
                    .with_sandbox_policy(Arc::new(b1_run_command_sandbox_policy()));
                if let Some(observer) = live {
                    tool = tool.with_output_observer(observer);
                }
                Arc::new(tool) as Arc<dyn agent_bridle::Tool>
            };
            shell
        }
    };
    agent_bridle::Registry::builder()
        .tool(shell)
        .tool(Arc::new(agent_bridle::WebFetchTool::new()))
        .build()
}

/// #1176: should an about-to-run command be shadow-recorded? Exactly when it
/// runs UNCONFINED — via either of the two unconfined routes:
/// - `host_bypass` — the `--yolo`/`--disable-ocap` host-shell bypass, or
/// - `full_access` — a `--full-access` session, whose caveats are
///   `Caveats::top()`, so the confined bridle dispatch runs effectively
///   unconfined and would otherwise learn nothing.
///
/// A genuinely confined session (`host_bypass == false` and no full-access) is
/// NOT recorded: its leash is real, so there is no shadow to catch. Pure over
/// the two booleans so the gating is unit-tested without a live shell. Before
/// #1176's full-access parity, only the host-bypass arm recorded — a bare
/// `--full-access` run armed the recorder yet never wrote.
pub(super) fn shadow_records(host_bypass: bool, full_access: bool) -> bool {
    host_bypass || full_access
}

/// Does the caller's effective exec FLOOR permit running `cmd` on the
/// UNCONFINED host shell?
///
/// `None` means the caller found no effective exec floor after composing the
/// session, posture, mode, and persona constraints. The floor therefore
/// imposes nothing and the `--disable-ocap` bypass behaves as it did pre-#307.
///
/// `Some(scope)` ⇒ the bypass may proceed ONLY for a single, simple command
/// whose program (leading token) the scope authorizes. This is deliberately
/// conservative on TWO counts, because the host shell runs `cmd` verbatim with
/// no per-spawn interceptor:
///
/// 1. A **compound** command (containing a shell metacharacter that could chain
///    another program — `&&`, `||`, `;`, `|`, `` ` ``, `$(`, newline, `&`, `>`,
///    `<`) is NOT allowed to bypass. `echo ok && rm -rf /` would otherwise
///    smuggle `rm` past an `echo` grant. It falls through to the confined
///    shell, which gates every spawn.
/// 2. Only the leading token is matched, so a bare allow-listed program runs;
///    anything else is denied.
///
/// The denied command isn't refused outright — it falls to the confined-shell
/// path, which enforces the already-composed effective `caveats`. Every active
/// exec floor therefore keeps its ceiling even under `--yolo`.
pub(super) fn exec_floor_permits(floor: Option<&crate::caveats::Scope<String>>, cmd: &str) -> bool {
    use crate::caveats::ScopeExt as _;
    let Some(scope) = floor else {
        return true; // no effective exec floor ⇒ bypass unchanged
    };
    // Conservative: any shell control/redirection metacharacter that could
    // introduce a second program defeats leading-token matching, so refuse the
    // bypass and let the confined shell gate each spawn.
    const SHELL_META: &[char] = &['&', '|', ';', '`', '$', '\n', '>', '<', '(', ')'];
    if cmd.contains(SHELL_META) {
        return false;
    }
    match cmd.split_ascii_whitespace().next() {
        // An empty command runs nothing; let it through to the normal path.
        None => true,
        Some(prog) => scope.permits(&prog.to_string()),
    }
}

/// INTERIM (#297): run `cmd` on the PLAIN host shell — no leash, no
/// interceptor, no sandbox — and wrap the outcome in an envelope structurally
/// identical to the confined shell's (`{ exit_code, stdout, stderr,
/// sandbox_kind }`, with `denied` / `denials` omitted exactly as the bridle
/// envelope omits them when nothing was denied). [`envelope_denied`] and
/// [`shell_envelope_output`] — and therefore the loop's truncation / denial /
/// exit-code handling — apply to it unchanged.
///
/// A spawn failure surfaces as `Err`, which the caller formats as the same
/// `error: …` string a bridle dispatch failure produces.
/// Run `cmd` through the SAME confined-shell path the `run_command` tool uses —
/// the venv env seam, the `--disable-ocap` host bypass under the #307 exec
/// floor, the agent-bridle confined shell, and the #263 permission-gate re-ask —
/// and render the envelope. Shared by the `run_command` and `lifecycle` (#891)
/// arms so both honor **identical** exec caveats; the central presenter owns
/// the tool-call and completed-result block.
pub(super) async fn dispatch_bridled_shell(
    args: serde_json::Value,
    caveats: &crate::caveats::Caveats,
    sink: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
) -> agent_bridle::ToolResult<serde_json::Value> {
    let mut live = LiveOutputSession::start(sink);
    // NOTE (cross-platform review, `unconfined-fallback-on-missing-backend`):
    // run_command dispatches at the DEFAULT (Advisory) strength floor. On a
    // supported platform whose native backend is present (Linux+Landlock,
    // macOS+Seatbelt, Windows+AppContainer) the fs/net fence is kernel-enforced,
    // so this is confined. But where a RESTRICTED fs/net axis has NO native
    // backend at runtime (`best_available_sandbox` = advisory `NoopSandbox`) this
    // route runs ADVISORY (host) rather than refusing — the ConstrainedExecutor
    // callers fail closed there (Kernel floor). A blanket Kernel floor here is
    // WRONG: run_command legitimately restricts `exec`, which Landlock enforces
    // only as `interceptor` (the exec-behavior-bound BOUNDED residual), so a
    // blanket Kernel floor would refuse every exec-restricted command even on
    // Landlock. The correct fix is a PER-AXIS floor at the bridle boundary
    // (fs/net = Kernel, exec = Interceptor-OK); tracked as an ACTIVE deviation.
    let result = bridle_registry(shell_engine(), live.as_ref().map(LiveOutputSession::relay))
        .dispatch("shell", args, caveats)
        .await;
    if let Some(live) = live.as_mut() {
        let ordinary_completion = result
            .as_ref()
            .ok()
            .and_then(|envelope| envelope.get("timed_out"))
            .and_then(serde_json::Value::as_bool)
            != Some(true);
        if result.is_ok() && ordinary_completion {
            live.finish_after_observer();
        } else {
            live.finish();
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn exec_confined_command(
    cmd: &str,
    // The directory the command runs in (#1159): the workspace root for
    // lifecycle, or a resolved workspace-confined cwd for run_command.
    cwd: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    permission_gate: Option<&mut dyn PermissionGate>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    presentation: &mut dyn ToolPresentation,
) -> String {
    // Venv injection (#783): the confined shell carries the venv via
    // agent-bridle's structured `env` seam (see `confined_dispatch_args` /
    // `venv_env_map`), NOT by prepending `export …;` to the command — an
    // `export` builtin is not a program, so the safe-subset engine refuses it on
    // a compound command (the #783 root cause). `cmd_with_venv` (the
    // `export …;`-prefixed form) is built ONLY for the host-bypass path below:
    // that runs on a real `/bin/sh` where `export` is a genuine builtin.
    let cmd_with_venv = match venv_cmd_prefix() {
        Some(prefix) => format!("{prefix}{cmd}"),
        None => cmd.to_string(),
    };

    // INTERIM (#297): --disable-ocap / --yolo / NEWT_DISABLE_OCAP=1 — run the
    // command UNCONFINED on the host shell instead of the bridle's confined
    // shell. Nothing is denied here, so the #263 permission gate below is never
    // consulted. #307 FLOOR: a named-permission-preset clamp WINS over the
    // bypass — the unconfined host path is taken ONLY if the floor permits this
    // command's leading token; else it falls through to the confined shell,
    // which enforces the already-clamped `caveats`. `None` keeps the bypass
    // bit-for-bit.
    let host_bypass = ocap_disabled() && exec_floor_permits(exec_floor, cmd);

    // #1176: shadow-OCAP — record the authority a leash WOULD have gated on
    // whenever this command runs UNCONFINED: the yolo/disable-ocap host bypass
    // above, OR a --full-access session (its caveats are `Caveats::top()`, so
    // the confined bridle dispatch below runs effectively unconfined and would
    // otherwise learn nothing). A genuinely confined session is not recorded —
    // its leash is real. No-op unless recording is armed (NEWT_FLIGHT_RECORDER).
    // `newt ocap propose` folds the capture into reviewable policy candidates.
    if shadow_records(host_bypass, full_access_requested()) {
        crate::flight_recorder::log_unconfined(cmd);
    }

    if host_bypass {
        let mut live = LiveOutputSession::start(live_tool_output);
        let run = host_shell_dispatch(
            &cmd_with_venv,
            cwd,
            live.as_ref().map(LiveOutputSession::relay),
        )
        .await;
        if let Some(live) = live.as_mut() {
            live.finish();
        }
        return match run {
            Ok(envelope) => shell_envelope_output(
                &envelope,
                tool_output_lines,
                color,
                tool_offload,
                spill_store,
                Some(&mut *presentation),
            ),
            Err(e) => format!("error: {e}"),
        };
    }

    // #783: RAW cmd + venv via the env seam — never the `export …;` prefix,
    // which the confined safe-subset engine refuses.
    let dispatch_args = confined_dispatch_args(cmd, cwd);
    match dispatch_bridled_shell(dispatch_args.clone(), caveats, live_tool_output.clone()).await {
        // The confined shell ran. Its envelope carries
        // `{ exit_code, stdout, stderr, timed_out, ... }` plus — when the leash
        // refused a capability — the STRUCTURED denial fields
        // `{ denied: true, denials: [{ kind, target, reason }] }`. In free-form
        // mode an out-of-scope command is denied *inside* the shell by the brush
        // interceptor (the command genuinely does not run); we lift that to the
        // capability-denied UX by reading the structured `denied` field — NEVER
        // a stderr grep.
        Ok(envelope) if envelope_denied(&envelope) => {
            // Repair evidence is distinct from the prompted-decision log: keep
            // the redacted raw command + structured refusal even when prompting
            // is off or the operator allows it. This is what lets `newt ocap
            // denials` distinguish a policy gap from a parser/implementation
            // defect instead of repeatedly granting a bogus target.
            crate::denial_journal::record_envelope(
                cmd,
                cwd,
                crate::denial_journal::DenialStage::Initial,
                &envelope,
            );
            // #263: an interactive gate may turn this denial into a human grant.
            // ONE consult + ONE re-execution per call: a second denial (a
            // different target reached on the re-run) surfaces as the standard
            // envelope — the model can retry, which prompts afresh.
            if let Some(gate) = permission_gate {
                // #905: promptable exec denials OR net-host denials (agent-bridle
                // #196). On Allow, the re-mint widens the matching axis (net adds
                // the host to the allow-list), so the proxy admits it on re-run.
                if let Some(requests) =
                    exec_denial_requests(&envelope).or_else(|| net_denial_requests(&envelope))
                {
                    if let PermissionDecision::Allow(widened) = gate.ask(&requests) {
                        return match dispatch_bridled_shell(
                            dispatch_args,
                            &widened,
                            live_tool_output,
                        )
                        .await
                        {
                            Ok(env2) if envelope_denied(&env2) => {
                                crate::denial_journal::record_envelope(
                                    cmd,
                                    cwd,
                                    crate::denial_journal::DenialStage::AfterGrant,
                                    &env2,
                                );
                                denied_run_command_result(&env2, color)
                            }
                            Ok(env2) => shell_envelope_output(
                                &env2,
                                tool_output_lines,
                                color,
                                tool_offload,
                                spill_store,
                                Some(&mut *presentation),
                            ),
                            Err(e) => format!("error: {e}"),
                        };
                    }
                }
            }
            denied_run_command_result(&envelope, color)
        }
        Ok(envelope) => shell_envelope_output(
            &envelope,
            tool_output_lines,
            color,
            tool_offload,
            spill_store,
            Some(&mut *presentation),
        ),
        // An argv-mode leash denial, or an error from inside the tool — surface
        // the reason; the dispatch error Display is safe to show.
        Err(e) => format!("error: {e}"),
    }
}

pub(super) async fn host_shell_dispatch(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> std::io::Result<serde_json::Value> {
    let run = host_shell_output(cmd, cwd, live).await?;
    Ok(serde_json::json!({
        "exit_code": run.exit_code,
        "stdout": decode_shell_stream(&run.stdout),
        "stderr": decode_shell_stream(&run.stderr),
        // Same `timed_out` flag the confined bridle envelope carries, so the
        // host-bypass path can never wedge the session on a hung child (#297).
        "timed_out": run.timed_out,
        // Honest provenance, same field the bridle envelope always carries:
        // nothing sandboxed this run.
        "sandbox_kind": "none",
    }))
}

/// Result of a host-bypass shell run. Unlike a raw [`std::process::Output`] this
/// carries an explicit `timed_out` flag so the dispatch layer can emit the same
/// envelope shape the confined path does when a child is killed for running long.
pub(super) struct HostShellRun {
    pub(super) exit_code: i64,
    pub(super) stdout: Vec<u8>,
    stderr: Vec<u8>,
    pub(super) timed_out: bool,
}

/// Wall-clock ceiling for a single host-bypass shell command. A child that
/// blocks past this (a REPL awaiting input, an accidental `cat` with no args,
/// an interactive prompt) is killed rather than wedging the whole turn — the
/// interrupt keyboard-watcher cannot help once a foreground child owns the tty.
///
/// Convention-driven: override with `NEWT_HOST_EXEC_TIMEOUT_SECS` (a positive
/// integer number of seconds). Absent/blank/invalid/zero falls back to the
/// 120s default, mirroring the confined shell's bound.
fn host_exec_timeout() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 120;
    let secs = std::env::var("NEWT_HOST_EXEC_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

pub(super) fn decode_shell_stream(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => repair_bsd_cat_v_utf8(bytes)
            .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// macOS/BSD `cat -v` is not Unicode-aware: for a UTF-8 glyph such as `─`
/// (`e2 94 80`) it emits the lead byte raw (`e2`) and renders only the
/// continuation bytes as ASCII meta-control notation (`M-^TM-^@`). That byte
/// stream is invalid UTF-8, so a plain lossy decode becomes `�M-^TM-^@`.
///
/// Repair only that precise shape. This keeps ordinary valid UTF-8 untouched and
/// leaves unrelated binary output on the existing lossy fallback.
fn repair_bsd_cat_v_utf8(bytes: &[u8]) -> Option<String> {
    let mut repaired = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        let lead = bytes[i];
        let Some(cont_count) = utf8_continuation_count(lead) else {
            repaired.push(lead);
            i += 1;
            continue;
        };

        let mut seq = Vec::with_capacity(cont_count + 1);
        seq.push(lead);
        let mut j = i + 1;
        let mut ok = true;
        for _ in 0..cont_count {
            match parse_cat_v_meta_byte(bytes, j) {
                Some((cont, next)) if (0x80..=0xbf).contains(&cont) => {
                    seq.push(cont);
                    j = next;
                }
                _ => {
                    ok = false;
                    break;
                }
            }
        }

        if ok && std::str::from_utf8(&seq).is_ok() {
            repaired.extend_from_slice(&seq);
            changed = true;
            i = j;
        } else {
            repaired.push(lead);
            i += 1;
        }
    }

    changed.then(|| String::from_utf8(repaired).ok()).flatten()
}

fn utf8_continuation_count(lead: u8) -> Option<usize> {
    match lead {
        0xc2..=0xdf => Some(1),
        0xe0..=0xef => Some(2),
        0xf0..=0xf4 => Some(3),
        _ => None,
    }
}

fn parse_cat_v_meta_byte(bytes: &[u8], start: usize) -> Option<(u8, usize)> {
    if start + 2 > bytes.len() || &bytes[start..start + 2] != b"M-" {
        return None;
    }
    let pos = start + 2;
    match bytes.get(pos).copied()? {
        b'^' => {
            let c = bytes.get(pos + 1).copied()?;
            let low = if c == b'?' {
                0x7f
            } else if (b'@'..=b'_').contains(&c) {
                c - b'@'
            } else {
                return None;
            };
            Some((low | 0x80, pos + 2))
        }
        c if (0x20..=0x7e).contains(&c) => Some((c | 0x80, pos + 1)),
        _ => None,
    }
}

async fn drain_host_pipe<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    stream: crate::agentic::ToolOutputStream,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;

    let mut output = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(output);
        }
        if let Some(live) = live.as_ref() {
            live.write(stream, &chunk[..read]);
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

/// INTERIM (#297) host shell selection: `bash -c` with an `sh -c` fallback
/// when bash is absent — the same sh-compatible free-form mode the confined
/// shell ran, so [`venv_cmd_prefix`]'s `export …;` prefix works unchanged.
///
/// Hardened (#297) so a hung child can never wedge the turn:
/// - `kill_on_drop(true)` + `process_group(0)` (own process group) so the whole
///   child *tree* dies when we drop the handle, not just the immediate `bash`.
/// - stdin redirected from `/dev/null` so a child that reads stdin sees EOF
///   instead of blocking forever waiting on a tty the agent can't feed.
/// - a [`host_exec_timeout`] wall-clock ceiling: on expiry the child is killed
///   and we return `timed_out: true` with exit code 124, matching the confined
///   path's envelope shape.
#[cfg(not(windows))]
pub(super) async fn host_shell_output(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> std::io::Result<HostShellRun> {
    host_shell_output_with_timeout(cmd, cwd, live, host_exec_timeout()).await
}

/// newt's control-plane env vars that must NEVER flow into a host-shell child
/// (#8 / invariant 9). Two classes, both newt-internal:
///
/// - **authority switches** — an inherited `NEWT_DISABLE_OCAP` /
///   `NEWT_FULL_ACCESS` / `NEWT_UNSAFE_HOST_EXEC` / … would silently re-assert
///   authority the session did not grant: a `newt` spawned by a Yolo child would
///   re-derive Yolo from the env twin instead of a fresh operator decision (the
///   authority-switch-survives-one-hop hole);
/// - **newt's own secrets** — `NEWT_AGENT_KEY` (the capability envelope),
///   `NEWT_OPERATOR_KEY`, and `NEWT_TOKEN_PASSPHRASE` (the encrypted-token-store
///   unlock) would let the child forge capabilities or decrypt the token store.
///
/// Knowledge in data (three Cs): a new `NEWT_` control switch is added here. The
/// child KEEPS the operator's *general* environment — the `--full-access` /
/// `--disable-ocap` lane is the operator's explicit "run with my ambient
/// authority" opt-out, so provider credentials etc. are their deliberate grant;
/// only newt's OWN control plane is excised.
pub(super) const CHILD_STRIPPED_AUTHORITY_ENV: &[&str] = &[
    "NEWT_DISABLE_OCAP",
    "NEWT_FULL_ACCESS",
    "NEWT_UNSAFE_HOST_EXEC",
    "NEWT_BENCH_OCAP",
    "NEWT_SHELL_ENGINE",
    "NEWT_SHELL_ENV_PASSTHROUGH",
    "NEWT_WRITE_PATHS",
    "NEWT_READ_PATHS",
    "NEWT_EXEC_PATHS",
    "NEWT_VENV",
    "NEWT_NO_ROUTE",
    "NEWT_AGENT_KEY",
    "NEWT_OPERATOR_KEY",
    "NEWT_TOKEN_PASSPHRASE",
];

/// Excise newt's whole control plane ([`CHILD_STRIPPED_AUTHORITY_ENV`]) from a
/// host-shell child. `env_remove` marks each key removed in the child's env plan
/// whether or not it is currently set, so no authority switch or newt secret can
/// reach the child regardless of the ambient environment.
fn strip_child_authority_env(c: &mut tokio::process::Command) {
    for key in CHILD_STRIPPED_AUTHORITY_ENV {
        c.env_remove(key);
    }
}

/// Build the host-shell child command, stripping newt's whole control plane
/// (authority switches + newt's own secrets, [`strip_child_authority_env`]) so
/// none can flow into the child and re-assert authority the session did not
/// grant it or leak newt's credentials (#8 / step-7.1a, invariant 9). The child
/// inherits the rest of the environment (the Yolo lane's explicit ambient-
/// authority grant). Own process group (setsid-equivalent) + `kill_on_drop` so a
/// hung or tty-stealing child is reaped as a whole tree.
#[cfg(not(windows))]
pub(super) fn host_shell_command(program: &str, cmd: &str, cwd: &str) -> tokio::process::Command {
    use std::process::Stdio;
    let mut c = tokio::process::Command::new(program);
    c.arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        // A child that reads stdin gets EOF, never a blocking wait on a tty
        // the agent cannot drive.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    strip_child_authority_env(&mut c);
    c
}

#[cfg(not(windows))]
pub(super) async fn host_shell_output_with_timeout(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    timeout: std::time::Duration,
) -> std::io::Result<HostShellRun> {
    async fn run_one(
        mut child: tokio::process::Child,
        live: Option<std::sync::Arc<LiveOutputRelay>>,
        timeout: std::time::Duration,
    ) -> std::io::Result<HostShellRun> {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("host shell stdout was not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("host shell stderr was not piped"))?;
        let completed = async {
            let (status, stdout, stderr) = tokio::try_join!(
                child.wait(),
                drain_host_pipe(
                    stdout,
                    live.clone(),
                    crate::agentic::ToolOutputStream::Stdout
                ),
                drain_host_pipe(stderr, live, crate::agentic::ToolOutputStream::Stderr),
            )?;
            Ok::<_, std::io::Error>((status, stdout, stderr))
        };
        match tokio::time::timeout(timeout, completed).await {
            Ok(Ok((status, stdout, stderr))) => Ok(HostShellRun {
                exit_code: status.code().unwrap_or(-1) as i64,
                stdout,
                stderr,
                timed_out: false,
            }),
            Ok(Err(e)) => Err(e),
            Err(_elapsed) => Ok(HostShellRun {
                exit_code: 124,
                stdout: Vec::new(),
                stderr: format!(
                    "command exceeded {}s host-shell timeout and was killed\n",
                    timeout.as_secs()
                )
                .into_bytes(),
                timed_out: true,
            }),
        }
    }

    match host_shell_command("bash", cmd, cwd).spawn() {
        Ok(child) => run_one(child, live, timeout).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            run_one(host_shell_command("sh", cmd, cwd).spawn()?, live, timeout).await
        }
        Err(e) => Err(e),
    }
}

/// INTERIM (#297) host shell selection on Windows: `cmd /C`, the same shape
/// as [`build_check_shell`]. Bounded by [`host_exec_timeout`] with
/// `kill_on_drop` so a hung child cannot wedge the turn.
#[cfg(windows)]
pub(super) async fn host_shell_output(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> std::io::Result<HostShellRun> {
    host_shell_output_with_timeout(cmd, cwd, live, host_exec_timeout()).await
}

#[cfg(windows)]
pub(super) async fn host_shell_output_with_timeout(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    timeout: std::time::Duration,
) -> std::io::Result<HostShellRun> {
    use std::process::Stdio;

    // step-7.1a / #8 / invariant 9: newt's whole control plane (authority
    // switches + newt's own secrets) must not flow into the host-shell child.
    let mut cmd_builder = tokio::process::Command::new("cmd");
    cmd_builder
        .args(["/C", cmd])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    strip_child_authority_env(&mut cmd_builder);
    let mut child = cmd_builder.spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("host shell stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("host shell stderr was not piped"))?;
    let completed = async {
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            drain_host_pipe(
                stdout,
                live.clone(),
                crate::agentic::ToolOutputStream::Stdout
            ),
            drain_host_pipe(stderr, live, crate::agentic::ToolOutputStream::Stderr),
        )?;
        Ok::<_, std::io::Error>((status, stdout, stderr))
    };
    match tokio::time::timeout(timeout, completed).await {
        Ok(Ok((status, stdout, stderr))) => Ok(HostShellRun {
            exit_code: status.code().unwrap_or(-1) as i64,
            stdout,
            stderr,
            timed_out: false,
        }),
        Ok(Err(e)) => Err(e),
        Err(_elapsed) => Ok(HostShellRun {
            exit_code: 124,
            stdout: Vec::new(),
            stderr: format!(
                "command exceeded {}s host-shell timeout and was killed\r\n",
                timeout.as_secs()
            )
            .into_bytes(),
            timed_out: true,
        }),
    }
}

/// Whether a confined-shell envelope carries the STRUCTURED `denied: true`
/// flag — the leash's machine-readable signal that the brush interceptor
/// refused an exec / open inside the free-form command. Reads the structured
/// field agent-bridle emits; it does NOT parse stdout/stderr (the old stderr
/// string-match was fragile — a command that merely *printed* a denial-like
/// phrase could be misread, and any wording drift would silently break
/// detection).
pub(super) fn envelope_denied(envelope: &serde_json::Value) -> bool {
    envelope
        .get("denied")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Build a human-readable denial message from the envelope's structured
/// `denials: [{ kind, target, reason }]` list, joining each entry's `reason`.
/// Falls back to a generic message when the list is missing or empty.
pub(super) fn envelope_denial_reason(envelope: &serde_json::Value) -> String {
    let reasons: Vec<String> = envelope
        .get("denials")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("reason").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if reasons.is_empty() {
        "denied: the capability leash refused an operation".to_string()
    } else {
        reasons.join("; ")
    }
}

/// The allowlist NAME for an exec target — the trailing path component (the
/// program's basename), so `/usr/bin/env` and `C:\tools\env.exe` resolve to the
/// command a grant would actually allowlist. Used when lifting a denied exec
/// into a #263 [`PermissionRequest`].
pub(super) fn exec_allowlist_name(target: &str) -> &str {
    target
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(target)
}

/// The standard `run_command` capability-denial result, composed EXACTLY ONCE:
/// a single `capability denied: <bare reason>. <recovery hint>` for the model.
///
/// #775 (§2.5): the model-facing message is ONE clean level. Two earlier defects
/// are removed:
///
/// 1. The bare denial `reason` (a full sentence from the leash, e.g.
///    `exec of "export" is not within the granted authority`) is NO LONGER
///    stuffed into the old `print_denied` bare `'{target}'` slot. Doing so produced
///    the garbled `capability denied: exec does not permit '<whole reason
///    sentence> - add it via …>'` — a denial sentence nested inside another.
///    That notice path received only the BARE command target, matching its
///    `{axis} does not permit '{target}'` contract (the same shape the fs path
///    uses via [`super::denied_fs_result`]).
/// 2. The stale `extra_exec` config hint is gone from the model-facing message.
///    #721 superseded "edit your `[tui.permissions]` config" with the
///    model-actionable [`denial_recovery_hint`] (`request_permissions`), so the
///    model now sees the bare reason once plus that hint — never a config edit
///    it cannot perform mid-turn.
///
/// The #263 prompt path still falls back here on deny (and on a second denial
/// after a re-execution).
pub(super) fn denied_run_command_result(envelope: &serde_json::Value, _color: bool) -> String {
    // Model-facing message: composed exactly once — and it names the exact
    // recovery call (#1160), since the envelope carries axis + target.
    format!(
        "capability denied: {}. {}",
        envelope_denial_reason(envelope),
        denial_recovery_hint(
            denial_axis_label(envelope),
            &exec_denial_target_label(envelope)
        )
    )
}

/// The bare target for the human exec-denial NOTICE: the denied command name(s)
/// the leash refused, NEVER the reason sentence. Joins multiple targets with
/// `, `; falls back to a generic label so the notice always prints one clean
/// `{axis} does not permit '{target}'` line. (#775 — restores
/// the former denial notice's bare-`'{target}'` contract.)
pub(super) fn exec_denial_target_label(envelope: &serde_json::Value) -> String {
    let targets: Vec<&str> = envelope
        .get("denials")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("target").and_then(serde_json::Value::as_str))
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if targets.is_empty() {
        "a command".to_string()
    } else {
        targets.join(", ")
    }
}

/// The standard `run_command` success path: return stdout/stderr, or `(exit N)`
/// when the command produced no output. Factored verbatim so
/// the #263 re-execution path shares one formatter with the first dispatch.
pub(super) fn shell_envelope_output(
    envelope: &serde_json::Value,
    _tool_output_lines: usize,
    _color: bool,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    presentation: Option<&mut dyn ToolPresentation>,
) -> String {
    let stdout = envelope
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let stderr = envelope
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let out = format!("{stdout}{stderr}");
    // #1969: the exit code decides success, not the shape of the output.
    //
    // This used to be consulted ONLY when the output was empty, so every
    // command that failed LOUDLY — which is every failing compile — returned
    // a non-empty string with no failure marker and was classified by
    // `tool_result_ok`'s prefix test as a success. Three consumers read that
    // one bit: the turn's `ToolEvent` ledger, `RepeatCallGuard` (which
    // memoizes a `Failure` only for `!ok`, so the per-run steer never fired
    // on a repeated failing build), and `loop_watch::repeated_failure`.
    //
    // The marker is a prefix rather than a suffix because that is what
    // `tool_result_ok` reads, and it names the code because "it failed" with
    // no evidence is the claim this repo keeps refusing to accept elsewhere.
    // The diagnostics follow it untouched — the model still needs them.
    let code = envelope
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    let mark_failure = |payload: String| {
        if code == 0 {
            payload
        } else {
            format!("error: command exited {code}\n{payload}")
        }
    };
    if out.trim().is_empty() {
        // A failing command that printed nothing needs no second rendering of
        // its own code: the marker already carries it.
        return if code == 0 {
            format!("(exit {code})")
        } else {
            format!("error: command exited {code}")
        };
    }
    {
        // The terminal follows the full output tail even when the model-facing
        // payload below is token-capped or replaced by a spill handle.
        if let Some(presentation) = presentation {
            presentation.override_result(out.clone());
        }
        // #726/#945: the MODEL-facing payload is capped by the shared TOKEN
        // budget using head+tail. When tool_offload is on, spill the FULL
        // redacted output before capping so the true tail and elided middle stay
        // recoverable via memory_fetch("spill:<id>") and grep.
        //
        // The spill decision sizes with the SAME conservative estimator the cap
        // uses (via `should_spill_full_output` → `cap_estimator`, not the 4 c/t
        // context default) — otherwise output between the conservative cap
        // (3 c/t) and the looser default (4 c/t) gets head/tail-truncated by the
        // cap yet judged "under budget" by the spill gate, so its elided middle
        // is never spilled and becomes unrecoverable. One shared owner keeps
        // "will the cap truncate?" and "should we spill?" from ever diverging.
        let max_tokens = max_output_tokens();
        let est = output_budget::cap_estimator();
        let should_spill = output_budget::should_spill_full_output(
            out.len(),
            out.chars().count(),
            max_tokens,
            tool_offload,
        );
        let capped = if should_spill {
            match spill_store {
                Some(store) => {
                    let (id, redacted) = content_spill::store_redacted_full(
                        &out,
                        Some("run_command".to_string()),
                        store,
                    );
                    let teaser_tokens = est
                        .tokens_for_chars(content_spill::TOOL_RESULT_SPILL_CAP.saturating_sub(512));
                    match id {
                        // Committed: cap with the `spill:<id>` retrieval handle.
                        Some(id) => cap_model_output_with_handle(
                            &redacted,
                            max_tokens.min(teaser_tokens),
                            output_head_tokens(),
                            Some(&id),
                        ),
                        // Commit failed: fail closed — cap the redacted output with
                        // NO handle rather than promise a `spill:<id>` that resolves
                        // to nothing (BHV-SPILL-001).
                        None => cap_model_output(&redacted, max_tokens),
                    }
                }
                None => cap_model_output(&out, max_tokens),
            }
        } else {
            cap_model_output(&out, max_tokens)
        };
        // #898: if this command's output carries a forge "open a pull/merge
        // request" URL (git prints it on push of a new branch), append an
        // explicit next-step hint so the model opens the PR instead of stalling.
        // Detected from the UNcapped output so a long push log can't truncate the
        // URL away, and appended AFTER the cap so the hint always survives.
        mark_failure(match pr_creation_url(&out) {
            Some(url) => format!("{capped}{}", pr_next_step_hint(url)),
            None => capped,
        })
    }
}

/// #898: the forge "open a pull/merge request" URL that git prints on `push` of
/// a new branch — GitHub `…/pull/new/<branch>` (or a `…/compare/…` link) and
/// GitLab `…/merge_requests/new…`. Returned so [`shell_envelope_output`] can
/// append a next-step hint: models routinely push and then stall instead of
/// opening the PR (issue #898). Scans whitespace-split tokens because git emits
/// the URL on its own `remote:`-prefixed line.
pub(super) fn pr_creation_url(output: &str) -> Option<&str> {
    output.split_whitespace().find(|tok| {
        tok.starts_with("https://")
            && (tok.contains("/pull/new/")
                || tok.contains("/merge_requests/new")
                || tok.contains("/compare/"))
    })
}

/// The next-step hint appended after a push whose output carries a PR-creation
/// URL (#898). Names the concrete `gh` command AND the tool boundary — the
/// embedded `git` tool cannot push or open PRs — so the model proceeds through
/// run_command + `gh` instead of looping back to the pushless git tool.
fn pr_next_step_hint(url: &str) -> String {
    format!(
        "\n\n[newt] A branch was pushed. To open a pull request now, call \
         run_command with `gh pr create --fill` (the `gh` CLI is available; the \
         `git` tool cannot push or open PRs). Or open this URL: {url}"
    )
}

/// Lift a confined-shell denial envelope into promptable #263 requests.
///
/// Returns `Some` only when EVERY structured denial entry is an `exec` kind
/// with a non-empty target — the case the human can meaningfully grant (the
/// allowlist name, same basename rule as the config hint). Any other kind
/// (e.g. an `open` refused inside the shell) keeps the standard denial:
/// guessing which fs axis an opaque `open` maps to would over-grant.
/// #1150: a STRUCTURAL refusal is a can't, not a may-not — the confined shell
/// engine cannot interpret the construct (`$(...)`, backgrounding `&`, heredocs,
/// fd duplication), so NO grant unlocks it. Offering "allow once / session /
/// always" for one is a grant→denial contradiction that destroys trust in the
/// whole permission loop (the operator grants, the engine denies anyway). We
/// detect it by the stable markers agent-bridle's `Refusal::Display` emits, so
/// these fall through to the plain denial (which already names the --yolo
/// escape) with no grant menu.
fn is_structural_refusal(reason: &str) -> bool {
    reason.contains("refused by design:")
        || reason.contains("dynamic construct the confined shell")
        || reason.contains("not yet supported by the confined shell")
}

pub(super) fn exec_denial_requests(envelope: &serde_json::Value) -> Option<Vec<PermissionRequest>> {
    let denials = envelope.get("denials")?.as_array()?;
    if denials.is_empty() {
        return None;
    }
    let mut requests = Vec::with_capacity(denials.len());
    for d in denials {
        if d.get("kind")?.as_str()? != "exec" {
            return None;
        }
        // A structural refusal anywhere in the batch: grants are meaningless,
        // so surface the plain denial for the whole call (#1150).
        if d.get("reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_structural_refusal)
        {
            return None;
        }
        let target = d.get("target")?.as_str().filter(|t| !t.is_empty())?;
        requests.push(PermissionRequest {
            tool: "run_command".to_string(),
            kind: DenialKind::Exec,
            target: exec_allowlist_name(target).to_string(),
            reason: d
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Some(requests)
}

/// #905: lift a confined-shell NET denial envelope into promptable #263 requests
/// — the `net`-axis sibling of [`exec_denial_requests`]. agent-bridle #196
/// surfaces a refused CONNECT host as `Denial { kind: "net", target: <host> }`
/// (with `denied: true`), so when the operator's `net` allow-list refuses a host
/// the shell reached (e.g. `git push` to `github.com`), this turns each into a
/// `PermissionRequest { kind: Net, target: host }` the gate can prompt per-host.
///
/// Returns `Some` only when EVERY denial is a `net` kind with a non-empty host
/// target — the case a grant is meaningful (add the host to the net allow-list).
/// A mixed or non-net batch returns `None` and keeps the standard denial.
pub(super) fn net_denial_requests(envelope: &serde_json::Value) -> Option<Vec<PermissionRequest>> {
    let denials = envelope.get("denials")?.as_array()?;
    if denials.is_empty() {
        return None;
    }
    let mut requests = Vec::with_capacity(denials.len());
    for d in denials {
        if d.get("kind")?.as_str()? != "net" {
            return None;
        }
        let host = d.get("target")?.as_str().filter(|t| !t.is_empty())?;
        requests.push(PermissionRequest {
            tool: "run_command".to_string(),
            kind: DenialKind::Net,
            target: host.to_string(),
            reason: d
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Some(requests)
}

/// #905: the axis label for the human denial NOTICE — `net` when EVERY denial is
/// a net (host) refusal, else `exec` (exec / mixed / empty default). Keeps a net
/// denial from being mislabeled `exec does not permit '<host>'`.
pub(super) fn denial_axis_label(envelope: &serde_json::Value) -> &'static str {
    let all_net = envelope
        .get("denials")
        .and_then(serde_json::Value::as_array)
        .filter(|arr| !arr.is_empty())
        .is_some_and(|arr| {
            arr.iter()
                .all(|d| d.get("kind").and_then(serde_json::Value::as_str) == Some("net"))
        });
    if all_net {
        "net"
    } else {
        "exec"
    }
}
