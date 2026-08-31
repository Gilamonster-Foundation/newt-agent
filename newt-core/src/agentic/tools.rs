//! Built-in tool definitions and the tool executor for the agentic loop.
//! Moved verbatim from `newt-tui` in Step 9.7 — the Caveats enforcement,
//! shrink guard, build-check feedback, and agent-bridle routing are unchanged.
// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:15 EDT | Date: 2026-08-12

use super::artifact_read::{execute_artifact_read_silent, ArtifactReadContext};
use super::content_spill::{self, SpillStore};
use super::crew_tool::CrewRunner;
use super::display::{ToolDisplay, ToolPresentation};
use super::git_tool::GitTool;
use super::mcp::{classify_mcp_effect, leash_mcp_call, McpEffect, McpGrant, McpTools};
use super::memory_fetch::{execute_memory_fetch, memory_fetch_tool_definition, MemorySource};
use super::note_sink::{execute_save_note, save_note_tool_definition, NoteSink};
use super::permissions::{
    DenialKind, HumanQuestionOutcome, PermissionDecision, PermissionGate, PermissionRequest,
};
use super::prompt_intake::PromptDisposition;
use super::prompt_read::{execute_prompt_read_silent, PromptReadContext};
use super::recall::{execute_recall, recall_tool_definition, RecallSource};
use super::report::{execute_render_report, render_report_tool_definition};
use crate::caveats::CaveatsExt as _;
use crate::PermissionAction;
#[cfg(test)]
use output_budget::DEFAULT_MAX_OUTPUT_TOKENS;
#[cfg(test)]
use output_budget::DEFAULT_OUTPUT_CAP_CHARS_PER_TOKEN;
use output_budget::{
    cap_model_output, cap_model_output_with_handle, max_output_tokens, output_head_tokens,
    paginate_read,
};
pub use output_budget::{
    set_max_output_tokens, set_output_cap_chars_per_token, set_output_head_tokens,
};

mod catalog;
pub(crate) mod exposure;
mod live_output;
mod output_budget;
/// Real-resource (PTY) proof of the tool-call liveness contract (#1727): a
/// silent tool is never a blank row, and the first live byte takes the row
/// for good. Unix-only — it needs a real pty pair.
#[cfg(all(test, unix))]
mod tool_spinner_pty_test;
use live_output::{LiveOutputRelay, LiveOutputSession, ToolSpinner};

#[cfg(test)]
use catalog::lifecycle_tool_definition;
pub(crate) use catalog::{
    classify_gated_off_reach, classify_phantom_reach, is_context_remaining_call, is_hallucination,
    known_builtin_tool_name, merged_tool_definitions, resolve_tool_alias, AliasOutcome,
};
use catalog::{
    disposition_tool_denied_message, persona_tool_denied_message,
    run_command_creates_shell_git_commit, run_command_redirect, unknown_tool_message,
};
pub use catalog::{
    filter_advertised_tools, filter_tools_for_disposition, persona_tool_allowed, tool_allowed,
    tool_definitions,
};
#[cfg(test)]
use catalog::{
    levenshtein, nearest_tool_name, ALL_TOOL_NAMES, BASE_TOOL_NAMES, EXTENDED_TOOL_REGISTRY,
};
pub use exposure::ExposureSettings;
pub(crate) use exposure::{select_exposed, select_openai_compatible_tools};
/// Build a shell prefix that exports venv/exec-path vars into the agent-bridle
/// confined shell.
///
/// Agent-bridle's confined shell does not inherit the host environment
/// (`do_not_inherit_env(true)`), so we inject `VIRTUAL_ENV` and prepend
/// venv/extra `bin/` dirs to `PATH` by prefixing every `run_command` cmd.
/// `NEWT_VENV` (set from `--venv` or auto-detected from `$VIRTUAL_ENV` by the
/// CLI) takes precedence; falls back to `$VIRTUAL_ENV` if the TUI was invoked
/// directly without going through the CLI's `dispatch`.
/// Atomically validate a model-emitted tool call **before any side effect**
/// (invariant #3: no malformed tool call reaches a tool). Both the name and the
/// arguments are checked up front; the caller receives EITHER a ready-to-dispatch
/// `(name, object-args)` pair OR a human-readable reason the call is malformed —
/// and on the malformed branch it must echo the reason back to the model and
/// execute nothing.
///
/// This replaces the `serde_json::from_str(s).unwrap_or(Value::Null)` coercion
/// that used to sit at three separate dispatch sites (both chat loops + the
/// Responses loop): a garbled or truncated `arguments` string was silently turned
/// into `null` and the tool ran anyway with empty/wrong input. Routing every site
/// through this one gate makes that class of bug unrepresentable — a malformed
/// call cannot produce a `(name, args)` pair to execute.
///
/// Rules:
/// - `name` must be a present, non-blank string.
/// - `arguments` must resolve to a JSON **object**: an object passes through;
///   `null`/absent and an empty/whitespace string mean "no arguments" (`{}`); a
///   non-empty string is parsed and must yield an object; anything else (an
///   unparseable string, or a JSON scalar/array) is malformed. A parse failure is
///   NEVER coerced to `null`.
pub(crate) fn validate_tool_call(
    name: Option<&str>,
    raw_args: &serde_json::Value,
) -> Result<(String, serde_json::Value), String> {
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "tool call is missing a name".to_string())?;
    let args = match raw_args {
        serde_json::Value::Null => serde_json::json!({}),
        serde_json::Value::Object(_) => raw_args.clone(),
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                serde_json::json!({})
            } else {
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(v @ serde_json::Value::Object(_)) => v,
                    Ok(_) => {
                        return Err(format!(
                            "tool '{name}' arguments must be a JSON object, but the model sent a non-object JSON value"
                        ))
                    }
                    Err(e) => {
                        return Err(format!(
                            "tool '{name}' arguments are not valid JSON (call truncated or malformed): {e}"
                        ))
                    }
                }
            }
        }
        other => {
            return Err(format!(
                "tool '{name}' arguments must be a JSON object, got {other}"
            ))
        }
    };
    Ok((name.to_string(), args))
}

/// One validated tool call, ready to dispatch.
pub(crate) struct ValidatedCall {
    pub call_id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Why a whole tool-call batch was rejected. The two classes call for DIFFERENT
/// recovery — one is recoverable on the wire, one is not:
#[derive(Debug)]
pub(crate) enum BatchRejection {
    /// A call's id is missing, blank, or duplicated — a tool result **cannot** be
    /// correlated back to its call. There is no valid recovery message to send,
    /// so the caller MUST abort the turn (no fabricated outputs, no follow-up).
    /// Fabricating an empty/duplicate id only yields a provider 400 or a silent
    /// mispairing.
    CorrelationImpossible(String),
    /// Correlation is intact (every id is present + unique, or the wire carries
    /// no ids at all), but a call's name/arguments is invalid. The caller MAY
    /// echo a synthetic rejection keyed by each (valid) id and re-dispatch, so
    /// the model can retry with a well-formed call.
    ContentInvalid(String),
}

impl BatchRejection {
    /// The human-readable reason, whichever class.
    pub(crate) fn reason(&self) -> &str {
        match self {
            Self::CorrelationImpossible(r) | Self::ContentInvalid(r) => r,
        }
    }
}

/// Validate an ENTIRE batch of model-emitted tool calls **before any execution**
/// (invariant #3, at the batch level). A single response can carry several calls;
/// validating-then-executing one at a time lets a valid *mutating* call run
/// before a later sibling is found malformed. This checks the whole batch up
/// front and returns `Err` if ANY call is bad, so the caller executes ZERO calls
/// from an unvalidated response — no sibling mutates the workspace ahead of the
/// batch being known good.
///
/// Wire shapes differ (Responses vs the two chat forms), so each call is passed
/// pre-extracted as `(call_id, name, raw_args)`. **Correlation is checked FIRST**
/// — when `require_call_id` (the id-carrying wires: Responses `call_id`/`id`,
/// chat `tool_call_id`), every call must have a **non-empty, unique** id, else
/// [`BatchRejection::CorrelationImpossible`] (unrecoverable — the caller aborts).
/// Only then is each call's name/arguments validated ([`validate_tool_call`]); a
/// bad one yields [`BatchRejection::ContentInvalid`] (recoverable — ids are known
/// good, so a rejection can be correctly correlated). Order is preserved.
pub(crate) fn validate_tool_call_batch(
    calls: &[(Option<&str>, Option<&str>, &serde_json::Value)],
    require_call_id: bool,
) -> Result<Vec<ValidatedCall>, BatchRejection> {
    // 1. Correlation first: an id problem is unrecoverable and must abort before
    //    we even consider content (a follow-up on a mis-keyed transcript is worse
    //    than aborting the turn).
    if require_call_id {
        let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (i, &(call_id, _, _)) in calls.iter().enumerate() {
            let n = i + 1;
            let id = call_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    BatchRejection::CorrelationImpossible(format!(
                        "tool call #{n} is missing a call id — its result cannot be correlated"
                    ))
                })?;
            if !seen_ids.insert(id) {
                return Err(BatchRejection::CorrelationImpossible(format!(
                    "tool call #{n} repeats call id {id:?} — ambiguous result routing"
                )));
            }
        }
    }
    // 2. Content: name + object arguments for every call (ids are now known good).
    let mut validated = Vec::with_capacity(calls.len());
    for (i, &(call_id, name, raw_args)) in calls.iter().enumerate() {
        let n = i + 1;
        let (name, args) = validate_tool_call(name, raw_args)
            .map_err(|e| BatchRejection::ContentInvalid(format!("tool call #{n}: {e}")))?;
        let call_id = call_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        validated.push(ValidatedCall {
            call_id,
            name,
            args,
        });
    }
    Ok(validated)
}

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
    // Reads the process's FROZEN `LaunchAuthority` (resolved once from
    // `NEWT_DISABLE_OCAP` near startup), never the live env — so a switch that
    // appears after startup cannot widen authority mid-process
    // (`noninteractive-launch-policy`). `launch_authority::from_env` is the sole
    // env reader.
    crate::launch_authority::current().ocap_disabled()
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
    // Frozen `LaunchAuthority` (resolved once from `NEWT_FULL_ACCESS` at
    // startup), never the live env — a later-appearing switch cannot widen the
    // session preset mid-process. See [`crate::launch_authority`].
    crate::launch_authority::current().full_access()
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

