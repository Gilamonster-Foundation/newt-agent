use super::super::prompt_intake::PromptDisposition;
use super::*;
use std::sync::LazyLock;

pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command in the workspace directory and return its output. \
                                Runs in a CONFINED shell: stream redirects to a target outside your \
                                fs_write scope are DENIED (e.g. `2>/dev/null`, `> /dev/null`) — drop \
                                the redirect and read stdout/stderr from the result instead. Prefer the \
                                dedicated tools over shelling out: `find`/`read_file`/`list_dir` over \
                                `find`/`cat`/`ls`, the `git` tool over `git`, and `lifecycle` over raw \
                                build/test/lint commands. Do NOT pass `git` (or another tool's name) as \
                                the command here — `git` is a separate tool; invoke it directly. Shelling \
                                out to a name that has a dedicated tool is rejected.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to run" },
                        "cwd": { "type": "string", "description": "Optional working directory for this command, relative to the workspace root (e.g. \"crates/foo\") or an absolute path inside it. Confined to the workspace. Prefer this over `cd` (which the confined shell rejects)." }
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
                "name": "delete_file",
                "description": "Delete one file in the workspace. Use this when a file should be \
                                removed entirely; it is governed by the same fs_write permission \
                                and operator prompt path as write_file and edit_file. Refuses \
                                directories, missing files, and paths outside the granted write scope.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to workspace root" }
                    },
                    "required": ["path"]
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
                "description": "Find files and directories under the workspace recursively, WITHOUT a shell. Returns relative paths, one per line, already sorted. Respects .gitignore and skips noise by default. For repository code investigation, use category=source by default; this harness-owned category follows the resolved language-pack registry instead of treating docs, manifests, locks, or generated artifacts as code. Use category=any only when the operator requests those artifacts or a full-tree search. Set language to narrow source evidence by a configured name/alias. For top-N size use sort=size + show_size; for top-N line count use sort=lines + show_lines — no pipeline needed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory to search under, relative to workspace root. Default '.' (the whole workspace)." },
                        "name": { "type": "string", "description": "Glob matched against each entry's basename, e.g. '*.py' or 'pyo3_module.rs'. '*' matches any run, '?' any single char. Omit to match everything." },
                        "type": { "type": "string", "enum": ["f", "d", "any"], "description": "Restrict to files ('f'), directories ('d'), or both ('any', the default)." },
                        "category": { "type": "string", "enum": ["any", "source"], "description": "Semantic file category. Use 'source' by default for repository code investigation; it filters through the configured language-pack registry and excludes repository metadata. Use 'any' for explicitly requested non-source artifacts or full-tree searches. Omitted preserves historical unfiltered behavior." },
                        "language": { "type": "string", "description": "Optional source-language name or alias (case-insensitive), e.g. Rust, Python, TypeScript/ts, Java, C++/cpp, C#/csharp/dotnet, Ruby/rb, bash/shell. Implies category='source'. Project language-pack aliases work too." },
                        "max_depth": { "type": "integer", "description": "Maximum directory depth below `path` (1 = immediate children only). Omit for unlimited." },
                        "max_results": { "type": "integer", "description": "Cap on the number of matches returned. Default 1000; output notes when truncated. Use 10 for 'top 10' rankings." },
                        "respect_gitignore": { "type": "boolean", "description": "When true (default) skip .gitignored paths plus .git/target/node_modules/hidden dirs. Set false to search everything." },
                        "case_sensitive": { "type": "boolean", "description": "Case-sensitive basename match. Default true." },
                        "code": { "type": "boolean", "description": "Backward-compatible alias for category='source'. When true, keep only files recognized by the language-pack registry and exclude repository metadata. Implies files only. Default false." },
                        "sort": { "type": "string", "enum": ["name", "size", "lines"], "description": "Result order: 'name' (default, paths ascending), 'size' (byte size descending) or 'lines' (line count descending). Combine with max_results for the top-N and show_size/show_lines to see the metric." },
                        "show_size": { "type": "boolean", "description": "Prefix each result with its byte size and a tab ('<size>\\t<path>'). Default false. Use with sort='size' to answer 'largest files' questions without a shell." },
                        "show_lines": { "type": "boolean", "description": "Prefix each result with its line count and a tab ('<lines>\\t<path>'). Default false. Use with sort='lines' to answer 'files with the most lines' questions without a shell (no `wc -l`). Line mode wins over show_size when both are set." }
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
        },
        {
            "type": "function",
            "function": {
                "name": "request_permissions",
                "description": "Ask the operator to GRANT a capability you were denied — the \
                                capability-grant path (#721). Call this AFTER a `capability denied` \
                                result to request authority you don't currently have. If an operator \
                                is present they may allow it; then retry the original operation. In a \
                                headless session (no operator) you'll be told the capability must be \
                                configured by the owner — change approach. This requests AUTHORITY \
                                (it mints a capability grant); it is NOT a way to ask the user a \
                                free-text question.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "capability": { "type": "string", "enum": ["exec", "fs_read", "fs_write", "net"], "description": "Which capability axis to request" },
                        "target": { "type": "string", "description": "What to grant: a command name (exec), a path (fs_read/fs_write), or a host (net)" },
                        "reason": { "type": "string", "description": "Why you need it — shown to the operator deciding" }
                    },
                    "required": ["capability", "target", "reason"]
                }
            }
        }
    ])
}

/// #728: always-advertised `request_user_input` tool definition — the GENERIC
/// ask-the-human primitive. Pushed unconditionally in [`merged_tool_definitions`]
/// like `resume_context` / `tool_search` / `get_context_remaining`: a model must
/// always be able to ask the human a question, and it degrades honestly headless
/// (the executor answers "no human available this session" when no interactive
/// gate is present). Question-only for v1 — the multiple-choice `options` hint is
/// deferred so no advertised arg is left unused.
fn request_user_input_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "request_user_input",
            "description": "Ask the human operator a free-text question and get \
                            their answer. Use this to resolve genuine ambiguity \
                            instead of guessing or narrating (e.g. 'which database \
                            should I target?', 'is this the file you meant?'). This \
                            asks for INFORMATION, not authority — to request a \
                            capability you were denied, use request_permissions \
                            instead. In a headless session (no operator) you'll be \
                            told no human is available — then proceed with your best \
                            judgment and state your assumption explicitly.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The free-text question to ask the human" }
                },
                "required": ["question"]
            }
        }
    })
}

