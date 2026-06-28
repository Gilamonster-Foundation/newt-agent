//! Built-in tool definitions and the tool executor for the agentic loop.
//! Moved verbatim from `newt-tui` in Step 9.7 — the Caveats enforcement,
//! shrink guard, build-check feedback, and agent-bridle routing are unchanged.

use super::crew_tool::CrewRunner;
use super::display::{print_denied, print_tool_call, print_tool_output};
use super::git_tool::GitTool;
use super::mcp::McpTools;
use super::memory_fetch::{execute_memory_fetch, memory_fetch_tool_definition, MemorySource};
use super::note_sink::{execute_save_note, save_note_tool_definition, NoteSink};
use super::permissions::{DenialKind, PermissionDecision, PermissionGate, PermissionRequest};
use super::recall::{execute_recall, recall_tool_definition, RecallSource};
use crate::caveats::CaveatsExt as _;

/// #719: default line window for `read_file`'s **model-facing** payload. The
/// on-screen display is capped separately; this bounds what enters the model's
/// context, so one read of a 15k-line file (e.g. `newt-tui/src/lib.rs`) can no
/// longer saturate a small local model's window and abandon the task.
const DEFAULT_READ_LIMIT: usize = 2_000;

/// #719: hard char backstop on the `read_file` payload, independent of the line
/// window — catches pathological long-line files (minified blobs) that a few
/// lines can still blow up.
const MAX_READ_CHARS: usize = 100_000;

/// Window + cap a file's contents for `read_file`'s model-facing payload (#719).
/// Returns lines `[offset, offset+limit)` (1-based `offset`, default 1; `limit`
/// default [`DEFAULT_READ_LIMIT`]), truncated to [`MAX_READ_CHARS`], with a
/// footer pointing at the next window so the model paginates instead of drowning.
/// A whole-file read that fits both caps is returned verbatim (exact bytes).
/// Pure (no fs) — unit-tested directly.
fn paginate_read(contents: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    let total = contents.lines().count();
    let start = offset.filter(|&o| o > 0).unwrap_or(1); // 1-based
    let limit = limit.filter(|&l| l > 0).unwrap_or(DEFAULT_READ_LIMIT);
    // Common case: a whole-file read that fits both caps → return verbatim.
    if start == 1 && limit >= total && contents.len() <= MAX_READ_CHARS {
        return contents.to_string();
    }
    let start0 = start - 1;
    if start0 >= total {
        return format!("(offset {start} is past end of file — {total} lines total)");
    }
    let window: Vec<&str> = contents.lines().skip(start0).take(limit).collect();
    let end = start0 + window.len(); // 1-based last line shown == end
    let mut body = window.join("\n");
    let char_capped = body.len() > MAX_READ_CHARS;
    if char_capped {
        let mut cut = MAX_READ_CHARS;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    let footer = if char_capped {
        Some(format!(
            "payload truncated to {MAX_READ_CHARS} chars from line {start}; \
             call read_file with a higher offset (and/or smaller limit) to continue"
        ))
    } else if end < total {
        Some(format!(
            "showing lines {start}-{end} of {total}; \
             call read_file with offset={} to continue",
            end + 1
        ))
    } else {
        None
    };
    match footer {
        Some(f) => format!("{body}\n\n[{f}]"),
        None => body,
    }
}

pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command in the workspace directory and return its output",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to run" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file in the workspace. Returns up to `limit` lines \
                                (default 2000) starting at 1-based `offset` (default 1). Large \
                                files come back with a footer pointing at the next window — read \
                                them in pages with offset/limit rather than all at once, or the \
                                full file can saturate the context window.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to workspace root" },
                        "offset": { "type": "integer", "description": "1-based line number to start at (default 1)" },
                        "limit": { "type": "integer", "description": "Maximum number of lines to return (default 2000)" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write or overwrite a file in the workspace. \
                                WARNING: use edit_file instead when modifying an existing file — \
                                write_file replaces the entire contents and will fail if the new \
                                content is significantly shorter than the original (shrink guard). \
                                Only use write_file for new files or full rewrites you have \
                                explicitly generated in their entirety.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to workspace root" },
                        "content": { "type": "string", "description": "The complete new file contents" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Make a targeted edit to an existing file by replacing one exact \
                                string with another. Safer than write_file for modifying existing \
                                files — you only generate the change, not the whole file. \
                                Fails with a clear error if old_string is not found or matches \
                                multiple times (add more surrounding context to make it unique).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to workspace root" },
                        "old_string": { "type": "string", "description": "Exact string to find and replace (must match exactly once)" },
                        "new_string": { "type": "string", "description": "Replacement string" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List files in a directory",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory path relative to workspace root (use '.' for root)" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find",
                "description": "Find files and directories by name under the workspace, recursively, WITHOUT a shell (use this instead of the `find` shell command). Returns matching paths relative to the workspace root, one per line, already sorted — no need to pipe to `sort`. Respects .gitignore and skips noise (.git, target, node_modules) by default.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory to search under, relative to workspace root. Default '.' (the whole workspace)." },
                        "name": { "type": "string", "description": "Glob matched against each entry's basename, e.g. '*.py' or 'pyo3_module.rs'. '*' matches any run, '?' any single char. Omit to match everything." },
                        "type": { "type": "string", "enum": ["f", "d", "any"], "description": "Restrict to files ('f'), directories ('d'), or both ('any', the default)." },
                        "max_depth": { "type": "integer", "description": "Maximum directory depth below `path` (1 = immediate children only). Omit for unlimited." },
                        "max_results": { "type": "integer", "description": "Cap on the number of matches returned. Default 1000; output notes when truncated." },
                        "respect_gitignore": { "type": "boolean", "description": "When true (default) skip .gitignored paths plus .git/target/node_modules/hidden dirs. Set false to search everything." },
                        "case_sensitive": { "type": "boolean", "description": "Case-sensitive basename match. Default true." }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "use_skill",
                "description": "Load a skill's full procedural instructions on demand. The system prompt lists the available skills (name + description); call this with a skill's name to get its complete SKILL.md body plus the paths of any bundled files (scripts/templates) you can read or run.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "The skill name as shown in the 'Available skills' index" }
                    },
                    "required": ["name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch an http(s) URL and return its main content as clean markdown. Use this to read documentation, issues, or pages the task references. Reachable hosts are gated by the session's network capability; the returned text is untrusted page content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The http(s) URL to fetch" },
                        "max_bytes": { "type": "integer", "description": "Optional cap on bytes downloaded (default 5 MiB, max 25 MiB)" }
                    },
                    "required": ["url"]
                }
            }
        }
    ])
}

