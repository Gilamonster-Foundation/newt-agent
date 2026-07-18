//! Built-in tool definitions and the tool executor for the agentic loop.
//! Moved verbatim from `newt-tui` in Step 9.7 — the Caveats enforcement,
//! shrink guard, build-check feedback, and agent-bridle routing are unchanged.

use super::artifact_read::{execute_artifact_read_silent, ArtifactReadContext};
use super::crew_tool::CrewRunner;
use super::display::{ToolDisplay, ToolPresentation};
use super::git_tool::GitTool;
use super::mcp::McpTools;
use super::memory_fetch::{execute_memory_fetch, memory_fetch_tool_definition, MemorySource};
use super::note_sink::{execute_save_note, save_note_tool_definition, NoteSink};
use super::permissions::{DenialKind, PermissionDecision, PermissionGate, PermissionRequest};
use super::prompt_intake::PromptDisposition;
use super::prompt_read::{execute_prompt_read_silent, PromptReadContext};
use super::recall::{execute_recall, recall_tool_definition, RecallSource};
use super::report::{execute_render_report, render_report_tool_definition};
use super::spill::{self, SpillStore};
use crate::caveats::CaveatsExt as _;
#[cfg(test)]
use output_budget::DEFAULT_MAX_OUTPUT_TOKENS;
use output_budget::{
    cap_model_output, cap_model_output_with_handle, max_output_tokens, output_head_tokens,
    paginate_read,
};
pub use output_budget::{set_max_output_tokens, set_output_head_tokens};

mod catalog;
mod output_budget;

#[cfg(test)]
use catalog::lifecycle_tool_definition;
pub(crate) use catalog::{
    classify_gated_off_reach, classify_phantom_reach, is_context_remaining_call, is_hallucination,
    known_builtin_tool_name, merged_tool_definitions, resolve_tool_alias, AliasOutcome,
};
use catalog::{
    disposition_tool_denied_message, persona_tool_denied_message, run_command_redirect,
    unknown_tool_message,
};
pub use catalog::{
    filter_advertised_tools, filter_tools_for_disposition, persona_tool_allowed, tool_allowed,
    tool_definitions,
};
#[cfg(test)]
use catalog::{
    levenshtein, nearest_tool_name, ALL_TOOL_NAMES, BASE_TOOL_NAMES, EXTENDED_TOOL_REGISTRY,
};
/// Build a shell prefix that exports venv/exec-path vars into the agent-bridle
/// confined shell.
///
/// Agent-bridle's confined shell does not inherit the host environment
/// (`do_not_inherit_env(true)`), so we inject `VIRTUAL_ENV` and prepend
/// venv/extra `bin/` dirs to `PATH` by prefixing every `run_command` cmd.
/// `NEWT_VENV` (set from `--venv` or auto-detected from `$VIRTUAL_ENV` by the
/// CLI) takes precedence; falls back to `$VIRTUAL_ENV` if the TUI was invoked
/// directly without going through the CLI's `dispatch`.
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
fn venv_env_map() -> std::collections::BTreeMap<String, String> {
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
fn resolve_exec_cwd(workspace: &str, cwd: Option<&str>) -> String {
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
fn split_leading_cd(cmd: &str) -> (Option<String>, String) {
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

fn confined_dispatch_args(cmd: &str, cwd: &str) -> serde_json::Value {
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
fn shell_engine() -> crate::ShellEngine {
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
fn bridle_registry(
    engine: crate::ShellEngine,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> agent_bridle::Registry {
    use std::sync::Arc;
    let shell: Arc<dyn agent_bridle::Tool> = match engine {
        crate::ShellEngine::SafeSubset => {
            let mut tool = agent_bridle::ShellTool::new();
            if let Some(observer) = live.clone() {
                tool = tool.with_output_observer(observer);
            }
            Arc::new(tool)
        }
        crate::ShellEngine::Host => {
            let mut tool = agent_bridle::HostShellTool::new();
            if let Some(observer) = live.clone() {
                tool = tool.with_output_observer(observer);
            }
            Arc::new(tool)
        }
        crate::ShellEngine::Brush => {
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
            let mut tool = agent_bridle::BrushShellTool::new();
            if let Some(observer) = live {
                tool = tool.with_output_observer(observer);
            }
            Arc::new(tool)
        }
    };
    agent_bridle::Registry::builder()
        .tool(shell)
        .tool(Arc::new(agent_bridle::WebFetchTool::new()))
        .build()
}

// ---------------------------------------------------------------------------
// INTERIM (#297): the --disable-ocap / --yolo exec escape hatch
// ---------------------------------------------------------------------------

/// INTERIM (#297): is the ocap exec bypass asserted for this invocation?
///
/// True only when `NEWT_DISABLE_OCAP=1` — set by the CLI's `--disable-ocap`
/// flag (alias `--yolo`) or exported directly for harness/pod use. The value
/// must be exactly `"1"`: a security bypass reads fail-closed, so anything
/// else (including `true`) leaves confinement on. Deliberately env-only —
/// there is NO config-file key, so the bypass can never silently persist; it
/// must be asserted per invocation.
///
/// Scope: `run_command` only. On stub-shell builds (the only crates.io-
/// publishable configuration) agent-bridle's `shell` tool fails closed on
/// every command, which makes agentic coding impossible without the brush
/// `CommandInterceptor` patch underneath. `web_fetch` is NOT bypassed: the
/// stub-shell branch stubs only the shell tool — `agent-bridle-tool-web`
/// ships the real leash-enforcing implementation (verified at agent-bridle
/// rev `2129c91`), so it does not fail closed. The fs tools keep the
/// newt-native workspace fence untouched: yolo is unconfined exec, fenced fs
/// — never a global authority-off switch.
///
/// Remove (or demote to a debug flag) when brush upstreams the
/// `CommandInterceptor` hook (reubeno/brush#1184) and agent-bridle's real
/// confined shell becomes the default everywhere — see agent-bridle#20 and
/// the `[patch.crates-io]` note in the workspace Cargo.toml.
pub fn ocap_disabled() -> bool {
    std::env::var("NEWT_DISABLE_OCAP").is_ok_and(|v| v == "1")
}

/// Is the per-invocation `full_access` preset override asserted?
///
/// True only when `NEWT_FULL_ACCESS=1` — set by the CLI's `--full-access`
/// flag. The session policy is then built from the `full_access` preset
/// (`Caveats::top()`) regardless of the configured `[tui.permissions]`
/// preset, exactly as if the config said `preset = "full_access"` for this
/// one run. Like [`ocap_disabled`], the value must be exactly `"1"` — a
/// widening switch reads fail-closed — and it is deliberately env-only, so
/// the override can never silently persist.
///
/// This is a DISTINCT switch from [`ocap_disabled`] (`--yolo`): the two
/// compose but never alias. `--full-access` widens the session *authority*
/// (fs fence, net leash, exec allowlist → unrestricted, which also empties
/// the #774 exec floor); `--yolo` changes the exec *mechanism* (host shell
/// instead of the confined shell) and still honors whatever floor is in
/// force. `--yolo --full-access` together yield an unrestricted host shell.
pub fn full_access_requested() -> bool {
    std::env::var("NEWT_FULL_ACCESS").is_ok_and(|v| v == "1")
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
fn shadow_records(host_bypass: bool, full_access: bool) -> bool {
    host_bypass || full_access
}

/// #1193: is the session in the read-only PLAN phase? Set by `enter_plan_mode`,
/// cleared by `exit_plan_mode`. Env-signalled like [`ocap_disabled`] /
/// [`full_access_requested`] so the TUI can read it when resolving per-turn
/// caveats (it MEETs the read-only clamp in, which only narrows authority —
/// the model voluntarily restricting itself is always safe). Value must be
/// exactly "1".
pub fn in_plan_phase() -> bool {
    std::env::var("NEWT_PLAN_PHASE").is_ok_and(|v| v == "1")
}

/// The read-only authority a plan phase clamps the session to (#1193): reads
/// everywhere, but NO writes, NO exec, NO net. MEETing this into the session
/// caveats enforces "planning is read-only" — the design's safety guarantee,
/// not the model's good intentions. `net`/max_calls left permissive here; the
/// TUI `meet` intersects with the real session net so nothing widens.
pub fn plan_phase_clamp() -> crate::caveats::Caveats {
    use crate::caveats::{CountBound, Scope};
    crate::caveats::Caveats {
        fs_read: Scope::All,
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// facade P4 (#780): is the convenience **routing** turned OFF for this call?
///
/// True only when `NEWT_NO_ROUTE=1` — set by the CLI's `--no-route` flag. It
/// disables the L2 *convenience routing* ([`super::routing`]): a model's
/// `run_command("cat X")` runs the normal exec path as-is instead of being
/// rewritten to the governed `read_file` built-in.
///
/// **This is a DISTINCT switch from [`ocap_disabled`] (§7-F5).**
/// `--no-route` / `NEWT_NO_ROUTE` turns off L2 convenience only; it NEVER
/// disables the L3 boundary — the confined shell still gates exec and the fs
/// fence still governs reads. `--disable-ocap` / `--yolo` / `NEWT_DISABLE_OCAP`
/// (L3-OFF, a full host unconfine) is a completely separate mechanism: the two
/// names never alias, and turning routing off can never imply unconfined exec.
/// Reads fail-closed — only the exact value `1` turns routing off; deliberately
/// env-only, no config key, so it cannot silently persist.
pub fn routing_disabled() -> bool {
    std::env::var("NEWT_NO_ROUTE").is_ok_and(|v| v == "1")
}

/// #307: does the named-permission-preset exec FLOOR permit running `cmd` on
/// the UNCONFINED host shell?
///
/// `None` ⇒ no preset is active; the floor imposes nothing, so the answer is
/// `true` (the `--disable-ocap` bypass behaves exactly as it did pre-#307).
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
/// path, which enforces the (already preset-clamped) `caveats`. So a restricted
/// triage/on-call mode keeps its ceiling even under `--yolo`.
fn exec_floor_permits(floor: Option<&crate::caveats::Scope<String>>, cmd: &str) -> bool {
    use crate::caveats::ScopeExt as _;
    let Some(scope) = floor else {
        return true; // no preset ⇒ bypass unchanged (bit-for-bit)
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
const LIVE_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
const LIVE_OUTPUT_QUEUE_CHUNKS: usize = 32;
const LIVE_OUTPUT_OBSERVER_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
const LIVE_OUTPUT_FINISH_WAIT: std::time::Duration = std::time::Duration::from_millis(500);
const LIVE_OUTPUT_OPEN: u8 = 0;
const LIVE_OUTPUT_FINISHING: u8 = 1;
const LIVE_OUTPUT_CANCELLED: u8 = 2;
const LIVE_OUTPUT_CLOSED: u8 = 3;

enum LiveOutputDispatch {
    Write(crate::agentic::ToolOutputStream, Vec<u8>),
    Wake,
}

struct LiveOutputCompletion {
    finished: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
}

struct LiveOutputRelay {
    sender: std::sync::mpsc::SyncSender<LiveOutputDispatch>,
    phase: std::sync::Arc<std::sync::atomic::AtomicU8>,
    completion: std::sync::Arc<LiveOutputCompletion>,
}

impl LiveOutputRelay {
    fn write(&self, stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::TrySendError;

        for part in chunk.chunks(LIVE_OUTPUT_CHUNK_BYTES) {
            if self.phase.load(Ordering::Acquire) != LIVE_OUTPUT_OPEN {
                break;
            }
            match self
                .sender
                .try_send(LiveOutputDispatch::Write(stream, part.to_vec()))
            {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => break,
                Err(TrySendError::Disconnected(_)) => {
                    self.phase.store(LIVE_OUTPUT_CLOSED, Ordering::Release);
                    break;
                }
            }
        }
    }

    fn request_finish(&self) {
        use std::sync::atomic::Ordering;
        if self
            .phase
            .compare_exchange(
                LIVE_OUTPUT_OPEN,
                LIVE_OUTPUT_FINISHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let _ = self.sender.try_send(LiveOutputDispatch::Wake);
        }
    }

    fn cancel(&self) {
        use std::sync::atomic::Ordering;
        let changed = self
            .phase
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |phase| {
                (phase != LIVE_OUTPUT_CLOSED).then_some(LIVE_OUTPUT_CANCELLED)
            })
            .is_ok();
        if changed {
            let _ = self.sender.try_send(LiveOutputDispatch::Wake);
        }
    }

    fn wait_finished(&self, timeout: std::time::Duration) -> bool {
        let finished = self
            .completion
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (finished, _) = self
            .completion
            .wake
            .wait_timeout_while(finished, timeout, |finished| !*finished)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *finished
    }
}

impl agent_bridle::ShellOutputObserver for LiveOutputRelay {
    fn on_output(
        &self,
        _invocation: agent_bridle::ShellInvocationId,
        stream: agent_bridle::ShellOutputStream,
        chunk: &[u8],
    ) {
        let stream = match stream {
            agent_bridle::ShellOutputStream::Stdout => crate::agentic::ToolOutputStream::Stdout,
            agent_bridle::ShellOutputStream::Stderr => crate::agentic::ToolOutputStream::Stderr,
        };
        self.write(stream, chunk);
    }

    fn on_finish(&self, _invocation: agent_bridle::ShellInvocationId) {
        self.request_finish();
    }
}

struct LiveOutputSession {
    relay: std::sync::Arc<LiveOutputRelay>,
    sink: std::sync::Arc<dyn crate::agentic::LiveToolOutput>,
    generation: u64,
    closed: bool,
}

impl LiveOutputSession {
    fn start(sink: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>) -> Option<Self> {
        static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let sink = sink?;
        let generation = NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<LiveOutputDispatch>(LIVE_OUTPUT_QUEUE_CHUNKS);
        let phase = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(LIVE_OUTPUT_OPEN));
        let completion = std::sync::Arc::new(LiveOutputCompletion {
            finished: std::sync::Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let worker_phase = phase.clone();
        let worker_completion = completion.clone();
        let worker_sink = sink.clone();
        if std::thread::Builder::new()
            .name(format!("newt-live-output-{generation}"))
            .spawn(move || {
                run_live_output_dispatch(
                    receiver,
                    worker_sink,
                    generation,
                    &worker_phase,
                    &worker_completion,
                );
            })
            .is_err()
        {
            return None;
        }
        Some(Self {
            relay: std::sync::Arc::new(LiveOutputRelay {
                sender,
                phase,
                completion,
            }),
            sink,
            generation,
            closed: false,
        })
    }

    fn relay(&self) -> std::sync::Arc<LiveOutputRelay> {
        self.relay.clone()
    }

    fn finish(&mut self) {
        if self.closed {
            return;
        }
        self.relay.request_finish();
        if !self.relay.wait_finished(LIVE_OUTPUT_FINISH_WAIT) {
            self.relay.cancel();
            self.abandon_generation();
        }
        self.closed = true;
    }

    fn finish_after_observer(&mut self) {
        if self.closed {
            return;
        }
        if !self.relay.wait_finished(LIVE_OUTPUT_OBSERVER_WAIT) {
            self.relay.request_finish();
            if !self.relay.wait_finished(LIVE_OUTPUT_FINISH_WAIT) {
                self.relay.cancel();
                self.abandon_generation();
            }
        }
        self.closed = true;
    }

    #[cfg(all(test, not(windows)))]
    fn cancel(&mut self) {
        if !self.closed {
            self.relay.cancel();
            self.abandon_generation();
            self.closed = true;
        }
    }

    fn abandon_generation(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.sink.abandon(self.generation);
        }));
    }
}

impl Drop for LiveOutputSession {
    fn drop(&mut self) {
        self.finish();
    }
}

fn run_live_output_dispatch(
    receiver: std::sync::mpsc::Receiver<LiveOutputDispatch>,
    sink: std::sync::Arc<dyn crate::agentic::LiveToolOutput>,
    generation: u64,
    phase: &std::sync::atomic::AtomicU8,
    completion: &LiveOutputCompletion,
) {
    use std::sync::atomic::Ordering;

    let abandon = || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink.abandon(generation);
        }));
    };

    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.start(generation))).is_err() {
        phase.store(LIVE_OUTPUT_CANCELLED, Ordering::Release);
        abandon();
        phase.store(LIVE_OUTPUT_CLOSED, Ordering::Release);
        mark_live_output_complete(completion);
        return;
    }

    if phase.load(Ordering::Acquire) == LIVE_OUTPUT_CANCELLED {
        abandon();
        phase.store(LIVE_OUTPUT_CLOSED, Ordering::Release);
        mark_live_output_complete(completion);
        return;
    }

    let deliver = |stream, chunk: Vec<u8>| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink.write(generation, stream, &chunk);
        }))
        .is_ok()
    };

    loop {
        match receiver.recv() {
            Ok(LiveOutputDispatch::Write(stream, chunk))
                if phase.load(Ordering::Acquire) != LIVE_OUTPUT_CANCELLED =>
            {
                if !deliver(stream, chunk) {
                    phase.store(LIVE_OUTPUT_CANCELLED, Ordering::Release);
                }
            }
            Ok(LiveOutputDispatch::Write(_, _)) | Ok(LiveOutputDispatch::Wake) => {}
            Err(_) => {
                phase.store(LIVE_OUTPUT_CANCELLED, Ordering::Release);
            }
        }

        match phase.load(Ordering::Acquire) {
            LIVE_OUTPUT_OPEN => continue,
            LIVE_OUTPUT_FINISHING => {
                while phase.load(Ordering::Acquire) == LIVE_OUTPUT_FINISHING {
                    let Ok(dispatch) = receiver.try_recv() else {
                        break;
                    };
                    if let LiveOutputDispatch::Write(stream, chunk) = dispatch {
                        if !deliver(stream, chunk) {
                            phase.store(LIVE_OUTPUT_CANCELLED, Ordering::Release);
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
        break;
    }

    if phase.load(Ordering::Acquire) == LIVE_OUTPUT_FINISHING {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.finish(generation)))
            .is_err()
        {
            abandon();
        }
    } else {
        abandon();
    }
    phase.store(LIVE_OUTPUT_CLOSED, Ordering::Release);
    mark_live_output_complete(completion);
}

fn mark_live_output_complete(completion: &LiveOutputCompletion) {
    let mut finished = completion
        .finished
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *finished = true;
    completion.wake.notify_all();
}

async fn dispatch_bridled_shell(
    args: serde_json::Value,
    caveats: &crate::caveats::Caveats,
    sink: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
) -> agent_bridle::ToolResult<serde_json::Value> {
    let mut live = LiveOutputSession::start(sink);
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
async fn exec_confined_command(
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

async fn host_shell_dispatch(
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
struct HostShellRun {
    exit_code: i64,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
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

fn decode_shell_stream(bytes: &[u8]) -> String {
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
async fn host_shell_output(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> std::io::Result<HostShellRun> {
    host_shell_output_with_timeout(cmd, cwd, live, host_exec_timeout()).await
}

#[cfg(not(windows))]
async fn host_shell_output_with_timeout(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    timeout: std::time::Duration,
) -> std::io::Result<HostShellRun> {
    use std::process::Stdio;

    fn shell(program: &str, cmd: &str, cwd: &str) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(program);
        c.arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            // A child that reads stdin gets EOF, never a blocking wait on a tty
            // the agent cannot drive.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group (setsid-equivalent, std-only since Rust 1.64):
            // detaches the child from our controlling tty so it can't steal the
            // interrupt byte, and gives the drop-kill a whole group to reap.
            .process_group(0)
            // Kill the child *tree* — not just `bash` — if we drop the handle
            // (timeout, cancel, panic).
            .kill_on_drop(true);
        c
    }

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

    match shell("bash", cmd, cwd).spawn() {
        Ok(child) => run_one(child, live, timeout).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            run_one(shell("sh", cmd, cwd).spawn()?, live, timeout).await
        }
        Err(e) => Err(e),
    }
}

/// INTERIM (#297) host shell selection on Windows: `cmd /C`, the same shape
/// as [`build_check_shell`]. Bounded by [`host_exec_timeout`] with
/// `kill_on_drop` so a hung child cannot wedge the turn.
#[cfg(windows)]
async fn host_shell_output(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
) -> std::io::Result<HostShellRun> {
    host_shell_output_with_timeout(cmd, cwd, live, host_exec_timeout()).await
}

#[cfg(windows)]
async fn host_shell_output_with_timeout(
    cmd: &str,
    cwd: &str,
    live: Option<std::sync::Arc<LiveOutputRelay>>,
    timeout: std::time::Duration,
) -> std::io::Result<HostShellRun> {
    use std::process::Stdio;

    let mut child = tokio::process::Command::new("cmd")
        .args(["/C", cmd])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

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

/// Lexically normalise a path *string* — collapse `.` and `..` components
/// without touching the filesystem — so containment is decided on the location
/// the caller actually named, not on a raw byte prefix. Does NOT resolve
/// symlinks (that needs `canonicalize`, which requires the path to exist and is
/// the still-open `fs-canonical-containment` deviation): a symlink *inside* the
/// workspace can still point out. What this DOES close are the string-only
/// escapes — `..` traversal and sibling-prefix collisions.
fn lexically_normalize(path: &str) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    for comp in std::path::Path::new(path).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a real segment; never climb above a root/prefix.
                if !out.pop() {
                    out.push(comp.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Returns true if `full_path` is permitted by `scope`.
///
/// The `Caveats` lattice stores workspace-root strings (not individual file
/// paths) with exact-set semantics; this layer adds containment so that "the
/// workspace root is permitted" means "any path *under* it is permitted". Both
/// the candidate and each root are lexically normalised (collapsing `..`) and
/// then compared by whole path components via [`std::path::Path::starts_with`],
/// so `..` traversal (`/ws/../etc/passwd`) and sibling-prefix collisions
/// (`/ws-secret` vs root `/ws`) no longer escape the fence — unlike the raw
/// string prefix match this replaced. Symlink containment is still open
/// (`fs-canonical-containment`); creating one needs exec, which is gated separately.
pub(crate) fn tui_permits_path(scope: &crate::caveats::Scope<String>, full_path: &str) -> bool {
    match scope {
        crate::caveats::Scope::All => true,
        crate::caveats::Scope::Only(set) if set.is_empty() => false,
        crate::caveats::Scope::Only(set) => {
            let candidate = lexically_normalize(full_path);
            set.iter()
                .any(|root| candidate.starts_with(lexically_normalize(root)))
        }
    }
}

/// Full-access/custom unrestricted writes keep the final y/N guard in ordinary
/// interactive mode. Under --yolo the operator already chose an explicit
/// auto-run mode, so do not let EOF on stdin become a fake human denial.
fn confirm_unrestricted_fs_mutation(
    caveats: &crate::caveats::Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
    question: &str,
) -> bool {
    if !matches!(caveats.fs_write, crate::caveats::Scope::All) {
        return true;
    }
    if ocap_disabled() {
        return true;
    }
    match gate {
        Some(g) => g
            .ask_question(question)
            .is_some_and(|answer| answer.trim().eq_ignore_ascii_case("y")),
        None => false,
    }
}

/// Run the configured build-check command in `workspace` and return a compact
/// result string appended to the tool output so the model sees it immediately.
pub(crate) fn run_build_check(cmd: &str, workspace: &str) -> String {
    let result = build_check_shell(cmd).current_dir(workspace).output();
    match result {
        Ok(out) if out.status.success() => "  ✓ build check passed".to_string(),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let combined = format!("{stdout}{stderr}");
            let excerpt: String = combined.lines().take(8).collect::<Vec<_>>().join("\n");
            format!("  ✗ build check failed:\n{excerpt}")
        }
        Err(e) => format!("  ⚠ build check could not run: {e}"),
    }
}

#[cfg(windows)]
fn build_check_shell(cmd: &str) -> std::process::Command {
    let mut shell = std::process::Command::new("cmd");
    shell.args(["/C", cmd]);
    shell
}

#[cfg(not(windows))]
fn build_check_shell(cmd: &str) -> std::process::Command {
    let mut shell = std::process::Command::new("sh");
    shell.args(["-c", cmd]);
    shell
}

#[cfg(all(test, windows))]
fn passing_build_check_cmd() -> &'static str {
    "exit /B 0"
}

#[cfg(all(test, not(windows)))]
fn passing_build_check_cmd() -> &'static str {
    "true"
}

#[cfg(all(test, windows))]
fn failing_build_check_cmd(message: &str) -> String {
    format!("echo {message} 1>&2 & exit /B 1")
}

#[cfg(all(test, not(windows)))]
fn failing_build_check_cmd(message: &str) -> String {
    format!("echo {message} >&2; exit 1")
}

/// Whether a confined-shell envelope carries the STRUCTURED `denied: true`
/// flag — the leash's machine-readable signal that the brush interceptor
/// refused an exec / open inside the free-form command. Reads the structured
/// field agent-bridle emits; it does NOT parse stdout/stderr (the old stderr
/// string-match was fragile — a command that merely *printed* a denial-like
/// phrase could be misread, and any wording drift would silently break
/// detection).
fn envelope_denied(envelope: &serde_json::Value) -> bool {
    envelope
        .get("denied")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Build a human-readable denial message from the envelope's structured
/// `denials: [{ kind, target, reason }]` list, joining each entry's `reason`.
/// Falls back to a generic message when the list is missing or empty.
fn envelope_denial_reason(envelope: &serde_json::Value) -> String {
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
fn exec_allowlist_name(target: &str) -> &str {
    target
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(target)
}

/// #721: the model-actionable recovery appended to every capability denial the
/// MODEL sees. A denial used to be a DEAD-END: it told the *human* to edit
/// `[tui.permissions]`, which the model cannot do mid-turn, so the loop stalled
/// (the issue's `mkdir` reproduction). This sentence tells the *model* what IT
/// can do — ask the operator to grant the capability via the
/// `request_permissions` tool, or change approach. The gate is unchanged: the
/// call is STILL denied; only the coaching is added. In a headless flow with no
/// operator, `request_permissions` answers "no operator available", which is
/// itself a recoverable signal (switch strategy) rather than a config edit.
/// #1160: the copy-pasteable recovery hint — when the denial site KNOWS the axis and
/// target, the hint names the exact recovery call instead of describing it.
/// The model shouldn't have to infer parameters the harness already holds
/// (headless there is no operator to guess-and-check against).
fn denial_recovery_hint(capability: &str, target: &str) -> String {
    format!(
        "This is outside your granted authority — to ask the operator, call \
         request_permissions(capability=\"{capability}\", target=\"{target}\", \
         reason=\"<why you need it>\"), or take a different approach that stays \
         within your current authority."
    )
}

/// #479 (G4): the model-facing recovery coach when `crew`/`compose_roster` is
/// reached while the crew/team surface is OFF — the DEFAULT, since the runner is
/// only built when the operator sets `NEWT_TEAM`. Replaces the flat
/// `unknown tool: … (no crew surface …)` dead-end (which left the model nowhere
/// to go) with a model-actionable message in the #721 [`denial_recovery_hint`]
/// style: it names the operator gesture that enables the surface (`NEWT_TEAM`)
/// AND a real solo alternative (the always-available file/exec tools), so the
/// reach is recoverable instead of a wall. The OCAP presence-gate is unchanged —
/// crew stays `NEWT_TEAM`-gated; only the coaching is added.
const CREW_OFF_RECOVERY_HINT: &str =
    "the crew/team surface is not enabled this session (the operator launches it \
     with NEWT_TEAM). Accomplish this yourself with the available tools \
     (read_file/write_file/edit_file/run_command/...), or ask the operator to \
     enable a crew.";

/// The model-facing result for a `crew`/`compose_roster` dispatch when no
/// `CrewRunner` was injected. One factored message + regression point carrying
/// [`CREW_OFF_RECOVERY_HINT`], so the recoverable wording can never drift.
fn crew_off_recovery_result(name: &str) -> String {
    format!("'{name}' is unavailable: {CREW_OFF_RECOVERY_HINT}")
}

/// #721: the model-facing capability-denial message for an fs tool — the base
/// "{kind} does not permit '{path}'" line plus the recoverable, model-actionable
/// [`denial_recovery_hint`]. One factored message + regression point shared by
/// every fs denial (read_file / write_file / edit_file / delete_file / list_dir /
/// find), so the recoverable wording can never drift between them.
fn denied_fs_result(kind: &str, path: &str) -> String {
    format!(
        "capability denied: {kind} does not permit '{path}'. {}",
        denial_recovery_hint(kind, path)
    )
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
///    uses via [`denied_fs_result`]).
/// 2. The stale `extra_exec` config hint is gone from the model-facing message.
///    #721 superseded "edit your `[tui.permissions]` config" with the
///    model-actionable [`denial_recovery_hint`] (`request_permissions`), so the
///    model now sees the bare reason once plus that hint — never a config edit
///    it cannot perform mid-turn.
///
/// The #263 prompt path still falls back here on deny (and on a second denial
/// after a re-execution).
fn denied_run_command_result(envelope: &serde_json::Value, _color: bool) -> String {
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
fn exec_denial_target_label(envelope: &serde_json::Value) -> String {
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
fn shell_envelope_output(
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
    if out.trim().is_empty() {
        let code = envelope
            .get("exit_code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        format!("(exit {code})")
    } else {
        // The terminal follows the full output tail even when the model-facing
        // payload below is token-capped or replaced by a spill handle.
        if let Some(presentation) = presentation {
            presentation.override_result(out.clone());
        }
        // #726/#945: the MODEL-facing payload is capped by the shared TOKEN
        // budget using head+tail. When tool_offload is on, spill the FULL
        // redacted output before capping so the true tail and elided middle stay
        // recoverable via memory_fetch("spill:<id>") and grep.
        let max_tokens = max_output_tokens();
        let est = crate::tokens::TokenEstimation::default();
        let over_model_budget = max_tokens != 0 && est.tokens_for_chars(out.len()) > max_tokens;
        let over_spill_budget = out.chars().count() > spill::TOOL_RESULT_SPILL_CAP;
        let should_spill =
            max_tokens != 0 && tool_offload && (over_model_budget || over_spill_budget);
        let capped = if should_spill {
            match spill_store {
                Some(store) => {
                    let (id, redacted) = spill::store_redacted_full(&out, store);
                    let teaser_tokens =
                        est.tokens_for_chars(spill::TOOL_RESULT_SPILL_CAP.saturating_sub(512));
                    cap_model_output_with_handle(
                        &redacted,
                        max_tokens.min(teaser_tokens),
                        output_head_tokens(),
                        Some(&id),
                    )
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
        match pr_creation_url(&out) {
            Some(url) => format!("{capped}{}", pr_next_step_hint(url)),
            None => capped,
        }
    }
}

/// #898: the forge "open a pull/merge request" URL that git prints on `push` of
/// a new branch — GitHub `…/pull/new/<branch>` (or a `…/compare/…` link) and
/// GitLab `…/merge_requests/new…`. Returned so [`shell_envelope_output`] can
/// append a next-step hint: models routinely push and then stall instead of
/// opening the PR (issue #898). Scans whitespace-split tokens because git emits
/// the URL on its own `remote:`-prefixed line.
fn pr_creation_url(output: &str) -> Option<&str> {
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

fn exec_denial_requests(envelope: &serde_json::Value) -> Option<Vec<PermissionRequest>> {
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
fn net_denial_requests(envelope: &serde_json::Value) -> Option<Vec<PermissionRequest>> {
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
fn denial_axis_label(envelope: &serde_json::Value) -> &'static str {
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

/// Consult the #263 gate for one denied fs path. Returns `true` only when
/// the human allowed it AND the re-minted caveats actually permit the path —
/// the widened authority is re-checked, never assumed.
fn fs_gate_allows(
    gate: &mut dyn PermissionGate,
    tool: &str,
    kind: DenialKind,
    full_path: &str,
    axis: impl Fn(&crate::caveats::Caveats) -> &crate::caveats::Scope<String>,
) -> bool {
    let request = PermissionRequest {
        tool: tool.to_string(),
        kind,
        target: full_path.to_string(),
        reason: format!("{} does not permit '{full_path}'", kind.as_str()),
    };
    match gate.ask(std::slice::from_ref(&request)) {
        PermissionDecision::Allow(widened) => tui_permits_path(axis(&widened), full_path),
        PermissionDecision::Deny => false,
    }
}

/// #1056: is `out` the embedded git tool's capability-denial for a WRITE op
/// (`add`/`commit`/`reset`/`branch`/…), as opposed to an engine error (e.g.
/// "nothing to commit") or a read? newt-git returns `capability denied: git <op>
/// not permitted` when the projected [`GitCaveats`](crate::git_caveats::GitCaveats)
/// deny a write; read ops (`status`/`log`/`diff`) are ungated so they never
/// produce this, and engine errors don't carry the `capability denied` marker.
fn is_git_write_denial(out: &str) -> bool {
    out.contains("capability denied: git ") && out.contains("not permitted")
}

/// #1056: route a denied LOCAL git write through the gate — the git sibling of
/// [`fs_gate_allows`]. The git-write capability is **non-axis** (it widens no
/// `Caveats` axis), so the decision is binary: `Allow` ⇒ the git arm re-dispatches
/// under the local-write surface; `Deny` (or no gate, i.e. headless) ⇒ keep the
/// denial. The readonly-`/mode` FLOOR is enforced inside
/// [`PermissionGate::ask`], which refuses the grant when the active preset
/// projects no git-commit authority — so a git write can never pierce it here.
/// #1191: git operations that DESTROY work irrecoverably — `stash-drop` (drops
/// a saved stash for good) and `branch-delete` (loses a branch's unique
/// commits). These are the `rm -rf` of git: a compaction-confused model ran
/// exactly this sequence (stash → checkout main → stash-drop → branch-delete)
/// and destroyed 136 lines of the operator's requested work. Unlike a normal
/// git WRITE (commit/add/checkout), a data-loss op is NEVER blanket-allowed —
/// not even under --full-access — so the operator always gets a say before
/// work is destroyed (the danger-table principle: some ops can't be granted
/// away). Read-side and constructive ops (commit/branch/checkout/stash-pop…)
/// are unaffected.
fn is_git_data_loss_op(op: &str) -> bool {
    matches!(op, "stash-drop" | "branch-delete")
}

/// Ask the operator to confirm a data-loss git op (#1191). Returns true only on
/// an explicit Allow; no gate (headless) or a decline means DON'T destroy the
/// work. Distinct reason text so the prompt reads as a destructive-action
/// confirmation, not a routine authority grant.
fn git_data_loss_confirmed(gate: &mut dyn PermissionGate, op: &str) -> bool {
    let request = PermissionRequest {
        tool: "git".to_string(),
        kind: DenialKind::GitWrite,
        target: op.to_string(),
        reason: format!(
            "git {op} DESTROYS work irrecoverably (a dropped stash / deleted              branch cannot be recovered) — confirm before proceeding"
        ),
    };
    matches!(
        gate.ask(std::slice::from_ref(&request)),
        PermissionDecision::Allow(_)
    )
}

fn git_gate_allows(gate: &mut dyn PermissionGate, op: &str) -> bool {
    let request = PermissionRequest {
        tool: "git".to_string(),
        kind: DenialKind::GitWrite,
        target: op.to_string(),
        reason: format!("git {op} is outside the granted git-write authority"),
    };
    matches!(
        gate.ask(std::slice::from_ref(&request)),
        PermissionDecision::Allow(_)
    )
}

/// #721: map the model-supplied `capability` string for `request_permissions`
/// onto a [`DenialKind`] axis. A small synonym set absorbs the names a weak
/// local model tends to emit; an unrecognized value returns `None` so the tool
/// coaches instead of guessing an axis (guessing would request the WRONG
/// authority). Pure — unit-tested directly.
fn parse_capability(s: &str) -> Option<DenialKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "exec" | "run" | "run_command" | "command" | "shell" => Some(DenialKind::Exec),
        "fs_read" | "fs-read" | "read" | "read_file" => Some(DenialKind::FsRead),
        "fs_write" | "fs-write" | "write" | "write_file" => Some(DenialKind::FsWrite),
        "net" | "network" | "web" | "web_fetch" => Some(DenialKind::Net),
        _ => None,
    }
}

/// #721: the model-facing `request_permissions` tool — the capability-GRANT
/// path. It builds a [`PermissionRequest`] from `{capability, target, reason}`
/// and consults the SAME #263 [`PermissionGate`] a denial would: `Allow` reports
/// granted (and the gate has remembered any session grant, so the model's retry
/// of the original op rides the existing #263 re-exec machinery), `Deny` reports
/// declined, and **no gate** (headless / eval / ACP) reports that no operator is
/// available to grant — a recoverable signal (switch strategy), never a hang.
///
/// Reconciliation with #728: `request_permissions` (capability GRANT via
/// `gate.ask` / the #263 flow) and `request_user_input` (generic free-text Q&A
/// via `gate.ask_question`) are DISTINCT tools that share the ONE human-interface
/// gate ([`PermissionGate`]). They realize "both surface to the human" without
/// being merged: this one widens authority through the ocap gate, the other only
/// gathers text. `request_permissions` is deliberately NOT routed through
/// `request_user_input` — it mints caveats, which a free-text answer cannot.
fn execute_request_permissions(
    args: &serde_json::Value,
    gate: Option<&mut dyn PermissionGate>,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let capability = args["capability"].as_str().unwrap_or("").trim();
    let target = args["target"].as_str().unwrap_or("").trim();
    let reason = args["reason"].as_str().unwrap_or("").trim();
    let Some(kind) = parse_capability(capability) else {
        return format!(
            "request_permissions: unknown capability '{capability}'. Use one of: \
             exec, fs_read, fs_write, net."
        );
    };
    if target.is_empty() {
        return "request_permissions: 'target' is required — the command name (exec), \
                   the path (fs_read/fs_write), or the host (net)."
            .to_string();
    }

    let request = PermissionRequest {
        tool: "request_permissions".to_string(),
        kind,
        target: target.to_string(),
        reason: if reason.is_empty() {
            format!("model requested {capability} for '{target}'")
        } else {
            reason.to_string()
        },
    };

    let out = match gate {
        // The gate consults the operator and (for a session grant) remembers it,
        // exactly as a denial-driven prompt does. We do not re-execute anything
        // here — the model retries its original tool call, which rides the #263
        // re-exec path under the now-granted caveats.
        Some(g) => match g.ask(std::slice::from_ref(&request)) {
            PermissionDecision::Allow(_widened) => format!(
                "granted: the operator allowed {capability} for '{target}'. \
                 Retry the original operation now."
            ),
            PermissionDecision::Deny => format!(
                "denied: the operator declined {capability} for '{target}'. \
                 Do not retry it — take a different approach."
            ),
        },
        // Headless / eval / ACP: no interactive gate exists to grant authority.
        // This is recoverable (change strategy), not a config edit the model
        // can perform mid-turn.
        None => format!(
            "no operator available to grant {capability} for '{target}' — this session \
             has no interactive permission gate (headless / eval / piped). The capability \
             must be configured by the owner (e.g. [tui.permissions] in newt config); \
             take a different approach for now."
        ),
    };
    out
}

/// #728: returned by `request_user_input` when there is no human to ask — either
/// no interactive gate this session (headless / eval / ACP / piped) or the gate
/// has no operator available (`ask_question` returned `None`). A recoverable
/// signal the model can act on, NEVER a hang.
const HEADLESS_NO_HUMAN: &str = "no human available this session (running headless) \
    — proceed with your best judgment or state your assumption explicitly.";

/// #728: the model-facing `request_user_input` tool — the GENERIC ask-the-human
/// path. It surfaces a free-text `question` to the operator through the SAME
/// human-interface gate a permission prompt uses ([`PermissionGate::ask_question`])
/// and returns the typed answer. With an operator present the answer is returned
/// verbatim; with NO gate (headless / eval / ACP / piped) — or when the gate has
/// no human to consult (`ask_question` returns `None`) — it returns the
/// [`HEADLESS_NO_HUMAN`] message and NEVER blocks.
///
/// Reconciliation with #721: this is the free-text Q&A path; `request_permissions`
/// is the capability-GRANT path (it mints caveats via the gate). Both surface to
/// the human through the one gate but are DISTINCT tools — one gathers text
/// (`ask_question`), the other widens authority (`ask`) — and are not merged.
fn execute_request_user_input(
    args: &serde_json::Value,
    gate: Option<&mut dyn PermissionGate>,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let question = args["question"].as_str().unwrap_or("").trim();

    if question.is_empty() {
        return "request_user_input: 'question' is required — the free-text \
                   question to ask the human."
            .to_string();
    }

    // `Some(answer)` from the gate → return it verbatim; a `None` gate (headless)
    // OR an `ask_question` that returns `None` (no human to consult) → the
    // recoverable headless message. Either way we never block without an answer.
    match gate.and_then(|g| g.ask_question(question)) {
        Some(answer) => answer,
        None => HEADLESS_NO_HUMAN.to_string(),
    }
}

/// Best-effort host extraction for the #263 net pre-check. This only gates
/// whether to PROMPT — reachability enforcement stays with the bridle's
/// leash (host allowlist + SSRF screen). `None` (unparseable / non-http URL)
/// skips the pre-check entirely, leaving today's dispatch path untouched.
pub(crate) fn host_of_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    // IPv6 literal `[::1]:8080` — the host is the bracketed part.
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        host_port.split(':').next()?
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// File-type restriction for the embedded `find` tool (#496).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FindType {
    Files,
    Dirs,
    Any,
}

/// Result ordering for the embedded `find` tool (#1258). `Name` is the historical
/// default (paths ascending); `Size` orders by byte size descending so an
/// evidence-only turn can answer "the N largest files" without shell access.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FindSort {
    Name,
    Size,
}

/// Parsed, validated options for one `find` invocation.
struct FindOpts<'a> {
    /// Glob matched against each basename; `None` matches everything.
    name: Option<&'a str>,
    type_filter: FindType,
    /// Max depth below the search root (1 = immediate children); `None` =
    /// unlimited.
    max_depth: Option<usize>,
    /// Hard cap on returned matches.
    max_results: usize,
    /// Honour .gitignore + skip .git/target/node_modules/hidden dirs.
    respect_gitignore: bool,
    case_sensitive: bool,
    /// Prefix each line with the entry's byte size + a tab (#1258).
    show_size: bool,
    /// Result ordering (#1258): [`FindSort::Name`] (default) or size-descending.
    sort: FindSort,
}

/// One-line summary of a `find` invocation for the tool trace (#529): the path
/// plus only the *non-default* filters, so two searches with different filters
/// don't both render as a bare `find: .`. Defaults (any type, unlimited depth,
/// the 1000-match cap, gitignore-respecting, case-sensitive) are omitted.
fn find_detail(path: &str, opts: &FindOpts) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(name) = opts.name {
        parts.push(format!("name={name}"));
    }
    match opts.type_filter {
        FindType::Files => parts.push("type=f".to_string()),
        FindType::Dirs => parts.push("type=d".to_string()),
        FindType::Any => {}
    }
    if let Some(d) = opts.max_depth {
        parts.push(format!("depth={d}"));
    }
    // Mirrors the parse default in the `find` arm.
    if opts.max_results != 1000 {
        parts.push(format!("max={}", opts.max_results));
    }
    if !opts.respect_gitignore {
        parts.push("no-gitignore".to_string());
    }
    if !opts.case_sensitive {
        parts.push("icase".to_string());
    }
    if matches!(opts.sort, FindSort::Size) {
        parts.push("sort=size".to_string());
    }
    if opts.show_size {
        parts.push("size".to_string());
    }
    if parts.is_empty() {
        path.to_string()
    } else {
        format!("{path} ({})", parts.join(", "))
    }
}

fn find_opts_from_args(args: &serde_json::Value) -> FindOpts<'_> {
    FindOpts {
        name: args["name"].as_str(),
        type_filter: match args["type"].as_str() {
            Some("f") => FindType::Files,
            Some("d") => FindType::Dirs,
            _ => FindType::Any,
        },
        max_depth: args["max_depth"].as_u64().map(|d| d as usize),
        max_results: args["max_results"]
            .as_u64()
            .map(|m| m as usize)
            .unwrap_or(1000),
        respect_gitignore: args["respect_gitignore"].as_bool().unwrap_or(true),
        case_sensitive: args["case_sensitive"].as_bool().unwrap_or(true),
        show_size: args["show_size"].as_bool().unwrap_or(false),
        sort: match args["sort"].as_str() {
            Some("size") => FindSort::Size,
            _ => FindSort::Name,
        },
    }
}

/// Pure: order, de-duplicate, truncate, and format the collected `(size, path)`
/// matches per `opts` (#1258). Split out of [`find_walk`] so the ordering /
/// truncation / formatting is unit-testable without touching the filesystem.
///
/// - [`FindSort::Name`]: paths ascending (the historical default).
/// - [`FindSort::Size`]: byte size **descending**, path breaking ties so the
///   order is deterministic.
///
/// `show_size` prefixes each line with the byte size and a tab. Truncation to
/// `max_results` happens AFTER ordering (so `sort=size` yields the true top-N,
/// not the first-N-walked), and reports whether any match was dropped.
fn finalize_find(mut entries: Vec<(u64, String)>, opts: &FindOpts<'_>) -> (Vec<String>, bool) {
    // De-duplicate by path (defensive — the walk shouldn't repeat) via a path
    // sort, which also establishes the Name ordering.
    entries.sort_by(|a, b| a.1.cmp(&b.1));
    entries.dedup_by(|a, b| a.1 == b.1);
    if matches!(opts.sort, FindSort::Size) {
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    let truncated = entries.len() > opts.max_results;
    entries.truncate(opts.max_results);
    let lines = entries
        .into_iter()
        .map(|(size, path)| {
            if opts.show_size {
                format!("{size}\t{path}")
            } else {
                path
            }
        })
        .collect();
    (lines, truncated)
}

/// Stable detail for the universal tool audit header. This is deliberately
/// value-aware for known tools, so content-bearing arguments are summarized
/// instead of copied into the terminal transcript.
fn tool_call_detail(name: &str, args: &serde_json::Value, workspace: &std::path::Path) -> String {
    let string = |key: &str, default: &str| {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    match name {
        "run_command" => string("command", ""),
        "write_file" => {
            let path = string("path", "");
            let bytes = args["content"].as_str().unwrap_or("").len();
            format!("{path} ({bytes} bytes)")
        }
        "read_file" | "edit_file" | "delete_file" => string("path", ""),
        "list_dir" => string("path", "."),
        "find" => {
            let path = args["path"].as_str().unwrap_or(".");
            find_detail(path, &find_opts_from_args(args))
        }
        "use_skill" => string("name", ""),
        "web_fetch" => string("url", ""),
        "request_permissions" => string("capability", ""),
        "request_user_input" => string("question", ""),
        "prompt_read" | "artifact_read" => string("address", "current"),
        "tool_search" | "recall" | "code_search" | "experience_recall" => string("query", ""),
        "memory_fetch" => string("address", ""),
        "save_note" => string("action", ""),
        "git" => string("op", ""),
        "state_set" | "state_get" => string("key", ""),
        "experience_record" => string("task", ""),
        "lifecycle" => {
            let phase = string("phase", "");
            let action = string("action", "run");
            let resolved = crate::tooling::Phase::from_key(&phase)
                .map(|phase| crate::tooling::resolved_phase_commands(workspace, phase))
                .unwrap_or_default();
            if resolved.is_empty() {
                format!("{phase} ({action})")
            } else {
                format!("{phase} ({action}) → {}", resolved.join(" && "))
            }
        }
        "render_report" => string("title", ""),
        "resume_context"
        | "state_clear"
        | "update_plan"
        | "plan_get"
        | "get_context_remaining"
        | "enter_plan_mode"
        | "exit_plan_mode" => String::new(),
        _ => args.to_string(),
    }
}

fn correction_alias_detail(args: &serde_json::Value) -> String {
    if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
        if let Some(content) = args.get("content").and_then(serde_json::Value::as_str) {
            return format!("{path} ({} bytes)", content.len());
        }
        let mut sizes = Vec::new();
        for key in ["old_string", "new_string"] {
            if let Some(value) = args.get(key).and_then(serde_json::Value::as_str) {
                sizes.push(format!("{key}={} bytes", value.len()));
            }
        }
        if sizes.is_empty() {
            return path.to_string();
        }
        return format!("{path} ({})", sizes.join(", "));
    }
    let keys = args
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if keys.is_empty() {
        "{}".to_string()
    } else {
        format!("arguments: {}", keys.join(", "))
    }
}

/// Resolve the operator-facing name and detail before execution while keeping
/// the executor's raw-name security checks intact. Transparent aliases and
/// shell reads use the canonical governed tool; corrective aliases retain the
/// attempted name but never echo content-bearing values.
pub(crate) fn tool_presentation(
    raw_name: &str,
    raw_args: &serde_json::Value,
    workspace: &std::path::Path,
) -> (String, String) {
    let (name, correction) = match resolve_tool_alias(raw_name) {
        Some(AliasOutcome::Rewrite(canonical)) => (canonical, false),
        Some(AliasOutcome::Correct(_)) => (raw_name, true),
        None => (raw_name, false),
    };
    if correction {
        return (name.to_string(), correction_alias_detail(raw_args));
    }

    if name == "run_command" && !routing_disabled() {
        let command = raw_args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if let super::routing::RouteDecision::Route { tool, args } =
            super::routing::RouteTable::builtin().classify(command)
        {
            let detail = tool_call_detail(tool, &args, workspace);
            return (tool.to_string(), detail);
        }
    }

    (
        name.to_string(),
        tool_call_detail(name, raw_args, workspace),
    )
}

/// Translate a shell-style basename glob (`*`, `?`) into an anchored regex.
/// Every other character is matched literally (regex metacharacters escaped),
/// so `pyo3_module.rs` matches only that exact basename, not `pyo3Xmodulexrs`.
fn glob_to_regex(glob: &str, case_sensitive: bool) -> Result<regex::Regex, String> {
    let mut re = String::with_capacity(glob.len() + 8);
    if !case_sensitive {
        re.push_str("(?i)");
    }
    re.push('^');
    for ch in glob.chars() {
        match ch {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            // Escape every regex metacharacter so the rest is literal.
            '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' => {
                re.push('\\');
                re.push(ch);
            }
            other => re.push(other),
        }
    }
    re.push('$');
    regex::Regex::new(&re).map_err(|e| format!("invalid name pattern: {e}"))
}

/// Recursively walk `root` and collect matches as workspace-relative,
/// `/`-normalised, sorted paths. Pure-`ignore`-crate traversal (no shell, no
/// subprocess) — the whole point of #496. Never follows symlinked directories
/// (avoids cycles and workspace escapes). Returns `(matches, truncated)` where
/// `truncated` is true if `max_results` was reached and more existed.
fn find_walk(
    root: &std::path::Path,
    workspace_root: &std::path::Path,
    opts: &FindOpts<'_>,
) -> Result<(Vec<String>, bool), String> {
    let pattern = match opts.name {
        Some(g) if !g.is_empty() => Some(glob_to_regex(g, opts.case_sensitive)?),
        _ => None,
    };

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(opts.respect_gitignore)
        .ignore(opts.respect_gitignore)
        .git_ignore(opts.respect_gitignore)
        .git_global(opts.respect_gitignore)
        .git_exclude(opts.respect_gitignore)
        .parents(opts.respect_gitignore)
        // Honour .gitignore even outside a git repo (the agent's cwd may not be
        // a checkout); without this `ignore` silently ignores gitignore files.
        .require_git(false)
        .follow_links(false);
    if let Some(d) = opts.max_depth {
        builder.max_depth(Some(d));
    }
    // The `ignore` walker prunes via .gitignore/hidden but has no built-in
    // skip for build/dep dirs. Prune them explicitly (and cheaply, before
    // descent) so a default `find` doesn't drown in target/ or node_modules/.
    // `.git` is already covered by `.hidden(true)`. Skipped only when respecting
    // ignores — `respect_gitignore=false` means "search everything".
    if opts.respect_gitignore {
        let mut ob = ignore::overrides::OverrideBuilder::new(root);
        // In override globs a leading `!` excludes; with no whitelist globs
        // present, everything else stays included.
        if ob.add("!target/").is_ok() && ob.add("!node_modules/").is_ok() {
            if let Ok(ov) = ob.build() {
                builder.overrides(ov);
            }
        }
    }

    // Collect every match as `(byte size, workspace-relative path)`. The whole
    // match set is gathered (not truncated mid-walk) so `sort=size` can order the
    // full set and return the TRUE top-N; `finalize_find` then orders, truncates,
    // and formats. The walk still prunes target/node_modules/gitignored paths, so
    // the collected set stays bounded for a source workspace.
    let mut entries: Vec<(u64, String)> = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            // Skip individual unreadable entries rather than failing the walk.
            Err(_) => continue,
        };
        // depth 0 is the search root itself — never a match.
        if entry.depth() == 0 {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        match opts.type_filter {
            FindType::Files if is_dir => continue,
            FindType::Dirs if !is_dir => continue,
            _ => {}
        }
        if let Some(re) = &pattern {
            let base = entry.file_name().to_string_lossy();
            if !re.is_match(&base) {
                continue;
            }
        }
        // Size is read only when it will be used (shown or sorted on) — an
        // unreadable metadata falls back to 0 rather than dropping the match.
        let size = if opts.show_size || matches!(opts.sort, FindSort::Size) {
            entry.metadata().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        let rel = entry
            .path()
            .strip_prefix(workspace_root)
            .unwrap_or_else(|_| entry.path());
        entries.push((size, rel.to_string_lossy().replace('\\', "/")));
    }
    Ok(finalize_find(entries, opts))
}

/// Execute a single tool call and return the result string sent back to the model.
///
/// `run_command` is routed through agent-bridle's Caveats-confined, brush-backed
/// `shell` tool: the WHOLE command runs inside the leash (`echo ok && rm -rf /`
/// no longer slips `rm` past an `echo` grant — every external spawn passes the
/// interceptor's `before_exec` / `before_open` gate). The fs tools
/// (`read_file` / `write_file` / `list_dir`) keep enforcing the same `caveats`
/// via `permits_*` — rerouting them is out of scope.
///
/// `note_sink` backs the `save_note` tool (Step 19.3), `recall_source` the
/// `recall` tool (Step 17.5), and `memory_source` the `memory_fetch` tool
/// (progressive-disclosure memory, #319). `None` ⇒ the tool was never
/// advertised, so a call here is treated like any unknown tool.
///
/// `permission_gate` is the #263 prompted-grant seam: when present, a
/// capability denial consults the human (allow once / session allow / deny)
/// before failing; an allow re-executes the denied call under the gate's
/// freshly minted caveats. `None` (the default, and every headless caller)
/// keeps every denial exactly as it was — bit-for-bit. #721's
/// `request_permissions` tool also rides this gate: it lets the MODEL proactively
/// request a grant (vs. only reacting to a denial), and reports "no operator
/// available" when the gate is `None`.
///
/// INTERIM (#297): when [`ocap_disabled`] is asserted (`--disable-ocap` /
/// `--yolo` / `NEWT_DISABLE_OCAP=1`), `run_command` skips the confined shell
/// and runs on the plain host shell with the same venv/PATH prefix and an
/// envelope of the same shape — nothing is denied, so the #263 gate is never
/// consulted for exec. Every other tool (fs fence, `web_fetch` leash) is
/// unaffected. Removed when brush upstreams `CommandInterceptor`
/// (agent-bridle#20).
///
/// `exec_floor` (issue #307) is the **named-permission-preset clamp** acting as
/// a hard authority FLOOR over exec. `None` (every existing caller, and the
/// no-preset case) leaves the `--disable-ocap` bypass exactly as it was —
/// bit-for-bit. `Some(scope)` makes the bypass conditional: an out-of-floor
/// command does NOT take the unconfined host path, it falls through to the
/// confined shell, which enforces the already-clamped `caveats` and denies it.
/// This is what makes a deliberately-restricted on-call/triage mode win over a
/// `--yolo` flag — the preset clamp is consulted as a ceiling the bypass
/// cannot cross.
/// Open one artifact candidate without ever blocking on a FIFO/device or
/// following a final symlink. Artifact capture is diagnostic, so platforms
/// without this race-safe primitive fail closed rather than weakening the file
/// tools' existing mutation policy.
#[cfg(unix)]
fn artifact_open_regular_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact path is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn artifact_open_regular_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    // Open the final component itself, rather than following a symlink or
    // another reparse point that could have replaced it after the lexical
    // check. This is the Windows analogue of Unix O_NOFOLLOW.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact path is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn artifact_open_regular_file(_path: &std::path::Path) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "race-safe artifact file capture is unavailable on this platform",
    ))
}

fn artifact_preimage_state(
    path: &std::path::Path,
    read_authorized: bool,
) -> super::artifact_hooks::ArtifactFileState {
    use std::io::Read as _;

    if !read_authorized {
        return super::artifact_hooks::ArtifactFileState::unavailable("fs_read_not_granted");
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            super::artifact_hooks::ArtifactFileState::unavailable("symlink_preimage_not_hashed")
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            super::artifact_hooks::ArtifactFileState::unavailable("preimage_not_regular_file")
        }
        Ok(_) => {
            let mut file = match artifact_open_regular_file(path) {
                Ok(file) => file,
                Err(_) => {
                    return super::artifact_hooks::ArtifactFileState::unavailable(
                        "preimage_read_failed",
                    )
                }
            };
            let mut hasher = blake3::Hasher::new();
            let mut bytes = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        hasher.update(&buffer[..read]);
                        bytes = bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
                    }
                    Err(_) => {
                        return super::artifact_hooks::ArtifactFileState::unavailable(
                            "preimage_read_failed",
                        )
                    }
                }
            }
            super::artifact_hooks::ArtifactFileState::from_digest(
                hasher.finalize().to_hex().to_string(),
                bytes,
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            super::artifact_hooks::ArtifactFileState::absent()
        }
        Err(_) => super::artifact_hooks::ArtifactFileState::unavailable("preimage_read_failed"),
    }
}

/// Verify a governed write against the bytes it submitted without allocating a
/// second copy of the file. Only a regular file can satisfy the postcondition.
fn artifact_file_matches(path: &std::path::Path, expected: &[u8]) -> std::io::Result<bool> {
    use std::io::Read as _;

    let mut file = artifact_open_regular_file(path)?;
    let mut offset = 0_usize;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(offset == expected.len());
        }
        let Some(end) = offset.checked_add(read) else {
            return Ok(false);
        };
        if expected.get(offset..end) != Some(&buffer[..read]) {
            return Ok(false);
        }
        offset = end;
    }
}

/// Return true only when the artifact locator can be proven to resolve inside
/// the physical workspace. The file tools intentionally retain their existing
/// lexical OCAP policy, but provenance must not turn a write through an
/// in-workspace symlink into a false claim about workspace state.
///
/// For a path that does not exist yet, walk to the nearest existing ancestor.
/// An existing (including dangling) symlink must canonicalize successfully;
/// otherwise the provenance check fails closed instead of walking past it.
fn artifact_path_is_physically_within_workspace(
    workspace: &std::path::Path,
    target: &std::path::Path,
) -> bool {
    let Ok(workspace) = workspace.canonicalize() else {
        return false;
    };
    let mut probe = target;
    loop {
        match std::fs::symlink_metadata(probe) {
            Ok(_) => {
                return probe
                    .canonicalize()
                    .is_ok_and(|resolved| resolved.starts_with(&workspace));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = probe.parent() else {
                    return false;
                };
                probe = parent;
            }
            Err(_) => return false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record_governed_file_change(
    sink: Option<&dyn super::artifact_read::PromptArtifactSink>,
    context: Option<ArtifactReadContext<'_>>,
    path: &str,
    operation: &'static str,
    before: Option<super::artifact_hooks::ArtifactFileState>,
    after: super::artifact_hooks::ArtifactFileState,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let (Some(sink), Some(context), Some(before)) = (sink, context, before) else {
        return String::new();
    };
    match super::artifact_hooks::record_file_change(sink, context, path, operation, before, after) {
        Ok(_) => String::new(),
        Err(error) => {
            let warning = format!("warning: failed to record file-change artifact: {error}");
            format!("\n{warning}")
        }
    }
}

fn artifact_postcondition_warning(
    path: &str,
    detail: &str,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let warning =
        format!("warning: {path} changed, but no file-change artifact was recorded: {detail}");
    format!("\n{warning}")
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn super::scratchpad::ScratchpadStore>,
    code_search: Option<super::semantic::CodeSearch<'_>>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
) -> String {
    execute_tool_with_offload(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        build_check_cmd,
        note_sink,
        recall_source,
        memory_source,
        permission_gate,
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        experience_store,
        step_ledger,
        false,
        None,
        // The convenience wrapper carries no persona surface — callers that
        // enforce a persona allow-list use `execute_tool_with_offload` directly.
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_with_offload(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn super::scratchpad::ScratchpadStore>,
    code_search: Option<super::semantic::CodeSearch<'_>>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
) -> String {
    execute_tool_with_offload_and_prompt(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        build_check_cmd,
        note_sink,
        recall_source,
        memory_source,
        None,
        permission_gate,
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        experience_store,
        step_ledger,
        tool_offload,
        spill_store,
        persona_tools,
    )
    .await
}

/// Prompt-aware tool dispatcher used by inference loops.
///
/// The historical [`execute_tool_with_offload`] entry point remains
/// source-compatible for embedders; only loops carrying a verified active
/// prompt need this extended seam.
#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_with_offload_and_prompt(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    prompt_context: Option<PromptReadContext<'_>>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn super::scratchpad::ScratchpadStore>,
    code_search: Option<super::semantic::CodeSearch<'_>>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
) -> String {
    execute_tool_with_offload_and_prompt_and_artifacts(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        build_check_cmd,
        note_sink,
        recall_source,
        memory_source,
        prompt_context,
        None,
        None,
        permission_gate,
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        experience_store,
        step_ledger,
        tool_offload,
        spill_store,
        persona_tools,
        PromptDisposition::Act,
    )
    .await
}

/// Prompt- and artifact-aware tool dispatcher used by inference loops.
/// The prompt-only entry point above remains source-compatible for embedders.
#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_with_offload_and_prompt_and_artifacts(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
    artifact_sink: Option<&dyn super::artifact_read::PromptArtifactSink>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn super::scratchpad::ScratchpadStore>,
    code_search: Option<super::semantic::CodeSearch<'_>>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
) -> String {
    execute_tool_with_offload_and_prompt_and_artifacts_cancellable(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        build_check_cmd,
        note_sink,
        recall_source,
        memory_source,
        prompt_context,
        artifact_context,
        artifact_sink,
        permission_gate,
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        experience_store,
        step_ledger,
        tool_offload,
        spill_store,
        persona_tools,
        disposition,
        None,
        None,
    )
    .await
    .expect("tool execution without a cancellation flag cannot be interrupted")
}

/// Cancellation-aware loop entry point. The header is written synchronously
/// before the cancel-first race begins; an already-set interrupt therefore
/// closes a complete audit block without ever polling the tool body.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_tool_with_offload_and_prompt_and_artifacts_cancellable(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
    artifact_sink: Option<&dyn super::artifact_read::PromptArtifactSink>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn super::scratchpad::ScratchpadStore>,
    code_search: Option<super::semantic::CodeSearch<'_>>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
    live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Option<String> {
    let mut display = ToolDisplay::new(
        std::io::stdout(),
        color,
        super::display::term_cols(),
        super::display::spill_lines(),
    );
    execute_tool_with_display_cancellable(
        &mut display,
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        build_check_cmd,
        note_sink,
        recall_source,
        memory_source,
        prompt_context,
        artifact_context,
        artifact_sink,
        permission_gate,
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        experience_store,
        step_ledger,
        tool_offload,
        spill_store,
        persona_tools,
        disposition,
        live_tool_output,
        cancel,
    )
    .await
}

async fn wait_for_tool_cancellation(cancel: Option<&std::sync::atomic::AtomicBool>) {
    match cancel {
        None => std::future::pending::<()>().await,
        Some(flag) => {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_tool_with_display_cancellable<W: std::io::Write + Send>(
    display: &mut ToolDisplay<W>,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
    artifact_sink: Option<&dyn super::artifact_read::PromptArtifactSink>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn super::scratchpad::ScratchpadStore>,
    code_search: Option<super::semantic::CodeSearch<'_>>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
    live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Option<String> {
    let (presentation_name, presentation_detail) =
        tool_presentation(name, args, std::path::Path::new(workspace));
    display.call(&presentation_name, &presentation_detail);
    let result = {
        let execution = execute_tool_inner(
            display,
            name,
            args,
            workspace,
            color,
            tool_output_lines,
            caveats,
            mcp,
            build_check_cmd,
            note_sink,
            recall_source,
            memory_source,
            prompt_context,
            artifact_context,
            artifact_sink,
            permission_gate,
            exec_floor,
            git_tool,
            crew_runner,
            scratchpad_store,
            code_search,
            experience_store,
            step_ledger,
            tool_offload,
            spill_store,
            persona_tools,
            disposition,
            live_tool_output,
        );
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = wait_for_tool_cancellation(cancel) => None,
            result = &mut execution => Some(result),
        }
    };
    match result {
        Some(result) => {
            display.result(&result);
            Some(result)
        }
        None => {
            let result = format!("error: {name} interrupted — tool cancelled before completion");
            display.result(&result);
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_tool_inner(
    presentation: &mut dyn ToolPresentation,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
    artifact_sink: Option<&dyn super::artifact_read::PromptArtifactSink>,
    mut permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn super::scratchpad::ScratchpadStore>,
    code_search: Option<super::semantic::CodeSearch<'_>>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    // FR-1 part 2 (#997): the active persona's tool allow-list (its `tools:`
    // front-matter), or `None` when no persona is active. The name-scoped
    // enforcement half — advertisement is only cosmetic, so the boundary is
    // real ONLY because this executor refuses a disallowed name even if the
    // model calls it unprompted.
    persona_tools: Option<&[String]>,
    // The prompt's validated disposition. This is deliberately a required
    // dispatcher input: catalog filtering is cosmetic, whereas this check is
    // the boundary that refuses fabricated tool names before MCP routing,
    // aliases, or permission widening can run.
    disposition: PromptDisposition,
    live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
) -> String {
    // Prompt-comprehension boundary: enforce the validated disposition BEFORE
    // every other routing or grant path. In particular, unknown names (including
    // generic `server__tool` MCP calls) fail closed under a non-Act disposition;
    // there is no safe way to infer a remote tool's authority from its name.
    if !tool_allowed(disposition, name) {
        let msg = disposition_tool_denied_message(disposition, name);
        return msg;
    }

    // A read/recovery tool can itself hit a caveat denial (for example
    // `web_fetch` outside the net allow-list). Non-Act turns must never turn
    // that into an authority grant, so remove the human grant seam even for the
    // narrow tools the disposition permits. The existing caveats still decide
    // whether the read is legal.
    if disposition != PromptDisposition::Act {
        permission_gate = None;
    }

    // FR-3 (#998): the absolute deny-list — a grant-independent veto checked
    // immediately after the prompt-disposition boundary, above every other
    // leash (persona, MCP, alias, routing). It refuses
    // catastrophic exec (ssh / rm / systemctl restart …) by STRUCTURAL target,
    // so no capability, mode, or persona grant can unlock it. Runs on the RAW
    // name + args (pre-rewrite) so a shell alias or a routed command can't slip
    // past — and only the exec TARGET is matched, so the same words quoted in a
    // coach's question or a runbook note are untouched.
    if let Some(denied) = super::deny::deny_check(name, args) {
        return denied.reason;
    }

    // FR-1 part 2 (#997): persona tool allow-list — refuse a BUILT-IN tool the
    // active persona does not grant, before any routing (alias, run_command
    // redirect). Checked against the CANONICAL name (aliases resolved first) so a
    // denied tool can't slip past under a foreign spelling; the always-on infra
    // tools ([`is_always_on_tool`]) always pass or the loop would wedge.
    //
    // Remote MCP tools are EXCLUDED here (FR-2, #1001): they carry no fs/exec/net
    // axis, so instead of a hard veto they fall through to the `mcp.handles`
    // branch, which PROMPTS the human for a name-based grant. A built-in stays
    // hard-vetoed — only remote tools get the softer prompt.
    if let Some(allow) = persona_tools {
        let canonical = match resolve_tool_alias(name) {
            Some(AliasOutcome::Rewrite(canonical)) => canonical,
            _ => name,
        };
        if !persona_tool_allowed(canonical, allow) && !mcp.handles(name) {
            let msg = persona_tool_denied_message(canonical);
            return msg;
        }
    }

    // Remote MCP tools (namespaced `server__tool`) route to their server before
    // the built-in match. They map to NONE of the fs/exec/net caveat axes, so
    // FR-2 (#1001) gives them a NAME-based leash: the active persona's tool
    // allow-list. A tool the persona already grants dispatches directly; one it
    // does not is PROMPTED through the #263 [`PermissionGate`] (allow once /
    // session / deny) rather than hard-vetoed — so a human can grant a remote
    // READ tool on demand while a mutating one stays gated. With no persona
    // (`persona_tools == None`) remote tools dispatch unleashed, as before.
    if mcp.handles(name) {
        if let Some(allow) = persona_tools {
            if !persona_tool_allowed(name, allow) {
                let request = PermissionRequest {
                    tool: name.to_string(),
                    kind: DenialKind::RemoteTool,
                    target: name.to_string(),
                    reason: format!(
                        "remote tool `{name}` is outside the active persona's tool allow-list"
                    ),
                };
                // This branch always returns, so consuming the gate here never
                // races the later fs/exec dispatch (unreached once mcp handled).
                let granted = match permission_gate {
                    Some(gate) => {
                        matches!(gate.ask(&[request]), PermissionDecision::Allow(_))
                    }
                    // Headless / no operator to consult: fail-closed, like every
                    // other gate this session.
                    None => false,
                };
                if !granted {
                    let msg = persona_tool_denied_message(name);
                    return msg;
                }
            }
        }
        let out = mcp.call(name, args).await;
        return out;
    }

    // Step 27.1: resolve foreign / hallucinated tool names (str_replace_editor,
    // execute, bash, …) BEFORE the dispatch match. Compatible-arg aliases
    // rewrite to the canonical name and dispatch transparently; the rest return
    // a correction that names the right tool. Real names (and MCP `server__tool`
    // names, handled above) fall through unchanged.
    let name = match resolve_tool_alias(name) {
        Some(AliasOutcome::Rewrite(canonical)) => canonical,
        Some(AliasOutcome::Correct(msg)) => return msg,
        None => name,
    };

    // facade P4 (#780): hidden tool-call routing. After alias normalization, a
    // `run_command` (or a shell alias rewritten to one) whose command is a
    // read-only reach (`cat`/`ls`/`find` + read-only `git`) is SILENTLY
    // rewritten to the governed built-in, so the model's instinctive shell
    // calls go through the SAME fs / git caveat checks they would by calling
    // the built-in directly — routing is NOT a bypass (§4.4). The route/gate
    // split is pure DATA ([`super::routing::RouteTable`]). State-modifying git
    // and everything else stay on the exec path (`RouteDecision::Exec`).
    //
    // `--no-route` / `NEWT_NO_ROUTE` ([`routing_disabled`]) turns this L2
    // convenience OFF — the command runs the normal exec path as-is — while the
    // L3 boundary (the confined shell below, the fs fence) STAYS. It is a switch
    // DISTINCT from `--disable-ocap` (§7-F5): the routing escape never disables
    // confinement.
    let routed: Option<(&'static str, serde_json::Value)> =
        if name == "run_command" && !routing_disabled() {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let decision = super::routing::RouteTable::builtin().classify(command);
            // §4.4: log every silent rewrite (the original command + the
            // governed built-in it routed to). `None` ⇒ nothing was rewritten.
            if let Some(line) = super::routing::audit_line(command, &decision) {
                tracing::debug!(target: "newt::routing", "{line}");
            }
            match decision {
                super::routing::RouteDecision::Route { tool, args } => Some((tool, args)),
                super::routing::RouteDecision::Exec => None,
            }
        } else {
            None
        };
    let (name, args): (&str, &serde_json::Value) = match &routed {
        Some((tool, routed_args)) => (*tool, routed_args),
        None => (name, args),
    };

    match name {
        // Exact prompt recovery is always available. Durable TUI sessions pass
        // a conversation-fenced source; headless callers pass an ephemeral
        // context and still recover the exact active task text.
        "prompt_read" => match prompt_context {
            Some(context) => {
                let output = execute_prompt_read_silent(args, context);
                presentation.override_result(output.display);
                output.model
            }
            None => "prompt_read error: no active prompt context in this session".to_string(),
        },

        // Derived-work recovery is also always available. The context carries
        // only harness-owned prompt ids and an already conversation/workspace-
        // fenced source; model arguments can select an address, never a fence.
        "artifact_read" => match artifact_context {
            Some(context) => {
                let output = execute_artifact_read_silent(args, context);
                presentation.override_result(output.display);
                output.model
            }
            None => "error: artifact_read: no active artifact context in this session".to_string(),
        },

        // Model-curated memory (Step 19.3): routes add / replace / remove
        // through the caller's NoteSink — the same MemoryManager → NoteStore
        // path as `/remember`, so the 19.1 char-cap curator error and the
        // 19.2 write-time security scan apply identically.
        "save_note" => match note_sink {
            Some(sink) => execute_save_note(args, sink, color, tool_output_lines),
            // Without a sink the tool was never advertised — a call here is
            // a model hallucination; answer like any unknown tool.
            None => "unknown tool: save_note (no note store in this session)".to_string(),
        },

        // Cross-session recall (Step 17.5): searches PAST conversations via
        // the caller's RecallSource — workspace-fenced by the store, current
        // conversation excluded by the source implementation.
        "recall" => match recall_source {
            Some(source) => execute_recall(args, source, color, tool_output_lines),
            // Without a source the tool was never advertised — same
            // unknown-tool answer as the sink-less save_note path.
            None => "unknown tool: recall (no conversation store in this session)".to_string(),
        },

        // Progressive-disclosure memory (Workstream A MVP, #319): pulls the
        // verbatim body of one ADDRESSED item (`note:<id>` / `turn:<conv>#<seq>`)
        // via the caller's MemorySource — workspace-fenced by the underlying
        // NoteStore / ConversationStore. Same presence-gating as `recall`.
        "memory_fetch" => match memory_source {
            Some(source) => execute_memory_fetch(args, source, color, tool_output_lines),
            // Without a source the tool was never advertised — same
            // unknown-tool answer as the source-less recall path.
            None => "unknown tool: memory_fetch (no memory source in this session)".to_string(),
        },

        // Step 26.4 (#583): scratchpad state tools — presence-gated on the
        // injected store (advertised only when the `scratchpad` feature is on).
        "state_set" => match scratchpad_store {
            Some(s) => super::scratchpad::execute_state_set(args, s, color, tool_output_lines),
            None => "unknown tool: state_set (no scratchpad in this session)".to_string(),
        },
        "state_get" => match scratchpad_store {
            Some(s) => super::scratchpad::execute_state_get(args, s, color, tool_output_lines),
            None => "unknown tool: state_get (no scratchpad in this session)".to_string(),
        },
        "state_clear" => match scratchpad_store {
            Some(s) => super::scratchpad::execute_state_clear(s, color, tool_output_lines),
            None => "unknown tool: state_clear (no scratchpad in this session)".to_string(),
        },

        // Step 26.5.5 (#582): semantic code search — presence-gated on the
        // injected searcher (advertised only when the `semantic` feature is on).
        "code_search" => match code_search {
            Some(search) => {
                super::semantic::execute_code_search(args, search, color, tool_output_lines).await
            }
            None => {
                "unknown tool: code_search (semantic retrieval is off this session)".to_string()
            }
        },

        // Step 26.6a (#585): experiential record/recall — presence-gated on the
        // store (advertised only when the `experiential` feature is on).
        "experience_record" => match experience_store {
            Some(s) => {
                super::experiential::execute_experience_record(args, s, color, tool_output_lines)
            }
            None => "unknown tool: experience_record (experiential memory is off)".to_string(),
        },
        "experience_recall" => match experience_store {
            Some(s) => super::experiential::execute_experience_recall(
                args,
                s,
                super::experiential::EXPERIENCE_TOP_K,
                color,
                tool_output_lines,
            ),
            None => "unknown tool: experience_recall (experiential memory is off)".to_string(),
        },

        // Step 26.6b (#586) / #715 PR2: scheduled update_plan — the single plan
        // WRITE tool, presence-gated on the ledger (advertised only when the
        // `scheduled` feature is on). Replaces plan_set + plan_advance.
        "update_plan" => match step_ledger {
            Some(ledger) => {
                let mut out =
                    super::scheduled::execute_update_plan(args, ledger, color, tool_output_lines);
                if tool_result_ok(&out) {
                    if let (Some(sink), Some(context)) = (artifact_sink, artifact_context) {
                        let plan = ledger.snapshot();
                        if !plan.is_empty() {
                            if let Err(error) =
                                super::artifact_hooks::record_plan_revision(sink, context, &plan)
                            {
                                let warning =
                                    format!("warning: failed to record plan artifact: {error}");
                                out.push('\n');
                                out.push_str(&warning);
                            }
                        }
                    }
                }
                out
            }
            None => "unknown tool: update_plan (scheduled planning is off)".to_string(),
        },

        // #1193: enter/exit the read-only PLAN phase. The env flag is read by
        // the TUI when resolving per-turn caveats (it MEETs plan_phase_clamp,
        // which only narrows authority — self-restriction is always safe). The
        // clamp takes effect on the NEXT turn; this turn's remaining calls stay
        // under the current authority.
        "enter_plan_mode" => {
            // SAFETY: single-threaded session tool dispatch; the TUI reads it
            // between turns (same pattern as NEWT_FULL_ACCESS / NEWT_DISABLE_OCAP).
            unsafe { std::env::set_var("NEWT_PLAN_PHASE", "1") };
            "entered PLAN MODE (read-only): writes are denied until you call exit_plan_mode. Read/search the relevant code, draft the ordered steps with update_plan, then exit_plan_mode to execute."
                .to_string()
        }
        "exit_plan_mode" => {
            // SAFETY: as above.
            unsafe { std::env::remove_var("NEWT_PLAN_PHASE") };
            "exited PLAN MODE: writes re-enabled. Execute your plan step by step, marking each done with update_plan as you go."
                .to_string()
        }
        // #716: read-only plan view (the alias target for "what was I doing?"
        // probes) — same presence gate as update_plan.
        "plan_get" => match step_ledger {
            Some(l) => super::scheduled::execute_plan_get(l, color, tool_output_lines),
            None => "unknown tool: plan_get (scheduled planning is off)".to_string(),
        },

        // #714: self-scoped resume recovery — reads THIS conversation's recent
        // turns (via the RecallSource's this_conversation_recent, the opposite
        // of recall's filter), the <plan>, and the <state>. Advertised ALWAYS,
        // so it reuses the already-present recall_source / step_ledger /
        // scratchpad_store params and degrades gracefully when they are None.
        "resume_context" => super::resume::execute_resume_context(
            recall_source,
            step_ledger,
            scratchpad_store,
            prompt_context,
            color,
            tool_output_lines,
        ),

        // #721: the capability-GRANT request — rides the SAME #263 gate a denial
        // would consult. Advertised always; `permission_gate` is `None` for
        // headless / eval / ACP, where it answers "no operator available" rather
        // than blocking. Consumes the gate (mutually exclusive with the
        // run_command / fs arms that also use it — only one arm runs per call).
        "request_permissions" => {
            execute_request_permissions(args, permission_gate, color, tool_output_lines)
        }

        // #728: the GENERIC ask-the-human tool — surfaces a free-text question to
        // the operator via the SAME #263 human-interface gate (`ask_question`)
        // and returns the answer. Advertised always; `permission_gate` is `None`
        // for headless / eval / ACP, where it answers "no human available this
        // session" rather than blocking. Consumes the gate (mutually exclusive
        // with the run_command / fs / request_permissions arms that also use it —
        // only one arm runs per call).
        "request_user_input" => {
            execute_request_user_input(args, permission_gate, color, tool_output_lines)
        }

        // #725: tool discovery — search THIS session's advertised catalog by
        // intent so a model that half-remembers a capability finds the real tool
        // name instead of fabricating one. Advertised always; the catalog is
        // rebuilt here from the live presence sources (the `with_*` flags derive
        // from which optional capabilities this call was handed), so the search
        // reflects exactly what was advertised — built-ins, presence-gated tools,
        // AND the connected MCP `server__tool` entries. The matcher
        // ([`super::tool_search::execute_tool_search`]) is pure; the catalog
        // build lives here and presentation stays at the dispatcher boundary.
        "tool_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            // FR-1 part 2 (#997): search only what THIS persona may call, so
            // discovery never surfaces a tool the executor would then refuse.
            let catalog = filter_advertised_tools(
                merged_tool_definitions(
                    &*mcp,
                    note_sink.is_some(),
                    recall_source.is_some(),
                    memory_source.is_some(),
                    git_tool.is_some(),
                    crew_runner.is_some(),
                    scratchpad_store.is_some(),
                    code_search.is_some(),
                    experience_store.is_some(),
                    step_ledger.is_some(),
                ),
                persona_tools,
            );
            let catalog = filter_tools_for_disposition(catalog, disposition);
            super::tool_search::execute_tool_search(query, &catalog)
        }

        // Embedded git (PR4, #461): dispatch through the injected GitTool
        // (newt-git's LocalGitTool). `GitCaveats::from_session` projects the
        // session's authority onto the git surface (fail-closed: a read-only
        // session can read but not commit). Same presence-gating as `recall` —
        // without an injected impl the tool was never advertised.
        "git" => match git_tool {
            Some(tool) => {
                let gc = crate::git_caveats::GitCaveats::from_session(caveats);
                let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
                // #1191: data-loss ops (stash-drop / branch-delete) are gated
                // ALWAYS — even under --full-access — because they destroy work
                // irrecoverably. A confused (e.g. post-compaction) model must
                // not be able to `rm -rf` the operator's in-progress work; the
                // operator gets the final say. No gate / a decline refuses.
                if is_git_data_loss_op(op) {
                    let confirmed = permission_gate
                        .as_deref_mut()
                        .is_some_and(|gate| git_data_loss_confirmed(gate, op));
                    if !confirmed {
                        let refusal = format!(
                            "refused: git {op} destroys work irrecoverably and was not \
                             confirmed by the operator. If you stashed changes you still \
                             need, restore them with stash-pop/stash-apply instead of \
                             dropping; do NOT delete a branch that holds unmerged work. \
                             The operator must confirm any data-loss git op."
                        );
                        return refusal;
                    }
                }
                let mut out = match tool.dispatch(op, args, &gc) {
                    Ok(rendered) => rendered,
                    // Denials + engine errors surface verbatim so the model
                    // sees WHY (e.g. "denied: commit" on a read-only session).
                    Err(e) => format!("error: {e}"),
                };
                // #1056: a LOCAL git WRITE denied by the projected authority is
                // NOT a dead end (the trap that stranded the model between the git
                // tool and `run_command git`). Route it through the gate like
                // exec/fs: on an operator grant, re-dispatch under the local-write
                // surface (`GitCaveats::top()` — all LOCAL ops; network stays
                // closed / shell-net-gated). No gate (headless) or a decline keeps
                // the denial. The readonly-`/mode` floor is enforced in the gate.
                if is_git_write_denial(&out) {
                    let granted = permission_gate
                        .as_deref_mut()
                        .is_some_and(|gate| git_gate_allows(gate, op));
                    if granted {
                        out = match tool.dispatch(op, args, &crate::git_caveats::GitCaveats::top())
                        {
                            Ok(rendered) => rendered,
                            Err(e) => format!("error: {e}"),
                        };
                    }
                }
                out
            }
            None => "unknown tool: git (no git surface in this session)".to_string(),
        },

        // Agent-callable orchestration (#479): compose_roster proposes a crew
        // roster from the live environment; crew dispatches a crew/team on a task
        // and returns the diff + verify status for the overseer to review. Both
        // route through the injected CrewRunner, which runs spawned crews under
        // `meet`-attenuated caveats. Same presence-gating as `git` (the `/team`
        // toggle) — without an injected impl the tools were never advertised.
        "compose_roster" | "crew" => match crew_runner {
            Some(runner) => {
                let out = match runner.dispatch(name, args, caveats).await {
                    Ok(rendered) => rendered,
                    Err(e) => format!("error: {e}"),
                };
                out
            }
            // #479 (G4): replace the flat dead-end with a recoverable coach —
            // name the operator gesture (NEWT_TEAM) + a real solo alternative.
            None => crew_off_recovery_result(name),
        },

        "run_command" => {
            let raw_cmd = args["command"].as_str().unwrap_or("");

            // Fold a leading `cd <path> &&` into the run cwd so the `cd`
            // builtin never reaches exec (it isn't an executable — `execvp("cd")`
            // fails). The remainder runs where the model meant, and the OCAP
            // prompt names the real capability, not `cd … && …`.
            let (cd_path, cmd_owned) = split_leading_cd(raw_cmd);
            let cmd = cmd_owned.as_str();

            // A bare `cd <path>` with nothing after it: directory changes don't
            // persist between independent commands (#1159 carries cwd per call),
            // so there is nothing to run. Guide the model to the mechanism that
            // works instead of failing on an un-exec'able builtin.
            if cd_path.is_some() && cmd.trim().is_empty() {
                let path = cd_path.as_deref().unwrap_or("");
                return format!(
                    "note: a bare `cd` has no effect — each command runs \
                     independently, so there is no persistent shell to change. \
                     Prefix the command instead (`cd {path} && <command>`, which \
                     newt runs in `{path}`) or pass `cwd`."
                );
            }

            // Corrective guard: the model tried to call a tool as a shell binary.
            // Return a correction so the model can retry with the right tool call.
            // #898: git NETWORK ops (push/fetch/pull/clone) are NOT bounced — the
            // embedded git tool can't do them, so they fall through to the shell
            // (net-gated), letting the model push a branch and open a PR.
            if let Some(tool) = run_command_redirect(cmd) {
                return format!(
                    "error: '{tool}' is a tool, not a shell command. \
                     Call it as a separate tool invocation — \
                     do not pass '{tool}' as a command argument to run_command."
                );
            }

            // Route the WHOLE command through agent-bridle's confined shell
            // (free-form `cmd` mode) under the SAME Caveats the TUI resolved from
            // `[tui].permissions`. The confined-exec core is shared with the
            // `lifecycle` arm (#891) so both honor identical exec caveats. A
            // folded leading `cd` becomes the cwd (it wins over an explicit
            // `cwd` arg — it's the more specific, in-command intent).
            let run_cwd = resolve_exec_cwd(
                workspace,
                cd_path.as_deref().or_else(|| args["cwd"].as_str()),
            );
            exec_confined_command(
                cmd,
                &run_cwd,
                color,
                tool_output_lines,
                caveats,
                exec_floor,
                permission_gate,
                tool_offload,
                spill_store,
                live_tool_output.clone(),
                presentation,
            )
            .await
        }

        // #891: the model-facing lifecycle surface over the #880 system. Resolve
        // THIS repo's command for the named phase (`.newt/config.toml
        // [lifecycle]` → matching tooling packs) and run it through the SAME
        // confined exec path as run_command. `action=list` returns the resolved
        // command WITHOUT running it — a pure discovery read.
        // #1004: present collected findings as a rendered Markdown document in
        // the plain scroller. Always-on (no injected capability); hands the
        // rendered block to the presenter and returns a short ack to the model.
        "render_report" => {
            let (result, document) = execute_render_report(args, color);
            if let Some(document) = document {
                presentation.document(&document);
            }
            result
        }

        "lifecycle" => {
            let phase_key = args.get("phase").and_then(|v| v.as_str()).unwrap_or("");
            let Some(phase) = crate::tooling::Phase::from_key(phase_key) else {
                let valid = crate::tooling::Phase::ALL
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return format!(
                    "error: unknown lifecycle phase '{phase_key}'. Valid phases: {valid}."
                );
            };
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("run");
            // Resolve from the `[lifecycle]` override → matching tooling packs.
            // Several toolchains may each contribute a command; join with `&&`
            // so the confined shell runs them in sequence and short-circuits on
            // the first failure (the safe-subset engine supports `&&`).
            let cmds =
                crate::tooling::resolved_phase_commands(std::path::Path::new(workspace), phase);
            if cmds.is_empty() {
                return format!(
                    "no command configured for lifecycle phase '{}'. Set it in \
                     .newt/config.toml [lifecycle] or a tooling pack; `lifecycle` \
                     only runs commands the project declares.",
                    phase.as_str()
                );
            }
            let joined = cmds.join(" && ");
            match action {
                "list" => format!("lifecycle {} → {joined}", phase.as_str()),
                "run" => {
                    exec_confined_command(
                        &joined,
                        workspace,
                        color,
                        tool_output_lines,
                        caveats,
                        exec_floor,
                        permission_gate,
                        tool_offload,
                        spill_store,
                        live_tool_output.clone(),
                        presentation,
                    )
                    .await
                }
                other => format!(
                    "error: unknown lifecycle action '{other}'. Use 'run' (default) or 'list'."
                ),
            }
        }

        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_read, &full_str) {
                // #263: the gate may grant the read; deny (or no gate) keeps
                // the standard denial text bit-for-bit.
                let allowed = permission_gate.is_some_and(|gate| {
                    fs_gate_allows(gate, "read_file", DenialKind::FsRead, &full_str, |c| {
                        &c.fs_read
                    })
                });
                if !allowed {
                    return denied_fs_result("fs_read", path);
                }
            }
            // #1176: shadow-OCAP — under --full-access the fs fence is top(), so
            // this read runs unconfined; record the path a leash would have
            // gated on (no-op unless recording is armed). `newt ocap propose`
            // folds it into reviewable fs candidates.
            if full_access_requested() {
                crate::flight_recorder::log_observed(
                    crate::flight_recorder::ShadowAxis::FsRead,
                    &full_str,
                    "read_file",
                );
            }
            match std::fs::read_to_string(&full) {
                Ok(contents) => {
                    // #719: window + cap the MODEL-facing payload (the on-screen
                    // display is capped separately) so one read of a large file
                    // can't saturate the context window and abandon the task.
                    let offset = args["offset"].as_u64().map(|n| n as usize);
                    let limit = args["limit"].as_u64().map(|n| n as usize);
                    // #726: char backstop now derives from the shared token
                    // budget so read_file and run_command share one cap.
                    paginate_read(&contents, offset, limit, max_output_tokens())
                }
                Err(e) => format!("error reading {path}: {e}"),
            }
        }

        "write_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_write, &full_str) {
                // #263: the gate may grant the write (the human's choice at
                // the prompt is the consent — the y/N confirm below stays
                // governed by the original scope shape, which a denial here
                // proves is `Only`, i.e. no second confirm).
                let allowed = permission_gate.as_deref_mut().is_some_and(|gate| {
                    fs_gate_allows(gate, "write_file", DenialKind::FsWrite, &full_str, |c| {
                        &c.fs_write
                    })
                });
                if !allowed {
                    return denied_fs_result("fs_write", path);
                }
            }
            // #1176: shadow-OCAP — under --full-access the fs fence is top(), so
            // this write runs unconfined; record the path (write=true) a leash
            // would have gated on (no-op unless recording is armed).
            if full_access_requested() {
                crate::flight_recorder::log_observed(
                    crate::flight_recorder::ShadowAxis::FsWrite,
                    &full_str,
                    "write_file",
                );
            }
            // Capture provenance only after fs_write authorization. The
            // preimage digest is withheld unless this turn also has fs_read;
            // a write-only grant must not mint a persistent equality oracle.
            let artifact_tracking = artifact_sink.is_some() && artifact_context.is_some();
            let artifact_path_within = artifact_tracking
                && artifact_path_is_physically_within_workspace(
                    std::path::Path::new(workspace),
                    &full,
                );
            let artifact_before = artifact_path_within.then(|| {
                artifact_preimage_state(&full, tui_permits_path(&caveats.fs_read, &full_str))
            });

            // Shrink guard: refuse if the proposed write removes > 30% of
            // lines AND > 30 lines absolute. This catches the failure mode
            // where a model replaces an entire large file with a small
            // fragment (observed in the wild: 4,247 → 107 lines).
            if let Ok(existing) = std::fs::read_to_string(&full) {
                let orig_lines = existing.lines().count();
                let new_lines = content.lines().count();
                let removed = orig_lines.saturating_sub(new_lines);
                if removed > 30 && new_lines < orig_lines * 7 / 10 {
                    let pct = removed * 100 / orig_lines.max(1);
                    let msg = format!(
                        "error: write_file would shrink {path} from {orig_lines} → {new_lines} lines \
                         (-{pct}%). This is likely unintentional. Use edit_file to make targeted \
                         changes, or ensure your content includes the full file."
                    );
                    return msg;
                }
            }

            // Show first 20 lines as preview.
            let preview: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
            let has_more = content.lines().count() > 20;
            presentation.preview(
                &format!("{preview}{}", if has_more { "\n…" } else { "" }),
                tool_output_lines,
            );

            // Auto-write when the caveat explicitly scopes fs_write (the
            // preset itself is the user's consent). Unrestricted writes still
            // confirm, but through the TUI gate so stdin is guarded out of
            // cbreak/nonblocking mode. --yolo is the explicit auto-accept mode.
            let confirmed = confirm_unrestricted_fs_mutation(
                caveats,
                &mut permission_gate,
                "Write this file? [y/N]",
            );

            if confirmed {
                let full = std::path::Path::new(workspace).join(path);
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&full, content) {
                    Ok(_) => {
                        let line_count = content.lines().count();
                        // Verify exactly the bytes this governed tool submitted
                        // before an arbitrary build-check command can touch the
                        // workspace. A mismatch emits no false provenance.
                        let artifact = if !artifact_tracking {
                            String::new()
                        } else if !artifact_path_within
                            || !artifact_path_is_physically_within_workspace(
                                std::path::Path::new(workspace),
                                &full,
                            )
                        {
                            artifact_postcondition_warning(
                                path,
                                "the physical path could not be proven inside the workspace",
                                color,
                                tool_output_lines,
                            )
                        } else {
                            match artifact_file_matches(&full, content.as_bytes()) {
                                Ok(true) => record_governed_file_change(
                                    artifact_sink,
                                    artifact_context,
                                    path,
                                    "write_file",
                                    artifact_before,
                                    super::artifact_hooks::ArtifactFileState::from_bytes(
                                        content.as_bytes(),
                                    ),
                                    color,
                                    tool_output_lines,
                                ),
                                Ok(false) => artifact_postcondition_warning(
                                    path,
                                    "post-write bytes did not match the submitted content",
                                    color,
                                    tool_output_lines,
                                ),
                                Err(_) => artifact_postcondition_warning(
                                    path,
                                    "post-write bytes could not be verified",
                                    color,
                                    tool_output_lines,
                                ),
                            }
                        };
                        let check = build_check_cmd
                            .map(|cmd| run_build_check(cmd, workspace))
                            .unwrap_or_default();
                        format!("wrote {path} ({line_count} lines){artifact}{check}")
                    }
                    Err(e) => format!("error writing {path}: {e}"),
                }
            } else {
                format!("user declined to write {path}")
            }
        }

        "delete_file" => {
            let path = args["path"].as_str().unwrap_or("");
            if path.trim().is_empty() {
                return "error: path is required".to_string();
            }
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_write, &full_str) {
                // #1022: deletion is a normal fs_write operation. A denial
                // consults the same prompted-grant path as write_file/edit_file,
                // so deletion is possible with operator approval instead of
                // being structurally unavailable.
                let allowed = permission_gate.as_deref_mut().is_some_and(|gate| {
                    fs_gate_allows(gate, "delete_file", DenialKind::FsWrite, &full_str, |c| {
                        &c.fs_write
                    })
                });
                if !allowed {
                    return denied_fs_result("fs_write", path);
                }
            }

            let meta = match std::fs::symlink_metadata(&full) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return format!("error deleting {path}: file does not exist");
                }
                Err(e) => return format!("error deleting {path}: {e}"),
            };
            if meta.file_type().is_dir() {
                return format!("error deleting {path}: delete_file refuses directories");
            }
            let artifact_tracking = artifact_sink.is_some() && artifact_context.is_some();
            let artifact_path_within = artifact_tracking
                && artifact_path_is_physically_within_workspace(
                    std::path::Path::new(workspace),
                    &full,
                );
            let artifact_before = artifact_path_within.then(|| {
                artifact_preimage_state(&full, tui_permits_path(&caveats.fs_read, &full_str))
            });

            let confirmed = confirm_unrestricted_fs_mutation(
                caveats,
                &mut permission_gate,
                "Delete this file? [y/N]",
            );

            if !confirmed {
                return format!("user declined to delete {path}");
            }

            match std::fs::remove_file(&full) {
                Ok(_) => {
                    let artifact = if !artifact_tracking {
                        String::new()
                    } else if !artifact_path_within
                        || !artifact_path_is_physically_within_workspace(
                            std::path::Path::new(workspace),
                            &full,
                        )
                    {
                        artifact_postcondition_warning(
                            path,
                            "the physical path could not be proven inside the workspace",
                            color,
                            tool_output_lines,
                        )
                    } else {
                        match std::fs::symlink_metadata(&full) {
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                record_governed_file_change(
                                    artifact_sink,
                                    artifact_context,
                                    path,
                                    "delete_file",
                                    artifact_before,
                                    super::artifact_hooks::ArtifactFileState::absent(),
                                    color,
                                    tool_output_lines,
                                )
                            }
                            _ => artifact_postcondition_warning(
                                path,
                                "the path still existed after delete_file returned success",
                                color,
                                tool_output_lines,
                            ),
                        }
                    };
                    let check = build_check_cmd
                        .map(|cmd| run_build_check(cmd, workspace))
                        .unwrap_or_default();
                    format!("deleted {path}{artifact}{check}")
                }
                Err(e) => format!("error deleting {path}: {e}"),
            }
        }

        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let old_string = args["old_string"].as_str().unwrap_or("");
            let new_string = args["new_string"].as_str().unwrap_or("");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_write, &full_str) {
                // #263: same prompted-grant path as write_file.
                let allowed = permission_gate.is_some_and(|gate| {
                    fs_gate_allows(gate, "edit_file", DenialKind::FsWrite, &full_str, |c| {
                        &c.fs_write
                    })
                });
                if !allowed {
                    return denied_fs_result("fs_write", path);
                }
            }
            // #1176: shadow-OCAP — edit is a write; record under --full-access.
            if full_access_requested() {
                crate::flight_recorder::log_observed(
                    crate::flight_recorder::ShadowAxis::FsWrite,
                    &full_str,
                    "edit_file",
                );
            }
            if old_string.is_empty() {
                return "error: old_string must not be empty — use write_file to create new files"
                    .to_string();
            }
            let existing = match std::fs::read_to_string(&full) {
                Ok(s) => s,
                Err(e) => return format!("error reading {path}: {e}"),
            };
            let count = existing.matches(old_string).count();
            if count == 0 {
                // Show the file's actual head so the model can copy the exact
                // text and self-correct on the next call — instead of guessing
                // old_string blind and looping (the failure mode that left a
                // model unable to add a header comment). The content is already
                // in hand from the read above; no extra round needed.
                const HEAD: usize = 40;
                let total = existing.lines().count();
                let head: String = existing
                    .lines()
                    .take(HEAD)
                    .map(|l| format!("  {l}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let more = if total > HEAD {
                    format!("\n  … ({} more line(s))", total - HEAD)
                } else {
                    String::new()
                };
                return format!(
                    "error: old_string not found in {path} — do not guess again. Copy the \
                     EXACT text (including leading whitespace) from the contents below, then \
                     retry. To add a header/first line, set old_string to the shown first \
                     line and put your header + that line in new_string; to create a new \
                     file use write_file.\n--- {path} (first {shown} of {total} line(s)) ---\n{head}{more}",
                    shown = total.min(HEAD),
                );
            }
            if count > 1 {
                return format!(
                    "error: old_string matches {count} locations in {path}. \
                     Add more surrounding context to make it unique."
                );
            }
            let artifact_tracking = artifact_sink.is_some() && artifact_context.is_some();
            let artifact_path_within = artifact_tracking
                && artifact_path_is_physically_within_workspace(
                    std::path::Path::new(workspace),
                    &full,
                );
            let artifact_before = artifact_path_within.then(|| {
                if !tui_permits_path(&caveats.fs_read, &full_str) {
                    super::artifact_hooks::ArtifactFileState::unavailable("fs_read_not_granted")
                } else if std::fs::symlink_metadata(&full)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    super::artifact_hooks::ArtifactFileState::unavailable(
                        "symlink_preimage_not_hashed",
                    )
                } else {
                    super::artifact_hooks::ArtifactFileState::from_bytes(existing.as_bytes())
                }
            });
            let updated = existing.replacen(old_string, new_string, 1);
            let old_lines = existing.lines().count();
            let new_lines = updated.lines().count();
            let delta = new_lines as i64 - old_lines as i64;
            let delta_str = if delta >= 0 {
                format!("+{delta}")
            } else {
                format!("{delta}")
            };
            match std::fs::write(&full, &updated) {
                Ok(_) => {
                    let artifact = if !artifact_tracking {
                        String::new()
                    } else if !artifact_path_within
                        || !artifact_path_is_physically_within_workspace(
                            std::path::Path::new(workspace),
                            &full,
                        )
                    {
                        artifact_postcondition_warning(
                            path,
                            "the physical path could not be proven inside the workspace",
                            color,
                            tool_output_lines,
                        )
                    } else {
                        match artifact_file_matches(&full, updated.as_bytes()) {
                            Ok(true) => record_governed_file_change(
                                artifact_sink,
                                artifact_context,
                                path,
                                "edit_file",
                                artifact_before,
                                super::artifact_hooks::ArtifactFileState::from_bytes(
                                    updated.as_bytes(),
                                ),
                                color,
                                tool_output_lines,
                            ),
                            Ok(false) => artifact_postcondition_warning(
                                path,
                                "post-edit bytes did not match the computed replacement",
                                color,
                                tool_output_lines,
                            ),
                            Err(_) => artifact_postcondition_warning(
                                path,
                                "post-edit bytes could not be verified",
                                color,
                                tool_output_lines,
                            ),
                        }
                    };
                    let check = build_check_cmd
                        .map(|cmd| run_build_check(cmd, workspace))
                        .unwrap_or_default();
                    format!(
                        "edited {path} ({delta_str} lines, now {new_lines} total){artifact}{check}"
                    )
                }
                Err(e) => format!("error writing {path}: {e}"),
            }
        }

        "list_dir" => {
            let path = args["path"].as_str().unwrap_or(".");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_read, &full_str) {
                // #263: same prompted-grant path as read_file.
                let allowed = permission_gate.is_some_and(|gate| {
                    fs_gate_allows(gate, "list_dir", DenialKind::FsRead, &full_str, |c| {
                        &c.fs_read
                    })
                });
                if !allowed {
                    return denied_fs_result("fs_read", path);
                }
            }
            // #1176: shadow-OCAP — record the listed dir under --full-access.
            if full_access_requested() {
                crate::flight_recorder::log_observed(
                    crate::flight_recorder::ShadowAxis::FsRead,
                    &full_str,
                    "list_dir",
                );
            }
            match std::fs::read_dir(&full) {
                Ok(entries) => {
                    let mut names: Vec<String> = entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    names.join("\n")
                }
                Err(e) => format!("error: {e}"),
            }
        }

        // #496: embedded, shell-free file search. The reported breakage was an
        // agent that needed `find` but the build's shell tool was unavailable;
        // this arm walks the workspace with the `ignore` crate (no subprocess),
        // gated by the same fs_read caveat as list_dir/read_file.
        "find" => {
            let path = args["path"].as_str().unwrap_or(".");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_read, &full_str) {
                let allowed = permission_gate.is_some_and(|gate| {
                    fs_gate_allows(gate, "find", DenialKind::FsRead, &full_str, |c| &c.fs_read)
                });
                if !allowed {
                    return denied_fs_result("fs_read", path);
                }
            }
            // #1176: shadow-OCAP — record the search root under --full-access
            // (the fuzzer's "find the 10 largest files" is a canonical case).
            if full_access_requested() {
                crate::flight_recorder::log_observed(
                    crate::flight_recorder::ShadowAxis::FsRead,
                    &full_str,
                    "find",
                );
            }
            let opts = find_opts_from_args(args);
            if !full.exists() {
                return format!("error: no such path '{path}'");
            }
            // Defence-in-depth for a *recursive* read: refuse a root that
            // canonicalises outside the workspace (e.g. via `..`). `find` never
            // follows symlinks, so descent can't escape either.
            if let (Ok(ws_canon), Ok(root_canon)) = (
                std::path::Path::new(workspace).canonicalize(),
                full.canonicalize(),
            ) {
                if !root_canon.starts_with(&ws_canon) {
                    return denied_fs_result("fs_read", path);
                }
            }
            match find_walk(&full, std::path::Path::new(workspace), &opts) {
                Ok((hits, truncated)) => {
                    let mut listing = if hits.is_empty() {
                        "no matches".to_string()
                    } else {
                        hits.join("\n")
                    };
                    if truncated {
                        listing
                            .push_str(&format!("\n… (truncated at {} matches)", opts.max_results));
                    }
                    listing
                }
                Err(e) => format!("error: {e}"),
            }
        }

        "use_skill" => {
            let skill_name = args["name"].as_str().unwrap_or("");
            // Reads from the configured skill search path. This is a read of
            // trusted operator config (procedural knowledge), not an exec of
            // arbitrary code, so it is NOT leash-gated — any SCRIPTS the skill
            // bundles still run through `run_command`'s confined shell and are
            // governed by the session caveats. The same first-directory-wins
            // precedence as the index means we load the copy the model was
            // actually shown.
            let dirs = crate::Config::resolve()
                .map(|c| c.skill_search_dirs())
                .unwrap_or_default();
            match newt_skills::load_body_from(&dirs, skill_name) {
                Ok(body) => body,
                Err(e) => format!("error: {e}"),
            }
        }

        "web_fetch" => {
            let url = args["url"].as_str().unwrap_or("");

            // Route through agent-bridle's `web_fetch` tool under the SAME
            // Caveats. The `net` axis gates which hosts are reachable (host
            // allowlist + SSRF screen); an out-of-scope host is denied by the
            // leash, surfaced via the dispatch error. The tool returns extracted
            // markdown (`{ url, final_url, status, title, markdown }`) — the body
            // is untrusted page content, not a command result.
            let mut fetch_args = serde_json::json!({ "url": url });
            if let Some(max_bytes) = args.get("max_bytes").and_then(serde_json::Value::as_u64) {
                fetch_args["max_bytes"] = serde_json::json!(max_bytes);
            }
            // #263: with a gate present, pre-check the host against the `net`
            // axis so an out-of-allowlist host becomes a prompt instead of a
            // leash error. Allow ⇒ dispatch under the gate's minted caveats;
            // deny (or no gate, or an unparseable URL) ⇒ dispatch under the
            // ORIGINAL caveats — the leash produces today's denial verbatim.
            let widened_for_net = match (permission_gate, host_of_url(url)) {
                (Some(gate), Some(host)) if !caveats.permits_net(&host) => {
                    let request = PermissionRequest {
                        tool: "web_fetch".to_string(),
                        kind: DenialKind::Net,
                        target: host.clone(),
                        reason: format!("net does not permit '{host}'"),
                    };
                    match gate.ask(std::slice::from_ref(&request)) {
                        PermissionDecision::Allow(widened) => Some(widened),
                        PermissionDecision::Deny => None,
                    }
                }
                _ => None,
            };
            let effective_caveats = widened_for_net.as_ref().unwrap_or(caveats);
            // #1176: shadow-OCAP — under --full-access the net leash is top(),
            // so this fetch runs unconfined; record the host a leash would have
            // gated on (no-op unless recording is armed).
            if full_access_requested() {
                if let Some(host) = host_of_url(url) {
                    crate::flight_recorder::log_observed(
                        crate::flight_recorder::ShadowAxis::Net,
                        &host,
                        url,
                    );
                }
            }
            match agent_bridle::registry()
                .dispatch("web_fetch", fetch_args, effective_caveats)
                .await
            {
                Ok(result) => {
                    let markdown = result
                        .get("markdown")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let title = result
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let final_url = result
                        .get("final_url")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(url);
                    let out = if title.is_empty() {
                        format!("{final_url}\n\n{markdown}")
                    } else {
                        format!("# {title}\n{final_url}\n\n{markdown}")
                    };
                    out
                }
                // A `net`-axis leash denial, or a fetch error (SSRF screen,
                // timeout, non-2xx) — surface the reason; Display is safe.
                Err(e) => format!("error: {e}"),
            }
        }

        other => unknown_tool_message(other),
    }
}

/// Classify an [`execute_tool`] result string as success or failure for the
/// turn's recorded tool events (Step 17.6, #246). Best-effort by necessity —
/// tool results are plain strings fed back to the model — so this mirrors
/// the failure prefixes this module (and `McpTools::call`) actually emit:
/// `error:`, `capability denied:`, and `unknown tool`. A successful
/// `run_command` whose *output* happens to start with one of these is
/// misclassified; the recorded event is an outcome claim, not a gate.
pub(crate) fn tool_result_ok(result: &str) -> bool {
    let r = result.trim_start();
    !(r.starts_with("error:")
        || r.starts_with("capability denied:")
        || r.starts_with("unknown tool"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::NoMcp;

    // ── #1258: the embedded `find` size column (pure, fs-free) ──────────────

    /// A `FindOpts` for the finalize/parse tests: defaults except the fields a
    /// test overrides.
    fn find_opts(max_results: usize, show_size: bool, sort: FindSort) -> FindOpts<'static> {
        FindOpts {
            name: None,
            type_filter: FindType::Any,
            max_depth: None,
            max_results,
            respect_gitignore: true,
            case_sensitive: true,
            show_size,
            sort,
        }
    }

    #[test]
    fn find_opts_parses_size_column_options() {
        let sized = serde_json::json!({ "show_size": true, "sort": "size" });
        let opts = find_opts_from_args(&sized);
        assert!(opts.show_size);
        assert_eq!(opts.sort, FindSort::Size);
        // Defaults: no size column, name order.
        let empty = serde_json::json!({});
        let d = find_opts_from_args(&empty);
        assert!(!d.show_size);
        assert_eq!(d.sort, FindSort::Name);
        // An unknown sort value falls back to name (never errors).
        let bogus = serde_json::json!({ "sort": "bogus" });
        let bad = find_opts_from_args(&bogus);
        assert_eq!(bad.sort, FindSort::Name);
    }

    #[test]
    fn finalize_find_name_sort_is_paths_ascending() {
        let entries = vec![
            (10, "src/b.rs".to_string()),
            (99, "src/a.rs".to_string()),
            (1, "src/c.rs".to_string()),
        ];
        let (lines, truncated) = finalize_find(entries, &find_opts(1000, false, FindSort::Name));
        assert_eq!(lines, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert!(!truncated, "under the cap");
    }

    #[test]
    fn finalize_find_size_sort_is_bytes_descending_with_show_size() {
        let entries = vec![
            (10, "small.rs".to_string()),
            (900, "big.rs".to_string()),
            (50, "mid.rs".to_string()),
        ];
        let (lines, _) = finalize_find(entries, &find_opts(1000, true, FindSort::Size));
        assert_eq!(
            lines,
            vec!["900\tbig.rs", "50\tmid.rs", "10\tsmall.rs"],
            "byte size descending, each line prefixed '<size>\\t<path>'"
        );
    }

    #[test]
    fn finalize_find_size_ties_break_by_path_for_determinism() {
        let entries = vec![(42, "z.rs".to_string()), (42, "a.rs".to_string())];
        let (lines, _) = finalize_find(entries, &find_opts(1000, false, FindSort::Size));
        assert_eq!(lines, vec!["a.rs", "z.rs"], "equal sizes → path ascending");
    }

    #[test]
    fn finalize_find_size_sort_truncates_to_true_top_n() {
        // The N largest, not the first-N-walked: order THEN truncate.
        let entries = vec![
            (1, "a".to_string()),
            (100, "b".to_string()),
            (50, "c".to_string()),
            (200, "d".to_string()),
        ];
        let (lines, truncated) = finalize_find(entries, &find_opts(2, true, FindSort::Size));
        assert_eq!(lines, vec!["200\td", "100\tb"]);
        assert!(truncated, "two matches dropped past the cap");
    }

    #[test]
    fn finalize_find_dedups_by_path() {
        let entries = vec![
            (10, "dup.rs".to_string()),
            (10, "dup.rs".to_string()),
            (20, "other.rs".to_string()),
        ];
        let (lines, _) = finalize_find(entries, &find_opts(1000, false, FindSort::Name));
        assert_eq!(lines, vec!["dup.rs", "other.rs"]);
    }

    #[derive(Default)]
    struct RecordingLiveOutput {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl crate::agentic::LiveToolOutput for RecordingLiveOutput {
        fn start(&self, _generation: u64) {
            self.events.lock().unwrap().push("start".into());
        }

        fn write(&self, _generation: u64, stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
            self.events
                .lock()
                .unwrap()
                .push(format!("{stream:?}:{}", String::from_utf8_lossy(chunk)));
        }

        fn finish(&self, _generation: u64) {
            self.events.lock().unwrap().push("finish".into());
        }

        fn abandon(&self, _generation: u64) {
            self.events.lock().unwrap().push("abandon".into());
        }
    }

    #[test]
    fn live_output_session_closes_before_late_chunks() {
        let sink = std::sync::Arc::new(RecordingLiveOutput::default());
        let mut session = LiveOutputSession::start(Some(sink.clone())).expect("live session");
        let relay = session.relay();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"now");
        session.finish();
        relay.write(crate::agentic::ToolOutputStream::Stderr, b"late");

        assert_eq!(
            *sink.events.lock().unwrap(),
            ["start", "Stdout:now", "finish"]
        );
    }

    #[test]
    fn live_output_slow_start_does_not_block_execution_and_is_abandoned_on_drop() {
        struct BlockingStart {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            abandoned: std::sync::mpsc::Sender<()>,
            writes: std::sync::atomic::AtomicUsize,
            finishes: std::sync::atomic::AtomicUsize,
        }
        impl crate::agentic::LiveToolOutput for BlockingStart {
            fn start(&self, _generation: u64) {
                let _ = self.entered.send(());
                let _ = self.release.lock().unwrap().recv();
            }
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                self.writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            fn finish(&self, _generation: u64) {
                self.finishes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.abandoned.send(());
            }
        }

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(BlockingStart {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
            abandoned: abandoned_tx,
            writes: std::sync::atomic::AtomicUsize::new(0),
            finishes: std::sync::atomic::AtomicUsize::new(0),
        });

        let (created_tx, created_rx) = std::sync::mpsc::channel();
        let creator_sink = sink.clone();
        let creator = std::thread::spawn(move || {
            let session = LiveOutputSession::start(Some(creator_sink)).expect("live session");
            let _ = created_tx.send(session);
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("presentation worker entered start");
        let session = match created_rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(session) => session,
            Err(error) => {
                let _ = release_tx.send(());
                creator.join().unwrap();
                panic!("arbitrary sink startup blocked tool execution: {error}");
            }
        };
        creator.join().unwrap();
        let relay = session.relay();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"queued");
        drop(session);
        abandoned_rx
            .try_recv()
            .expect("drop invalidated the blocked startup synchronously");

        release_tx.send(()).unwrap();
        assert!(
            relay.wait_finished(std::time::Duration::from_secs(1)),
            "worker did not close after blocked startup returned"
        );
        assert_eq!(sink.writes.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(
            sink.finishes.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "delayed startup queued a late erase"
        );
    }

    #[test]
    fn live_output_start_panic_is_contained_and_abandoned() {
        struct PanickingStart {
            abandoned: std::sync::mpsc::Sender<()>,
        }
        impl crate::agentic::LiveToolOutput for PanickingStart {
            fn start(&self, _generation: u64) {
                panic!("startup failed");
            }
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                panic!("failed startup must not receive writes");
            }
            fn finish(&self, _generation: u64) {
                panic!("failed startup must not finish");
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.abandoned.send(());
            }
        }

        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let mut session = LiveOutputSession::start(Some(std::sync::Arc::new(PanickingStart {
            abandoned: abandoned_tx,
        })))
        .expect("worker creation succeeds independently of sink startup");
        session
            .relay()
            .write(crate::agentic::ToolOutputStream::Stdout, b"ignored");
        session.finish();

        abandoned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("startup panic invalidated its generation");
    }

    #[test]
    fn live_output_finish_does_not_wait_for_an_inflight_write() {
        struct BlockingOutput {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            finished: std::sync::mpsc::Sender<()>,
            abandoned: std::sync::mpsc::Sender<()>,
        }
        impl crate::agentic::LiveToolOutput for BlockingOutput {
            fn start(&self, _generation: u64) {}
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                let _ = self.entered.send(());
                let _ = self.release.lock().unwrap().recv();
            }
            fn finish(&self, _generation: u64) {
                let _ = self.finished.send(());
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.abandoned.send(());
            }
        }

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(BlockingOutput {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
            finished: finished_tx,
            abandoned: abandoned_tx,
        });
        let mut session = LiveOutputSession::start(Some(sink)).unwrap();
        let relay = session.relay();
        let writer = std::thread::spawn(move || {
            relay.write(crate::agentic::ToolOutputStream::Stdout, b"held");
        });
        entered_rx.recv().unwrap();
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let finisher = std::thread::spawn(move || {
            session.finish();
            let _ = returned_tx.send(());
        });

        returned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("session finish must not wait forever on an arbitrary observer callback");
        abandoned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("timed-out teardown invalidates the generation before returning");
        assert!(
            finished_rx.try_recv().is_err(),
            "timed-out teardown must not queue a late terminal erase"
        );
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        finisher.join().unwrap();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "worker erased the generation after canonical rendering could resume"
        );
    }

    #[test]
    fn live_output_timeout_invalidates_an_inflight_finish_before_returning() {
        struct SlowFinish {
            finish_entered: std::sync::mpsc::Sender<()>,
            release_finish: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            generation_valid: std::sync::atomic::AtomicBool,
            erased: std::sync::atomic::AtomicBool,
        }
        impl crate::agentic::LiveToolOutput for SlowFinish {
            fn start(&self, _generation: u64) {
                self.generation_valid
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
            }
            fn finish(&self, _generation: u64) {
                let _ = self.finish_entered.send(());
                let _ = self.release_finish.lock().unwrap().recv();
                if self
                    .generation_valid
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    self.erased
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            }
            fn abandon(&self, _generation: u64) {
                self.generation_valid
                    .store(false, std::sync::atomic::Ordering::Release);
            }
        }

        let (finish_entered_tx, finish_entered_rx) = std::sync::mpsc::channel();
        let (release_finish_tx, release_finish_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(SlowFinish {
            finish_entered: finish_entered_tx,
            release_finish: std::sync::Mutex::new(release_finish_rx),
            generation_valid: std::sync::atomic::AtomicBool::new(false),
            erased: std::sync::atomic::AtomicBool::new(false),
        });
        let mut session = LiveOutputSession::start(Some(sink.clone())).unwrap();
        let relay = session.relay();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"paint");
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let finisher = std::thread::spawn(move || {
            session.finish();
            let _ = returned_tx.send(());
        });

        finish_entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker entered finish");
        returned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("bounded teardown returned after invalidating the generation");
        assert!(
            !sink
                .generation_valid
                .load(std::sync::atomic::Ordering::Acquire),
            "canonical rendering resumed before generation invalidation"
        );

        release_finish_tx.send(()).unwrap();
        assert!(relay.wait_finished(std::time::Duration::from_secs(1)));
        finisher.join().unwrap();
        assert!(
            !sink.erased.load(std::sync::atomic::Ordering::Acquire),
            "in-flight finish erased terminal output after canonical rendering resumed"
        );
    }

    #[test]
    fn live_output_cancel_stops_finishing_queue_drain() {
        struct GatedOutput {
            writes: std::sync::Mutex<Vec<Vec<u8>>>,
            first_entered: std::sync::mpsc::Sender<()>,
            release_first: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            held_entered: std::sync::mpsc::Sender<()>,
            release_held: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            finished: std::sync::mpsc::Sender<()>,
        }
        impl crate::agentic::LiveToolOutput for GatedOutput {
            fn start(&self, _generation: u64) {}

            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                chunk: &[u8],
            ) {
                self.writes.lock().unwrap().push(chunk.to_vec());
                match chunk {
                    b"first" => {
                        let _ = self.first_entered.send(());
                        let _ = self.release_first.lock().unwrap().recv();
                    }
                    b"held" => {
                        let _ = self.held_entered.send(());
                        let _ = self.release_held.lock().unwrap().recv();
                    }
                    _ => {}
                }
            }

            fn finish(&self, _generation: u64) {
                let _ = self.finished.send(());
            }

            fn abandon(&self, _generation: u64) {}
        }

        let (first_entered_tx, first_entered_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let (held_entered_tx, held_entered_rx) = std::sync::mpsc::channel();
        let (release_held_tx, release_held_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(GatedOutput {
            writes: std::sync::Mutex::new(Vec::new()),
            first_entered: first_entered_tx,
            release_first: std::sync::Mutex::new(release_first_rx),
            held_entered: held_entered_tx,
            release_held: std::sync::Mutex::new(release_held_rx),
            finished: finished_tx,
        });
        let mut session = LiveOutputSession::start(Some(sink.clone())).unwrap();
        let relay = session.relay();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"first");
        first_entered_rx.recv().unwrap();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"held");
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"stale");

        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let finisher = std::thread::spawn(move || {
            session.finish();
            let _ = returned_tx.send(());
        });
        while relay.phase.load(std::sync::atomic::Ordering::Acquire) != LIVE_OUTPUT_FINISHING {
            std::thread::yield_now();
        }
        release_first_tx.send(()).unwrap();
        held_entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("finishing worker drained the next queued write");
        returned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("finish cancels after its bounded wait");
        release_held_tx.send(()).unwrap();
        finisher.join().unwrap();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "cancelled queue drain must not finish after the bounded handoff"
        );

        assert_eq!(
            *sink.writes.lock().unwrap(),
            vec![b"first".to_vec(), b"held".to_vec()]
        );
    }

    #[test]
    fn live_output_write_panic_is_contained_and_abandons_the_generation() {
        struct PanickingOutput(std::sync::mpsc::Sender<()>);
        impl crate::agentic::LiveToolOutput for PanickingOutput {
            fn start(&self, _generation: u64) {}
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                panic!("presentation failed");
            }
            fn finish(&self, _generation: u64) {
                panic!("cancelled generation must not finish");
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.0.send(());
            }
        }

        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let mut session =
            LiveOutputSession::start(Some(std::sync::Arc::new(PanickingOutput(abandoned_tx))))
                .unwrap();
        session
            .relay()
            .write(crate::agentic::ToolOutputStream::Stdout, b"panic");
        session.finish();

        abandoned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("panic teardown abandoned the live generation");
    }

    #[test]
    fn dropping_live_output_session_finishes_before_returning() {
        struct FinishSignal {
            finished: std::sync::mpsc::Sender<()>,
            abandoned: std::sync::mpsc::Sender<()>,
        }
        impl crate::agentic::LiveToolOutput for FinishSignal {
            fn start(&self, _generation: u64) {}
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
            }
            fn finish(&self, _generation: u64) {
                let _ = self.finished.send(());
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.abandoned.send(());
            }
        }

        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let session = LiveOutputSession::start(Some(std::sync::Arc::new(FinishSignal {
            finished: finished_tx,
            abandoned: abandoned_tx,
        })))
        .unwrap();
        drop(session);

        finished_rx
            .try_recv()
            .expect("drop closed the live frame synchronously");
        assert!(
            abandoned_rx.try_recv().is_err(),
            "a responsive sink should finish rather than be abandoned"
        );
    }

    #[cfg(not(windows))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_live_sink_cannot_delay_host_timeout() {
        struct BlockingOutput {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl crate::agentic::LiveToolOutput for BlockingOutput {
            fn start(&self, _generation: u64) {}
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                let _ = self.entered.send(());
                let _ = self.release.lock().unwrap().recv();
            }
            fn finish(&self, _generation: u64) {}
            fn abandon(&self, _generation: u64) {}
        }

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(BlockingOutput {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
        });
        let mut session = LiveOutputSession::start(Some(sink)).unwrap();
        let relay = session.relay();
        let run = tokio::spawn(async move {
            host_shell_output_with_timeout(
                "printf ready; sleep 5",
                ".",
                Some(relay),
                std::time::Duration::from_millis(100),
            )
            .await
        });
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(std::time::Duration::from_secs(1))
        })
        .await
        .unwrap()
        .expect("renderer entered its blocking write");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), run).await;
        if outcome.is_err() {
            let _ = release_tx.send(());
            panic!("blocked presentation defeated the host timeout");
        }
        let run = outcome.unwrap().unwrap().unwrap();
        assert!(run.timed_out);
        assert_eq!(run.exit_code, 124);

        session.cancel();
        release_tx.send(()).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_live_sink_cannot_backpressure_host_pipe_capture() {
        struct BlockingOutput {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl crate::agentic::LiveToolOutput for BlockingOutput {
            fn start(&self, _generation: u64) {}
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                let _ = self.entered.send(());
                let _ = self.release.lock().unwrap().recv();
            }
            fn finish(&self, _generation: u64) {}
            fn abandon(&self, _generation: u64) {}
        }

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(BlockingOutput {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
        });
        let mut session = LiveOutputSession::start(Some(sink)).unwrap();
        let relay = session.relay();
        let run = tokio::spawn(async move {
            host_shell_output_with_timeout(
                "head -c 262144 /dev/zero",
                ".",
                Some(relay),
                std::time::Duration::from_secs(5),
            )
            .await
        });
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(std::time::Duration::from_secs(1))
        })
        .await
        .unwrap()
        .expect("renderer entered its blocking write");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
        if outcome.is_err() {
            let _ = release_tx.send(());
            panic!("blocked presentation backpressured host pipe capture");
        }
        let run = outcome.unwrap().unwrap().unwrap();
        assert!(!run.timed_out);
        assert_eq!(run.exit_code, 0);
        assert_eq!(run.stdout.len(), 262_144);

        session.cancel();
        release_tx.send(()).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test(flavor = "multi_thread")]
    async fn host_bypass_publishes_output_before_command_completion() {
        struct ChannelOutput(std::sync::mpsc::Sender<Vec<u8>>);
        impl crate::agentic::LiveToolOutput for ChannelOutput {
            fn start(&self, _generation: u64) {}
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                chunk: &[u8],
            ) {
                let _ = self.0.send(chunk.to_vec());
            }
            fn finish(&self, _generation: u64) {}
            fn abandon(&self, _generation: u64) {}
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(ChannelOutput(tx));
        let session = LiveOutputSession::start(Some(sink)).unwrap();
        let relay = session.relay();
        let handle = tokio::spawn(async move {
            host_shell_output("printf ready; sleep 0.2; printf done", ".", Some(relay)).await
        });

        let first =
            tokio::task::spawn_blocking(move || rx.recv_timeout(std::time::Duration::from_secs(2)))
                .await
                .unwrap()
                .expect("first live host-shell chunk");
        assert_eq!(first, b"ready");
        assert!(
            !handle.is_finished(),
            "command completed before its live chunk"
        );

        let run = handle.await.unwrap().unwrap();
        assert_eq!(run.stdout, b"readydone");
        assert_eq!(run.exit_code, 0);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn bridled_shell_forwards_live_bytes_without_changing_the_envelope() {
        let sink = std::sync::Arc::new(RecordingLiveOutput::default());
        let caveats = crate::caveats::Caveats {
            exec: crate::caveats::Scope::only(["echo".to_string()]),
            ..crate::caveats::Caveats::top()
        };

        let envelope = dispatch_bridled_shell(
            serde_json::json!({"cmd": "echo observed", "cwd": "."}),
            &caveats,
            Some(sink.clone()),
        )
        .await
        .expect("confined echo dispatch");

        assert_eq!(envelope["stdout"], "observed\n");
        assert_eq!(
            *sink.events.lock().unwrap(),
            ["start", "Stdout:observed\n", "finish"]
        );
    }

    // ---- #717: classify_phantom_reach (pure, no fs) ----

    #[test]
    fn classify_phantom_rewrite_alias() {
        // A shell alias resolves to the canonical run_command rewrite.
        let got = classify_phantom_reach("bash", &serde_json::json!({"command": "ls"}), "ok", true);
        assert_eq!(
            got,
            Some(crate::PhantomResolution::Rewrite("run_command".into()))
        );
    }

    #[test]
    fn classify_phantom_correct_alias() {
        // An edit alias with the wrong arg shape returns Correct guidance.
        let got = classify_phantom_reach(
            "str_replace_editor",
            &serde_json::json!({}),
            "ignored",
            false,
        );
        match got {
            Some(crate::PhantomResolution::Correct(msg)) => {
                assert!(msg.contains("edit_file"), "guidance names the tool: {msg}");
            }
            other => panic!("expected Correct, got {other:?}"),
        }
    }

    #[test]
    fn classify_phantom_unknown_name() {
        // A foreign name with no alias is a true phantom tool. (Note: #716 turned
        // the plan/crew/workflow notions into recognized aliases, so this uses a
        // name no family claims.)
        let got = classify_phantom_reach(
            "summon_kraken",
            &serde_json::json!({}),
            "unknown tool: summon_kraken",
            false,
        );
        assert_eq!(got, Some(crate::PhantomResolution::Unknown));
    }

    #[test]
    fn classify_phantom_plan_alias_is_correct() {
        // #716 + #717: a foreign plan notion now resolves through the alias seam,
        // so the telemetry classifier records it as a Correct (coach) reach — the
        // new arms get phantom-reach telemetry for free.
        let got = classify_phantom_reach("make_plan", &serde_json::json!({}), "ignored", false);
        match got {
            Some(crate::PhantomResolution::Correct(msg)) => {
                assert!(
                    msg.contains("update_plan"),
                    "guidance names the tool: {msg}"
                );
            }
            other => panic!("expected Correct, got {other:?}"),
        }
    }

    #[test]
    fn classify_phantom_state_get_miss() {
        // state_get on an unset key is an empty-by-design real-tool miss.
        let got = classify_phantom_reach(
            "state_get",
            &serde_json::json!({"key": "nope"}),
            "no such key: nope",
            true,
        );
        assert_eq!(
            got,
            Some(crate::PhantomResolution::RealToolMiss(
                "state_get on an unset key".into()
            ))
        );
    }

    #[test]
    fn classify_phantom_recall_miss() {
        // recall with no hits is an empty-by-design real-tool miss.
        let got = classify_phantom_reach(
            "recall",
            &serde_json::json!({"query": "zzz"}),
            "no matches in past conversations for \"zzz\" — try different keywords",
            true,
        );
        assert_eq!(
            got,
            Some(crate::PhantomResolution::RealToolMiss(
                "recall returned no matches".into()
            ))
        );
    }

    #[test]
    fn classify_phantom_resume_reach_is_a_rewrite() {
        // #714 + #717: a "where were we" reach resolves through the alias seam to
        // a Rewrite, so the telemetry already captures it (no new wiring needed).
        let got = classify_phantom_reach("where_were_we", &serde_json::json!({}), "ignored", false);
        assert_eq!(
            got,
            Some(crate::PhantomResolution::Rewrite("resume_context".into()))
        );
    }

    #[test]
    fn classify_phantom_real_success_is_none() {
        // An ordinary successful real tool call is not phantom telemetry.
        let got = classify_phantom_reach(
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            "line 1\nline 2\n",
            true,
        );
        assert_eq!(got, None);
    }

    // ---- #725: tool_search discovery (alias + name registry) ----

    #[test]
    fn tool_search_is_a_real_tool_name() {
        // It must be in the canonical registry so a model calling it is never
        // treated as a hallucination.
        assert!(ALL_TOOL_NAMES.contains(&"tool_search"));
    }

    #[test]
    fn discovery_verbs_alias_to_tool_search() {
        // The instinctive "which tool does X?" reaches silently Rewrite to the
        // real tool_search.
        for verb in [
            "find_tool",
            "search_tools",
            "list_tools",
            "which_tool",
            "available_tools",
            "what_tools",
            "tools",
        ] {
            match resolve_tool_alias(verb) {
                Some(AliasOutcome::Rewrite(c)) => assert_eq!(c, "tool_search", "verb: {verb}"),
                other => panic!(
                    "expected Rewrite(tool_search) for {verb}, got something else: {}",
                    other.is_some()
                ),
            }
        }
    }

    #[test]
    fn tool_search_is_not_an_alias_of_itself() {
        // The real name must fall through unchanged (no recursive rewrite).
        assert!(resolve_tool_alias("tool_search").is_none());
    }

    #[test]
    fn classify_phantom_discovery_reach_is_a_rewrite() {
        // #725 + #717: a discovery reach resolves through the alias seam to a
        // Rewrite, so the phantom telemetry captures it for free.
        let got = classify_phantom_reach("find_tool", &serde_json::json!({}), "ignored", false);
        assert_eq!(
            got,
            Some(crate::PhantomResolution::Rewrite("tool_search".into()))
        );
    }

    #[test]
    fn classify_phantom_tool_search_real_call_is_none() {
        // A real tool_search call is not phantom telemetry.
        let got = classify_phantom_reach(
            "tool_search",
            &serde_json::json!({"query": "read"}),
            "Tools matching \"read\":\n- read_file — Read a file",
            true,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn tool_search_is_not_a_hallucination() {
        assert!(!is_hallucination(
            "tool_search",
            &serde_json::json!({"query": "x"})
        ));
    }

    // ---- #719: read_file payload window/cap/pagination (pure, no fs) ----

    #[test]
    fn paginate_read_caps_a_large_file_to_the_default_window() {
        // A 15k-line file must NOT flood the model: default window is 2000 lines
        // with a footer to continue (regression for the 12.5k→168k saturation).
        let body: String = (1..=15_057)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = paginate_read(&body, None, None, DEFAULT_MAX_OUTPUT_TOKENS);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "line 1");
        assert_eq!(lines[1999], "line 2000");
        assert!(
            !out.contains("line 2001"),
            "window stops at 2000: {:?}",
            &out[..40]
        );
        assert!(out.contains("of 15057"), "footer names the total");
        assert!(
            out.contains("offset=2001"),
            "footer points at the next window"
        );
    }

    #[test]
    fn paginate_read_offset_and_limit_return_just_that_window() {
        let body: String = (1..=100)
            .map(|n| format!("L{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = paginate_read(&body, Some(10), Some(5), DEFAULT_MAX_OUTPUT_TOKENS);
        assert!(out.starts_with("L10\nL11\nL12\nL13\nL14"), "{out:?}");
        assert!(out.contains("offset=15"), "continues at line 15: {out:?}");
    }

    #[test]
    fn paginate_read_small_file_is_returned_verbatim_without_a_footer() {
        // Whole-file read that fits both caps → exact bytes, no footer.
        assert_eq!(
            paginate_read("a\nb\nc\n", None, None, DEFAULT_MAX_OUTPUT_TOKENS),
            "a\nb\nc\n"
        );
    }

    #[test]
    fn paginate_read_char_backstop_tracks_the_token_budget() {
        // #726: the char backstop is now token-derived (budget × chars/token),
        // NOT a hardcoded 100k. One enormous line: the line window can't help;
        // the token-derived char backstop must. With a 1000-token budget the
        // backstop is ~4000 chars, so a 50k-char line is truncated near there.
        let budget = 1_000;
        let max_chars = crate::tokens::TokenEstimation::default().chars_for_tokens(budget);
        let body = "x".repeat(50_000);
        let out = paginate_read(&body, None, None, budget);
        assert!(
            out.len() < max_chars + 300,
            "char-capped to the token budget (~{max_chars} chars): {} bytes",
            out.len()
        );
        assert!(out.contains("truncated"), "marks the truncation");
        assert!(
            out.contains("~1000 tokens"),
            "footer names the token budget: {out:?}"
        );

        // A LARGER budget keeps more of the same line — the backstop tracks the
        // budget rather than a fixed constant.
        let wide = paginate_read(&body, None, None, 4_000);
        assert!(
            wide.len() > out.len(),
            "a wider token budget keeps more chars: {} vs {}",
            wide.len(),
            out.len()
        );
    }

    #[test]
    fn paginate_read_zero_budget_disables_the_char_backstop() {
        // #726: max_output_tokens == 0 means "no cap" — only the line window
        // applies, so a single huge line comes back verbatim.
        let body = "y".repeat(500_000);
        let out = paginate_read(&body, None, None, 0);
        assert_eq!(out, body, "zero budget = no char backstop");
    }

    #[test]
    fn paginate_read_offset_past_end_is_a_clear_message() {
        let out = paginate_read("a\nb", Some(99), None, DEFAULT_MAX_OUTPUT_TOKENS);
        assert!(out.contains("past end"), "{out:?}");
    }

    // ---- #726: shared token-based model-facing output cap ----

    #[test]
    fn cap_model_output_passes_small_output_through_unchanged() {
        // Well under budget → exact bytes, no marker.
        let small = "hello\nworld\n";
        assert_eq!(cap_model_output(small, DEFAULT_MAX_OUTPUT_TOKENS), small);
    }

    #[test]
    fn cap_model_output_truncates_over_budget_as_head_tail() {
        let big = format!("HEAD_MARKER\n{}\nTAIL_MARKER", "middle\n".repeat(20_000));
        let out = cap_model_output_with_handle(&big, 1_000, 100, None);
        assert!(out.len() < big.len(), "must shrink: {} bytes", out.len());
        assert!(out.contains("HEAD_MARKER"), "head dropped: {out:?}");
        assert!(out.contains("TAIL_MARKER"), "tail dropped: {out:?}");
        assert!(out.contains("head+tail shown"), "marker present: {out:?}");
        assert!(
            !out.contains(&"middle\n".repeat(1_000)),
            "middle should be elided"
        );
    }

    #[test]
    fn cap_model_output_truncates_at_a_char_boundary() {
        // A multi-byte char straddling the cut must not be split — the body must
        // stay valid UTF-8 (no panic, no replacement char).
        let budget = 10; // ~40 chars
        let body = "é".repeat(1_000); // 2 bytes each
        let out = cap_model_output(&body, budget);
        assert!(out.is_char_boundary(out.len()), "valid boundary");
        assert!(
            out.chars()
                .all(|c| c == 'é' || !c.is_control() || c == '\n'),
            "no split char: {out:?}"
        );
    }

    #[test]
    fn cap_model_output_zero_budget_is_no_cap() {
        let body = "z".repeat(500_000);
        assert_eq!(cap_model_output(&body, 0), body);
    }

    #[test]
    fn token_to_char_math_uses_the_default_four_chars_per_token() {
        // The budget→char conversion is the default 4 chars/token (the shared
        // estimator constant), so a 10k-token budget is a 40k-char backstop.
        let est = crate::tokens::TokenEstimation::default();
        assert_eq!(est.chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS), 40_000);
    }

    #[test]
    fn find_detail_bare_path_has_no_filters() {
        let opts = FindOpts {
            name: None,
            type_filter: FindType::Any,
            max_depth: None,
            max_results: 1000,
            respect_gitignore: true,
            case_sensitive: true,
            show_size: false,
            sort: FindSort::Name,
        };
        assert_eq!(find_detail(".", &opts), ".");
    }

    #[test]
    fn find_detail_shows_only_non_default_filters() {
        let opts = FindOpts {
            name: Some("*.rs"),
            type_filter: FindType::Files,
            max_depth: Some(2),
            max_results: 50,
            respect_gitignore: false,
            case_sensitive: false,
            show_size: false,
            sort: FindSort::Name,
        };
        assert_eq!(
            find_detail("src", &opts),
            "src (name=*.rs, type=f, depth=2, max=50, no-gitignore, icase)"
        );
    }

    #[test]
    fn find_detail_omits_each_default_independently() {
        let opts = FindOpts {
            name: None,
            type_filter: FindType::Dirs,
            max_depth: None,
            max_results: 1000,
            respect_gitignore: true,
            case_sensitive: true,
            show_size: false,
            sort: FindSort::Name,
        };
        assert_eq!(find_detail(".", &opts), ". (type=d)");
    }

    #[test]
    fn find_detail_notes_the_size_column_and_size_sort() {
        let opts = FindOpts {
            name: Some("*.rs"),
            type_filter: FindType::Files,
            max_depth: None,
            max_results: 10,
            respect_gitignore: true,
            case_sensitive: true,
            show_size: true,
            sort: FindSort::Size,
        };
        assert_eq!(
            find_detail(".", &opts),
            ". (name=*.rs, type=f, max=10, sort=size, size)"
        );
    }

    #[test]
    fn use_skill_tool_is_advertised_in_definitions() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"use_skill"), "got: {names:?}");
    }

    #[test]
    fn merged_tool_definitions_with_empty_mcp_is_builtin_set() {
        let merged = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false,
        );
        let names: Vec<&str> = merged
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "run_command",
                "read_file",
                "write_file",
                "edit_file",
                "delete_file",
                "list_dir",
                "find",
                "use_skill",
                "web_fetch",
                // #721: advertised ALWAYS (core capability-grant request, no
                // presence gate) — part of the base tool_definitions() set.
                "request_permissions",
                // #714: advertised ALWAYS (no presence gate), so it joins the
                // base set even with every `with_*` flag off.
                "resume_context",
                // Exact prompt recovery is an invariant, independent of the
                // optional general-memory disclosure surface.
                "prompt_read",
                // Prompt-rooted work recovery is equally invariant and
                // always present, even before any artifact has been written.
                "artifact_read",
                // #725: advertised ALWAYS (a discovery tool must always be
                // present), so it too joins the base set with every flag off.
                "tool_search",
                // #727: advertised ALWAYS (read-only budget self-read, no
                // presence gate), pushed right after resume_context.
                "get_context_remaining",
                // #728: advertised ALWAYS (a model must always be able to ask the
                // human; degrades honestly headless), pushed last.
                "request_user_input",
                // #891: advertised ALWAYS (the model-facing lifecycle surface;
                // degrades honestly with "no command configured"), pushed after
                // request_user_input.
                "lifecycle",
                // #1004: advertised ALWAYS (present-findings surface; needs no
                // injected capability, degrades to raw source when color is
                // off), pushed after lifecycle.
                "render_report",
            ]
        );
    }

    /// FR-1 part 2 (#997): a persona's `tools:` allow-list scopes the ADVERTISED
    /// catalog — only the named tools survive, PLUS the always-on infra tools the
    /// loop can't run without (which no persona may fence off). `None` leaves the
    /// catalog whole (the zero-cost path for every non-persona session).
    #[test]
    fn persona_allow_list_filters_the_advertised_catalog() {
        let full =
            merged_tool_definitions(&NoMcp, true, true, true, true, true, true, true, true, true);
        let name_set = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .filter_map(|d| d["function"]["name"].as_str().map(str::to_owned))
                .collect()
        };
        // No persona → catalog untouched.
        assert_eq!(
            name_set(&filter_advertised_tools(full.clone(), None)),
            name_set(&full),
            "None must be a no-op"
        );
        // A read-only coach (`tools = ["read_file"]`): read_file survives; the
        // mutating built-ins are dropped; every always-on infra tool still rides.
        let allow = vec!["read_file".to_string()];
        let got = name_set(&filter_advertised_tools(full, Some(&allow)));
        assert!(got.iter().any(|n| n == "read_file"), "granted tool kept");
        for denied in [
            "write_file",
            "edit_file",
            "delete_file",
            "run_command",
            "list_dir",
        ] {
            assert!(
                !got.iter().any(|n| n == denied),
                "{denied} must be filtered out"
            );
        }
        for infra in [
            "resume_context",
            "prompt_read",
            "tool_search",
            "get_context_remaining",
            "request_user_input",
            "lifecycle",
        ] {
            assert!(
                got.iter().any(|n| n == infra),
                "{infra} is always-on and must survive any persona"
            );
        }
    }

    /// FR-1 part 2 (#997): `persona_tool_allowed` is the single predicate behind
    /// BOTH the advertise-filter and the executor reject — a tool is callable iff
    /// the persona names it OR it is always-on infra — so the set the model sees
    /// and the set it may run can never drift apart.
    #[test]
    fn persona_tool_allowed_admits_named_and_always_on_only() {
        let allow = vec!["read_file".to_string()];
        assert!(persona_tool_allowed("read_file", &allow), "named → allowed");
        assert!(
            persona_tool_allowed("request_user_input", &allow),
            "always-on infra → allowed even when unlisted"
        );
        assert!(
            !persona_tool_allowed("write_file", &allow),
            "unlisted non-infra → denied"
        );
        assert!(
            !persona_tool_allowed("delete_file", &allow),
            "unlisted non-infra → denied"
        );
    }

    /// Prompt disposition is an independent, fail-closed catalog boundary:
    /// non-Act turns retain only explicit read/recovery tools, so a generic MCP
    /// name cannot appear merely because its schema was connected to the session.
    #[test]
    fn prompt_disposition_filters_catalog_and_unknown_names_fail_closed() {
        let defs = serde_json::json!([
            { "type": "function", "function": { "name": "read_file" } },
            { "type": "function", "function": { "name": "write_file" } },
            { "type": "function", "function": { "name": "run_command" } },
            { "type": "function", "function": { "name": "request_permissions" } },
            { "type": "function", "function": { "name": "incident__read" } },
            { "not": "a callable definition" }
        ]);
        let names = |defs: &serde_json::Value| {
            defs.as_array()
                .unwrap()
                .iter()
                .filter_map(|def| def["function"]["name"].as_str())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };

        let research = filter_tools_for_disposition(defs.clone(), PromptDisposition::Research);
        assert_eq!(names(&research), vec!["read_file"]);
        assert!(tool_allowed(PromptDisposition::Explain, "read_file"));
        assert!(!tool_allowed(PromptDisposition::Explain, "write_file"));
        assert!(!tool_allowed(PromptDisposition::Research, "incident__read"));
        assert!(!tool_allowed(PromptDisposition::Ask, "read_file"));
        assert!(tool_allowed(PromptDisposition::Act, "incident__write"));
        // #1258: `find` carries the size column (sort=size/show_size), so an
        // evidence-only turn answers "largest files" through it — pin that it
        // stays in the Explain/Research set (guards against a future move to a
        // gated tool that would re-box the diagnosed session).
        assert!(tool_allowed(PromptDisposition::Explain, "find"));
        assert!(tool_allowed(PromptDisposition::Research, "find"));
        assert_eq!(
            filter_tools_for_disposition(
                serde_json::json!({ "not": "a catalog" }),
                PromptDisposition::Research
            ),
            serde_json::json!([]),
            "a non-Act catalog with no enumerable tool names must fail closed"
        );

        // Act is the compatibility/default path: it preserves definitions,
        // including an opaque extension definition the disposition filter cannot
        // classify by name.
        assert_eq!(
            filter_tools_for_disposition(defs.clone(), PromptDisposition::Act),
            defs
        );
    }

    /// FR-1 part 2 (#997): the executor is the ENFORCEMENT half. Even a
    /// hallucinated call the advertise-filter can't intercept is refused BY NAME
    /// before any side effect — while a granted tool and the always-on infra
    /// pass. Regression for a coach persona whose `tools:` list must be a real
    /// boundary, not a cosmetic hint.
    #[tokio::test]
    async fn executor_refuses_tools_outside_the_persona_allow_list() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = crate::caveats::Caveats::top();
        let allow = vec!["read_file".to_string()];
        // write_file is NOT granted → refused with the persona message, and the
        // file is never written (top caveats would otherwise permit it).
        let target = ws.path().join("blocked.txt");
        let args = serde_json::json!({
            "path": target.to_string_lossy(),
            "content": "should never be written",
        });
        let out = call_offload("write_file", &args, &ws, &caveats, Some(&allow)).await;
        assert!(
            out.contains("not available under the active persona"),
            "expected persona refusal, got: {out}"
        );
        assert!(!target.exists(), "a denied write must not touch the fs");
        // An always-on infra tool rides even though it is unlisted.
        let infra = call_offload(
            "get_context_remaining",
            &serde_json::json!({}),
            &ws,
            &caveats,
            Some(&allow),
        )
        .await;
        assert!(
            !infra.contains("not available under the active persona"),
            "always-on infra must not be refused: {infra}"
        );
    }

    /// FR-3 (#998): the absolute deny-list is wired into the executor and is
    /// GRANT-INDEPENDENT — even with top caveats and NO persona, a `run_command`
    /// whose exec target is forbidden (`ssh`) is refused before the shell runs,
    /// while an ordinary command is untouched. Guards against the deny module
    /// being present but never called.
    #[tokio::test]
    async fn executor_enforces_the_absolute_deny_list() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = crate::caveats::Caveats::top(); // maximal grant — deny still bites
        let denied = call_offload(
            "run_command",
            &serde_json::json!({ "command": "ssh host 'uptime'" }),
            &ws,
            &caveats,
            None, // no persona — the floor is independent of any grant
        )
        .await;
        assert!(
            denied.contains("absolute deny-list"),
            "ssh must hit the deny-list, got: {denied}"
        );
        // A benign command sails past the deny gate (it reaches normal exec).
        let ok = call_offload(
            "run_command",
            &serde_json::json!({ "command": "echo coaching" }),
            &ws,
            &caveats,
            None,
        )
        .await;
        assert!(
            !ok.contains("absolute deny-list"),
            "an ordinary command must not be denied, got: {ok}"
        );
    }

    /// Test-only thin wrapper over the 22-arg [`execute_tool_with_offload`] that
    /// fixes every optional seam to `None` and surfaces just the persona list.
    async fn call_offload(
        name: &str,
        args: &serde_json::Value,
        ws: &tempfile::TempDir,
        caveats: &crate::caveats::Caveats,
        persona_tools: Option<&[String]>,
    ) -> String {
        execute_tool_with_offload(
            name,
            args,
            &ws.path().to_string_lossy(),
            false,
            20,
            caveats,
            &mut NoMcp,
            None,  // build_check_cmd
            None,  // note_sink
            None,  // recall_source
            None,  // memory_source
            None,  // permission_gate
            None,  // exec_floor
            None,  // git_tool
            None,  // crew_runner
            None,  // scratchpad_store
            None,  // code_search
            None,  // experience_store
            None,  // step_ledger
            false, // tool_offload
            None,  // spill_store
            persona_tools,
        )
        .await
    }

    /// #894: each registry entry's schema-builder produces the SAME name the
    /// entry declares — catches a copy-paste where the `ToolSpec.name` and the
    /// `*_tool_definition()` disagree.
    #[test]
    fn registry_specs_match_their_definition_names() {
        for spec in EXTENDED_TOOL_REGISTRY {
            let def = (spec.definition)();
            assert_eq!(
                def["function"]["name"].as_str(),
                Some(spec.name),
                "ToolSpec name {:?} != definition name",
                spec.name
            );
        }
    }

    /// #894: no built-in tool name is declared twice across the base array and
    /// the registry (a dup would double-advertise and confuse dispatch).
    #[test]
    fn builtin_tool_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for name in ALL_TOOL_NAMES.iter() {
            assert!(seen.insert(*name), "duplicate built-in tool name: {name}");
        }
    }

    /// #894 anti-drift (the payoff): with EVERY gate on, the advertised set from
    /// `merged_tool_definitions` equals `ALL_TOOL_NAMES` in BOTH directions. This
    /// is the test that would have caught the `lifecycle` drift — a tool
    /// advertised/dispatched but missing from the real-name set (or vice versa)
    /// fails here.
    #[test]
    fn advertised_set_matches_all_tool_names_both_directions() {
        let all =
            merged_tool_definitions(&NoMcp, true, true, true, true, true, true, true, true, true);
        let advertised: std::collections::HashSet<&str> = all
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        let names: std::collections::HashSet<&str> = ALL_TOOL_NAMES.iter().copied().collect();
        // Every advertised tool is a real (non-hallucinated) name...
        for a in &advertised {
            assert!(
                names.contains(a),
                "advertised but not in ALL_TOOL_NAMES: {a}"
            );
        }
        // ...and every real name is actually advertised when its gate is on.
        for n in &names {
            assert!(
                advertised.contains(n),
                "in ALL_TOOL_NAMES but never advertised: {n}"
            );
        }
    }

    /// #894: `BASE_TOOL_NAMES` mirrors the names inlined in `tool_definitions()`
    /// exactly and in order — the one hand-kept mirror, guarded here.
    #[test]
    fn base_tool_names_match_tool_definitions() {
        let defs = tool_definitions();
        let base: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        assert_eq!(base, BASE_TOOL_NAMES);
    }

    /// #894 regression for the concrete drift that motivated the registry: the
    /// `lifecycle` tool (#891) is advertised + dispatched, so it MUST be a real
    /// name — otherwise every legitimate `lifecycle` call is miscounted as a
    /// hallucination (inflating the anti-loop counter). Before the registry it
    /// was missing from `ALL_TOOL_NAMES`; the derivation makes that impossible.
    #[test]
    fn lifecycle_is_a_real_tool_name_not_a_hallucination() {
        assert!(
            ALL_TOOL_NAMES.contains(&"lifecycle"),
            "lifecycle must be a real tool name"
        );
        assert!(
            !is_hallucination("lifecycle", &serde_json::json!({"phase": "test"})),
            "a real lifecycle call must not be flagged as a hallucination"
        );
    }

    #[test]
    fn lifecycle_definition_enum_matches_phase_vocabulary() {
        // The schema's phase enum is built from `Phase::ALL`, so it can never
        // drift from the vocabulary the executor parses with `Phase::from_key`.
        let def = lifecycle_tool_definition();
        assert_eq!(def["function"]["name"], "lifecycle");
        let enum_vals: Vec<&str> = def["function"]["parameters"]["properties"]["phase"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let vocab: Vec<&str> = crate::tooling::Phase::ALL
            .iter()
            .map(|p| p.as_str())
            .collect();
        assert_eq!(enum_vals, vocab);
    }

    #[test]
    fn run_phase_aliases_route_to_lifecycle() {
        for a in ["run_phase", "run_lifecycle", "lifecycle_run"] {
            assert!(
                matches!(
                    resolve_tool_alias(a),
                    Some(AliasOutcome::Rewrite("lifecycle"))
                ),
                "{a} should rewrite to lifecycle"
            );
        }
        // The canonical name is NOT an alias — it dispatches directly.
        assert!(resolve_tool_alias("lifecycle").is_none());
    }

    #[tokio::test]
    async fn lifecycle_unknown_phase_lists_valid_phases() {
        // An unknown phase returns before any fs/subprocess touch, so this is a
        // fully-mocked unit test.
        let caveats = crate::caveats::Caveats::top();
        let args = serde_json::json!({ "phase": "deploy" });
        let out = execute_tool(
            "lifecycle",
            &args,
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            out.starts_with("error: unknown lifecycle phase 'deploy'"),
            "{out}"
        );
        assert!(out.contains("check"), "should name valid phases: {out}");
    }

    /// `save_note` is sink-gated: absent from the base `tool_definitions`
    /// (headless/eval callers see no memory tool) and from the merged set
    /// without a sink; present in the merged set when a sink exists.
    #[test]
    fn save_note_advertised_only_with_a_sink() {
        fn names(defs: &serde_json::Value) -> Vec<&str> {
            defs.as_array()
                .unwrap()
                .iter()
                .filter_map(|d| d["function"]["name"].as_str())
                .collect()
        }
        // Headless/eval callers see no memory tool in the base set …
        let base = tool_definitions();
        assert!(!names(&base).contains(&"save_note"), "got: {base}");
        // … nor in the merged set without a sink …
        let without = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false,
        );
        assert!(!names(&without).contains(&"save_note"));
        // … but a sink advertises it.
        let with = merged_tool_definitions(
            &NoMcp, true, false, false, false, false, false, false, false, false,
        );
        assert!(names(&with).contains(&"save_note"), "got: {with}");
    }

    /// `recall` is source-gated exactly like `save_note` is sink-gated
    /// (Step 17.5): absent from the base set and from the merged set
    /// without a source; present when one exists.
    #[test]
    fn recall_advertised_only_with_a_source() {
        fn names(defs: &serde_json::Value) -> Vec<&str> {
            defs.as_array()
                .unwrap()
                .iter()
                .filter_map(|d| d["function"]["name"].as_str())
                .collect()
        }
        let base = tool_definitions();
        assert!(!names(&base).contains(&"recall"), "got: {base}");
        let without = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false,
        );
        assert!(!names(&without).contains(&"recall"));
        let with = merged_tool_definitions(
            &NoMcp, false, true, false, false, false, false, false, false, false,
        );
        assert!(names(&with).contains(&"recall"), "got: {with}");
        // The two gates are independent: both on advertises both.
        let both = merged_tool_definitions(
            &NoMcp, true, true, false, false, false, false, false, false, false,
        );
        assert!(names(&both).contains(&"save_note"));
        assert!(names(&both).contains(&"recall"));
    }

    /// `memory_fetch` is source-gated exactly like `recall` (#319): absent
    /// from the base set and from the merged set without a `MemorySource`;
    /// present when one exists. The flag is independent of the others.
    #[test]
    fn memory_fetch_advertised_only_with_a_source() {
        fn names(defs: &serde_json::Value) -> Vec<&str> {
            defs.as_array()
                .unwrap()
                .iter()
                .filter_map(|d| d["function"]["name"].as_str())
                .collect()
        }
        let base = tool_definitions();
        assert!(!names(&base).contains(&"memory_fetch"), "got: {base}");
        // Flag off (every existing caller, the inert default) → not advertised.
        let without = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false,
        );
        assert!(!names(&without).contains(&"memory_fetch"));
        // Flag on → advertised.
        let with = merged_tool_definitions(
            &NoMcp, false, false, true, false, false, false, false, false, false,
        );
        assert!(names(&with).contains(&"memory_fetch"), "got: {with}");
        // Independent of the save_note / recall gates: all three on lists all.
        let all = merged_tool_definitions(
            &NoMcp, true, true, true, false, false, false, false, false, false,
        );
        assert!(names(&all).contains(&"save_note"));
        assert!(names(&all).contains(&"recall"));
        assert!(names(&all).contains(&"memory_fetch"));
    }

    /// `is_hallucination` correctly identifies tool-name-as-command and unknown
    /// tool names, and correctly skips MCP-namespaced tools.
    #[test]
    fn hallucination_detection_coverage() {
        // tool name passed to run_command → hallucination
        assert!(is_hallucination(
            "run_command",
            &serde_json::json!({"command": "list_dir ."})
        ));
        // normal shell command → not a hallucination
        assert!(!is_hallucination(
            "run_command",
            &serde_json::json!({"command": "cargo test"})
        ));
        // unknown tool → hallucination
        assert!(is_hallucination(
            "definitely_not_a_real_tool",
            &serde_json::json!({})
        ));
        // MCP-namespaced tool → not a hallucination
        assert!(!is_hallucination(
            "my_server__some_tool",
            &serde_json::json!({})
        ));
        // known direct tools → not hallucinations when called correctly
        for t in [
            "list_dir",
            "read_file",
            "write_file",
            "edit_file",
            "delete_file",
            "use_skill",
            "web_fetch",
            "save_note",
            "recall",
        ] {
            assert!(!is_hallucination(t, &serde_json::json!({"path": "."})));
        }
    }

    /// #898/#1022: `run_command_redirect` bounces embedded-tool-served LOCAL git
    /// ops (and other direct tools), but lets git passthrough ops fall through
    /// to the shell — otherwise a model can never `git push` or `git rm`.
    #[test]
    fn resolve_exec_cwd_confines_to_workspace() {
        // #1159: relative cwd joins under the workspace; absolute passes through
        // (the fs fence rejects escapes); empty/None → workspace root.
        assert_eq!(resolve_exec_cwd("/ws", None), "/ws");
        assert_eq!(resolve_exec_cwd("/ws", Some("")), "/ws");
        assert_eq!(resolve_exec_cwd("/ws", Some("  ")), "/ws");
        // Relative join uses the platform separator (correct — the cwd feeds
        // the confined shell on this OS); compare against the same join.
        let joined = std::path::Path::new("/ws")
            .join("crates/foo")
            .to_string_lossy()
            .into_owned();
        assert_eq!(resolve_exec_cwd("/ws", Some("crates/foo")), joined);
        // An absolute path passes through verbatim (platform-appropriate).
        let abs = if cfg!(windows) {
            "C:\\ws\\sub"
        } else {
            "/ws/sub"
        };
        assert_eq!(resolve_exec_cwd("/ws", Some(abs)), abs);
        // The dispatch args carry it verbatim.
        let a = confined_dispatch_args("ls", &joined);
        assert_eq!(a["cwd"], joined);
    }

    #[test]
    fn split_leading_cd_folds_the_habitual_cd_prefix() {
        // The reported failure: `cd <workspace> && git checkout -b …` tried to
        // exec the `cd` builtin. Fold it: cwd = the path, run the real command.
        let (path, rest) = split_leading_cd("cd /ws/newt-agent && git checkout -b x");
        assert_eq!(path.as_deref(), Some("/ws/newt-agent"));
        assert_eq!(rest, "git checkout -b x");

        // `;` connective folds the same way.
        let (path, rest) = split_leading_cd("cd sub ; ls -la");
        assert_eq!(path.as_deref(), Some("sub"));
        assert_eq!(rest, "ls -la");

        // A quoted path with spaces is returned unquoted; the remainder is kept.
        let (path, rest) = split_leading_cd("cd \"/a b/c\" && cargo test");
        assert_eq!(path.as_deref(), Some("/a b/c"));
        assert_eq!(rest, "cargo test");

        // Only the FIRST cd is folded — a second cd stays in the remainder for
        // the shell engine (we don't chase chdir chains).
        let (path, rest) = split_leading_cd("cd a && cd b && ls");
        assert_eq!(path.as_deref(), Some("a"));
        assert_eq!(rest, "cd b && ls");

        // A bare `cd <path>` folds to an empty remainder (the caller turns this
        // into a guidance note — nothing to exec).
        let (path, rest) = split_leading_cd("cd /somewhere");
        assert_eq!(path.as_deref(), Some("/somewhere"));
        assert!(rest.is_empty());
    }

    #[test]
    fn split_leading_cd_leaves_non_cd_and_ambiguous_commands_whole() {
        // Not a cd at all → unchanged.
        assert_eq!(split_leading_cd("git status"), (None, "git status".into()));
        // `cd` as a substring of another word is not a match.
        assert_eq!(split_leading_cd("cding foo"), (None, "cding foo".into()));
        // `cd <path>` followed by something other than a sequential connective
        // (a pipe here) is left whole — we only fold the safe `&&`/`;` shapes.
        assert_eq!(
            split_leading_cd("cd x | grep y"),
            (None, "cd x | grep y".into())
        );
    }

    #[test]
    fn run_command_redirect_lets_git_network_ops_through() {
        // Ops the embedded git tool cannot do faithfully → fall through (None).
        for cmd in [
            "git push origin fix/foo",
            "git push",
            "git fetch origin",
            "git pull",
            "git clone https://example.com/r.git",
            "git rm src/cockpit.rs",
        ] {
            assert_eq!(run_command_redirect(cmd), None, "{cmd} must fall through");
        }
        // Local ops the embedded git tool handles → still redirect.
        for cmd in [
            "git status",
            "git log --oneline",
            "git add .",
            "git commit -m x",
        ] {
            assert_eq!(
                run_command_redirect(cmd),
                Some("git"),
                "{cmd} must redirect"
            );
        }
        // Other direct tools still redirect; plain shell commands run as-is.
        assert_eq!(run_command_redirect("read_file foo.txt"), Some("read_file"));
        assert_eq!(run_command_redirect("list_dir ."), Some("list_dir"));
        assert_eq!(run_command_redirect("cargo test"), None);
        assert_eq!(run_command_redirect("gh pr create --fill"), None);
        assert_eq!(run_command_redirect(""), None);
    }

    /// #1262: a command with shell COMPOSITION is a real shell program the
    /// embedded tools cannot serve — it must never be redirected (the diagnosed
    /// session's legitimate pipeline was bounced + miscounted as a corrected
    /// hallucination). Bare servable forms keep redirecting.
    #[test]
    fn run_command_redirect_passes_composed_commands_through() {
        // The exact diagnosed pipeline.
        assert_eq!(
            run_command_redirect(
                "find . -name \"*.rs\" -type f -print0 | xargs -0 du -k | sort -rn | head 20"
            ),
            None,
            "a pipeline leading with `find` is not a misdirected find call"
        );
        // Redirects and sequencing are composition too.
        assert_eq!(run_command_redirect("find . -name '*.log' > out.txt"), None);
        assert_eq!(run_command_redirect("git status && git diff"), None);
        assert_eq!(run_command_redirect("list_dir . ; echo done"), None);
        assert_eq!(run_command_redirect("read_file $(pick_file)"), None);
        assert_eq!(run_command_redirect("read_file `pick_file`"), None);
        // Bare servable forms still redirect (the true positives hold).
        assert_eq!(run_command_redirect("find . -name \"*.rs\""), Some("find"));
        assert_eq!(run_command_redirect("list_dir src"), Some("list_dir"));
        assert_eq!(run_command_redirect("git status"), Some("git"));
    }

    /// #1262: the loop's `hallucination_count` increments exactly on
    /// `is_hallucination` (mod.rs call classification), so this pure pin IS the
    /// turn-metrics behavior: the diagnosed pipeline counts ZERO hallucinations;
    /// a bare misdirected call still counts one.
    #[test]
    fn pipeline_is_never_counted_as_a_hallucination() {
        assert!(!is_hallucination(
            "run_command",
            &serde_json::json!({"command":
                "find . -name \"*.rs\" -type f -print0 | xargs -0 du -k | sort -rn | head 20"})
        ));
        assert!(!is_hallucination(
            "run_command",
            &serde_json::json!({"command": "git status && git diff"})
        ));
        // The true positive holds: a bare misdirected call still counts.
        assert!(is_hallucination(
            "run_command",
            &serde_json::json!({"command": "list_dir ."})
        ));
    }

    /// #898 regression: a real `git push` at run_command must NOT be counted as
    /// a hallucination (it now runs), while a local `git status` still is.
    #[test]
    fn is_hallucination_allows_git_network_ops() {
        assert!(!is_hallucination(
            "run_command",
            &serde_json::json!({"command": "git push origin fix/foo"})
        ));
        assert!(!is_hallucination(
            "run_command",
            &serde_json::json!({"command": "git fetch"})
        ));
        assert!(!is_hallucination(
            "run_command",
            &serde_json::json!({"command": "git rm src/cockpit.rs"})
        ));
        assert!(is_hallucination(
            "run_command",
            &serde_json::json!({"command": "git status"})
        ));
    }

    /// #898: the forge PR/MR-creation URL is extracted from git's push output
    /// (GitHub and GitLab), and ordinary URLs do not false-positive.
    #[test]
    fn pr_creation_url_extracts_github_and_gitlab() {
        let github = "remote: Create a pull request for 'fix/foo' on GitHub by visiting:\n\
                      remote:      https://github.com/OWNER/REPO/pull/new/fix/foo\n";
        assert_eq!(
            pr_creation_url(github),
            Some("https://github.com/OWNER/REPO/pull/new/fix/foo")
        );
        let gitlab = "remote: To create a merge request for topic, visit:\n\
                      remote:   https://gitlab.com/g/p/-/merge_requests/new?x=topic\n";
        assert_eq!(
            pr_creation_url(gitlab),
            Some("https://gitlab.com/g/p/-/merge_requests/new?x=topic")
        );
        // No PR URL present → None (ordinary fetch/clone output, plain links).
        assert_eq!(pr_creation_url("Already up to date.\n"), None);
        assert_eq!(
            pr_creation_url("see https://github.com/OWNER/REPO/issues/1"),
            None
        );
    }

    /// #898: after a push whose output carries a PR-creation URL,
    /// `shell_envelope_output` appends the `gh pr create` next-step hint (and the
    /// URL survives), while ordinary command output is left untouched.
    #[test]
    fn shell_envelope_output_appends_pr_hint_on_push() {
        let push = serde_json::json!({
            "exit_code": 0,
            "stdout": "",
            "stderr": "remote: Create a pull request for 'fix/foo' on GitHub by visiting:\n\
                       remote:      https://github.com/OWNER/REPO/pull/new/fix/foo\n",
        });
        let out = shell_envelope_output(&push, 50, false, false, None, None);
        assert!(out.contains("gh pr create --fill"), "hint missing: {out}");
        assert!(
            out.contains("https://github.com/OWNER/REPO/pull/new/fix/foo"),
            "url dropped: {out}"
        );

        // Ordinary output: no hint, payload unchanged.
        let plain = serde_json::json!({ "exit_code": 0, "stdout": "hello\n", "stderr": "" });
        let out = shell_envelope_output(&plain, 50, false, false, None, None);
        assert!(!out.contains("gh pr create"), "spurious hint: {out}");
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn shell_envelope_output_spills_full_output_before_head_tail_cap() {
        let full = format!(
            "HEAD_ONLY_MARKER\n{}\nMIDDLE_ONLY_MARKER\n{}\nTAIL_ONLY_MARKER\n",
            "alpha\n".repeat(10_000),
            "omega\n".repeat(10_000)
        );
        let envelope = serde_json::json!({
            "exit_code": 0,
            "stdout": full,
            "stderr": "",
        });
        let store = spill::SessionSpillStore::default();
        let mut display = ToolDisplay::new(Vec::new(), false, 80, 3);
        display.call("run_command", "large-output-command");
        let out =
            shell_envelope_output(&envelope, 50, false, true, Some(&store), Some(&mut display));
        display.result(&out);

        assert!(out.contains("HEAD_ONLY_MARKER"), "head dropped: {out}");
        assert!(out.contains("TAIL_ONLY_MARKER"), "tail dropped: {out}");
        assert!(
            out.contains("memory_fetch(\"spill:s0\")"),
            "spill handle missing: {out}"
        );
        assert!(
            out.contains("grep=\"<pattern>\""),
            "search affordance missing: {out}"
        );
        let stored = store.fetch("s0").expect("full output stored");
        assert!(
            stored.contains("MIDDLE_ONLY_MARKER"),
            "spilled payload was capped before storage"
        );
        assert!(stored.ends_with("TAIL_ONLY_MARKER\n"));
        let rendered = String::from_utf8(display.into_inner()).unwrap();
        assert!(
            rendered.contains("▓ TAIL_ONLY_MARKER\n…\n"),
            "operator spill lost the raw shell tail: {rendered}"
        );
        assert!(
            !rendered.contains("memory_fetch(\"spill:s0\")"),
            "operator saw the model teaser instead of raw shell output: {rendered}"
        );
    }

    #[test]
    fn shell_envelope_without_streams_commits_the_exit_result() {
        let envelope = serde_json::json!({
            "exit_code": 3,
            "stdout": "",
            "stderr": "",
        });
        let mut display = ToolDisplay::new(Vec::new(), false, 80, 3);
        display.call("run_command", "exit 3");
        let out = shell_envelope_output(&envelope, 50, false, false, None, Some(&mut display));
        display.result(&out);

        assert_eq!(out, "(exit 3)");
        assert_eq!(
            String::from_utf8(display.into_inner()).unwrap(),
            "⚙  run_command: exit 3\n▒ (exit 3)\n…\n"
        );
    }

    #[test]
    fn envelope_denied_reads_structured_flag_only() {
        assert!(envelope_denied(&serde_json::json!({"denied": true})));
        assert!(!envelope_denied(&serde_json::json!({"denied": false})));
        assert!(!envelope_denied(&serde_json::json!({})));
        // A non-bool `denied` is treated as not-denied, never a panic.
        assert!(!envelope_denied(&serde_json::json!({"denied": "yes"})));
    }

    #[test]
    fn envelope_denial_reason_joins_or_falls_back() {
        let multi = serde_json::json!({
            "denials": [
                {"kind": "exec", "target": "rm", "reason": "exec rm denied"},
                {"kind": "open", "target": "/etc/shadow", "reason": "open denied"}
            ]
        });
        assert_eq!(
            envelope_denial_reason(&multi),
            "exec rm denied; open denied"
        );
        // Missing or empty denials → the generic message, never a panic.
        let generic = "denied: the capability leash refused an operation";
        assert_eq!(envelope_denial_reason(&serde_json::json!({})), generic);
        assert_eq!(
            envelope_denial_reason(&serde_json::json!({"denials": []})),
            generic
        );
        // Entries without a string `reason` are skipped.
        assert_eq!(
            envelope_denial_reason(&serde_json::json!({"denials": [{"kind": "exec"}]})),
            generic
        );
    }

    #[test]
    fn exec_allowlist_name_takes_basename() {
        assert_eq!(exec_allowlist_name("env"), "env");
        assert_eq!(exec_allowlist_name("/usr/bin/env"), "env");
        assert_eq!(exec_allowlist_name("/usr/bin/"), "bin");
        assert_eq!(exec_allowlist_name("C:\\tools\\env.exe"), "env.exe");
    }

    /// #775 (§2.5): denial recovery uses the BARE command target(s), never the
    /// reason sentence. Stuffing the full reason into the former notice's
    /// `'{target}'` field produced the
    /// field-report garble `capability denied: exec does not permit '<whole
    /// reason sentence>'`.
    #[test]
    fn exec_denial_target_label_is_the_bare_command_not_the_reason() {
        let one = serde_json::json!({
            "denied": true,
            "denials": [{
                "kind": "exec",
                "target": "export",
                "reason": "exec of \"export\" is not within the granted authority"
            }]
        });
        let label = exec_denial_target_label(&one);
        assert_eq!(label, "export");
        // It is the bare command — NEVER the reason sentence (which, in the
        // `'{target}'` slot, was the nested garble).
        assert!(!label.contains("is not within the granted authority"));
        // Multiple targets join cleanly; an envelope with no target falls back
        // to a generic label so the notice still prints one clean line.
        let multi = serde_json::json!({
            "denials": [
                {"kind": "exec", "target": "export", "reason": "r"},
                {"kind": "exec", "target": "set", "reason": "r"}
            ]
        });
        assert_eq!(exec_denial_target_label(&multi), "export, set");
        assert_eq!(
            exec_denial_target_label(&serde_json::json!({})),
            "a command"
        );
    }

    #[test]
    fn host_of_url_extracts_hosts_conservatively() {
        assert_eq!(host_of_url("https://docs.rs/serde"), Some("docs.rs".into()));
        assert_eq!(host_of_url("http://Docs.RS"), Some("docs.rs".into()));
        assert_eq!(
            host_of_url("https://user:pw@example.com:8443/p?q#f"),
            Some("example.com".into())
        );
        assert_eq!(host_of_url("https://[::1]:8080/x"), Some("::1".into()));
        // Unparseable / non-http inputs skip the pre-check (None) rather
        // than guessing — enforcement stays with the leash either way.
        assert_eq!(host_of_url("not a url"), None);
        assert_eq!(host_of_url("ftp://example.com"), None);
        assert_eq!(host_of_url("https:///path-only"), None);
    }

    #[test]
    fn exec_denial_requests_lifts_only_pure_exec_envelopes() {
        // The promptable case: every entry is an exec denial with a target;
        // the request target is the allowlist basename (the grantable name).
        let exec_only = serde_json::json!({
            "denied": true,
            "denials": [
                {"kind": "exec", "target": "/usr/bin/npm", "reason": "exec npm denied"},
                {"kind": "exec", "target": "node", "reason": "exec node denied"}
            ]
        });
        let reqs = exec_denial_requests(&exec_only).expect("promptable");
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].tool, "run_command");
        assert_eq!(reqs[0].kind, DenialKind::Exec);
        assert_eq!(
            reqs[0].target, "npm",
            "basename, same rule as the config hint"
        );
        assert_eq!(reqs[0].reason, "exec npm denied");
        assert_eq!(reqs[1].target, "node");

        // A non-exec entry anywhere keeps the standard denial: mapping an
        // opaque `open` onto an fs axis would over-grant.
        let mixed = serde_json::json!({
            "denials": [
                {"kind": "exec", "target": "npm", "reason": "r"},
                {"kind": "open", "target": "/etc/shadow", "reason": "r"}
            ]
        });
        assert!(exec_denial_requests(&mixed).is_none());

        // Missing/empty pieces are never promptable.
        assert!(exec_denial_requests(&serde_json::json!({})).is_none());
        assert!(exec_denial_requests(&serde_json::json!({"denials": []})).is_none());
        let empty_target = serde_json::json!({
            "denials": [{"kind": "exec", "target": "", "reason": "r"}]
        });
        assert!(exec_denial_requests(&empty_target).is_none());
        let no_target = serde_json::json!({
            "denials": [{"kind": "exec", "reason": "r"}]
        });
        assert!(exec_denial_requests(&no_target).is_none());

        // #1150: a STRUCTURAL refusal must NOT be promptable — offering a grant
        // for `$(` (which the engine cannot interpret) is a grant->denial
        // contradiction. The exact reason strings are agent-bridle's
        // Refusal::Display output (verified against parse.rs).
        let dynamic = serde_json::json!({
            "denied": true,
            "denials": [{
                "kind": "exec",
                "target": "command/arithmetic substitution `$(`",
                "reason": "refused by design: command/arithmetic substitution `$(` is a \
                           dynamic construct the confined shell does not interpret (use the \
                           embedder's unbridled/--yolo path for a full shell)"
            }]
        });
        assert!(
            exec_denial_requests(&dynamic).is_none(),
            "structural refusal must not offer a grant menu (#1150)"
        );
        let unsupported = serde_json::json!({
            "denials": [{
                "kind": "exec",
                "target": "heredoc/herestring `<<`",
                "reason": "not yet supported by the confined shell engine: \
                           heredoc/herestring `<<` (tracked on agent-bridle#34)"
            }]
        });
        assert!(exec_denial_requests(&unsupported).is_none());
        // A genuine authority denial (not structural) STAYS promptable.
        let authority = serde_json::json!({
            "denials": [{"kind": "exec", "target": "cargo",
                         "reason": "exec of \"cargo\" is not within the granted authority"}]
        });
        assert!(exec_denial_requests(&authority).is_some());
    }

    /// #905: a NET denial envelope (agent-bridle #196 shape) lifts to a per-host
    /// net PermissionRequest; the target is the CONNECT host verbatim (no
    /// basename mangling). Non-net / mixed / empty batches stay flat.
    #[test]
    fn net_denial_requests_lifts_only_pure_net_envelopes() {
        let net_only = serde_json::json!({
            "denied": true,
            "denials": [
                {"kind": "net", "target": "github.com", "reason": "net does not permit 'github.com'"},
                {"kind": "net", "target": "api.github.com", "reason": "net does not permit 'api.github.com'"}
            ]
        });
        let reqs = net_denial_requests(&net_only).expect("promptable");
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].tool, "run_command");
        assert_eq!(reqs[0].kind, DenialKind::Net);
        assert_eq!(
            reqs[0].target, "github.com",
            "host verbatim, not a basename"
        );
        assert_eq!(reqs[0].reason, "net does not permit 'github.com'");
        assert_eq!(reqs[1].target, "api.github.com");

        // A non-net entry anywhere → not net-promptable (exec lifter handles exec).
        let mixed = serde_json::json!({
            "denials": [
                {"kind": "net", "target": "github.com", "reason": "r"},
                {"kind": "exec", "target": "npm", "reason": "r"}
            ]
        });
        assert!(net_denial_requests(&mixed).is_none());
        // Exec-only is not net-promptable; empty/missing targets never are.
        let exec_only = serde_json::json!({"denials": [{"kind": "exec", "target": "npm"}]});
        assert!(net_denial_requests(&exec_only).is_none());
        assert!(net_denial_requests(&serde_json::json!({"denials": []})).is_none());
        let empty_target = serde_json::json!({"denials": [{"kind": "net", "target": ""}]});
        assert!(net_denial_requests(&empty_target).is_none());
    }

    /// #905: the human denial NOTICE labels a pure-net refusal `net` (not `exec`),
    /// so it never reads "exec does not permit '<host>'". Exec / mixed stay `exec`.
    #[test]
    fn denials_name_the_exact_recovery_call() {
        // #1160: the model shouldn't infer parameters the harness holds — a
        // denial carries the copy-pasteable request_permissions(...) call.
        let fs = denied_fs_result("fs_write", "/etc/hosts");
        assert!(
            fs.contains(r#"request_permissions(capability="fs_write", target="/etc/hosts""#),
            "{fs}"
        );
        let hint = denial_recovery_hint("exec", "cargo");
        assert!(
            hint.contains(r#"capability="exec""#) && hint.contains(r#"target="cargo""#),
            "{hint}"
        );
    }

    #[test]
    fn denial_axis_label_is_net_only_for_pure_net() {
        let net = serde_json::json!({"denials": [{"kind": "net", "target": "github.com"}]});
        assert_eq!(denial_axis_label(&net), "net");
        let exec = serde_json::json!({"denials": [{"kind": "exec", "target": "rm"}]});
        assert_eq!(denial_axis_label(&exec), "exec");
        let mixed = serde_json::json!({
            "denials": [{"kind": "net", "target": "h"}, {"kind": "exec", "target": "rm"}]
        });
        assert_eq!(denial_axis_label(&mixed), "exec", "mixed defaults to exec");
        assert_eq!(denial_axis_label(&serde_json::json!({})), "exec");
    }

    #[test]
    fn tui_permits_path_prefix_semantics() {
        use crate::caveats::Scope;
        assert!(tui_permits_path(&Scope::All, "/anything/at/all"));
        assert!(!tui_permits_path(&Scope::<String>::none(), "/ws/file"));
        let only = Scope::only(["/ws".to_string()]);
        assert!(tui_permits_path(&only, "/ws/sub/file.rs"));
        assert!(tui_permits_path(&only, "/ws"), "the workspace root itself");
        assert!(!tui_permits_path(&only, "/elsewhere/file.rs"));
        // `..` traversal must NOT escape: a path that lexically resolves outside
        // the workspace is denied even though it textually begins with it.
        assert!(
            !tui_permits_path(&only, "/ws/../etc/passwd"),
            "`..` traversal escapes the workspace"
        );
        assert!(
            !tui_permits_path(&only, "/ws/../../etc/passwd"),
            "repeated `..` traversal escapes the workspace"
        );
        // A sibling dir that merely shares the string prefix is not under /ws.
        assert!(
            !tui_permits_path(&only, "/ws-secret/file.rs"),
            "sibling-prefix collision escapes the workspace"
        );
        // A `..` that stays inside the workspace is still permitted.
        assert!(tui_permits_path(&only, "/ws/sub/../file.rs"));
    }

    /// Ratchet for the OPEN `fs-canonical-containment` deviation (issue #522,
    /// `docs/security/ocap-deviations.md`). `tui_permits_path` is string-lexical:
    /// it collapses `..` but does NOT resolve symlinks, so a link *inside* the
    /// workspace pointing OUT is permitted even though the OS would read the
    /// outside target. This test builds the path the call sites do
    /// (`workspace.join(model_path)`) over a REAL symlink and PINS that residual.
    ///
    /// When canonicalize-then-contain lands (the deviation's closure criterion),
    /// the gate will deny the symlinked path and this assertion MUST flip to
    /// `!tui_permits_path(...)` — that break is the signal to close the deviation.
    /// Unix-only: Windows symlinks need privileges (mirrors
    /// `find_does_not_follow_symlinks_out_of_workspace`).
    #[cfg(unix)]
    #[test]
    fn tui_permits_path_symlink_escape_is_the_known_residual() {
        use crate::caveats::Scope;
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), b"x").unwrap();
        let ws = tempfile::TempDir::new().unwrap();
        // A symlink under the workspace whose target is OUTSIDE it.
        std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

        let only = Scope::only([ws.path().to_string_lossy().into_owned()]);

        // What the read/write call sites feed the gate for model path "link/secret".
        let via_link = ws.path().join("link").join("secret");
        // RESIDUAL: permitted today — the gate can't see through the symlink.
        // Flip to `!` when the gate canonicalizes (closes fs-canonical-containment).
        assert!(
            tui_permits_path(&only, &via_link.to_string_lossy()),
            "string gate permits a symlinked escape — known residual (#522)"
        );

        // Contrast: a plain `..` escape through the SAME root is already denied
        // (lexical containment, the part #502 did fix) — so this isn't a blanket
        // hole, only the symlink-resolution gap.
        let dotdot = ws.path().join("..").join("etc").join("passwd");
        assert!(
            !tui_permits_path(&only, &dotdot.to_string_lossy()),
            "`..` escape is denied even though symlink escape is not"
        );
    }

    /// The file tools retain the lexical OCAP residual above, but their
    /// provenance hook must fail closed so it never labels an outside target as
    /// a workspace artifact.
    #[cfg(unix)]
    #[test]
    fn artifact_provenance_rejects_physical_symlink_escapes() {
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("existing"), b"x").unwrap();
        let ws = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

        assert!(artifact_path_is_physically_within_workspace(
            ws.path(),
            &ws.path().join("new/leaf.txt")
        ));
        assert!(!artifact_path_is_physically_within_workspace(
            ws.path(),
            &ws.path().join("link/existing")
        ));
        assert!(!artifact_path_is_physically_within_workspace(
            ws.path(),
            &ws.path().join("link/new-file")
        ));

        std::os::unix::fs::symlink(outside.path().join("missing"), ws.path().join("dangling"))
            .unwrap();
        assert!(!artifact_path_is_physically_within_workspace(
            ws.path(),
            &ws.path().join("dangling")
        ));
    }

    #[test]
    fn artifact_file_streaming_hash_and_postcondition_are_exact() {
        let ws = tempfile::TempDir::new().unwrap();
        let bytes = vec![0x5a; 3 * 64 * 1024 + 17];
        let path = ws.path().join("large.bin");
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(
            artifact_preimage_state(&path, true),
            super::super::artifact_hooks::ArtifactFileState::from_bytes(&bytes)
        );
        assert!(artifact_file_matches(&path, &bytes).unwrap());
        let mut different = bytes.clone();
        different[64 * 1024] ^= 1;
        assert!(!artifact_file_matches(&path, &different).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_preimage_never_opens_non_regular_files() {
        let ws = tempfile::TempDir::new().unwrap();
        let socket = ws.path().join("local.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert_eq!(
            artifact_preimage_state(&socket, true),
            super::super::artifact_hooks::ArtifactFileState::unavailable(
                "preimage_not_regular_file"
            )
        );
    }

    // --- PR4: the `git` tool is presence-gated -----------------------------

    #[test]
    fn git_tool_advertised_only_with_the_presence_gate() {
        fn names(defs: &serde_json::Value) -> Vec<&str> {
            defs.as_array()
                .unwrap()
                .iter()
                .filter_map(|d| d["function"]["name"].as_str())
                .collect()
        }
        let with = merged_tool_definitions(
            &NoMcp, false, false, false, true, false, false, false, false, false,
        );
        assert!(names(&with).contains(&"git"), "with_git advertises git");
        let without = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false,
        );
        assert!(!names(&without).contains(&"git"), "no git without the gate");
        // #479: the /team toggle advertises both crew tools, and only then.
        let team = merged_tool_definitions(
            &NoMcp, false, false, false, false, true, false, false, false, false,
        );
        assert!(
            names(&team).contains(&"crew") && names(&team).contains(&"compose_roster"),
            "with_team advertises crew + compose_roster"
        );
        assert!(
            !names(&without).contains(&"crew"),
            "no crew without the gate"
        );
        // Step 26.4 (#583): the scratchpad state tools, only with the gate on.
        let scratch = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, true, false, false, false,
        );
        for t in ["state_set", "state_get", "state_clear"] {
            assert!(
                names(&scratch).contains(&t),
                "{t} advertised with_scratchpad"
            );
            assert!(!names(&without).contains(&t), "{t} hidden without the gate");
            assert!(
                !is_hallucination(t, &serde_json::json!({})),
                "{t} is a real tool"
            );
        }
        // Step 26.5.5 (#582): the code_search tool, only with its gate on.
        let code = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, true, false, false,
        );
        assert!(
            names(&code).contains(&"code_search"),
            "code_search advertised"
        );
        assert!(
            !names(&without).contains(&"code_search"),
            "code_search hidden without the gate"
        );
        assert!(!is_hallucination("code_search", &serde_json::json!({})));
        // Step 26.6a (#585): the experiential record/recall tools, only with the gate.
        let exp = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, true, false,
        );
        for t in ["experience_record", "experience_recall"] {
            assert!(names(&exp).contains(&t), "{t} advertised with_experiential");
            assert!(!names(&without).contains(&t), "{t} hidden without the gate");
            assert!(
                !is_hallucination(t, &serde_json::json!({})),
                "{t} is a real tool"
            );
        }
        // Step 26.6b (#586) / #715 PR2: the scheduled update_plan + plan_get tools,
        // only with the gate (plan_set/plan_advance collapsed into update_plan).
        let sched = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, true,
        );
        for t in ["update_plan", "plan_get"] {
            assert!(names(&sched).contains(&t), "{t} advertised with_scheduled");
            assert!(!names(&without).contains(&t), "{t} hidden without the gate");
            assert!(
                !is_hallucination(t, &serde_json::json!({})),
                "{t} is a real tool"
            );
        }
    }

    #[tokio::test]
    async fn state_tools_dispatch_only_with_a_store() {
        use crate::agentic::scratchpad::{ScratchpadStore, SessionScratchpadStore};
        let caveats = crate::caveats::Caveats::top();
        let args = serde_json::json!({ "key": "k", "value": "v" });
        // Step 26.4: without a store the tool was never advertised → unknown.
        let none = execute_tool(
            "state_set",
            &args,
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(none.starts_with("unknown tool: state_set"), "{none}");
        // With a store → routes to the executor and mutates it.
        let store = SessionScratchpadStore::default();
        let set = execute_tool(
            "state_set",
            &args,
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&store as &dyn ScratchpadStore),
            None,
            None,
            None,
        )
        .await;
        assert_eq!(set, "stored: k");
        assert_eq!(store.get("k").as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn code_search_dispatch_only_with_a_searcher() {
        use crate::agentic::semantic::{CodeSearch, Embedder, SessionSemanticIndex};
        struct E;
        #[async_trait::async_trait]
        impl Embedder for E {
            async fn embed(&self, _t: &str) -> anyhow::Result<Vec<f32>> {
                Ok(vec![1.0])
            }
        }
        let caveats = crate::caveats::Caveats::top();
        let args = serde_json::json!({ "query": "find it" });
        // Step 26.5.5: no searcher → unknown tool (presence-gate parity).
        let none = execute_tool(
            "code_search",
            &args,
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(none.starts_with("unknown tool: code_search"), "{none}");
        // with a searcher (empty index) → routes to the executor (labelled no-match).
        let idx = SessionSemanticIndex::default();
        let search = CodeSearch {
            embedder: &E,
            index: &idx,
            top_k: 1,
        };
        let out = execute_tool(
            "code_search",
            &args,
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(search),
            None,
            None,
        )
        .await;
        assert!(out.contains("no code matched"), "{out}");
    }

    #[tokio::test]
    async fn experiential_dispatch_only_with_a_store() {
        use crate::agentic::experiential::{ExperienceStore, SessionExperienceStore};
        let caveats = crate::caveats::Caveats::top();
        let args = serde_json::json!({
            "task": "ci flake", "outcome": "fixed", "lesson": "pin the seed for the fuzz test"
        });
        // Step 26.6a: no store → unknown tool for BOTH arms (presence-gate parity).
        for name in ["experience_record", "experience_recall"] {
            let out = execute_tool(
                name, &args, ".", false, 20, &caveats, &mut NoMcp, None, None, None, None, None,
                None, None, None, None, None, None, None,
            )
            .await;
            assert!(out.starts_with(&format!("unknown tool: {name}")), "{out}");
        }
        // with a store → record routes to the executor and mutates it.
        let store = SessionExperienceStore::default();
        let out = execute_tool(
            "experience_record",
            &args,
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&store as &dyn ExperienceStore),
            None,
        )
        .await;
        assert_eq!(out, "recorded experience");
        assert_eq!(store.count(), 1);
    }

    #[tokio::test]
    async fn scheduled_dispatch_only_with_a_ledger() {
        use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
        let caveats = crate::caveats::Caveats::top();
        let args = serde_json::json!({ "plan": [
            { "step": "a", "status": "in_progress" },
            { "step": "b", "status": "pending" },
        ] });
        // Step 26.6b / #716 / #715 PR2: no ledger → unknown tool for ALL plan arms
        // (presence-gate parity, including the read-only plan_get).
        for name in ["update_plan", "plan_get"] {
            let out = execute_tool(
                name, &args, ".", false, 20, &caveats, &mut NoMcp, None, None, None, None, None,
                None, None, None, None, None, None, None,
            )
            .await;
            assert!(out.starts_with(&format!("unknown tool: {name}")), "{out}");
        }
        // with a ledger → update_plan routes to the executor and mutates it.
        let ledger = SessionStepLedger::default();
        let out = execute_tool(
            "update_plan",
            &args,
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&ledger as &dyn StepLedger),
        )
        .await;
        assert!(out.starts_with("<plan>\n"), "{out}");
        assert_eq!(ledger.count(), 2);
        // #716: plan_get with a ledger renders the <plan> block, read-only.
        let got = execute_tool(
            "plan_get",
            &serde_json::json!({}),
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&ledger as &dyn StepLedger),
        )
        .await;
        assert!(got.starts_with("<plan>\n"), "{got}");
        assert_eq!(ledger.count(), 2, "plan_get is read-only");
    }

    #[tokio::test]
    async fn resume_context_dispatch_degrades_without_a_recall_source() {
        // #714: advertised ALWAYS, so dispatch never reports "unknown tool" —
        // with no recall_source (headless) it returns the clear no-history line.
        let caveats = crate::caveats::Caveats::top();
        let out = execute_tool(
            "resume_context",
            &serde_json::json!({}),
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None, // build_check_cmd
            None, // note_sink
            None, // recall_source
            None, // memory_source
            None, // permission_gate
            None, // exec_floor
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert!(
            out.contains("no conversation history available this session"),
            "{out}"
        );
        assert!(!out.starts_with("unknown tool"), "{out}");
    }

    #[test]
    fn run_build_check_reports_pass_fail_and_spawn_error() {
        let ws = tempfile::TempDir::new().unwrap();
        let ws_str = ws.path().to_string_lossy();
        assert_eq!(
            run_build_check(passing_build_check_cmd(), &ws_str),
            "  ✓ build check passed"
        );
        let failed = run_build_check(&failing_build_check_cmd("boom"), &ws_str);
        assert!(failed.contains("✗ build check failed"), "got: {failed}");
        assert!(failed.contains("boom"), "stderr excerpt shown: {failed}");
        // A nonexistent workspace dir → the command can't even spawn.
        let err = run_build_check(passing_build_check_cmd(), "/definitely/not/a/dir");
        assert!(err.contains("⚠ build check could not run"), "got: {err}");
    }
}

// ---------------------------------------------------------------------------
// execute_tool branch tests — edit_file / shrink guard / denial paths
// ---------------------------------------------------------------------------

#[cfg(test)]
mod execute_tool_branch_tests {
    use super::super::NoMcp;
    use super::*;
    use crate::agentic::{
        ArtifactReadContext, ArtifactReadRecord, PromptArtifactSink, SessionArtifactStore,
    };
    use crate::artifact::{ArtifactId, ArtifactKind, ArtifactRelation, NewPromptArtifact};
    use crate::caveats::{Caveats, CountBound, Scope};
    use crate::PromptId;
    use std::sync::Mutex;

    /// fs read everywhere, fs write scoped to the workspace (skips the y/N
    /// confirm — the scoped preset is the consent), nothing else.
    fn caveats_rw(ws: &std::path::Path) -> Caveats {
        Caveats {
            fs_read: Scope::All,
            fs_write: Scope::only([ws.to_string_lossy().into_owned()]),
            exec: Scope::none(),
            net: Scope::none(),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        }
    }

    #[derive(Default)]
    struct RecordingArtifactSink {
        artifacts: Mutex<Vec<NewPromptArtifact>>,
    }

    impl RecordingArtifactSink {
        fn only_artifact(&self) -> NewPromptArtifact {
            let artifacts = self.artifacts.lock().unwrap();
            assert_eq!(artifacts.len(), 1, "expected exactly one artifact");
            artifacts[0].clone()
        }

        #[cfg(unix)]
        fn is_empty(&self) -> bool {
            self.artifacts.lock().unwrap().is_empty()
        }
    }

    impl PromptArtifactSink for RecordingArtifactSink {
        fn append_artifact(
            &self,
            originating_prompt_id: PromptId,
            objective_root_id: PromptId,
            artifact: NewPromptArtifact,
        ) -> anyhow::Result<ArtifactReadRecord> {
            let mut artifacts = self.artifacts.lock().unwrap();
            artifacts.push(artifact.clone());
            Ok(ArtifactReadRecord {
                id: ArtifactId::new(),
                prompt_id: originating_prompt_id,
                root_prompt_id: objective_root_id,
                writer_fingerprint: "tool-test".to_string(),
                seq: artifacts.len() as u64,
                prev_hash: "previous".to_string(),
                kind: format!("{:?}", artifact.kind()),
                relation: format!("{:?}", artifact.relation()),
                locator: artifact.locator().map(str::to_string),
                body: artifact.body().map(str::to_string),
                metadata: artifact.metadata().clone(),
                ts_claim: 1,
                artifact_hash: "hash".to_string(),
            })
        }
    }

    fn artifact_context() -> ArtifactReadContext<'static> {
        let prompt = PromptId::new();
        ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_artifact_tool(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
        build_check: Option<&str>,
        sink: &RecordingArtifactSink,
    ) -> String {
        execute_tool_with_offload_and_prompt_and_artifacts(
            name,
            &args,
            &ws.to_string_lossy(),
            false,
            20,
            caveats,
            &mut NoMcp,
            build_check,
            None, // note_sink
            None, // recall_source
            None, // memory_source
            None, // prompt_context
            Some(artifact_context()),
            Some(sink),
            None,  // permission_gate
            None,  // exec_floor
            None,  // git_tool
            None,  // crew_runner
            None,  // scratchpad_store
            None,  // code_search
            None,  // experience_store
            None,  // step_ledger
            false, // tool_offload
            None,  // spill_store
            None,  // persona_tools
            PromptDisposition::Act,
        )
        .await
    }

    // --- PR4: the `git` tool arm in execute_tool ---------------------------

    /// A stub GitTool: echoes the op, and refuses `commit` when the projected
    /// GitCaveats deny it — exercises the arm's caveat projection without a repo.
    struct StubGit;
    impl crate::agentic::GitTool for StubGit {
        fn dispatch(
            &self,
            op: &str,
            _args: &serde_json::Value,
            caps: &crate::git_caveats::GitCaveats,
        ) -> Result<String, String> {
            match op {
                "status" => Ok("on branch main (HEAD abc123)".to_string()),
                "commit" if !caps.permits_commit() => {
                    Err("capability denied: git commit not permitted".to_string())
                }
                "commit" => Ok("committed abc123: msg".to_string()),
                // #1191: data-loss ops the gate guards — if we reach here, the
                // gate ALLOWED (the refusal path returns before dispatch).
                "stash-drop" => Ok("dropped stash@{0}".to_string()),
                "branch-delete" => Ok("deleted branch feature".to_string()),
                other => Err(format!("unknown git op '{other}'")),
            }
        }
    }

    async fn run_scheduled_tool(
        name: &str,
        ws: &tempfile::TempDir,
        ledger: &crate::agentic::scheduled::SessionStepLedger,
    ) -> String {
        execute_tool(
            name,
            &serde_json::json!({}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats_rw(ws.path()),
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(ledger as &dyn crate::agentic::scheduled::StepLedger),
        )
        .await
    }

    async fn run_git(
        op: &str,
        caveats: &Caveats,
        git: Option<&dyn crate::agentic::GitTool>,
    ) -> String {
        let ws = tempfile::TempDir::new().unwrap();
        execute_tool(
            "git",
            &serde_json::json!({ "op": op }),
            &ws.path().to_string_lossy(),
            false,
            20,
            caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            git,
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await
    }

    #[tokio::test]
    async fn git_arm_dispatches_when_injected() {
        let ws = tempfile::TempDir::new().unwrap();
        let out = run_git("status", &caveats_rw(ws.path()), Some(&StubGit)).await;
        assert!(out.contains("on branch main"), "got: {out}");
    }

    #[tokio::test]
    async fn git_arm_surfaces_denials_from_projected_caveats() {
        // A session with no fs_write → from_session denies commit_local.
        let ws = tempfile::TempDir::new().unwrap();
        let read_only = Caveats {
            fs_write: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let out = run_git("commit", &read_only, Some(&StubGit)).await;
        assert!(
            out.contains("error:") && out.contains("commit"),
            "got: {out}"
        );
        // The same session can still run a read op.
        let out = run_git("status", &read_only, Some(&StubGit)).await;
        assert!(out.contains("on branch main"), "got: {out}");
    }

    /// Same as `run_git` but with a gate AND the git tool both injected — the
    /// #1056 path where a denied git write consults the operator.
    #[allow(clippy::too_many_arguments)]
    async fn run_git_gated(
        op: &str,
        caveats: &Caveats,
        git: &dyn crate::agentic::GitTool,
        gate: &mut MockGate,
    ) -> String {
        let ws = tempfile::TempDir::new().unwrap();
        execute_tool(
            "git",
            &serde_json::json!({ "op": op }),
            &ws.path().to_string_lossy(),
            false,
            20,
            caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            Some(gate), // permission_gate
            None,       // exec_floor
            Some(git),  // git_tool
            None,       // crew_runner
            None,       // scratchpad_store
            None,       // code_search
            None,       // experience_store
            None,       // step_ledger
        )
        .await
    }

    /// #1056: a git WRITE the projected authority denies is no longer a dead end
    /// — with a gate that ALLOWS, the arm re-dispatches under the local-write
    /// surface and the commit lands (the deadlock fix). The gate is consulted for
    /// a `git_write` capability.
    #[test]
    fn plan_phase_seam_and_clamp() {
        use crate::caveats::ScopeExt as _;
        // The clamp is read-only: reads yes, writes/exec/net no.
        let c = plan_phase_clamp();
        assert!(c.fs_read.permits(&"/anything".to_string()));
        assert!(!c.fs_write.permits(&"/anything".to_string()));
        assert!(!c.exec.permits(&"cargo".to_string()));
        assert!(!c.net.permits(&"github.com".to_string()));
        // MEETing it into a full grant yields read-only (never widens).
        let full = crate::caveats::Caveats::top();
        let planned = full.meet(&c);
        assert!(
            !planned.fs_write.permits(&"/x".to_string()),
            "writes denied in plan phase"
        );
        assert!(planned.fs_read.permits(&"/x".to_string()), "reads allowed");
    }

    #[tokio::test]
    async fn enter_and_exit_plan_mode_toggle_the_phase_flag() {
        // enter_plan_mode / exit_plan_mode are REAL tools that flip the
        // read-only-phase env the TUI reads when clamping caveats (#1193).
        let ws = tempfile::TempDir::new().unwrap();
        // Ensure a clean starting state.
        // SAFETY: single-threaded test.
        unsafe { std::env::remove_var("NEWT_PLAN_PHASE") };
        assert!(!in_plan_phase());
        let ledger = crate::agentic::scheduled::SessionStepLedger::default();
        let enter = run_scheduled_tool("enter_plan_mode", &ws, &ledger).await;
        assert!(enter.contains("PLAN MODE"), "{enter}");
        assert!(in_plan_phase(), "enter_plan_mode set the phase");
        let exit = run_scheduled_tool("exit_plan_mode", &ws, &ledger).await;
        assert!(exit.contains("exited PLAN MODE"), "{exit}");
        assert!(!in_plan_phase(), "exit_plan_mode cleared the phase");
    }

    #[tokio::test]
    async fn git_data_loss_ops_are_gated_even_under_full_write_authority() {
        // #1191: the exact catastrophe — a confused model tries to destroy
        // work (stash-drop / branch-delete). Even with FULL write authority
        // (the --full-access analogue), the op is refused without an explicit
        // operator confirmation, and proceeds only WITH it. Safe ops never
        // consult the data-loss gate.
        let ws = tempfile::TempDir::new().unwrap();
        let full = caveats_rw(ws.path());

        // Gate DECLINES → refused, StubGit never dropped the stash.
        let mut deny = MockGate::new(false, &full);
        let out = run_git_gated("stash-drop", &full, &StubGit, &mut deny).await;
        assert!(
            out.starts_with("refused:"),
            "must refuse without confirm: {out}"
        );
        assert!(
            !out.contains("dropped stash"),
            "the drop must NOT have run: {out}"
        );
        assert!(
            deny.asks
                .iter()
                .any(|(t, k)| t == "git" && k.contains("stash-drop")),
            "the data-loss confirmation was asked: {:?}",
            deny.asks
        );

        // Gate ALLOWS → proceeds.
        let mut allow = MockGate::new(true, &full);
        let out = run_git_gated("branch-delete", &full, &StubGit, &mut allow).await;
        assert!(
            out.contains("deleted branch"),
            "confirmed → proceeds: {out}"
        );

        // A SAFE op is never gated as data-loss.
        let mut g = MockGate::new(false, &full);
        let out = run_git_gated("status", &full, &StubGit, &mut g).await;
        assert!(out.contains("on branch main"), "safe op runs: {out}");
        assert!(
            !g.asks
                .iter()
                .any(|(_, k)| k.contains("stash-drop") || k.contains("branch-delete")),
            "status must not trip the data-loss gate: {:?}",
            g.asks
        );
    }

    #[tokio::test]
    async fn git_data_loss_op_refused_headless_no_gate() {
        // No permission gate (headless) → a data-loss op is refused, never run.
        let ws = tempfile::TempDir::new().unwrap();
        let out = run_git("stash-drop", &caveats_rw(ws.path()), Some(&StubGit)).await;
        assert!(
            out.starts_with("refused:"),
            "headless data-loss refused: {out}"
        );
    }

    #[tokio::test]
    async fn git_write_denial_routes_through_gate_and_commits_on_allow() {
        let ws = tempfile::TempDir::new().unwrap();
        let read_only = Caveats {
            fs_write: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = MockGate::new(true, &read_only);
        let out = run_git_gated("commit", &read_only, &StubGit, &mut gate).await;
        assert!(
            out.contains("committed"),
            "gate-granted commit should land: {out}"
        );
        assert!(
            gate.asks
                .iter()
                .any(|(t, k)| t == "git" && k.starts_with("git_write:commit")),
            "a git_write grant was requested: {:?}",
            gate.asks
        );
    }

    /// Deny-by-default invariant: a gate that DECLINES keeps the git write denied.
    #[tokio::test]
    async fn git_write_denied_when_operator_declines() {
        let ws = tempfile::TempDir::new().unwrap();
        let read_only = Caveats {
            fs_write: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = MockGate::new(false, &read_only);
        let out = run_git_gated("commit", &read_only, &StubGit, &mut gate).await;
        assert!(
            out.contains("capability denied: git commit not permitted"),
            "a declined git write stays denied: {out}"
        );
    }

    /// #1056: a git READ is never gated — the arm only routes WRITE denials, so a
    /// read op never even consults the gate.
    #[tokio::test]
    async fn git_read_is_never_gated() {
        let ws = tempfile::TempDir::new().unwrap();
        let read_only = Caveats {
            fs_write: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = MockGate::new(false, &read_only);
        let out = run_git_gated("status", &read_only, &StubGit, &mut gate).await;
        assert!(
            out.contains("on branch main"),
            "read op runs ungated: {out}"
        );
        assert!(gate.asks.is_empty(), "a read must not consult the gate");
    }

    #[tokio::test]
    async fn git_arm_unknown_op_is_an_error_not_a_panic() {
        let ws = tempfile::TempDir::new().unwrap();
        let out = run_git("frobnicate", &caveats_rw(ws.path()), Some(&StubGit)).await;
        assert!(
            out.contains("error:") && out.contains("unknown git op"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn git_arm_without_injection_is_unknown_tool() {
        let ws = tempfile::TempDir::new().unwrap();
        let out = run_git("status", &caveats_rw(ws.path()), None).await;
        assert!(out.contains("unknown tool: git"), "got: {out}");
    }

    // #479: the agent-callable crew/compose_roster tools route through the
    // injected CrewRunner — same presence-gating + dispatch shape as `git`.
    struct StubCrew;
    #[async_trait::async_trait]
    impl crate::agentic::CrewRunner for StubCrew {
        async fn dispatch(
            &self,
            op: &str,
            _args: &serde_json::Value,
            _caveats: &Caveats,
        ) -> Result<String, String> {
            match op {
                "compose_roster" => Ok("proposed roster: planner <- qwen3-coder:30b".to_string()),
                "crew" => Ok("crew ran: diff +1/-0, status PASS".to_string()),
                other => Err(format!("unknown op: {other}")),
            }
        }
    }

    async fn run_crew_tool(
        name: &str,
        args: serde_json::Value,
        crew: Option<&dyn crate::agentic::CrewRunner>,
    ) -> String {
        let ws = tempfile::TempDir::new().unwrap();
        execute_tool(
            name,
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats_rw(ws.path()),
            &mut NoMcp,
            None, // build_check_cmd
            None, // note_sink
            None, // recall_source
            None, // memory_source
            None, // permission_gate
            None, // exec_floor
            None, // git_tool
            crew,
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await
    }

    #[tokio::test]
    async fn crew_arm_dispatches_when_injected() {
        let out = run_crew_tool(
            "crew",
            serde_json::json!({ "task": "do X" }),
            Some(&StubCrew),
        )
        .await;
        assert!(
            out.contains("crew ran") && out.contains("PASS"),
            "got: {out}"
        );
        let out = run_crew_tool(
            "compose_roster",
            serde_json::json!({ "mode": "crew" }),
            Some(&StubCrew),
        )
        .await;
        assert!(out.contains("proposed roster"), "got: {out}");
    }

    /// #479 (G4): with no `CrewRunner` injected (the OFF default), the dispatch
    /// arm coaches recovery instead of the old flat `unknown tool` dead-end — it
    /// names the operator gesture (`NEWT_TEAM`) and a real solo alternative, and
    /// must NOT read as "unknown tool".
    #[tokio::test]
    async fn crew_arm_without_injection_coaches_recovery() {
        for name in ["crew", "compose_roster"] {
            let out = run_crew_tool(name, serde_json::json!({ "task": "x" }), None).await;
            assert!(out.contains("NEWT_TEAM"), "{name}: {out}");
            assert!(out.contains("read_file"), "{name}: {out}");
            assert!(!out.contains("unknown tool"), "{name}: {out}");
        }
    }

    /// #479 (G4): the factored coach helper names the gate + a real alternative
    /// and never reads as "unknown tool" — the regression point for the wording.
    #[test]
    fn crew_off_recovery_result_names_gate_and_alternative() {
        let out = crew_off_recovery_result("crew");
        assert!(out.contains("'crew'"), "{out}");
        assert!(out.contains("NEWT_TEAM"), "{out}");
        // A real, always-available solo alternative is offered.
        assert!(out.contains("write_file"), "{out}");
        assert!(!out.contains("unknown tool"), "{out}");
    }

    /// #479 (G4): the gated-off telemetry seam. A `crew`/`compose_roster` reach
    /// with the surface OFF records a `GatedOff` phantom; the same names with the
    /// surface ON record nothing (they dispatch normally), and a non-crew name is
    /// never gated-off.
    #[test]
    fn classify_gated_off_reach_only_fires_for_off_crew_names() {
        for name in ["crew", "compose_roster"] {
            assert_eq!(
                classify_gated_off_reach(name, false),
                Some(crate::PhantomResolution::GatedOff(
                    "crew/team surface off (NEWT_TEAM)".into()
                )),
                "{name} OFF should record GatedOff"
            );
            assert_eq!(
                classify_gated_off_reach(name, true),
                None,
                "{name} ON dispatches normally — no phantom"
            );
        }
        // A non-crew tool is never a gated-off reach, OFF or ON.
        assert_eq!(classify_gated_off_reach("read_file", false), None);
        assert_eq!(classify_gated_off_reach("read_file", true), None);
    }

    /// #479 (G4) guard: the OFF-state changes do not touch `is_hallucination`
    /// (crew/compose_roster stay real names) or `classify_phantom_reach` for the
    /// crew names — both kept exactly so the ON path stays a normal dispatch.
    #[test]
    fn crew_names_stay_real_and_unflagged_by_existing_seams() {
        for name in ["crew", "compose_roster"] {
            assert!(
                !is_hallucination(name, &serde_json::json!({ "task": "x" })),
                "{name} must stay a real tool name"
            );
            assert_eq!(
                classify_phantom_reach(name, &serde_json::json!({ "task": "x" }), "ok", true),
                None,
                "{name} must not be flagged by classify_phantom_reach"
            );
        }
    }

    // --- #496: the embedded `find` tool -----------------------------------

    /// Convenience for `find` calls through the real dispatch under a
    /// read-everything session.
    async fn run_find(args: serde_json::Value, ws: &std::path::Path) -> String {
        run_tool("find", args, ws, &caveats_rw(ws), None).await
    }

    fn touch(root: &std::path::Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    /// Regression for #496: an agent needed `find . -name pyo3_module.rs` but
    /// the build's shell tool was unavailable. The embedded tool must locate the
    /// file by basename, ignoring decoys, and return its workspace-relative path
    /// (no shell, no `| sort`). Fails before this tool existed (`unknown tool:
    /// find`).
    #[tokio::test]
    async fn find_locates_file_by_name_issue_496() {
        let ws = tempfile::TempDir::new().unwrap();
        touch(ws.path(), "newt-core/src/pyo3_module.rs");
        touch(ws.path(), "newt-data/src/other.rs");
        touch(ws.path(), "docs/pyo3_module.md"); // decoy: wrong extension
        let out = run_find(serde_json::json!({ "name": "pyo3_module.rs" }), ws.path()).await;
        assert_eq!(out, "newt-core/src/pyo3_module.rs", "got: {out}");
    }

    /// The other call the blocked agent reached for:
    /// `find examples -maxdepth 2 -type f -name '*.py'`. Exercises glob + type
    /// filter + max_depth together, and confirms output is pre-sorted.
    #[tokio::test]
    async fn find_glob_type_and_maxdepth_together() {
        let ws = tempfile::TempDir::new().unwrap();
        touch(ws.path(), "examples/a.py"); // depth 1 — match
        touch(ws.path(), "examples/sub/b.py"); // depth 2 — match
        touch(ws.path(), "examples/sub/deep/c.py"); // depth 3 — too deep
        touch(ws.path(), "examples/readme.md"); // wrong extension
        std::fs::create_dir_all(ws.path().join("examples/empty_dir")).unwrap();
        let out = run_find(
            serde_json::json!({
                "path": "examples", "name": "*.py", "type": "f", "max_depth": 2
            }),
            ws.path(),
        )
        .await;
        // Pre-sorted, exactly the two in-depth .py files, no dir, no .md, no
        // depth-3 file — and no shell `| sort` needed.
        assert_eq!(out, "examples/a.py\nexamples/sub/b.py", "got: {out}");
    }

    /// Output is sorted ascending regardless of filesystem/creation order.
    #[tokio::test]
    async fn find_output_is_sorted() {
        let ws = tempfile::TempDir::new().unwrap();
        for f in ["m.txt", "a.txt", "z.txt", "c.txt"] {
            touch(ws.path(), f);
        }
        let out = run_find(serde_json::json!({ "name": "*.txt" }), ws.path()).await;
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            vec!["a.txt", "c.txt", "m.txt", "z.txt"],
            "got: {out}"
        );
    }

    /// #1235: every tool invocation goes through one display boundary. The
    /// operator sees the command plus a bounded tail, while the model-facing
    /// result remains complete.
    #[tokio::test]
    async fn find_command_and_full_result_share_the_spill_boundary() {
        let ws = tempfile::TempDir::new().unwrap();
        for f in ["e.rs", "b.rs", "d.rs", "a.rs", "c.rs"] {
            touch(ws.path(), f);
        }
        let args = serde_json::json!({
            "path": ".",
            "name": "*.rs",
            "type": "f",
        });
        let caveats = caveats_rw(ws.path());
        let (out, rendered) =
            run_tool_captured("find", args, ws.path(), &caveats, &mut NoMcp).await;

        assert_eq!(out, "a.rs\nb.rs\nc.rs\nd.rs\ne.rs");
        assert_eq!(
            rendered,
            "⚙  find: . (name=*.rs, type=f)\n\
             ▲ 2 more lines above\n\
             ▒ c.rs\n\
             ▒ d.rs\n\
             ▓ e.rs\n\
             …\n"
        );
    }

    #[tokio::test]
    async fn routed_find_uses_the_governed_tool_in_the_audit_header() {
        let ws = tempfile::TempDir::new().unwrap();
        for f in ["b.rs", "a.rs"] {
            touch(ws.path(), f);
        }
        let caveats = caveats_rw(ws.path());
        let (out, rendered) = run_tool_captured(
            "run_command",
            serde_json::json!({"command": "find . -name '*.rs' -type f"}),
            ws.path(),
            &caveats,
            &mut NoMcp,
        )
        .await;

        assert_eq!(out, "a.rs\nb.rs");
        assert!(
            rendered.starts_with("⚙  find: . (name=*.rs, type=f)\n"),
            "routed action was not audited canonically: {rendered}"
        );
        assert!(!rendered.contains("⚙  run_command:"));
    }

    #[tokio::test]
    async fn correction_alias_header_never_echoes_file_content() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let secret = "PRIVATE_BODY_MUST_NOT_APPEAR_IN_HEADER";
        let (out, rendered) = run_tool_captured(
            "create_file",
            serde_json::json!({"path": "secret.txt", "content": secret}),
            ws.path(),
            &caveats,
            &mut NoMcp,
        )
        .await;

        assert!(out.contains("write_file"), "got: {out}");
        assert!(
            rendered.starts_with(&format!(
                "⚙  create_file: secret.txt ({} bytes)\n",
                secret.len()
            )),
            "unsafe or unhelpful alias audit: {rendered}"
        );
        assert!(!rendered.contains(secret));
    }

    #[test]
    fn lifecycle_audit_names_the_resolved_command() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(
            ws.path().join("Cargo.toml"),
            "[package]\nname='audit-fixture'\n",
        )
        .unwrap();
        let (name, detail) = tool_presentation(
            "lifecycle",
            &serde_json::json!({"phase": "test", "action": "run"}),
            ws.path(),
        );
        let resolved =
            crate::tooling::resolved_phase_commands(ws.path(), crate::tooling::Phase::Test);

        assert_eq!(name, "lifecycle");
        assert!(!resolved.is_empty());
        assert_eq!(detail, format!("test (run) → {}", resolved.join(" && ")));
    }

    #[test]
    fn audit_preserves_whitespace_in_real_paths() {
        let ws = tempfile::TempDir::new().unwrap();
        let (name, detail) = tool_presentation(
            "read_file",
            &serde_json::json!({"path": " leading and trailing "}),
            ws.path(),
        );

        assert_eq!(name, "read_file");
        assert_eq!(detail, " leading and trailing ");

        let (name, detail) = tool_presentation(
            "run_command",
            &serde_json::json!({"command": "cd nested && printf exact-command"}),
            ws.path(),
        );
        assert_eq!(name, "run_command");
        assert_eq!(detail, "cd nested && printf exact-command");
    }

    #[tokio::test]
    async fn find_error_uses_the_same_spill_boundary() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let (out, rendered) = run_tool_captured(
            "find",
            serde_json::json!({"path": "missing", "name": "*.rs", "type": "f"}),
            ws.path(),
            &caveats,
            &mut NoMcp,
        )
        .await;

        assert_eq!(out, "error: no such path 'missing'");
        assert_eq!(
            rendered,
            "⚙  find: missing (name=*.rs, type=f)\n\
             ▒ error: no such path 'missing'\n\
             …\n"
        );
    }

    struct EmptyRemote;

    #[async_trait::async_trait]
    impl McpTools for EmptyRemote {
        fn handles(&self, name: &str) -> bool {
            name == "test__empty"
        }

        fn tool_defs(&self) -> Vec<serde_json::Value> {
            Vec::new()
        }

        async fn call(&mut self, _name: &str, _args: &serde_json::Value) -> String {
            String::new()
        }
    }

    #[tokio::test]
    async fn empty_tool_result_still_commits_a_complete_spill_block() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let (out, rendered) = run_tool_captured(
            "test__empty",
            serde_json::json!({}),
            ws.path(),
            &caveats,
            &mut EmptyRemote,
        )
        .await;

        assert!(out.is_empty());
        assert_eq!(
            rendered,
            "⚙  test__empty: {}\n\
             ▒ (no output)\n\
             …\n"
        );
    }

    #[tokio::test]
    async fn unknown_tool_has_exactly_one_complete_audit_block() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let (out, rendered) = run_tool_captured(
            "definitely_unknown",
            serde_json::json!({}),
            ws.path(),
            &caveats,
            &mut NoMcp,
        )
        .await;

        assert!(out.contains("unknown tool"), "got: {out}");
        assert_eq!(rendered.matches("⚙  definitely_unknown:").count(), 1);
        assert_eq!(rendered.matches("…\n").count(), 1);
        assert_eq!(rendered.matches("▒ unknown tool:").count(), 1);
    }

    #[tokio::test]
    async fn pre_set_cancellation_closes_the_block_without_polling_a_mutation() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let args = serde_json::json!({"path": "must-not-exist.txt", "content": "blocked"});
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3);

        let out = execute_tool_with_display_cancellable(
            &mut display,
            "write_file",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            None,
            None,
            PromptDisposition::Act,
            None,
            Some(&cancel),
        )
        .await;

        assert!(out.is_none());
        assert!(!ws.path().join("must-not-exist.txt").exists());
        assert_eq!(
            String::from_utf8(display.into_inner()).unwrap(),
            "⚙  write_file: must-not-exist.txt (7 bytes)\n\
             ▒ error: write_file interrupted — tool cancelled before completion\n\
             …\n"
        );
    }

    #[tokio::test]
    async fn prompt_read_central_display_never_echoes_recovered_prompt_text() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let exact = "operator secret that must reach only the model";
        let context = PromptReadContext::new(None, exact, None);
        let (out, rendered) = run_tool_captured_with_context(
            "prompt_read",
            serde_json::json!({}),
            ws.path(),
            &caveats,
            &mut NoMcp,
            Some(context),
            None,
        )
        .await;

        let model: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(model["model_text"], exact);
        assert_eq!(rendered.matches("⚙  prompt_read:").count(), 1);
        assert!(rendered.contains("ephemeral prompt: returned"));
        assert!(!rendered.contains(exact));
    }

    #[tokio::test]
    async fn artifact_read_central_display_never_echoes_recovered_body() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let prompt = PromptId::new();
        let secret = "artifact body that must reach only the model";
        let store = SessionArtifactStore::new("central-display-test").unwrap();
        let record = store
            .append_artifact(
                prompt,
                prompt,
                NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom)
                    .with_body(secret),
            )
            .unwrap();
        let context =
            ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&store));
        let (out, rendered) = run_tool_captured_with_context(
            "artifact_read",
            serde_json::json!({"address": record.id.to_string()}),
            ws.path(),
            &caveats,
            &mut NoMcp,
            None,
            Some(context),
        )
        .await;

        let model: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(model["artifact"]["body"], secret);
        assert_eq!(rendered.matches("⚙  artifact_read:").count(), 1);
        assert!(rendered.contains(&format!(
            "returned {} of {} body characters",
            secret.chars().count(),
            secret.chars().count()
        )));
        assert!(!rendered.contains(secret));
    }

    #[tokio::test]
    async fn render_report_has_one_header_document_and_ack_block() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let (out, rendered) = run_tool_captured(
            "render_report",
            serde_json::json!({
                "title": "Build status",
                "body": "All required checks passed."
            }),
            ws.path(),
            &caveats,
            &mut NoMcp,
        )
        .await;

        assert!(out.starts_with("report rendered:"), "got: {out}");
        assert_eq!(rendered.matches("⚙  render_report:").count(), 1);
        assert_eq!(rendered.matches("All required checks passed.").count(), 1);
        assert_eq!(rendered.matches("▒ report rendered:").count(), 1);
    }

    /// `type` restricts to files or directories.
    #[tokio::test]
    async fn find_type_filter() {
        let ws = tempfile::TempDir::new().unwrap();
        touch(ws.path(), "pkg/file.rs");
        std::fs::create_dir_all(ws.path().join("pkg/sub")).unwrap();
        let dirs = run_find(serde_json::json!({ "type": "d" }), ws.path()).await;
        assert!(
            dirs.contains("pkg") && dirs.contains("pkg/sub"),
            "got: {dirs}"
        );
        assert!(!dirs.contains("file.rs"), "dirs-only leaked a file: {dirs}");
        let files = run_find(serde_json::json!({ "type": "f" }), ws.path()).await;
        assert!(files.contains("pkg/file.rs"), "got: {files}");
        assert!(
            !files.lines().any(|l| l == "pkg" || l == "pkg/sub"),
            "files-only leaked a dir: {files}"
        );
    }

    /// .gitignore + the default build/dep skips are honoured by default and
    /// can be disabled with `respect_gitignore=false`.
    #[tokio::test]
    async fn find_gitignore_and_default_skips() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join(".gitignore"), "ignored.txt\n").unwrap();
        touch(ws.path(), "kept.txt");
        touch(ws.path(), "ignored.txt");
        touch(ws.path(), "target/build_artifact.txt");
        touch(ws.path(), "node_modules/dep.txt");

        let on = run_find(serde_json::json!({ "name": "*.txt" }), ws.path()).await;
        assert!(on.contains("kept.txt"), "got: {on}");
        assert!(!on.contains("ignored.txt"), "gitignore not honoured: {on}");
        assert!(!on.contains("target/"), "target not skipped: {on}");
        assert!(
            !on.contains("node_modules/"),
            "node_modules not skipped: {on}"
        );

        let off = run_find(
            serde_json::json!({ "name": "*.txt", "respect_gitignore": false }),
            ws.path(),
        )
        .await;
        assert!(off.contains("ignored.txt"), "opt-out should show it: {off}");
        assert!(off.contains("target/build_artifact.txt"), "got: {off}");
    }

    /// `max_results` caps output and the result notes the truncation.
    #[tokio::test]
    async fn find_max_results_caps_and_notes_truncation() {
        let ws = tempfile::TempDir::new().unwrap();
        for i in 0..10 {
            touch(ws.path(), &format!("f{i}.txt"));
        }
        let out = run_find(
            serde_json::json!({ "name": "*.txt", "max_results": 3 }),
            ws.path(),
        )
        .await;
        let body: Vec<&str> = out.lines().filter(|l| l.ends_with(".txt")).collect();
        assert_eq!(body.len(), 3, "should cap at 3: {out}");
        assert!(out.contains("truncated at 3"), "got: {out}");
    }

    /// A missing root is a clear error, and an empty match set says so.
    #[tokio::test]
    async fn find_missing_root_and_no_matches() {
        let ws = tempfile::TempDir::new().unwrap();
        touch(ws.path(), "a.txt");
        let missing = run_find(serde_json::json!({ "path": "does/not/exist" }), ws.path()).await;
        assert!(missing.starts_with("error:"), "got: {missing}");
        let empty = run_find(serde_json::json!({ "name": "*.nope" }), ws.path()).await;
        assert_eq!(empty, "no matches", "got: {empty}");
    }

    /// fs_read denial: no scope + no prompt gate ⇒ capability denied (same UX
    /// as list_dir/read_file).
    #[tokio::test]
    async fn find_denied_without_fs_read() {
        let ws = tempfile::TempDir::new().unwrap();
        touch(ws.path(), "secret.txt");
        let denied = Caveats {
            fs_read: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let out = run_tool(
            "find",
            serde_json::json!({ "name": "*" }),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert!(out.starts_with("capability denied"), "got: {out}");
    }

    /// A `..` root that escapes the workspace is refused even when the session
    /// grants fs_read everywhere (defence-in-depth for a recursive read).
    #[tokio::test]
    async fn find_refuses_root_outside_workspace() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("outside.txt"), b"x").unwrap();
        let ws = parent.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // fs_read: All, so the only thing that can stop the escape is the
        // canonical-root containment check.
        let out = run_find(serde_json::json!({ "path": ".." }), &ws).await;
        assert!(out.starts_with("capability denied"), "got: {out}");
    }

    /// An empty `name` is treated as "match everything" (the `!g.is_empty()`
    /// guard routes `Some("")` to the no-filter path; without it the glob would
    /// compile to `^$` and match nothing).
    #[tokio::test]
    async fn find_empty_name_matches_everything() {
        let ws = tempfile::TempDir::new().unwrap();
        touch(ws.path(), "a.txt");
        touch(ws.path(), "sub/b.rs");
        let out = run_find(serde_json::json!({ "name": "" }), ws.path()).await;
        for expected in ["a.txt", "sub", "sub/b.rs"] {
            assert!(
                out.lines().any(|l| l == expected),
                "empty name should match `{expected}`: {out}"
            );
        }
    }

    /// Hidden entries (dotfiles / dotdirs) are pruned by default and surface
    /// only when `respect_gitignore=false` — relevant because dotfiles can hold
    /// secrets (.env, .ssh). Pins the `.hidden(respect_gitignore)` branch.
    #[tokio::test]
    async fn find_hidden_entries_gated_by_respect_gitignore() {
        let ws = tempfile::TempDir::new().unwrap();
        touch(ws.path(), "visible.txt");
        touch(ws.path(), ".hidden.txt");
        touch(ws.path(), ".config/secret.txt");

        let default = run_find(serde_json::json!({ "name": "*" }), ws.path()).await;
        assert!(
            default.lines().any(|l| l == "visible.txt"),
            "got: {default}"
        );
        assert!(
            !default.contains(".hidden") && !default.contains(".config"),
            "hidden entries must be skipped by default: {default}"
        );

        let all = run_find(
            serde_json::json!({ "name": "*", "respect_gitignore": false }),
            ws.path(),
        )
        .await;
        assert!(all.contains(".hidden.txt"), "opt-out should show it: {all}");
        assert!(all.contains(".config/secret.txt"), "got: {all}");
    }

    /// Security boundary: `find` never follows symlinked directories, so a link
    /// pointing outside the workspace cannot leak the target's contents (pins
    /// `.follow_links(false)`). Unix-only — Windows symlinks need privileges.
    #[cfg(unix)]
    #[tokio::test]
    async fn find_does_not_follow_symlinks_out_of_workspace() {
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"x").unwrap();
        let ws = tempfile::TempDir::new().unwrap();
        touch(ws.path(), "inside.txt");
        std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

        // The symlink is present but is NOT descended into.
        let leaked = run_find(serde_json::json!({ "name": "secret.txt" }), ws.path()).await;
        assert_eq!(
            leaked, "no matches",
            "symlink was followed out of ws: {leaked}"
        );
        // Sanity: a real in-workspace file is still found.
        let found = run_find(serde_json::json!({ "name": "inside.txt" }), ws.path()).await;
        assert_eq!(found, "inside.txt", "got: {found}");
    }

    #[test]
    fn glob_to_regex_anchors_and_escapes() {
        // '*' is a wildcard; '.' is literal (not "any char").
        let re = glob_to_regex("*.py", true).unwrap();
        assert!(re.is_match("foo.py"));
        assert!(!re.is_match("foo.pyc")); // anchored at end
        assert!(!re.is_match("fooxpy")); // '.' is literal
                                         // Exact basename, '?' = single char, case-sensitivity honoured.
        assert!(glob_to_regex("a?c", true).unwrap().is_match("abc"));
        assert!(!glob_to_regex("a?c", true).unwrap().is_match("ac"));
        assert!(glob_to_regex("readme.md", false)
            .unwrap()
            .is_match("README.MD"));
        assert!(!glob_to_regex("readme.md", true)
            .unwrap()
            .is_match("README.MD"));
    }

    async fn run_tool(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
        build_check: Option<&str>,
    ) -> String {
        execute_tool(
            name,
            &args,
            &ws.to_string_lossy(),
            false,
            20,
            caveats,
            &mut NoMcp,
            build_check,
            None,
            None,
            None, // memory_source
            None,
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await
    }

    async fn run_tool_captured(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
        mcp: &mut dyn McpTools,
    ) -> (String, String) {
        run_tool_captured_with_context(name, args, ws, caveats, mcp, None, None).await
    }

    async fn run_tool_captured_with_context(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
        mcp: &mut dyn McpTools,
        prompt_context: Option<PromptReadContext<'_>>,
        artifact_context: Option<ArtifactReadContext<'_>>,
    ) -> (String, String) {
        run_tool_captured_with_context_and_live(
            name,
            args,
            ws,
            caveats,
            mcp,
            prompt_context,
            artifact_context,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_tool_captured_with_context_and_live(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
        mcp: &mut dyn McpTools,
        prompt_context: Option<PromptReadContext<'_>>,
        artifact_context: Option<ArtifactReadContext<'_>>,
        live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    ) -> (String, String) {
        let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3);
        let out = execute_tool_with_display_cancellable(
            &mut display,
            name,
            &args,
            &ws.to_string_lossy(),
            false,
            20,
            caveats,
            mcp,
            None, // build_check_cmd
            None, // note_sink
            None, // recall_source
            None, // memory_source
            prompt_context,
            artifact_context,
            None, // artifact_sink
            None, // permission_gate
            None, // exec_floor
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
            false,
            None,
            None,
            PromptDisposition::Act,
            live_tool_output,
            None,
        )
        .await
        .expect("uncancelled test dispatch should complete");
        let rendered = String::from_utf8(display.into_inner()).unwrap();
        (out, rendered)
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn live_shell_observation_does_not_change_headless_completion_bytes() {
        #[derive(Default)]
        struct CapturedLiveOutput {
            events: std::sync::Mutex<Vec<String>>,
        }
        impl crate::agentic::LiveToolOutput for CapturedLiveOutput {
            fn start(&self, _generation: u64) {
                self.events.lock().unwrap().push("start".into());
            }
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                chunk: &[u8],
            ) {
                self.events
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(chunk).into_owned());
            }
            fn finish(&self, _generation: u64) {
                self.events.lock().unwrap().push("finish".into());
            }
            fn abandon(&self, _generation: u64) {
                self.events.lock().unwrap().push("abandon".into());
            }
        }

        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats {
            exec: crate::caveats::Scope::only(["echo".to_string()]),
            ..caveats_rw(ws.path())
        };
        let args = serde_json::json!({"command": "echo byte-stable"});
        let (headless_out, headless_rendered) = run_tool_captured_with_context_and_live(
            "run_command",
            args.clone(),
            ws.path(),
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
        )
        .await;
        let sink = std::sync::Arc::new(CapturedLiveOutput::default());
        let (live_out, live_rendered) = run_tool_captured_with_context_and_live(
            "run_command",
            args,
            ws.path(),
            &caveats,
            &mut NoMcp,
            None,
            None,
            Some(sink.clone()),
        )
        .await;

        assert_eq!(live_out, headless_out);
        assert_eq!(live_rendered.as_bytes(), headless_rendered.as_bytes());
        assert!(
            !headless_rendered.as_bytes().contains(&0x1b),
            "headless completion emitted cursor-control bytes: {headless_rendered:?}"
        );
        let events = sink.events.lock().unwrap();
        assert_eq!(events.first().map(String::as_str), Some("start"));
        assert_eq!(events.last().map(String::as_str), Some("finish"));
        assert!(
            events.iter().any(|event| event.contains("byte-stable")),
            "live events: {events:?}; model output: {live_out:?}"
        );
    }

    #[tokio::test]
    async fn edit_file_replaces_unique_match_and_reports_delta() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("f.txt"), "hello world\nsecond line\n").unwrap();
        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "edit_file",
            serde_json::json!({
                "path": "f.txt",
                "old_string": "world",
                "new_string": "rust\nand more"
            }),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.starts_with("edited f.txt (+1 lines"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("f.txt")).unwrap(),
            "hello rust\nand more\nsecond line\n"
        );
    }

    #[tokio::test]
    async fn edit_file_rejects_empty_missing_and_ambiguous_old_string() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("f.txt"), "dup\ndup\n").unwrap();
        let caveats = caveats_rw(ws.path());

        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "f.txt", "old_string": "", "new_string": "x"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.contains("old_string must not be empty"), "got: {out}");

        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "f.txt", "old_string": "absent", "new_string": "x"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.contains("old_string not found in f.txt"), "got: {out}");
        // The miss error now shows the file's actual contents so the model can
        // copy the exact text instead of blind-guessing old_string again.
        assert!(out.contains("do not guess again"), "got: {out}");
        assert!(
            out.contains("dup"),
            "miss error must include the file content: {out}"
        );

        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "f.txt", "old_string": "dup", "new_string": "x"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.contains("matches 2 locations"), "got: {out}");
        // The ambiguous edit must NOT have touched the file.
        assert_eq!(
            std::fs::read_to_string(ws.path().join("f.txt")).unwrap(),
            "dup\ndup\n"
        );
    }

    #[tokio::test]
    async fn edit_file_denied_outside_fs_write_scope_and_missing_file() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats {
            fs_write: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "f.txt", "old_string": "a", "new_string": "b"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(
            out.contains("capability denied: fs_write"),
            "denied before any fs access, got: {out}"
        );

        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "missing.txt", "old_string": "a", "new_string": "b"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.contains("error reading missing.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn edit_file_appends_build_check_result() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("f.txt"), "old\n").unwrap();
        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "f.txt", "old_string": "old", "new_string": "new"}),
            ws.path(),
            &caveats,
            Some(passing_build_check_cmd()),
        )
        .await;
        assert!(out.contains("✓ build check passed"), "got: {out}");

        let failing_check = failing_build_check_cmd("broke");
        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "f.txt", "old_string": "new", "new_string": "newer"}),
            ws.path(),
            &caveats,
            Some(&failing_check),
        )
        .await;
        assert!(out.contains("✗ build check failed"), "got: {out}");
        assert!(out.contains("broke"), "model sees the failure text: {out}");
    }

    #[tokio::test]
    async fn write_file_shrink_guard_refuses_large_deletion() {
        let ws = tempfile::TempDir::new().unwrap();
        let big: String = (0..100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(ws.path().join("big.txt"), &big).unwrap();
        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "write_file",
            serde_json::json!({"path": "big.txt", "content": "tiny\n"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(
            out.contains("would shrink big.txt from 100 → 1 lines"),
            "got: {out}"
        );
        assert!(out.contains("edit_file"), "points at the safer tool: {out}");
        // The guard refused — the original file must be intact.
        assert_eq!(
            std::fs::read_to_string(ws.path().join("big.txt")).unwrap(),
            big
        );
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "write_file",
            serde_json::json!({"path": "a/b/c.txt", "content": "nested"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.starts_with("wrote a/b/c.txt"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("a/b/c.txt")).unwrap(),
            "nested"
        );
    }

    #[tokio::test]
    async fn delete_file_removes_one_file_and_appends_build_check() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("old.rs"), "fn main() {}\n").unwrap();
        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "delete_file",
            serde_json::json!({"path": "old.rs"}),
            ws.path(),
            &caveats,
            Some(passing_build_check_cmd()),
        )
        .await;
        assert!(out.starts_with("deleted old.rs"), "got: {out}");
        assert!(out.contains("✓ build check passed"), "got: {out}");
        assert!(
            !ws.path().join("old.rs").exists(),
            "delete_file must remove the target file"
        );
    }

    #[tokio::test]
    async fn delete_file_records_digest_to_absent_transition() {
        let ws = tempfile::TempDir::new().unwrap();
        let original = b"retired implementation\n";
        std::fs::write(ws.path().join("old.rs"), original).unwrap();
        let sink = RecordingArtifactSink::default();

        let out = run_artifact_tool(
            "delete_file",
            serde_json::json!({"path": "old.rs"}),
            ws.path(),
            &caveats_rw(ws.path()),
            None,
            &sink,
        )
        .await;

        assert!(out.starts_with("deleted old.rs"), "got: {out}");
        let artifact = sink.only_artifact();
        assert_eq!(artifact.kind(), ArtifactKind::FileChange);
        assert_eq!(artifact.locator(), Some("old.rs"));
        assert_eq!(artifact.metadata()["operation"], "delete_file");
        assert_eq!(artifact.metadata()["before"]["available"], true);
        assert_eq!(artifact.metadata()["before"]["exists"], true);
        assert_eq!(
            artifact.metadata()["before"]["digest"],
            blake3::hash(original).to_hex().to_string()
        );
        assert_eq!(artifact.metadata()["after"]["available"], true);
        assert_eq!(artifact.metadata()["after"]["exists"], false);
        assert!(artifact.metadata()["after"]["digest"].is_null());
    }

    #[tokio::test]
    async fn write_only_authority_does_not_record_a_preimage_digest() {
        let ws = tempfile::TempDir::new().unwrap();
        let original = b"secret preimage\n";
        let replacement = b"public result\n";
        std::fs::write(ws.path().join("state.txt"), original).unwrap();
        let caveats = Caveats {
            fs_read: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let sink = RecordingArtifactSink::default();

        let out = run_artifact_tool(
            "write_file",
            serde_json::json!({
                "path": "state.txt",
                "content": std::str::from_utf8(replacement).unwrap(),
            }),
            ws.path(),
            &caveats,
            None,
            &sink,
        )
        .await;

        assert!(out.starts_with("wrote state.txt"), "got: {out}");
        let artifact = sink.only_artifact();
        assert_eq!(artifact.metadata()["before"]["available"], false);
        assert_eq!(
            artifact.metadata()["before"]["reason"],
            "fs_read_not_granted"
        );
        assert!(artifact.metadata()["before"].get("digest").is_none());
        assert_eq!(
            artifact.metadata()["after"]["digest"],
            blake3::hash(replacement).to_hex().to_string()
        );
        assert!(
            !artifact
                .metadata()
                .to_string()
                .contains(&blake3::hash(original).to_hex().to_string()),
            "the preimage digest must not become a persistent read oracle"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn build_check_mutation_is_not_recorded_as_the_governed_write_postimage() {
        let ws = tempfile::TempDir::new().unwrap();
        let governed = b"governed bytes\n";
        let build_hook = b"build-hook bytes\n";
        let sink = RecordingArtifactSink::default();

        let out = run_artifact_tool(
            "write_file",
            serde_json::json!({
                "path": "target.txt",
                "content": std::str::from_utf8(governed).unwrap(),
            }),
            ws.path(),
            &caveats_rw(ws.path()),
            Some("printf 'build-hook bytes\\n' > target.txt"),
            &sink,
        )
        .await;

        assert!(out.contains("build check passed"), "got: {out}");
        assert_eq!(
            std::fs::read(ws.path().join("target.txt")).unwrap(),
            build_hook
        );
        let artifact = sink.only_artifact();
        assert_eq!(artifact.metadata()["operation"], "write_file");
        assert_eq!(
            artifact.metadata()["after"]["digest"],
            blake3::hash(governed).to_hex().to_string(),
            "the artifact must describe the tool's immediate verified write"
        );
        assert_ne!(
            artifact.metadata()["after"]["digest"],
            blake3::hash(build_hook).to_hex().to_string(),
            "a later build hook mutation must not be attributed to write_file"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn physical_symlink_escape_mutates_under_existing_policy_but_emits_no_artifact() {
        let ws = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("target.txt"), "outside before\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();
        let sink = RecordingArtifactSink::default();

        let out = run_artifact_tool(
            "write_file",
            serde_json::json!({
                "path": "link/target.txt",
                "content": "outside after\n",
            }),
            ws.path(),
            &caveats_rw(ws.path()),
            None,
            &sink,
        )
        .await;

        assert!(out.starts_with("wrote link/target.txt"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("target.txt")).unwrap(),
            "outside after\n",
            "this pins the existing lexical mutation policy"
        );
        assert!(
            out.contains("no file-change artifact was recorded")
                && out.contains("physical path could not be proven inside the workspace"),
            "the physical escape must be surfaced honestly: {out}"
        );
        assert!(
            sink.is_empty(),
            "an out-of-workspace mutation must not be claimed as a workspace artifact"
        );
    }

    #[tokio::test]
    async fn delete_file_denies_missing_files_directories_and_fs_write_misses() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
        std::fs::create_dir(ws.path().join("dir")).unwrap();

        let denied = Caveats {
            fs_write: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let out = run_tool(
            "delete_file",
            serde_json::json!({"path": "secret.txt"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert!(out.contains("capability denied: fs_write"), "got: {out}");
        assert!(
            ws.path().join("secret.txt").exists(),
            "denied delete must not remove the file"
        );

        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "delete_file",
            serde_json::json!({"path": "missing.txt"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.contains("file does not exist"), "got: {out}");

        let out = run_tool(
            "delete_file",
            serde_json::json!({"path": "dir"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.contains("refuses directories"), "got: {out}");
        assert!(ws.path().join("dir").is_dir(), "directory must remain");
    }

    #[tokio::test]
    async fn read_file_denial_and_missing_file_errors() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
        let denied = Caveats {
            fs_read: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let out = run_tool(
            "read_file",
            serde_json::json!({"path": "secret.txt"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert!(out.contains("capability denied: fs_read"), "got: {out}");

        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "read_file",
            serde_json::json!({"path": "nope.txt"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.contains("error reading nope.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn list_dir_denial_and_missing_dir_errors() {
        let ws = tempfile::TempDir::new().unwrap();
        let denied = Caveats {
            fs_read: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let out = run_tool(
            "list_dir",
            serde_json::json!({"path": "."}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert!(out.contains("capability denied: fs_read"), "got: {out}");

        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "list_dir",
            serde_json::json!({"path": "not-a-dir"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.starts_with("error:"), "got: {out}");
    }

    #[tokio::test]
    async fn unknown_tool_name_is_reported_not_executed() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "definitely_not_a_tool",
            serde_json::json!({}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        // Step 27.1: the bare "unknown tool: X" is now a corrective message that
        // still leads with the same prefix but also names the real catalog.
        assert!(
            out.starts_with("unknown tool: definitely_not_a_tool"),
            "got: {out}"
        );
        assert!(out.contains("Available tools include:"), "got: {out}");
    }

    // -- Step 27.1: tool-alias resolution + corrective feedback -------------

    #[test]
    fn alias_rewrites_shell_names_to_run_command() {
        for n in [
            "execute",
            "exec",
            "bash",
            "shell",
            "sh",
            "zsh",
            "terminal",
            "run_shell_command",
            "shell_command",
            "system",
        ] {
            assert!(
                matches!(
                    resolve_tool_alias(n),
                    Some(AliasOutcome::Rewrite("run_command"))
                ),
                "{n} should rewrite to run_command"
            );
        }
    }

    #[test]
    fn alias_corrects_edit_and_create_names() {
        for n in [
            "str_replace_editor",
            "str_replace",
            "apply_patch",
            "edit",
            "replace_in_file",
        ] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            assert!(msg.contains("edit_file"), "{n}: {msg}");
            assert!(msg.contains("write_file"), "{n}: {msg}");
        }
        for n in ["create_file", "new_file", "touch"] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            assert!(msg.contains("write_file"), "{n}: {msg}");
        }
        for n in ["remove_file", "delete", "remove", "unlink", "rm_file"] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            assert!(msg.contains("delete_file"), "{n}: {msg}");
            assert!(msg.contains("fs_write"), "{n}: {msg}");
        }
    }

    #[test]
    fn alias_coaches_mkdir_to_write_file() {
        // #721: newt has no directory-creation tool — coach to write_file, which
        // does create_dir_all on the parent. Turns the issue's `mkdir -p …/src`
        // dead-end into a self-correcting tool call.
        for n in [
            "mkdir",
            "make_dir",
            "makedirs",
            "mkdirs",
            "create_dir",
            "create_directory",
        ] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            assert!(msg.contains("write_file"), "{n}: {msg}");
            assert!(msg.contains("create_dir_all"), "{n}: {msg}");
        }
        // `touch` is intentionally NOT in the mkdir arm — it stays a create-file
        // alias (→ write_file), so there is no duplicate match arm / collision.
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias("touch") else {
            panic!("touch should still be a create-file Correct outcome");
        };
        assert!(msg.contains("write_file"), "touch: {msg}");
    }

    #[test]
    fn alias_passes_through_real_and_mcp_names() {
        for n in [
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "delete_file",
            "git",
            "update_plan",
            "plan_get",
            "server__do_thing",
        ] {
            assert!(
                resolve_tool_alias(n).is_none(),
                "{n} must dispatch unchanged"
            );
        }
    }

    // -- #716: plan / plan-read / crew / workflow alias families --------------

    #[test]
    fn alias_corrects_plan_names_to_update_plan() {
        // #1193: enter_plan_mode / exit_plan_mode are now REAL tools (a
        // read-only plan phase), so they no longer coach to update_plan — they
        // dispatch. The plan-CONTENT verbs still coach to update_plan.
        for n in [
            "make_plan",
            "create_plan",
            "plan",
            "planning",
            "todo",
            "todos",
            "todo_write",
        ] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            assert!(msg.contains("update_plan"), "{n}: {msg}");
        }
        // The phase verbs are real tools now — NOT aliases.
        for n in ["enter_plan_mode", "exit_plan_mode"] {
            assert!(
                resolve_tool_alias(n).is_none(),
                "{n} is a real tool, not an alias"
            );
        }
        // #715 PR2: the advance-ish verbs coach update_plan + "completed" too.
        for n in [
            "next_step",
            "complete_step",
            "finish_step",
            "mark_done",
            "step_done",
        ] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            assert!(msg.contains("update_plan"), "{n}: {msg}");
            assert!(msg.contains("completed"), "{n}: {msg}");
        }
        // #715 PR2: update_plan is the REAL tool now → not an alias (returns None),
        // exactly like the resume_context fix; the old set_plan name is gone too.
        assert!(
            resolve_tool_alias("update_plan").is_none(),
            "update_plan must dispatch as the real tool, not a self-alias"
        );
    }

    #[test]
    fn alias_rewrites_plan_read_names_to_plan_get() {
        for n in [
            "get_plan",
            "show_plan",
            "read_plan",
            "current_plan",
            "what_was_i_doing",
        ] {
            assert!(
                matches!(
                    resolve_tool_alias(n),
                    Some(AliasOutcome::Rewrite("plan_get"))
                ),
                "{n} should rewrite to plan_get"
            );
        }
    }

    #[test]
    fn alias_rewrites_resume_reaches_to_resume_context() {
        // #714: the instinctive "where did we leave off" reaches redirect to the
        // self-recovery tool, not plan_get.
        for n in [
            "resume",
            "where_were_we",
            "where_did_we_leave_off",
            "catch_me_up",
            "recap",
        ] {
            assert!(
                matches!(
                    resolve_tool_alias(n),
                    Some(AliasOutcome::Rewrite("resume_context"))
                ),
                "{n} should rewrite to resume_context"
            );
        }
        // The REAL tool name is not an alias: it returns None so a direct
        // resume_context call dispatches as a real tool and is NOT logged as a
        // phantom Rewrite by #717 telemetry (real names must return None).
        assert!(
            resolve_tool_alias("resume_context").is_none(),
            "the real tool name must return None, not a self-Rewrite"
        );
        // No regression: `what_was_i_doing` still asks specifically for the plan.
        assert!(
            matches!(
                resolve_tool_alias("what_was_i_doing"),
                Some(AliasOutcome::Rewrite("plan_get"))
            ),
            "what_was_i_doing must stay → plan_get"
        );
    }

    #[test]
    fn alias_corrects_crew_names_and_flags_team_gating() {
        for n in [
            "delegate",
            "spawn_agent",
            "subagent",
            "sub_agent",
            "crew_dispatch",
            "run_crew",
            "dispatch_crew",
            "fork_agent",
            "assign",
            "team",
        ] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            // Names the real targets...
            assert!(msg.contains("compose_roster"), "{n}: {msg}");
            assert!(msg.contains("crew"), "{n}: {msg}");
            // ...but makes clear the model cannot self-enable the /team surface.
            assert!(msg.contains("/team"), "{n}: {msg}");
            assert!(
                msg.contains("human enables") || msg.contains("cannot turn it on yourself"),
                "crew correction must not imply the model can invoke it: {msg}"
            );
        }
    }

    #[test]
    fn alias_corrects_workflow_names_to_plan_plus_crew() {
        for n in ["workflow", "run_workflow", "start_workflow", "pipeline"] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            assert!(msg.contains("no workflow tool"), "{n}: {msg}");
            assert!(msg.contains("update_plan"), "{n}: {msg}");
        }
    }

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("read_file", "read_file"), 0);
        assert_eq!(levenshtein("read_fil", "read_file"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn nearest_tool_name_suggests_close_only() {
        assert_eq!(nearest_tool_name("read_fil"), Some("read_file"));
        assert_eq!(nearest_tool_name("edit_fil"), Some("edit_file"));
        assert_eq!(nearest_tool_name("memory_fetchh"), Some("memory_fetch"));
        assert_eq!(nearest_tool_name("definitely_not_a_tool"), None);
    }

    #[test]
    fn unknown_tool_message_names_catalog_and_suggestion() {
        let m = unknown_tool_message("read_fil");
        assert!(m.starts_with("unknown tool: read_fil"), "{m}");
        assert!(m.contains("Did you mean 'read_file'"), "{m}");
        assert!(m.contains("Available tools include:"), "{m}");

        let m2 = unknown_tool_message("zzzzzzzzzzzz");
        assert!(m2.starts_with("unknown tool: zzzzzzzzzzzz"), "{m2}");
        assert!(!m2.contains("Did you mean"), "{m2}");
        assert!(m2.contains("Available tools include:"), "{m2}");
    }

    /// An incompatible-arg alias is corrected (not dead-ended) by execute_tool:
    /// a model that emits `str_replace_editor` is told to use edit_file. The
    /// correction returns before any fs/caveat work, so this is deterministic.
    #[tokio::test]
    async fn execute_tool_corrects_str_replace_editor_alias() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let out = run_tool(
            "str_replace_editor",
            serde_json::json!({"command": "str_replace", "path": "f.txt"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.contains("edit_file"), "got: {out}");
        assert!(!out.starts_with("unknown tool"), "got: {out}");
    }

    // -- #263 prompted permission grants through execute_tool ---------------

    /// Scripted gate: records every request it is asked about and answers
    /// allow (with caveats widened by exactly the requested grants) or deny.
    struct MockGate {
        allow: bool,
        base: Caveats,
        asks: Vec<(String, String)>,
    }

    impl MockGate {
        fn new(allow: bool, base: &Caveats) -> Self {
            Self {
                allow,
                base: base.clone(),
                asks: Vec::new(),
            }
        }
    }

    impl super::PermissionGate for MockGate {
        fn ask(&mut self, requests: &[super::PermissionRequest]) -> super::PermissionDecision {
            for r in requests {
                self.asks
                    .push((r.tool.clone(), format!("{}:{}", r.kind.as_str(), r.target)));
            }
            if self.allow {
                let grants: Vec<_> = requests
                    .iter()
                    .map(|r| (r.kind, r.target.clone()))
                    .collect();
                super::PermissionDecision::Allow(crate::agentic::widen_caveats(&self.base, &grants))
            } else {
                super::PermissionDecision::Deny
            }
        }
        // #728: this gate exercises the GRANT path only; it has no human to
        // answer free-text questions, so it returns None (a trivial impl).
        fn ask_question(&mut self, _question: &str) -> Option<String> {
            None
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_tool_gated(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
        gate: &mut MockGate,
    ) -> String {
        execute_tool(
            name,
            &args,
            &ws.to_string_lossy(),
            false,
            20,
            caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            Some(gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await
    }

    /// FR-2 (#1001): a one-tool remote MCP server for testing the remote-tool
    /// leash — records whether `call` actually dispatched.
    struct OneRemoteTool {
        name: &'static str,
        called: bool,
    }
    impl OneRemoteTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                called: false,
            }
        }
    }
    #[async_trait::async_trait]
    impl McpTools for OneRemoteTool {
        fn handles(&self, name: &str) -> bool {
            name == self.name
        }
        fn tool_defs(&self) -> Vec<serde_json::Value> {
            vec![serde_json::json!({
                "type": "function",
                "function": { "name": self.name, "description": "", "parameters": {} }
            })]
        }
        async fn call(&mut self, _name: &str, _args: &serde_json::Value) -> String {
            self.called = true;
            "remote-tool-ran".to_string()
        }
    }

    async fn run_remote_gated(
        name: &str,
        ws: &std::path::Path,
        caveats: &Caveats,
        persona_tools: Option<&[String]>,
        mcp: &mut dyn McpTools,
        gate: Option<&mut MockGate>,
    ) -> String {
        let gate = gate.map(|g| g as &mut dyn super::PermissionGate);
        execute_tool_with_offload(
            name,
            &serde_json::json!({}),
            &ws.to_string_lossy(),
            false,
            20,
            caveats,
            mcp,
            None,
            None,
            None,
            None,
            gate,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            None,
            persona_tools,
        )
        .await
    }

    /// Directly exercise the disposition-aware dispatcher. Unlike
    /// [`execute_tool_with_offload`], this reaches the new required boundary
    /// argument while fixing all unrelated optional seams to their inert shape.
    #[allow(clippy::too_many_arguments)]
    async fn run_tool_with_disposition(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
        mcp: &mut dyn McpTools,
        gate: Option<&mut MockGate>,
        disposition: PromptDisposition,
    ) -> String {
        let gate = gate.map(|gate| gate as &mut dyn PermissionGate);
        execute_tool_with_offload_and_prompt_and_artifacts(
            name,
            &args,
            &ws.to_string_lossy(),
            false,
            20,
            caveats,
            mcp,
            None, // build_check_cmd
            None, // note_sink
            None, // recall_source
            None, // memory_source
            None, // prompt_context
            None, // artifact_context
            None, // artifact_sink
            gate,
            None,  // exec_floor
            None,  // git_tool
            None,  // crew_runner
            None,  // scratchpad_store
            None,  // code_search
            None,  // experience_store
            None,  // step_ledger
            false, // tool_offload
            None,  // spill_store
            None,  // persona_tools
            disposition,
        )
        .await
    }

    /// A non-Act disposition is an executor boundary, not just a reduced tool
    /// schema: fabricated mutations, exec, capability requests, and remote MCP
    /// calls must be refused before they reach their own dispatch logic.
    #[tokio::test]
    async fn non_act_disposition_denies_mutation_exec_grants_and_generic_mcp() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats::top(); // prove disposition wins over ambient authority

        let mut no_mcp = NoMcp;
        let write = run_tool_with_disposition(
            "write_file",
            serde_json::json!({ "path": "must-not-write.txt", "content": "no" }),
            ws.path(),
            &caveats,
            &mut no_mcp,
            None,
            PromptDisposition::Research,
        )
        .await;
        assert!(write.contains("current prompt disposition"), "got: {write}");
        assert!(
            !ws.path().join("must-not-write.txt").exists(),
            "disposition rejection must precede the write handler"
        );

        let exec = run_tool_with_disposition(
            "run_command",
            serde_json::json!({ "command": "touch must-not-exec.txt" }),
            ws.path(),
            &caveats,
            &mut no_mcp,
            None,
            PromptDisposition::Explain,
        )
        .await;
        assert!(exec.contains("current prompt disposition"), "got: {exec}");
        assert!(
            !ws.path().join("must-not-exec.txt").exists(),
            "disposition rejection must precede the shell handler"
        );

        let mut gate = MockGate::new(true, &caveats);
        let grant = run_tool_with_disposition(
            "request_permissions",
            serde_json::json!({
                "capability": "fs_write",
                "target": "/tmp/should-not-be-granted",
                "reason": "test",
            }),
            ws.path(),
            &caveats,
            &mut no_mcp,
            Some(&mut gate),
            PromptDisposition::Research,
        )
        .await;
        assert!(grant.contains("current prompt disposition"), "got: {grant}");
        assert!(
            gate.asks.is_empty(),
            "non-Act must not consult a grant gate"
        );

        let mut mcp = OneRemoteTool::new("incident__read");
        let remote = run_tool_with_disposition(
            "incident__read",
            serde_json::json!({}),
            ws.path(),
            &caveats,
            &mut mcp,
            None,
            PromptDisposition::Research,
        )
        .await;
        assert!(
            remote.contains("current prompt disposition"),
            "got: {remote}"
        );
        assert!(
            !mcp.called,
            "generic MCP must be denied before remote routing in non-Act"
        );

        std::fs::write(ws.path().join("evidence.txt"), "durable evidence\n").unwrap();
        let read = run_tool_with_disposition(
            "read_file",
            serde_json::json!({ "path": "evidence.txt" }),
            ws.path(),
            &caveats,
            &mut no_mcp,
            None,
            PromptDisposition::Research,
        )
        .await;
        assert!(
            read.contains("durable evidence"),
            "safe read must remain usable: {read}"
        );
    }

    /// Permitted non-Act reads still honor their caveats, but they must not
    /// silently turn a denial into an interactive authority grant.
    #[tokio::test]
    async fn non_act_read_tools_do_not_consult_permission_gate() {
        let ws = tempfile::TempDir::new().unwrap();
        let mut caveats = Caveats::top();
        caveats.net = crate::caveats::Scope::none();
        let mut gate = MockGate::new(true, &caveats);
        let mut mcp = NoMcp;

        let _ = run_tool_with_disposition(
            "web_fetch",
            serde_json::json!({ "url": "https://example.com" }),
            ws.path(),
            &caveats,
            &mut mcp,
            Some(&mut gate),
            PromptDisposition::Research,
        )
        .await;
        assert!(
            gate.asks.is_empty(),
            "a non-Act web read may be caveat-denied but must never mint net authority"
        );
    }

    /// FR-2 (#1001): a remote MCP tool OUTSIDE the persona's allow-list is
    /// PROMPTED (not hard-vetoed like a built-in). Deny → withheld and `call`
    /// never runs; Allow → dispatched; a tool the persona already grants
    /// dispatches with NO prompt; headless (no gate) fails closed.
    #[tokio::test]
    async fn remote_tool_outside_allow_list_is_prompted_not_hard_vetoed() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = crate::caveats::Caveats::top();
        let coach = vec!["read_file".to_string()]; // no incident__create

        // Gate DENIES → withheld; `call` never invoked; the human WAS prompted,
        // and prompted as a remote-tool grant (not an fs/exec/net axis).
        let mut mcp = OneRemoteTool::new("incident__create");
        let mut gate = MockGate::new(false, &caveats);
        let out = run_remote_gated(
            "incident__create",
            ws.path(),
            &caveats,
            Some(&coach),
            &mut mcp,
            Some(&mut gate),
        )
        .await;
        assert!(!mcp.called, "denied remote tool must NOT dispatch");
        assert_eq!(gate.asks.len(), 1, "the human was prompted");
        assert_eq!(gate.asks[0].1, "remote_tool:incident__create");
        assert!(out.contains("persona"), "returns a denial: {out}");

        // Gate ALLOWS → dispatched.
        let mut mcp = OneRemoteTool::new("incident__create");
        let mut gate = MockGate::new(true, &caveats);
        let out = run_remote_gated(
            "incident__create",
            ws.path(),
            &caveats,
            Some(&coach),
            &mut mcp,
            Some(&mut gate),
        )
        .await;
        assert!(mcp.called, "granted remote tool dispatches");
        assert_eq!(out, "remote-tool-ran");

        // A remote tool the persona GRANTS dispatches with NO prompt.
        let granted = vec!["incident__create".to_string()];
        let mut mcp = OneRemoteTool::new("incident__create");
        let mut gate = MockGate::new(false, &caveats); // would deny if asked
        run_remote_gated(
            "incident__create",
            ws.path(),
            &caveats,
            Some(&granted),
            &mut mcp,
            Some(&mut gate),
        )
        .await;
        assert!(mcp.called, "allow-listed remote tool dispatches");
        assert!(
            gate.asks.is_empty(),
            "no prompt when the persona already grants it"
        );

        // Headless (no gate) → fail-closed: withheld, `call` never runs.
        let mut mcp = OneRemoteTool::new("incident__create");
        let out = run_remote_gated(
            "incident__create",
            ws.path(),
            &caveats,
            Some(&coach),
            &mut mcp,
            None,
        )
        .await;
        assert!(
            !mcp.called,
            "headless must fail closed for an ungranted remote tool"
        );
        assert!(out.contains("persona"), "headless denial: {out}");
    }

    // -- #721 recoverable denials + request_permissions ---------------------

    #[test]
    fn exec_denial_is_recoverable_not_a_dead_end() {
        // #721 + #775: the exec denial the MODEL sees is ONE clean level —
        // `capability denied: <bare reason>. <recovery hint>` — leading to the
        // model-actionable request_permissions path, NOT the stale `extra_exec`
        // config edit (which #721 superseded and the model cannot perform
        // mid-turn).
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [{
                "kind": "exec",
                "target": "mkdir",
                "reason": "exec of \"mkdir\" is not within the granted authority"
            }]
        });
        let out = denied_run_command_result(&envelope, false);
        assert!(out.starts_with("capability denied:"), "got: {out}");
        assert!(out.contains("request_permissions"), "got: {out}");
        // #775: the stale `extra_exec` config hint is GONE from the model-facing
        // message (it leaked in before).
        assert!(
            !out.contains("extra_exec"),
            "the model message must not carry the stale config hint: {out}"
        );
    }

    /// #775 (§2.5) regression: the model-facing `run_command` denial is ONE
    /// clean level and never a denial sentence NESTED inside another. Before
    /// the fix, `denied_run_command_result` appended the `extra_exec` config
    /// hint to the reason (and the former notice stuffed that whole sentence into
    /// its bare `'{target}'` slot), yielding `capability denied: exec does not
    /// permit '<reason> - add it via …>'`. The model-facing return now carries
    /// exactly one `capability denied:`, the bare reason, and the recovery hint.
    #[test]
    fn run_command_denial_is_single_level_not_nested() {
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [{
                "kind": "exec",
                "target": "export",
                "reason": "exec of \"export\" is not within the granted authority"
            }]
        });
        let out = denied_run_command_result(&envelope, false);
        // Exactly one denial prefix — never a `capability denied:` inside another.
        assert_eq!(
            out.matches("capability denied:").count(),
            1,
            "exactly one denial level: {out}"
        );
        // RED on today: the stale config hint was glued onto the model message.
        assert!(!out.contains("add it via"), "stale config hint: {out}");
        assert!(!out.contains("extra_exec"), "stale config hint: {out}");
        // No reason sentence nested inside a `does not permit '…'` slot.
        assert!(
            !out.contains("does not permit 'exec of"),
            "nested denial sentence: {out}"
        );
        // The bare reason and the #721 recovery hint are both present.
        assert!(
            out.contains("exec of \"export\" is not within the granted authority"),
            "got: {out}"
        );
        assert!(out.contains("request_permissions"), "got: {out}");
    }

    #[test]
    fn parse_capability_maps_synonyms_and_rejects_unknown() {
        assert_eq!(parse_capability("exec"), Some(DenialKind::Exec));
        assert_eq!(parse_capability("shell"), Some(DenialKind::Exec));
        assert_eq!(parse_capability("FS_READ"), Some(DenialKind::FsRead));
        assert_eq!(parse_capability("write"), Some(DenialKind::FsWrite));
        assert_eq!(parse_capability("network"), Some(DenialKind::Net));
        assert_eq!(parse_capability("gpu"), None);
        assert_eq!(parse_capability(""), None);
    }

    #[test]
    fn request_permissions_grant_deny_and_no_gate() {
        let base = Caveats::top();

        // Mock gate ALLOWS → "granted" + the retry coaching; the gate was asked
        // with the parsed axis + target.
        let mut gate = MockGate::new(true, &base);
        let out = execute_request_permissions(
            &serde_json::json!({"capability": "exec", "target": "mkdir", "reason": "make a dir"}),
            Some(&mut gate),
            false,
            20,
        );
        assert!(out.starts_with("granted:"), "got: {out}");
        assert!(out.contains("Retry the original operation"), "got: {out}");
        assert_eq!(gate.asks.len(), 1);
        assert_eq!(
            gate.asks[0],
            ("request_permissions".to_string(), "exec:mkdir".to_string())
        );

        // Mock gate DENIES → "denied" + don't-retry coaching.
        let mut gate = MockGate::new(false, &base);
        let out = execute_request_permissions(
            &serde_json::json!({"capability": "fs_write", "target": "/tmp/x", "reason": "w"}),
            Some(&mut gate),
            false,
            20,
        );
        assert!(out.starts_with("denied:"), "got: {out}");
        assert!(out.contains("different approach"), "got: {out}");

        // NO gate (headless / eval) → "no operator available" — recoverable,
        // never a hang or a config-only dead end.
        let out = execute_request_permissions(
            &serde_json::json!({"capability": "net", "target": "docs.rs", "reason": "fetch"}),
            None,
            false,
            20,
        );
        assert!(out.contains("no operator available"), "got: {out}");
    }

    #[test]
    fn request_permissions_coaches_bad_inputs() {
        // Unknown capability → coach listing the valid axes (no gate consulted).
        let out = execute_request_permissions(
            &serde_json::json!({"capability": "gpu", "target": "x", "reason": "y"}),
            None,
            false,
            20,
        );
        assert!(out.contains("unknown capability"), "got: {out}");
        assert!(out.contains("fs_read"), "got: {out}");
        // Missing target → coach.
        let out = execute_request_permissions(
            &serde_json::json!({"capability": "exec", "reason": "y"}),
            None,
            false,
            20,
        );
        assert!(out.contains("'target' is required"), "got: {out}");
    }

    #[test]
    fn request_permissions_is_a_real_tool_not_a_phantom() {
        // #721: a real, always-advertised tool — never an alias / hallucination.
        assert!(resolve_tool_alias("request_permissions").is_none());
        assert!(ALL_TOOL_NAMES.contains(&"request_permissions"));
        assert!(classify_phantom_reach(
            "request_permissions",
            &serde_json::json!({"capability": "exec", "target": "mkdir", "reason": "r"}),
            "granted: the operator allowed exec for 'mkdir'.",
            true,
        )
        .is_none());
    }

    // -- #728 request_user_input (generic ask-the-human) --------------------

    /// A gate that answers a free-text question with a scripted answer (or None
    /// for "no human"). Its grant path (`ask`) is irrelevant here — it denies.
    struct AskGate {
        answer: Option<String>,
        asked: Vec<String>,
    }
    impl AskGate {
        fn new(answer: Option<&str>) -> Self {
            Self {
                answer: answer.map(str::to_string),
                asked: Vec::new(),
            }
        }
    }
    impl super::PermissionGate for AskGate {
        fn ask(&mut self, _requests: &[super::PermissionRequest]) -> super::PermissionDecision {
            super::PermissionDecision::Deny
        }
        fn ask_question(&mut self, question: &str) -> Option<String> {
            self.asked.push(question.to_string());
            self.answer.clone()
        }
    }

    #[test]
    fn request_user_input_returns_the_human_answer() {
        // A gate whose ask_question returns Some(answer) → the tool returns that
        // answer verbatim, and the gate was asked the exact question.
        let mut gate = AskGate::new(Some("postgres"));
        let out = execute_request_user_input(
            &serde_json::json!({"question": "which database should I target?"}),
            Some(&mut gate),
            false,
            20,
        );
        assert_eq!(out, "postgres");
        assert_eq!(
            gate.asked,
            vec!["which database should I target?".to_string()]
        );
    }

    #[test]
    fn request_user_input_no_gate_reports_headless_never_hangs() {
        // No gate (headless / eval / ACP) → the recoverable "no human available"
        // message — never a hang. (This test completing IS the no-hang proof: it
        // touches no real stdin.)
        let out = execute_request_user_input(
            &serde_json::json!({"question": "are you sure?"}),
            None,
            false,
            20,
        );
        assert_eq!(out, HEADLESS_NO_HUMAN);
        assert!(out.contains("no human available"), "got: {out}");
    }

    #[test]
    fn request_user_input_gate_with_no_human_reports_headless() {
        // A gate present but with no human to consult (ask_question → None) →
        // the SAME headless message, not a hang or an empty answer.
        let mut gate = AskGate::new(None);
        let out = execute_request_user_input(
            &serde_json::json!({"question": "pick one"}),
            Some(&mut gate),
            false,
            20,
        );
        assert_eq!(out, HEADLESS_NO_HUMAN);
    }

    #[test]
    fn request_user_input_requires_a_question() {
        // Missing / blank question → coach; the gate is never consulted.
        let mut gate = AskGate::new(Some("unused"));
        let out = execute_request_user_input(
            &serde_json::json!({"question": "   "}),
            Some(&mut gate),
            false,
            20,
        );
        assert!(out.contains("'question' is required"), "got: {out}");
        assert!(
            gate.asked.is_empty(),
            "gate not consulted for a blank question"
        );
    }

    #[test]
    fn request_user_input_is_a_real_tool_not_a_phantom() {
        // #728: a real, always-advertised tool — never an alias of itself or a
        // hallucination.
        assert!(resolve_tool_alias("request_user_input").is_none());
        assert!(ALL_TOOL_NAMES.contains(&"request_user_input"));
        assert!(classify_phantom_reach(
            "request_user_input",
            &serde_json::json!({"question": "which db?"}),
            "postgres",
            true,
        )
        .is_none());
        // The always-advertised def rides in every session (empty MCP).
        let defs = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false,
        );
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"request_user_input"), "got: {names:?}");
    }

    #[test]
    fn ask_verbs_rewrite_to_request_user_input() {
        // #728: the instinctive ask-the-human verbs resolve to the real tool.
        for verb in [
            "ask_user",
            "ask_human",
            "prompt_user",
            "get_user_input",
            "ask_question",
            "clarify",
            "ask",
        ] {
            match resolve_tool_alias(verb) {
                Some(AliasOutcome::Rewrite(c)) => {
                    assert_eq!(c, "request_user_input", "verb: {verb}");
                }
                _ => panic!("expected Rewrite(request_user_input) for {verb}"),
            }
        }
    }

    #[tokio::test]
    async fn request_user_input_dispatches_through_execute_tool() {
        // End-to-end through the dispatcher: the question reaches the gate and
        // the answer flows back. Fully mocked (AskGate, no real stdin).
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats::top();
        let mut gate = AskGate::new(Some("the answer"));
        let out = execute_tool(
            "request_user_input",
            &serde_json::json!({"question": "what now?"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            Some(&mut gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert_eq!(out, "the answer");
        assert_eq!(gate.asked, vec!["what now?".to_string()]);
    }

    #[test]
    fn get_context_remaining_is_a_real_tool_not_a_phantom() {
        // #727: real, always-advertised, no-arg budget read — never treated as
        // an alias of itself or a hallucination.
        assert!(resolve_tool_alias("get_context_remaining").is_none());
        assert!(ALL_TOOL_NAMES.contains(&"get_context_remaining"));
        assert!(classify_phantom_reach(
            "get_context_remaining",
            &serde_json::json!({}),
            "Context budget: ~10 tokens used of an input ceiling of ~80 (80% of num_ctx 100).",
            true,
        )
        .is_none());
        // The always-advertised def rides in every session (empty MCP).
        let defs = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false,
        );
        assert!(defs
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["function"]["name"] == "get_context_remaining"));
    }

    #[test]
    fn budget_verbs_rewrite_to_get_context_remaining() {
        // #727: the instinctive "how much context is left" reaches all resolve
        // to the canonical no-arg read (safe silent Rewrite — matching arg shape).
        for n in [
            "context_remaining",
            "tokens_left",
            "remaining_tokens",
            "budget",
            "how_much_context",
            "context_budget",
            "token_budget",
        ] {
            assert!(
                matches!(
                    resolve_tool_alias(n),
                    Some(AliasOutcome::Rewrite("get_context_remaining"))
                ),
                "{n} must rewrite to get_context_remaining"
            );
            // A Rewrite alias is mined by the #717 telemetry as a Rewrite.
            assert!(
                is_context_remaining_call(n),
                "{n} must be recognized as a budget call by the loop"
            );
        }
        // The canonical name is recognized by the loop but is NOT an alias.
        assert!(is_context_remaining_call("get_context_remaining"));
        assert!(resolve_tool_alias("get_context_remaining").is_none());
        // An unrelated name is neither.
        assert!(!is_context_remaining_call("read_file"));
    }

    /// FLAG OFF (no gate): the denial is deterministic and still DENIES every
    /// fs op (the #263 default-deny posture is intact) — now in the #721
    /// recoverable form (`denied_fs_result`, carrying the request_permissions
    /// path), pinned via the shared helper so the wording can't drift.
    #[tokio::test]
    async fn no_gate_denials_are_bit_for_bit_unchanged() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
        let denied = Caveats {
            fs_read: Scope::none(),
            fs_write: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let out = run_tool(
            "read_file",
            serde_json::json!({"path": "secret.txt"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_read", "secret.txt"));
        let out = run_tool(
            "list_dir",
            serde_json::json!({"path": "."}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_read", "."));
        let out = run_tool(
            "write_file",
            serde_json::json!({"path": "a.txt", "content": "c"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_write", "a.txt"));
        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "a.txt", "old_string": "a", "new_string": "b"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_write", "a.txt"));
        let out = run_tool(
            "delete_file",
            serde_json::json!({"path": "secret.txt"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_write", "secret.txt"));
        // #721: every fs denial now carries the model-actionable recovery path.
        assert!(out.contains("request_permissions"), "got: {out}");
    }

    /// Gate allows an fs_read denial → the read proceeds and returns the
    /// real contents; the gate was consulted with the tool + axis + full
    /// path it would be granting.
    #[tokio::test]
    async fn gate_allow_turns_fs_read_denial_into_the_real_result() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("secret.txt"), "the contents").unwrap();
        let denied = Caveats {
            fs_read: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = MockGate::new(true, &denied);
        let out = run_tool_gated(
            "read_file",
            serde_json::json!({"path": "secret.txt"}),
            ws.path(),
            &denied,
            &mut gate,
        )
        .await;
        assert_eq!(out, "the contents");
        let full = ws.path().join("secret.txt").to_string_lossy().into_owned();
        assert_eq!(
            gate.asks,
            vec![("read_file".to_string(), format!("fs_read:{full}"))]
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn permission_retry_closes_each_live_generation_before_the_next_starts() {
        #[derive(Default)]
        struct LifecycleOutput(std::sync::Mutex<Vec<String>>);
        impl crate::agentic::LiveToolOutput for LifecycleOutput {
            fn start(&self, generation: u64) {
                self.0.lock().unwrap().push(format!("start:{generation}"));
            }
            fn write(
                &self,
                generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                chunk: &[u8],
            ) {
                self.0.lock().unwrap().push(format!(
                    "write:{generation}:{}",
                    String::from_utf8_lossy(chunk)
                ));
            }
            fn finish(&self, generation: u64) {
                self.0.lock().unwrap().push(format!("finish:{generation}"));
            }
            fn abandon(&self, generation: u64) {
                self.0.lock().unwrap().push(format!("abandon:{generation}"));
            }
        }

        let ws = tempfile::TempDir::new().unwrap();
        let denied = Caveats {
            exec: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = MockGate::new(true, &denied);
        let sink = std::sync::Arc::new(LifecycleOutput::default());
        let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3);
        let out = exec_confined_command(
            // Use an external executable under every engine. Bare `echo` is a
            // Brush builtin and therefore correctly needs no exec grant.
            "/bin/echo retry-visible",
            &ws.path().to_string_lossy(),
            false,
            20,
            &denied,
            None,
            Some(&mut gate),
            false,
            None,
            Some(sink.clone()),
            &mut display,
        )
        .await;

        assert!(out.contains("retry-visible"), "retry result: {out}");
        assert_eq!(gate.asks.len(), 1, "permission prompt count");
        let events = sink.0.lock().unwrap();
        let starts: Vec<_> = events
            .iter()
            .filter(|event| event.starts_with("start:"))
            .cloned()
            .collect();
        assert_eq!(starts.len(), 2, "one viewport per attempt: {events:?}");
        let first_generation = starts[0].trim_start_matches("start:");
        let retry_start = events
            .iter()
            .position(|event| event == &starts[1])
            .expect("retry start event");
        assert!(
            events[..retry_start]
                .iter()
                .any(|event| event == &format!("finish:{first_generation}")),
            "retry started before the denied generation finished: {events:?}"
        );
        let second_generation = starts[1].trim_start_matches("start:");
        assert!(
            events.iter().any(|event| {
                event.starts_with(&format!("write:{second_generation}:"))
                    && event.contains("retry-visible")
            }),
            "retry bytes were not delivered to its generation: {events:?}"
        );
        let expected_finish = format!("finish:{second_generation}");
        assert_eq!(events.last(), Some(&expected_finish), "events: {events:?}");
    }

    /// Gate denies → the result is the standard denial, bit-for-bit equal to
    /// the no-gate path (#263: deny = the current denial result).
    #[tokio::test]
    async fn gate_deny_keeps_the_standard_denial_bit_for_bit() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
        let denied = Caveats {
            fs_read: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = MockGate::new(false, &denied);
        let gated = run_tool_gated(
            "read_file",
            serde_json::json!({"path": "secret.txt"}),
            ws.path(),
            &denied,
            &mut gate,
        )
        .await;
        let ungated = run_tool(
            "read_file",
            serde_json::json!({"path": "secret.txt"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(gated, ungated);
        assert_eq!(gated, denied_fs_result("fs_read", "secret.txt"));
        assert_eq!(gate.asks.len(), 1, "the human was asked exactly once");
    }

    /// Gate allows fs_write denials → write_file, edit_file, and delete_file proceed.
    #[tokio::test]
    async fn gate_allow_turns_fs_write_denials_into_real_writes() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("f.txt"), "old\n").unwrap();
        std::fs::write(ws.path().join("stale.txt"), "remove me\n").unwrap();
        let denied = Caveats {
            fs_write: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = MockGate::new(true, &denied);
        let out = run_tool_gated(
            "write_file",
            serde_json::json!({"path": "new.txt", "content": "fresh"}),
            ws.path(),
            &denied,
            &mut gate,
        )
        .await;
        assert!(out.starts_with("wrote new.txt"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("new.txt")).unwrap(),
            "fresh"
        );
        let out = run_tool_gated(
            "edit_file",
            serde_json::json!({"path": "f.txt", "old_string": "old", "new_string": "new"}),
            ws.path(),
            &denied,
            &mut gate,
        )
        .await;
        assert!(out.starts_with("edited f.txt"), "got: {out}");
        let out = run_tool_gated(
            "delete_file",
            serde_json::json!({"path": "stale.txt"}),
            ws.path(),
            &denied,
            &mut gate,
        )
        .await;
        assert!(out.starts_with("deleted stale.txt"), "got: {out}");
        assert!(
            !ws.path().join("stale.txt").exists(),
            "gate-approved delete must remove the file"
        );
        assert_eq!(gate.asks.len(), 3);
        assert_eq!(gate.asks[0].0, "write_file");
        assert!(
            gate.asks[1].1.starts_with("fs_write:"),
            "got: {:?}",
            gate.asks[1]
        );
        assert_eq!(gate.asks[2].0, "delete_file");
    }

    /// list_dir consults the gate on an fs_read denial like read_file does.
    #[tokio::test]
    async fn gate_allow_turns_list_dir_denial_into_the_listing() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("seen.txt"), "x").unwrap();
        let denied = Caveats {
            fs_read: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = MockGate::new(true, &denied);
        let out = run_tool_gated(
            "list_dir",
            serde_json::json!({"path": "."}),
            ws.path(),
            &denied,
            &mut gate,
        )
        .await;
        assert!(out.contains("seen.txt"), "got: {out}");
    }

    /// A buggy/hostile gate answering Allow with caveats that STILL don't
    /// cover the path must not bypass enforcement: the widened authority is
    /// re-checked, never assumed (fs_gate_allows' re-check).
    #[tokio::test]
    async fn gate_allow_without_real_coverage_is_still_denied() {
        struct LyingGate;
        impl super::PermissionGate for LyingGate {
            fn ask(&mut self, _requests: &[super::PermissionRequest]) -> super::PermissionDecision {
                // "Allow", but the caveats grant nothing at all.
                super::PermissionDecision::Allow(Caveats {
                    fs_read: Scope::none(),
                    fs_write: Scope::none(),
                    exec: Scope::none(),
                    net: Scope::none(),
                    max_calls: CountBound::Unlimited,
                    valid_for_generation: Scope::All,
                })
            }
            fn ask_question(&mut self, _question: &str) -> Option<String> {
                None
            }
        }
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
        let denied = Caveats {
            fs_read: Scope::none(),
            ..caveats_rw(ws.path())
        };
        let mut gate = LyingGate;
        let out = execute_tool(
            "read_file",
            &serde_json::json!({"path": "secret.txt"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &denied,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            Some(&mut gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_read", "secret.txt"));
    }

    /// web_fetch with a gate: an out-of-allowlist host consults the gate
    /// with the parsed host; on deny the dispatch runs under the ORIGINAL
    /// caveats, so the leash produces today's denial (an `error:` result —
    /// nothing is fetched).
    #[tokio::test]
    async fn web_fetch_gate_deny_dispatches_under_original_caveats() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path()); // net: Scope::none()
        let mut gate = MockGate::new(false, &caveats);
        let out = run_tool_gated(
            "web_fetch",
            serde_json::json!({"url": "https://denied.example.com:8443/page"}),
            ws.path(),
            &caveats,
            &mut gate,
        )
        .await;
        assert!(out.starts_with("error:"), "leash denial surfaces: {out}");
        assert_eq!(
            gate.asks,
            vec![(
                "web_fetch".to_string(),
                "net:denied.example.com".to_string()
            )]
        );
    }

    /// Regression for the field report: github.com is outside the default net
    /// scope, so a TUI-provided gate must be consulted before the bridle leash
    /// returns the denial to the model.
    #[tokio::test]
    async fn web_fetch_github_denial_consults_permission_gate() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path()); // net: Scope::none()
        let mut gate = MockGate::new(false, &caveats);
        let out = run_tool_gated(
            "web_fetch",
            serde_json::json!({"url": "https://github.com/openai/codex"}),
            ws.path(),
            &caveats,
            &mut gate,
        )
        .await;
        assert!(out.starts_with("error:"), "leash denial surfaces: {out}");
        assert_eq!(
            gate.asks,
            vec![("web_fetch".to_string(), "net:github.com".to_string())]
        );
    }

    /// An unparseable URL skips the net pre-check entirely — the gate is
    /// never consulted and the dispatch (with the original caveats) answers.
    #[tokio::test]
    async fn web_fetch_unparseable_url_never_prompts() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let mut gate = MockGate::new(true, &caveats);
        let out = run_tool_gated(
            "web_fetch",
            serde_json::json!({"url": "not-a-url"}),
            ws.path(),
            &caveats,
            &mut gate,
        )
        .await;
        assert!(out.starts_with("error:"), "got: {out}");
        assert!(gate.asks.is_empty(), "no prompt for an unparseable URL");
    }

    // -- save_note dispatch through execute_tool (Step 19.3) ----------------

    #[tokio::test]
    async fn save_note_without_sink_is_unknown_tool() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        // run_tool passes note_sink: None — the no-sink (headless) shape.
        let out = run_tool(
            "save_note",
            serde_json::json!({"action": "add", "text": "a fact"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.starts_with("unknown tool: save_note"), "got: {out}");
    }

    #[tokio::test]
    async fn save_note_with_sink_routes_through_execute_tool() {
        use crate::agentic::note_sink::tests::MockSink;
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let mut sink = MockSink::default();
        let out = execute_tool(
            "save_note",
            &serde_json::json!({"action": "add", "text": "workspace builds with just check"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            Some(&mut sink),
            None,
            None, // memory_source
            None,
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert_eq!(sink.calls, vec!["add:workspace builds with just check"]);
        assert!(
            out.starts_with("note saved: workspace builds"),
            "got: {out}"
        );
    }

    // -- recall dispatch through execute_tool (Step 17.5) -------------------

    #[tokio::test]
    async fn recall_without_source_is_unknown_tool() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        // run_tool passes recall_source: None — the no-store (headless) shape.
        let out = run_tool(
            "recall",
            serde_json::json!({"query": "tokio panic"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.starts_with("unknown tool: recall"), "got: {out}");
    }

    #[tokio::test]
    async fn recall_with_source_routes_through_execute_tool() {
        use crate::agentic::recall::tests::{hit, MockSource};
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let source = MockSource {
            hits: vec![hit(
                "123456789012-abcd",
                "past work",
                3,
                ">>>tokio<<< panic",
            )],
            ..Default::default()
        };
        let out = execute_tool(
            "recall",
            &serde_json::json!({"query": "tokio panic"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            Some(&source),
            None, // memory_source
            None,
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert_eq!(
            *source.calls.lock().unwrap(),
            vec![("tokio panic".to_string(), 5)]
        );
        assert!(out.contains("«tokio» panic"), "got: {out}");
        assert!(out.contains("past work"), "got: {out}");
    }

    // -- memory_fetch dispatch through execute_tool (#319) ------------------

    /// FLAG OFF (no source): a `memory_fetch` call is treated like any unknown
    /// tool — the inert-by-default shape (the tool was never advertised, so a
    /// call here is a hallucination). Mirrors `recall_without_source`.
    #[tokio::test]
    async fn memory_fetch_without_source_is_unknown_tool() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        // run_tool passes memory_source: None — the no-source (headless) shape.
        let out = run_tool(
            "memory_fetch",
            serde_json::json!({"address": "note:1"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert!(out.starts_with("unknown tool: memory_fetch"), "got: {out}");
    }

    /// FLAG ON (source present): a `memory_fetch` call routes through the
    /// injected `MemorySource` and returns its body. Mirrors
    /// `recall_with_source_routes_through_execute_tool`.
    #[tokio::test]
    async fn memory_fetch_with_source_routes_through_execute_tool() {
        use crate::agentic::memory_fetch::tests::MockSource;
        use crate::agentic::MemAddr;
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_rw(ws.path());
        let source = MockSource {
            body: Some("the exact note body".to_string()),
            ..Default::default()
        };
        let out = execute_tool(
            "memory_fetch",
            &serde_json::json!({"address": "note:1"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            Some(&source),
            None,
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert_eq!(out, "the exact note body");
        assert_eq!(
            *source.calls.lock().unwrap(),
            vec![MemAddr::Note { id: "1".into() }]
        );
    }
}

// ---------------------------------------------------------------------------
// INTERIM (#297) --disable-ocap / --yolo tests — the exec escape hatch.
// Removed with the bypass when brush upstreams CommandInterceptor
// (agent-bridle#20).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod disable_ocap_tests {
    use super::super::NoMcp;
    use super::*;
    use crate::caveats::{Caveats, CountBound, Scope};
    use tokio::sync::{Mutex, MutexGuard};

    /// Serializes every test that reads or writes `NEWT_DISABLE_OCAP` (and
    /// the venv vars the bypass forwards): the process environment is shared
    /// across the parallel test runner. Async-aware (tokio) so the guard may
    /// be held across the `execute_tool` awaits; no poisoning — the `EnvVar`
    /// guards below restore the environment even on panic.
    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    async fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().await
    }

    /// RAII env override: set/unset `key` for the test body, restore the
    /// previous value on drop — including on a failed assertion, so yolo can
    /// never leak into a neighboring test.
    struct EnvVar {
        key: &'static str,
        saved: Option<String>,
    }

    impl EnvVar {
        fn set(key: &'static str, value: &str) -> Self {
            let saved = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, saved }
        }

        fn unset(key: &'static str) -> Self {
            let saved = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, saved }
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Workspace-fenced fs, NO exec, NO net — the shape under which the
    /// confined shell denies (real build) or fails closed (stub build).
    fn caveats_no_exec(ws: &std::path::Path) -> Caveats {
        Caveats {
            fs_read: Scope::only([ws.to_string_lossy().into_owned()]),
            fs_write: Scope::only([ws.to_string_lossy().into_owned()]),
            exec: Scope::none(),
            net: Scope::none(),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        }
    }

    async fn run_tool(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
    ) -> String {
        run_tool_with_floor(name, args, ws, caveats, None).await
    }

    /// #307: like [`run_tool`] but with an explicit exec FLOOR (the active
    /// named-permission-preset clamp). `Some(scope)` makes the `--disable-ocap`
    /// bypass conditional on the floor permitting the command; `None` is the
    /// pre-#307 behavior.
    async fn run_tool_with_floor(
        name: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        caveats: &Caveats,
        exec_floor: Option<&Scope<String>>,
    ) -> String {
        execute_tool(
            name,
            &args,
            &ws.to_string_lossy(),
            false,
            20,
            caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            None,
            exec_floor,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await
    }

    /// The switch reads fail-closed: ONLY the exact value `1` (the value the
    /// CLI exports and the issue documents) asserts the bypass. This is also
    /// the env-var-equivalence half of the #297 test list — the flag and the
    /// env var are one mechanism (`--disable-ocap` just exports the var).
    #[test]
    fn ocap_disabled_requires_exactly_1() {
        let _l = ENV_LOCK.blocking_lock();
        {
            let _unset = EnvVar::unset("NEWT_DISABLE_OCAP");
            assert!(!ocap_disabled(), "absent ⇒ confinement stays on");
        }
        for (value, expected) in [
            ("1", true),
            ("0", false),
            ("", false),
            ("true", false),
            ("yes", false),
            ("YOLO", false),
        ] {
            let _set = EnvVar::set("NEWT_DISABLE_OCAP", value);
            assert_eq!(
                ocap_disabled(),
                expected,
                "NEWT_DISABLE_OCAP={value:?} must read as {expected}"
            );
        }
    }

    /// Same fail-closed contract for the `--full-access` preset override:
    /// ONLY the exact value `1` asserts it (the flag and the env var are one
    /// mechanism — `--full-access` just exports the var).
    #[test]
    fn full_access_requested_requires_exactly_1() {
        let _l = ENV_LOCK.blocking_lock();
        {
            let _unset = EnvVar::unset("NEWT_FULL_ACCESS");
            assert!(!full_access_requested(), "absent ⇒ configured preset rules");
        }
        for (value, expected) in [
            ("1", true),
            ("0", false),
            ("", false),
            ("true", false),
            ("yes", false),
            ("FULL", false),
        ] {
            let _set = EnvVar::set("NEWT_FULL_ACCESS", value);
            assert_eq!(
                full_access_requested(),
                expected,
                "NEWT_FULL_ACCESS={value:?} must read as {expected}"
            );
        }
    }

    /// #1176 shadow-OCAP recording gate. The decision table, which
    /// `exec_confined_command` consults before dispatch:
    /// - host-bypass (yolo) → record (unconfined host shell);
    /// - full-access on the confined path → record (caveats are top()) —
    ///   THE PARITY FIX: before it, a bare `--full-access` run armed the
    ///   recorder yet the confined dispatch never wrote;
    /// - a genuinely confined session (neither) → do NOT record (real leash).
    #[test]
    fn shadow_records_iff_the_run_is_unconfined() {
        assert!(shadow_records(true, false), "yolo host bypass records");
        assert!(
            shadow_records(false, true),
            "--full-access confined dispatch records (the #1176 parity fix)"
        );
        assert!(shadow_records(true, true), "both routes still record");
        assert!(
            !shadow_records(false, false),
            "a genuinely confined session has a real leash — nothing to shadow"
        );
    }

    /// FLAG OFF ⇒ the command goes to the confined dispatch, which governs it.
    /// Built against the agent-bridle env-seam branch (#783), the bridle ships
    /// the REAL safe-subset shell (not the old fail-closed stub), so an
    /// ungranted `echo` under `exec = none` is DENIED by the L3 boundary. This
    /// is the "when the real shell returns" case the prior stub-build note
    /// anticipated; the "unavailable in this build" stub error is retired.
    #[tokio::test]
    async fn flag_off_run_command_keeps_the_confined_dispatch_verbatim() {
        let _l = env_lock().await;
        let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
        // #1243 Leg 1: pin safe-subset. This test proves the disable-ocap FLOOR
        // (engine-independent, runs before the engine); it must not depend on
        // the L3-gated default (`echo` is a TRUE bash builtin under brush — it
        // never spawns, so it isn't exec-gated, which is correct but would make
        // this floor assertion box-dependent).
        let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_tool(
            "run_command",
            serde_json::json!({"command": "echo hi"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert!(
            out.contains("capability denied"),
            "flag off ⇒ the confined dispatch must govern (deny) the command, got: {out}"
        );
    }

    /// FLAG ON: a command the confined shell fails closed on now runs on the
    /// host shell and returns its real output through the SAME envelope
    /// formatter (`shell_envelope_output`).
    #[cfg(unix)]
    #[tokio::test]
    async fn yolo_runs_the_denied_command_on_the_host_shell() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_tool(
            "run_command",
            serde_json::json!({"command": "echo yolo-ok"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert_eq!(out, "yolo-ok\n");

        // No output ⇒ the same `(exit N)` shape the bridle path produces.
        let out = run_tool(
            "run_command",
            serde_json::json!({"command": "exit 3"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert_eq!(out, "(exit 3)");
    }

    /// #726/#945: a verbose `run_command` MUST NOT flood the model's context
    /// window, but MUST still surface both ends of the output — a command's
    /// summary/failure/exit status lives at the TAIL, and #726's original
    /// head-only cap silently dropped exactly that (the gap #945 closed). Runs
    /// through the real host shell (yolo path) so it exercises the actual
    /// `shell_envelope_output` → cap composition. The global budget is the
    /// default 10k in the test binary (nothing raises it above default), so
    /// the assertions are upper-bounded and robust regardless of a smaller
    /// racing value. This test goes through the legacy `execute_tool` path
    /// (`run_tool`/`run_tool_with_floor` below), which has no spill store —
    /// the no-spill-id elision marker branch, not the `spill:<id>` one.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_command_output_over_budget_is_token_capped() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        // ~350k chars of output — well over the default ~40k-char budget.
        let out = run_tool(
            "run_command",
            serde_json::json!({"command": "seq 1 60000"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert!(
            out.len() < 41_500,
            "model-facing output capped near the ~40k-char budget, got {} bytes",
            out.len()
        );
        assert!(
            out.contains("chars elided (head+tail shown"),
            "carries the head+tail elision marker: {:?}",
            &out[..out.len().min(400)]
        );
        // #945: the HEAD survives — the earliest lines are still visible.
        assert!(
            out.starts_with("1\n2\n3\n"),
            "head preserved: {:?}",
            &out[..out.len().min(160)]
        );
        // #945 (the regression this test now guards): the TAIL survives too —
        // under the old head-only cap this was the first assertion to break
        // (it asserted the OPPOSITE: `!out.contains("60000")`).
        assert!(
            out.trim_end().ends_with("60000"),
            "tail preserved, not dropped by the cap: {:?}",
            &out[out.len().saturating_sub(160)..]
        );
    }

    // --- #307 floor property: preset clamp WINS over --disable-ocap -------

    /// Unit-cover every branch of the bypass-floor predicate.
    #[test]
    fn exec_floor_permits_covers_each_branch() {
        use crate::caveats::Scope;
        // No floor ⇒ always permit (bit-for-bit pre-#307).
        assert!(exec_floor_permits(None, "rm -rf /"));
        // Empty command ⇒ let it through to the normal path.
        let only_echo = Scope::only(["echo".to_string()]);
        assert!(exec_floor_permits(Some(&only_echo), ""));
        // In-floor simple command ⇒ permitted.
        assert!(exec_floor_permits(Some(&only_echo), "echo hi"));
        // Out-of-floor program ⇒ refused.
        assert!(!exec_floor_permits(Some(&only_echo), "rm hi"));
        // Compound command ⇒ refused even with an allow-listed leading token.
        assert!(!exec_floor_permits(Some(&only_echo), "echo hi && rm x"));
        assert!(!exec_floor_permits(Some(&only_echo), "echo a | tee b"));
        assert!(!exec_floor_permits(Some(&only_echo), "echo $(rm x)"));
        // `Scope::All` floor permits any simple command.
        let all: Scope<String> = Scope::All;
        assert!(exec_floor_permits(Some(&all), "anything goes"));
        assert!(!exec_floor_permits(Some(&all), "anything; sneaky"));
    }

    /// ADVERSARIAL PROBE (review #312): exhaustively attack `exec_floor_permits`
    /// with EVERY shell injection / compound form so the floor is proven against
    /// more than just `&&`. An `echo`-only floor must refuse to bypass for any
    /// form that could chain or substitute a second program.
    #[test]
    fn exec_floor_refuses_every_metacharacter_form() {
        use crate::caveats::Scope;
        let echo = Scope::only(["echo".to_string()]);
        // Each of these begins with the allow-listed `echo` but smuggles or
        // could smuggle a second program. None may bypass.
        let attacks = [
            "echo ok && rm -rf /tmp/x", // && and
            "echo ok || rm -rf /tmp/x", // || or
            "echo ok ; rm -rf /tmp/x",  // ; sequence
            "echo ok | sh",             // | pipe
            "echo ok|sh",               // | no spaces
            "echo $(rm x)",             // $() command substitution
            "echo ${IFS}rm",            // ${} parameter expansion
            "echo `rm x`",              // backtick substitution
            "echo ok & rm x",           // & background
            "echo ok > /etc/passwd",    // > redirect out
            "echo ok >> /etc/passwd",   // >> append
            "echo < /etc/shadow",       // < redirect in
            "echo ok 2> err",           // 2> fd redirect (contains >)
            "(rm x)",                   // ( subshell
            "echo ok\nrm -rf /tmp/x",   // newline-separated
            "echo ok\nrm x\n",          // trailing newline
        ];
        for a in attacks {
            assert!(
                !exec_floor_permits(Some(&echo), a),
                "metacharacter form must NOT bypass the floor: {a:?}"
            );
        }
        // Forms with NO shell metacharacter that should still be refused because
        // the LEADING TOKEN is not the allow-listed program:
        let leading_token_attacks = [
            "rm -rf /tmp/x", // plain out-of-floor program
            "FOO=bar rm x",  // env-prefix: leading token `FOO=bar` ∉ floor
            "/bin/echo ok",  // path form: `/bin/echo` ≠ `echo` (exact match)
            "  rm x",        // leading whitespace, still `rm`
            "env rm x",      // `env` wrapper, leading token `env` ∉ floor
            "bash -c rm",    // `bash` ∉ floor
        ];
        for a in leading_token_attacks {
            assert!(
                !exec_floor_permits(Some(&echo), a),
                "out-of-floor leading token must be refused: {a:?}"
            );
        }
        // Sanity: a bare in-floor command with only a benign arg DOES bypass —
        // the floor is a ceiling, not a blanket off-switch. (A dangerous arg to
        // a permitted program is the user's accepted risk: they allow-listed it.)
        assert!(exec_floor_permits(Some(&echo), "echo hello world"));
        assert!(exec_floor_permits(Some(&echo), "echo -n trailing"));
    }

    /// FLOOR TEST (a) — the security contract: with `--disable-ocap` asserted,
    /// an exec FLOOR that denies the command must STOP the unconfined bypass.
    /// `echo` is outside a readonly floor (`exec = none`), so even with yolo on
    /// it does NOT run on the host shell — it falls through to the confined
    /// dispatch, which (env-seam real shell) DENIES it. A deliberately
    /// restricted triage mode is NOT un-clamped by `--yolo`.
    #[tokio::test]
    async fn floor_blocks_disable_ocap_for_a_denied_exec() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        // #1243 Leg 1: pin safe-subset — this asserts the exec FLOOR blocks the
        // bypass (engine-independent); `echo` is a brush builtin, so the default
        // engine would run it unspawned and make the floor test box-dependent.
        let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        // A readonly-triage preset clamp: exec denies everything.
        let floor = crate::NamedPermissionPreset {
            readonly: true,
            ..Default::default()
        }
        .clamp();
        let out = run_tool_with_floor(
            "run_command",
            serde_json::json!({"command": "echo should-not-run"}),
            ws.path(),
            &caveats,
            Some(&floor.exec),
        )
        .await;
        // The bypass did NOT fire: the command never reached the host shell, so
        // it fell to the confined dispatch and was denied, not `should-not-run\n`.
        assert_ne!(out, "should-not-run\n", "the floor must block the bypass");
        assert!(
            out.contains("capability denied"),
            "fell to confined dispatch and was denied, got: {out}"
        );
    }

    /// FLOOR TEST (a, positive) — a command INSIDE the floor still takes the
    /// fast unconfined path under `--disable-ocap`. The floor is a ceiling, not
    /// a blanket off-switch: an explicitly allow-listed command runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn floor_allows_disable_ocap_for_an_in_floor_exec() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        // A triage preset that allow-lists `echo`.
        let floor = crate::NamedPermissionPreset {
            readonly: true,
            exec_allow: vec!["echo".to_string()],
            ..Default::default()
        }
        .clamp();
        let out = run_tool_with_floor(
            "run_command",
            serde_json::json!({"command": "echo in-floor-ok"}),
            ws.path(),
            &caveats,
            Some(&floor.exec),
        )
        .await;
        assert_eq!(out, "in-floor-ok\n", "in-floor command runs unconfined");
    }

    /// FLOOR conservatism — a COMPOUND command never bypasses under an active
    /// floor, even if its leading token is allow-listed: `echo ok && rm -rf /`
    /// must not smuggle `rm` past an `echo` grant. It falls to the confined
    /// shell (env-seam real shell ⇒ denied), which gates each spawn.
    #[tokio::test]
    async fn floor_refuses_bypass_for_a_compound_command() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        // #1243 Leg 1: pin safe-subset so this confined-denial assertion is
        // deterministic — shell_engine() reads NEWT_SHELL_ENGINE FIRST, so the
        // test is immune to a NEWT_FULL_ACCESS leak from a concurrent test
        // (which on Windows would select brush, whose `echo` builtin runs
        // un-gated instead of the whole compound being atomically denied).
        let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        // `echo` is allow-listed, but the `&&` chains an unlisted `rm`.
        let floor = crate::NamedPermissionPreset {
            readonly: true,
            exec_allow: vec!["echo".to_string()],
            ..Default::default()
        }
        .clamp();
        let out = run_tool_with_floor(
            "run_command",
            serde_json::json!({"command": "echo ok && rm -rf /tmp/x"}),
            ws.path(),
            &caveats,
            Some(&floor.exec),
        )
        .await;
        assert_ne!(out, "ok\n", "a compound command must not bypass the floor");
        // Compound ⇒ never bypasses; it falls to the confined shell, which
        // (env-seam real shell) denies the ungranted command under `exec = none`.
        assert!(
            out.contains("capability denied"),
            "fell to confined dispatch and was denied, got: {out}"
        );
    }

    /// FLOOR TEST (c) — `None` floor is bit-for-bit the pre-#307 bypass: a
    /// denied-by-caveats command still runs unconfined under `--disable-ocap`,
    /// proving the floor is opt-in and the no-preset case is unchanged.
    #[cfg(unix)]
    #[tokio::test]
    async fn no_floor_keeps_disable_ocap_bit_for_bit() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_tool_with_floor(
            "run_command",
            serde_json::json!({"command": "echo no-floor-ok"}),
            ws.path(),
            &caveats,
            None,
        )
        .await;
        assert_eq!(out, "no-floor-ok\n", "no floor ⇒ bypass unchanged");
    }

    /// Envelope parity (#297): the host-shell envelope is structurally
    /// identical to the bridle one — `exit_code` / `stdout` / `stderr` /
    /// `sandbox_kind`, `denied`/`denials` omitted (⇒ not denied) — so the
    /// existing envelope readers apply unchanged.
    #[cfg(unix)]
    #[tokio::test]
    async fn host_shell_envelope_matches_the_bridle_shape() {
        let ws = tempfile::TempDir::new().unwrap();
        let envelope = host_shell_dispatch(
            "echo out; echo err >&2; exit 3",
            &ws.path().to_string_lossy(),
            None,
        )
        .await
        .expect("host shell runs");
        assert_eq!(envelope["exit_code"], 3);
        assert_eq!(envelope["stdout"], "out\n");
        assert_eq!(envelope["stderr"], "err\n");
        assert_eq!(envelope["sandbox_kind"], "none");
        // Omitted exactly as the bridle envelope omits them on the
        // nothing-was-denied path — `envelope_denied` reads it natively.
        assert!(envelope.get("denied").is_none(), "got: {envelope}");
        assert!(envelope.get("denials").is_none(), "got: {envelope}");
        assert!(!envelope_denied(&envelope));
        // And the shared formatter renders it like any confined result.
        assert_eq!(
            shell_envelope_output(&envelope, 20, false, false, None, None),
            "out\nerr\n"
        );
    }

    #[test]
    fn decode_shell_stream_preserves_valid_utf8() {
        let text = "// ── Model — test ──\n";
        assert_eq!(decode_shell_stream(text.as_bytes()), text);
    }

    #[test]
    fn decode_shell_stream_repairs_bsd_cat_v_utf8_notation() {
        // This is what macOS/BSD `cat -v` emits for "─ —\n" in a UTF-8
        // locale: the leading e2 byte is raw, while continuation bytes are
        // rendered as M-^T/M-^@ etc. A lossy decode would display
        // "�M-^TM-^@ �M-^@M-^T".
        let cat_v = b"\xe2M-^TM-^@ \xe2M-^@M-^T\n";
        assert_eq!(decode_shell_stream(cat_v), "─ —\n");
    }

    #[test]
    fn decode_shell_stream_repairs_two_byte_bsd_cat_v_notation() {
        // "é" is c3 a9; BSD `cat -v` leaves c3 raw and renders a9 as M-).
        let cat_v = b"caf\xc3M-)\n";
        assert_eq!(decode_shell_stream(cat_v), "café\n");
    }

    /// The venv/PATH prefix logic rides the HOST-BYPASS path unchanged: the
    /// `export VIRTUAL_ENV=…; export PATH=…;` prefix is prepended to the
    /// `--yolo` command, which runs on a real `/bin/sh` where `export` works.
    /// (The confined path no longer gets the prefix — it uses the env seam;
    /// see `confined_dispatch_uses_env_seam_not_export_prefix_783`.)
    #[cfg(unix)]
    #[tokio::test]
    async fn yolo_keeps_the_venv_prefix_logic() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let _venv = EnvVar::set("NEWT_VENV", "/opt/fake-venv");
        let _virtual = EnvVar::unset("VIRTUAL_ENV");
        let _paths = EnvVar::unset("NEWT_EXEC_PATHS");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_tool(
            "run_command",
            serde_json::json!({"command": "echo \"$VIRTUAL_ENV\""}),
            ws.path(),
            &caveats,
        )
        .await;
        assert_eq!(out, "/opt/fake-venv\n");
    }

    /// In --yolo mode an unrestricted fs mutation prompt must not read EOF as
    /// a human decline. The flag is already an explicit interactive override,
    /// so final write/delete confirms auto-accept instead of auto-skipping.
    #[tokio::test]
    async fn yolo_auto_confirms_unrestricted_write_and_delete_prompts() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats::top();

        let out = run_tool(
            "write_file",
            serde_json::json!({"path": "auto.txt", "content": "ok\n"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert!(out.starts_with("wrote auto.txt"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("auto.txt")).unwrap(),
            "ok\n"
        );

        let out = run_tool(
            "delete_file",
            serde_json::json!({"path": "auto.txt"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert!(out.starts_with("deleted auto.txt"), "got: {out}");
        assert!(
            !ws.path().join("auto.txt").exists(),
            "yolo-confirmed delete must remove the file"
        );
    }

    /// Non-yolo unrestricted fs mutations still ask, but through the
    /// PermissionGate question seam. In the TUI that seam owns
    /// PromptStdinGuard, so cbreak/VMIN=0 stdin cannot auto-answer "not y".
    #[tokio::test]
    async fn unrestricted_write_and_delete_confirm_through_permission_gate() {
        struct ConfirmGate {
            answer: Option<String>,
            questions: Vec<String>,
        }
        impl super::PermissionGate for ConfirmGate {
            fn ask(&mut self, _requests: &[super::PermissionRequest]) -> super::PermissionDecision {
                super::PermissionDecision::Deny
            }
            fn ask_question(&mut self, question: &str) -> Option<String> {
                self.questions.push(question.to_string());
                self.answer.clone()
            }
        }

        let _l = env_lock().await;
        let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats::top();
        let mut gate = ConfirmGate {
            answer: Some("y".to_string()),
            questions: Vec::new(),
        };

        let out = execute_tool(
            "write_file",
            &serde_json::json!({"path": "guarded.txt", "content": "ok\n"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            Some(&mut gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert!(out.starts_with("wrote guarded.txt"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("guarded.txt")).unwrap(),
            "ok\n"
        );

        let out = execute_tool(
            "delete_file",
            &serde_json::json!({"path": "guarded.txt"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            Some(&mut gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert!(out.starts_with("deleted guarded.txt"), "got: {out}");
        assert!(!ws.path().join("guarded.txt").exists());
        assert_eq!(
            gate.questions,
            vec![
                "Write this file? [y/N]".to_string(),
                "Delete this file? [y/N]".to_string()
            ]
        );
    }

    /// #783 regression (Bug A): the confined-shell dispatch carries the RAW
    /// user command and the venv via agent-bridle's structured `env` seam — NOT
    /// an `export …;` prefix on `cmd`. The old code built
    /// `{ "cmd": cmd_with_venv, "cwd": … }` (the prefixed form), and the
    /// confined safe-subset engine refuses an `export` builtin on a compound
    /// command, which is the bug. Pure: builds the dispatch args only, no spawn.
    #[tokio::test]
    async fn confined_dispatch_uses_env_seam_not_export_prefix_783() {
        let _l = env_lock().await;
        let _venv = EnvVar::set("NEWT_VENV", "/opt/fake-venv");
        let _virtual = EnvVar::unset("VIRTUAL_ENV");
        let _paths = EnvVar::unset("NEWT_EXEC_PATHS");

        // The literal failing case from #783.
        let cmd = "hostname; sw_vers 2>/dev/null | head -1; uname -s";
        let args = confined_dispatch_args(cmd, "/work/dir");

        // The command is passed RAW — no `export …;` prefix smuggled in.
        assert_eq!(args["cmd"], cmd);
        assert!(
            !args["cmd"]
                .as_str()
                .expect("cmd is a string")
                .contains("export "),
            "confined cmd must not carry an export prefix: {args}"
        );
        assert_eq!(args["cwd"], "/work/dir");

        // The venv rides the env seam: VIRTUAL_ENV + venv bin prepended to PATH.
        assert_eq!(args["env"]["VIRTUAL_ENV"], "/opt/fake-venv");
        let path = args["env"]["PATH"].as_str().expect("PATH in the env seam");
        assert!(
            path.starts_with("/opt/fake-venv/bin"),
            "venv bin must be prepended to PATH: {path}"
        );
    }

    /// #783: with neither venv input set, the env seam is empty (no spurious
    /// VIRTUAL_ENV / PATH keys) — the no-venv invocation is unaffected.
    #[tokio::test]
    async fn confined_dispatch_env_seam_without_venv_783() {
        let _l = env_lock().await;
        let _venv = EnvVar::unset("NEWT_VENV");
        let _virtual = EnvVar::unset("VIRTUAL_ENV");
        let _paths = EnvVar::unset("NEWT_EXEC_PATHS");
        let _pass = EnvVar::unset("NEWT_SHELL_ENV_PASSTHROUGH"); // ⇒ default HOME+USER
        let _home = EnvVar::set("HOME", "/home/testuser");

        let args = confined_dispatch_args("ls -la", "/work/dir");
        assert_eq!(args["cmd"], "ls -la");
        let env = &args["env"];
        // #783: without a venv, no VIRTUAL_ENV / PATH override is injected...
        assert!(
            env.get("VIRTUAL_ENV").is_none(),
            "no venv ⇒ no VIRTUAL_ENV: {args}"
        );
        assert!(
            env.get("PATH").is_none(),
            "no venv/exec-paths ⇒ no PATH override: {args}"
        );
        // ...but HOME now passes through so brush can expand `~` (the confined
        // shell had NO env before, so `~` stayed literal and left `~/…` debris),
        // and SHELL identifies the confined engine.
        assert_eq!(
            env["HOME"], "/home/testuser",
            "HOME must pass through: {args}"
        );
        assert!(
            env.get("SHELL").is_some(),
            "SHELL must identify the confined engine: {args}"
        );
    }

    /// fs fence under yolo (#297): the newt-native workspace fence is NOT
    /// bypassed — a write/read outside the granted scope keeps the standard
    /// denial bit-for-bit. Yolo is unconfined exec, never authority-off.
    #[tokio::test]
    async fn yolo_keeps_the_fs_workspace_fence() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let escape = "/definitely-outside-the-fence/escape.txt";
        let out = run_tool(
            "write_file",
            serde_json::json!({"path": escape, "content": "nope"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_write", escape));
        assert!(!std::path::Path::new(escape).exists());

        let out = run_tool(
            "delete_file",
            serde_json::json!({"path": escape}),
            ws.path(),
            &caveats,
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_write", escape));

        let out = run_tool(
            "read_file",
            serde_json::json!({"path": "/etc/hostname"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert_eq!(out, denied_fs_result("fs_read", "/etc/hostname"));
    }

    /// Precedence (#297): with both `--disable-ocap` and a #263 gate present,
    /// exec never prompts — nothing is denied, so the gate is structurally
    /// unreachable for run_command. (fs prompting stays live; the fs-fence
    /// test above and the #263 suite cover that axis.)
    #[cfg(unix)]
    #[tokio::test]
    async fn yolo_never_consults_the_permission_gate_for_exec() {
        struct PanicGate;
        impl super::PermissionGate for PanicGate {
            fn ask(&mut self, requests: &[super::PermissionRequest]) -> super::PermissionDecision {
                panic!("yolo exec must never prompt, but the gate was asked: {requests:?}");
            }
            fn ask_question(&mut self, question: &str) -> Option<String> {
                panic!("yolo exec must never prompt, but the gate was asked: {question:?}");
            }
        }
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let mut gate = PanicGate;
        let out = execute_tool(
            "run_command",
            &serde_json::json!({"command": "echo no-prompt"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            Some(&mut gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert_eq!(out, "no-prompt\n");
    }

    /// The corrective tool-name guard still answers BEFORE the bypass: yolo
    /// changes where commands run, not what counts as a command.
    #[tokio::test]
    async fn yolo_keeps_the_tool_name_corrective_guard() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_tool(
            "run_command",
            serde_json::json!({"command": "read_file foo.txt"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert!(out.contains("is a tool, not a shell command"), "got: {out}");
    }

    // --- facade P4 (#780): hidden tool-call routing dispatch ---------------

    /// A git stub that proves *which path served the call*: a routed
    /// `git status` lands here as op `status`; a routed write op would surface
    /// the unexpected-op error (so a test can assert it was NOT routed).
    struct RoutingStubGit;
    impl crate::agentic::GitTool for RoutingStubGit {
        fn dispatch(
            &self,
            op: &str,
            _args: &serde_json::Value,
            _caps: &crate::git_caveats::GitCaveats,
        ) -> Result<String, String> {
            match op {
                "status" => Ok("on branch main (routed via git built-in)".to_string()),
                other => Err(format!("unexpected routed git op '{other}'")),
            }
        }
    }

    async fn run_routed_with_git(command: &str, ws: &std::path::Path, caveats: &Caveats) -> String {
        execute_tool(
            "run_command",
            &serde_json::json!({ "command": command }),
            &ws.to_string_lossy(),
            false,
            20,
            caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            None, // permission_gate
            None, // exec_floor
            Some(&RoutingStubGit as &dyn crate::agentic::GitTool),
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await
    }

    /// The routing switch reads fail-closed (only the exact `1`), and it is a
    /// DISTINCT mechanism from `ocap_disabled` (§7-F5): asserting `NEWT_NO_ROUTE`
    /// never moves `ocap_disabled`, and asserting `NEWT_DISABLE_OCAP` never moves
    /// `routing_disabled`. The two switches can never alias.
    #[test]
    fn routing_disabled_requires_exactly_1_and_is_independent_of_ocap() {
        let _l = ENV_LOCK.blocking_lock();
        let _no_ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        {
            let _unset = EnvVar::unset("NEWT_NO_ROUTE");
            assert!(!routing_disabled(), "absent ⇒ routing stays on");
        }
        for (value, expected) in [("1", true), ("0", false), ("", false), ("true", false)] {
            let _set = EnvVar::set("NEWT_NO_ROUTE", value);
            assert_eq!(routing_disabled(), expected, "NEWT_NO_ROUTE={value:?}");
            // F5: turning routing off NEVER turns on the L3-off unconfine.
            assert!(
                !ocap_disabled(),
                "NEWT_NO_ROUTE must not imply --disable-ocap"
            );
        }
        // And the inverse: --disable-ocap must not imply --no-route.
        let _unset_route = EnvVar::unset("NEWT_NO_ROUTE");
        let _on_ocap = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        assert!(ocap_disabled());
        assert!(
            !routing_disabled(),
            "--disable-ocap must not imply --no-route"
        );
    }

    /// TDD: a routed read goes through the SAME fs floor — routing is NOT a
    /// bypass. An out-of-scope `cat /etc/shadow` routes to `read_file` and is
    /// denied by `fs_read` exactly as a direct `read_file` would be (the denial
    /// short-circuits before any real fs access). The `fs_read` denial wording
    /// also proves it reached the `read_file` arm (vs. the exec/shell path).
    #[tokio::test]
    async fn routed_cat_goes_through_the_fs_floor_not_a_bypass() {
        let _l = env_lock().await;
        let _route_on = EnvVar::unset("NEWT_NO_ROUTE");
        let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
        // #1243 Leg 1: pin safe-subset (deterministic confined engine) so a
        // concurrent NEWT_FULL_ACCESS leak can't flip this to brush on Windows.
        let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path()); // fs_read scoped to ws only
        let out = run_tool(
            "run_command",
            serde_json::json!({ "command": "cat /etc/shadow" }),
            ws.path(),
            &caveats,
        )
        .await;
        assert!(
            out.contains("capability denied: fs_read does not permit")
                && out.contains("/etc/shadow"),
            "routed cat must hit the fs floor, not run unconfined; got: {out}"
        );
    }

    /// TDD: read-only `git status` is silently routed to the governed `git`
    /// built-in (the stub proves the built-in served it). Revert the routing
    /// promotion and this is red — the command would instead hit the run_command
    /// corrective guard.
    #[tokio::test]
    async fn routed_git_status_dispatches_through_the_git_builtin() {
        let _l = env_lock().await;
        let _route_on = EnvVar::unset("NEWT_NO_ROUTE");
        let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_routed_with_git("git status", ws.path(), &caveats).await;
        assert!(
            out.contains("routed via git built-in"),
            "git status must route to the governed git built-in; got: {out}"
        );
    }

    /// #1022: `run_command("rm file")` routes to the governed delete_file arm,
    /// so deletion works under fs_write without requiring raw shell `rm`.
    #[tokio::test]
    async fn routed_rm_dispatches_through_delete_file() {
        let _l = env_lock().await;
        let _route_on = EnvVar::unset("NEWT_NO_ROUTE");
        let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("stale.txt"), "remove me\n").unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_tool(
            "run_command",
            serde_json::json!({ "command": "rm stale.txt" }),
            ws.path(),
            &caveats,
        )
        .await;
        assert!(out.starts_with("deleted stale.txt"), "got: {out}");
        assert!(
            !ws.path().join("stale.txt").exists(),
            "routed rm must remove the file through delete_file"
        );
    }

    /// TDD: state-modifying `git add` is GATED as exec — NOT silently routed
    /// (owner decision 2). It never reaches the git built-in (no unexpected-op
    /// error from the stub); it falls through to the normal run_command path.
    #[tokio::test]
    async fn state_modifying_git_add_is_not_routed() {
        let _l = env_lock().await;
        let _route_on = EnvVar::unset("NEWT_NO_ROUTE");
        let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_routed_with_git("git add a.txt", ws.path(), &caveats).await;
        assert!(
            !out.contains("routed"),
            "git add must NOT route to the git built-in; got: {out}"
        );
        // It falls through to the run_command path (git ∈ DIRECT_TOOL_NAMES ⇒
        // the existing corrective guard), never silently routed.
        assert!(out.contains("is a tool, not a shell command"), "got: {out}");
    }

    /// F5 (§7-F5): `--no-route` bypasses routing but NEVER disables L3. With
    /// `NEWT_NO_ROUTE=1`, the same out-of-bounds `cat` is no longer routed to
    /// `read_file` (no `fs_read` denial), yet it does NOT run unconfined — it
    /// falls to the confined shell (env-seam real shell ⇒ denied), and
    /// `ocap_disabled()` stays false. The boundary holds.
    #[tokio::test]
    async fn no_route_bypasses_routing_but_keeps_l3() {
        let _l = env_lock().await;
        let _route_off = EnvVar::set("NEWT_NO_ROUTE", "1");
        let _ocap_off = EnvVar::unset("NEWT_DISABLE_OCAP");
        // #1243 Leg 1: pin safe-subset (deterministic confined engine) so a
        // concurrent NEWT_FULL_ACCESS leak can't flip this to brush on Windows.
        let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        assert!(routing_disabled() && !ocap_disabled(), "L2 off, L3 on");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_tool(
            "run_command",
            serde_json::json!({ "command": "cat /etc/shadow" }),
            ws.path(),
            &caveats,
        )
        .await;
        // Routing was OFF ⇒ NOT rewritten to read_file (no fs_read denial)…
        assert!(
            !out.contains("fs_read does not permit"),
            "--no-route must not route to read_file; got: {out}"
        );
        // …and the command did NOT run unconfined: it took the confined shell
        // (env-seam real shell ⇒ denied — the L3 boundary held).
        assert!(
            out.contains("capability denied"),
            "the L3 confined dispatch must still gate the command; got: {out}"
        );
    }
}