/// The read-only authority a plan phase clamps the session to (#1193): reads
/// everywhere, but NO writes, NO exec, NO net. MEETing this into the session
/// caveats enforces "planning is read-only" — the design's safety guarantee,
/// not the model's good intentions. The call-count and generation bounds stay
/// permissive here; the TUI `meet` still preserves any tighter session limits.
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
fn exec_floor_permits(floor: Option<&crate::caveats::Scope<String>>, cmd: &str) -> bool {
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
async fn dispatch_bridled_shell(
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
const CHILD_STRIPPED_AUTHORITY_ENV: &[&str] = &[
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
fn host_shell_command(program: &str, cmd: &str, cwd: &str) -> tokio::process::Command {
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
async fn host_shell_output_with_timeout(
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

// The lexical-normalisation + prefix-containment helpers now live in one shared
// place — `crate::caveats` — so the interactive tool gate here and the headless
// `newt-coder` apply path decide containment identically (no drift surface).
// Only the Linux object-bound helpers below normalise paths directly; the
// prefix gate itself goes through `crate::caveats::permits_path`.
#[cfg(target_os = "linux")]
use crate::caveats::lexically_normalize;

/// Returns true if `full_path` is permitted by `scope`, under prefix
/// (containment) semantics. Thin alias for [`crate::caveats::permits_path`],
/// kept so the many call sites in this module read as the tool-gate they are;
/// the containment logic lives in one shared owner.
pub(crate) fn tui_permits_path(scope: &crate::caveats::Scope<String>, full_path: &str) -> bool {
    crate::caveats::permits_path(scope, full_path)
}

/// The root in `scope` that lexically authorises `full_path`, if any.
///
/// `Some(Some(root))` — permitted, and `root` is the granted directory the path
/// must resolve *beneath* (the object-binding anchor). `Some(None)` — permitted
/// with no containing root (`Scope::All`, e.g. `--full-access`), so there is no
/// object fence. `None` — not permitted. Mirrors [`tui_permits_path`]'s matching
/// exactly (same normalisation + `starts_with`), so the object-bound read
/// resolves beneath the very root the gate approved.
#[cfg(target_os = "linux")]
fn authorizing_root<'a>(
    scope: &'a crate::caveats::Scope<String>,
    full_path: &str,
) -> Option<Option<&'a str>> {
    match scope {
        crate::caveats::Scope::All => Some(None),
        crate::caveats::Scope::Only(set) if set.is_empty() => None,
        crate::caveats::Scope::Only(set) => {
            let candidate = lexically_normalize(full_path);
            set.iter()
                .find(|root| candidate.starts_with(lexically_normalize(root)))
                .map(|r| Some(r.as_str()))
        }
    }
}

/// The `..`-free path of `full_path` relative to its authorising `root`. The
/// gate matched `starts_with` on the normalised forms, so this strip succeeds;
/// the result is what [`crate::fs_cap::WorkspaceDir`] resolves beneath the root fd.
#[cfg(target_os = "linux")]
fn contained_relative(full_path: &str, root: &str) -> std::path::PathBuf {
    let cand = lexically_normalize(full_path);
    let nroot = lexically_normalize(root);
    let rel = cand.strip_prefix(&nroot).unwrap_or(&cand);
    // An empty relative path means the target *is* the root (e.g. `list_dir "."`);
    // resolve `.` so `openat2` opens the root dir itself, not an empty path.
    if rel.as_os_str().is_empty() {
        std::path::PathBuf::from(".")
    } else {
        rel.to_path_buf()
    }
}

/// A `WorkspaceDir` open error that means "the object escaped the fence" (the
/// kernel refused the resolve) rather than an ordinary I/O failure. `openat2`
/// returns `EXDEV` for a `RESOLVE_BENEATH` violation and `ELOOP` for a
/// `RESOLVE_NO_MAGICLINKS`/symlink-loop rejection; both are containment denials.
#[cfg(target_os = "linux")]
fn is_fs_containment_denied(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EXDEV) | Some(libc::ELOOP))
}

/// The object-binding target for a scope-authorised fs op: `Some(Some((root,
/// rel)))` to resolve `rel` beneath `root`'s fd; `Some(None)` for `Scope::All`
/// (no fence — the caller uses `std::fs`); `None` if the scope denies (a logic
/// error at a call site that already gated — callers fail closed). One shared
/// resolver behind the object-bound read/list arms.
#[cfg(target_os = "linux")]
fn object_bound_target<'a>(
    scope: &'a crate::caveats::Scope<String>,
    full_str: &str,
) -> Option<Option<(&'a str, std::path::PathBuf)>> {
    authorizing_root(scope, full_str)
        .map(|opt| opt.map(|root| (root, contained_relative(full_str, root))))
}

/// Unconfined directory listing via `std::fs` — the `Scope::All` / non-Linux /
/// #263-gate-approved path. One owner so the three call sites don't drift.
fn std_list_dir(full: &std::path::Path) -> Result<Vec<String>, String> {
    match std::fs::read_dir(full) {
        Ok(entries) => Ok(entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()),
        Err(e) => Err(format!("error: {e}")),
    }
}

/// Object-bound read of `full` (the workspace-joined model path) beneath the
/// root that authorised it. Returns the file contents, or a ready-to-return
/// tool-output string on failure: a containment escape becomes an `fs_read`
/// denial, any other failure the ordinary read error. Under `Scope::All`
/// (`--full-access`) there is no object fence, so it reads via `std::fs` — the
/// pre-existing unconfined behaviour. Linux-only (`openat2`); the non-Linux
/// fallback keeps the lexical-gate + `std::fs` path.
///
/// `axis` labels the denial (`fs_read` for read_file; `fs_write` for edit_file,
/// whose read is authorised by — and contained beneath — the `fs_write` root).
#[cfg(target_os = "linux")]
fn object_bound_read(
    scope: &crate::caveats::Scope<String>,
    axis: &str,
    path: &str,
    full: &std::path::Path,
    full_str: &str,
) -> Result<String, String> {
    use std::io::Read;
    match object_bound_target(scope, full_str) {
        // The gate already permitted this read, so `None` here would be a logic
        // error (the two matchers disagreeing); fail closed rather than read.
        None => Err(denied_fs_result(axis, path)),
        Some(None) => {
            std::fs::read_to_string(full).map_err(|e| format!("error reading {path}: {e}"))
        }
        Some(Some((root, rel))) => {
            let read = crate::fs_cap::WorkspaceDir::open_root(std::path::Path::new(root)).and_then(
                |dir| {
                    let mut f = dir.open(&rel)?;
                    let mut s = String::new();
                    f.read_to_string(&mut s)?;
                    Ok(s)
                },
            );
            match read {
                Ok(s) => Ok(s),
                Err(e) if is_fs_containment_denied(&e) => Err(denied_fs_result(axis, path)),
                Err(e) => Err(format!("error reading {path}: {e}")),
            }
        }
    }
}

/// Object-bound directory listing beneath the authorising root — the `list_dir`
/// analogue of [`object_bound_read`]. A symlink-escape directory is refused by
/// the kernel (an `fs_read` denial); the entries are read straight off the dir
/// fd. `Scope::All` lists via `std::fs`. Linux-only.
#[cfg(target_os = "linux")]
fn object_bound_list(
    scope: &crate::caveats::Scope<String>,
    path: &str,
    full: &std::path::Path,
    full_str: &str,
) -> Result<Vec<String>, String> {
    match object_bound_target(scope, full_str) {
        None => Err(denied_fs_result("fs_read", path)),
        Some(None) => std_list_dir(full),
        Some(Some((root, rel))) => {
            match crate::fs_cap::WorkspaceDir::open_root(std::path::Path::new(root))
                .and_then(|dir| dir.read_dir(&rel))
            {
                Ok(names) => Ok(names
                    .into_iter()
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect()),
                Err(e) if is_fs_containment_denied(&e) => Err(denied_fs_result("fs_read", path)),
                Err(e) => Err(format!("error: {e}")),
            }
        }
    }
}

/// Non-Linux fallback for the object-bound fs arms: `openat2` is unavailable, so
/// they keep the lexical-gate + `std::fs` behaviour (the symlink residual
/// persists on non-Linux — see `fs-canonical-containment`; CI/prod is Linux).
#[cfg(not(target_os = "linux"))]
fn object_bound_read(
    _scope: &crate::caveats::Scope<String>,
    _axis: &str,
    path: &str,
    full: &std::path::Path,
    _full_str: &str,
) -> Result<String, String> {
    std::fs::read_to_string(full).map_err(|e| format!("error reading {path}: {e}"))
}

#[cfg(not(target_os = "linux"))]
fn object_bound_list(
    _scope: &crate::caveats::Scope<String>,
    _path: &str,
    full: &std::path::Path,
    _full_str: &str,
) -> Result<Vec<String>, String> {
    std_list_dir(full)
}

/// Unconfined write via `std::fs` (creating parents) — the `Scope::All` /
/// non-Linux / #263-gate-approved path. One owner so the call sites don't drift.
fn std_write(full: &std::path::Path, path: &str, content: &str) -> Result<(), String> {
    if let Some(parent) = full.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(full, content).map_err(|e| format!("error writing {path}: {e}"))
}