/// The `lifecycle` tool (#891) — the model-facing surface over the data-driven
/// lifecycle system (#880). Instead of guessing a raw shell command, the model
/// names a phase and newt runs THIS repo's resolved command for it
/// ([`crate::tooling::resolved_phase_commands`]): the `.newt/config.toml`
/// `[lifecycle]` override, else the matching tooling packs (Rust / Python /
/// PyO3 / Go / drop-in / custom). Advertised ALWAYS — like `resume_context`,
/// it degrades honestly (a phase with no configured command returns a clear
/// "no command configured", not an error), so it needs no presence gate. The
/// phase-name enum is built from [`crate::tooling::Phase::ALL`] so the schema
/// can never drift from the vocabulary.
///
/// #1972: `dir` resolves detection AND execution against a subdirectory,
/// through the SAME `resolve_exec_cwd` seam `run_command`'s `cwd` uses — a
/// polyglot/nested-project workspace (e.g. `agent-voice/Cargo.toml` under a
/// workspace root with no root-level markers) is no longer structurally
/// invisible to this tool. When root detection finds nothing, the executor
/// names any nested project it DID find instead of silently no-op'ing
/// (`crate::tooling::unconfigured_phase_message`).
pub fn lifecycle_tool_definition() -> serde_json::Value {
    let phases: Vec<&str> = crate::tooling::Phase::ALL
        .iter()
        .map(|p| p.as_str())
        .collect();
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "lifecycle",
            "description": "Run a named project lifecycle phase using THIS repo's \
                configured command instead of guessing a raw shell command. \
                Phases: setup (resolve deps / prepare a checkout), format \
                (auto-format the tree), lint (static analysis), test (run the \
                tests), check (the full gate a change must pass), clean (remove \
                build artifacts). The command is resolved from \
                .newt/config.toml [lifecycle] and the repo's tooling packs \
                (Rust / Python / PyO3 / Go / custom), so `check` runs the RIGHT \
                gate for this project. Prefer this over run_command for build / \
                test / format / lint / check work so the project's own \
                conventions are honored uniformly across build systems. Use \
                action=list to see the resolved command without running it. \
                In a polyglot/monorepo workspace, pass `dir` to target a \
                nested project directly — if you omit it and nothing is \
                configured at the workspace root, the response names any \
                nested project it found so you can retry with `dir` set.",
            "parameters": {
                "type": "object",
                "properties": {
                    "phase": {
                        "type": "string",
                        "enum": phases,
                        "description": "The lifecycle phase to run."
                    },
                    "action": {
                        "type": "string",
                        "enum": ["run", "list"],
                        "description": "run (default) executes the phase's resolved command; \
                                        list returns the command without running it."
                    },
                    "dir": {
                        "type": "string",
                        "description": "Optional subdirectory to resolve and run the phase in, \
                            relative to the workspace root (e.g. \"agent-voice\") or an absolute \
                            path inside it. Confined to the workspace. Use this for a nested \
                            project the workspace root itself has no lifecycle command for."
                    }
                },
                "required": ["phase"]
            }
        }
    })
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
    with_operating_mode_control: bool,
    with_plan_mode_control: bool,
    with_plan_mode_active: bool,
) -> serde_json::Value {
    let mut defs = match tool_definitions() {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    // #894: everything after the base array is [`EXTENDED_TOOL_REGISTRY`],
    // advertised in declaration order when its gate is satisfied. The always-on
    // tools (resume_context #714 / prompt_read / artifact_read / tool_search
    // #725 / get_context_remaining #727 / request_user_input #728 / lifecycle
    // #891) carry `Gate::Always` and ride every session, degrading honestly
    // when their backing source is `None`; the
    // rest are gated on the matching injected `with_*` capability (git PR4,
    // compose_roster+crew #479, scratchpad #583, code_search #582, experiential
    // #585, scheduled #586/#715/#716). Adding a tool is one `ToolSpec`, which
    // updates this listing AND `ALL_TOOL_NAMES` together — no more drift.
    for spec in EXTENDED_TOOL_REGISTRY {
        if gate_satisfied(
            spec.gate,
            with_save_note,
            with_recall,
            with_memory_fetch,
            with_git,
            with_team,
            with_scratchpad,
            with_code_search,
            with_experiential,
            with_scheduled,
            with_operating_mode_control,
            with_plan_mode_control,
            with_plan_mode_active,
        ) {
            defs.push((spec.definition)());
        }
    }
    // MCP `_meta` is connector-side catalog metadata, not part of either
    // inference provider's function-tool schema. Recovery consumes it from the
    // raw `McpTools::tool_defs()` catalog before this merge; scrub it from every
    // model-facing definition while retaining the callable tool itself.
    let mut mcp_defs = mcp.tool_defs();
    for definition in &mut mcp_defs {
        super::strip_mcp_catalog_metadata(definition);
    }
    defs.extend(mcp_defs);
    serde_json::Value::Array(defs)
}

/// Tools a persona allow-list cannot fence off: always-on loop infrastructure
/// plus the presence-gated operating/Plan mode controls. These are session
/// controls, not task authority; hiding an exit could strand the session in a
/// read-only style, while exposing them still cannot widen human authority.
fn is_persona_unfenceable_tool(name: &str) -> bool {
    EXTENDED_TOOL_REGISTRY.iter().any(|spec| {
        matches!(
            spec.gate,
            Gate::Always | Gate::OperatingMode | Gate::PlanMode | Gate::ScheduledPlanMode
        ) && spec.name == name
    })
}

/// FR-1 part 2 (#997): is `name` callable under a persona whose `tools:`
/// front-matter is `allow`? True when the persona names it, OR it is an
/// always-on infra tool the loop can't run without. This is the single
/// predicate behind BOTH the advertise-filter ([`filter_advertised_tools`])
/// and the executor reject ([`execute_tool_with_offload`]) so the set the model
/// SEES and the set it may RUN can never drift apart.
///
/// `pub` (not `pub(crate)`) since #1021 PR 5.2: a headless entry point
/// (`newt-mcp-server`) filters its own, separately-built catalog by the same
/// rule rather than reimplementing it.
pub fn persona_tool_allowed(name: &str, allow: &[String]) -> bool {
    allow.iter().any(|t| t == name) || is_persona_unfenceable_tool(name)
}

/// The advertised name of one tool definition (`{"function":{"name":…}}`), or
/// `None` for a shape without one (kept rather than dropped by the filter).
fn tool_def_name(def: &serde_json::Value) -> Option<&str> {
    def.get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
}

/// FR-1 part 2 (#997): restrict an advertised catalog to a persona's allow-list.
/// `allow = None` (no persona / no `tools:` list) returns `defs` untouched — the
/// zero-cost path for every non-persona session. When `Some`, keep only the
/// tools [`persona_tool_allowed`] admits (the persona's names ∪ the always-on
/// infra). Pure over `serde_json::Value`; the caller wraps
/// [`merged_tool_definitions`] with it at each catalog site.
///
/// `pub` (not `pub(crate)`) since #1021 PR 5.2: a headless entry point
/// (`newt-mcp-server`) filters its own, separately-built catalog by the same
/// rule rather than reimplementing it.
pub fn filter_advertised_tools(
    defs: serde_json::Value,
    allow: Option<&[String]>,
) -> serde_json::Value {
    let Some(allow) = allow else { return defs };
    let serde_json::Value::Array(arr) = defs else {
        return defs;
    };
    serde_json::Value::Array(
        arr.into_iter()
            .filter(|def| match tool_def_name(def) {
                Some(name) => persona_tool_allowed(name, allow),
                None => true,
            })
            .collect(),
    )
}

/// Whether `name` is available under the prompt's validated disposition.
///
/// `Act` retains the complete catalog. `Explain`, `Research`, and `Plan` are
/// deliberately a small, explicit read/recovery set (`Plan` additionally gets
/// the harness-owned ledger writer): an unknown name is denied rather than
/// assumed safe, which also fences every generic MCP name (`server__tool`)
/// until MCP supplies machine-readable authority metadata. `Ask` is terminal
/// at the harness layer, so no model tool invocation is admitted as defense in
/// depth.
///
/// This predicate is shared by advertisement and dispatch. The latter remains
/// the security boundary: a model can always fabricate an omitted tool name.
pub fn tool_allowed(disposition: PromptDisposition, name: &str) -> bool {
    match disposition {
        PromptDisposition::Act => true,
        PromptDisposition::Ask => false,
        PromptDisposition::Explain | PromptDisposition::Research => {
            common_read_only_tool_allowed(name)
        }
        PromptDisposition::Plan => {
            // Human/model Plan mode is deliberately offline. Do not advertise
            // `web_fetch` when the matching plan caveat always denies network.
            (name != "web_fetch" && common_read_only_tool_allowed(name))
                // `update_plan` changes only the harness-owned session ledger
                // (and its derived audit artifact), never workspace or external
                // state. `exit_plan_mode` removes only the model-entered
                // self-clamp; it cannot override a human `/mode plan` or the
                // session's underlying caveats.
                || matches!(name, "update_plan" | "exit_plan_mode")
        }
    }
}

fn common_read_only_tool_allowed(name: &str) -> bool {
    matches!(
        name,
        // Workspace / prompt / artifact recovery.
        "read_file"
                | "list_dir"
                | "find"
                | "prompt_read"
                | "artifact_read"
                | "resume_context"
                // Read-only evidence and memory retrieval. `web_fetch` remains
                // subject to the existing net caveat; dispatch removes the
                // permission gate in non-Act modes so it cannot mint a grant.
                | "web_fetch"
                | "use_skill"
                | "recall"
                | "memory_fetch"
                | "state_get"
                | "code_search"
                | "where_is"
                | "experience_recall"
                | "plan_get"
                // Read-only harness utilities / presentation.
                | "tool_search"
                | "get_context_remaining"
                | "render_report"
                // Auto-mode selection changes only a future turn's working
                // style. It cannot change the current disposition, caveats, or
                // permissions, so a read-only turn may safely schedule its
                // successor without widening itself.
                | "select_operating_mode"
                // #1259: the formal ask-the-human escalation. An evidence turn
                // that is genuinely boxed in (no capable tool) ends as a
                // legitimate QUESTION instead of penalized narration — the
                // #1257 double-bind. Free-text Q&A only: `request_permissions`
                // stays excluded (evidence turns must not mint capability
                // grants). Its dedicated question seam cannot mint caveats or
                // widen the accepted turn; an absent interactive gate still
                // returns a recoverable no-human message without hanging.
                | "request_user_input"
    )
}

/// Restrict an advertised tool catalog to the current prompt disposition.
///
/// Apply this alongside [`filter_advertised_tools`]: the persona filter scopes
/// an operator-selected role, while this filter scopes the authority implied by
/// the current prompt. Under a non-Act disposition, malformed definitions are
/// dropped too because their callable name cannot be proven safe.
pub fn filter_tools_for_disposition(
    defs: serde_json::Value,
    disposition: PromptDisposition,
) -> serde_json::Value {
    if disposition == PromptDisposition::Act {
        return defs;
    }
    let serde_json::Value::Array(arr) = defs else {
        // Unlike the persona filter, non-Act disposition filtering is an
        // authority boundary. A non-array value cannot prove any callable
        // name safe, so advertise no tools rather than forwarding it.
        return serde_json::Value::Array(Vec::new());
    };
    serde_json::Value::Array(
        arr.into_iter()
            .filter(|def| tool_def_name(def).is_some_and(|name| tool_allowed(disposition, name)))
            .collect(),
    )
}

/// The refusal returned when a model calls a tool outside the current prompt
/// disposition. Kept distinct from a persona refusal: changing personas cannot
/// widen a non-Act prompt into an execution turn.
pub(super) fn disposition_tool_denied_message(
    disposition: PromptDisposition,
    name: &str,
) -> String {
    // #2051: the guidance text lives in `disposition_voice`, the single owner
    // of the disposition vocabulary. This string used to open "This is an
    // Explain turn", which is the sentence a 9b model read back to the
    // operator; the voice table both drops that framing and appends the
    // non-narration clause.
    debug_assert!(
        disposition != PromptDisposition::Act,
        "Act permits every tool and must never reach a disposition refusal"
    );
    let guidance = super::super::DispositionVoices::default().denied_block(disposition);
    format!("Tool `{name}` is unavailable under the current prompt disposition. {guidance}")
}

/// The refusal returned to the model when it calls a tool the active persona
/// does not grant (FR-1 part 2, #997). Names the tool and points at the escape
/// hatch, so the model self-corrects to a granted tool instead of looping.
pub(super) fn persona_tool_denied_message(name: &str) -> String {
    format!(
        "Tool `{name}` is not available under the active persona: its `tools:` \
         front-matter restricts which tools it may call. Choose one of the \
         granted tools, or clear the persona (`/persona clear`) if broader \
         access is genuinely required."
    )
}

/// Direct tool names the model must call as tool invocations, never as shell
/// commands passed to `run_command`.
const DIRECT_TOOL_NAMES: &[&str] = &[
    "list_dir",
    "read_file",
    "write_file",
    "edit_file",
    "delete_file",
    "use_skill",
    "web_fetch",
    // #496: `find …` typed at run_command redirects to the embedded `find`
    // tool — which works even when the shell is unavailable in this build.
    "find",
    // PR4: `git …` typed at run_command redirects to the embedded `git` tool —
    // but ONLY its built-in-served local ops; passthrough ops fall through (see
    // [`GIT_PASSTHROUGH_SUBCOMMANDS`] / [`run_command_redirect`], #898/#1022).
    "git",
];

/// #898/#1022: git subcommands that must pass through to the shell. The embedded `git` tool
/// (newt-git) is LOCAL-ONLY — `clone`/`fetch`/`push` are deferred — so if
/// run_command bounced *every* `git …` back to that pushless tool, a model
/// could never push a branch (and then never see the "Create a pull request …
/// by visiting: <URL>" line git prints, and never open a PR — issue #898). `rm`
/// also passes through because `git rm` has index semantics that plain
/// `delete_file` cannot reproduce (#1022). These ops are therefore allowed to
/// fall through to the confined shell, where the `exec` / `net` / fs leashes
/// still apply. Local read/edit ops (`status`/`log`/`diff`/`add`/`commit`/…)
/// keep redirecting to the embedded tool when it can serve them.
const GIT_PASSTHROUGH_SUBCOMMANDS: &[&str] = &["push", "fetch", "pull", "clone", "rm"];

/// Shell composition/metacharacter markers (#1262): a command containing any of
/// these is a real SHELL program — pipes, redirects, sequencing, substitution —
/// that no embedded direct tool can serve. Pure data (three-Cs): extending what
/// counts as composition is a data edit.
const SHELL_COMPOSITION_MARKERS: &[&str] = &["|", ">", "<", ";", "&&", "||", "$(", "`", "&"];

/// Whether `command` uses shell composition ([`SHELL_COMPOSITION_MARKERS`]) —
/// in which case it must fall through to the confined shell as-is.
fn uses_shell_composition(command: &str) -> bool {
    SHELL_COMPOSITION_MARKERS
        .iter()
        .any(|m| command.contains(m))
}

/// Decide whether a `run_command` invocation is really a misdirected call to a
/// direct tool (`list_dir`/`read_file`/…/`git`), and if so which one — so the
/// executor can bounce it with a correction and [`is_hallucination`] can count
/// it. Returns `None` when the command should run in the shell as-is.
///
/// #1262: a command with shell COMPOSITION (`find … | xargs du | sort`,
/// `git status && git diff`, `find … > out.txt`) is never a misdirected tool
/// call — the embedded tools cannot serve pipes/redirects/sequencing, and
/// bouncing it produced false "hallucination corrected" counts for commands
/// that were exactly right (the diagnosed ornith:35b pipeline). Only a BARE
/// invocation redirects. Generalizes the [`GIT_PASSTHROUGH_SUBCOMMANDS`]
/// judgment ("the embedded tool can't serve this — fall through") to command
/// shape.
///
/// `git` is special (#898/#1022): only its built-in-served LOCAL ops redirect
/// to the embedded git tool; its passthrough ops
/// ([`GIT_PASSTHROUGH_SUBCOMMANDS`]) fall through so the model can actually
/// push a branch, run `git rm`, and read any PR-creation URL git prints.
pub(super) fn run_command_redirect(command: &str) -> Option<&'static str> {
    if uses_shell_composition(command) {
        return None;
    }
    let mut tokens = command.split_ascii_whitespace();
    let first = tokens.next().unwrap_or("");
    if first == "git" {
        let sub = tokens.next().unwrap_or("");
        if GIT_PASSTHROUGH_SUBCOMMANDS.contains(&sub) {
            return None;
        }
        return Some("git");
    }
    DIRECT_TOOL_NAMES.iter().copied().find(|&t| t == first)
}

