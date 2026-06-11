//! Built-in tool definitions and the tool executor for the agentic loop.
//! Moved verbatim from `newt-tui` in Step 9.7 — the Caveats enforcement,
//! shrink guard, build-check feedback, and agent-bridle routing are unchanged.

use super::display::{print_denied, print_tool_call, print_tool_output};
use super::mcp::McpTools;
use super::note_sink::{execute_save_note, save_note_tool_definition, NoteSink};

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
                "description": "Read the contents of a file in the workspace",
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
pub(crate) fn merged_tool_definitions(
    mcp: &dyn McpTools,
    with_save_note: bool,
) -> serde_json::Value {
    let mut defs = match tool_definitions() {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    if with_save_note {
        defs.push(save_note_tool_definition());
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
    !matches!(
        tool_name,
        "run_command"
            | "list_dir"
            | "read_file"
            | "write_file"
            | "edit_file"
            | "use_skill"
            | "web_fetch"
            | "save_note"
    )
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

/// Returns true if `full_path` is permitted by `scope`, using prefix matching
/// against the stored workspace-root strings.
///
/// The `Caveats` lattice stores workspace root strings (not individual file paths)
/// and uses exact-set semantics. The TUI adds path-prefix semantics here so that
/// "workspace root is permitted" translates to "any file under it is permitted".
pub(crate) fn tui_permits_path(scope: &crate::caveats::Scope<String>, full_path: &str) -> bool {
    match scope {
        crate::caveats::Scope::All => true,
        crate::caveats::Scope::Only(set) if set.is_empty() => false,
        crate::caveats::Scope::Only(set) => {
            set.iter().any(|root| full_path.starts_with(root.as_str()))
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

/// Execute a single tool call and return the result string sent back to the model.
///
/// `run_command` is routed through agent-bridle's Caveats-confined, brush-backed
/// `shell` tool: the WHOLE command runs inside the leash (`echo ok && rm -rf /`
/// no longer slips `rm` past an `echo` grant — every external spawn passes the
/// interceptor's `before_exec` / `before_open` gate). The fs tools
/// (`read_file` / `write_file` / `list_dir`) keep enforcing the same `caveats`
/// via `permits_*` — rerouting them is out of scope.
///
/// `note_sink` backs the `save_note` tool (Step 19.3). `None` ⇒ the tool was
/// never advertised, so a call here is treated like any unknown tool.
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
) -> String {
    // Remote MCP tools (namespaced `server__tool`) route to their server before
    // the built-in match. They carry no Caveats leash in this build.
    if mcp.handles(name) {
        print_tool_call(name, &args.to_string(), color);
        let out = mcp.call(name, args).await;
        print_tool_output(&out, tool_output_lines, color);
        return out;
    }

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
            let dispatch_args = serde_json::json!({
                "cmd": cmd_with_venv,
                "cwd": workspace,
            });
            match agent_bridle::registry()
                .dispatch("shell", dispatch_args, caveats)
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
                    let reason = envelope_denial_reason_with_guidance(&envelope);
                    print_denied("exec", &reason, color);
                    format!("capability denied: {reason}")
                }
                Ok(envelope) => {
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
                let msg = format!("capability denied: fs_read does not permit '{path}'");
                print_denied("fs_read", path, color);
                return msg;
            }
            print_tool_call("read_file", path, color);
            match std::fs::read_to_string(&full) {
                Ok(contents) => {
                    print_tool_output(&contents, tool_output_lines, color);
                    contents
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
                let msg = format!("capability denied: fs_write does not permit '{path}'");
                print_denied("fs_write", path, color);
                return msg;
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
                let msg = format!("capability denied: fs_write does not permit '{path}'");
                print_denied("fs_write", path, color);
                return msg;
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
                return format!(
                    "error: old_string not found in {path}. \
                     Check for whitespace differences or read the file first to confirm the exact text."
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
                let msg = format!("capability denied: fs_read does not permit '{path}'");
                print_denied("fs_read", path, color);
                return msg;
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
            match agent_bridle::registry()
                .dispatch("web_fetch", fetch_args, caveats)
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

        other => format!("unknown tool: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::NoMcp;

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
        let merged = merged_tool_definitions(&NoMcp, false);
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
                "use_skill",
                "web_fetch"
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
        let without = merged_tool_definitions(&NoMcp, false);
        assert!(!names(&without).contains(&"save_note"));
        // … but a sink advertises it.
        let with = merged_tool_definitions(&NoMcp, true);
        assert!(names(&with).contains(&"save_note"), "got: {with}");
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
    fn tui_permits_path_prefix_semantics() {
        use crate::caveats::Scope;
        assert!(tui_permits_path(&Scope::All, "/anything/at/all"));
        assert!(!tui_permits_path(&Scope::<String>::none(), "/ws/file"));
        let only = Scope::only(["/ws".to_string()]);
        assert!(tui_permits_path(&only, "/ws/sub/file.rs"));
        assert!(!tui_permits_path(&only, "/elsewhere/file.rs"));
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
        assert_eq!(out, "unknown tool: definitely_not_a_tool");
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
        )
        .await;
        assert_eq!(sink.calls, vec!["add:workspace builds with just check"]);
        assert!(
            out.starts_with("note saved: workspace builds"),
            "got: {out}"
        );
    }
}