/// Object-bound write of `content` to `full` (the workspace-joined model path)
/// beneath the root that authorised it — the write analogue of
/// [`object_bound_read`]. The file (and any missing parents) is created *beneath*
/// the granted root's fd (`openat2 RESOLVE_BENEATH`), so a symlink / `..` /
/// absolute escape the lexical gate admits is refused by the kernel: a
/// containment escape becomes an `fs_write` denial, any other failure the
/// ordinary write error. `Scope::All` (`--full-access`) writes via `std::fs`.
/// Linux-only; the non-Linux fallback keeps the lexical-gate + `std::fs` path.
#[cfg(target_os = "linux")]
fn object_bound_write(
    scope: &crate::caveats::Scope<String>,
    axis: &str,
    path: &str,
    full: &std::path::Path,
    full_str: &str,
    content: &str,
) -> Result<(), String> {
    use std::io::Write;
    match object_bound_target(scope, full_str) {
        None => Err(denied_fs_result(axis, path)),
        Some(None) => std_write(full, path, content),
        Some(Some((root, rel))) => {
            let write = crate::fs_cap::WorkspaceDir::open_root(std::path::Path::new(root))
                .and_then(|dir| {
                    if let Some(parent) = rel.parent() {
                        if !parent.as_os_str().is_empty() {
                            dir.create_dir_all(parent)?;
                        }
                    }
                    let mut f = dir.create(&rel)?;
                    f.write_all(content.as_bytes())?;
                    Ok(())
                });
            match write {
                Ok(()) => Ok(()),
                Err(e) if is_fs_containment_denied(&e) => Err(denied_fs_result(axis, path)),
                Err(e) => Err(format!("error writing {path}: {e}")),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn object_bound_write(
    _scope: &crate::caveats::Scope<String>,
    _axis: &str,
    path: &str,
    full: &std::path::Path,
    _full_str: &str,
    content: &str,
) -> Result<(), String> {
    std_write(full, path, content)
}

/// Object-bound file removal beneath the authorising root — the `delete_file`
/// analogue of [`object_bound_write`]. The parent is resolved object-bound and
/// the entry removed via `unlinkat`, so a symlink / `..` / absolute escape is
/// refused by the kernel (an `fs_write` denial). `Scope::All` removes via
/// `std::fs`. Linux-only.
#[cfg(target_os = "linux")]
fn object_bound_delete(
    scope: &crate::caveats::Scope<String>,
    path: &str,
    full: &std::path::Path,
    full_str: &str,
) -> Result<(), String> {
    match object_bound_target(scope, full_str) {
        None => Err(denied_fs_result("fs_write", path)),
        Some(None) => std::fs::remove_file(full).map_err(|e| format!("error deleting {path}: {e}")),
        Some(Some((root, rel))) => {
            match crate::fs_cap::WorkspaceDir::open_root(std::path::Path::new(root))
                .and_then(|dir| dir.unlink(&rel))
            {
                Ok(()) => Ok(()),
                Err(e) if is_fs_containment_denied(&e) => Err(denied_fs_result("fs_write", path)),
                Err(e) => Err(format!("error deleting {path}: {e}")),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn object_bound_delete(
    _scope: &crate::caveats::Scope<String>,
    path: &str,
    full: &std::path::Path,
    _full_str: &str,
) -> Result<(), String> {
    std::fs::remove_file(full).map_err(|e| format!("error deleting {path}: {e}"))
}

/// Whether `find`'s recursive-read root is contained to the WORKSPACE. Unlike
/// the other arms, `find` contains to the workspace *independent of the fs_read
/// scope* (even under `Scope::All`) — a recursive read is dangerous, so the
/// search root must stay in-tree. On Linux this is an object-bound
/// `openat2(RESOLVE_BENEATH)` resolve of the root beneath the workspace fd
/// (TOCTOU-free — replaces the old canonicalize-then-`starts_with`); `find` never
/// follows symlinks during descent, so a contained root bounds the whole walk.
#[cfg(target_os = "linux")]
fn find_root_contained(
    _scope: &crate::caveats::Scope<String>,
    workspace: &str,
    full: &std::path::Path,
    _full_str: &str,
) -> bool {
    // The search root relative to the workspace. `full` = `workspace.join(path)`,
    // so a lexical strip yields the model-supplied remainder (`..`, an absolute
    // root, or a real subpath); `openat2` then adjudicates containment.
    let rel = match full.strip_prefix(workspace) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
        Ok(_) => std::path::PathBuf::from("."), // root == workspace
        Err(_) => return false,                 // absolute / not under the workspace
    };
    match crate::fs_cap::WorkspaceDir::open_root(std::path::Path::new(workspace))
        .and_then(|dir| dir.open_dir(&rel))
    {
        Ok(_) => true,
        Err(e) if is_fs_containment_denied(&e) => false,
        // ENOENT / perms are not a containment escape; the walk surfaces them.
        Err(_) => true,
    }
}

#[cfg(not(target_os = "linux"))]
fn find_root_contained(
    _scope: &crate::caveats::Scope<String>,
    workspace: &str,
    full: &std::path::Path,
    _full_str: &str,
) -> bool {
    match (
        std::path::Path::new(workspace).canonicalize(),
        full.canonicalize(),
    ) {
        (Ok(ws), Ok(root)) => root.starts_with(&ws),
        // Can't canonicalize — keep the old permissive behaviour (deny only on a
        // proven escape).
        _ => true,
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
    // D0 (#1878): the definition is built DIRECTLY. C0a moved the rendering
    // here but left the model behind, and said so — "this form is still
    // flattened to a string across the free-text `ask_question` seam and
    // re-parsed below, the one place a typed form crosses a seam as rendered
    // text". The flattening stays (the free-text seam is D1's), but the
    // legacy `Question` round trip on either side of it is gone.
    let definition = mutation_confirm_definition(question);
    let rendered = crate::markup::plain::render(&definition);
    match gate {
        // ONLY an explicit answer resolving to AllowOnce authorizes the
        // mutation. Every other outcome — no operator, Esc/Ctrl-C/Ctrl-D,
        // EOF, input failure, an ambiguous answer, or a non-"y" answer —
        // fails closed (mutation denied).
        Some(g) => match g.ask_question(&rendered) {
            HumanQuestionOutcome::Answer(answer) => {
                // The ONE resolver (D0), and a fail-closed CONSTANT: the deny
                // is the absence of an AllowOnce, never an option picked out
                // of the definition by role. `role` is author-assigned, so
                // deriving the failure mode from it would hand the author the
                // failure mode (A3).
                confirm_choice_options(&definition)
                    .and_then(|options| newt_interaction::binding::resolve_typed(options, &answer))
                    .and_then(|option| {
                        crate::interaction_adapter::action_for_option(option.as_str())
                    })
                    == Some(PermissionAction::AllowOnce)
            }
            _ => false,
        },
        None => false,
    }
}

/// The choice control's options, if this definition has one.
fn confirm_choice_options(
    definition: &newt_interaction::InteractionDefinition,
) -> Option<&Vec<newt_interaction::ChoiceOption>> {
    definition.controls.iter().find_map(|c| match &c.kind {
        newt_interaction::ControlKind::Choice { options } => Some(options),
        _ => None,
    })
}

/// The `--yolo` mutation confirm, as an `InteractionDefinition`.
///
/// Field-identical to what `question_to_definition(&mutation_confirm_question(..))`
/// produced, so `markup::plain::render` emits the same bytes it always has
/// (`mutation_confirm_renders_its_frozen_form`). What changed is that there is
/// no longer a legacy `Question` in the middle: this was the last production
/// construction of one, and the last caller of `Question::parse`.
fn mutation_confirm_definition(question: &str) -> newt_interaction::InteractionDefinition {
    use newt_interaction::{
        ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition, InteractionKind,
        OptionId, Requirement, SemanticRole,
    };
    let option = |wire: &str, role, key: &str, label: &str, alias: &str| {
        ChoiceOption {
        id: OptionId::new(wire).expect(
            "the confirm wire names are consts drawn from [A-Za-z0-9_-]; this              cannot vary at runtime",
        ),
        role,
        label: label.to_string(),
        key: key.to_string(),
        aliases: vec![alias.to_string()],
    }
    };
    InteractionDefinition::new(
        // Confirm, not Choice (#1912). This is decision-shaped — one choice
        // control, `Allow` + `Deny` — and `InteractionKind::Confirm` is the
        // canonical kind for that. It was `Choice`, which made the kind
        // useless as a discriminator: C0c found the same shape declared under
        // both and had to go unconditional.
        InteractionKind::Confirm,
        question.to_string(),
        vec![Control {
            id: ControlId::new(crate::interaction_adapter::DECISION_CONTROL)
                .expect("`decision` is a valid control id; it is a const"),
            kind: ControlKind::Choice {
                options: vec![
                    option(
                        PermissionAction::AllowOnce.as_str(),
                        SemanticRole::Allow,
                        "y",
                        "y to confirm",
                        "Y",
                    ),
                    option(
                        PermissionAction::Deny.as_str(),
                        SemanticRole::Deny,
                        "n",
                        "n to skip",
                        "N",
                    ),
                ],
            },
            label: String::new(),
            // A mutation confirm must be answered: an unanswered one denies,
            // which is a decision and not an absence.
            requirement: Requirement::Required,
        }],
    )
}

/// Run the configured build-check command in `workspace` and return a compact
/// result string appended to the tool output so the model sees it immediately.
///
/// The `build_check_cmd` is **repository-configured** (`.newt/config.toml`), so a
/// hostile repo controls the shell string. It is therefore attacker-influenced
/// execution and runs **confined** through [`ConstrainedExecutor`] (P4): the
/// child starts env-empty (only `PATH`/`HOME` granted — no credentials, #8), its
/// writes are fenced to the workspace + temp dir and its network denied
/// ([`build_tool_caveats`], #9), and where the kernel fence cannot be established
/// the spawn is **refused** rather than run unconfined (#10). It is no longer a
/// raw `sh -c` on the host.
pub(crate) fn run_build_check(cmd: &str, workspace: &str) -> String {
    use crate::confined_exec::{
        build_tool_caveats, ConstrainedExecutor, ExecOrigin, ExecRequest, NetGrant,
    };
    let (program, args) = build_check_argv(cmd);
    let mut req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        program,
        args,
        workspace,
        build_tool_caveats(std::path::Path::new(workspace)),
    )
    // The `net: none` caveat already Landlock-denies TCP egress; the seccomp
    // egress floor (`DenyAll`) additionally closes the UDP/DNS/raw leg Landlock
    // cannot filter, so an attacker-influenced build step (a hostile `build.rs` /
    // test) cannot resolve a name or exfiltrate over UDP. Pure hardening: the
    // build already ran with no network, so nothing that legitimately fetched
    // regresses (fetches happen outside the confined check).
    .net_grant(NetGrant::DenyAll)
    // `HOME` + `TMPDIR` point at the workspace so any HOME-relative or scratch
    // writes stay inside the write fence; nothing credential-bearing is granted
    // (#8).
    .env("HOME", workspace)
    .env("TMPDIR", workspace);
    // `PATH` so the configured build tools (cargo/make/…) resolve. It is not a
    // credential; the fence still governs everything the resolved tool may do.
    if let Ok(path) = std::env::var("PATH") {
        req = req.env("PATH", path);
    }

    match ConstrainedExecutor::run(&req) {
        Ok(out) if out.success => "  ✓ build check passed".to_string(),
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

/// The interpreter + argv for the configured build-check string, per platform.
#[cfg(windows)]
fn build_check_argv(cmd: &str) -> (&'static str, Vec<String>) {
    ("cmd", vec!["/C".to_string(), cmd.to_string()])
}

#[cfg(not(windows))]
fn build_check_argv(cmd: &str) -> (&'static str, Vec<String>) {
    ("sh", vec!["-c".to_string(), cmd.to_string()])
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
        // #1547: this must be FORWARD guidance, not a dead-end. The old copy
        // rerouted the model to a `[tui.permissions]` edit it cannot perform
        // mid-run and told it to "take a different approach for now" — which,
        // in the confined bench lane (where the model already holds broad
        // workspace + system-root authority), abandons a task it was authorized
        // to finish and burns rounds. Tell it to stop re-asking and proceed
        // within the authority it already has; only report the blocker if the
        // target is genuinely essential and out of scope.
        None => format!(
            "no operator available to grant {capability} for '{target}' — this session \
             has no interactive permission gate (headless / eval / piped), so authority \
             cannot be widened mid-run and re-calling request_permissions will not help. \
             Proceed within the authority you already have and the tools available to you; \
             if '{target}' is genuinely essential and outside your current scope, say so in \
             your final answer rather than retrying it."
        ),
    };
    out
}

/// #728: returned by `request_user_input` when there is NO interactive gate this
/// session (headless / eval / ACP / piped) — the process genuinely has no human
/// interface. A recoverable signal the model can act on, NEVER a hang. When a
/// gate IS present but reports an outcome other than an answer, one of the
/// specific messages below is returned instead — a deliberate operator cancel or
/// exit must never be misreported as "running headless".
const HEADLESS_NO_HUMAN: &str = "no human available this session (running headless) \
    — proceed with your best judgment or state your assumption explicitly.";
/// Gate present but no interactive operator to answer this session. Does NOT
/// claim the process is headless (that is not known from here).
const NO_OPERATOR_AVAILABLE: &str = "no operator is available to answer this session \
    — proceed with your best judgment or state your assumption explicitly.";
/// The operator pressed Esc / backed out of the question.
const OPERATOR_CANCELLED: &str = "the operator cancelled this question; no answer was provided.";
/// The operator pressed Ctrl-C / Ctrl-D.
const OPERATOR_EXIT_REQUESTED: &str = "the operator requested exit; stop the current interaction.";
/// The operator's input stream closed (EOF) before an answer was provided.
const OPERATOR_INPUT_CLOSED: &str =
    "the operator input stream closed before an answer was provided.";
/// Reading operator input failed; no answer was provided.
const OPERATOR_INPUT_FAILED: &str = "operator input failed; no answer was provided.";

/// #728: the model-facing `request_user_input` tool — the GENERIC ask-the-human
/// path. It surfaces a free-text `question` to the operator through the SAME
/// human-interface gate a permission prompt uses ([`PermissionGate::ask_question`])
/// and returns a truthful, model-facing string for each typed
/// [`HumanQuestionOutcome`]. With an operator present the answer is returned
/// verbatim; with NO gate (headless / eval / ACP / piped) it returns
/// [`HEADLESS_NO_HUMAN`]; a deliberate operator cancel/exit, an unavailable
/// operator, EOF, or an input failure each get their own honest message — never
/// "headless". It NEVER blocks. The turn-cancel / process-exit flags remain
/// authoritative inside the gate; this returned text is still required to be
/// truthful in case it is logged or reaches the model before cancellation.
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

    // No gate at all ⇒ the process is genuinely headless. Otherwise consult the
    // gate and translate its typed outcome into a truthful, distinct message —
    // an operator cancel/exit is NOT "headless".
    let Some(gate) = gate else {
        return HEADLESS_NO_HUMAN.to_string();
    };
    match gate.ask_question(question) {
        HumanQuestionOutcome::Answer(answer) => answer,
        HumanQuestionOutcome::Unavailable => NO_OPERATOR_AVAILABLE.to_string(),
        HumanQuestionOutcome::Cancelled => OPERATOR_CANCELLED.to_string(),
        HumanQuestionOutcome::ExitRequested => OPERATOR_EXIT_REQUESTED.to_string(),
        HumanQuestionOutcome::InputClosed => OPERATOR_INPUT_CLOSED.to_string(),
        HumanQuestionOutcome::InputFailed => OPERATOR_INPUT_FAILED.to_string(),
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

/// MCP `_meta` extension by which an admitted connector declares the exact URL
/// prefixes a tool can read.  The value is an array of absolute HTTP(S) URLs.
///
/// This is routing metadata, not model-facing JSON Schema: catalog adapters
/// preserve it on the outer OpenAI-style definition, and
/// [`strip_mcp_catalog_metadata`] removes it before tools go to an inference
/// provider.
pub const MCP_RESOURCE_URL_PREFIXES_META_KEY: &str = "newt/resourceUrlPrefixes";

/// Copy Newt's recognized resource-affinity declaration from MCP tool `_meta`
/// onto an OpenAI-style tool definition.
///
/// The helper is intentionally pure and narrow so both the headless and TUI
/// catalog adapters can preserve authoritative server metadata without copying
/// arbitrary MCP `_meta` onto an inference-provider wire. The declaration is
/// fail-closed: every member must be valid, and an empty or malformed array
/// adds no routing authority.
pub fn preserve_mcp_resource_url_affinity(
    definition: &mut serde_json::Value,
    mcp_tool_meta: Option<&serde_json::Value>,
) {
    let Some(mcp_tool_meta) = mcp_tool_meta else {
        return;
    };
    if validated_resource_url_prefixes(mcp_tool_meta).is_none() {
        return;
    }
    let prefixes = mcp_tool_meta[MCP_RESOURCE_URL_PREFIXES_META_KEY].clone();
    let Some(definition) = definition.as_object_mut() else {
        return;
    };
    let meta = definition
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(meta) = meta.as_object_mut() else {
        return;
    };
    meta.insert(MCP_RESOURCE_URL_PREFIXES_META_KEY.to_string(), prefixes);
}

/// Remove connector-only metadata before a tool definition crosses the model
/// wire. Recovery reads metadata from `McpTools::tool_defs()` directly; normal
/// tool advertisement and `tool_search` use the scrubbed merged catalog.
pub(super) fn strip_mcp_catalog_metadata(definition: &mut serde_json::Value) {
    if let Some(definition) = definition.as_object_mut() {
        definition.remove("_meta");
    }
}

fn resource_url_prefix(prefix: &str) -> Option<reqwest::Url> {
    if prefix.trim() != prefix {
        return None;
    }
    let parsed = reqwest::Url::parse(prefix).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    Some(parsed)
}

fn validated_resource_url_prefixes(meta: &serde_json::Value) -> Option<Vec<reqwest::Url>> {
    let prefixes = meta.get(MCP_RESOURCE_URL_PREFIXES_META_KEY)?.as_array()?;
    if prefixes.is_empty() {
        return None;
    }
    prefixes
        .iter()
        .map(|prefix| prefix.as_str().and_then(resource_url_prefix))
        .collect()
}

fn resource_url_has_prefix(url: &reqwest::Url, prefix: &reqwest::Url) -> bool {
    if url.scheme() != prefix.scheme()
        || url.host_str() != prefix.host_str()
        || url.port_or_known_default() != prefix.port_or_known_default()
    {
        return false;
    }

    let target_path = url.path();
    let prefix_path = prefix.path();
    if prefix_path == "/" {
        return true;
    }
    let prefix_base = prefix_path.strip_suffix('/').unwrap_or(prefix_path);
    target_path == prefix_base || target_path.starts_with(&format!("{prefix_base}/"))
}

fn tool_declares_resource_url(tool: &serde_json::Value, url: &reqwest::Url) -> bool {
    tool.get("_meta")
        .and_then(validated_resource_url_prefixes)
        .is_some_and(|prefixes| {
            prefixes
                .iter()
                .any(|prefix| resource_url_has_prefix(url, prefix))
        })
}

/// Build a bounded, credential-free discovery query from a resource URL.
///
/// Only host/path words participate: query strings and fragments may contain
/// credentials, so they are never copied into a model-visible recovery hint.
/// Path words come first because they usually describe the resource better
/// than deployment-oriented host labels. A simple plural stem lets a URL path
/// such as `/reviews/42` find a tool described as "review" without claiming
/// that the lexical match is authoritative.
fn resource_url_discovery_query(url: &reqwest::Url) -> String {
    const MAX_TERMS: usize = 8;
    const MAX_TERM_CHARS: usize = 32;
    const GENERIC_TERMS: &[&str] = &["com", "net", "org", "www"];

    let mut terms = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let sources = std::iter::once(url.path()).chain(url.host_str());
    for source in sources {
        for raw in source.split(|ch: char| !ch.is_ascii_alphanumeric()) {
            let term = raw.to_ascii_lowercase();
            if term.is_empty()
                || term.chars().count() > MAX_TERM_CHARS
                || GENERIC_TERMS.contains(&term.as_str())
            {
                continue;
            }
            for candidate in [
                Some(term.as_str()),
                term.strip_suffix('s').filter(|stem| stem.len() >= 3),
            ]
            .into_iter()
            .flatten()
            {
                if seen.insert(candidate.to_string()) {
                    terms.push(candidate.to_string());
                    if terms.len() == MAX_TERMS {
                        return terms.join(" ");
                    }
                }
            }
        }
    }
    terms.join(" ")
}

/// Return the connected MCP catalog that is actually callable in this prompt.
/// Empty means there is no authenticated-source route to recommend, so the raw
/// fetch failure must remain unchanged instead of promising a nonexistent tool.
fn callable_mcp_catalog(
    mcp: &dyn McpTools,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
) -> serde_json::Value {
    let defs = serde_json::Value::Array(mcp.tool_defs());
    let defs = filter_advertised_tools(defs, persona_tools);
    filter_tools_for_disposition(defs, disposition)
}

/// Turn an authentication/private-address raw-fetch failure into either an
/// authoritative URL-affine MCP route or an honest connected-catalog discovery
/// hint when this session has callable MCP tools.
///
/// The original error stays first and the SSRF guard stays intact. The added
/// result steers the model through the live catalog before the two field-seen
/// dead ends: shelling out to a second unauthenticated HTTP client, or asking
/// the operator for unrelated local client configuration. Metadata-free
/// discovery never asserts that a lexical candidate can access the URL.
fn authenticated_url_recovery(
    failure: String,
    url: &str,
    mcp: &dyn McpTools,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
) -> String {
    let Some(url) = reqwest::Url::parse(url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .filter(|url| url.host_str().is_some())
        .filter(|url| url.username().is_empty() && url.password().is_none())
    else {
        return failure;
    };

    let catalog = callable_mcp_catalog(mcp, persona_tools, disposition);
    let matching = catalog
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tool| {
            tool.pointer("/function/name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| !name.is_empty())
        })
        .filter(|tool| tool_declares_resource_url(tool, &url))
        .cloned()
        .collect::<Vec<_>>();
    if catalog.as_array().is_none_or(Vec::is_empty) {
        return failure;
    }

    if matching.is_empty() {
        let query = resource_url_discovery_query(&url);
        let candidates = super::tool_search::execute_tool_search(&query, &catalog);
        return format!(
            "{failure}\n\nAuthenticated-source recovery (non-authoritative discovery): raw HTTP \
             cannot access this resource and no connected MCP tool explicitly declares access to \
             its URL. The connected catalog may still contain an imported authenticated source. \
             Next call `tool_search` with the URL-derived query `{query}` and inspect the returned \
             tool contracts. This is discovery only: do not assume that a candidate can read or \
             authenticate to the URL, and do not call one unless its description and parameters \
             fit the resource. Do not fall back to `run_command`/curl or `request_user_input` for \
             unrelated local shell/client configuration until connected-source discovery has \
             been tried.\n{candidates}"
        );
    }

    let matching = serde_json::Value::Array(matching);
    let candidates = super::tool_search::execute_tool_search("", &matching);
    format!(
        "{failure}\n\nAuthenticated-source recovery: raw HTTP cannot access this resource, but \
         the connected MCP catalog explicitly declares one or more URL-affine tools. Next call \
         `tool_search` with an exact candidate name from the authoritative list below, inspect \
         its contract, then call the matching namespaced MCP tool for the resource. Do not fall \
         back to `run_command`/curl or `request_user_input` for local shell/client configuration \
         until the connected MCP routes have been tried.\n{candidates}"
    )
}

/// Whether a bridle raw-fetch error is the private-address SSRF refusal that an
/// authenticated connector is designed to handle. Match the stable, explicit
/// security diagnostic rather than treating ordinary timeouts/DNS failures as
/// evidence that a private source exists.
fn is_private_address_fetch_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("ssrf block")
        && (lower.contains("private/loopback address") || lower.contains("private address"))
}

/// Whether a failed raw fetch explicitly reports HTTP authentication or
/// authorization refusal. Bridle versions may surface non-2xx responses either
/// as structured results or errors, so both representations share the same
/// MCP-first recovery contract.
fn is_authentication_fetch_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "http 401",
        "http status 401",
        "401 unauthorized",
        "http 403",
        "http status 403",
        "403 forbidden",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// Render a successful bridle dispatch. HTTP 401/403 are transport successes
/// but content failures: treating their login/error body as page evidence led
/// the agent into unauthenticated shell fallbacks. Surface them as failures and,
/// when possible, route discovery to connected MCP tools.
fn render_web_fetch_result(
    url: &str,
    result: &serde_json::Value,
    mcp: &dyn McpTools,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
) -> String {
    let status = result.get("status").and_then(serde_json::Value::as_u64);
    if matches!(status, Some(401 | 403)) {
        return authenticated_url_recovery(
            format!(
                "error: web_fetch returned HTTP {}",
                status.unwrap_or_default()
            ),
            url,
            mcp,
            persona_tools,
            disposition,
        );
    }

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
    if title.is_empty() {
        format!("{final_url}\n\n{markdown}")
    } else {
        format!("# {title}\n{final_url}\n\n{markdown}")
    }
}

/// Render a bridle dispatch error, adding the authenticated-source recovery
/// only for the private-address SSRF case. The error itself is never weakened.
fn render_web_fetch_error(
    url: &str,
    error: &str,
    mcp: &dyn McpTools,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
) -> String {
    let failure = format!("error: {error}");
    if is_private_address_fetch_error(error) || is_authentication_fetch_error(error) {
        authenticated_url_recovery(failure, url, mcp, persona_tools, disposition)
    } else {
        failure
    }
}

/// File-type restriction for the embedded `find` tool (#496).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FindType {
    Files,
    Dirs,
    Any,
}

/// Harness-owned semantic category for repository entries. `Source` is backed
/// by the resolved language-pack registry; it is not a prompt-specific
/// extension list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FindCategory {
    Any,
    Source,
}

/// Result ordering for the embedded `find` tool (#1258). `Name` is the historical
/// default (paths ascending); `Size` orders by byte size descending, `Lines` by
/// newline count descending — so an evidence-only turn can answer "the N largest
/// files" (bytes) OR "the files with the most lines" without shell access. Line
/// count is a first-class evidence question, not a bytesize fallback.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FindSort {
    Name,
    Size,
    Lines,
}

/// Parsed, validated options for one `find` invocation.
struct FindOpts<'a> {
    /// Glob matched against each basename; `None` matches everything.
    name: Option<&'a str>,
    type_filter: FindType,
    /// Semantic file category. A named language implies `Source`.
    category: FindCategory,
    /// Optional language-pack name or human alias.
    language: Option<&'a str>,
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
    /// Prefix each line with the entry's line (newline) count + a tab. When set
    /// (or `sort=lines`) the metric column is line count, not bytes.
    show_lines: bool,
    /// Result ordering (#1258): [`FindSort::Name`] (default), byte-size- or
    /// line-count-descending.
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
    if opts.category == FindCategory::Source {
        parts.push("category=source".to_string());
    }
    if let Some(language) = opts.language {
        parts.push(format!("language={language}"));
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
    match opts.sort {
        FindSort::Size => parts.push("sort=size".to_string()),
        FindSort::Lines => parts.push("sort=lines".to_string()),
        FindSort::Name => {}
    }
    if opts.show_size {
        parts.push("size".to_string());
    }
    if opts.show_lines {
        parts.push("lines".to_string());
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
        // `code: true` is the backward-compatible alias for the source category
        // (#1405 shipped it; #1406 makes `category`/`language` canonical). A
        // named language also implies source.
        category: if args["category"].as_str() == Some("source")
            || args["language"].as_str().is_some()
            || args["code"].as_bool() == Some(true)
        {
            FindCategory::Source
        } else {
            FindCategory::Any
        },
        language: args["language"].as_str(),
        max_depth: args["max_depth"].as_u64().map(|d| d as usize),
        max_results: args["max_results"]
            .as_u64()
            .map(|m| m as usize)
            .unwrap_or(1000),
        respect_gitignore: args["respect_gitignore"].as_bool().unwrap_or(true),
        case_sensitive: args["case_sensitive"].as_bool().unwrap_or(true),
        show_size: args["show_size"].as_bool().unwrap_or(false),
        show_lines: args["show_lines"].as_bool().unwrap_or(false),
        sort: match args["sort"].as_str() {
            Some("size") => FindSort::Size,
            Some("lines") => FindSort::Lines,
            _ => FindSort::Name,
        },
    }
}

fn find_source_extensions(
    workspace: &std::path::Path,
    opts: &FindOpts<'_>,
) -> Result<Option<Vec<String>>, String> {
    if opts.category == FindCategory::Any {
        return Ok(None);
    }
    let api_cfg = crate::Config::resolve()
        .ok()
        .and_then(|cfg| cfg.context.map(|context| context.api_surface))
        .unwrap_or_default();
    let packs = crate::api_surface::resolve_language_packs(workspace, &api_cfg);
    crate::api_surface::source_extensions_for(&packs, opts.language).map(Some)
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
    if matches!(opts.sort, FindSort::Size | FindSort::Lines) {
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    let truncated = entries.len() > opts.max_results;
    entries.truncate(opts.max_results);
    // The metric column is line count in line-mode (show_lines or sort=lines),
    // otherwise byte size — a single tab-prefixed number either way.
    let show_metric = opts.show_size || opts.show_lines;
    let lines = entries
        .into_iter()
        .map(|(metric, path)| {
            if show_metric {
                format!("{metric}\t{path}")
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
        "select_operating_mode" => string("mode", ""),
        "prompt_read" | "artifact_read" => string("address", "current"),
        "tool_search" | "recall" | "code_search" | "experience_recall" => string("query", ""),
        "where_is" => string("symbol", ""),
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
/// `on_hit` (#1264): called once per accepted match, in DISCOVERY order, with
/// the workspace-relative path — the live-viewport producer seam. Presentation
/// only: the returned listing is still ordered/truncated by [`finalize_find`].
fn find_walk(
    root: &std::path::Path,
    workspace_root: &std::path::Path,
    opts: &FindOpts<'_>,
    source_extensions: Option<&[String]>,
    mut on_hit: impl FnMut(&str),
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
        if let Some(extensions) = source_extensions {
            if is_dir
                || !entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        extensions
                            .iter()
                            .any(|known| known.eq_ignore_ascii_case(extension))
                    })
            {
                continue;
            }
        }
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
        let rel = entry
            .path()
            .strip_prefix(workspace_root)
            .unwrap_or_else(|_| entry.path());
        let rel_display = rel.to_string_lossy().replace('\\', "/");
        // The metric is read only when it will be used (shown or sorted on). Line
        // mode (show_lines / sort=lines) wins over byte mode when both are set:
        // reads the file and counts newlines (dirs → 0); byte mode reads cheap
        // metadata. Unreadable → 0 rather than dropping the match.
        let want_lines = opts.show_lines || matches!(opts.sort, FindSort::Lines);
        let want_size = opts.show_size || matches!(opts.sort, FindSort::Size);
        let metric = if want_lines {
            if is_dir {
                0
            } else {
                count_lines(entry.path())
            }
        } else if want_size {
            entry.metadata().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        on_hit(&rel_display);
        entries.push((metric, rel_display));
    }
    Ok(finalize_find(entries, opts))
}

/// Count newlines in a file, matching `wc -l` semantics (a trailing line without
/// a newline is not counted). Unreadable files → 0 so the match survives rather
/// than aborting the walk. Reads the whole file — line count is a legitimate
/// evidence question and the walk is already bounded to a source workspace. The
/// counting itself is factored into [`count_newlines`] so it stays unit-testable
/// without touching the filesystem (the mocked tier).
fn count_lines(path: &std::path::Path) -> u64 {
    match std::fs::read(path) {
        Ok(bytes) => count_newlines(&bytes),
        Err(_) => 0,
    }
}

/// Pure `wc -l` newline count over raw bytes: the number of `\n` bytes, so a
/// final line lacking a trailing newline is not counted.
fn count_newlines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&b| b == b'\n').count() as u64
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

/// The `exit_plan_mode` tool result. Under a tenacity level that
/// [`requires an edit`](crate::tenacity::Tenacity::exit_plan_requires_edit) on
/// plan exit (Insistent / Relentless), it appends a MANDATORY-EDIT directive so
/// the model executes the first step instead of sliding back into more reading
/// (#tenacity / #11). Lower levels leave plan exit advisory. The tenacity
/// action-forcing loop (#10) then enforces it: a subsequent read-only round
/// trips the forcing nudge within the level's (small) budget.
fn exit_plan_mode_result(tenacity: crate::tenacity::Tenacity) -> String {
    let base = "exited the model-entered PLAN PHASE. Subsequent tool calls return to this turn's validated disposition and underlying session permissions; the next outer turn returns to the human-selected operating mode. `/mode plan` and other clamps still remain read-only.";
    if tenacity.exit_plan_requires_edit() {
        format!(
            "{base}\n\nThe plan is set — now EXECUTE it. Your NEXT action must be a concrete \
             change (edit_file or write_file) that begins the first step — not another \
             read, search, or plan. If a prerequisite is missing, make the smallest edit \
             that unblocks it."
        )
    } else {
        base.to_string()
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
/// The optional collaborator seams a tool dispatch may carry, bundled into ONE
/// value (reuse discipline: "prefer making a bug unrepresentable"). The
/// positional form threaded ~19 `Option` params through every facade layer —
/// and a bare-`None` run misaligned by one slot compiles fine while silently
/// disabling the wrong seam (the exact hazard hit while threading `where_is`,
/// #1285). Named fields + `..Default::default()` make that miswiring
/// impossible, and a NEW seam is one field plus its construction sites, not a
/// signature change through six layers.
///
/// `Default` is all-`None`: the bare dispatch a test or embedder starts from.
#[derive(Default)]
pub(crate) struct ToolCollaborators<'a> {
    pub(crate) build_check_cmd: Option<&'a str>,
    /// #1947: the turn's tool ledger, distilled — what `render_report`'s
    /// capability claims are checked against.
    ///
    /// `Option` is load-bearing and not a convenience. `None` means there is
    /// no RECORDER (eval, headless), which is not the same fact as an empty
    /// ledger; conflating them would refute every report in those tiers for
    /// a reason that has nothing to do with the report.
    pub(crate) tool_evidence: Option<&'a super::capability_check::Evidence>,
    pub(crate) note_sink: Option<&'a mut dyn NoteSink>,
    pub(crate) recall_source: Option<&'a dyn RecallSource>,
    pub(crate) memory_source: Option<&'a dyn MemorySource>,
    pub(crate) prompt_context: Option<PromptReadContext<'a>>,
    pub(crate) artifact_context: Option<ArtifactReadContext<'a>>,
    pub(crate) artifact_sink: Option<&'a dyn super::artifact_read::PromptArtifactSink>,
    pub(crate) permission_gate: Option<&'a mut dyn PermissionGate>,
    pub(crate) exec_floor: Option<&'a crate::caveats::Scope<String>>,
    pub(crate) git_tool: Option<&'a dyn GitTool>,
    pub(crate) crew_runner: Option<&'a dyn CrewRunner>,
    pub(crate) scratchpad_store: Option<&'a dyn super::scratchpad::ScratchpadStore>,
    pub(crate) code_search: Option<super::semantic::CodeSearch<'a>>,
    pub(crate) where_is: Option<&'a crate::where_is::WhereIsIndex>,
    /// #1387 navigator tool context (usage/graph/project). `None` ⇒ tools degrade.
    pub(crate) nav: Option<crate::navigator::NavToolCtx<'a>>,
    pub(crate) experience_store: Option<&'a dyn super::experiential::ExperienceStore>,
    pub(crate) step_ledger: Option<&'a dyn super::scheduled::StepLedger>,
    pub(crate) operating_mode_control: Option<&'a dyn super::OperatingModeControl>,
    pub(crate) plan_mode_control: Option<&'a dyn super::PlanModeControl>,
    pub(crate) spill_store: Option<&'a dyn SpillStore>,
    pub(crate) persona_tools: Option<&'a [String]>,
    pub(crate) live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    /// Optional completed spill renderer for Rich TUI interactive viewport (#1640).
    pub(crate) completed_spill_renderer:
        Option<std::sync::Arc<dyn crate::agentic::CompletedSpillRenderer>>,
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
    where_is: Option<&crate::where_is::WhereIsIndex>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
) -> String {
    // The convenience wrapper carries no offload/persona/prompt surface —
    // callers that need those seams use the wider entry points.
    let collab = ToolCollaborators {
        build_check_cmd,
        // Reborrow the invariant `&mut dyn` seams to the local region (the
        // same coercion the loop's call sites perform on ChatCtx fields).
        note_sink: note_sink.map(|s| &mut *s as &mut dyn NoteSink),
        recall_source,
        memory_source,
        permission_gate: permission_gate.map(|g| &mut *g as &mut dyn PermissionGate),
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        where_is,
        nav: None,
        experience_store,
        step_ledger,
        ..Default::default()
    };
    execute_tool_with_collaborators(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("tool execution without a cancellation flag cannot be interrupted")
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
    where_is: Option<&crate::where_is::WhereIsIndex>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
) -> String {
    let collab = ToolCollaborators {
        build_check_cmd,
        // Reborrow the invariant `&mut dyn` seams to the local region (the
        // same coercion the loop's call sites perform on ChatCtx fields).
        note_sink: note_sink.map(|s| &mut *s as &mut dyn NoteSink),
        recall_source,
        memory_source,
        permission_gate: permission_gate.map(|g| &mut *g as &mut dyn PermissionGate),
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        where_is,
        nav: None,
        experience_store,
        step_ledger,
        spill_store,
        persona_tools,
        ..Default::default()
    };
    execute_tool_with_collaborators(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        tool_offload,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("tool execution without a cancellation flag cannot be interrupted")
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
    where_is: Option<&crate::where_is::WhereIsIndex>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
) -> String {
    let collab = ToolCollaborators {
        build_check_cmd,
        // Reborrow the invariant `&mut dyn` seams to the local region (the
        // same coercion the loop's call sites perform on ChatCtx fields).
        note_sink: note_sink.map(|s| &mut *s as &mut dyn NoteSink),
        recall_source,
        memory_source,
        prompt_context,
        permission_gate: permission_gate.map(|g| &mut *g as &mut dyn PermissionGate),
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        where_is,
        nav: None,
        experience_store,
        step_ledger,
        spill_store,
        persona_tools,
        ..Default::default()
    };
    execute_tool_with_collaborators(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        tool_offload,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("tool execution without a cancellation flag cannot be interrupted")
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
    where_is: Option<&crate::where_is::WhereIsIndex>,
    experience_store: Option<&dyn super::experiential::ExperienceStore>,
    step_ledger: Option<&dyn super::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
) -> String {
    let collab = ToolCollaborators {
        build_check_cmd,
        // Reborrow the invariant `&mut dyn` seams to the local region (the
        // same coercion the loop's call sites perform on ChatCtx fields).
        note_sink: note_sink.map(|s| &mut *s as &mut dyn NoteSink),
        recall_source,
        memory_source,
        prompt_context,
        artifact_context,
        artifact_sink,
        permission_gate: permission_gate.map(|g| &mut *g as &mut dyn PermissionGate),
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        where_is,
        nav: None,
        experience_store,
        step_ledger,
        spill_store,
        persona_tools,
        ..Default::default()
    };
    execute_tool_with_collaborators(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        tool_offload,
        disposition,
        None,
    )
    .await
    .expect("tool execution without a cancellation flag cannot be interrupted")
}

/// Cancellation-aware loop entry point — the collaborator-struct core every
/// public wrapper above flattens into. The header is written synchronously
/// before the cancel-first race begins; an already-set interrupt therefore
/// closes a complete audit block without ever polling the tool body.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_tool_with_collaborators(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    collab: ToolCollaborators<'_>,
    tool_offload: bool,
    disposition: PromptDisposition,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Option<String> {
    let mut display = ToolDisplay::new(
        std::io::stdout(),
        color,
        super::display::term_cols(),
        super::display::spill_lines(),
        super::display::spill_summary(),
    );
    // Thread the completed spill renderer for Rich TUI interactive viewport (#1640)
    if let Some(ref renderer) = collab.completed_spill_renderer {
        display.set_completed_spill_renderer(renderer.clone());
    }
    execute_tool_with_display_cancellable(
        &mut display,
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        tool_offload,
        disposition,
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
    collab: ToolCollaborators<'_>,
    tool_offload: bool,
    disposition: PromptDisposition,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Option<String> {
    let (presentation_name, presentation_detail) =
        tool_presentation(name, args, std::path::Path::new(workspace));
    display.call(&presentation_name, &presentation_detail);
    let result = {
        // #1727: the row under the header is never silent while the tool is
        // in flight. The spinner is scoped to this block, so it is erased
        // before `display.result` below on every path — return, cancel, or
        // panic — and the session's live sink is wrapped so the FIRST live
        // chunk takes the row over from it. See `ToolSpinner`.
        let spinner = ToolSpinner::start(&presentation_name, color);
        let collab = ToolCollaborators {
            live_tool_output: spinner.wrap(collab.live_tool_output),
            ..collab
        };
        let execution = execute_tool_inner(
            display,
            name,
            args,
            workspace,
            color,
            tool_output_lines,
            caveats,
            mcp,
            collab,
            tool_offload,
            disposition,
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
            // The turn is being torn down — an interactive viewport painted
            // here would outlive every dismiss hook (the provider loops
            // return immediately) and strand a dead frame above the caller's
            // interrupt notice. Static excerpt only.
            display.drop_completed_spill_renderer();
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
    collab: ToolCollaborators<'_>,
    tool_offload: bool,
    // The prompt's validated disposition. This is deliberately a required
    // dispatcher input: catalog filtering is cosmetic, whereas this check is
    // the boundary that refuses fabricated tool names before MCP routing,
    // aliases, or permission widening can run.
    disposition: PromptDisposition,
) -> String {
    // One unpack; the dispatch body below binds the same names it always has.
    let ToolCollaborators {
        build_check_cmd,
        tool_evidence,
        note_sink,
        recall_source,
        memory_source,
        prompt_context,
        artifact_context,
        artifact_sink,
        mut permission_gate,
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        where_is,
        nav,
        experience_store,
        step_ledger,
        operating_mode_control,
        plan_mode_control,
        spill_store,
        persona_tools,
        live_tool_output,
        completed_spill_renderer: _,
    } = collab;

    // A model-entered Plan phase takes effect immediately for every later
    // tool call in the same inference round. The outer TUI also resolves it
    // into Plan caveats on the next turn; this local clamp closes the
    // enter-then-write gap before that boundary is rebuilt.
    let disposition = if plan_mode_control.is_some_and(super::PlanModeControl::is_plan_mode)
        && disposition == PromptDisposition::Act
    {
        PromptDisposition::Plan
    } else {
        disposition
    };

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
    // narrow tools the disposition permits. `request_user_input` is the sole
    // exception: its question path mints no caveat and cannot widen this turn,
    // but it must still be able to hand control back to an interactive operator.
    // The existing caveats continue to decide whether each read is legal.
    if disposition != PromptDisposition::Act && name != "request_user_input" {
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
    // tools (always-on infrastructure plus presence-gated session controls)
    // always pass or the loop could wedge.
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
    // READ tool on demand while a mutating one stays gated. With NO persona
    // (`persona_tools == None`) "no persona" is NOT "unrestricted"
    // (`mcp-under-leash`): a read-class tool passes, but a mutating/unknown one
    // must be granted by a human `PermissionGate`, else it is denied — closing
    // the pre-leash hole where a no-persona session dispatched every remote tool
    // unmediated. The effect class comes from the tool NAME by a droppable
    // convention (`classify_mcp_effect`), never the server's own hints.
    if mcp.handles(name) {
        // `PermissionRequest` for the human-in-the-loop cases (persona
        // out-of-list, or a no-persona mutating tool).
        let prompt_gate = |permission_gate: Option<&mut dyn PermissionGate>, reason: String| {
            let request = PermissionRequest {
                tool: name.to_string(),
                kind: DenialKind::RemoteTool,
                target: name.to_string(),
                reason,
            };
            match permission_gate {
                Some(gate) => matches!(gate.ask(&[request]), PermissionDecision::Allow(_)),
                // Headless / no operator to consult: fail-closed.
                None => false,
            }
        };
        // Authority is a structural GRANT provenance, NEVER the server-chosen
        // tool name (`mcp-under-leash`). A hostile admitted server that names a
        // destructive tool with a read verb (`get_…`) earns nothing here — a
        // read verb is not a grant.
        let grant: Option<McpGrant> = match persona_tools {
            // Persona path: allow-listed operations dispatch; otherwise the
            // human is prompted and an explicit deny hard-stops.
            Some(allow) if persona_tool_allowed(name, allow) => Some(McpGrant::PersonaAllowList),
            Some(_) => prompt_gate(
                permission_gate,
                format!("remote tool `{name}` is outside the active persona's tool allow-list"),
            )
            .then_some(McpGrant::HumanApproved),
            // No-persona path (`mcp-under-leash`): "no persona" is NOT
            // "unrestricted" and is NOT read-tolerant by tool name. EVERY
            // operation is human-gated — the name-classified effect is shown only
            // as a HINT (it grants nothing) — and fails closed when headless, so
            // a server-renamed `get_…` cannot self-authorize.
            None => {
                let hint = match classify_mcp_effect(name) {
                    McpEffect::Read => "appears read-class",
                    McpEffect::Mutating => "mutating/unknown",
                };
                prompt_gate(
                    permission_gate,
                    format!(
                        "remote tool `{name}` ({hint}) has no persona to bound it — \
                         no persona is not unrestricted, and a tool name is not a grant"
                    ),
                )
                .then_some(McpGrant::HumanApproved)
            }
        };
        // The witness leash: `mcp.call` requires a `LeasedMcpCall`, so this is
        // the only way to dispatch — an un-leashed call does not type-check.
        return match leash_mcp_call(name, args, grant) {
            Ok(leased) => mcp.call(&leased).await,
            Err(_) => persona_tool_denied_message(name),
        };
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

        // #1285: exact, typed-verdict symbol lookup — presence-gated on the
        // retained where_is index (built from the honest gather + language packs).
        "where_is" => match where_is {
            Some(index) => crate::where_is::execute_where_is(args, index, tool_output_lines),
            None => "unknown tool: where_is (no symbol index built for this session)".to_string(),
        },

        // #1387 Code Navigator narrow tools — degrade via execute_nav_tool when
        // session indexes are absent.
        name if crate::navigator::NAV_TOOL_NAMES.contains(&name) => {
            let ctx = nav.unwrap_or(crate::navigator::NavToolCtx {
                workspace,
                where_is,
                usage: None,
                graph: None,
                project: None,
                files: None,
                status: None,
            });
            crate::navigator::execute_nav_tool(name, args, &ctx)
                .unwrap_or_else(|| format!("unknown tool: {name}"))
        }

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

        // #1193: enter/exit the session-local, read-only Plan phase. The
        // dispatcher consults the same collaborator before every call, so a
        // successful enter clamps later calls in this model tool round.
        "enter_plan_mode" => match (plan_mode_control, step_ledger) {
            (Some(control), Some(_)) => match control.set_plan_mode(true) {
                Ok(()) => "entered PLAN MODE (read-only): subsequent tool calls are immediately limited to Plan reads and the plan ledger until you call exit_plan_mode. Read/search the relevant code, draft the ordered steps with update_plan, then exit_plan_mode to execute.".to_string(),
                Err(error) => format!("error: enter_plan_mode: {error}"),
            },
            _ => {
                "unknown tool: enter_plan_mode (scheduled planning and a session Plan-mode control are both required)".to_string()
            }
        },
        "exit_plan_mode" => match plan_mode_control {
            Some(control) => match control.set_plan_mode(false) {
                Ok(()) => exit_plan_mode_result(crate::tenacity::effective_tenacity()),
                Err(error) => format!("error: exit_plan_mode: {error}"),
            },
            None => {
                "unknown tool: exit_plan_mode (no session Plan-mode control is available)"
                    .to_string()
            }
        },
        // `/mode auto`: schedule a bounded working-style transition for a
        // future turn. The injected collaborator owns session-local state;
        // this call cannot alter the current disposition or caveats.
        "select_operating_mode" => {
            let mode = args
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            match operating_mode_control {
                Some(control) => control
                    .select_operating_mode(mode)
                    .unwrap_or_else(|error| format!("error: select_operating_mode: {error}")),
                None => {
                    "unknown tool: select_operating_mode (available only while /mode auto is active)"
                        .to_string()
                }
            }
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
                    operating_mode_control.is_some(),
                    plan_mode_control.is_some(),
                    plan_mode_control.is_some_and(super::PlanModeControl::is_plan_mode),
                ),
                persona_tools,
            );
            let catalog = filter_tools_for_disposition(catalog, disposition);
            super::tool_search::execute_tool_search_for_disposition(query, &catalog, disposition)
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

            // Attribution invariant (#1709 family): a COMPOSED shell command that
            // creates a git commit (`git add . && git commit -m x`,
            // `echo msg | git commit -F -`, `git -c user.email=… commit`,
            // `/usr/bin/git -C <repo> commit`, `GIT_AUTHOR_NAME=… git commit`)
            // bypasses `LocalGitTool::finalize_commit_message` and would land an
            // unattributed Newt commit. Routing the composed command through the
            // embedded `git` tool is impossible (it cannot serve `&&`/pipes/
            // redirects), and reusing the finalizer would require parsing an
            // arbitrary shell command's commit message — fragile and out of
            // scope. So FAIL PREDICTABLY: refuse the commit and direct the model
            // to the first-class `git` tool, which stamps attribution itself.
            // Read-only git (status/log/diff) and network ops (push/fetch/…)
            // are unaffected; this never reaches the confined shell.
            if run_command_creates_shell_git_commit(cmd) {
                return "error: refusing to create a git commit via the shell — that \
                     bypasses harness-managed commit attribution (the `git` tool \
                     stamps the Co-authored-by trailer + provenance itself; a \
                     shell `git commit`/`merge`/`cherry-pick`/`revert`/`rebase` \
                     would let the model forge or omit it). \
                     Use the `git` tool with op \"commit\" (or \"amend\", \"rebase\") \
                     for the routable forms. `git merge`/`cherry-pick`/`revert` \
                     have no first-class Newt route — the operator must run them \
                     directly, not via run_command. \
                     Read-only git (status/log/diff) and `git push`/`fetch` are \
                     unaffected; abort forms (`--abort`/`--quit`) pass through."
                    .to_string();
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
            let (result, document) = execute_render_report(args, color, tool_evidence);
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
            // Did the fs_read SCOPE authorise this (the automatic, object-bound
            // fence), or only a #263 permission-gate grant (the human approving
            // this exact out-of-scope path)? That distinction decides whether the
            // read is object-bound below.
            let scope_permits = tui_permits_path(&caveats.fs_read, &full_str);
            if !scope_permits {
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
            // step-52.2: object-bound read when the SCOPE authorised it — resolve
            // `path` beneath the granted root (openat2 RESOLVE_BENEATH), so a
            // symlink / `..` / absolute escape the lexical gate admits is refused
            // by the kernel. A gate-approved out-of-scope path was explicitly
            // vouched for by the human (#263), so it reads as-is; `Scope::All`
            // (--full-access) is unconfined inside `object_bound_read`.
            let read = if scope_permits {
                object_bound_read(&caveats.fs_read, "fs_read", path, &full, &full_str)
            } else {
                std::fs::read_to_string(&full).map_err(|e| format!("error reading {path}: {e}"))
            };
            match read {
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
                Err(tool_output) => tool_output,
            }
        }

        "write_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            // Scope- vs #263-gate-authorised, same split as read_file (step-52.2):
            // decides whether the write below is object-bound.
            let scope_permits = tui_permits_path(&caveats.fs_write, &full_str);
            if !scope_permits {
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
                // step-52.4: object-bound write when the SCOPE authorised it —
                // create the file (and any missing parents) beneath the granted
                // root's fd (openat2 RESOLVE_BENEATH), so a symlink / `..` /
                // absolute escape the lexical gate admits is refused by the
                // kernel. A gate-approved out-of-scope path was vouched for by the
                // human (#263); `Scope::All` is unconfined inside the helper.
                let write_result = if scope_permits {
                    object_bound_write(&caveats.fs_write, "fs_write", path, &full, &full_str, content)
                } else {
                    std_write(&full, path, content)
                };
                match write_result {
                    Ok(()) => {
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
                    Err(tool_output) => tool_output,
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
            // step-52.6: same scope-vs-#263-gate split — decides whether the
            // removal below is object-bound (via unlinkat on the resolved parent).
            let scope_permits = tui_permits_path(&caveats.fs_write, &full_str);
            if !scope_permits {
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

            // step-52.6: object-bound removal when the scope authorised it (the
            // parent is resolved beneath the root and the entry unlinked via its
            // fd, so a symlink/`..`/absolute escape is refused by the kernel).
            let delete_result = if scope_permits {
                object_bound_delete(&caveats.fs_write, path, &full, &full_str)
            } else {
                std::fs::remove_file(&full).map_err(|e| format!("error deleting {path}: {e}"))
            };
            match delete_result {
                Ok(()) => {
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
                Err(tool_output) => tool_output,
            }
        }

        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let old_string = args["old_string"].as_str().unwrap_or("");
            let new_string = args["new_string"].as_str().unwrap_or("");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            // step-52.5: same scope-vs-#263-gate split as write_file — decides
            // whether the read of `existing` and the write of `updated` below are
            // object-bound (both authorised by, and contained beneath, fs_write).
            let scope_permits = tui_permits_path(&caveats.fs_write, &full_str);
            if !scope_permits {
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
            // step-52.5: read the existing file object-bound beneath the same
            // fs_write root (a symlink-escape edit is refused here, so the
            // no-match head display below can't leak an outside file either).
            let read = if scope_permits {
                object_bound_read(&caveats.fs_write, "fs_write", path, &full, &full_str)
            } else {
                std::fs::read_to_string(&full).map_err(|e| format!("error reading {path}: {e}"))
            };
            let existing = match read {
                Ok(s) => s,
                Err(tool_output) => return tool_output,
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
            // step-52.5: object-bound write when the scope authorised it.
            let write_result = if scope_permits {
                object_bound_write(&caveats.fs_write, "fs_write", path, &full, &full_str, &updated)
            } else {
                std_write(&full, path, &updated)
            };
            match write_result {
                Ok(()) => {
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
                Err(tool_output) => tool_output,
            }
        }

        "list_dir" => {
            let path = args["path"].as_str().unwrap_or(".");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            // Scope- vs #263-gate-authorised, same split as read_file (step-52.2).
            let scope_permits = tui_permits_path(&caveats.fs_read, &full_str);
            if !scope_permits {
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
            // step-52.3: object-bound listing when the scope authorised it (a
            // symlink-escape directory is refused by the kernel); a gate-approved
            // out-of-scope path lists as-is.
            let listing = if scope_permits {
                object_bound_list(&caveats.fs_read, path, &full, &full_str)
            } else {
                std_list_dir(&full)
            };
            match listing {
                Ok(mut names) => {
                    names.sort();
                    names.join("\n")
                }
                Err(tool_output) => tool_output,
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
            let source_extensions =
                match find_source_extensions(std::path::Path::new(workspace), &opts) {
                    Ok(extensions) => extensions,
                    Err(error) => return format!("error: {error}"),
                };
            if !full.exists() {
                return format!("error: no such path '{path}'");
            }
            // step-52.6: object-bound root containment for a *recursive* read —
            // resolve the search root beneath the granted fs_read root
            // (openat2 RESOLVE_BENEATH on Linux; canonicalize fallback elsewhere),
            // TOCTOU-free. `find` never follows symlinks during descent, so a
            // contained root bounds the whole walk.
            if !find_root_contained(&caveats.fs_read, workspace, &full, &full_str) {
                return denied_fs_result("fs_read", path);
            }
            // #1264: stream hits through the LIVE viewport as the walk
            // discovers them — the first built-in on the #1235 machinery (the
            // diagnosed session watched 339 lines spill with no live window at
            // any moment). The live frame shows DISCOVERY order (presentation
            // only); the canonical listing below stays ordered/truncated by
            // `finalize_find` — the authoritative envelope is unchanged.
            let mut live = LiveOutputSession::start(live_tool_output.clone());
            let relay = live.as_ref().map(LiveOutputSession::relay);
            let on_hit = |line: &str| {
                if let Some(relay) = &relay {
                    let mut chunk = line.as_bytes().to_vec();
                    chunk.push(b'\n');
                    relay.write(crate::agentic::ToolOutputStream::Stdout, &chunk);
                }
            };
            let walked = find_walk(
                &full,
                std::path::Path::new(workspace),
                &opts,
                source_extensions.as_deref(),
                on_hit,
            );
            if let Some(live) = live.as_mut() {
                live.finish();
            }
            match walked {
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
                Ok(result) =>
                    render_web_fetch_result(url, &result, &*mcp, persona_tools, disposition),
                // A `net`-axis leash denial, or a fetch error (SSRF screen,
                // timeout) — surface the reason; Display is safe. Private-address
                // denials gain an MCP-first recovery hint without weakening the
                // refusal itself.
                Err(e) => render_web_fetch_error(url, &e.to_string(), &*mcp, persona_tools, disposition),
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

// Private-source recovery is a composition invariant, not only a renderer
// contract: replay the complete model -> built-in -> discovery -> MCP loop.
#[cfg(test)]
#[path = "tools_tests/private_url_mcp_bat.rs"]
mod private_url_mcp_bat_tests;

#[cfg(test)]
#[path = "tools_tests/tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// execute_tool branch tests — edit_file / shrink guard / denial paths
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tools_tests/execute_tool_branch_tests.rs"]
mod execute_tool_branch_tests;

// ---------------------------------------------------------------------------
// #1969 — the ledger's `ok` bit follows the exit code, not a string prefix.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tools_tests/exit_code_ok_tests.rs"]
mod exit_code_ok_tests;

// ---------------------------------------------------------------------------
// INTERIM (#297) --disable-ocap / --yolo tests — the exec escape hatch.
// Removed with the bypass when brush upstreams CommandInterceptor
// (agent-bridle#20).
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tools_tests/disable_ocap_tests.rs"]
mod disable_ocap_tests;