/// Shell separators that sequence, pipeline, or redirect sub-commands. A
/// composed command is split on these so each sub-command is examined
/// independently. This is a deliberately COARSE over-split (e.g. `|` inside a
/// quoted `-m` message is split too) — the policy is fail-closed, so an
/// over-split can only ever *block* a commit attempt, never *allow* one through.
const SHELL_SEGMENT_SEPARATORS: &[char] = &['&', '|', ';', '>', '<', '`', '\n'];

/// Git GLOBAL options that consume the following token as their value, so the
/// real subcommand is the first non-option token AFTER skipping these. A bounded
/// git-global-option set used only to locate the subcommand — not a general git
/// or shell parser. The `=`-attached forms (`--git-dir=/p`) carry their value in
/// the same token, so they are NOT here (they are bare flags that skip one).
const GIT_GLOBAL_OPTS_TAKING_VALUE: &[&str] =
    &["-c", "-C", "--git-dir", "--work-tree", "--namespace"];

/// Whether `token` names the `git` binary — the bare name `git` OR a qualified
/// path ending in `/git` (the model often invokes `/usr/bin/git -C <repo> …`).
fn is_git_binary(token: &str) -> bool {
    token == "git" || token.ends_with("/git")
}

/// Whether `token` is a shell environment-assignment prefix (`NAME=VALUE`),
/// which may precede the git binary to forge commit identity
/// (`GIT_AUTHOR_NAME=… git commit …`).
fn is_env_assignment(token: &str) -> bool {
    let Some((name, _val)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && name.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_')
}

/// Find the git SUBCOMMAND for a `git` invocation given the tokens that follow
/// the binary: the first token that is neither a flag nor the value of a
/// value-taking global option. `None` if no subcommand is present.
fn git_subcommand_after_binary<'a>(tail: &'a [&'a str]) -> Option<&'a str> {
    let mut i = 0;
    while i < tail.len() {
        let tok = tail[i];
        if tok.starts_with('-') {
            i += if GIT_GLOBAL_OPTS_TAKING_VALUE.contains(&tok) {
                2
            } else {
                1
            };
            continue;
        }
        return Some(tok);
    }
    None
}

/// Shell `git` subcommands that CREATE a commit and therefore bypass
/// harness-managed attribution when run through `run_command` (the audit
/// set: `commit`, `merge`, `cherry-pick`, `revert`, `rebase`). Each can land
/// an unattributed Newt commit; the routable forms (`commit`, `amend`,
/// `rebase`) have a first-class embedded `git` tool op, and
/// `merge`/`cherry-pick`/`revert` have NO first-class Newt route and are
/// DENIED (the operator must run them directly, not via the agent's
/// `run_command`). See [`run_command_creates_shell_git_commit`].
const SHELL_GIT_COMMIT_SUBCOMMANDS: &[&str] =
    &["commit", "merge", "cherry-pick", "revert", "rebase"];

/// Flags that make a commit-producing subcommand NOT create a commit — the
/// abort/quit forms. `git rebase --abort`, `git cherry-pick --quit`,
/// `git revert --abort`, `git merge --abort`/`--quit` all back out of an
/// in-progress operation WITHOUT creating a commit, so they are HARMLESS and
/// preserved (fall through to the confined shell). `--skip` and `--continue`
/// are deliberately NOT here: both CONTINUE the operation and DO create
/// commits, so they stay blocked. (`--no-commit` for `merge`/`cherry-pick` is
/// a documented possible future refinement — it genuinely creates no commit —
/// but is rare and out of this narrow fix.)
const SHELL_GIT_ABORT_FLAGS: &[&str] = &["--abort", "--quit"];

/// Decide whether a `run_command` invocation would create a git COMMIT via the
/// shell `git` CLI — bypassing `LocalGitTool::finalize_commit_message` and
/// landing an unattributed commit. [`run_command_redirect`] already bounces a
/// BARE `git commit` to the embedded tool; this catches the COMPOSED cases that
/// fall through to the confined shell (`git add . && git commit -m x`,
/// `echo msg | git commit -F -`, `git -c user.email=… commit`,
/// `/usr/bin/git -C <repo> commit`, `GIT_AUTHOR_NAME=… git commit`), and now
/// ALSO the other audit-identified commit-producing forms: `git merge`,
/// `git cherry-pick`, `git revert`, and `git rebase` (composed or bare).
///
/// Scope is deliberately narrow and fail-closed:
/// - Detects the [`SHELL_GIT_COMMIT_SUBCOMMANDS`] set. `commit`/`amend`/
///   `rebase` have a first-class embedded route (the `git` tool's
///   `commit`/`amend`/`rebase` ops, which the attribution finalizer owns); the
///   model is directed there. `merge`/`cherry-pick`/`revert` have NO first-class
///   Newt route and are DENIED (the operator must run them directly).
/// - ABORT/QUIT forms ([`SHELL_GIT_ABORT_FLAGS`]) of `merge`/`cherry-pick`/
///   `revert`/`rebase` create NO commit and pass through. `--skip`/`--continue`
///   DO create commits and stay blocked.
/// - Read-only git (`status`/`log`/`diff`/…) and git NETWORK ops
///   ([`GIT_PASSTHROUGH_SUBCOMMANDS`]) are NOT commit creation and pass through.
///
/// This is a bounded lexical gate, NOT a general shell parser: it splits only
/// on sequencing/pipeline/redirect separators and recognizes a fixed set of git
/// global options to locate the subcommand. It over-splits on quoted
/// metacharacters by design (fail-closed).
pub(super) fn run_command_creates_shell_git_commit(command: &str) -> bool {
    // Normalize command substitution `$(…)` to a separator (a real command is
    // single-line, so `\n` cannot occur) so a sub-command inside it is examined
    // independently — the same fail-closed over-split as the other separators.
    let normalized = command.replace("$(", "\n");
    for segment in normalized.split(SHELL_SEGMENT_SEPARATORS) {
        let toks: Vec<&str> = segment.split_whitespace().collect();
        if toks.is_empty() {
            continue;
        }
        // Skip leading shell env assignments (`NAME=VALUE git …`).
        let mut i = 0;
        while i < toks.len() && is_env_assignment(toks[i]) {
            i += 1;
        }
        if i >= toks.len() || !is_git_binary(toks[i]) {
            continue;
        }
        if let Some(sub) = git_subcommand_after_binary(&toks[i + 1..]) {
            if !SHELL_GIT_COMMIT_SUBCOMMANDS.contains(&sub) {
                continue;
            }
            // The subcommand's args are the tokens after it. An abort/quit
            // flag makes merge/cherry-pick/revert/rebase create NO commit —
            // preserve it. (`commit` has no abort form, so this never exempts
            // a real `git commit`; `--amend` is not an abort flag.)
            let args = &toks[i + 2..];
            if sub != "commit" && args.iter().any(|a| SHELL_GIT_ABORT_FLAGS.contains(a)) {
                continue;
            }
            return true;
        }
    }
    false
}

/// #894: the built-in tool registry — ONE self-describing entry per non-base
/// tool (name + JSON schema builder + presence gate), replacing the parallel
/// hand-kept lists that used to drift (the `lifecycle` tool, #891, was
/// advertised + dispatched yet missing from the old `ALL_TOOL_NAMES`, so every
/// legitimate `lifecycle` call was miscounted as a hallucination). Adding a
/// tool here now updates BOTH the advertised set ([`merged_tool_definitions`])
/// AND the real-name set ([`ALL_TOOL_NAMES`]) atomically — you cannot half-wire
/// one without the other. The base tools stay inlined in [`tool_definitions`]
/// (their names mirrored in [`BASE_TOOL_NAMES`], guarded by a test); the
/// executor dispatch match is intentionally left as a separate pass.
///
/// Presence condition for a registered tool. `Always` tools ride every session
/// (they degrade honestly when their backing source is absent); the rest are
/// advertised only when the matching `with_*` capability is injected.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Gate {
    Always,
    SaveNote,
    Recall,
    MemoryFetch,
    Git,
    Team,
    Scratchpad,
    CodeSearch,
    Experiential,
    Scheduled,
    PlanMode,
    ScheduledPlanMode,
    OperatingMode,
}

/// One built-in (non-base) tool, declared in exactly one place.
pub(super) struct ToolSpec {
    /// The tool name the model calls — must equal `(definition)()`'s function name.
    pub(super) name: &'static str,
    /// Builds the tool's JSON schema (the same `*_tool_definition()` fns used
    /// before; the registry just references them).
    pub(super) definition: fn() -> serde_json::Value,
    /// When this tool is advertised + treated as a real (non-hallucinated) name.
    pub(super) gate: Gate,
}

/// The registered non-base tools, in advertised order. This IS the order
/// [`merged_tool_definitions`] pushes them (after the base array), preserved
/// byte-for-byte from the previous hand-written push ladder.
pub(super) const EXTENDED_TOOL_REGISTRY: &[ToolSpec] = &[
    // Always-on (degrade gracefully when their source is None) —
    // #714 / prompt recovery / artifact recovery / #725 / #727 / #728 / #891.
    ToolSpec {
        name: "resume_context",
        definition: super::super::resume::resume_context_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "prompt_read",
        definition: super::super::prompt_read::prompt_read_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "artifact_read",
        definition: super::super::artifact_read::artifact_read_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "tool_search",
        definition: super::super::tool_search::tool_search_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "get_context_remaining",
        definition: super::super::budget::get_context_remaining_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "request_user_input",
        definition: request_user_input_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "lifecycle",
        definition: lifecycle_tool_definition,
        gate: Gate::Always,
    },
    // #1004: present collected findings as a rendered Markdown document. Needs
    // no injected capability (it writes only to the output sink every session
    // has), so it rides Always and degrades to raw source when color is off.
    ToolSpec {
        name: "render_report",
        definition: render_report_tool_definition,
        gate: Gate::Always,
    },
    // Presence-gated on an injected capability (one `with_*` flag each).
    ToolSpec {
        name: "save_note",
        definition: save_note_tool_definition,
        gate: Gate::SaveNote,
    },
    ToolSpec {
        name: "recall",
        definition: recall_tool_definition,
        gate: Gate::Recall,
    },
    ToolSpec {
        name: "memory_fetch",
        definition: memory_fetch_tool_definition,
        gate: Gate::MemoryFetch,
    },
    ToolSpec {
        name: "git",
        definition: super::super::git_tool::git_tool_definition,
        gate: Gate::Git,
    },
    ToolSpec {
        name: "compose_roster",
        definition: super::super::crew_tool::compose_roster_tool_definition,
        gate: Gate::Team,
    },
    ToolSpec {
        name: "crew",
        definition: super::super::crew_tool::crew_tool_definition,
        gate: Gate::Team,
    },
    ToolSpec {
        name: "state_set",
        definition: super::super::scratchpad::state_set_tool_definition,
        gate: Gate::Scratchpad,
    },
    ToolSpec {
        name: "state_get",
        definition: super::super::scratchpad::state_get_tool_definition,
        gate: Gate::Scratchpad,
    },
    ToolSpec {
        name: "state_clear",
        definition: super::super::scratchpad::state_clear_tool_definition,
        gate: Gate::Scratchpad,
    },
    ToolSpec {
        name: "code_search",
        definition: super::super::semantic::code_search_tool_definition,
        gate: Gate::CodeSearch,
    },
    // #1285: where_is is a read-only navigation UTILITY (like tool_search /
    // get_context_remaining) — advertised every session and degrading honestly
    // when no symbol index was built (the index is model-free + cheap, so the
    // harness builds it for any project). Gate::Always keeps the merged catalog
    // free of a per-call flag; the executor's `where_is: None` arm coaches.
    ToolSpec {
        name: "where_is",
        definition: crate::where_is::where_is_tool_definition,
        gate: Gate::Always,
    },
    // #1387 Code Navigator — narrow structural/lexical tools (Always; degrade
    // honestly when session indexes are absent). code_search / where_is keep
    // their existing names for compatibility.
    ToolSpec {
        name: "goto_definition",
        definition: crate::navigator::goto_definition_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "text_search",
        definition: crate::navigator::text_search_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "find_references",
        definition: crate::navigator::find_references_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "find_tests",
        definition: crate::navigator::find_tests_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "find_callers",
        definition: crate::navigator::find_callers_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "find_callees",
        definition: crate::navigator::find_callees_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "find_implementations",
        definition: crate::navigator::find_implementations_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "find_hierarchy",
        definition: crate::navigator::find_hierarchy_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "inspect_type",
        definition: crate::navigator::inspect_type_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "impact",
        definition: crate::navigator::impact_tool_definition,
        gate: Gate::Always,
    },
    ToolSpec {
        name: "experience_record",
        definition: super::super::experiential::experience_record_tool_definition,
        gate: Gate::Experiential,
    },
    ToolSpec {
        name: "experience_recall",
        definition: super::super::experiential::experience_recall_tool_definition,
        gate: Gate::Experiential,
    },
    ToolSpec {
        name: "update_plan",
        definition: super::super::scheduled::update_plan_tool_definition,
        gate: Gate::Scheduled,
    },
    ToolSpec {
        name: "plan_get",
        definition: super::super::scheduled::plan_get_tool_definition,
        gate: Gate::Scheduled,
    },
    ToolSpec {
        name: "enter_plan_mode",
        definition: super::super::scheduled::enter_plan_mode_tool_definition,
        gate: Gate::ScheduledPlanMode,
    },
    ToolSpec {
        name: "exit_plan_mode",
        definition: super::super::scheduled::exit_plan_mode_tool_definition,
        gate: Gate::PlanMode,
    },
    ToolSpec {
        name: "select_operating_mode",
        definition: super::super::operating_mode::select_operating_mode_tool_definition,
        gate: Gate::OperatingMode,
    },
];

/// The base tools inlined in [`tool_definitions`], by name. The base array is
/// the one place these tools are declared; this mirror exists only so
/// [`ALL_TOOL_NAMES`] can be assembled without re-parsing JSON, and it is kept
/// in lockstep by `base_tool_names_match_tool_definitions`. The EXTENDED tools'
/// names are NOT duplicated here — they live on their [`ToolSpec`].
pub(super) const BASE_TOOL_NAMES: &[&str] = &[
    "run_command",
    "read_file",
    "write_file",
    "edit_file",
    "delete_file",
    "list_dir",
    "find",
    "use_skill",
    "web_fetch",
    "request_permissions",
];

/// Every tool newt can dispatch by name — the base tools plus every registered
/// one — DERIVED from [`BASE_TOOL_NAMES`] + [`EXTENDED_TOOL_REGISTRY`] so it can
/// never drift from the advertised set. Single source of truth for
/// [`is_hallucination`] (which names are real) and [`nearest_tool_name`]
/// (suggestion candidates). MCP `server__tool` names contain `__` and are
/// matched separately.
pub(super) static ALL_TOOL_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    BASE_TOOL_NAMES
        .iter()
        .copied()
        .chain(EXTENDED_TOOL_REGISTRY.iter().map(|s| s.name))
        .collect()
});

