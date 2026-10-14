//! Tool-call classification: which calls only read, which may change the
//! workspace, which write, and which count as passing verification.
//!
//! Moved verbatim from `agentic/mod.rs`; `mod.rs` re-imports every item so
//! existing paths keep resolving.

use super::{git_tool, routing, tools};

pub(super) fn is_read_only_tool(name: &str) -> bool {
    crate::navigator::NAV_TOOL_NAMES.contains(&name)
        || matches!(
            name,
            "list_dir"
                | "read_file"
                | "find"
                | "search"
                | "web_fetch"
                | "use_skill"
                | "save_note"
                | "recall"
                | "prompt_read"
                | "artifact_read"
        )
}

pub(super) fn is_read_only_call(name: &str, args: &serde_json::Value) -> bool {
    is_read_only_tool(name)
        || (name == "git"
            && args
                .get("op")
                .and_then(|op| op.as_str())
                .is_some_and(git_tool::is_scoped_read_op))
        || (name == "run_command"
            && args
                .get("command")
                .and_then(|v| v.as_str())
                .is_some_and(is_read_only_shell_probe))
}

/// The tools that actually modify the workspace tree — `write_file` and
/// `edit_file`. Aliases the model sometimes invents (`create_file`,
/// `str_replace`, `apply_patch`) get a coaching reply and change nothing, so
/// they are deliberately NOT in the set.
///
/// Public because the `solve_result` trace line reports on writes too, and a
/// second copy of this literal set in the CLI is exactly the drift the reuse
/// discipline forbids: adding a write tool here would silently stop being
/// counted there.
pub fn is_workspace_write_call(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file")
}

