//! Built-in tool definitions and the tool executor for the agentic loop.
//! Moved verbatim from `newt-tui` in Step 9.7 — the Caveats enforcement,
//! shrink guard, build-check feedback, and agent-bridle routing are unchanged.
// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:15 EDT | Date: 2026-08-12

mod worktree;
use worktree::execute as execute_tool_inner;

use super::artifact_read::{execute_artifact_read_silent, ArtifactReadContext};
#[cfg(test)]
use super::content_spill::{self, SpillStore};
#[cfg(test)]
use super::display::ToolDisplay;
use super::display::ToolPresentation;
use super::mcp::{leash_mcp_call, McpGrant, McpTools};
use super::memory_fetch::{execute_memory_fetch, memory_fetch_tool_definition};
use super::note_sink::{execute_save_note, save_note_tool_definition};
use super::permissions::{
    DenialKind, HumanQuestionOutcome, PermissionDecision, PermissionGate, PermissionRequest,
    BOUND_REASON_PREFIX,
};
use super::prompt_intake::PromptDisposition;
use super::prompt_read::execute_prompt_read_silent;
#[cfg(test)]
use super::prompt_read::PromptReadContext;
use super::recall::{execute_recall, recall_tool_definition};
use super::report::{execute_render_report, render_report_tool_definition};
use crate::caveats::CaveatsExt as _;
use crate::PermissionAction;
#[cfg(test)]
use output_budget::paginate_read;
#[cfg(test)]
use output_budget::DEFAULT_MAX_OUTPUT_TOKENS;
#[cfg(test)]
use output_budget::DEFAULT_OUTPUT_CAP_CHARS_PER_TOKEN;
#[cfg(test)]
use output_budget::{cap_model_output, cap_model_output_with_handle};
use output_budget::{paginate_unspillable, read_file_page};
// #2672: numeric args as models send them (int, "531", 531.0); fails loudly otherwise.
use super::tool_args::nonnegative_usize as arg_usize;
pub use output_budget::{
    set_max_output_tokens, set_output_cap_chars_per_token, set_output_head_tokens,
};

mod build_shell;
mod catalog;
mod dependency_fetch;
mod dispatch;
pub(super) mod file_capture;
mod file_change;
#[cfg(feature = "ast")]
mod move_from;
mod navigation;
mod path_suggest;
#[cfg(test)]
use dispatch::execute_tool_with_display_cancellable;
pub use dispatch::{
    execute_tool, execute_tool_with_offload, execute_tool_with_offload_and_prompt_and_artifacts,
};
pub(crate) use dispatch::{execute_tool_with_collaborators, ToolCollaborators};
use path_suggest::FileIoError;
pub(crate) mod exposure;
mod grep_tool;
mod live_output;
mod native_git;
pub(crate) mod output_budget;
mod read_history;
pub(crate) use read_history::ReadHistory;
mod shell;
pub use shell::{run_command_dialect_sentence, shell_dialect_sentence};
/// Real-resource (PTY) proof of the tool-call liveness contract (#1727): a
/// silent tool is never a blank row, and the first live byte takes the row
/// for good. Unix-only — it needs a real pty pair.
#[cfg(all(test, unix))]
mod tool_spinner_pty_test;
use live_output::LiveOutputSession;
#[cfg(test)]
pub(crate) use shell::absent_binary_refusal;
pub use shell::venv_cmd_prefix;
#[cfg(test)]
use shell::{
    confined_dispatch_args, decode_shell_stream, denial_recovery_hints, denied_run_command_result,
    envelope_denial_reason, envelope_denied, exec_denial_requests, exec_floor_permits,
    net_denial_requests, pr_creation_url, same_file_redirect_refusal, shadow_records, shell_engine,
    shell_envelope_output, venv_env_map,
};
use shell::{
    declared_filesystem_requests, dispatch_caveats_for_git_shell, exec_confined_command,
    permits_filesystem_request, resolve_exec_cwd, split_leading_cd,
};
#[cfg(all(test, not(windows)))]
use shell::{
    host_shell_command, host_shell_output, host_shell_output_with_timeout,
    CHILD_STRIPPED_AUTHORITY_ENV,
};
pub(crate) use shell::{ABSENT_BINARY_MARKER, NOT_ON_HOST_MARKER};

#[cfg(test)]
use catalog::lifecycle_tool_definition;
pub(crate) use catalog::{
    classify_gated_off_reach, classify_phantom_reach, is_context_remaining_call, is_hallucination,
    known_builtin_tool_name, merged_tool_definitions, resolve_tool_alias, AliasOutcome,
};
use catalog::{
    disposition_tool_denied_message, is_mcp_tool_name, run_command_creates_shell_git_commit,
    run_command_redirect, unknown_tool_message,
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
pub(crate) use exposure::{select_exposed, select_openai_compatible_tools, HiddenTools};
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
                            "tool '{name}' arguments are not valid JSON (call truncated or malformed): {e}. \
                             If you were writing a large file, write it in smaller pieces."
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

/// Abort a turn whose tool calls cannot be correlated with their results. The
/// model answered with unusable output, so this files as `model_error` on every
/// wire, as the same defect does in a strictly decoded stream (#2318).
pub(crate) fn uncorrelatable_tool_calls(reason: &str) -> anyhow::Error {
    super::observability::DispatchError::new(
        super::observability::ErrorClass::Model,
        format!("malformed provider output: {reason}"),
    )
    .into()
}

/// Consecutive uncorrelatable batches a turn tolerates before it aborts. The
/// third in a row is fatal; any well-formed batch resets the count.
pub(crate) const MAX_UNCORRELATABLE_BATCHES: u32 = 3;

/// Recovery for a batch whose calls cannot be correlated (missing, blank or
/// duplicate ids), shared by every wire that requires call ids. Nothing was
/// dispatched and no id is fabricated: while the budget lasts this returns the
/// plain user-role message to append (there is no tool result to key it to) so
/// the caller can `continue` the loop; once `strikes` reaches
/// [`MAX_UNCORRELATABLE_BATCHES`] it aborts exactly as before.
pub(crate) fn reask_uncorrelatable(
    strikes: &mut u32,
    reason: &str,
    smart_harness: Option<&super::smart_harness::SmartHarness>,
    tool_events: Option<&mut Vec<crate::ToolEvent>>,
) -> anyhow::Result<String> {
    *strikes += 1;
    let recoverable = *strikes < MAX_UNCORRELATABLE_BATCHES;
    if let Some(harness) = smart_harness {
        harness.reject_tools(reason, recoverable)?;
    }
    if !recoverable {
        return Err(uncorrelatable_tool_calls(reason));
    }
    // The trace must show why this round produced nothing (mirrors the
    // content-invalid arm's not-ok "(rejected tool-call batch)" event).
    if let Some(rec) = tool_events {
        rec.push(crate::ToolEvent::from_call(
            "(rejected tool-call batch)",
            &serde_json::Value::Null,
            false,
            Some(0),
        ));
    }
    let reason: String = reason.chars().take(200).collect();
    Ok(format!(
        "Your last reply's tool calls could not be correlated ({reason}), so none of them \
         were run. Re-emit the calls you intended as native tool calls (not as text in your \
         reply), each with its own unique, non-empty call id."
    ))
}

/// Drop the id-less `tool_calls` from the assistant turn recorded just before
/// validation: with no ids there is nothing to answer them with, and replaying
/// them would send the provider a transcript it rejects.
///
/// Only an assistant turn is touched: if the last message is anything else the
/// transcript is left alone (the re-ask is still sent).
pub(crate) fn withdraw_tool_calls(assistant_turn: &mut serde_json::Value) {
    if assistant_turn["role"] != "assistant" {
        return;
    }
    if let Some(turn) = assistant_turn.as_object_mut() {
        turn.remove("tool_calls");
        // A tool-only reply is replayed with `content: ""` on the default path,
        // and strict gateways reject an empty assistant turn: null and blank
        // alike get the placeholder.
        if turn
            .get("content")
            .is_none_or(|c| c.is_null() || c.as_str().is_some_and(|t| t.trim().is_empty()))
        {
            turn.insert(
                "content".into(),
                "(tool calls without ids were discarded)".into(),
            );
        }
    }
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
/// Scope: `run_command` only.
///
/// **The stub-shell caveat this paragraph used to carry is no longer true, and
/// its own removal condition has been met.** It said stub-shell builds were the
/// only crates.io-publishable configuration and that agent-bridle's `shell`
/// tool failed closed on every command there. Measured on 2026-09-08:
/// `cargo tree -p newt-core --no-default-features -i agent-bridle-tool-shell
/// -e features` reports the `brush` and `carried-coreutils` features enabled,
/// from the unconditional dependency edge in `newt-core/Cargo.toml`; no newt
/// feature disables the shell backend, there is no `[patch]` section in the
/// workspace, and `brush-ocap-*` resolve from crates.io in `Cargo.lock`. The
/// publishable configuration and the real-shell configuration are the same
/// configuration. `web_fetch` is NOT bypassed, and never was affected by the
/// caveat above either way — `agent-bridle-tool-web` ships the real
/// leash-enforcing implementation (verified at agent-bridle rev `2129c91`), so
/// it does not fail closed. The fs tools keep the
/// newt-native workspace fence untouched: yolo is unconfined exec, fenced fs
/// — never a global authority-off switch.
///
/// The removal condition recorded here — "when agent-bridle's real confined
/// shell becomes the default everywhere" — has been met, by published
/// `brush-ocap-*` crates rather than by an upstream brush merge
/// (reubeno/brush#1184 remains open). Whether the `--disable-ocap` escape
/// hatch itself should now be removed or demoted to a debug flag is a separate
/// question about the bypass, not about shell availability, and is not decided
/// here.
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

// The lexical-normalisation + prefix-containment helpers now live in one shared
// place — `crate::caveats` — so the interactive tool gate here and the headless
// `newt-coder` apply path decide containment identically (no drift surface).
// Only the Linux object-bound helpers below normalise paths directly; the
// prefix gate itself goes through `crate::caveats::permits_path`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::caveats::lexically_normalize;

/// Returns true if `full_path` is permitted by `scope`, under prefix
/// (containment) semantics. Thin alias for [`crate::caveats::permits_path`],
/// kept so the many call sites in this module read as the tool-gate they are;
/// the containment logic lives in one shared owner.
pub(crate) fn tui_permits_path(scope: &crate::caveats::Scope<String>, full_path: &str) -> bool {
    crate::caveats::permits_path(scope, full_path)
}

/// #2516/#2533: a routed call's result note is always APPENDED, never
/// prepended. `tool_result_ok` classifies a result by its PREFIX
/// (`error:`, `capability denied:`, …); prepending a note would shift
/// that prefix off the front of the string and make a failed/denied
/// routed call read as `ok: true` — the exact bug #2516 round 1 fixed
/// for the git route and #2533 round 1 reintroduced for the build
/// route. One helper, used by every route, makes the mistake
/// unrepresentable instead of relying on each site to remember. Not
/// platform-gated — every route on every OS goes through this.
fn append_routed_note(text: String, note: impl std::fmt::Display) -> String {
    format!("{text}\n{note}")
}

/// Does `just` have a justfile to find, starting at `dir`? `just` itself
/// walks up from the cwd through every ancestor looking for `justfile` /
/// `Justfile` (that's how a subcrate's `just check` finds the workspace
/// root's), so the routed check must do the same or it refuses to route a
/// `just` call the shell path would have run fine. Not platform-gated —
/// `std::path::Path::ancestors` and `.exists()` are portable.
fn justfile_findable_from(dir: &std::path::Path) -> bool {
    dir.ancestors()
        .any(|d| d.join("justfile").exists() || d.join("Justfile").exists())
}

/// The root in `scope` that lexically authorises `full_path`, if any.
///
/// `Some(Some(root))` — permitted, and `root` is the granted file or directory
/// that anchors the object-bound access. `Some(None)` — permitted
/// with no containing root (`Scope::All`, e.g. `--full-access`), so there is no
/// object fence. `None` — not permitted. Mirrors [`tui_permits_path`]'s matching
/// exactly (same normalisation + `starts_with`), so the object-bound read
/// resolves beneath the very root the gate approved.
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_fs_containment_denied(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EXDEV) | Some(libc::ELOOP))
        || (cfg!(target_os = "macos")
            && matches!(e.raw_os_error(), Some(libc::ENOTDIR) | Some(libc::EMLINK)))
}

/// The object-binding target for a scope-authorised fs op: `Some(Some((root,
/// rel)))` to resolve `rel` beneath `root`'s fd; `Some(None)` for `Scope::All`
/// (no fence — the caller uses `std::fs`); `None` if the scope denies (a logic
/// error at a call site that already gated — callers fail closed). One shared
/// resolver behind the object-bound read/list arms.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn object_bound_target<'a>(
    scope: &'a crate::caveats::Scope<String>,
    full_str: &str,
) -> Option<Option<(&'a str, std::path::PathBuf)>> {
    authorizing_root(scope, full_str)
        .map(|opt| opt.map(|root| (root, contained_relative(full_str, root))))
}

/// Unconfined directory listing via `std::fs` — the `Scope::All` / non-Linux /
/// #263-gate-approved path. One owner so the three call sites don't drift.
fn std_list_dir(full: &std::path::Path) -> Result<Vec<String>, FileIoError> {
    match std::fs::read_dir(full) {
        Ok(entries) => Ok(entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()),
        Err(e) => Err(FileIoError::io(&e, full, format!("error: {e}"))),
    }
}

/// Object-bound read of `full` (the workspace-joined model path) beneath the
/// root that authorised it. Returns the file contents, or a ready-to-return
/// tool-output string on failure: a containment escape becomes an `fs_read`
/// denial, any other failure the ordinary read error. Under `Scope::All`
/// (`--full-access`) there is no object fence, so it reads via `std::fs` — the
/// pre-existing unconfined behaviour. Linux (`openat2`) and macOS (no-follow fd walk); the other-platform
/// fallback keeps the lexical-gate + `std::fs` path.
///
/// `axis` labels the denial (`fs_read` for read_file; `fs_write` for edit_file,
/// whose read is authorised by — and contained beneath — the `fs_write` root).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn object_bound_read(
    scope: &crate::caveats::Scope<String>,
    axis: &str,
    path: &str,
    full: &std::path::Path,
    full_str: &str,
) -> Result<String, FileIoError> {
    use std::io::Read;
    match object_bound_target(scope, full_str) {
        // The gate already permitted this read, so `None` here would be a logic
        // error (the two matchers disagreeing); fail closed rather than read.
        None => Err(denied_fs_result(axis, full_str).into()),
        Some(None) => std::fs::read_to_string(full)
            .map_err(|e| FileIoError::io(&e, full, format!("error: reading {path}: {e}"))),
        Some(Some((root, rel))) => {
            let read = crate::fs_cap::WorkspaceDir::open_granted_file(
                std::path::Path::new(root),
                &rel,
                false,
            )
            .and_then(|mut f| {
                let mut s = String::new();
                f.read_to_string(&mut s)?;
                Ok(s)
            });
            match read {
                Ok(s) => Ok(s),
                Err(e) if is_fs_containment_denied(&e) => {
                    Err(denied_fs_result(axis, full_str).into())
                }
                Err(e) => Err(FileIoError::io(
                    &e,
                    full,
                    format!("error: reading {path}: {e}"),
                )),
            }
        }
    }
}

/// Object-bound directory listing beneath the authorising root — the `list_dir`
/// analogue of [`object_bound_read`]. A symlink-escape directory is refused by
/// the kernel (an `fs_read` denial); the entries are read straight off the dir
/// fd. `Scope::All` lists via `std::fs`. Available on Linux and macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn object_bound_list(
    scope: &crate::caveats::Scope<String>,
    full: &std::path::Path,
    full_str: &str,
) -> Result<Vec<String>, FileIoError> {
    match object_bound_target(scope, full_str) {
        None => Err(denied_fs_result("fs_read", full_str).into()),
        Some(None) => std_list_dir(full),
        Some(Some((root, rel))) => {
            match crate::fs_cap::WorkspaceDir::open_root(std::path::Path::new(root))
                .and_then(|dir| dir.read_dir(&rel))
            {
                Ok(names) => Ok(names
                    .into_iter()
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect()),
                Err(e) if is_fs_containment_denied(&e) => {
                    Err(denied_fs_result("fs_read", full_str).into())
                }
                Err(e) => Err(FileIoError::io(&e, full, format!("error: {e}"))),
            }
        }
    }
}