/// Whether `tool_name` is a built-in newt tool name. Dynamic MCP tool names are
/// intentionally excluded; callers that need MCP should check their live MCP
/// registry separately.
pub(crate) fn known_builtin_tool_name(tool_name: &str) -> bool {
    ALL_TOOL_NAMES.contains(&tool_name)
}

/// Whether a [`Gate`] is satisfied given this session's injected capabilities.
/// Extracted so [`merged_tool_definitions`] reads as one loop over the registry.
#[allow(clippy::too_many_arguments)] // mirrors merged_tool_definitions' with_* flags
fn gate_satisfied(
    gate: Gate,
    with_save_note: bool,
    with_recall: bool,
    with_memory_fetch: bool,
    with_git: bool,
    with_team: bool,
    with_scratchpad: bool,
    with_code_search: bool,
    with_experiential: bool,
    with_scheduled: bool,
    with_operating_mode_control: bool,
    with_plan_mode_control: bool,
    with_plan_mode_active: bool,
) -> bool {
    match gate {
        Gate::Always => true,
        Gate::SaveNote => with_save_note,
        Gate::Recall => with_recall,
        Gate::MemoryFetch => with_memory_fetch,
        Gate::Git => with_git,
        Gate::Team => with_team,
        Gate::Scratchpad => with_scratchpad,
        Gate::CodeSearch => with_code_search,
        Gate::Experiential => with_experiential,
        Gate::Scheduled => with_scheduled,
        // Provider loops freeze their schema for the whole multi-round turn.
        // If enter is visible at turn start, exit must be visible too so the
        // model can enter, write its plan, and leave in that same turn. An
        // already-active phase also keeps exit visible when scheduled planning
        // is toggled off between turns.
        Gate::PlanMode => (with_scheduled && with_plan_mode_control) || with_plan_mode_active,
        Gate::ScheduledPlanMode => with_scheduled && with_plan_mode_control,
        Gate::OperatingMode => with_operating_mode_control,
    }
}