/// U4b: did this completed call PASS a progress-evidencing check — a `lifecycle`
/// run of a gate phase (`test`/`check`, [`crate::tooling::Phase::is_gate`]), or a
/// `run_command` ROUTED to the confined build lane (#2533/#2543's
/// `routing::RouteTable`) whose subcommand IS a gate — `cargo test`/`cargo
/// check`, or a `just` recipe literally named `test`/`check`? Mirrors
/// `Phase::is_gate` exactly (`Phase::from_key` happens to share its "test"/
/// "check" keys with cargo's own subcommand strings and with a recipe name,
/// so this is the SAME rule, not a parallel one): `cargo build` (an
/// incremental no-op when already built), `cargo clippy`, and a `just fmt`/
/// `just clean` all route to `build_exec` too but do NOT count — #2548
/// round 2's should-fix, because the lifecycle side never counted `lint`/
/// `format`/`clean`/`setup` either, and a model alternating `edit_file` with
/// a trivially-passing `just fmt` must not reset the brake.
///
/// A bare `run_command` is still deliberately excluded: a shell exit code is
/// not evidence of a passing build (`| tail` masks it — see
/// `WorkflowRuntimeState::record_observation`), and recognising build commands by keyword
/// would be an invented classifier. #2551 round 2 (should-fix): this used to
/// RE-DERIVE the route via a second `classify_call`, which reads the
/// filesystem — and the call in between can change it. Measured: `cd
/// target; cargo test | tail -5` on a clean checkout refuses to route at
/// DISPATCH (`target/` does not exist yet) and runs unrouted at the root,
/// where `cargo test` CREATES `target/` and `| tail` masks the exit code;
/// re-classifying AFTER the call then routes successfully and would have
/// counted exactly the masked pass #2548 exists to exclude. `routed_to` is
/// dispatch's OWN recorded decision (`tools.rs`'s routing site, the same
/// place that decided what actually ran) — read here, never recomputed —
/// so verification cannot see a filesystem state dispatch never acted on.
pub(super) fn is_progress_verification(
    name: &str,
    args: &serde_json::Value,
    execution: Option<crate::ExecOutcome>,
    routed_to: Option<&(&'static str, serde_json::Value)>,
) -> bool {
    if execution != Some(crate::ExecOutcome::Passed) {
        return false;
    }
    if name == "lifecycle" {
        return args
            .get("phase")
            .and_then(|v| v.as_str())
            .and_then(crate::tooling::Phase::from_key)
            .is_some_and(crate::tooling::Phase::is_gate);
    }
    if name != "run_command" {
        return false;
    }
    let Some(("build_exec", routed)) = routed_to.map(|(tool, args)| (*tool, args)) else {
        return false;
    };
    // `argv[0]` is the program (`cargo`/`just`); `argv[1]` is cargo's
    // subcommand or the just recipe name — the SAME slot `Phase::from_key`
    // reads for `lifecycle`, so one function decides "is this a gate" for
    // both call shapes. #2524 follow-up: `cargo_build_route` keeps a
    // leading `+toolchain` selector (`cargo +stable test`) in `argv`, so
    // that slot may be `argv[1]` (`+stable`) instead of the subcommand —
    // skip it with the SAME `is_toolchain_selector` `cargo_build_route`
    // itself checks, shared rather than re-derived, so the two can never
    // disagree about what counts as a selector.
    let Some(argv) = routed.get("argv").and_then(|v| v.as_array()) else {
        return false;
    };
    let sub = match argv.get(1).and_then(|v| v.as_str()) {
        Some(token) if routing::is_toolchain_selector(token) => {
            argv.get(2).and_then(|v| v.as_str())
        }
        other => other,
    };
    sub.and_then(crate::tooling::Phase::from_key)
        .is_some_and(crate::tooling::Phase::is_gate)
}

/// #2374: built-in tools that never touch workspace files: harness state,
/// plans, retrieval, operator prompts, mode switches.
pub(super) fn is_workspace_inert_tool(name: &str) -> bool {
    matches!(
        name,
        "code_search"
            | "where_is"
            | "tool_search"
            | "memory_fetch"
            | "re_read"
            | "resume_context"
            | "experience_recall"
            | "experience_record"
            | "state_get"
            | "state_set"
            | "state_clear"
            | "plan_get"
            | "update_plan"
            | "render_report"
            | "request_permissions"
            | "request_user_input"
            | "select_operating_mode"
            | "enter_plan_mode"
            | "exit_plan_mode"
    ) || tools::is_context_remaining_call(name)
}

/// #2374: simple shell commands that only read. Used only by result-aware
/// verification, so the default path's probe steering is unchanged. It differs
/// from [`is_read_only_shell_probe`]'s list on purpose: `sed` is left out,
/// because `-ri`, `-Ei`, `--in-place` and its `w` command all write.
pub(super) const VERIFICATION_READ_PROGRAMS: &[&str] = &[
    "grep",
    "rg",
    "head",
    "tail",
    "wc",
    "pwd",
    "cat",
    "ls",
    "find",
    "stat",
    "file",
    "tree",
    "git status",
    "git diff",
    "git log",
    "git show",
];

/// #2374: the tool a call reaches: a rewrite alias's canonical name, the name
/// itself otherwise, and `None` for a corrective alias, which only returns
/// coaching text.
pub(crate) fn dispatched_tool_name(name: &str) -> Option<&str> {
    match tools::resolve_tool_alias(name) {
        Some(tools::AliasOutcome::Correct(_)) => None,
        Some(tools::AliasOutcome::Rewrite(canonical)) => Some(canonical),
        None => Some(name),
    }
}

/// #2374: whether a call may have changed the workspace: anything that is
/// neither read-only nor a workspace-inert built-in, judged as the tool it
/// reaches. Unknown and MCP tools fail closed (they may).
pub(crate) fn may_change_workspace(name: &str, args: &serde_json::Value) -> bool {
    let Some(name) = dispatched_tool_name(name) else {
        return false;
    };
    if name == "run_command" {
        return args["command"]
            .as_str()
            .is_none_or(|command| !is_verification_read_command(command));
    }
    !is_read_only_call(name, args) && !is_workspace_inert_tool(name)
}

/// A single simple command that only reads: one of
/// [`VERIFICATION_READ_PROGRAMS`], with no shell metacharacters and no action
/// that writes or runs (`find -delete` / `-exec` / `-fprint`, `--output=`,
/// `tree -o`), however quotes or backslashes spell the token.
pub(crate) fn is_verification_read_command(command: &str) -> bool {
    let command = command.trim();
    const SHELL_META: &[char] = &['&', '|', ';', '`', '$', '\n', '>', '<', '(', ')'];
    if command.is_empty() || command.contains(SHELL_META) {
        return false;
    }
    let tokens: Vec<String> = command
        .split_whitespace()
        .map(|t| t.replace(['"', '\'', '\\'], ""))
        .collect();
    let acts = tokens.iter().any(|t| {
        t.starts_with("--output")
            || matches!(
                t.as_str(),
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
    }) || (tokens[0] == "tree" && tokens.iter().any(|t| t == "-o"));
    if acts {
        return false;
    }
    VERIFICATION_READ_PROGRAMS
        .iter()
        .any(|p| command == *p || command.starts_with(&format!("{p} ")))
}

pub(super) fn is_read_only_shell_probe(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    const SHELL_META: &[char] = &['&', '|', ';', '`', '$', '\n', '>', '<', '(', ')'];
    if command.contains(SHELL_META) {
        return false;
    }
    let mut tokens = command.split_ascii_whitespace();
    let Some(program) = tokens.next() else {
        return false;
    };
    match program {
        "grep" | "rg" | "head" | "tail" | "wc" | "pwd" => true,
        "sed" => !tokens.any(|t| t == "-i" || t.starts_with("-i")),
        _ => false,
    }
}

// The narrate-then-stop rescue budget is `ChatCtx.narration_nudge_cap`