/// Non-Linux fallback for the object-bound fs arms: `openat2` is unavailable, so
/// they keep the lexical-gate + `std::fs` behaviour (the symlink residual
/// persists on non-Linux — see `fs-canonical-containment`; CI/prod is Linux).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn object_bound_read(
    _scope: &crate::caveats::Scope<String>,
    _axis: &str,
    path: &str,
    full: &std::path::Path,
    _full_str: &str,
) -> Result<String, FileIoError> {
    std::fs::read_to_string(full)
        .map_err(|e| FileIoError::io(&e, full, format!("error: reading {path}: {e}")))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn object_bound_list(
    _scope: &crate::caveats::Scope<String>,
    full: &std::path::Path,
    _full_str: &str,
) -> Result<Vec<String>, FileIoError> {
    std_list_dir(full)
}

/// Unconfined write via `std::fs` (creating parents) — the `Scope::All` /
/// non-Linux / #263-gate-approved path. One owner so the call sites don't drift.
fn std_write(full: &std::path::Path, path: &str, content: &str) -> Result<(), FileIoError> {
    if let Some(parent) = full.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(full, content)
        .map_err(|e| FileIoError::io(&e, full, format!("error: writing {path}: {e}")))
}

/// Object-bound write of `content` to `full` (the workspace-joined model path)
/// beneath the root that authorised it — the write analogue of
/// [`object_bound_read`]. The file (and any missing parents) is created *beneath*
/// the granted root's fd (`openat2 RESOLVE_BENEATH`), so a symlink / `..` /
/// absolute escape the lexical gate admits is refused by the kernel: a
/// containment escape becomes an `fs_write` denial, any other failure the
/// ordinary write error. `Scope::All` (`--full-access`) writes via `std::fs`.
/// Linux and macOS; the other-platform fallback keeps the lexical-gate + `std::fs` path.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn object_bound_write(
    scope: &crate::caveats::Scope<String>,
    axis: &str,
    path: &str,
    full: &std::path::Path,
    full_str: &str,
    content: &str,
) -> Result<(), FileIoError> {
    use std::io::Write;
    match object_bound_target(scope, full_str) {
        None => Err(denied_fs_result(axis, full_str).into()),
        Some(None) => std_write(full, path, content),
        Some(Some((root, rel))) => {
            let write =
                crate::fs_cap::WorkspaceDir::create_granted_file(std::path::Path::new(root), &rel)
                    .and_then(|mut f| {
                        f.write_all(content.as_bytes())?;
                        Ok(())
                    });
            match write {
                Ok(()) => Ok(()),
                Err(e) if is_fs_containment_denied(&e) => {
                    Err(denied_fs_result(axis, full_str).into())
                }
                Err(e) => Err(FileIoError::io(
                    &e,
                    full,
                    format!("error: writing {path}: {e}"),
                )),
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn object_bound_write(
    _scope: &crate::caveats::Scope<String>,
    _axis: &str,
    path: &str,
    full: &std::path::Path,
    _full_str: &str,
    content: &str,
) -> Result<(), FileIoError> {
    std_write(full, path, content)
}

/// Object-bound file removal beneath the authorising root — the `delete_file`
/// analogue of [`object_bound_write`]. The parent is resolved object-bound and
/// the entry removed via `unlinkat`, so a symlink / `..` / absolute escape is
/// refused by the kernel (an `fs_write` denial). `Scope::All` removes via
/// `std::fs`. Available on Linux and macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn object_bound_delete(
    scope: &crate::caveats::Scope<String>,
    path: &str,
    full: &std::path::Path,
    full_str: &str,
) -> Result<(), FileIoError> {
    match object_bound_target(scope, full_str) {
        None => Err(denied_fs_result("fs_write", full_str).into()),
        Some(None) => std::fs::remove_file(full)
            .map_err(|e| FileIoError::io(&e, full, format!("error: deleting {path}: {e}"))),
        Some(Some((root, rel))) => {
            match crate::fs_cap::WorkspaceDir::open_root(std::path::Path::new(root))
                .and_then(|dir| dir.unlink(&rel))
            {
                Ok(()) => Ok(()),
                Err(e) if is_fs_containment_denied(&e) => {
                    Err(denied_fs_result("fs_write", full_str).into())
                }
                Err(e) => Err(FileIoError::io(
                    &e,
                    full,
                    format!("error: deleting {path}: {e}"),
                )),
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn object_bound_delete(
    _scope: &crate::caveats::Scope<String>,
    path: &str,
    full: &std::path::Path,
    _full_str: &str,
) -> Result<(), FileIoError> {
    std::fs::remove_file(full)
        .map_err(|e| FileIoError::io(&e, full, format!("error: deleting {path}: {e}")))
}

/// Whether `find`'s recursive-read root is contained to the WORKSPACE. Unlike
/// the other arms, `find` contains to the workspace *independent of the fs_read
/// scope* (even under `Scope::All`) — a recursive read is dangerous, so the
/// search root must stay in-tree. On Linux this is an object-bound
/// `openat2(RESOLVE_BENEATH)` check of the root beneath the workspace fd.
/// The subsequent legacy walker reopens the path after this descriptor is
/// discarded, so smart mode refuses this adapter and uses the confined shell.
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
/// child starts env-empty with explicit toolchain/support variables only. Its
/// writes stay within the workspace and its network uses only the caller's
/// existing operator grant, with deny-all as the default. Where that fence cannot be established
/// the spawn is **refused** rather than run unconfined (#10). It is no longer a
/// raw `sh -c` on the host.
pub(crate) fn run_build_check(
    cmd: &str,
    workspace: &str,
    network: &crate::Scope<String>,
) -> String {
    use crate::confined_exec::{build_tool_request, ConstrainedExecutor};
    let (program, args) = build_check_argv(cmd);
    let workspace = std::path::Path::new(workspace);
    let req = build_tool_request(workspace, workspace, program, args, network);

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

/// U5: a model that sends `\n` as two characters on a single line lands a
/// one-line blob where it meant several lines. The edit still applies (a string
/// literal may legitimately hold `\n`), so this only WARNS, and only when the
/// edit introduces literal backslash-n escapes into text with no real newline.
/// Never rewrites the model's text.
fn literal_newline_escape_warning(old_string: &str, new_string: &str) -> Option<&'static str> {
    (!new_string.contains('\n')
        && new_string.matches("\\n").count() > old_string.matches("\\n").count())
    .then_some(
        "warning: the edit was applied, but new_string contains literal `\\n` sequences \
             and no real newlines. If you meant line breaks, re-send with real newlines.",
    )
}

/// Preserve execution evidence while coaching an unavailable or timed-out
/// lifecycle run.
fn lifecycle_run_result(
    args: &serde_json::Value,
    mut result: (String, crate::ExecOutcome),
) -> (String, crate::ExecOutcome) {
    let suggestion = build_call_suggestion(args, args["phase"].as_str().unwrap_or(""));
    match result.1 {
        crate::ExecOutcome::Unavailable => {
            result.0.push_str(&format!(
                "\nThis was lifecycle action=run. For compiler/test validation, action=build \
                 requests explicit approval for toolchain/cache reads and workspace writes, \
                 with network denied unless covered by an existing operator grant. Existing permission requirements and denials remain binding; \
                 do not retry a declined grant.\nSuggested lifecycle call: {suggestion}"
            ));
        }
        // #F11 (verify-lane-steering): the confined shell already appends
        // the build-lane suggestion at the END of a timed-out envelope
        // (`timed_out_note`, shell.rs), after possibly-truncated partial
        // output. Measured (newt main a996fb9e): a model that hit the 60s
        // wall on `lifecycle phase=test` (default action=run) never reached
        // it there and retried the same losing call. `action=build` stays
        // the default's escalation, not the default itself — it demands
        // explicit permission-gate approval and denies network, so flipping
        // the default would silently change what a bare `phase=test` call
        // is authorized to do. Put the exact next call FIRST instead.
        crate::ExecOutcome::TimedOut => {
            result.0 = format!("Suggested lifecycle call: {suggestion}\n{}", result.0);
        }
        _ => {}
    }
    result
}

/// F19: labels a `lifecycle action=run` result that was escalated into the
/// build lane after hitting its wall, so the transcript says plainly why
/// this ran through `action=build`'s authority instead of leaving the model
/// to infer it from a suggestion it already ignored once. `wall` is the
/// clock that actually applied to the timed-out first run (#2541 round 3
/// item 3: `escalates` only reaches this path when that wall was the
/// DEFAULT, never the build lane's — see `escalates` — so this is always
/// `shell::run_command_wall_secs()`'s value, derived rather than a literal).
fn escalated_after_timeout(
    mut result: (String, crate::ExecOutcome),
    wall: std::time::Duration,
) -> (String, crate::ExecOutcome) {
    result.0 = format!(
        "This was lifecycle action=run; it hit the {}s wall, so it was re-run once in the \
         action=build lane (same permission-gate re-check, existing network grants only).\n{}",
        wall.as_secs(),
        result.0
    );
    result
}

/// Render a wall clock for the routed-note wording (F28 round 2): whole
/// minutes when the wall is minute-granular (`30 min`, matching round 1's
/// wording), seconds otherwise (`2s`) — a `timeout 2 …` test wall must not
/// be misreported as "0 min"/"1 min" by naive `div_ceil(60)` rounding.
fn format_wall(wall: std::time::Duration) -> String {
    let secs = wall.as_secs();
    if secs > 0 && secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// #2541/#2732: only an ordinary command timeout justifies spending the
/// build lane's authority on a second run. Classified builds already spent
/// the longer budget and must not repeat it. Other outcomes need no escalation.
fn escalates(outcome: crate::ExecOutcome, wall: std::time::Duration) -> bool {
    outcome == crate::ExecOutcome::TimedOut && wall != shell::LIFECYCLE_BUILD_TIMEOUT
}

/// #2541 round 2 item 1: compose the escalation's outcome with `first`'s
/// evidence — `first`'s partial output is the ONLY record of which test hung,
/// and it must survive even when the escalation itself never ran. When the
/// build lane actually executed (any outcome but `Denied`/`Unavailable` — a
/// frame-isolation refusal is `Denied` too, see `run_confined_build_lane`),
/// its own result already speaks for the whole call, so `first` is dropped
/// exactly as before.
///
/// When it refused to run (round 3 item 4): `first`'s RAW evidence is kept —
/// never through `lifecycle_run_result`, whose `TimedOut` arm prepends
/// "Suggested lifecycle call: action=build". That suggestion is exactly the
/// call the gate just declined; opening a refused escalation by recommending
/// it is wrong. Instead this says plainly that build authority was declined
/// for this call, not to retry it, and to narrow the command.
fn escalation_result(
    first: (String, crate::ExecOutcome),
    escalated: (String, crate::ExecOutcome),
    wall: std::time::Duration,
    command_budget: crate::RunCommandBudget,
) -> (String, crate::ExecOutcome) {
    if matches!(
        escalated.1,
        crate::ExecOutcome::Denied | crate::ExecOutcome::Unavailable
    ) {
        // #2541 follow-up: the confined shell already appended
        // `shell::timed_out_note(wall)` to `first.0` — which recommends
        // `lifecycle action=build` — before this call ever knew the
        // escalation would be declined. A result must carry ONE
        // recommendation, not that suggestion immediately followed by
        // "declined, do not retry it". Strip it when present; if the exact
        // suffix isn't there (a different envelope shape), leave the text
        // alone rather than guess.
        let first_text = first
            .0
            .strip_suffix(&shell::timed_out_note(wall, command_budget))
            .unwrap_or(&first.0);
        return (
            format!(
                "{first_text}\n\nThe action=build escalation did not run:\n{}\n\nBuild \
                 authority was declined for this call — do not retry it; narrow the \
                 command instead (one crate, one test filter).",
                escalated.0
            ),
            escalated.1,
        );
    }
    escalated_after_timeout(escalated, wall)
}

/// `lifecycle action=run` for NON-build-tool commands, with F19's timeout
/// escalation: on the default wall (never the build wall — see `escalates`),
/// one re-run through the SAME build lane `action=build` uses
/// (`run_confined_build_lane` — same `leq`/permission-gate re-check, never
/// bypassed). Since F38, runs whose resolved command starts with a build tool
/// (cargo/just/make) take the build lane directly and never reach this path.
/// A dedicated fn (not inlined in the `lifecycle` dispatch arm) so
/// `permission_gate`'s two sequential reborrows each end cleanly at their own
/// `.await`, rather than the borrow checker unifying them against the enclosing
/// dispatch fn's much larger lifetime graph.
#[allow(clippy::too_many_arguments)]
async fn lifecycle_run_with_escalation(
    args: &serde_json::Value,
    joined: &str,
    effective_dir: &str,
    effective_path: &std::path::Path,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    mut permission_gate: Option<&mut dyn PermissionGate>,
    tool_offload: bool,
    spill_store: Option<&dyn super::content_spill::SpillStore>,
    live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    smart_harness: Option<&super::smart_harness::SmartHarness>,
    presentation: &mut dyn ToolPresentation,
    command_budget: crate::RunCommandBudget,
) -> (String, crate::ExecOutcome) {
    let first = exec_confined_command(
        joined,
        effective_dir,
        workspace,
        color,
        tool_output_lines,
        caveats,
        &[],
        exec_floor,
        &mut permission_gate,
        tool_offload,
        spill_store,
        live_tool_output,
        presentation,
        command_budget,
    )
    .await;
    // F19: nine `action=run` calls died at the 60s wall before one
    // `action=build` call passed (measured, replay 2483-r7) — the model kept
    // retrying `run` even though the timeout note already named the
    // escalation. Route it instead of asking the model to remember: one
    // re-run, not a loop. #2541 round 3 item 2: the wall that actually
    // applied to THIS command — since #2543 a cargo/just phase already ran
    // under the build wall in the run lane, so `escalates` must see which
    // wall killed it, not assume the default.
    let wall = shell::dispatch_wall(joined, command_budget);
    if !escalates(first.1, wall) {
        return lifecycle_run_result(args, first);
    }
    let (program, argv) = build_check_argv(joined);
    let escalated = run_confined_build_lane(
        workspace,
        effective_path,
        program,
        argv,
        joined,
        smart_harness,
        caveats,
        &mut permission_gate,
        tool_output_lines,
        color,
        tool_offload,
        spill_store,
        // #2541 round 2 item 5, round 3 item 3: names the escalation in the
        // permission prompt reason, so the operator sees WHY a
        // build-authority prompt appeared out of an `action=run` call
        // instead of `action=build` — the wall is derived, not a literal
        // (this arm only runs when `wall` was the default, per `escalates`).
        Some(&format!(
            "lifecycle action=run hit the confined shell's {}s wall",
            wall.as_secs()
        )),
        None,
        shell::LIFECYCLE_BUILD_TIMEOUT,
        presentation,
    )
    .await;
    escalation_result(first, escalated, wall, command_budget)
}

/// F38: does a lifecycle `run`'s resolved command start with a build tool?
/// When it does, the run lane cannot execute it (the tool isn't in the run
/// lane's profile), so route directly to the build lane — the same path
/// `action=build` takes. Uses the same `is_build_tool_exec` predicate that
/// `dispatch_wall` and the routing table read.
fn lifecycle_run_routes_to_build_lane(cmd: &str) -> bool {
    shell::leading_program(cmd).is_some_and(crate::confined_exec::is_build_tool_exec)
}

/// The explicit `{"phase":...,"action":"build"}` call to suggest — one JSON
/// source shared by the timed-out/unavailable `action=run` coaching
/// (`lifecycle_run_result`) and F12's `phase="build"` refusal below, which
/// cannot reuse `args["phase"]` verbatim since that IS the bad value.
fn build_call_suggestion(args: &serde_json::Value, phase: &str) -> serde_json::Value {
    let mut suggestion = serde_json::json!({"phase": phase, "action": "build"});
    if let Some(dir) = args.get("dir").and_then(serde_json::Value::as_str) {
        suggestion["dir"] = dir.into();
    }
    suggestion
}

/// A call-scoped grant for the existing lifecycle surface, not an exec-axis
/// wildcard. The command stays visible at the decision point; no shell grant
/// (including exec:cargo) silently acquires compiler/test descendant authority.
/// `escalation_note`, when `Some`, is #2541 round 2 item 5: a build-authority
/// prompt that arrived because a DIFFERENT call (`action=run`) escalated
/// needs to say so, or the operator sees a `lifecycle action=build` request
/// they never asked for.
fn lifecycle_build_request(
    workspace: &str,
    command: &str,
    build: &crate::Caveats,
    escalation_note: Option<&str>,
) -> PermissionRequest {
    let reads = match &build.fs_read {
        crate::Scope::Only(roots) => roots.iter().cloned().collect::<Vec<_>>().join("\n"),
        crate::Scope::All => unreachable!("build reads are calibrated"),
    };
    let escalation = escalation_note
        .map(|note| format!("\nThis is an escalation: {note}."))
        .unwrap_or_default();
    let network = build_network_description(&build.net);
    PermissionRequest {
        tool: "lifecycle".into(),
        kind: DenialKind::Build,
        target: workspace.into(),
        reason: format!("Run this resolved lifecycle command: {command}\nRead roots (including any credentials stored within them):\n{reads}\nWrites within the workspace and its build scratch directory; {network}. Compiler, build-script and test subprocesses inherit the same filesystem fence.{escalation}"),
        harness_bound: false,
    }
}

fn build_network_description(network: &crate::Scope<String>) -> &'static str {
    match network {
        crate::Scope::All => "network allowed by the existing operator grant",
        crate::Scope::Only(hosts) if hosts.is_empty() => "network denied",
        crate::Scope::Only(_) => "network limited to the existing operator grant",
    }
}

/// Which end of a build's output `build_piped_to_trim_route`'s recognised
/// `| tail -N` / `| head -N` suffix asked to keep (F23 / #2524
/// "tail-pipe-routes"). Parsed from the routed call's `"trim"` JSON
/// (`{"mode": "tail"|"head", "n": N}`, [`routing::parse_trim_spec`]'s
/// shape) — never trusted beyond that one shape, so a malformed or missing
/// `trim` object degrades to "no trim" rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputTrim {
    Tail(u32),
    Head(u32),
}

impl OutputTrim {
    fn from_json(value: Option<&serde_json::Value>) -> Option<Self> {
        let value = value?;
        let n = u32::try_from(value.get("n")?.as_u64()?).ok()?;
        if n == 0 {
            return None;
        }
        match value.get("mode")?.as_str()? {
            "tail" => Some(Self::Tail(n)),
            "head" => Some(Self::Head(n)),
            _ => None,
        }
    }

    /// Concatenate `stdout` then `stderr` (the routed call's `2>&1` case is
    /// the common one — merge into one stream, same as the pipe the model
    /// wrote would have seen) and keep the last/first `n` LINES of the
    /// result, matching `| tail -n`/`| head -n`'s own unit.
    fn apply(self, stdout: &str, stderr: &str) -> String {
        let mut combined = String::with_capacity(stdout.len() + stderr.len());
        combined.push_str(stdout);
        combined.push_str(stderr);
        let lines: Vec<&str> = combined.lines().collect();
        let kept = match self {
            Self::Tail(n) => &lines[lines.len().saturating_sub(n as usize)..],
            Self::Head(n) => &lines[..(n as usize).min(lines.len())],
        };
        kept.join("\n")
    }

    /// The routed-note clause naming what happened and why, so the model
    /// sees the trim rather than silently receiving less output than it
    /// piped for.
    fn note_clause(self) -> String {
        match self {
            Self::Tail(n) => {
                format!("output trimmed to the last {n} lines, as `| tail -{n}` asked")
            }
            Self::Head(n) => {
                format!("output trimmed to the first {n} lines, as `| head -{n}` asked")
            }
        }
    }
}

/// Run `program argv` through the confined build lane — `build_tool_request`'s
/// calibrated fence, the same `leq`/permission-gate re-check `lifecycle
/// action=build` performs, and [`shell::LIFECYCLE_BUILD_TIMEOUT`] (30 min).
///
/// The shared core of `lifecycle action=build` (a *resolved phase* command)
/// and the P4 build route (`super::routing`, `build_exec`: the model's
/// *literal argv*, verbatim — never re-resolved, so no operand is ever
/// dropped). `display` is the command text shown in the permission reason.
#[allow(clippy::too_many_arguments)]
async fn run_confined_build_lane(
    workspace: &str,
    cwd: &std::path::Path,
    program: &str,
    argv: Vec<String>,
    display: &str,
    smart_harness: Option<&super::smart_harness::SmartHarness>,
    caveats: &crate::caveats::Caveats,
    // F19: double indirection — see the matching comment on
    // `exec_confined_command`'s parameter in `shell.rs`.
    permission_gate: &mut Option<&mut dyn PermissionGate>,
    tool_output_lines: usize,
    color: bool,
    tool_offload: bool,
    spill_store: Option<&dyn super::content_spill::SpillStore>,
    // #2541 round 2 item 5: `Some` names the reason a call OTHER than
    // `action=build` is spending build authority (currently only F19's
    // `action=run` timeout escalation); `None` for a direct `action=build` /
    // `build_exec` call, whose reason speaks for itself.
    escalation_note: Option<&str>,
    // F23 / #2524 "tail-pipe-routes": `Some` when the routed call carried a
    // `| tail -N` / `| head -N` suffix (`build_piped_to_trim_route`) — the
    // build's REAL exit code is still what decides `ExecOutcome`; only the
    // rendered output is cut, on the harness side, never inside a shell that
    // could mask the exit code. `None` for every other caller (unchanged).
    trim: Option<OutputTrim>,
    // F28 round 2 (PR-F28 review, Blocker 2): the wall clock THIS call
    // should die at. `shell::LIFECYCLE_BUILD_TIMEOUT` for every caller
    // except a routed `timeout N …` wrapper, which passes
    // `min(N, LIFECYCLE_BUILD_TIMEOUT)` so a model's own shorter hang guard
    // is honoured instead of silently widened to the lane's full 30 min.
    wall: std::time::Duration,
    presentation: &mut dyn ToolPresentation,
) -> (String, crate::ExecOutcome) {
    use crate::confined_exec::{build_tool_request, ConstrainedExecutor};
    let (root, cwd) = match build_shell::build_directory(workspace, cwd, caveats) {
        Ok(directory) => directory,
        Err(refusal) => return refusal,
    };
    let request = build_tool_request(&root, &cwd, program, argv, &caveats.net).timeout(wall);
    let build = request.caveats();
    if let Some(harness) = smart_harness {
        if let Err(error) = harness.validate_tool_authority(build, &root) {
            return (
                format!("Error: frame isolation: {error}"),
                crate::ExecOutcome::Denied,
            );
        }
    }
    if !build.leq(caveats) {
        let permission =
            lifecycle_build_request(&root.to_string_lossy(), display, build, escalation_note);
        let allowed = permission_gate.as_deref_mut().is_some_and(|gate| {
            matches!(gate.ask_with_caveats(build, &[permission]), PermissionDecision::Allow(allowed) if build.leq(&allowed))
        });
        if !allowed {
            return (
                "capability denied: lifecycle action=build requires explicit confined build authority; no command ran".into(),
                crate::ExecOutcome::Denied,
            );
        }
    }
    let mut run = ConstrainedExecutor::run_async(request.clone()).await;
    let mut fetch_note = None;
    if let Ok(out) = &run {
        if !out.success && dependency_fetch::needs_dependency_fetch(&out.stderr) {
            let read = |path: &std::path::Path| std::fs::read_to_string(path).ok();
            match dependency_fetch::fetch_locked_dependencies(
                &root,
                &cwd,
                caveats,
                permission_gate,
                read,
            )
            .await
            {
                Ok(()) => {
                    run = ConstrainedExecutor::run_async(request).await;
                    fetch_note = Some(dependency_fetch::FETCHED_NOTE.to_owned());
                }
                Err(reason) => fetch_note = Some(dependency_fetch::blocked_note(&reason, &cwd)),
            }
        }
    }
    render_confined_build_result(
        run,
        fetch_note.as_deref(),
        trim,
        tool_output_lines,
        color,
        tool_offload,
        spill_store,
        presentation,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_confined_build_result(
    run: Result<crate::confined_exec::ConfinedOutput, crate::confined_exec::ExecRefused>,
    fetch_note: Option<&str>,
    trim: Option<OutputTrim>,
    tool_output_lines: usize,
    color: bool,
    tool_offload: bool,
    spill_store: Option<&dyn super::content_spill::SpillStore>,
    presentation: &mut dyn ToolPresentation,
) -> (String, crate::ExecOutcome) {
    let (mut text, outcome) = match run {
        Ok(out) => {
            // The build's own exit code (`out.code`) is untouched by `trim` —
            // only the rendered stdout/stderr text is cut, never inside a
            // shell pipe that could mask it (the exact hazard this route
            // exists to avoid).
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let view = trim.map(|trim| trim.apply(&stdout, &stderr));
            // #2731: the terminal override is installed inside the renderer.
            // Include recovery there, not only in the returned model text.
            let recovery_view = fetch_note.map(|note| {
                let selected = view.clone().unwrap_or_else(|| format!("{stdout}{stderr}"));
                format!("{selected}\n{note}")
            });
            let envelope = serde_json::json!({
                "exit_code": out.code,
                "stdout": stdout,
                "stderr": stderr,
                "timed_out": out.timed_out,
            });
            (
                shell::shell_envelope_output_with_view(
                    &envelope,
                    recovery_view.as_deref().or(view.as_deref()),
                    tool_output_lines,
                    color,
                    tool_offload,
                    spill_store,
                    Some(presentation),
                ),
                shell::envelope_outcome(&envelope),
            )
        }
        Err(error) => (format!("error: {error}"), crate::ExecOutcome::Unavailable),
    };
    if let Some(note) = fetch_note.filter(|note| !text.contains(note)) {
        text.push('\n');
        text.push_str(note);
    }
    (text, outcome)
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
    let capability = serde_json::json!(capability);
    let target = serde_json::json!(target);
    format!(
        "This is outside your granted authority — to ask the operator, call \
         request_permissions(capability={capability}, target={target}, \
         reason=\"<why you need it>\"), or take a different approach that stays \
         within your current authority."
    )
}

/// Facts held by the caller at the refusal boundary. A workspace is a default
/// directory, not an authority claim. Scope details come from the caller's
/// already-held snapshot, which need not include command-specific setup roots.
/// Formatting must not refresh or mint a grant.
fn denial_context(
    workspace: &str,
    requested_cwd: Option<&str>,
    caveats: Option<&crate::caveats::Caveats>,
) -> String {
    fn scope(scope: &crate::caveats::Scope<String>) -> String {
        match scope {
            crate::caveats::Scope::All => "all".into(),
            crate::caveats::Scope::Only(roots) => {
                const DISPLAY_ROOTS: usize = 4;
                let shown: Vec<_> = roots.iter().take(DISPLAY_ROOTS).collect();
                let mut text = serde_json::json!(shown).to_string();
                let omitted = roots.len().saturating_sub(shown.len());
                if omitted > 0 {
                    text.push_str(&format!(" ({omitted} more roots)"));
                }
                text
            }
        }
    }

    let mut text = format!(
        "Workspace root: {}\nDefault tool directory: workspace root",
        serde_json::json!(workspace)
    );
    if let Some(cwd) = requested_cwd {
        text.push_str(&format!(
            "\nRequested command directory: {}",
            serde_json::json!(cwd)
        ));
    }
    if let Some(caveats) = caveats {
        text.push_str(&format!(
            "\nKnown filesystem grants: fs_read={}, fs_write={}",
            scope(&caveats.fs_read),
            scope(&caveats.fs_write)
        ));
    }
    text
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
///
/// `path` is the EXACT target: the workspace-joined absolute path the #263
/// gate is asked for (`fs_gate_allows`), never the model's relative spelling
/// (#2629). `caveats::permits_path` is a lexical prefix test, so a relative
/// root granted through `request_permissions` would not cover the retry — the
/// model would be told "granted" and denied again.
fn denied_fs_result(kind: &str, path: &str) -> String {
    format!(
        "capability denied: {kind} does not permit '{path}'. {}",
        denial_recovery_hint(kind, path)
    )
}

/// Consult the #263 gate for one denied fs path. Returns `true` only when
/// the human allowed it AND the re-minted caveats actually permit the path —
/// the widened authority is re-checked, never assumed.
/// Authorise and read one file exactly as `read_file` does, for every tool
/// that reads a file's contents: `read_file` itself and `write_file`'s
/// `copy_from`, so a copied range crosses the same fence a read would.
///
/// Did the fs_read SCOPE authorise this (the automatic, object-bound fence),
/// or only a #263 permission-gate grant (the human approving this exact
/// out-of-scope path)? That distinction decides whether the read is
/// object-bound. `Err` carries the model-facing refusal or read error.
pub(super) fn authorized_read(
    tool: &str,
    path: &str,
    workspace: &str,
    caveats: &crate::caveats::Caveats,
    permission_gate: Option<&mut (dyn PermissionGate + '_)>,
) -> Result<String, String> {
    let full = std::path::Path::new(workspace).join(path);
    let full_str = full.to_string_lossy();
    let scope_permits = tui_permits_path(&caveats.fs_read, &full_str);
    if !scope_permits {
        // #263: the gate may grant the read; deny (or no gate) keeps the
        // standard denial text bit-for-bit.
        let allowed = permission_gate.is_some_and(|gate| {
            fs_gate_allows(gate, tool, DenialKind::FsRead, &full_str, |c| &c.fs_read)
        });
        if !allowed {
            return Err(denied_fs_result("fs_read", &full_str));
        }
    }
    // #1176: shadow-OCAP — under --full-access the fs fence is top(), so this
    // read runs unconfined; record the path a leash would have gated on (no-op
    // unless recording is armed). `newt ocap propose` folds it into reviewable
    // fs candidates.
    if full_access_requested() {
        crate::flight_recorder::log_observed(
            crate::flight_recorder::ShadowAxis::FsRead,
            &full_str,
            tool,
        );
    }
    // step-52.2: object-bound read when the SCOPE authorised it — resolve
    // `path` beneath the granted root (openat2 RESOLVE_BENEATH), so a symlink /
    // `..` / absolute escape the lexical gate admits is refused by the kernel.
    // A gate-approved out-of-scope path was explicitly vouched for by the human
    // (#263), so it reads as-is; `Scope::All` (--full-access) is unconfined
    // inside `object_bound_read`.
    if scope_permits {
        object_bound_read(&caveats.fs_read, "fs_read", path, &full, &full_str)
    } else {
        std::fs::read_to_string(&full)
            .map_err(|e| FileIoError::io(&e, &full, format!("error: reading {path}: {e}")))
    }
    .map_err(|error| error.render(workspace, &caveats.fs_read))
}

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
        harness_bound: false,
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
        harness_bound: false,
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
        harness_bound: false,
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

fn permission_granted_result(capability: &str, target: &str) -> String {
    let mut result = format!(
        "granted: the operator allowed {capability} for '{target}'. \
         Retry the original operation now."
    );
    let field = match parse_capability(capability) {
        Some(DenialKind::FsRead) => Some("fs_read"),
        Some(DenialKind::FsWrite) => Some("fs_write"),
        _ => None,
    };
    if let Some(field) =
        field.filter(|_| std::path::Path::new(target).is_absolute() && !target.contains('\0'))
    {
        result.push_str(&format!(
            " For a native run_command retry, keep the command and add {field}={}; \
             one-shot filesystem grants apply only to the declared invocation.",
            serde_json::json!([target])
        ));
    }
    result
}

/// Recognize only the built-in's actual grant response, never arbitrary tool
/// output or a success-shaped refusal. This releases loop suppression only;
/// the retried operation still passes through its ordinary authority gate.
pub(super) fn permission_grant_succeeded(
    name: &str,
    args: &serde_json::Value,
    ok: bool,
    result: &str,
    execution: Option<crate::ExecOutcome>,
) -> bool {
    let capability = args["capability"].as_str().unwrap_or("").trim();
    let target = args["target"].as_str().unwrap_or("").trim();
    if name != "request_permissions"
        || !ok
        || parse_capability(capability).is_none()
        || target.is_empty()
    {
        return false;
    }
    // #2628/#2636: when a pending run_command was re-run on approval, the
    // result is the command's real output rather than the "granted: …
    // Retry" string. Recognize that shape from the SAME typed
    // `ExecOutcome` the replay's own `executed()` call already records
    // (round1's "additional nonblocking concern": a string heuristic here
    // is misled by child output that happens to start with "denied:", and
    // — the defect this replaces — misled the other way by output that
    // merely doesn't start with a denial prefix, which is not evidence of
    // a real grant). `execution` is `None` for the ordinary non-replay
    // path (`request_permissions` never calls `executed()` unless it
    // replayed), so it cannot false-positive on that path.
    result == permission_granted_result(capability, target)
        || execution == Some(crate::ExecOutcome::Passed)
}

/// #2628/#2636/#2681: a `run_command` denied purely for undeclared filesystem
/// authority, OR for an exec target outside the granted authority, remembered
/// for ONE immediate `request_permissions` reply — see
/// `UNGRANTED_FS_AUTHORITY_DENIAL` and `exec_denial_requests` for the exact
/// denial shapes eligible here.
///
/// Binding (round1 finding 1): `missing` is the exact set of requests this
/// command was denied on, captured at denial time. Replay is permitted only
/// when the operator's grant now covers every one of them — an unrelated or
/// narrower approval does not authorize replaying this invocation. The slot
/// is cleared on ANY tool call other than the `request_permissions` that
/// consumes it (see `execute_authorized_tool`), so an unrelated intervening
/// command — successful, failed, or a replacement for this one — invalidates
/// a stale rerun.
pub(crate) struct PendingRerun {
    cmd: String,
    cwd: String,
    declared: Vec<PermissionRequest>,
    /// What the operator's grant must cover for replay to proceed: for an FS
    /// denial, the subset of `declared` not covered by caveats at denial
    /// time; for an exec denial (#2681), the exec target(s) the confined
    /// shell reported as denied — independent of `declared`, which a plain
    /// `run_command` call rarely populates at all.
    missing: Vec<PermissionRequest>,
}

/// #2636 round4/round5: typed token the caller requires, ALONGSIDE its own
/// coverage re-check, before treating an approval as replay authorization.
/// The private field plus the single checked constructor (`new`) mean
/// nothing outside this module can mint one except by actually satisfying
/// `covers_missing` — this is nonblocking hardening of the intended
/// construction site (`tools_tests` never had another path to one; see
/// round4's token ruling), not a claim that the type alone makes an
/// ineligible replay impossible elsewhere in the codebase.
pub(super) struct EligibleReplay(());

impl EligibleReplay {
    /// Mint the token only when `covers_missing` actually holds — the
    /// RETURNED caveats cover every entry in `pending.missing`, not merely
    /// pre-prompt eligibility.
    fn new(covers_missing: bool) -> Option<Self> {
        covers_missing.then_some(Self(()))
    }
}

/// #2636 round3 blocker 1 / #2681: does a single grant of `kind` for `target`
/// cover EVERY request in `missing`? Checked BEFORE asking the operator, using
/// only the requested capability/target — never the widened caveats the gate
/// eventually returns, which don't exist yet — so the prompt shown to the
/// operator can honestly say whether this approval alone would replay the
/// bound command. For `FsRead`/`FsWrite` a single-target grant only ever
/// produces one path-scope root, so this mirrors `permits_filesystem_request`
/// against that one-root scope. For `Exec` there is no path hierarchy to
/// widen into — coverage is the EXACT target string the command was denied
/// on, deliberately no more tolerant than that (a bare name and its resolved
/// absolute path are different targets, same as the narrowing already pinned
/// in newt-tui's `bare_pending_once_exec_grant_survives_an_absolute_request_
/// denial`). An unrelated target, the wrong axis, or a grant that covers only
/// part of `missing` (e.g. one of two denied paths) all return `false`.
fn single_grant_covers_missing(
    kind: DenialKind,
    target: &str,
    missing: &[PermissionRequest],
) -> bool {
    if missing.is_empty() {
        return false;
    }
    missing.iter().all(|request| {
        if request.kind != kind {
            return false;
        }
        match kind {
            DenialKind::FsRead | DenialKind::FsWrite => {
                let scope = crate::caveats::Scope::only([target.to_string()]);
                crate::caveats::permits_path(&scope, &request.target)
            }
            DenialKind::Exec => request.target == target,
            _ => false,
        }
    })
}

/// #2681 round 3 (P1): an exec denial replays safely ONLY when `cmd`, in
/// its entirety, is a single simple command with no dynamic content and no
/// redirect that can itself mutate state. Round 2's "denied spawn is the
/// FIRST command in source order" check conflated SOURCE order with
/// EXECUTION order: `agent_bridle::inspect_shell` is a non-executing,
/// source-order inventory, and source order is not a safety witness for
/// several shapes it still has to represent faithfully —
/// a command substitution's inner command expands (and can run arbitrary
/// effects, e.g. `/bin/echo "$(touch marker)"`) before the outer command it
/// decorates ever spawns; a pipeline's stages are not sequenced the way a
/// flattened list suggests (e.g. `/bin/echo denied | /bin/true` inventories
/// `/bin/echo` first even though both stages start together); and a
/// for-loop's single flattened body entry can run once per iteration, not
/// once. The SIMPLEST rule this inspection can honestly support is: exactly
/// one inventoried command, no `constructs` (command/backquote substitution,
/// arithmetic expansion — the dynamic-content axis), no `warnings` at all
/// (`inspect_shell` emits one for every `&&`/`||`/`;`-list member beyond the
/// first, every pipeline, `!`, `time`, `&` background, and every for-loop —
/// an empty list is itself proof none of those shapes are present), and no
/// redirect that can mutate state (see [`redirect_has_effect`]). This does
/// not rely on any engine checking a whole command before running part of
/// it: the safe-subset admission does in unit tests
/// (`approved_exec_replay_never_reruns_an_earlier_permitted_command`), but
/// that is not established for every production engine.
/// Anything else returns `false` and the model is told to re-issue the
/// command itself — the existing, always-safe fallback. Fails closed on an
/// inspection error.
fn exec_denial_is_replay_safe(target: &str, cmd: &str) -> bool {
    let Ok(inspection) = agent_bridle::inspect_shell(cmd) else {
        return false;
    };
    if !inspection.warnings.is_empty() || !inspection.constructs.is_empty() {
        return false;
    }
    let [only] = inspection.commands.as_slice() else {
        return false;
    };
    let Some(program) = only.program.as_deref() else {
        return false;
    };
    // Brush denies the resolved path, even when source names a bare executable.
    // This only establishes that the single command did not launch; it grants
    // nothing. The retry retains the exact path authority returned by the gate
    // and is checked again before spawning, even if PATH resolves differently.
    let names_target = program == target
        || (!program.contains(['/', '\\'])
            && std::path::Path::new(target).is_absolute()
            && std::path::Path::new(target)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(program));
    names_target && !only.redirects.iter().any(redirect_has_effect)
}

/// #2689/#2691 round 3 (P1): `exec_denial_is_replay_safe` compares TEXT only
/// — it says nothing about which AXIS was denied. A net denial's `target` is
/// a HOST, not the denied command's executable, and a host label that
/// happens to equal the program name (e.g. a CLI literally named after the
/// service it calls, like the `host` DNS-lookup tool reaching a host named
/// "host") would otherwise pass that string-equality check. Unlike an exec
/// denial, a network refusal does NOT prove the program never ran or never
/// produced an effect — the connection attempt happens mid-execution, after
/// the process has already started. So every request must be `DenialKind::
/// Exec` before the text predicate is even consulted; a net denial always
/// falls through to the existing "granted; re-run your command" path (its
/// manual retry), never an automatic replay.
fn requests_are_replay_safe(requests: &[PermissionRequest], cmd: &str) -> bool {
    requests.iter().all(|request| {
        request.kind == DenialKind::Exec && exec_denial_is_replay_safe(&request.target, cmd)
    })
}

/// A redirection that can mutate the filesystem independent of whether the
/// command it decorates ever actually spawns — unlike a read, an fd
/// duplication, or a here-doc/here-string, none of which touch anything
/// outside the command's own input. This crate does not control WHEN a
/// shell engine opens a redirect relative to its exec-authority check, so
/// [`exec_denial_is_replay_safe`] treats a write-shaped redirect as
/// disqualifying on principle rather than assuming today's engine order.
fn redirect_has_effect(redirect: &agent_bridle::InspectedRedirect) -> bool {
    matches!(
        redirect.operation,
        agent_bridle::RedirectOperation::Write
            | agent_bridle::RedirectOperation::Append
            | agent_bridle::RedirectOperation::ReadWrite
            | agent_bridle::RedirectOperation::Clobber
            | agent_bridle::RedirectOperation::OutputAndError
            | agent_bridle::RedirectOperation::AppendOutputAndError
    )
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
/// Returns `(Some(widened), message)` when the operator approved, `(None,
/// message)` on denial or when no gate is available.  The `widened` caveats
/// are threaded back to the dispatch arm for the #2628 one-shot re-run.
///
/// `bound`: `Some(pending)` when a #2628 `pending_rerun` is queued for THIS
/// call — the model is answering a specific prior denial, not asking
/// proactively. Then:
/// - On `Allow`, immediately tell the gate to drop any proactive once-grant
///   it queued for a later retry (round1 finding 2): the automatic replay is
///   about to spend it right here, so it must not also remain available to
///   widen a later, unrelated operation on the same (kind, target).
/// - The reason shown to the operator is harness-authored, not the
///   model-supplied text: it names the bound command and cwd and says
///   plainly that approval executes it once (round1 finding 4) — the model's
///   `reason` cannot be trusted to disclose that, since the tool call is
///   model-selected and unverified.
fn execute_request_permissions(
    caveats: &crate::Caveats,
    args: &serde_json::Value,
    gate: Option<&mut dyn PermissionGate>,
    _color: bool,
    _tool_output_lines: usize,
    workspace: &str,
    bound: Option<&PendingRerun>,
) -> (
    Option<crate::caveats::Caveats>,
    Option<EligibleReplay>,
    String,
) {
    let capability = args["capability"].as_str().unwrap_or("").trim();
    let target = args["target"].as_str().unwrap_or("").trim();
    let reason = args["reason"].as_str().unwrap_or("").trim();
    // #2750: an exec grant cannot clear the session's worktree guard. Keep
    // this operator command out of the capability prompt rather than issuing
    // a misleading "granted" receipt for an action that was never performed.
    if target == "worktree-lift" || target == "/permissions worktree-lift" {
        return (
            None,
            None,
            "request_permissions: worktree-lift is an operator command, not a capability target. The operator must use /permissions worktree-lift; no permission was granted and no worktree protection was lifted.".into(),
        );
    }

    let Some(kind) = parse_capability(capability) else {
        return (
            None,
            None,
            format!(
                "request_permissions: unknown capability '{capability}'. Use one of: \
                 exec, fs_read, fs_write, net."
            ),
        );
    };
    if target.is_empty() {
        return (
            None,
            None,
            "request_permissions: 'target' is required — the executable path or command name (exec), \
                   the path (fs_read/fs_write), or the host (net)."
                .to_string(),
        );
    }

    // #2636 round3 blocker 1: only promise the IMMEDIATELY-re-runs wording
    // when this exact grant would cover the COMPLETE missing set the command
    // was denied on. A partial or unrelated request still clears the pending
    // slot (already taken by the caller) but gets ordinary access-grant
    // wording that says plainly no automatic replay will happen.
    //
    // #2681 round 3 (P1): for `Exec`, coverage alone is not consent to
    // replay — `exec_denial_is_replay_safe` additionally requires
    // `pending.cmd` to be a single simple command with no dynamic content
    // or mutating redirect, so replay can never repeat an earlier
    // construct's, pipeline sibling's, or loop iteration's effect (see its
    // doc comment).
    let eligible = bound.filter(|pending| {
        single_grant_covers_missing(kind, target, &pending.missing)
            && (kind != DenialKind::Exec || exec_denial_is_replay_safe(target, &pending.cmd))
    });

    let request = PermissionRequest {
        tool: "request_permissions".to_string(),
        kind,
        target: target.to_string(),
        reason: match eligible {
            // #2636 finding 4: harness-authored disclosure of the execution this
            // approval triggers — never the model's `reason`. The `harness_bound`
            // field (not the prefix string) is what reason_is_model_authored checks.
            Some(pending) => format!(
                "{BOUND_REASON_PREFIX}approving this widens {capability} for '{target}' and \
                 IMMEDIATELY re-runs the command it was denied for, ONCE, under exactly that \
                 grant: `{}` in `{}`. It will not run again without a fresh approval.",
                pending.cmd, pending.cwd
            ),
            None if bound.is_some() => format!(
                "model requested {capability} for '{target}'. This grant alone does not \
                 cover everything the previously denied command needs, so it will NOT be \
                 automatically re-run — retry it yourself with a fresh run_command call."
            ),
            None if reason.is_empty() => format!("model requested {capability} for '{target}'"),
            None => reason.to_string(),
        },
        harness_bound: eligible.is_some(),
    };

    let quoted_target = serde_json::json!(target);
    let mut out = match gate {
        // The gate consults the operator and (for a session grant) remembers it,
        // exactly as a denial-driven prompt does.
        Some(g) => match g.ask(std::slice::from_ref(&request)) {
            PermissionDecision::Allow(widened) => {
                // #2636 round5: consumption and the `EligibleReplay` token both
                // require the RETURNED `widened` caveats to actually cover
                // every entry in `pending.missing` — not just pre-prompt
                // eligibility (the operator's Allow can widen a narrower or
                // different scope than requested). An eligible-but-insufficient
                // approval must neither execute the replay NOR spend the
                // once-grant: report plainly (via the caller's fallback
                // message) that the command was not re-run.
                let covers_missing = eligible.is_some_and(|pending| {
                    pending.missing.iter().all(|r| match r.kind {
                        // #2681: an exec entry is checked against the actual
                        // enforcement predicate, not `permits_filesystem_request`
                        // (which returns `false` for every non-FS kind by design).
                        DenialKind::Exec => widened.permits_exec(&r.target),
                        _ => permits_filesystem_request(&widened, r),
                    })
                });
                let replay_auth = EligibleReplay::new(covers_missing);
                if replay_auth.is_some() {
                    g.consume_pending_once(kind, target);
                }
                return (Some(widened), replay_auth, permission_granted_result(capability, target));
            }
            PermissionDecision::Deny => (
                None,
                None,
                format!(
                    "denied: the operator declined {capability} for {quoted_target}. \
                     Do not retry it — take a different approach."
                ),
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
        // A redundant headless request needs neither a new grant nor replay
        // consent. Bound denials still require the explicit approval path.
        None if bound.is_none() && match kind {
            DenialKind::Exec => caveats.permits_exec(target),
            DenialKind::Net => caveats.permits_net(target),
            _ => permits_filesystem_request(caveats, &request),
        } => (
            None,
            None,
            format!("already granted: {capability} for {quoted_target}. Retry the original operation now."),
        ),
        None => (
            None,
            None,
            format!(
                "no operator available to grant {capability} for {quoted_target} — this session \
                 has no interactive permission gate (headless / eval / piped), so authority \
                 cannot be widened mid-run and re-calling request_permissions will not help. \
                 Proceed within the authority you already have and the tools available to you; \
                 if {quoted_target} is genuinely essential and outside your current scope, say so in \
                 your final answer rather than retrying it."
            ),
        ),
    };
    if matches!(kind, DenialKind::FsRead | DenialKind::FsWrite) {
        out.2.push('\n');
        // A declined question supplies no fresh authority snapshot. In
        // particular, do not remint capabilities just to render diagnostics.
        out.2.push_str(&denial_context(workspace, None, None));
    }
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

/// Extract the denied host from a [`agent_bridle::ToolError::Denied`] reason
/// IFF the reason is an EXACT match for the literal wording two known leash
/// constructors emit — never a substring check (#2645 round 2). Both
/// `agent_bridle_core::ToolContext::check_net`
/// (vendor/agent-bridle-core/src/context.rs) AND agent-bridle-tool-web's own
/// `NetGuardError::HostNotAllowed` (`net_guard.rs`, converted to `Denied` at
/// `web_fetch.rs`'s `net_guard_to_tool`) use the identical
/// `format!("network access to {host:?} is not within the granted authority")`
/// — correcting round 2's comment, which claimed only `check_net` emits this
/// (#2645 round-2 review). Both are genuine net-authority denials for the
/// SAME host/scope the fetch loop already re-checks per hop
/// (`web_fetch.rs:250-269` calls `check_net` before `screen_host`), so
/// treating them alike is not a widening.
///
/// This function only ever sees the `Denied` variant's raw `reason` field —
/// never `ToolError`'s `Display` (which prepends `"denied: "`) and never any
/// other variant — see [`web_fetch_denial_host`], which matches the variant
/// FIRST (#2645 round 3). Requiring the anchored prefix/suffix (not
/// `.contains`) means wrapper text around an untrusted, interpolated string
/// — e.g. agent-bridle-tool-web's `redirect Location {location:?} is not a
/// valid URL: {e}` when a server sends a malformed `Location` — can never
/// satisfy the match merely by *containing* both trigger phrases somewhere in
/// a longer message.
fn parse_net_denial_host(reason: &str) -> Option<String> {
    let host = reason
        .strip_prefix("network access to \"")?
        .strip_suffix("\" is not within the granted authority")?;
    // A real hostname/IP never contains a literal `"` — the format's
    // `{host:?}` would escape one. Bail rather than guess at unescaping.
    (!host.is_empty() && !host.contains('"')).then(|| host.to_string())
}

/// The `web_fetch` dispatch-error → net-denial-journal decision (#2645 round
/// 3), factored out of the `execute_tool` match arm as a narrow seam: it
/// takes exactly what the call site has (the request URL, for parity with
/// the pre-#2645 shape of this decision, and the leash's `ToolError`) so
/// tests can drive it with a CONSTRUCTED error instead of a live network
/// dispatch. `url` is not consulted for the recorded host — #2645 round 2
/// fixed the bug where it was (a redirect can be denied on a DIFFERENT host
/// than the original request) — it is accepted only so the seam's signature
/// matches what a caller here actually has in hand.
///
/// Matches the `Denied` VARIANT before parsing anything (#2645 round-3
/// review): a `NotFound`, `Budget`, `Generation`, `Exec` or `Other` failure
/// never reaches [`parse_net_denial_host`] at all, regardless of what text it
/// happens to contain.
fn web_fetch_denial_host(_url: &str, err: &agent_bridle::ToolError) -> Option<String> {
    match err {
        agent_bridle::ToolError::Denied { reason } => parse_net_denial_host(reason),
        _ => None,
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
    let api_cfg = super::display::with_migration_notices(crate::Config::resolve_unpublished)
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
            if let Some(source) = args["move_from"]["path"].as_str() {
                return format!(
                    "{} (move functions from {})",
                    file_capture::display_text(&path),
                    file_capture::display_text(source)
                );
            }
            let copy = &args["copy_from"];
            match copy["path"].as_str() {
                Some(source) => format!(
                    "{} ({bytes} bytes + {}:{}-{})",
                    file_capture::display_text(&path),
                    file_capture::display_text(source),
                    copy["start_line"],
                    copy["end_line"],
                ),
                None => format!("{} ({bytes} bytes)", file_capture::display_text(&path)),
            }
        }
        "edit_file" | "delete_file" => file_capture::display_text(&string("path", "")).into_owned(),
        "read_file" => string("path", ""),
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
    read_scope: &crate::caveats::Scope<String>,
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
        if let super::routing::RouteDecision::Route { tool, args } =
            super::routing::RouteTable::builtin().classify_call(raw_args, workspace, read_scope)
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

/// The workspace walk `find` and `grep` share: gitignore-aware (also outside
/// a git checkout), no symlink following, and `target/`/`node_modules/` pruned
/// before descent. `respect_gitignore = false` means "walk everything".
pub(super) fn workspace_walker(
    root: &std::path::Path,
    respect_gitignore: bool,
    max_depth: Option<usize>,
) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(respect_gitignore)
        .ignore(respect_gitignore)
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore)
        .git_exclude(respect_gitignore)
        .parents(respect_gitignore)
        // Honour .gitignore even outside a git repo (the agent's cwd may not be
        // a checkout); without this `ignore` silently ignores gitignore files.
        .require_git(false)
        .follow_links(false);
    if let Some(d) = max_depth {
        builder.max_depth(Some(d));
    }
    // The `ignore` walker prunes via .gitignore/hidden but has no built-in
    // skip for build/dep dirs. Prune them explicitly (and cheaply, before
    // descent) so a default `find` doesn't drown in target/ or node_modules/.
    // `.git` is already covered by `.hidden(true)`. Skipped only when respecting
    // ignores — `respect_gitignore=false` means "search everything".
    if respect_gitignore {
        let mut ob = ignore::overrides::OverrideBuilder::new(root);
        // In override globs a leading `!` excludes; with no whitelist globs
        // present, everything else stays included.
        if ob.add("!target/").is_ok() && ob.add("!node_modules/").is_ok() {
            if let Ok(ov) = ob.build() {
                builder.overrides(ov);
            }
        }
    }
    builder
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

    let builder = workspace_walker(root, opts.respect_gitignore, opts.max_depth);

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
    open_regular_file(path, true)
}

#[cfg(unix)]
fn open_regular_file(path: &std::path::Path, nofollow: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | if nofollow { libc::O_NOFOLLOW } else { 0 })
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
    open_regular_file(path, true)
}

#[cfg(windows)]
fn open_regular_file(path: &std::path::Path, nofollow: bool) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    // Open the final component itself, rather than following a symlink or
    // another reparse point that could have replaced it after the lexical
    // check. This is the Windows analogue of Unix O_NOFOLLOW.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(if nofollow {
            FILE_FLAG_OPEN_REPARSE_POINT
        } else {
            0
        })
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
    open_regular_file(_path, true)
}

#[cfg(not(any(unix, windows)))]
fn open_regular_file(_path: &std::path::Path, _nofollow: bool) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "race-safe artifact file capture is unavailable on this platform",
    ))
}

/// Artifact reads use the same object boundary as model-facing file reads.
/// A physical-path check alone cannot protect a later reopen from a link swap.
fn artifact_open_scoped_regular_file(
    scope: &crate::caveats::Scope<String>,
    path: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let crate::caveats::Scope::Only(_) = scope {
        let Some(Some((root, relative))) = object_bound_target(scope, &path.to_string_lossy())
        else {
            return Err(std::io::Error::from_raw_os_error(libc::EACCES));
        };
        return crate::fs_cap::WorkspaceDir::open_granted_file(
            std::path::Path::new(root),
            &relative,
            true,
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = scope;
    artifact_open_regular_file(path)
}

fn artifact_preimage_state(
    scope: &crate::caveats::Scope<String>,
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
            let mut file = match artifact_open_scoped_regular_file(scope, path) {
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
fn artifact_file_matches(
    scope: &crate::caveats::Scope<String>,
    path: &std::path::Path,
    expected: &[u8],
) -> std::io::Result<bool> {
    file_contents_match(artifact_open_scoped_regular_file(scope, path)?, expected)
}

fn file_contents_match(mut file: std::fs::File, expected: &[u8]) -> std::io::Result<bool> {
    use std::io::Read as _;

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

/// #2424: the `exit_plan_mode` tool result now that exiting is a REQUEST, not
/// an immediate lift. The turn-end approval hook decides whether the clamp
/// actually lifts; on approval it delivers [`exit_plan_mode_result`]'s own
/// mandatory-edit guidance (when the initiative level requires one) to the
/// turn that follows, not to this ack.
const EXIT_PLAN_MODE_REQUESTED: &str = "exit requested. Awaiting operator approval — tool calls \
     remain clamped to Plan reads and the plan ledger until approved. If asked, continue drafting \
     with update_plan / render_report; do not assume approval.";

/// Plan-before-act: the ack appended to the first multi-step `update_plan` an
/// acting turn sets under an initiative level that looks before it acts. The
/// harness has entered the Plan phase on the model's behalf and asked for the
/// operator's answer, exactly as if the model had called `enter_plan_mode`
/// then `exit_plan_mode` itself: later calls in this batch are clamped, the
/// turn ends at the approval question, and approval seeds the implementing
/// turn with this plan. The model's first instinct (set the plan) is widened
/// into the collaboration the operator asked for; nothing new to learn.
const PLAN_APPROVAL_REQUESTED: &str = "plan recorded — awaiting operator approval before \
     implementation. Tool calls are limited to reads and the plan ledger until approved; do not \
     assume approval. If asked, refine the plan with update_plan.";

/// The exit-approved guidance. Under an initiative level that [`requires an
/// edit`](crate::initiative::Initiative::exit_plan_requires_edit) on plan exit
/// (Decisive / Eager), it appends a MANDATORY-EDIT directive so the model
/// executes the first step instead of sliding back into more reading (#11).
/// Lower levels leave plan exit advisory. The initiative action-forcing loop
/// (#10) then enforces it: a subsequent read-only round trips the forcing
/// nudge within the level's (small) budget.
///
/// #2424: pre-approval this WAS the `exit_plan_mode` tool's own ack (the
/// clamp lifted unconditionally). Now that lifting requires operator
/// approval (see [`EXIT_PLAN_MODE_REQUESTED`]), this text belongs on the
/// turn that FOLLOWS approval: the TUI's turn-end hook seeds that turn with a
/// harness-authored prompt (its own `ModelInputOrigin`, persisted under the
/// harness-retry provenance class — never disguised as operator input,
/// invariant 2.4) and this guidance rides inside it.
pub fn exit_plan_mode_result(initiative: crate::initiative::Initiative) -> String {
    let base = "exited the model-entered PLAN PHASE. Subsequent tool calls return to this turn's validated disposition and underlying session permissions; the next outer turn returns to the human-selected operating mode. `/mode plan` and other clamps still remain read-only.";
    if initiative.exit_plan_requires_edit() {
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
async fn execute_tool_unadopted(
    presentation: &mut dyn ToolPresentation,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    mut collab: ToolCollaborators<'_, '_>,
    tool_offload: bool,
    disposition: PromptDisposition,
) -> String {
    let Some(invocation) = collab.invocation else {
        return execute_authorized_tool(
            presentation,
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
        )
        .await;
    };
    let harness = invocation.harness();
    // Captured before `collab` is moved into the inner `execute_authorized_tool`
    // call below (`Option<&OnceLock<_>>` is `Copy`, so this doesn't disturb it).
    let execution = collab.execution;
    if let Err(error) = harness.validate_tool_authority(caveats, std::path::Path::new(workspace)) {
        invocation.host();
        if let Some(slot) = execution {
            let _ = slot.set(crate::ExecOutcome::Denied);
        }
        return format!("Error: frame isolation: {error}");
    }
    let mut gate =
        collab
            .permission_gate
            .take()
            .map(|inner| super::smart_harness::FramePermissionGate {
                harness,
                workspace: std::path::Path::new(workspace),
                inner,
                refusal: None,
            });
    let result = execute_authorized_tool(
        presentation,
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        ToolCollaborators {
            permission_gate: gate.as_mut().map(|gate| gate as &mut dyn PermissionGate),
            ..collab
        },
        tool_offload,
        disposition,
    )
    .await;
    match gate.and_then(|gate| gate.refusal) {
        Some(error) => {
            invocation.host();
            if let Some(slot) = execution {
                let _ = slot.set(crate::ExecOutcome::Denied);
            }
            format!("Error: frame isolation: permission denied: {error}")
        }
        None => result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_authorized_tool(
    presentation: &mut dyn ToolPresentation,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    collab: ToolCollaborators<'_, '_>,
    tool_offload: bool,
    // The prompt's validated disposition. This is deliberately a required
    // dispatcher input: catalog filtering is cosmetic, whereas this check is
    // the boundary that refuses fabricated tool names before MCP routing,
    // aliases, or permission widening can run.
    disposition: PromptDisposition,
) -> String {
    // One unpack; the dispatch body below binds the same names it always has.
    let ToolCollaborators {
        command_budget,
        shell_dialect,
        read_history,
        default_command_cwd,
        worktree_session,
        invocation,
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
        plan_draft_sink,
        spill_store,
        persona_tools,
        hidden_tools,
        live_tool_output,
        completed_spill_renderer: _,
        execution,
        governed_pr,
        command_directory,
        routed_to: routed_to_slot,
        pending_rerun,
        retry_authorized,
    } = collab;
    // #2636 (round1 finding 1): a queued #2628 rerun binds to the denial that
    // set it. ANY tool call other than the `request_permissions` that
    // consumes it invalidates a stale slot immediately — including a
    // successful/failed run_command that replaces the denied one, or any
    // unrelated tool activity in between. Only `request_permissions` reads
    // the slot below (and takes it), so every other arm sees it already
    // cleared.
    let pending_rerun = pending_rerun.map(|slot| {
        if name != "request_permissions" {
            *slot = None;
        }
        slot
    });
    let adopted = worktree_session.and_then(crate::worktree_adoption::WorktreeSession::snapshot);
    let build_workspace = adopted.as_ref().map_or(workspace, |policy| {
        policy.worktree.to_str().unwrap_or(workspace)
    });
    let smart_harness = invocation.map(|call| call.harness());
    // #2315: hand the shell's execution class to the funnel, return the text.
    let executed = |(text, outcome): (String, crate::ExecOutcome)| {
        if let Some(slot) = execution {
            let _ = slot.set(outcome);
        }
        text
    };
    let host_return = |text: String| {
        if let Some(invocation) = invocation {
            invocation.host();
        }
        text
    };

    // A model-entered Plan phase takes effect immediately for every later
    // tool call in the same inference round. The outer TUI also resolves it
    // into Plan caveats on the next turn; this local clamp closes the
    // enter-then-write gap before that boundary is rebuilt.
    let disposition = if plan_mode_control.is_some_and(super::PlanModeControl::is_plan_mode)
        && disposition != PromptDisposition::Ask
    {
        PromptDisposition::Plan
    } else {
        disposition
    };

    let (raw_name, raw_args) = (name, args);

    // Persona preferences affect discovery only. Evidence turns can request a
    // remote operation, but only a connected bridge and the human permission
    // gate below can authorize it. Plan/Ask still refuse before any prompt.
    let remote_call = mcp.handles(name);
    let evidence_turn = matches!(
        disposition,
        PromptDisposition::Explain | PromptDisposition::Research
    );
    if !tool_allowed(disposition, name)
        || (evidence_turn && is_mcp_tool_name(name) && !remote_call)
        || (name == "git"
            && disposition == PromptDisposition::Plan
            && !args
                .get("op")
                .and_then(|op| op.as_str())
                .is_some_and(super::git_tool::is_scoped_read_op))
    {
        return host_return(executed((
            disposition_tool_denied_message(disposition, name),
            crate::ExecOutcome::Denied,
        )));
    }

    // Explicit Plan retains its no-grant boundary. Inferred response style
    // cannot disable an operator's permission decision.
    if disposition == PromptDisposition::Plan && name != "request_user_input" && !remote_call {
        permission_gate = None;
    }

    // FR-3 (#998): the absolute deny-list — a grant-independent veto checked
    // immediately after the prompt-disposition boundary, above every other
    // leash (persona, MCP, alias, routing). It refuses
    // catastrophic exec (ssh / raw disk / systemctl restart …) by STRUCTURAL target,
    // so no capability, mode, or persona grant can unlock it. Runs on the RAW
    // name + args (pre-rewrite) so a shell alias or a routed command can't slip
    // past — and only the exec TARGET is matched, so the same words quoted in a
    // coach's question or a runbook note are untouched.
    if let Some(denied) = super::deny::deny_check(raw_name, raw_args) {
        return host_return(denied.reason);
    }

    // A known deferred schema is promoted through the existing exposure
    // controller. Promotion does not execute the call or grant permission;
    // the retry must pass the same dispatch checks and remote gate below.
    if let Some(message) = hidden_tools.and_then(|hidden| hidden.promote(name).message(name)) {
        return host_return(message);
    }

    if remote_call {
        let Some(gate) = permission_gate else {
            return host_return(executed((
                format!(
                    "MCP tool `{name}` requires OCAP permission, but no interactive permission gate is available. \
                     Enable permission prompts in an interactive session, then retry."
                ),
                crate::ExecOutcome::Denied,
            )));
        };
        let request = PermissionRequest {
            tool: name.to_string(),
            kind: DenialKind::RemoteTool,
            target: name.to_string(),
            reason: "Approve this remote MCP operation. Persona preferences and server tool hints do not grant permission.".into(),
            harness_bound: false,
        };
        let grant = matches!(gate.ask(&[request]), PermissionDecision::Allow(_))
            .then_some(McpGrant::HumanApproved);
        return match leash_mcp_call(name, args, grant) {
            Ok(leased) => mcp.call(&leased).await,
            Err(_) => host_return(executed((
                format!(
                    "MCP tool `{name}` was not run: OCAP permission was denied or cancelled. \
                     Respect that decision; do not retry it through another tool."
                ),
                crate::ExecOutcome::Denied,
            ))),
        };
    }

    // Step 27.1: resolve foreign / hallucinated tool names (str_replace_editor,
    // execute, bash, …) BEFORE the dispatch match. Compatible-arg aliases
    // rewrite to the canonical name and dispatch transparently; the rest return
    // a correction that names the right tool. Real names (and MCP `server__tool`
    // names, handled above) fall through unchanged.
    let name = match resolve_tool_alias(name) {
        Some(AliasOutcome::Rewrite(canonical)) => canonical,
        Some(AliasOutcome::Correct(msg)) => return host_return(msg),
        None => name,
    };

    let command_args = match shell::command_args_with_default_cwd(
        name,
        args,
        workspace,
        default_command_cwd,
        worktree_session,
    ) {
        Ok(args) => args,
        Err(error) => return host_return(error.to_owned()),
    };
    let args = command_args.as_ref();

    // Eligible file reads and lifecycle commands use the governed built-ins.
    // Git commands keep their original arguments and use the confined exec
    // path. The route/gate split is data in `routing::RouteTable`.
    //
    // `--no-route` / `NEWT_NO_ROUTE` ([`routing_disabled`]) turns this L2
    // convenience OFF — the command runs the normal exec path as-is — while the
    // L3 boundary (the confined shell below, the fs fence) STAYS. It is a switch
    // DISTINCT from `--disable-ocap` (§7-F5): the routing escape never disables
    // confinement.
    let routed: Option<(&'static str, serde_json::Value)> =
        if name == "run_command" && !routing_disabled() {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let decision = super::routing::RouteTable::builtin().classify_call(
                args,
                std::path::Path::new(workspace),
                &caveats.fs_read,
            );
            let decision = build_shell::route_for_ocap(decision, ocap_disabled());
            let decision = match decision {
                super::routing::RouteDecision::Route { tool: "find", .. }
                    if smart_harness.is_some() =>
                {
                    super::routing::RouteDecision::Exec
                }
                decision => decision,
            };
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
    // #2551 round 2: record the decision dispatch is ABOUT to act on —
    // never re-derived later (`is_progress_verification`'s should-fix).
    if let (Some(slot), Some((tool, routed_args))) = (routed_to_slot, &routed) {
        let _ = slot.set((*tool, routed_args.clone()));
    }
    let (name, args): (&str, &serde_json::Value) = match &routed {
        Some((tool, routed_args)) => (*tool, routed_args),
        None => (name, args),
    };

    // Narrow accident guard for explicit Git administration and unverified
    // verbs. Filesystem write authority and script execution remain unchanged.
    if adopted.is_some() && worktree::administration::refuses(name, args) {
        return host_return(executed((
            worktree::administration::NOTICE.into(),
            crate::ExecOutcome::Denied,
        )));
    }

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
        "re_read" => match smart_harness {
            Some(harness) => match harness.read(args) {
                Ok(text) => { invocation.expect("smart dispatch has a witness").retrieval(); text }
                Err(error) => {
                    invocation.expect("smart dispatch has a witness").host();
                    executed((format!("Error: re_read refused: {error}"), crate::ExecOutcome::Denied))
                }
            },
            None => executed((
                "Error: re_read is unavailable outside a smart harness session".to_string(),
                crate::ExecOutcome::Unavailable,
            )),
        },
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
            navigation::execute(name, args, workspace, caveats, &ctx)
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
                let before = ledger.snapshot();
                let mut out =
                    super::scheduled::execute_update_plan(args, ledger, color, tool_output_lines);
                if tool_result_ok(&out) {
                    let after = ledger.snapshot();
                    // Plan-before-act: a fresh multi-step plan in an ACTING
                    // turn, under a level that looks before it acts, is
                    // presented for approval before any mutation. Only Act:
                    // an evidence turn (Explain, Research) may plan its
                    // reading, and approval would seed an implementing turn
                    // the operator never asked for. Entering Plan can only
                    // attenuate a turn already validated for Act
                    // (plan_mode.rs); the request mints nothing, and the
                    // turn-end hook's approval restores only what the turn
                    // already held. A Plan-phase turn is already headed for
                    // that hook, and the implementing turn an approval seeded
                    // carries the operator's decision: neither is asked twice.
                    if super::scheduled::fresh_multi_step_plan(&before, &after)
                        && disposition == PromptDisposition::Act
                        && crate::initiative::effective_initiative().asks_before_acting()
                    {
                        if let Some(control) = plan_mode_control
                            .filter(|control| !control.implementing_approved_plan())
                        {
                            match control
                                .set_plan_mode(true)
                                .and_then(|()| control.request_exit())
                            {
                                Ok(()) => {
                                    out.push_str("\n\n");
                                    out.push_str(PLAN_APPROVAL_REQUESTED);
                                }
                                Err(error) => {
                                    out.push_str(&format!(
                                        "\nwarning: plan approval unavailable: {error}"
                                    ));
                                }
                            }
                        }
                    }
                    let mut artifact_warning = false;
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
                                artifact_warning = true;
                            }
                        }
                    }
                    // The operator sees a tick as one row; a new or rewritten
                    // plan keeps the full block the model receives, and so
                    // does a tick whose artifact record failed, so that
                    // warning is never hidden behind the row.
                    if !artifact_warning {
                        if let Some(line) = super::scheduled::step_change_line(&before, &after) {
                            presentation.override_result(line);
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
                Ok(()) => "entered PLAN MODE (read-only): subsequent tool calls are immediately limited to Plan reads and the plan ledger until you call exit_plan_mode. Read/search the relevant code, draft the ordered steps with update_plan, then exit_plan_mode to request approval — the operator decides whether execution proceeds.".to_string(),
                Err(error) => format!("error: enter_plan_mode: {error}"),
            },
            _ => {
                "unknown tool: enter_plan_mode (scheduled planning and a session Plan-mode control are both required)".to_string()
            }
        },
        // #2424: this no longer lifts the clamp itself — that was the second
        // root cause the design fixes (the model exiting its own clamp with
        // no human in the loop). It only RECORDS the request; the turn-end
        // approval hook decides whether `set_plan_mode(false)` actually runs.
        "exit_plan_mode" => match plan_mode_control {
            Some(control) => match control.request_exit() {
                Ok(()) => EXIT_PLAN_MODE_REQUESTED.to_string(),
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
        //
        // #2628/#2636: on approval, if a pending_rerun slot carries a denial
        // this exact approval covers, re-run it immediately with the widened
        // one-shot caveats so the model receives the result directly instead
        // of a "Retry the original operation now" instruction.
        //
        // Binding (round1 finding 1): replay proceeds ONLY when the grant
        // just obtained covers EVERY request the command was denied on
        // (`pending.missing`, FS or — #2681 — exec) — an approval for a
        // different capability, target, or axis is not consent to replay
        // this invocation, and the model's own retry path (a fresh
        // `run_command` call) is unaffected either way.
        "request_permissions" => {
            let rerun = pending_rerun.and_then(|slot| slot.take());
            let (granted, replay_auth, msg) = execute_request_permissions(
                caveats,
                args,
                permission_gate,
                color,
                tool_output_lines,
                workspace,
                rerun.as_ref(),
            );
            // #2636 round5: `replay_auth` is minted only when the RETURNED
            // `widened` caveats actually cover every entry in `pending.missing`
            // (`EligibleReplay::new`, tools.rs) — pre-prompt eligibility alone
            // is not enough. This match re-checks the same coverage
            // independently as a caller-side belt-and-suspenders, not because
            // the token could otherwise be forged: an ineligible or
            // insufficient approval never reaches `Some(_auth)` in the first
            // place. #2681: an exec entry is checked against the SAME minted
            // caveats via `permits_exec` (which is exactly what the confined
            // shell's own enforcement checks), not `permits_filesystem_request`
            // (which returns `false` for every non-FS kind by design).
            match (granted, replay_auth, rerun) {
                (Some(widened), Some(_auth), Some(pending))
                    if pending.missing.iter().all(|request| match request.kind {
                        DenialKind::Exec => widened.permits_exec(&request.target),
                        _ => permits_filesystem_request(&widened, request),
                    }) =>
                {
                    executed(
                        exec_confined_command(
                            &pending.cmd,
                            &pending.cwd,
                            workspace,
                            color,
                            tool_output_lines,
                            &widened,
                            &pending.declared,
                            exec_floor,
                            &mut None, // one-shot: no further interactive prompting
                            tool_offload,
                            spill_store,
                            live_tool_output.clone(),
                            presentation,
                            command_budget,
                        )
                        .await,
                    )
                }
                // No pending rerun, denied/headless, ineligible approval, or
                // the grant just given does not cover what this specific denial
                // needed: fall back to the grant/denial message.
                (_, _, _) => msg,
            }
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
            // An exact known name is an explicit schema request through an
            // already-exposed tool. Reuse only the controller's admitted
            // definitions; fuzzy search never activates or executes anything.
            if let Some(message) = hidden_tools
                .filter(|_| tool_allowed(disposition, query.trim()))
                .and_then(|hidden| hidden.promote(query.trim()).message(query.trim()))
            {
                return host_return(message);
            }
            // FR-1 part 2 (#997): search only what THIS persona may call, so
            // discovery never surfaces a tool the executor would then refuse.
            let catalog = filter_advertised_tools(
                merged_tool_definitions(
                    &*mcp,
                    note_sink.is_some(),
                    recall_source.is_some(),
                    memory_source.is_some(),
                    None, // Native Git is the default; the legacy adapter is internal.
                    crew_runner.is_some(),
                    scratchpad_store.is_some(),
                    code_search.is_some(),
                    experience_store.is_some(),
                    step_ledger.is_some(),
                    operating_mode_control.is_some(),
                    plan_mode_control.is_some(),
                    plan_mode_control.is_some_and(super::PlanModeControl::is_plan_mode),
                    command_budget,
                    shell_dialect,
                ),
                persona_tools,
            );
            // #2332: disposition narrowing happens inside the search, which
            // names what it hides instead of dropping it.
            super::tool_search::execute_tool_search_for_disposition(
                query,
                &catalog,
                disposition,
                hidden_tools,
            )
        }

        // Embedded git (PR4, #461): dispatch through the injected GitTool
        // (newt-git's LocalGitTool). `GitCaveats::from_session` projects the
        // session's authority onto the git surface (fail-closed: a read-only
        // session can read but not commit). Same presence-gating as `recall` —
        // without an injected impl the tool was never advertised.
        "git" => match git_tool {
            Some(tool) => {
                let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
                // This engine-read boundary is independent of disposition and
                // Git write grants. Refuse before any confirmation or retry.
                if let Err(error) = super::git_tool::check_git_read_scope(op, &caveats.fs_read) {
                    return host_return(format!("error: {error}"));
                }
                let gc = crate::git_caveats::GitCaveats::from_session(caveats);
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
                        return host_return(refusal);
                    }
                }
                let mut out = match tool.dispatch(op, args, &gc, caveats) {
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
                        out = match tool.dispatch(op, args, &crate::git_caveats::GitCaveats::top(), caveats)
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
        "crew" if smart_harness.is_some() =>
            {
                invocation.expect("smart dispatch has a witness").host();
                executed((
                    "Error: frame isolation: crew execution is unavailable until its file operations enforce the session filesystem boundary; use the confined local tools".into(),
                    crate::ExecOutcome::Unavailable,
                ))
            },
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
                return host_return(format!(
                    "note: a bare `cd` has no effect — each command runs \
                     independently, so there is no persistent shell to change. \
                     Prefix the command instead (`cd {path} && <command>`, which \
                     newt runs in `{path}`) or pass `cwd`."
                ));
            }

            // Corrective guard: the model tried to call a tool as a shell binary.
            // Return a correction so the model can retry with the right tool call.
            if let Some(tool) = run_command_redirect(cmd)
                .filter(|tool| smart_harness.is_none() || !matches!(*tool, "find" | "grep"))
            {
                return host_return(format!(
                    "error: '{tool}' is a tool, not a shell command. \
                     Call it as a separate tool invocation — \
                     do not pass '{tool}' as a command argument to run_command."
                ));
            }

            // Native commit runs the original shell source once, with Git's
            // expanded command bound to host-held attribution/signing policy.
            // Other commit-producing verbs still need multi-commit lifecycle
            // support; they keep the explicit guard below.
            // Match the ordinary host route: an explicit preset floor still wins.
            let publication_bypass = ocap_disabled() && shell::exec_floor_permits(exec_floor, cmd);
            let governed_git = native_git::publication::governed(cmd, publication_bypass, presentation);
            let commit_requested = governed_git && native_git::needs_commit_broker(cmd);
            let commit_broker = if commit_requested {
                match git_tool.and_then(|tool| tool.native_commit_policy()) {
                    Some(policy) => match crate::native_git_broker::NativeGitBroker::new(policy) {
                        Ok(broker) => Some(broker as std::sync::Arc<dyn agent_bridle_tool_shell::CommandBroker>),
                        Err(error) => return host_return(format!("error: native Git commit broker: {error}")),
                    },
                    None => None,
                }
            } else {
                None
            };
            if governed_git && run_command_creates_shell_git_commit(cmd) && commit_broker.is_none() {
                let reason = match agent_bridle::inspect_shell(cmd) {
                    Err(error) => format!("shell inspection failed: {error}"),
                    Ok(_) if commit_requested => "native commit attribution/signing policy is unavailable in this session".to_owned(),
                    Ok(_) => "this commit-producing Git operation is not supported by the native broker".to_owned(),
                };
                return host_return(format!(
                    "error: refusing Git commit publication for this invocation: {reason}. \
                     Supported `git commit` commands use harness-managed attribution and signing; \
                     other commit-producing operations (`merge`, `cherry-pick`, `revert`, `rebase`) \
                     still require native lifecycle support. No command ran."
                ));
            }

            // Route the WHOLE command through agent-bridle's confined shell
            // (free-form `cmd` mode) under the SAME Caveats the TUI resolved from
            // `[tui].permissions`. The confined-exec core is shared with the
            // `lifecycle` arm (#891) so both honor identical exec caveats. A
            // folded leading `cd` becomes the cwd (it wins over an explicit
            // `cwd` arg — it's the more specific, in-command intent).
            let command_cwd = resolve_exec_cwd(workspace, args["cwd"].as_str());
            let run_cwd = resolve_exec_cwd(&command_cwd, cd_path.as_deref());
            if let Err((error, outcome)) = shell::existing_exec_cwd(std::path::Path::new(&run_cwd)) {
                let task_root = worktree_session.and_then(|session| session.task_root(std::path::Path::new(workspace)));
                let previous = task_root.as_deref().or(default_command_cwd).unwrap_or_else(|| std::path::Path::new(workspace));
                return host_return(executed((
                    format!("{error}; default working directory remains {}", crate::worktree_adoption::task_path_literal(previous)),
                    outcome,
                )));
            }
            if let Some(slot) = command_directory {
                let _ = slot.set(std::path::PathBuf::from(&run_cwd));
            }
            // issue-1188: `git push` / `gh pr create` cannot reach the forge
            // from the confined child at all (#2619 narrows any host-scoped
            // net grant to `net: none` for spawned children on Linux — the
            // kernel fence cannot bound hosts). A recognized push/PR-create
            // invocation is diverted here to run HOST-SIDE, entirely outside
            // the confined shell, under a fixed argv the broker itself
            // constructs — never falling through to the confined executor,
            // where it would either hang against `net: none` or (if that
            // narrowing ever regressed) inherit the session's full net scope
            // inside a hostile-repo-influenced child.
            if governed_git && native_git::needs_push_broker(cmd) {
                return host_return(native_git::execute_governed_push(
                    cmd,
                    std::path::Path::new(&run_cwd),
                    caveats,
                    &mut permission_gate,
                    governed_pr,
                ));
            }
            if governed_git && native_git::needs_pr_create_broker(cmd) {
                return host_return(native_git::execute_governed_pr_create(
                    cmd,
                    std::path::Path::new(&run_cwd),
                    caveats,
                    &mut permission_gate,
                    governed_pr,
                ));
            }
            if governed_git {
                if let Err(reason) = native_git::preflight(
                    cmd,
                    std::path::Path::new(&run_cwd),
                    caveats,
                    &mut permission_gate,
                    commit_broker.is_some(),
                ) {
                    return host_return(reason);
                }
            }
            let filesystem_requests = match declared_filesystem_requests(args, cmd, &run_cwd) {
                Ok(requests) => requests,
                Err(error) => return host_return(error),
            };
            if let Some(policy) = &adopted {
                if let Some(result) = worktree::branch::execute(policy, cmd, std::path::Path::new(&run_cwd), caveats, &filesystem_requests) {
                    if let Some(invocation) = invocation { invocation.host(); }
                    return executed(result);
                }
            }
            if let Some(program) = build_shell::confined_build_program(cmd, ocap_disabled()) {
                // #2636 (round1 finding 3): a build denial is NEVER eligible
                // for #2628 replay. `build_shell::execute` enforces the
                // calibrated build fence and explicitly forbids re-running
                // after a denial (earlier stages may already have side
                // effects) — routing it back through plain `exec_confined_command`
                // on approval would drop that fence and could repeat a write
                // that already happened. `pending_rerun` is left untouched
                // here (already cleared above), so no build denial can ever
                // populate it.
                return executed(
                    build_shell::execute(
                        cmd,
                        &program,
                        &run_cwd,
                        build_workspace,
                        caveats,
                        &filesystem_requests,
                        &mut permission_gate,
                        smart_harness,
                        tool_output_lines,
                        color,
                        tool_offload,
                        spill_store,
                        live_tool_output.clone(),
                        presentation,
                        commit_broker,
                        args.get("timeout_secs").and_then(serde_json::Value::as_u64),
                        command_budget,
                    )
                    .await,
                );
            }
            // F32/#2537 round 3: a bare `git …` in this session's own repo, on
            // a non-default branch, gets kernel WRITE on its own gitdir +
            // `objects/` for THIS dispatch only — see
            // `dispatch_caveats_for_git_shell`'s doc comment. #2682 round 2
            // (#2686 review round 2): the common dir's `refs/heads/`/
            // `logs/refs/heads/` are NEVER granted here anymore — a
            // commit-creating command instead runs with HEAD DETACHED (so it
            // only ever touches `objects/` + this worktree's own admin dir,
            // both still granted below) and a host-side, bounded
            // `update-ref` publishes the result afterward. See
            // `git_hardening::own_branch_for_commit_ref_move`'s doc comment
            // for the full mechanism and why the old directory-wide grant
            // was a hole, not an accepted trade-off.
            // #2720: full access authorizes the operation, while the broker's
            // child must still be unable to overwrite its signing mechanism.
            let adopted_git_workspace = adopted.as_ref().filter(|policy| {
                std::path::Path::new(&run_cwd).canonicalize().ok()
                    .is_some_and(|cwd| cwd.starts_with(&policy.worktree))
            });
            let commit_workspace = if let Some(policy) = adopted_git_workspace {
                policy.worktree.to_str().unwrap_or(workspace)
            } else if commit_broker.is_some()
                && matches!(caveats.fs_write, crate::Scope::All)
            {
                run_cwd.as_str()
            } else {
                workspace
            };
            let invocation_caveats = if commit_broker.is_some() {
                match crate::native_git_broker::NativeGitBroker::invocation_caveats(
                    caveats, std::path::Path::new(commit_workspace),
                ) {
                    Ok(invocation) => invocation,
                    Err(error) => return host_return(format!("error: native Git commit broker: {error}")),
                }
            } else {
                caveats.clone()
            };
            let git_shell_caveats = dispatch_caveats_for_git_shell(cmd, commit_workspace, &invocation_caveats);
            let commit_broker_used = commit_broker.is_some();
            let ref_move = commit_requested
                .then(|| {
                    crate::git_hardening::own_branch_for_commit_ref_move(std::path::Path::new(
                        commit_workspace,
                    ))
                })
                .flatten();
            // Bind identity before dispatch. Recovery shares the execution
            // owner's lease, so dropping this async waiter does not reattach
            // while that owner's shutdown path is still running. Recovery
            // never publishes; escaped descendants remain a documented residual.
            let mut detach_guard = None;
            if let Some(move_info) = &ref_move {
                if adopted.as_ref().is_some_and(|policy| policy.protects_branch(&move_info.branch)) {
                    if let Some(invocation) = invocation { invocation.host(); }
                    return executed(("capability denied: the original checkout's branch remains read-only".into(), crate::ExecOutcome::Denied));
                }
                if let Err(error) =
                    crate::git_hardening::detach_own_head(&move_info.identity, &move_info.old_tip)
                {
                    return host_return(format!(
                        "error: could not prepare branch '{}' for a governed commit: {error}",
                        move_info.branch
                    ));
                }
                detach_guard = Some(std::sync::Arc::new(
                    crate::git_hardening::DetachedHeadGuard::new(
                        move_info.identity.clone(),
                        move_info.branch.clone(),
                        move_info.old_tip.clone(),
                    ),
                ));
            }
            let mut fs_pre_exec_missing: Option<Vec<PermissionRequest>> = None;
            let (mut exec_text, mut exec_outcome) = shell::exec_confined_command_with_broker(
                cmd,
                &run_cwd,
                workspace,
                color,
                tool_output_lines,
                &git_shell_caveats,
                &filesystem_requests,
                exec_floor,
                &mut permission_gate,
                tool_offload,
                spill_store,
                live_tool_output.clone(),
                presentation,
                commit_broker,
                detach_guard
                    .as_ref()
                    .map(|guard| guard.clone() as agent_bridle_tool_shell::ExecutionLease),
                &mut fs_pre_exec_missing,
                retry_authorized,
                args.get("timeout_secs").and_then(serde_json::Value::as_u64),
                adopted.as_ref(),
                command_budget,
            )
            .await;
            if let Some(move_info) = &ref_move {
                let branch = &move_info.branch;
                // Only once the confined child actually exited 0 — a denial
                // or a failing hook leaves the checked-out branch untouched,
                // same as before this command ran.
                if exec_outcome == crate::ExecOutcome::Passed {
                    match crate::git_hardening::advance_own_branch_ref(
                        &move_info.identity,
                        branch,
                        &move_info.old_tip,
                    ) {
                        Ok(_new_oid) => {
                            exec_text = exec_text.replace("detached HEAD ", &format!("{branch} "));
                        }
                        Err(refusal) => {
                            // #2686 review round 3, P2: a refused publication
                            // is a FAILURE of this tool call, not a passed
                            // one with different text — the model's requested
                            // branch update did not happen.
                            exec_outcome = crate::ExecOutcome::Failed;
                            let candidate = refusal
                                .candidate_oid
                                .as_deref()
                                .map(|oid| format!(" The commit exists, unpublished, as {oid}."))
                                .unwrap_or_default();
                            exec_text = format!(
                                "{exec_text}\nerror: the commit was created but refused publication to \
                                 branch '{branch}': {refusal}.{candidate} No branch ref moved; \
                                 retry the commit."
                            );
                        }
                    }
                } else {
                    // #2686 review round 5, P2: `detached_commit_candidate`
                    // now distinguishes "verified: nothing to report" from
                    // "could not even check" — surface the latter instead of
                    // silently treating it as the former.
                    match crate::git_hardening::detached_commit_candidate(
                        &move_info.identity,
                        &move_info.old_tip,
                    ) {
                        Ok(Some(candidate)) => {
                            // The dispatch itself did not exit 0 (e.g. a
                            // compound `git commit … && false`) — never
                            // publish behind a reported failure, but don't
                            // lose the commit silently either: name its oid
                            // so it is recoverable.
                            exec_text = format!(
                                "{exec_text}\nnote: a commit was created on detached HEAD but \
                                 left unpublished because the dispatch did not succeed: \
                                 {candidate}. It is reachable only by this SHA until a retried \
                                 commit publishes it."
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            exec_text = format!(
                                "{exec_text}\nerror: could not check whether a commit landed \
                                 on detached HEAD before reattaching: {error}"
                            );
                        }
                    }
                }
                // Always reattach, success or failure alike — an unpaired
                // detach leaves the worktree stuck on a detached HEAD for
                // every subsequent git command in this session.
                if let Err(error) =
                    crate::git_hardening::reattach_own_head(&move_info.identity, branch)
                {
                    exec_outcome = crate::ExecOutcome::Failed;
                    exec_text = format!(
                        "{exec_text}\nerror: could not restore branch '{branch}' as HEAD: {error}"
                    );
                }
                if let Some(guard) = detach_guard.as_mut() {
                    guard.resolve();
                }
            }
            let result = executed((exec_text, exec_outcome));
            // #2636 finding 1: use the typed out-param from the pre-exec denial
            // path — only a denial that fired BEFORE the child ran populates
            // fs_pre_exec_missing. Child stdout that happens to contain the
            // denial string does not. A native-Git commit-producing command is
            // excluded (commit_broker_used) to prevent replaying under a bypass
            // of the attribution/signing policy and the gitdir-write caveat.
            if let (Some(slot), Some(missing)) = (pending_rerun, fs_pre_exec_missing) {
                if !commit_broker_used {
                    *slot = Some(PendingRerun {
                        cmd: cmd.to_string(),
                        cwd: run_cwd.clone(),
                        declared: filesystem_requests.clone(),
                        missing,
                    });
                }
            }
            result
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
            // #2424: under the Plan disposition, a report is a revisable
            // draft, not a display — see docs/design/plan-mode-draft-present-approve.md.
            // `render_report` replaces the session's one draft slot instead of
            // printing it; the latest revision is presented exactly once, at
            // the end of the planning turn, by the caller that owns that
            // boundary (not here — this arm never double-presents).
            let plan_draft_sink = (disposition == PromptDisposition::Plan)
                .then_some(())
                .and(plan_draft_sink);
            let (result, document) =
                execute_render_report(args, color, tool_evidence, plan_draft_sink);
            if let Some(document) = document {
                presentation.document(&document);
            }
            result
        }

        "lifecycle" => {
            let phase_key = args.get("phase").and_then(|v| v.as_str()).unwrap_or("");
            // A model that learned `lifecycle` as a task-state reporter in
            // another harness sends `{event|status|state, message}` and no
            // phase. Listing build phases at it is a dead end; name the tools
            // that own that vocabulary instead (same coaching posture as
            // `catalog::resolve_tool_alias`).
            if phase_key.is_empty()
                && ["event", "status", "state"]
                    .iter()
                    .any(|k| args.get(k).is_some())
            {
                return "error: lifecycle runs a project build phase (setup, format, \
                        lint, test, check, clean); it does not record task state. To \
                        report progress, call update_plan with \
                        {\"plan\":[{\"step\",\"status\"}]}. If you are blocked on the \
                        operator, call request_user_input (or request_permissions for \
                        a grant). When the work is done, reply without a tool call — \
                        that final answer ends the turn."
                    .to_string();
            }
            let Some(phase) = crate::tooling::Phase::from_key(phase_key) else {
                let valid = crate::tooling::Phase::ALL
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                // F12 (verify-lane-steering round 2): `build` is an action,
                // not a phase — a model that learned "use lifecycle
                // action=build" often sends `phase="build"` instead. Point
                // it at the real call rather than leaving it in the
                // valid-phases dead end.
                if phase_key == "build" {
                    let suggestion = build_call_suggestion(args, "test");
                    return format!(
                        "error: unknown lifecycle phase '{phase_key}'. Valid phases: {valid}.\n\
                         Suggested lifecycle call: {suggestion}"
                    );
                }
                return format!(
                    "error: unknown lifecycle phase '{phase_key}'. Valid phases: {valid}."
                );
            };
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("run");
            // #1972: `dir` resolves through the SAME seam run_command's `cwd`
            // uses — detection AND execution both target it, not just the
            // workspace root, so a nested project isn't structurally
            // unreachable in a polyglot/monorepo workspace.
            let effective_dir = resolve_exec_cwd(workspace, args.get("dir").and_then(|v| v.as_str()));
            let effective_path = std::path::Path::new(&effective_dir);
            // Resolve from the `[lifecycle]` override → matching tooling packs.
            // Several toolchains may each contribute a command; join with `&&`
            // so the confined shell runs them in sequence and short-circuits on
            // the first failure (the safe-subset engine supports `&&`).
            let cmds = crate::tooling::resolved_phase_commands(effective_path, phase);
            if cmds.is_empty() {
                // #1972: root(-or-`dir`)-level detection found nothing — before
                // giving up, name any first-level subdirectory that DOES have
                // this phase configured, so a nested project (e.g.
                // `agent-voice/Cargo.toml`) is not silently invisible. The
                // message keeps the same "no command configured" lead either
                // way — see `tool_result_ok`, which treats the whole family as
                // a no-op rather than a claimable success.
                let nested =
                    crate::tooling::nested_projects_with_configured_phase(effective_path, phase);
                return crate::tooling::unconfigured_phase_message(phase, &nested);
            }
            let joined = cmds.join(" && ");
            // Inspect exactly the retained source used by both execution lanes.
            // Never re-resolve mutable tooling configuration after this check.
            if action != "list"
                && adopted.is_some()
                && worktree::administration::destructive(&joined)
            {
                return host_return(executed((
                    worktree::administration::NOTICE.into(),
                    crate::ExecOutcome::Denied,
                )));
            }
            // F38: a `run` whose resolved command starts with a build tool
            // (cargo/just/make) cannot execute in the run lane (the tool is not
            // in its profile), so it takes the build lane, exactly as
            // `action=build` does.
            let action = if action == "run" && lifecycle_run_routes_to_build_lane(&joined) {
                "build"
            } else {
                action
            };
            match action {
                "list" => format!("lifecycle {} → {joined}", phase.as_str()),
                "build" => {
                    let (program, argv) = build_check_argv(&joined);
                    executed(
                        run_confined_build_lane(
                            build_workspace,
                            effective_path,
                            program,
                            argv,
                            &joined,
                            smart_harness,
                            caveats,
                            &mut permission_gate,
                            tool_output_lines,
                            color,
                            tool_offload,
                            spill_store,
                            None,
                            None,
                            shell::LIFECYCLE_BUILD_TIMEOUT,
                            presentation,
                        )
                        .await,
                    )
                }
                "run" => executed(
                    lifecycle_run_with_escalation(
                        args,
                        &joined,
                        &effective_dir,
                        effective_path,
                        workspace,
                        color,
                        tool_output_lines,
                        caveats,
                        exec_floor,
                        permission_gate,
                        tool_offload,
                        spill_store,
                        live_tool_output.clone(),
                        smart_harness,
                        presentation,
                        command_budget,
                    )
                    .await,
                ),
                other => format!(
                    "error: unknown lifecycle action '{other}'. Use 'run' (default), 'list', or 'build'."
                ),
            }
        }

        // facade P4 build route (§4, `super::routing::build_lane_route`):
        // reached ONLY via a routed `run_command`, never advertised or
        // callable directly — a recognised `cargo build|check|test|clippy` /
        // `just <recipe>` argv runs through the SAME confined build lane
        // (`run_confined_build_lane`) `lifecycle action=build` uses, verbatim
        // — never a re-resolved phase command, so no operand is ever
        // dropped (the operand-dropping class #2482 fixed for git routes).
        "build_exec" => {
            let argv: Vec<String> = args
                .get("argv")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|tok| tok.as_str().map(str::to_string))
                .collect();
            let Some((program, rest)) = argv.split_first() else {
                return host_return("error: routed build command had no argv".into());
            };
            // PR1: `cwd` is the workspace-relative directory a leading `cd`
            // or a `cwd` field resolved (`routing::resolve_workspace_relative_dir`
            // already proved it canonicalizes to a real directory inside the
            // workspace AND this call's fs-read fence — `.` for the
            // workspace root itself). Absent when the model sent a bare
            // command with no `cd`/`cwd` at all.
            let cwd_field = args.get("cwd").and_then(serde_json::Value::as_str);
            let effective_dir = match cwd_field {
                Some(cwd) => std::path::Path::new(workspace).join(cwd),
                None => std::path::Path::new(workspace).to_path_buf(),
            };
            // `just` only makes sense with a justfile somewhere — the pure
            // router cannot see the filesystem, so the check lives here.
            // `just` itself searches the cwd AND every parent directory
            // (that's how a subcrate's `just check` finds the workspace
            // root's justfile), so checking only `effective_dir` errors on a
            // repo whose justfile sits one level up, where the shell path
            // would have found and run it. #2533 round 2: fall back to the
            // normal exec path instead — the routed argv is the literal
            // command, so it runs exactly as the shell would have, in the
            // SAME resolved directory the route folded.
            let display = argv.join(" ");
            if program == "just" && !justfile_findable_from(&effective_dir) {
                let ran_in = effective_dir.to_string_lossy().into_owned();
                let filesystem_requests =
                    match declared_filesystem_requests(args, &display, &ran_in) {
                        Ok(requests) => requests,
                        Err(error) => return host_return(error),
                    };
                return executed(
                    exec_confined_command(
                        &display,
                        &ran_in,
                        workspace,
                        color,
                        tool_output_lines,
                        caveats,
                        &filesystem_requests,
                        exec_floor,
                        &mut permission_gate,
                        tool_offload,
                        spill_store,
                        live_tool_output.clone(),
                        presentation,
                        command_budget,
                    )
                    .await,
                );
            }
            // F23 / #2524 "tail-pipe-routes": a `| tail -N` / `| head -N`
            // suffix the model piped on for readability, not semantics
            // (`build_piped_to_trim_route`) — the build's own exit code
            // still decides `outcome`; only the rendered text is cut.
            let trim = OutputTrim::from_json(args.get("trim"));
            // F28 round 2 (PR-F28 review, Blocker 2): a routed `timeout N …`
            // wrapper carries its parsed `N` here (`routing::strip_leading_timeout`),
            // in place of the round-1 `timeout_dropped` flag that just threw
            // it away. `0` is GNU `timeout`'s own "no timeout" — treated as
            // "use the lane's own wall", never as an instant timeout.
            let timeout_secs = args.get("timeout_secs").and_then(serde_json::Value::as_u64);
            let wall = match timeout_secs {
                Some(0) | None => shell::LIFECYCLE_BUILD_TIMEOUT,
                Some(secs) => std::cmp::min(
                    std::time::Duration::from_secs(secs),
                    shell::LIFECYCLE_BUILD_TIMEOUT,
                ),
            };
            let (text, outcome) = run_confined_build_lane(
                build_workspace,
                &effective_dir,
                program,
                rest.to_vec(),
                &display,
                smart_harness,
                caveats,
                &mut permission_gate,
                tool_output_lines,
                color,
                tool_offload,
                spill_store,
                None,
                trim,
                wall,
                presentation,
            )
            .await;
            // PR1: say what a `cd`/`cwd` actually folded into, rather than
            // #2550's root-only "dropped as a no-op" wording — a reader
            // needs to know WHICH directory the build lane actually ran in.
            let cd_clause = match cwd_field {
                Some(".") | None => String::new(),
                Some(cwd) => format!("; the leading `cd`/`cwd` was folded into cwd={cwd}"),
            };
            // F27 / #2554 round 2: say when a trailing exit-code `echo` was
            // dropped (`routing::mark_echo_dropped`) — the model asked for
            // an `EXIT: N` line and will not find one; without this clause
            // it looks like the request was silently ignored rather than
            // answered a different way, which is exactly the kind of gap
            // that drives a re-run (the no-progress spiral this line of
            // work exists to end).
            let echo_clause = if args.get("echo_dropped").and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                "; a trailing exit-code `echo` was dropped — this lane's own exit code, above, \
                 is the real one"
            } else {
                ""
            };
            // F28 round 2 (PR-F28 review, Blocker 2): say WHICH wall a
            // leading `timeout` wrapper actually left in effect — round 1's
            // "the lane's own 30 min limit already applies" was false
            // whenever the model's own N was shorter than the lane wall
            // (`timeout 60 cargo test`, a hang guard, used to silently wait
            // up to 30 min). `wall` above is already `min(N, lane wall)`.
            let timeout_clause = match timeout_secs {
                None | Some(0) => String::new(),
                Some(secs) => {
                    if wall.as_secs() == secs {
                        format!(
                            "; `timeout {secs}` honoured as this lane's wall ({})",
                            format_wall(wall)
                        )
                    } else {
                        format!(
                            "; `timeout {secs}` was capped at the lane's {} limit",
                            format_wall(wall)
                        )
                    }
                }
            };
            let wall_desc = format_wall(wall);
            let network = build_network_description(&caveats.net);
            let note = match trim {
                Some(trim) => format!(
                    "[routed `{display}` to the confined build lane — {wall_desc} limit, \
                     {network}; Cargo offline; {}{cd_clause}{echo_clause}{timeout_clause}]",
                    trim.note_clause()
                ),
                None => format!(
                    "[routed `{display}` to the confined build lane — {wall_desc} limit, \
                     {network}; Cargo offline{cd_clause}{echo_clause}{timeout_clause}]"
                ),
            };
            executed((append_routed_note(text, note), outcome))
        }

        // A memory address is not a path. A spill teaser names `memory_fetch`,
        // but weak models reach for `read_file` with the `spill:<cid>` handle;
        // treating it as a filename answered "No such file or directory" — a
        // lie that looped a live session four times (2026-09-23). Serve it
        // through the one memory resolver instead, with read_file's paging.
        "read_file" if super::memory_fetch::is_memory_address(args["path"].as_str().unwrap_or("")) => {
            let address = args["path"].as_str().unwrap_or("").trim();
            match memory_source {
                Some(source) => match super::memory_fetch::resolve_memory_address(address, source) {
                    Ok(body) => match (arg_usize(args, "offset"), arg_usize(args, "limit"),
                        arg_usize(args, "char_offset"))
                    {
                        (Ok(offset), Ok(limit), Ok(char_offset)) => paginate_unspillable(
                            address,
                            &body,
                            offset,
                            limit,
                            char_offset,
                        ),
                        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => format!("error: read_file: {e}"),
                    },
                    Err(refusal) => refusal,
                },
                None => format!(
                    "`{address}` is a memory address, not a file, and this session has \
                     no memory store to read it from."
                ),
            }
        }

        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");
            match authorized_read("read_file", path, workspace, caveats, permission_gate) {
                Ok(contents) => {
                    // #719: window + cap the MODEL-facing payload (the on-screen
                    // display is capped separately) so one read of a large file
                    // can't saturate the context window and abandon the task.
                    let offset = match arg_usize(args, "offset") {
                        Ok(v) => v,
                        Err(e) => return format!("error: read_file: {e}"),
                    };
                    let limit = match arg_usize(args, "limit") {
                        Ok(v) => v,
                        Err(e) => return format!("error: read_file: {e}"),
                    };
                    // #726: char backstop now derives from the shared token
                    // budget so read_file and run_command share one cap —
                    // held under the spill cap when offload is on, so a big
                    // file pages with `offset=` instead of becoming a handle.
                    let char_offset = match arg_usize(args, "char_offset") {
                        Ok(v) => v,
                        Err(e) => return format!("error: read_file: {e}"),
                    };
                    match read_history {
                        Some(history) => history.read(path, &contents, offset, limit, char_offset, tool_offload),
                        None => read_file_page(path, &contents, offset, limit, char_offset, tool_offload),
                    }
                }
                Err(tool_output) => tool_output,
            }
        }

        "write_file" => {
            let path = args["path"].as_str().unwrap_or("");
            // Validate before the gate or the file, as edit_file does: a write
            // that dropped `content` used to empty its target and report
            // success (the shrink guard misses files under 30 lines).
            let Some(content) = args.get("content").and_then(serde_json::Value::as_str) else {
                return "error: write_file needs `content` as a string — nothing was written"
                    .to_string();
            };
            if path.is_empty() {
                return "error: write_file needs `path` — nothing was written".to_string();
            }
            if args.get("move_from").is_some() {
                #[cfg(not(feature = "ast"))]
                return "error: move_from requires a build with the ast feature; nothing changed".into();
                #[cfg(feature = "ast")]
                {
                    let moved = match move_from::execute(args, workspace, caveats, smart_harness, &mut permission_gate) {
                        Ok(moved) => moved,
                        Err(error) => return format!("error: move_from: {error}"),
                    };
                    let mut artifacts = super::publish_early::bounded_move_hint(moved.child.lines().count()).to_owned();
                    for (label, before, after) in [
                        (moved.source.as_str(), super::artifact_hooks::ArtifactFileState::from_bytes(moved.before.as_bytes()), moved.parent.as_str()),
                        (path, super::artifact_hooks::ArtifactFileState::absent(), moved.child.as_str()),
                    ] {
                        artifacts.push_str(&record_governed_file_change(artifact_sink, artifact_context, label, "write_file",
                            Some(before), super::artifact_hooks::ArtifactFileState::from_bytes(after.as_bytes()), color, tool_output_lines));
                    }
                    return format!("Moved functions from {} into {}; confined cargo check --lib exited 0. Parent: {} lines; child: {} lines. Displaced entries retained as .newt-move-*.saved beside changed files; inspect before removing.{}",
                        file_capture::display_text(&moved.source), file_capture::display_text(path),
                        moved.parent.lines().count(), moved.child.lines().count(), artifacts);
                }
            }
            // Move code without retyping it: `copy_from` appends an exact line
            // range of another file after `content` (a typed header). The
            // source is read through read_file's own fence. Live 2026-09-23 a
            // retyped 1,263-line move fabricated symbols; a shell `sed` copy
            // failed on BSD-vs-GNU syntax and left `mod.rs-e` behind.
            let copied;
            let content = match args.get("copy_from") {
                Some(spec) => {
                    let (Some(source), Some(start), Some(end)) = (
                        spec["path"].as_str(),
                        spec["start_line"].as_u64(),
                        spec["end_line"].as_u64(),
                    ) else {
                        return "error: copy_from needs path, start_line and end_line".to_string();
                    };
                    let text = match authorized_read(
                        "write_file",
                        source,
                        workspace,
                        caveats,
                        permission_gate.as_deref_mut(),
                    ) {
                        Ok(text) => text,
                        Err(refusal) => return refusal,
                    };
                    copied = match file_capture::copy_line_range(
                        content,
                        &text,
                        start as usize,
                        end as usize,
                    ) {
                        Ok(text) => text,
                        Err(refusal) => return refusal,
                    };
                    copied.as_str()
                }
                None => content,
            };
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
                    return denied_fs_result("fs_write", &full_str);
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

            let mutation_scope = if scope_permits { &caveats.fs_write } else { &crate::caveats::Scope::All };
            if !file_capture::regular_target(&full) {
                return file_capture::present(
                    format!("error: write_file refuses a nonregular target: {path}"),
                    presentation,
                );
            }

            // Shrink guard: refuse if the proposed write removes > 30% of
            // lines AND > 30 lines absolute. This catches the failure mode
            // where a model replaces an entire large file with a small
            // fragment (observed in the wild: 4,247 → 107 lines).
            if let Ok(existing) = file_capture::read_for_edit(mutation_scope, &full, path) {
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
            let visible_preview = file_capture::display_text(&preview);
            let preview = if visible_preview != preview {
                format!("Preview escapes control characters:\n{visible_preview}")
            } else {
                preview
            };
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
                let receipt_before = file_capture::capture(&caveats.fs_read, &full);
                let artifact_before = artifact_path_within.then(|| {
                    artifact_preimage_state(&caveats.fs_read, &full, tui_permits_path(&caveats.fs_read, &full_str))
                });
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
                let receipt_after = file_capture::capture(&caveats.fs_read, &full);
                let receipt = file_capture::receipt(path, &receipt_before, &receipt_after);
                match write_result {
                    Ok(()) => {
                        if !file_capture::verified_after(&receipt_after, mutation_scope, &full, Some(content.as_bytes())) {
                            return receipt.present(
                                format!("error: write_file returned success for {path}, but the submitted bytes could not be verified"),
                                "",
                                presentation,
                            );
                        }
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
                            match artifact_file_matches(&caveats.fs_write, &full, content.as_bytes()) {
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
                            .map(|cmd| run_build_check(cmd, build_workspace, &caveats.net))
                            .unwrap_or_default();
                        receipt.present_success(format!("wrote {path} ({line_count} lines)"), &format!("{artifact}{check}{}", super::publish_early::bounded_move_hint(line_count)), presentation)
                    }
                    Err(tool_output) => receipt.present(file_capture::failure(tool_output.render(workspace, &caveats.fs_read), ""), "", presentation),
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
                    return denied_fs_result("fs_write", &full_str);
                }
            }

            let meta = match std::fs::symlink_metadata(&full) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return FileIoError::io(&e, &full, format!("error: deleting {path}: file does not exist")).render(workspace, &caveats.fs_read);
                }
                Err(e) => return format!("error: deleting {path}: {e}"),
            };
            if meta.file_type().is_dir() {
                return format!("error: deleting {path}: delete_file refuses directories");
            }
            let artifact_tracking = artifact_sink.is_some() && artifact_context.is_some();
            let artifact_path_within = artifact_tracking
                && artifact_path_is_physically_within_workspace(
                    std::path::Path::new(workspace),
                    &full,
                );

            let confirmed = confirm_unrestricted_fs_mutation(
                caveats,
                &mut permission_gate,
                "Delete this file? [y/N]",
            );

            if !confirmed {
                return format!("user declined to delete {path}");
            }

            let receipt_before = file_capture::capture(&caveats.fs_read, &full);
            let artifact_before = artifact_path_within.then(|| {
                artifact_preimage_state(&caveats.fs_read, &full, tui_permits_path(&caveats.fs_read, &full_str))
            });
            let mutation_scope = if scope_permits { &caveats.fs_write } else { &crate::caveats::Scope::All };

            // step-52.6: object-bound removal when the scope authorised it (the
            // parent is resolved beneath the root and the entry unlinked via its
            // fd, so a symlink/`..`/absolute escape is refused by the kernel).
            let delete_result = if scope_permits {
                object_bound_delete(&caveats.fs_write, path, &full, &full_str)
            } else {
                std::fs::remove_file(&full).map_err(|e| FileIoError::io(&e, &full, format!("error: deleting {path}: {e}")))
            };
            let receipt_after = file_capture::capture(&caveats.fs_read, &full);
            let receipt = file_capture::receipt(path, &receipt_before, &receipt_after);
            match delete_result {
                Ok(()) => {
                    if !file_capture::verified_after(&receipt_after, mutation_scope, &full, None) {
                        return receipt.present(
                            format!("error: delete_file returned success for {path}, but absence could not be verified"),
                            "",
                            presentation,
                        );
                    }
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
                        .map(|cmd| run_build_check(cmd, build_workspace, &caveats.net))
                        .unwrap_or_default();
                    receipt.present_success(format!("deleted {path}"), &format!("{artifact}{check}"), presentation)
                }
                Err(tool_output) => receipt.present(file_capture::failure(tool_output.render(workspace, &caveats.fs_read), ""), "", presentation),
            }
        }

        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let old_string = args["old_string"].as_str().unwrap_or("");
            // Validate the call BEFORE the permission gate or the file: an
            // incomplete edit must never become a mutation. `new_string` was
            // read with `unwrap_or("")` since #253, so an edit that arrived
            // without it silently DELETED its target; a weak model under
            // context pressure does drop keys. An explicit "" still deletes on
            // purpose.
            let Some(new_string) = args.get("new_string").and_then(serde_json::Value::as_str)
            else {
                return "error: edit_file needs `new_string` as a string (use \"\" to delete \
                        the matched text) — nothing was changed"
                    .to_string();
            };
            // The line-range mode (#2553) is removed: it was not bound to the
            // version the model observed, so a stale range edited the wrong
            // lines. A caller still sending one is refused, never reinterpreted.
            if args.get("start_line").is_some() || args.get("end_line").is_some() {
                return "error: edit_file does not take a line range (start_line/end_line) — \
                        give old_string, the exact text to replace; nothing was changed"
                    .to_string();
            }
            if path.is_empty() {
                return "error: edit_file needs `path` — nothing was changed".to_string();
            }
            if old_string.is_empty() {
                return "error: old_string must not be empty — use write_file to create new files"
                    .to_string();
            }
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
                    return denied_fs_result("fs_write", &full_str);
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
            if !file_capture::regular_target(&full) {
                return file_capture::present(
                    format!("error: edit_file refuses a nonregular target: {path}"),
                    presentation,
                );
            }
            let mutation_scope = if scope_permits { &caveats.fs_write } else { &crate::caveats::Scope::All };
            // step-52.5: read the existing file object-bound beneath the same
            // fs_write root (a symlink-escape edit is refused here, so the
            // no-match head display below can't leak an outside file either).
            let read = file_capture::read_for_edit(mutation_scope, &full, path);
            let existing = match read {
                Ok(s) => s,
                Err(tool_output) => return tool_output.render(workspace, &caveats.fs_read),
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
            let receipt_before = file_capture::capture(&caveats.fs_read, &full);
            // Detect a changed preimage before replacing it. This is an
            // observed-before/verified-after contract, not a filesystem lock.
            if !file_capture::current_preimage(&receipt_before, mutation_scope, &full, existing.as_bytes()) {
                return file_capture::present(
                    format!("error: edit_file refused a stale preimage for {path}; reread the file and retry"),
                    presentation,
                );
            }
            // step-52.5: object-bound write when the scope authorised it.
            let write_result = if scope_permits {
                object_bound_write(&caveats.fs_write, "fs_write", path, &full, &full_str, &updated)
            } else {
                std_write(&full, path, &updated)
            };
            let receipt_after = file_capture::capture(&caveats.fs_read, &full);
            let receipt = file_capture::receipt(path, &receipt_before, &receipt_after);
            match write_result {
                Ok(()) => {
                    if !file_capture::verified_after(&receipt_after, mutation_scope, &full, Some(updated.as_bytes())) {
                        return receipt.present(
                            format!("error: edit_file returned success for {path}, but the replacement bytes could not be verified"),
                            "",
                            presentation,
                        );
                    }
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
                        match artifact_file_matches(&caveats.fs_write, &full, updated.as_bytes()) {
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
                        .map(|cmd| run_build_check(cmd, build_workspace, &caveats.net))
                        .unwrap_or_default();
                    let escape_warning = literal_newline_escape_warning(old_string, new_string)
                        .map(|w| format!("\n{w}"))
                        .unwrap_or_default();
                    receipt.present_success(format!("edited {path} ({delta_str} lines, now {new_lines} total){escape_warning}"), &format!("{artifact}{check}{}", super::publish_early::bounded_move_hint(old_string.lines().count().max(new_string.lines().count()))), presentation)
                }
                Err(tool_output) => receipt.present(file_capture::failure(tool_output.render(workspace, &caveats.fs_read), ""), "", presentation),
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
                    return denied_fs_result("fs_read", &full_str);
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
                object_bound_list(&caveats.fs_read, &full, &full_str)
            } else {
                std_list_dir(&full)
            };
            match listing {
                Ok(mut names) => {
                    names.sort();
                    names.join("\n")
                }
                Err(tool_output) => tool_output.render(workspace, &caveats.fs_read),
            }
        }

        // #496: embedded, shell-free file search. The reported breakage was an
        // agent that needed `find` but the build's shell tool was unavailable;
        // this arm walks the workspace with the `ignore` crate (no subprocess),
        // gated by the same fs_read caveat as list_dir/read_file.
        "find" | "grep" if smart_harness.is_some() =>
            {
                invocation.expect("smart dispatch has a witness").host();
                executed((
                    format!("Error: frame isolation: native {name} is unavailable until its recursive walker retains the directory capability; use run_command for confined shell search"),
                    crate::ExecOutcome::Unavailable,
                ))
            },
        "find" => {
            let path = args["path"].as_str().unwrap_or(".");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            const WORKSPACE_ONLY: &str = "capability denied: find is workspace-only; an fs_read grant cannot enable an external search root. Use list_dir for an authorized directory, or run_command within your granted authority.";
            if !find_root_contained(&caveats.fs_read, workspace, &full, &full_str) {
                return WORKSPACE_ONLY.to_string();
            }
            if !tui_permits_path(&caveats.fs_read, &full_str) {
                let allowed = permission_gate.is_some_and(|gate| {
                    fs_gate_allows(gate, "find", DenialKind::FsRead, &full_str, |c| &c.fs_read)
                });
                if !allowed {
                    return denied_fs_result("fs_read", &full_str);
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
            if let Err(error) = std::fs::metadata(&full) {
                return FileIoError::io(&error, &full, format!("error: no such path '{path}'")).render(workspace, &caveats.fs_read);
            }
            // Recheck workspace containment after any human prompt. The legacy
            // walk still reopens this path; smart sessions refuse this adapter.
            if !find_root_contained(&caveats.fs_read, workspace, &full, &full_str) {
                return WORKSPACE_ONLY.to_string();
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

        "grep" => {
            // In-process regex line search (ripgrep's searcher + regex
            // matcher) — needs only `fs_read`, like `read_file`. No shell,
            // no subprocess, so a run_command refusal can't take the agent's
            // grep with it.
            let path = args["path"].as_str().unwrap_or(".");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            const WORKSPACE_ONLY: &str = "capability denied: grep is workspace-only; an fs_read grant cannot enable an external search root. Use list_dir for an authorized directory, or run_command within your granted authority.";
            if !find_root_contained(&caveats.fs_read, workspace, &full, &full_str) {
                return WORKSPACE_ONLY.to_string();
            }
            if !tui_permits_path(&caveats.fs_read, &full_str) {
                let allowed = permission_gate.is_some_and(|gate| {
                    fs_gate_allows(gate, "grep", DenialKind::FsRead, &full_str, |c| &c.fs_read)
                });
                if !allowed {
                    return denied_fs_result("fs_read", &full_str);
                }
            }
            // #1176: shadow-OCAP — record the search root under --full-access.
            if full_access_requested() {
                crate::flight_recorder::log_observed(
                    crate::flight_recorder::ShadowAxis::FsRead,
                    &full_str,
                    "grep",
                );
            }
            let glob = args["glob"].as_str();
            let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
            // Absent -> default; junk -> fail loudly (mirrors read_file paging).
            // #2672: numbers as models send them; junk fails loudly, never a default.
            let context = match arg_usize(args, "context") {
                Ok(v) => v.unwrap_or(0),
                Err(e) => return format!("error: grep: {e}"),
            };
            let max_results = match arg_usize(args, "max_results") {
                Ok(Some(0)) => return "error: grep: `max_results` must be at least 1".to_string(),
                Ok(v) => v.unwrap_or(grep_tool::DEFAULT_MAX_RESULTS),
                Err(e) => return format!("error: grep: {e}"),
            };
            if let Err(error) = std::fs::metadata(&full) {
                return FileIoError::io(&error, &full, format!("error: no such path '{path}'")).render(workspace, &caveats.fs_read);
            }
            if !find_root_contained(&caveats.fs_read, workspace, &full, &full_str) {
                return WORKSPACE_ONLY.to_string();
            }
            let opts = grep_tool::GrepOpts {
                pattern: args["pattern"].as_str().unwrap_or(""),
                glob,
                ignore_case,
                context,
                max_results,
            };
            match grep_tool::grep_search(&full, std::path::Path::new(workspace), &opts) {
                Ok(found) => {
                    let mut out = if found.lines.is_empty() {
                        format!("no matches for {} under {path}", opts.pattern)
                    } else {
                        found.lines.join("\n")
                    };
                    if found.truncated {
                        out.push_str(&format!(
                            "\n[stopped at {max_results} results; narrow pattern/path/glob or raise max_results]"
                        ));
                    }
                    if found.skipped > 0 {
                        out.push_str(&format!(
                            "\n[{} file(s) not searched: binary or unreadable]",
                            found.skipped
                        ));
                    }
                    out
                }
                Err(e) => format!("error: grep: {e}"),
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
            let dirs = super::display::with_migration_notices(crate::Config::resolve_unpublished)
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
                        harness_bound: false,
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
            let registry = agent_bridle::registry();
            let grant = registry.mint_grant(effective_caveats.clone());
            match registry.dispatch("web_fetch", fetch_args, &grant).await {
                Ok(result) =>
                    render_web_fetch_result(url, &result, &*mcp, persona_tools, disposition),
                // A `net`-axis leash denial, or a fetch error (SSRF screen,
                // timeout) — surface the reason; Display is safe. Private-address
                // denials gain an MCP-first recovery hint without weakening the
                // refusal itself.
                Err(e) => {
                    let reason = e.to_string();
                    // #2643: the exec/run_command path journals denials via
                    // `record_envelope`; this leash refusal has no envelope
                    // (net is checked before any subprocess exists), so it
                    // needs its own append or `newt ocap denials` never sees
                    // a `web_fetch` net denial at all. #2645 rounds 2-3: see
                    // `web_fetch_denial_host`'s doc comment for why this
                    // can't just substring-match `reason`.
                    if let Some(host) = web_fetch_denial_host(url, &e) {
                        crate::denial_journal::record_net_denial("web_fetch", &host, &reason);
                    }
                    render_web_fetch_error(url, &reason, &*mcp, persona_tools, disposition)
                }
            }
        }

        other => unknown_tool_message(other),
    }
}

/// Classify an [`execute_tool`] result string as success or failure for the
/// turn's recorded tool events (Step 17.6, #246). Best-effort by necessity —
/// tool results are plain strings fed back to the model — so this mirrors
/// the prefixes this module (and `McpTools::call`) actually emit for a call
/// that accomplished no claimable work: the hard-failure family (`error:`,
/// `capability denied:`, `unknown tool`), and — #1972 — the lifecycle tool's
/// honest no-op degrade (`no command configured`, `crate::tooling::
/// unconfigured_phase_message`). That prefix is deliberately NOT `error:`:
/// the tool genuinely did not fail (it degrades honestly, by design — see
/// `catalog::lifecycle_tool_definition`), so alarming the model with a fake
/// failure would be its own dishonesty; but recording it `ok=true` let a
/// no-op ledger as indistinguishable from a real phase having run. A
/// successful `run_command` whose *output* happens to start with one of
/// these is misclassified; the recorded event is an outcome claim, not a
/// gate.
pub(crate) fn tool_result_ok(result: &str) -> bool {
    let r = result.trim_start();
    !(r.starts_with("error:")
        || r.starts_with("capability denied:")
        || r.starts_with("unknown tool")
        || r.starts_with("no command configured"))
}

/// #2482 item 6 (follow-up to #2521): ground the ledgered `ok` bit in the
/// dispatch OUTCOME whenever one exists, full stop — not only for `Denied`.
/// A `run_command` that exits 0 but whose stdout happens to start with
/// `error:` is authoritative success (`Passed`) regardless of what its own
/// output says; a `Failed`/`TimedOut`/`Unavailable` run is authoritative
/// non-success even when its rendered text has no recognized failure prefix.
/// `tool_result_ok`'s text prefixes are a best-effort fallback for the
/// common case where no structured outcome was recorded at all (`None` —
/// most built-ins never touch the execution slot). Every `ExecOutcome`
/// variant is named explicitly (no `_ =>`) so a new variant must be
/// classified on purpose, not silently fall through to text.
pub(crate) fn tool_ok(result: &str, execution: Option<crate::ExecOutcome>) -> bool {
    match execution {
        Some(crate::ExecOutcome::Passed) => true,
        Some(
            crate::ExecOutcome::Failed
            | crate::ExecOutcome::Denied
            | crate::ExecOutcome::TimedOut
            | crate::ExecOutcome::Unavailable,
        ) => false,
        None => tool_result_ok(result),
    }
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
// F20 — a build command gets the build wall even when it is compound (the
// shell lane #2533's routing refuses to route it into).
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tools_tests/build_wall_tests.rs"]
mod build_wall_tests;

// ---------------------------------------------------------------------------
// INTERIM (#297) --disable-ocap / --yolo tests — the exec escape hatch.
// Removed with the bypass when brush upstreams CommandInterceptor
// (agent-bridle#20).
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tools_tests/disable_ocap_tests.rs"]
pub(crate) mod disable_ocap_tests;

// ---------------------------------------------------------------------------
// #2558 HANDOFF item 2 — the same-file redirect guard (`cmd f > f` must not
// truncate f), through real dispatch in both the confined and host lanes.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tools_tests/redirect_guard.rs"]
mod redirect_guard_tests;

#[cfg(test)]
#[path = "tools_tests/smart_frame_isolation.rs"]
mod smart_frame_isolation_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "tools_tests/smart_tool_completion.rs"]
mod smart_tool_completion_tests;

#[cfg(test)]
#[path = "tools_tests/sibling_worktree_build.rs"]
mod sibling_worktree_build_tests;

#[cfg(all(windows, feature = "test-util"))]
pub(crate) use shell::test_windows_ambient_dispatch;

#[cfg(test)]
#[path = "tools_tests/git_fixture.rs"]
mod git_fixture;

#[cfg(test)]
#[path = "tools_tests/command_shape_contract.rs"]
mod command_shape_contract;

/// Resolve the environment through the same seam as confined dispatch.
pub(crate) fn dispatch_exec_path() -> Option<std::ffi::OsString> {
    shell::venv_env_map()
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
}

#[cfg(all(test, target_os = "macos"))]
#[path = "tools_tests/path_aliases.rs"]
mod path_aliases;

#[cfg(all(test, unix))]
#[path = "tools_tests/path_alias_attacks.rs"]
mod path_alias_attacks;