/// The built-in tool definitions plus every connected MCP server's tools
/// (namespaced `server__tool`). This is what the agent loop advertises to the
/// model so it can call remote MCP tools alongside the built-ins.
///
/// `with_save_note` adds the `save_note` definition (Step 19.3) — true only
/// when the caller supplied a `NoteSink`, so headless/eval sessions (which
/// pass `note_sink: None`) never advertise a tool that can't be executed.
/// `with_recall` gates the `recall` definition (Step 17.5) the same way on
/// a supplied `RecallSource`. `with_memory_fetch` gates the `memory_fetch`
/// definition (progressive-disclosure memory, Workstream A MVP, #319) the same
/// way on a supplied `MemorySource` — `None` ⇒ the tool is never advertised,
/// so eval / headless / ACP sessions are unaffected bit-for-bit.
#[allow(clippy::too_many_arguments)] // presence-gated tool advertisers (one bool each)
pub(crate) fn merged_tool_definitions(
    mcp: &dyn McpTools,
    with_save_note: bool,
    with_recall: bool,
    with_memory_fetch: bool,
    with_git: bool,
    with_team: bool,
    with_scratchpad: bool,
    with_code_search: bool,
    with_experiential: bool,
    with_scheduled: bool,
) -> serde_json::Value {
    let mut defs = match tool_definitions() {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    // #714: `resume_context` is advertised ALWAYS — it is broadly useful and
    // degrades gracefully (the executor returns a clear "no history this
    // session" when its sources are `None`), so unlike the presence-gated tools
    // it carries no `with_*` flag and rides in every session, headless included.
    defs.push(super::resume::resume_context_tool_definition());
    if with_save_note {
        defs.push(save_note_tool_definition());
    }
    if with_recall {
        defs.push(recall_tool_definition());
    }
    if with_memory_fetch {
        defs.push(memory_fetch_tool_definition());
    }
    // PR4: the `git` tool is advertised only when a GitTool is injected (the
    // binary is in a git repo and supplied LocalGitTool) — presence gate.
    if with_git {
        defs.push(super::git_tool::git_tool_definition());
    }
    // #479: the `compose_roster` + `crew` tools are advertised only when a
    // CrewRunner is injected (the `/team` toggle) — presence gate.
    if with_team {
        defs.push(super::crew_tool::compose_roster_tool_definition());
        defs.push(super::crew_tool::crew_tool_definition());
    }
    // Step 26.4 (#583): the scratchpad state tools — advertised only when the
    // `scratchpad` feature is on AND a store is present (presence gate).
    if with_scratchpad {
        defs.push(super::scratchpad::state_set_tool_definition());
        defs.push(super::scratchpad::state_get_tool_definition());
        defs.push(super::scratchpad::state_clear_tool_definition());
    }
    // Step 26.5.5 (#582): the code_search tool — advertised only when the
    // `semantic` feature is on AND an index is present (presence gate).
    if with_code_search {
        defs.push(super::semantic::code_search_tool_definition());
    }
    // Step 26.6a (#585): the experiential record/recall tools, only with the gate.
    if with_experiential {
        defs.push(super::experiential::experience_record_tool_definition());
        defs.push(super::experiential::experience_recall_tool_definition());
    }
    // Step 26.6b (#586): the scheduled plan_set/plan_advance tools, only with the gate.
    // #716: the read-only plan_get joins them under the same scheduled gate.
    if with_scheduled {
        defs.push(super::scheduled::plan_set_tool_definition());
        defs.push(super::scheduled::plan_advance_tool_definition());
        defs.push(super::scheduled::plan_get_tool_definition());
    }
    defs.extend(mcp.tool_defs());
    serde_json::Value::Array(defs)
}

/// Direct tool names the model must call as tool invocations, never as shell
/// commands passed to `run_command`.
const DIRECT_TOOL_NAMES: &[&str] = &[
    "list_dir",
    "read_file",
    "write_file",
    "edit_file",
    "use_skill",
    "web_fetch",
    // #496: `find …` typed at run_command redirects to the embedded `find`
    // tool — which works even when the shell is unavailable in this build.
    "find",
    // PR4: `git …` typed at run_command redirects to the embedded `git` tool.
    "git",
];

/// Every tool newt can dispatch by name — the base tools plus all
/// presence-gated ones. Single source of truth for [`is_hallucination`] (which
/// names are real) and [`nearest_tool_name`] (suggestion candidates). MCP
/// `server__tool` names contain `__` and are matched separately.
const ALL_TOOL_NAMES: &[&str] = &[
    "run_command",
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "find",
    "use_skill",
    "web_fetch",
    "save_note",
    "recall",
    "memory_fetch",
    "git",
    "compose_roster",
    "crew",
    "state_set",
    "state_get",
    "state_clear",
    "code_search",
    "experience_record",
    "experience_recall",
    "plan_set",
    "plan_advance",
    "plan_get",
    // #714: always-advertised self-recovery read (degrades gracefully when its
    // sources are absent), so it is never treated as a hallucination.
    "resume_context",
];

/// Returns `true` if a tool call looks like a hallucination:
/// - `run_command` called with a tool name as the shell command, or
/// - An unknown tool name (excluding MCP-namespaced `server__tool` names).
pub(crate) fn is_hallucination(tool_name: &str, args: &serde_json::Value) -> bool {
    if tool_name == "run_command" {
        let cmd = args["command"].as_str().unwrap_or("");
        let first = cmd.split_ascii_whitespace().next().unwrap_or("");
        return DIRECT_TOOL_NAMES.contains(&first);
    }
    // MCP tools are namespaced with `__` — never treat them as hallucinations.
    if tool_name.contains("__") {
        return false;
    }
    !ALL_TOOL_NAMES.contains(&tool_name)
}

/// Outcome of resolving a foreign / hallucinated tool name against newt's real
/// tools (Step 27.1). Weak local models routinely emit tool names learned from
/// other agent harnesses (`str_replace_editor`, `execute`, `bash`); without a
/// resolution layer each such call is a flat "unknown tool" that burns a whole
/// tool-call round, so the model narrates instead of acting (the #215 advisory
/// drift). Resolving them turns a wasted round into a self-correcting one.
pub(crate) enum AliasOutcome {
    /// A foreign name whose argument shape matches a real newt tool: rewrite the
    /// call to this canonical name and dispatch it transparently.
    Rewrite(&'static str),
    /// A foreign name whose arguments do NOT match the real tool: return this
    /// correction (naming the right tool + its signature) so the model retries.
    Correct(String),
}

/// Map a non-newt tool name to a real tool, when we recognize it. Real tool
/// names and MCP `server__tool` names return `None` and dispatch unchanged.
pub(crate) fn resolve_tool_alias(name: &str) -> Option<AliasOutcome> {
    match name {
        // Shell aliases — same single `command` arg shape as run_command, so we
        // rewrite and dispatch. (If the shell is unavailable this build, the
        // run_command arm reports that — Step 27.3 presence-gates it.)
        "execute" | "exec" | "bash" | "shell" | "sh" | "zsh" | "terminal" | "run_shell_command"
        | "shell_command" | "system" => Some(AliasOutcome::Rewrite("run_command")),
        // Edit aliases — different arg shape; point at edit_file's signature.
        "str_replace_editor" | "str_replace" | "str-replace-editor" | "apply_patch" | "edit"
        | "editor" | "replace_in_file" | "search_replace" => Some(AliasOutcome::Correct(format!(
            "'{name}' is not a newt tool. To change an existing file, call \
                 edit_file with {{\"path\", \"old_string\", \"new_string\"}} \
                 (replaces one exact occurrence). For a new file or a full \
                 rewrite, call write_file with {{\"path\", \"content\"}}."
        ))),
        // Create-file aliases — point at write_file.
        "create_file" | "new_file" | "createfile" | "add_file" | "touch" => {
            Some(AliasOutcome::Correct(format!(
                "'{name}' is not a newt tool. To create or overwrite a file, call \
                 write_file with {{\"path\", \"content\"}}. To change part of an \
                 existing file, call edit_file with \
                 {{\"path\", \"old_string\", \"new_string\"}}."
            )))
        }
        // Read / list aliases — point at read_file / list_dir.
        "cat" | "open_file" | "view_file" | "view" | "open" => {
            Some(AliasOutcome::Correct(format!(
                "'{name}' is not a newt tool. To read a file, call read_file with \
                 {{\"path\"}}. To list a directory, call list_dir with {{\"path\"}}."
            )))
        }
        // #716 PLAN — start/revise a plan. The arg shape is free prose, not the
        // ordered `{"steps":[…]}` array plan_set wants, so Correct (coach), never
        // a silent Rewrite. When `scheduled` is off the dispatch arm for plan_set
        // returns "scheduled planning is off" — the model still gets a coherent
        // answer rather than a dead end.
        "enter_plan" | "enter_plan_mode" | "plan_mode" | "start_plan" | "begin_plan"
        | "make_plan" | "create_plan" | "plan" | "planning" | "update_plan" | "set_plan"
        | "todo" | "todos" | "todo_write" => Some(AliasOutcome::Correct(format!(
            "'{name}' is not a newt tool. To start or revise your plan, call plan_set with \
             {{\"steps\":[...]}} (ordered short imperative phrases); the active step shows in \
             the <plan> checklist each turn, and call plan_advance when a step is done."
        ))),
        // #716 PLAN-READ — read the current plan. plan_get takes no args, so the
        // foreign call's (empty) arg shape matches: safe to silently Rewrite.
        // `what_was_i_doing` stays here (→ plan_get) — it asks specifically for
        // the plan; the broader "where were we" reaches go to resume_context.
        "get_plan" | "show_plan" | "read_plan" | "current_plan" | "what_was_i_doing" => {
            Some(AliasOutcome::Rewrite("plan_get"))
        }
        // #714 RESUME — the instinctive "where did we leave off" reach. All take
        // no args (resume_context is a self-read), so the (empty) arg shape
        // matches: safe to silently Rewrite. Meets the dead-end reach the issue
        // observed (the model retrying recall) by landing it on the affordance
        // built for exactly this case.
        "resume" | "where_were_we" | "where_did_we_leave_off" | "catch_me_up" | "recap" => {
            Some(AliasOutcome::Rewrite("resume_context"))
        }
        // #716 CREW / DELEGATE — crew/team is the human-only `/team` toggle a
        // model cannot self-enable, and the targets may be unadvertised, so this
        // can only ever Correct (never silently Rewrite) and the message must NOT
        // imply the model can invoke crew itself.
        "delegate" | "spawn_agent" | "subagent" | "sub_agent" | "crew_dispatch" | "run_crew"
        | "dispatch_crew" | "fork_agent" | "assign" | "team" => {
            Some(AliasOutcome::Correct(format!(
                "'{name}' is not a newt tool. Crew/team delegation is only available once the \
                 human enables /team this session — you cannot turn it on yourself. When /team \
                 is on, compose_roster ({{\"mode\"}}) proposes a roster and crew ({{\"task\"}}) \
                 dispatches it."
            )))
        }
        // #716 WORKFLOW — no workflow/pipeline primitive exists; redirect to the
        // plan tools (and crew/team, which needs /team).
        "workflow" | "run_workflow" | "start_workflow" | "pipeline" => Some(AliasOutcome::Correct(
            "newt has no workflow tool; sequence the work with plan_set + plan_advance, or \
                 delegate subtasks via crew/team (needs /team)."
                .to_string(),
        )),
        _ => None,
    }
}

/// #717: classify a single tool/capability reach for the alias-seam telemetry.
///
/// Pure: given the name the model called, its args, the tool result string, and
/// whether the result read as success, decide whether this reach is phantom and
/// how it resolved. Returns `None` for an ordinary real call (nothing to mine).
/// See [`crate::PhantomReach`] / [`crate::PhantomResolution`].
///
/// `ok` is part of the signature for symmetry with the recording site (it keys
/// the sibling `ToolEvent`); v1's miss-patterns classify on name + result alone.
pub(crate) fn classify_phantom_reach(
    name: &str,
    args: &serde_json::Value,
    result: &str,
    ok: bool,
) -> Option<crate::PhantomResolution> {
    let _ = ok;
    // 1. A recognized foreign/alias name: rewrite to a real tool, or a
    //    correction naming the right one — the canonical alias-seam signal.
    match resolve_tool_alias(name) {
        Some(AliasOutcome::Rewrite(canonical)) => {
            return Some(crate::PhantomResolution::Rewrite(canonical.to_string()))
        }
        Some(AliasOutcome::Correct(msg)) => return Some(crate::PhantomResolution::Correct(msg)),
        None => {}
    }
    // 2. An unknown name with no alias is a true phantom tool (hallucination).
    if is_hallucination(name, args) {
        return Some(crate::PhantomResolution::Unknown);
    }
    // 3. A real tool that returned empty-by-design — a high-signal "miss" that
    //    currently logs ok=true. These are the mineable real-tool reaches; the
    //    loop emits one per call, so a 3x identical-recall loop yields 3 records.
    let r = result.trim_start();
    if name == "state_get" && r.starts_with("no such key") {
        return Some(crate::PhantomResolution::RealToolMiss(
            "state_get on an unset key".into(),
        ));
    }
    if name == "recall" && r.starts_with("no matches in past conversations") {
        return Some(crate::PhantomResolution::RealToolMiss(
            "recall returned no matches".into(),
        ));
    }
    None
}

/// Classic Levenshtein edit distance (pure two-row DP). Inputs are tool names
/// (short), so the simple version is plenty — for fuzzy suggestions only.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest real tool name to `name`, if one is near enough to be a likely
/// typo/variant (distance ≤ ⌈len/3⌉, min 1). Returns `None` when nothing is
/// close, so we never suggest a wildly-unrelated tool.
fn nearest_tool_name(name: &str) -> Option<&'static str> {
    let threshold = (name.chars().count() / 3).max(1);
    ALL_TOOL_NAMES
        .iter()
        .map(|&t| (levenshtein(name, t), t))
        .filter(|(d, _)| *d <= threshold)
        .min_by_key(|(d, _)| *d)
        .map(|(_, t)| t)
}

/// Corrective message for a genuinely-unknown tool name: name the real base
/// tools and, when one is close, suggest it — so a weak model that missed the
/// catalog gets a path back instead of a dead end (Step 27.1). Kept
/// `unknown tool: {name}`-prefixed so existing `starts_with` checks hold.
fn unknown_tool_message(name: &str) -> String {
    const BASE: &str =
        "run_command, read_file, write_file, edit_file, list_dir, find, use_skill, web_fetch";
    match nearest_tool_name(name) {
        Some(sugg) => format!(
            "unknown tool: {name}. Did you mean '{sugg}'? Available tools include: \
             {BASE} (plus git and any memory/plan tools enabled this session)."
        ),
        None => format!(
            "unknown tool: {name}. Available tools include: {BASE} (plus git and \
             any memory/plan tools enabled this session)."
        ),
    }
}

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
async fn host_shell_dispatch(cmd: &str, cwd: &str) -> std::io::Result<serde_json::Value> {
    let output = host_shell_output(cmd, cwd).await?;
    Ok(serde_json::json!({
        "exit_code": output.status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        // Honest provenance, same field the bridle envelope always carries:
        // nothing sandboxed this run.
        "sandbox_kind": "none",
    }))
}

/// INTERIM (#297) host shell selection: `bash -c` with an `sh -c` fallback
/// when bash is absent — the same sh-compatible free-form mode the confined
/// shell ran, so [`venv_cmd_prefix`]'s `export …;` prefix works unchanged.
#[cfg(not(windows))]
async fn host_shell_output(cmd: &str, cwd: &str) -> std::io::Result<std::process::Output> {
    fn shell(program: &str, cmd: &str, cwd: &str) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(program);
        c.arg("-c").arg(cmd).current_dir(cwd);
        c
    }
    match shell("bash", cmd, cwd).output().await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => shell("sh", cmd, cwd).output().await,
        other => other,
    }
}