/// Returns `true` if a tool call looks like a hallucination:
/// - `run_command` called with a tool name as the shell command, or
/// - An unknown tool name (excluding MCP-namespaced `server__tool` names).
pub(crate) fn is_hallucination(tool_name: &str, args: &serde_json::Value) -> bool {
    if tool_name == "run_command" {
        let cmd = args["command"].as_str().unwrap_or("");
        // A misdirected direct-tool call is a hallucination; a real shell
        // command (including the git NETWORK ops #898 lets through) is not.
        return run_command_redirect(cmd).is_some();
    }
    // MCP tools are namespaced with `__` — never treat them as hallucinations.
    if tool_name.contains("__") {
        return false;
    }
    // #894: derived from BASE_TOOL_NAMES + EXTENDED_TOOL_REGISTRY, so it can
    // never drift from the advertised set.
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
        // #891 lifecycle aliases — same `{phase, action}` arg shape as
        // `lifecycle`, so we rewrite and dispatch. Common phrasings a model
        // reaches for when it wants to run a named phase.
        "run_phase" | "run_lifecycle" | "lifecycle_run" => Some(AliasOutcome::Rewrite("lifecycle")),
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
        // Delete-file aliases — point at delete_file. `rm` typed as a shell
        // command is redirected separately by `run_command_redirect`; `git rm`
        // deliberately falls through to the shell/git path because it has index
        // semantics beyond plain file deletion.
        "remove_file" | "delete" | "remove" | "unlink" | "rm_file" => {
            Some(AliasOutcome::Correct(format!(
                "'{name}' is not a newt tool. To remove one file, call delete_file \
                 with {{\"path\"}}. It is governed by fs_write permissions and \
                 prompts the operator when a grant is needed."
            )))
        }
        // #721 mkdir coach — newt has no directory-creation tool, and the model
        // does not need one: write_file creates parent directories automatically
        // (create_dir_all), and an empty file for empty content. A model reaching
        // for `mkdir` (the issue's live `mkdir -p …/src` dead-end) is coached to
        // the tool that already covers it, turning an exec denial into a
        // self-correcting tool call. `touch` is intentionally NOT here — it is
        // already a create-file alias above (→ write_file), and a second arm
        // would be a duplicate match.
        "mkdir" | "make_dir" | "makedirs" | "mkdirs" | "create_dir" | "create_directory" => {
            Some(AliasOutcome::Correct(
                "newt has no mkdir/touch tool — call write_file; it creates parent \
                 directories automatically (create_dir_all). For an empty file, call \
                 write_file with empty content."
                    .to_string(),
            ))
        }
        // Read / list aliases — point at read_file / list_dir.
        "cat" | "open_file" | "view_file" | "view" | "open" => {
            Some(AliasOutcome::Correct(format!(
                "'{name}' is not a newt tool. To read a file, call read_file with \
                 {{\"path\"}}. To list a directory, call list_dir with {{\"path\"}}."
            )))
        }
        // #716 + #715 PR2 PLAN — start/revise a plan. The arg shape is free prose,
        // not the ordered `{"plan":[{"step","status"}]}` array update_plan wants, so
        // Correct (coach), never a silent Rewrite. When `scheduled` is off the
        // dispatch arm for update_plan returns "scheduled planning is off" — the
        // model still gets a coherent answer rather than a dead end. `update_plan`
        // itself is the REAL tool now (falls through to `None`), so it is never an
        // alias of itself; `set_plan`/`plan_advance` no longer exist.
        "make_plan" | "create_plan" | "plan" | "planning" | "todo" | "todos" | "todo_write" => {
            Some(AliasOutcome::Correct(format!(
                "'{name}' is not a newt tool. To start or revise your plan, call update_plan with \
                 {{\"plan\":[{{\"step\",\"status\"}}]}} — send the full ordered list each time, \
                 each step's status one of pending/in_progress/completed (exactly one \
                 in_progress)."
            )))
        }
        // #715 PR2 ADVANCE-ish verbs — there is no longer a separate "advance" tool;
        // progress is recorded by re-sending the whole plan with the finished step
        // marked completed. Coach the model back to update_plan.
        "next_step" | "complete_step" | "finish_step" | "mark_done" | "step_done" => {
            Some(AliasOutcome::Correct(format!(
                "'{name}' is not a newt tool. To advance your plan, call update_plan with the \
                 full plan and mark the finished step \"completed\" (and the next one \
                 \"in_progress\")."
            )))
        }
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
        // #725 TOOL-DISCOVERY — the instinctive "which tool does X?" reach. All
        // mean exactly tool_search (a `query` arg, or none — execute_tool_search
        // lists everything on an empty query), so silently Rewrite. `tool_search`
        // itself is the REAL tool and falls through to `None` below — it is never
        // an alias of itself.
        "find_tool" | "search_tools" | "list_tools" | "which_tool" | "available_tools"
        | "what_tools" | "tools" => Some(AliasOutcome::Rewrite("tool_search")),
        // #716 WORKFLOW — no workflow/pipeline primitive exists; redirect to the
        // plan tools (and crew/team, which needs /team).
        "workflow" | "run_workflow" | "start_workflow" | "pipeline" => Some(AliasOutcome::Correct(
            "newt has no workflow tool; sequence the work with update_plan (the full ordered \
                 plan with statuses), or delegate subtasks via crew/team (needs /team)."
                .to_string(),
        )),
        // #727 BUDGET — "how much context do I have left" reaches. get_context_remaining
        // takes no args (it is a self-read), so the foreign call's (empty) arg shape
        // matches: safe to silently Rewrite. The real name `get_context_remaining` is
        // NOT here — it falls through to `None` and dispatches unchanged (the loop
        // intercepts it), so it is never an alias of itself.
        "context_remaining" | "tokens_left" | "remaining_tokens" | "budget"
        | "how_much_context" | "context_budget" | "token_budget" => {
            Some(AliasOutcome::Rewrite("get_context_remaining"))
        }
        // #728 ASK-THE-HUMAN — the instinctive "ask the user a question" reach.
        // All mean exactly request_user_input (a `question` arg), so silently
        // Rewrite: the executor reads `question` and answers via the gate (or the
        // headless message). The real name `request_user_input` is NOT here — it
        // falls through to `None` and dispatches unchanged, so it is never an
        // alias of itself.
        "ask_user" | "ask_human" | "prompt_user" | "get_user_input" | "ask_question"
        | "clarify" | "ask" => Some(AliasOutcome::Rewrite("request_user_input")),
        _ => None,
    }
}

/// #727: true when `name` is `get_context_remaining` or one of its rewrite
/// aliases. The agentic loop computes the per-turn budget at the dispatch site
/// (where `num_ctx` and the conversation estimate are in scope) and renders it
/// there, bypassing [`execute_tool`] — so the loop must recognize both the
/// canonical name and the aliases that resolve to it.
pub(crate) fn is_context_remaining_call(name: &str) -> bool {
    name == "get_context_remaining"
        || matches!(
            resolve_tool_alias(name),
            Some(AliasOutcome::Rewrite("get_context_remaining"))
        )
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

/// #479 (G4): classify a `crew`/`compose_roster` reach made while the crew/team
/// surface is gated OFF (`advertise_team == false`, the default — the runner is
/// only built when the operator sets `NEWT_TEAM`).
///
/// Pure: given the name the model called and whether the team surface is
/// advertised this session, decide whether this is a gated-off delegation reach
/// worth mining. Returns `None` for everything else (non-crew names, or crew
/// names when the surface is ON — those dispatch normally).
///
/// This is a SEPARATE seam from [`classify_phantom_reach`] on purpose: `crew`
/// and `compose_roster` stay real names in [`ALL_TOOL_NAMES`] (so the ON path is
/// a normal dispatch and [`is_hallucination`] is unchanged), which means
/// `classify_phantom_reach` never flags them. The gated-off detection needs the
/// one fact that function does not have — `advertise_team` — which is known in
/// the agent loop, so the loop composes the two seams there.
pub(crate) fn classify_gated_off_reach(
    name: &str,
    advertise_team: bool,
) -> Option<crate::PhantomResolution> {
    if !advertise_team && (name == "crew" || name == "compose_roster") {
        return Some(crate::PhantomResolution::GatedOff(
            "crew/team surface off (NEWT_TEAM)".into(),
        ));
    }
    None
}

/// Classic Levenshtein edit distance (pure two-row DP). Inputs are tool names
/// (short), so the simple version is plenty — for fuzzy suggestions only.
pub(super) fn levenshtein(a: &str, b: &str) -> usize {
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
pub(super) fn nearest_tool_name(name: &str) -> Option<&'static str> {
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
pub(super) fn unknown_tool_message(name: &str) -> String {
    const BASE: &str =
        "run_command, read_file, write_file, edit_file, delete_file, list_dir, find, use_skill, web_fetch";
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