/// INTERIM (#297) host shell selection on Windows: `cmd /C`, the same shape
/// as [`build_check_shell`].
#[cfg(windows)]
async fn host_shell_output(cmd: &str, cwd: &str) -> std::io::Result<std::process::Output> {
    tokio::process::Command::new("cmd")
        .args(["/C", cmd])
        .current_dir(cwd)
        .output()
        .await
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

fn toml_string_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn exec_allowlist_name(target: &str) -> &str {
    target
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(target)
}

fn extra_exec_hint(envelope: &serde_json::Value) -> Option<String> {
    let denials = envelope.get("denials")?.as_array()?;
    let target = denials.iter().find_map(|d| {
        let kind = d.get("kind")?.as_str()?;
        if kind != "exec" {
            return None;
        }
        d.get("target")?
            .as_str()
            .filter(|target| !target.is_empty())
    })?;

    Some(format!(
        "add it via [tui.permissions] extra_exec = [\"{}\"] in your newt config",
        toml_string_literal(exec_allowlist_name(target))
    ))
}

fn envelope_denial_reason_with_guidance(envelope: &serde_json::Value) -> String {
    let reason = envelope_denial_reason(envelope);
    match extra_exec_hint(envelope) {
        Some(hint) => format!("{reason} - {hint}"),
        None => reason,
    }
}

/// The standard `run_command` capability-denial result — the exact text the
/// model has always received. Factored so the #263 prompt path can fall back
/// to it bit-for-bit on deny (and on a second denial after a re-execution).
fn denied_run_command_result(envelope: &serde_json::Value, color: bool) -> String {
    let reason = envelope_denial_reason_with_guidance(envelope);
    print_denied("exec", &reason, color);
    format!("capability denied: {reason}")
}

/// The standard `run_command` success path: print + return stdout/stderr,
/// or `(exit N)` when the command produced no output. Factored verbatim so
/// the #263 re-execution path shares one formatter with the first dispatch.
fn shell_envelope_output(
    envelope: &serde_json::Value,
    tool_output_lines: usize,
    color: bool,
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
    print_tool_output(&out, tool_output_lines, color);
    if out.trim().is_empty() {
        let code = envelope
            .get("exit_code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        format!("(exit {code})")
    } else {
        out
    }
}

/// Lift a confined-shell denial envelope into promptable #263 requests.
///
/// Returns `Some` only when EVERY structured denial entry is an `exec` kind
/// with a non-empty target — the case the human can meaningfully grant (the
/// allowlist name, same basename rule as the config hint). Any other kind
/// (e.g. an `open` refused inside the shell) keeps the standard denial:
/// guessing which fs axis an opaque `open` maps to would over-grant.
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
    if parts.is_empty() {
        path.to_string()
    } else {
        format!("{path} ({})", parts.join(", "))
    }
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

    let mut out: Vec<String> = Vec::new();
    let mut truncated = false;
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
        if out.len() >= opts.max_results {
            truncated = true;
            break;
        }
        let rel = entry
            .path()
            .strip_prefix(workspace_root)
            .unwrap_or_else(|_| entry.path());
        out.push(rel.to_string_lossy().replace('\\', "/"));
    }
    out.sort();
    out.dedup();
    Ok((out, truncated))
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
/// keeps every denial exactly as it was — bit-for-bit.
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
    // Remote MCP tools (namespaced `server__tool`) route to their server before
    // the built-in match. They carry no Caveats leash in this build.
    if mcp.handles(name) {
        print_tool_call(name, &args.to_string(), color);
        let out = mcp.call(name, args).await;
        print_tool_output(&out, tool_output_lines, color);
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

    match name {
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

        // Step 26.6b (#586): scheduled plan_set/plan_advance — presence-gated on
        // the ledger (advertised only when the `scheduled` feature is on).
        "plan_set" => match step_ledger {
            Some(l) => super::scheduled::execute_plan_set(args, l, color, tool_output_lines),
            None => "unknown tool: plan_set (scheduled planning is off)".to_string(),
        },
        "plan_advance" => match step_ledger {
            Some(l) => super::scheduled::execute_plan_advance(l, color, tool_output_lines),
            None => "unknown tool: plan_advance (scheduled planning is off)".to_string(),
        },
        // #716: read-only plan view (the alias target for "what was I doing?"
        // probes) — same presence gate as plan_set/plan_advance.
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
            color,
            tool_output_lines,
        ),

        // Embedded git (PR4, #461): dispatch through the injected GitTool
        // (newt-git's LocalGitTool). `GitCaveats::from_session` projects the
        // session's authority onto the git surface (fail-closed: a read-only
        // session can read but not commit). Same presence-gating as `recall` —
        // without an injected impl the tool was never advertised.
        "git" => match git_tool {
            Some(tool) => {
                let gc = crate::git_caveats::GitCaveats::from_session(caveats);
                let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
                print_tool_call("git", op, color);
                let out = match tool.dispatch(op, args, &gc) {
                    Ok(rendered) => rendered,
                    // Denials + engine errors surface verbatim so the model
                    // sees WHY (e.g. "denied: commit" on a read-only session).
                    Err(e) => format!("error: {e}"),
                };
                print_tool_output(&out, tool_output_lines, color);
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
                print_tool_call(name, &args.to_string(), color);
                let out = match runner.dispatch(name, args, caveats).await {
                    Ok(rendered) => rendered,
                    Err(e) => format!("error: {e}"),
                };
                print_tool_output(&out, tool_output_lines, color);
                out
            }
            None => format!("unknown tool: {name} (no crew surface in this session)"),
        },

        "run_command" => {
            let cmd = args["command"].as_str().unwrap_or("");

            // Corrective guard: the model tried to call a tool as a shell binary.
            // Return a correction so the model can retry with the right tool call.
            if let Some(tool) = DIRECT_TOOL_NAMES
                .iter()
                .copied()
                .find(|t| cmd.split_ascii_whitespace().next() == Some(*t))
            {
                return format!(
                    "error: '{tool}' is a tool, not a shell command. \
                     Call it as a separate tool invocation — \
                     do not pass '{tool}' as a command argument to run_command."
                );
            }

            print_tool_call("run_command", cmd, color);

            // Route the WHOLE command through agent-bridle's confined shell
            // (free-form `cmd` mode) under the SAME Caveats the TUI resolved
            // from `[tui].permissions`. `caveats` is `crate::caveats::Caveats`,
            // a re-export of `agent_mesh_protocol::caveats::Caveats` — the exact
            // type `Registry::dispatch` expects, so no conversion is needed.
            //
            // Inject venv env vars if active: the confined shell does not inherit
            // the host environment, so we prepend export statements to the cmd.
            let cmd_with_venv = match venv_cmd_prefix() {
                Some(prefix) => format!("{prefix}{cmd}"),
                None => cmd.to_string(),
            };

            // INTERIM (#297): --disable-ocap / --yolo / NEWT_DISABLE_OCAP=1 —
            // run the command UNCONFINED on the host shell instead of the
            // bridle's confined shell. Same venv/PATH prefix, same envelope
            // shape, same output formatting. Nothing is denied here, so the
            // #263 permission gate below is never consulted — the issue's
            // precedence rule (`--disable-ocap` > `--prompt-for-permissions`
            // for exec) falls out structurally. The fs tools below are NOT
            // bypassed: yolo is unconfined exec, fenced fs.
            //
            // #307 FLOOR: a named-permission-preset clamp WINS over the bypass.
            // When `exec_floor` is `Some`, the unconfined host path is taken
            // ONLY if the floor permits this command's leading token. An
            // out-of-floor command (e.g. `rm` under a readonly triage preset)
            // falls through to the confined shell below, which enforces the
            // already-clamped `caveats` and denies it — so `--yolo` can never
            // raise authority above the active preset. `None` (no preset) keeps
            // the bypass bit-for-bit.
            if ocap_disabled() && exec_floor_permits(exec_floor, cmd) {
                return match host_shell_dispatch(&cmd_with_venv, workspace).await {
                    Ok(envelope) => shell_envelope_output(&envelope, tool_output_lines, color),
                    Err(e) => format!("error: {e}"),
                };
            }

            let dispatch_args = serde_json::json!({
                "cmd": cmd_with_venv,
                "cwd": workspace,
            });
            match agent_bridle::registry()
                .dispatch("shell", dispatch_args.clone(), caveats)
                .await
            {
                // The confined shell ran. Its envelope carries
                // `{ exit_code, stdout, stderr, timed_out, ... }` plus — when the
                // leash refused a capability — the STRUCTURED denial fields
                // `{ denied: true, denials: [{ kind, target, reason }] }`. In
                // free-form mode an out-of-scope command is denied *inside* the
                // shell by the brush interceptor (the command genuinely does not
                // run); we lift that to the existing capability-denied UX by
                // reading the structured `denied` field — NEVER a stderr grep.
                Ok(envelope) if envelope_denied(&envelope) => {
                    // #263: an interactive gate may turn this denial into a
                    // human grant. ONE consult + ONE re-execution per call: a
                    // second denial (a different target reached on the re-run)
                    // surfaces as the standard envelope — the model can retry,
                    // which prompts afresh for the new target.
                    if let Some(gate) = permission_gate {
                        if let Some(requests) = exec_denial_requests(&envelope) {
                            if let PermissionDecision::Allow(widened) = gate.ask(&requests) {
                                return match agent_bridle::registry()
                                    .dispatch("shell", dispatch_args, &widened)
                                    .await
                                {
                                    Ok(env2) if envelope_denied(&env2) => {
                                        denied_run_command_result(&env2, color)
                                    }
                                    Ok(env2) => {
                                        shell_envelope_output(&env2, tool_output_lines, color)
                                    }
                                    Err(e) => format!("error: {e}"),
                                };
                            }
                        }
                    }
                    denied_run_command_result(&envelope, color)
                }
                Ok(envelope) => shell_envelope_output(&envelope, tool_output_lines, color),
                // An argv-mode leash denial, or an error from inside the tool —
                // surface the reason; the dispatch error Display is safe to show.
                Err(e) => format!("error: {e}"),
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
                    let msg = format!("capability denied: fs_read does not permit '{path}'");
                    print_denied("fs_read", path, color);
                    return msg;
                }
            }
            print_tool_call("read_file", path, color);
            match std::fs::read_to_string(&full) {
                Ok(contents) => {
                    // #719: window + cap the MODEL-facing payload (the on-screen
                    // display is capped separately) so one read of a large file
                    // can't saturate the context window and abandon the task.
                    let offset = args["offset"].as_u64().map(|n| n as usize);
                    let limit = args["limit"].as_u64().map(|n| n as usize);
                    let out = paginate_read(&contents, offset, limit);
                    print_tool_output(&out, tool_output_lines, color);
                    out
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
                let allowed = permission_gate.is_some_and(|gate| {
                    fs_gate_allows(gate, "write_file", DenialKind::FsWrite, &full_str, |c| {
                        &c.fs_write
                    })
                });
                if !allowed {
                    let msg = format!("capability denied: fs_write does not permit '{path}'");
                    print_denied("fs_write", path, color);
                    return msg;
                }
            }

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
                    print_denied("shrink-guard", path, color);
                    return msg;
                }
            }

            print_tool_call(
                "write_file",
                &format!("{path} ({} bytes)", content.len()),
                color,
            );

            // Show first 20 lines as preview.
            let preview: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
            let has_more = content.lines().count() > 20;
            print_tool_output(
                &format!("{preview}{}", if has_more { "\n…" } else { "" }),
                tool_output_lines,
                color,
            );

            // Auto-write when the caveat explicitly scopes fs_write (the
            // preset itself is the user's consent).  Ask y/N only under
            // full_access / custom where fs_write == Scope::All.
            let needs_confirm = matches!(caveats.fs_write, crate::caveats::Scope::All);

            let confirmed = if needs_confirm {
                print!("Write this file? [y/N] ");
                use std::io::Write as _;
                std::io::stdout().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer).is_ok()
                    && answer.trim().eq_ignore_ascii_case("y")
            } else {
                true
            };

            if confirmed {
                let full = std::path::Path::new(workspace).join(path);
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&full, content) {
                    Ok(_) => {
                        let line_count = content.lines().count();
                        println!("✓ wrote {path} ({line_count} lines)");
                        let check = build_check_cmd
                            .map(|cmd| run_build_check(cmd, workspace))
                            .unwrap_or_default();
                        format!("wrote {path} ({line_count} lines){check}")
                    }
                    Err(e) => format!("error writing {path}: {e}"),
                }
            } else {
                println!("skipped");
                format!("user declined to write {path}")
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
                    let msg = format!("capability denied: fs_write does not permit '{path}'");
                    print_denied("fs_write", path, color);
                    return msg;
                }
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
            let updated = existing.replacen(old_string, new_string, 1);
            let old_lines = existing.lines().count();
            let new_lines = updated.lines().count();
            let delta = new_lines as i64 - old_lines as i64;
            let delta_str = if delta >= 0 {
                format!("+{delta}")
            } else {
                format!("{delta}")
            };
            print_tool_call("edit_file", &format!("{path} ({delta_str} lines)"), color);
            match std::fs::write(&full, &updated) {
                Ok(_) => {
                    println!("✓ edited {path} ({delta_str} lines, now {new_lines} total)");
                    let check = build_check_cmd
                        .map(|cmd| run_build_check(cmd, workspace))
                        .unwrap_or_default();
                    format!("edited {path} ({delta_str} lines, now {new_lines} total){check}")
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
                    let msg = format!("capability denied: fs_read does not permit '{path}'");
                    print_denied("fs_read", path, color);
                    return msg;
                }
            }
            print_tool_call("list_dir", path, color);
            match std::fs::read_dir(&full) {
                Ok(entries) => {
                    let mut names: Vec<String> = entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    let listing = names.join("\n");
                    print_tool_output(&listing, tool_output_lines, color);
                    listing
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
                    let msg = format!("capability denied: fs_read does not permit '{path}'");
                    print_denied("fs_read", path, color);
                    return msg;
                }
            }
            let opts = FindOpts {
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
            };
            // #529: echo the active filters, not a bare `find: .` — two very
            // different searches must not render identically in the trace.
            print_tool_call("find", &find_detail(path, &opts), color);
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
                    let msg = format!("capability denied: fs_read does not permit '{path}'");
                    print_denied("fs_read", path, color);
                    return msg;
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
                    print_tool_output(&listing, tool_output_lines, color);
                    listing
                }
                Err(e) => format!("error: {e}"),
            }
        }

        "use_skill" => {
            let skill_name = args["name"].as_str().unwrap_or("");
            print_tool_call("use_skill", skill_name, color);
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
                Ok(body) => {
                    print_tool_output(&body, tool_output_lines, color);
                    body
                }
                Err(e) => format!("error: {e}"),
            }
        }

        "web_fetch" => {
            let url = args["url"].as_str().unwrap_or("");
            print_tool_call("web_fetch", url, color);

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
                    print_tool_output(&out, tool_output_lines, color);
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
                assert!(msg.contains("plan_set"), "guidance names the tool: {msg}");
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

    // ---- #719: read_file payload window/cap/pagination (pure, no fs) ----

    #[test]
    fn paginate_read_caps_a_large_file_to_the_default_window() {
        // A 15k-line file must NOT flood the model: default window is 2000 lines
        // with a footer to continue (regression for the 12.5k→168k saturation).
        let body: String = (1..=15_057)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = paginate_read(&body, None, None);
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
        let out = paginate_read(&body, Some(10), Some(5));
        assert!(out.starts_with("L10\nL11\nL12\nL13\nL14"), "{out:?}");
        assert!(out.contains("offset=15"), "continues at line 15: {out:?}");
    }

    #[test]
    fn paginate_read_small_file_is_returned_verbatim_without_a_footer() {
        // Whole-file read that fits both caps → exact bytes, no footer.
        assert_eq!(paginate_read("a\nb\nc\n", None, None), "a\nb\nc\n");
    }

    #[test]
    fn paginate_read_char_caps_a_pathological_long_line() {
        // One enormous line: the line window can't help; the char backstop must.
        let body = "x".repeat(MAX_READ_CHARS + 10_000);
        let out = paginate_read(&body, None, None);
        assert!(
            out.len() < MAX_READ_CHARS + 300,
            "char-capped: {} bytes",
            out.len()
        );
        assert!(out.contains("truncated"), "marks the truncation");
    }

    #[test]
    fn paginate_read_offset_past_end_is_a_clear_message() {
        let out = paginate_read("a\nb", Some(99), None);
        assert!(out.contains("past end"), "{out:?}");
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
        };
        assert_eq!(find_detail(".", &opts), ". (type=d)");
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
                "list_dir",
                "find",
                "use_skill",
                "web_fetch",
                // #714: advertised ALWAYS (no presence gate), so it joins the
                // base set even with every `with_*` flag off.
                "resume_context",
            ]
        );
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
            "use_skill",
            "web_fetch",
            "save_note",
            "recall",
        ] {
            assert!(!is_hallucination(t, &serde_json::json!({"path": "."})));
        }
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
    fn extra_exec_hint_only_for_exec_denials_with_targets() {
        assert!(extra_exec_hint(&serde_json::json!({})).is_none());
        assert!(extra_exec_hint(&serde_json::json!({"denials": []})).is_none());
        // Non-exec kinds never produce the exec escape-hatch hint.
        let open_only = serde_json::json!({
            "denials": [{"kind": "open", "target": "/x", "reason": "r"}]
        });
        assert!(extra_exec_hint(&open_only).is_none());
        // Empty target → no hint.
        let empty_target = serde_json::json!({
            "denials": [{"kind": "exec", "target": "", "reason": "r"}]
        });
        assert!(extra_exec_hint(&empty_target).is_none());
        // The happy path names the command in the TOML snippet.
        let exec = serde_json::json!({
            "denials": [{"kind": "exec", "target": "env", "reason": "r"}]
        });
        assert_eq!(
            extra_exec_hint(&exec).unwrap(),
            "add it via [tui.permissions] extra_exec = [\"env\"] in your newt config"
        );
    }

    #[test]
    fn exec_allowlist_name_takes_basename() {
        assert_eq!(exec_allowlist_name("env"), "env");
        assert_eq!(exec_allowlist_name("/usr/bin/env"), "env");
        assert_eq!(exec_allowlist_name("/usr/bin/"), "bin");
        assert_eq!(exec_allowlist_name("C:\\tools\\env.exe"), "env.exe");
    }

    #[test]
    fn toml_string_literal_escapes_backslash_and_quote() {
        assert_eq!(toml_string_literal("plain"), "plain");
        assert_eq!(toml_string_literal(r#"a"b"#), r#"a\"b"#);
        assert_eq!(toml_string_literal(r"a\b"), r"a\\b");
    }

    #[test]
    fn exec_denial_guidance_escapes_toml_literal() {
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [
                {
                    "kind": "exec",
                    "target": "bad\"cmd",
                    "reason": "exec bad command denied"
                }
            ]
        });
        let reason = envelope_denial_reason_with_guidance(&envelope);
        assert!(reason.contains("[tui.permissions] extra_exec = [\"bad\\\"cmd\"]"));
    }

    #[test]
    fn exec_denial_guidance_uses_command_name_for_absolute_paths() {
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [
                {
                    "kind": "exec",
                    "target": "/usr/bin/env",
                    "reason": "exec of \"/usr/bin/env\" is not within the granted authority"
                }
            ]
        });
        let reason = envelope_denial_reason_with_guidance(&envelope);
        assert!(reason.contains("[tui.permissions] extra_exec = [\"env\"]"));
        assert!(!reason.contains("extra_exec = [\"/usr/bin/env\"]"));
    }

    #[test]
    fn exec_denial_guidance_uses_command_name_for_windows_paths() {
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [
                {
                    "kind": "exec",
                    "target": "C:\\tools\\env.exe",
                    "reason": "exec of \"C:\\tools\\env.exe\" is not within the granted authority"
                }
            ]
        });
        let reason = envelope_denial_reason_with_guidance(&envelope);
        assert!(reason.contains("[tui.permissions] extra_exec = [\"env.exe\"]"));
        assert!(!reason.contains("extra_exec = [\"C:\\\\tools\\\\env.exe\"]"));
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
        // Step 26.6b (#586): the scheduled plan_set/plan_advance tools, only with the gate.
        let sched = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, true,
        );
        for t in ["plan_set", "plan_advance", "plan_get"] {
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
        let args = serde_json::json!({ "steps": ["a", "b"] });
        // Step 26.6b / #716: no ledger → unknown tool for ALL plan arms (presence
        // -gate parity, including the read-only plan_get).
        for name in ["plan_set", "plan_advance", "plan_get"] {
            let out = execute_tool(
                name, &args, ".", false, 20, &caveats, &mut NoMcp, None, None, None, None, None,
                None, None, None, None, None, None, None,
            )
            .await;
            assert!(out.starts_with(&format!("unknown tool: {name}")), "{out}");
        }
        // with a ledger → plan_set routes to the executor and mutates it.
        let ledger = SessionStepLedger::default();
        let out = execute_tool(
            "plan_set",
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
        assert_eq!(out, "plan set: 2 steps");
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
    use crate::caveats::{Caveats, CountBound, Scope};

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
                other => Err(format!("unknown git op '{other}'")),
            }
        }
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

    #[tokio::test]
    async fn crew_arm_without_injection_is_unknown_tool() {
        let out = run_crew_tool("crew", serde_json::json!({ "task": "x" }), None).await;
        assert!(out.contains("unknown tool: crew"), "got: {out}");
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
    }

    #[test]
    fn alias_passes_through_real_and_mcp_names() {
        for n in [
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "git",
            "plan_set",
            "plan_advance",
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
    fn alias_corrects_plan_names_to_plan_set() {
        for n in [
            "enter_plan",
            "enter_plan_mode",
            "plan_mode",
            "start_plan",
            "begin_plan",
            "make_plan",
            "create_plan",
            "plan",
            "planning",
            "update_plan",
            "set_plan",
            "todo",
            "todos",
            "todo_write",
        ] {
            let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
                panic!("{n} should produce a Correct outcome");
            };
            assert!(msg.contains("plan_set"), "{n}: {msg}");
            assert!(msg.contains("plan_advance"), "{n}: {msg}");
        }
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
            assert!(msg.contains("plan_set"), "{n}: {msg}");
            assert!(msg.contains("plan_advance"), "{n}: {msg}");
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

    /// FLAG OFF (no gate): the denial text is the exact string the model has
    /// always received — regression-pinned bit-for-bit (#263 acceptance).
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
        assert_eq!(
            out,
            "capability denied: fs_read does not permit 'secret.txt'"
        );
        let out = run_tool(
            "list_dir",
            serde_json::json!({"path": "."}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(out, "capability denied: fs_read does not permit '.'");
        let out = run_tool(
            "write_file",
            serde_json::json!({"path": "a.txt", "content": "c"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(out, "capability denied: fs_write does not permit 'a.txt'");
        let out = run_tool(
            "edit_file",
            serde_json::json!({"path": "a.txt", "old_string": "a", "new_string": "b"}),
            ws.path(),
            &denied,
            None,
        )
        .await;
        assert_eq!(out, "capability denied: fs_write does not permit 'a.txt'");
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
        assert_eq!(
            gated,
            "capability denied: fs_read does not permit 'secret.txt'"
        );
        assert_eq!(gate.asks.len(), 1, "the human was asked exactly once");
    }

    /// Gate allows fs_write denials → write_file and edit_file proceed.
    #[tokio::test]
    async fn gate_allow_turns_fs_write_denials_into_real_writes() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("f.txt"), "old\n").unwrap();
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
        assert_eq!(gate.asks.len(), 2);
        assert_eq!(gate.asks[0].0, "write_file");
        assert!(
            gate.asks[1].1.starts_with("fs_write:"),
            "got: {:?}",
            gate.asks[1]
        );
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
        assert_eq!(
            out,
            "capability denied: fs_read does not permit 'secret.txt'"
        );
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

    /// FLAG OFF = bit-for-bit current behavior, pinned. On this stub-shell
    /// build (the publishable configuration, see the [patch.crates-io] note)
    /// the bridle dispatch fails closed with the tracking-issue error for
    /// EVERY command — exactly the operator-reported breakage #297 hatches
    /// around. Restore-from-history note: like the newt-tui confinement
    /// tests, this assertion changes when the real brush shell returns.
    #[tokio::test]
    async fn flag_off_run_command_keeps_the_confined_dispatch_verbatim() {
        let _l = env_lock().await;
        let _off = EnvVar::unset("NEWT_DISABLE_OCAP");
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_no_exec(ws.path());
        let out = run_tool(
            "run_command",
            serde_json::json!({"command": "echo hi"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert!(out.starts_with("error:"), "got: {out}");
        assert!(
            out.contains("unavailable in this build"),
            "the stub dispatch error must surface unchanged, got: {out}"
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
    /// dispatch, which on this stub-shell build fails closed. A deliberately
    /// restricted triage mode is NOT un-clamped by `--yolo`.
    #[tokio::test]
    async fn floor_blocks_disable_ocap_for_a_denied_exec() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
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
        // we see the confined-dispatch error, not `should-not-run\n`.
        assert_ne!(out, "should-not-run\n", "the floor must block the bypass");
        assert!(
            out.starts_with("error:"),
            "fell to confined dispatch: {out}"
        );
        assert!(
            out.contains("unavailable in this build"),
            "confined stub error surfaces, got: {out}"
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
    /// shell (stub ⇒ error), which gates each spawn.
    #[tokio::test]
    async fn floor_refuses_bypass_for_a_compound_command() {
        let _l = env_lock().await;
        let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
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
        assert!(
            out.starts_with("error:"),
            "fell to confined dispatch: {out}"
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
        assert_eq!(shell_envelope_output(&envelope, 20, false), "out\nerr\n");
    }

    /// The venv/PATH prefix logic rides the host shell unchanged: the same
    /// `export VIRTUAL_ENV=…; export PATH=…;` prefix the confined shell got
    /// is prepended to the bypassed command.
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
        assert_eq!(
            out,
            format!("capability denied: fs_write does not permit '{escape}'")
        );
        assert!(!std::path::Path::new(escape).exists());

        let out = run_tool(
            "read_file",
            serde_json::json!({"path": "/etc/hostname"}),
            ws.path(),
            &caveats,
        )
        .await;
        assert_eq!(
            out,
            "capability denied: fs_read does not permit '/etc/hostname'"
        );
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
}
