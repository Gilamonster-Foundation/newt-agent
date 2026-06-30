//! MCP protocol handlers: initialize, tools/list, and tools/call.
//!
//! Registers all JSON-RPC methods that the newt MCP server exposes.
//!
//! `goal_run` is the only handler that needs runtime state — it
//! borrows a [`BackendRegistry`] (to pick a backend) and a [`Router`]
//! (to classify the prompt into a [`Tier`]). The other tools
//! (`code_read` / `code_edit` / `code_search`) are pure file I/O and
//! need no shared state.

use std::path::Path;
use std::sync::Arc;

use agent_bridle::{Caveats, Registry, ToolError};
use newt_core::router::{Router, Tier};
use newt_inference::BackendRegistry;
use serde_json::Value;

use crate::caveats::GrantedCaveats;
use crate::server::McpServer;

/// Register the core MCP protocol handlers on `server`.
///
/// `registry` and `router` are wired into the `goal_run` handler;
/// every other handler ignores them.
///
/// `shell_run` is the one tool that no longer runs free: it is dispatched
/// through agent-bridle's Caveats-confined brush shell, under a granted
/// [`Caveats`] leash sourced from `~/.newt/config.toml` (see
/// [`crate::caveats`]). The bridle [`Registry`] and the granted leash are
/// built once here and shared (via `Arc`) into the `tools/call` closure.
pub fn register_handlers(
    server: &mut McpServer,
    registry: Arc<BackendRegistry>,
    router: Arc<Router>,
) {
    let granted = GrantedCaveats::load();
    // Surface, loudly, whether shell_run is confined or running with full
    // ambient authority. An unconfined default is a WARNING.
    granted.warn_to_stderr();

    register_initialize(server);
    register_tools_list(server);
    register_tools_call(
        server,
        registry,
        router,
        Arc::new(agent_bridle::registry()),
        Arc::new(granted.caveats),
    );
}

// ── initialize ─────────────────────────────────────────────────────────────

fn register_initialize(server: &mut McpServer) {
    server.register("initialize", |_params| async move {
        Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "newt-mcp-server",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    });
}

// ── tools/list ─────────────────────────────────────────────────────────────

fn register_tools_list(server: &mut McpServer) {
    server.register("tools/list", |_params| async move {
        Ok(serde_json::json!({
            "tools": tool_definitions()
        }))
    });
}

/// Return the JSON array of tool definitions (shared by tools/list).
fn tool_definitions() -> Value {
    let tools = vec![
        serde_json::json!({
            "name": "code_read",
            "description": "Read a file's contents",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path to read"
                    }
                },
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "code_edit",
            "description": "Apply a unified diff patch to a file",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path to edit"
                    },
                    "patch": {
                        "type": "string",
                        "description": "Unified diff to apply"
                    }
                },
                "required": ["path", "patch"]
            }
        }),
        serde_json::json!({
            "name": "code_search",
            "description": "Search files for a regex pattern",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Regex pattern"
                    },
                    "path": {
                        "type": "string",
                        "description": "Root directory to search"
                    }
                },
                "required": ["query", "path"]
            }
        }),
        serde_json::json!({
            "name": "git",
            "description": "Run a git operation via the embedded engine (grit-lib), gated by GitCaveats derived from the granted Caveats leash. ops: status | log | diff | add | commit | branch. Local-only; network ops (clone/fetch/push) are fail-closed. A write op (add/commit/branch) under a read-only grant is DENIED (isError). Returns structured JSON.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["status", "log", "diff", "add", "commit", "branch"] },
                    "repo": { "type": "string", "description": "Repository path (default: current directory)" },
                    "limit": { "type": "integer", "description": "log: max commits (default 20)" },
                    "staged": { "type": "boolean", "description": "diff: staged (index vs HEAD) instead of worktree (default false)" },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "add: repo-relative paths to stage" },
                    "message": { "type": "string", "description": "commit: commit message" },
                    "name": { "type": "string", "description": "branch: new branch name" }
                },
                "required": ["op"]
            }
        }),
        serde_json::json!({
            "name": "goal_run",
            "description": "Run a tier-routed inference turn",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The prompt"
                    },
                    "tier": {
                        "type": "string",
                        "description": "Optional tier override",
                        "enum": ["FAST", "STANDARD", "COMPLEX", "REVIEW"]
                    }
                },
                "required": ["prompt"]
            }
        }),
        serde_json::json!({
            "name": "fs_list",
            "description": "List directory contents (dirs first, then files, alphabetical). Returns name, kind, and size_bytes for each entry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path to list"
                    }
                },
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "shell_run",
            "description": "Run a shell command and capture stdout/stderr. Returns exit_code, stdout, stderr, timed_out. Max timeout 300s. CAVEATS-CONFINED: the command runs inside agent-bridle's brush shell under the granted Caveats leash (from ~/.newt/config.toml [caveats]); a command outside the granted exec/fs scope is DENIED (isError) rather than executed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cmd": {
                        "type": "string",
                        "description": "Shell command to execute, confined in-process by the agent-bridle capability interceptor (exec + fs scopes)"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory (default: current directory)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default 30, max 300)"
                    }
                },
                "required": ["cmd"]
            }
        }),
        serde_json::json!({
            "name": "web_fetch",
            "description": "Fetch an http(s) URL and return its main content as clean markdown. CAVEATS-CONFINED: reachable hosts are gated by the granted `net` scope (host allowlist + SSRF screen); an out-of-scope host is DENIED (isError) rather than fetched. Returns { url, final_url, status, title, markdown } — the markdown is untrusted page content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The http(s) URL to fetch"
                    },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Optional cap on bytes downloaded (default 5 MiB, max 25 MiB)"
                    }
                },
                "required": ["url"]
            }
        }),
    ];

    Value::Array(tools)
}

// ── tools/call ─────────────────────────────────────────────────────────────

fn register_tools_call(
    server: &mut McpServer,
    registry: Arc<BackendRegistry>,
    router: Arc<Router>,
    bridle: Arc<Registry>,
    granted: Arc<Caveats>,
) {
    server.register("tools/call", move |params| {
        // Move clones into the async block so each invocation owns its
        // own Arc (the outer closure is `Fn`, not `FnOnce`).
        let registry = registry.clone();
        let router = router.clone();
        let bridle = bridle.clone();
        let granted = granted.clone();
        async move {
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));

            match name.as_str() {
                "code_read" => handle_code_read(&arguments),
                "code_edit" => handle_code_edit(&arguments),
                "code_search" => handle_code_search(&arguments),
                "goal_run" => handle_goal_run(&arguments, &registry, &router).await,
                "fs_list" => handle_fs_list(&arguments),
                "git" => handle_git(&arguments, &granted),
                "shell_run" => Ok(handle_shell_run(arguments, &bridle, &granted).await),
                "web_fetch" => Ok(handle_web_fetch(arguments, &bridle, &granted).await),
                other => anyhow::bail!("unknown tool: {other}"),
            }
        }
    });
}

// ── Tool implementations ───────────────────────────────────────────────────

/// `git` — the embedded git engine (grit-lib via `newt-git`), gated by
/// `GitCaveats` derived from the granted leash. Local ops only; a write under a
/// read-only grant returns an error (MCP `isError`).
fn handle_git(args: &Value, granted: &Caveats) -> anyhow::Result<Value> {
    use newt_git::{Author, DiffSpec, GitEngine};

    let op = args
        .get("op")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: op"))?;
    let repo = args.get("repo").and_then(|v| v.as_str()).unwrap_or(".");
    // The git surface is bounded by the granted leash: read-only when fs_write is
    // empty, full-local when writable, network always denied.
    let caps = newt_core::git_caveats::GitCaveats::from_session(granted);
    let eng = GitEngine::open(Path::new(repo)).map_err(|e| anyhow::anyhow!("git open: {e}"))?;

    let result: Value = match op {
        "status" => serde_json::to_value(eng.status(&caps).map_err(gerr)?)?,
        "log" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            serde_json::to_value(eng.log(&caps, limit).map_err(gerr)?)?
        }
        "diff" => {
            let spec = if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
                DiffSpec::Staged
            } else {
                DiffSpec::Worktree
            };
            serde_json::to_value(eng.diff(&caps, spec).map_err(gerr)?)?
        }
        "add" => {
            let paths: Vec<String> = args
                .get("paths")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            serde_json::to_value(eng.add(&caps, &paths).map_err(gerr)?)?
        }
        "commit" => {
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("git commit requires 'message'"))?;
            let author = Author {
                name: args
                    .get("author_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("newt-agent[bot]")
                    .to_string(),
                email: args
                    .get("author_email")
                    .and_then(|v| v.as_str())
                    .unwrap_or("newt-agent@users.noreply.github.com")
                    .to_string(),
            };
            serde_json::to_value(eng.commit(&caps, message, &author).map_err(gerr)?)?
        }
        "branch" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("git branch requires 'name'"))?;
            serde_json::to_value(eng.branch(&caps, name).map_err(gerr)?)?
        }
        other => anyhow::bail!("unknown git op: {other}"),
    };
    Ok(mcp_text_content(&serde_json::to_string_pretty(&result)?))
}

/// Map a `newt-git` engine error (incl. capability denial) to an `anyhow` error
/// so the dispatch surfaces it as an MCP `isError`.
fn gerr(e: newt_git::GitError) -> anyhow::Error {
    anyhow::anyhow!("git: {e}")
}

fn handle_code_read(args: &Value) -> anyhow::Result<Value> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: path"))?;

    let content = newt_tools::read(Path::new(path))?;
    Ok(mcp_text_content(&content))
}

fn handle_code_edit(args: &Value) -> anyhow::Result<Value> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: path"))?;
    let patch = args
        .get("patch")
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: patch"))?;

    newt_tools::edit(Path::new(path), patch)?;
    Ok(mcp_text_content(&format!("patched {path}")))
}

fn handle_code_search(args: &Value) -> anyhow::Result<Value> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: path"))?;

    let hits = newt_tools::search(query, Path::new(path))?;
    let lines: Vec<String> = hits
        .iter()
        .map(|h| format!("{}:{}: {}", h.path, h.line_number, h.line))
        .collect();
    let text = if lines.is_empty() {
        "no matches".to_string()
    } else {
        lines.join("\n")
    };
    Ok(mcp_text_content(&text))
}

fn handle_fs_list(args: &Value) -> anyhow::Result<Value> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: path"))?;

    let entries = newt_tools::list_dir(Path::new(path))?;
    Ok(mcp_text_content(&serde_json::to_string_pretty(&entries)?))
}

/// `shell_run` — the Caveats-confined shell.
///
/// History: PR #125 implemented this as an UNCONFINED `tokio::process` `sh -c`
/// with only a timeout — full ambient authority. P1 supersedes that: the same
/// MCP tool name and input schema (`cmd`, `cwd`, `timeout_secs`) now route the
/// command through agent-bridle's brush-backed confined shell under the granted
/// [`Caveats`] leash. A command outside the granted `exec`/`fs` scope is DENIED
/// in-process by the brush `CommandInterceptor` hook — it never runs.
///
/// `cmd` is mapped to agent-bridle's **free-form `cmd` mode** (added in
/// agent-bridle#4) so it stays a drop-in for clients: pipelines, `&&`,
/// redirections, globbing all still work, but every external spawn passes the
/// interceptor's `before_exec` / `before_open` gate.
///
/// Error mapping (the load-bearing semantics): a leash **denial** is surfaced
/// as an MCP *tool error* — `{ content: [..], isError: true }` carrying the
/// reason — NOT a JSON-RPC transport fault. That keeps the denial observable to
/// the model: it sees *why* it was refused without the call collapsing into a
/// `-32603`. So this function returns a `Value` directly (the `tools/call`
/// arm wraps it in `Ok`), and never bubbles a leash error up the transport.
async fn handle_shell_run(args: Value, bridle: &Registry, granted: &Caveats) -> Value {
    // Validate the one required field. A missing `cmd` is a tool-level mistake,
    // so it comes back as an in-band tool error, matching the leash-denial
    // shape rather than crashing the transport.
    if args.get("cmd").and_then(Value::as_str).is_none() {
        return mcp_error_content("missing required argument: cmd (must be a string)");
    }

    // Route to agent-bridle's free-form `cmd` mode. The shell tool already
    // reads `cmd`, `cwd`, and `timeout_secs` from this exact shape (and clamps
    // the timeout to its own 300s ceiling), so we forward the arguments
    // verbatim — no field translation needed.
    match bridle.dispatch("shell", args, granted).await {
        // The confined shell ran. Its envelope carries
        // `{ exit_code, stdout, stderr, timed_out, sandbox_kind }` plus —
        // when the leash refused a capability — the STRUCTURED denial fields
        // `{ denied: true, denials: [{ kind, target, reason }] }`.
        //
        // IMPORTANT (free-form `cmd` semantics): in free-form mode an
        // out-of-scope command is NOT a `ToolError::Denied` from `dispatch` —
        // there is no single named program to pre-check, so the brush
        // `CommandInterceptor`'s `before_exec` / `before_open` hook denies the
        // command *inside* the shell. The command genuinely does not run
        // (confinement is real), and the denial is reported through the
        // envelope's `denied` flag.
        //
        // To keep the leash observable end-to-end, we lift such an in-envelope
        // confinement denial to an MCP tool error (`isError: true`) carrying
        // the joined denial reason(s) — the same shape an argv-mode
        // `ToolError::Denied` produces. Detection reads the structured
        // `denied` field, NEVER parses stdout/stderr. An in-scope command
        // (even one that exits non-zero for its own reasons) is returned as a
        // normal success envelope.
        Ok(envelope) if is_denied(&envelope) => mcp_error_content(&denial_reason(&envelope)),
        Ok(envelope) => match serde_json::to_string_pretty(&envelope) {
            Ok(text) => mcp_text_content(&text),
            Err(e) => mcp_error_content(&format!("failed to serialize shell result: {e}")),
        },
        // An argv-mode leash denial (or budget / generation / unknown tool, or
        // an error from inside a tool that passed the leash) — surface the
        // reason in-band as an MCP tool error, never a transport fault.
        // (`ToolError::Display` is safe to show the agent.)
        Err(e @ ToolError::Denied { .. }) => mcp_error_content(&e.to_string()),
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

/// Fetch a web page through agent-bridle's `web_fetch`, leashed by the `net`
/// axis. Unlike the shell tool, a capability denial here is a normal
/// `ToolError::Denied` from `dispatch` (the `net` scope is checked inside the
/// tool), so there is no in-envelope `denied` flag to lift — we map `Ok` to MCP
/// text and any `Err` to an in-band MCP tool error.
async fn handle_web_fetch(args: Value, bridle: &Registry, granted: &Caveats) -> Value {
    if args.get("url").and_then(Value::as_str).is_none() {
        return mcp_error_content("missing required argument: url (must be a string)");
    }
    match bridle.dispatch("web_fetch", args, granted).await {
        // `{ url, final_url, status, title, markdown }` — untrusted page content.
        Ok(result) => match serde_json::to_string_pretty(&result) {
            Ok(text) => mcp_text_content(&text),
            Err(e) => mcp_error_content(&format!("failed to serialize web_fetch result: {e}")),
        },
        // Out-of-scope host (net denial), SSRF screen, timeout, non-2xx, etc.
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

/// Whether a confined-shell envelope carries the STRUCTURED `denied: true`
/// flag — the leash's machine-readable signal that the brush `CaveatInterceptor`
/// refused an exec / open in free-form mode.
///
/// This reads the structured field agent-bridle now emits; it does NOT parse
/// stdout/stderr. The old stderr string-match (`"is not within the granted"`,
/// etc.) was fragile — a successful command that merely *printed* a denial-like
/// phrase could be misread, and any wording drift in the leash would silently
/// break detection. The structured envelope removes both hazards.
fn is_denied(envelope: &Value) -> bool {
    envelope
        .get("denied")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Build a human-readable denial message from the envelope's structured
/// `denials: [{ kind, target, reason }]` list, joining each entry's `reason`.
/// Falls back to a generic message when the list is missing or empty.
fn denial_reason(envelope: &Value) -> String {
    let reasons: Vec<String> = envelope
        .get("denials")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("reason").and_then(Value::as_str))
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

/// Parse a `tier` argument from the JSON-RPC call. Accepts the four
/// canonical names (case-insensitive) per the tools/list schema.
fn parse_tier(s: &str) -> anyhow::Result<Tier> {
    match s.to_ascii_uppercase().as_str() {
        "FAST" => Ok(Tier::Fast),
        "STANDARD" => Ok(Tier::Standard),
        "COMPLEX" => Ok(Tier::Complex),
        "REVIEW" => Ok(Tier::Review),
        other => anyhow::bail!("invalid tier: {other} (expected FAST|STANDARD|COMPLEX|REVIEW)"),
    }
}

/// Wire `goal_run`:
///   1. validate `prompt` (required)
///   2. validate `tier` override if present, else `Router::classify`
///   3. pick a backend from the registry for that tier
///   4. await `backend.complete(...)`
///   5. wrap the reply in the MCP content envelope, prefixed with the
///      backend's `model_id` so callers can see which model answered
async fn handle_goal_run(
    args: &Value,
    registry: &BackendRegistry,
    router: &Router,
) -> anyhow::Result<Value> {
    let prompt = args
        .get("prompt")
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: prompt"))?
        .to_string();

    let tier = match args.get("tier").and_then(|t| t.as_str()) {
        Some(s) => parse_tier(s)?,
        None => router.classify(&prompt),
    };

    let backend = registry
        .pick(tier)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let chat = newt_inference::ChatRequest::new().user(prompt);
    let reply = backend
        .complete(chat)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    Ok(mcp_text_content(&format!(
        "[{}] {}",
        reply.model_id, reply.content
    )))
}

/// Wrap a string in the MCP content envelope: `{ "content": [{ "type": "text", "text": ... }] }`.
fn mcp_text_content(text: &str) -> Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": text
        }]
    })
}

/// Wrap a reason in the MCP **tool error** envelope: the content shape plus
/// `isError: true`. This is what a leash denial looks like across the MCP
/// boundary — an in-band tool error the model can read, not a transport fault.
fn mcp_error_content(reason: &str) -> Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": reason
        }],
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helper ──────────────────────────────────────────────────────────────

    /// Build a fully-wired McpServer with an empty registry and default
    /// router, then send a single request through it.
    async fn rpc(request: &Value) -> Value {
        rpc_with(
            Arc::new(BackendRegistry::new()),
            Arc::new(Router::new()),
            request,
        )
        .await
    }

    /// Like [`rpc`], but with a caller-supplied registry and router so
    /// goal_run tests can swap in a mock backend.
    async fn rpc_with(
        registry: Arc<BackendRegistry>,
        router: Arc<Router>,
        request: &Value,
    ) -> Value {
        let mut server = McpServer::new();
        register_handlers(&mut server, registry, router);

        let input = format!("{}\n", serde_json::to_string(request).unwrap());
        let mut output: Vec<u8> = Vec::new();
        server.run(input.as_bytes(), &mut output).await.unwrap();
        let text = String::from_utf8(output).unwrap();
        serde_json::from_str(text.trim()).unwrap()
    }

    /// Like [`rpc`], but wires `tools/call` with an EXPLICIT bridle leash so a
    /// test can confine `shell_run` deterministically — independent of the
    /// host's `~/.newt/config.toml`. (`register_handlers` would source the
    /// granted Caveats from the real home dir, which a test must not depend
    /// on.) This drives `register_tools_call` directly with a chosen grant.
    async fn rpc_with_caveats(granted: Caveats, request: &Value) -> Value {
        let mut server = McpServer::new();
        register_initialize(&mut server);
        register_tools_list(&mut server);
        register_tools_call(
            &mut server,
            Arc::new(BackendRegistry::new()),
            Arc::new(Router::new()),
            Arc::new(agent_bridle::registry()),
            Arc::new(granted),
        );

        let input = format!("{}\n", serde_json::to_string(request).unwrap());
        let mut output: Vec<u8> = Vec::new();
        server.run(input.as_bytes(), &mut output).await.unwrap();
        let text = String::from_utf8(output).unwrap();
        serde_json::from_str(text.trim()).unwrap()
    }

    /// A restrictive grant: only `echo` may exec, capped call budget. Used by
    /// the superseded-`shell_run` regression tests below.
    fn echo_only_grant() -> Caveats {
        use agent_bridle::{CountBound, Scope};
        Caveats {
            exec: Scope::only(["echo".to_string()]),
            max_calls: CountBound::AtMost(8),
            ..Caveats::top()
        }
    }

    // ── initialize ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn initialize_returns_protocol_version() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .await;

        let result = &resp["result"];
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "newt-mcp-server");
        assert!(result["capabilities"]["tools"].is_object());
    }

    // ── tools/list ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tools_list_returns_expected_tools() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        }))
        .await;

        let tools = resp["result"]["tools"].as_array().unwrap();
        // At least the four core tools; optional feature sets add more.
        assert!(
            tools.len() >= 4,
            "expected at least 4 tools, got {}",
            tools.len()
        );

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"code_read"));
        assert!(names.contains(&"code_edit"));
        assert!(names.contains(&"code_search"));
        assert!(names.contains(&"goal_run"));
        assert!(names.contains(&"fs_list"));
        assert!(names.contains(&"shell_run"));
    }

    #[tokio::test]
    async fn tools_list_has_input_schemas() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}
        }))
        .await;

        let tools = resp["result"]["tools"].as_array().unwrap();
        for tool in tools {
            assert!(
                tool["inputSchema"].is_object(),
                "tool {} missing inputSchema",
                tool["name"]
            );
        }
    }

    // ── tools/call — code_read ──────────────────────────────────────────────

    #[tokio::test]
    async fn code_read_happy_path() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "hello from newt\n").unwrap();

        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {
                "name": "code_read",
                "arguments": { "path": tmp.path().to_str().unwrap() }
            }
        }))
        .await;

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello from newt"));
    }

    #[tokio::test]
    async fn code_read_missing_file() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 11, "method": "tools/call",
            "params": {
                "name": "code_read",
                "arguments": { "path": "/tmp/newt-mcp-no-such-file-xyz" }
            }
        }))
        .await;

        assert!(resp["error"].is_object(), "expected error, got: {resp}");
        assert_eq!(resp["error"]["code"], -32603);
    }

    // ── tools/call — git (embedded engine) ──────────────────────────────────

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        assert!(std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    fn temp_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        run_git(p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("a.txt"), "hello\n").unwrap();
        run_git(p, &["add", "a.txt"]);
        run_git(
            p,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-q",
                "-m",
                "first",
            ],
        );
        d
    }

    #[tokio::test]
    async fn git_status_returns_structured_json() {
        let repo = temp_repo();
        let resp = rpc_with_caveats(
            Caveats::top(),
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 40, "method": "tools/call",
                "params": {
                    "name": "git",
                    "arguments": { "op": "status", "repo": repo.path().to_str().unwrap() }
                }
            }),
        )
        .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["branch"], "main");
        assert_eq!(v["clean"], true);
    }

    #[tokio::test]
    async fn git_commit_denied_under_readonly_grant() {
        use agent_bridle::Scope;
        let repo = temp_repo();
        // fs_write empty -> GitCaveats::from_session denies stage/commit (fail-closed).
        let readonly = Caveats {
            fs_write: Scope::none(),
            ..Caveats::top()
        };
        let resp = rpc_with_caveats(
            readonly,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 41, "method": "tools/call",
                "params": {
                    "name": "git",
                    "arguments": {
                        "op": "commit",
                        "repo": repo.path().to_str().unwrap(),
                        "message": "should be refused"
                    }
                }
            }),
        )
        .await;
        assert!(
            resp["error"].is_object(),
            "read-only grant must refuse git commit, got: {resp}"
        );
    }

    // ── tools/call — code_search ────────────────────────────────────────────

    #[tokio::test]
    async fn code_search_happy_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "needle in hay\nhay only\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "more hay\nneedle again\n").unwrap();

        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 20, "method": "tools/call",
            "params": {
                "name": "code_search",
                "arguments": {
                    "query": "needle",
                    "path": tmp.path().to_str().unwrap()
                }
            }
        }))
        .await;

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("needle"), "expected hits, got: {text}");
        // Two files should have matches
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 hits, got: {text}");
    }

    #[tokio::test]
    async fn code_search_no_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "nothing here\n").unwrap();

        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 21, "method": "tools/call",
            "params": {
                "name": "code_search",
                "arguments": {
                    "query": "zzz_absent",
                    "path": tmp.path().to_str().unwrap()
                }
            }
        }))
        .await;

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "no matches");
    }

    // ── tools/call — code_edit ──────────────────────────────────────────────

    #[tokio::test]
    async fn code_edit_happy_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "line1\nline2\n").unwrap();

        let patch = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,2 +1,2 @@
 line1
-line2
+edited
";

        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 30, "method": "tools/call",
            "params": {
                "name": "code_edit",
                "arguments": {
                    "path": file.to_str().unwrap(),
                    "patch": patch
                }
            }
        }))
        .await;

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("patched"), "expected success, got: {text}");

        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, "line1\nedited\n");
    }

    #[tokio::test]
    async fn code_edit_bad_patch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "actual\n").unwrap();

        let patch = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,1 +1,1 @@
 WRONG_CONTEXT
";

        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 31, "method": "tools/call",
            "params": {
                "name": "code_edit",
                "arguments": {
                    "path": file.to_str().unwrap(),
                    "patch": patch
                }
            }
        }))
        .await;

        assert!(resp["error"].is_object(), "expected error, got: {resp}");
        assert_eq!(resp["error"]["code"], -32603);
    }

    // ── tools/call — unknown tool ───────────────────────────────────────────

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 50, "method": "tools/call",
            "params": {
                "name": "nonexistent_tool",
                "arguments": {}
            }
        }))
        .await;

        assert!(resp["error"].is_object(), "expected error, got: {resp}");
        assert_eq!(resp["error"]["code"], -32603);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    // ── tools/call — shell_run is now Caveats-confined (P1 regression) ──────
    //
    // These prove `shell_run` ENFORCES the granted leash. They would FAIL
    // against PR #125's unconfined `sh -c` implementation, which ran any `cmd`
    // with full ambient authority and never consulted a Caveats grant.

    /// REGRESSION (P1): with a grant that includes `echo`, an in-scope `echo`
    /// RUNS under the confined shell and returns its output. Built against the
    /// agent-bridle env-seam branch (#783) the bridle ships the REAL safe-subset
    /// shell (no stub), so the in-scope command succeeds (isError unset) — the
    /// "unavailable in this build" stub error is retired.
    #[tokio::test]
    async fn shell_run_in_scope_echo_succeeds() {
        let resp = rpc_with_caveats(
            echo_only_grant(),
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 60, "method": "tools/call",
                "params": {
                    "name": "shell_run",
                    "arguments": { "cmd": "echo bridled" }
                }
            }),
        )
        .await;

        assert!(
            resp["error"].is_null(),
            "result must be in-band, not a transport error: {resp}"
        );
        let result = &resp["result"];
        assert!(
            result["isError"].as_bool() != Some(true),
            "an in-scope echo must succeed, not error: {result}"
        );
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("bridled"),
            "the in-scope echo output must be returned: {text}"
        );
    }

    /// Out-of-scope `rm` is DENIED by the real safe-subset shell (env-seam
    /// branch, #783): it is not in the `echo`-only exec grant, so the confined
    /// shell refuses it with a capability denial lifted to an MCP `isError`
    /// (not the old stub "unavailable" error).
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_run_out_of_scope_rm_is_denied() {
        let resp = rpc_with_caveats(
            echo_only_grant(),
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 61, "method": "tools/call",
                "params": {
                    "name": "shell_run",
                    "arguments": { "cmd": "rm -rf /tmp/newt-bridle-should-not-run" }
                }
            }),
        )
        .await;

        assert!(
            resp["error"].is_null(),
            "stub result must be in-band, not a transport error: {resp}"
        );
        let result = &resp["result"];
        assert_eq!(
            result["isError"], true,
            "out-of-scope rm must surface as an MCP tool error: {result}"
        );
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("not within the granted authority"),
            "out-of-scope rm must surface a capability denial: {text}"
        );
    }

    /// REGRESSION (P1): detection is now driven by the STRUCTURED `denied`
    /// field, exercised directly against `is_denied` / `denial_reason`. This
    /// pins the contract that we read the machine-readable envelope, never
    /// parse stdout/stderr. (A regression to stderr-grepping would make these
    /// helpers ignore the `denied`/`denials` fields and break.)
    #[test]
    fn denial_detection_reads_structured_fields_not_stderr() {
        // A denial envelope: `denied: true`, even with an EMPTY stderr and a
        // stdout that merely mentions success — the structured field wins.
        let denied = serde_json::json!({
            "exit_code": 126,
            "stdout": "all good, nothing denied here",
            "stderr": "",
            "denied": true,
            "denials": [
                { "kind": "exec", "target": "rm", "reason": "rm is not within the granted authority" }
            ]
        });
        assert!(
            is_denied(&denied),
            "structured denied:true must be detected"
        );
        assert_eq!(
            denial_reason(&denied),
            "rm is not within the granted authority"
        );

        // A clean envelope whose stdout merely PRINTS denial-like words must NOT
        // be misread as a denial — the old stderr-grep hazard, now impossible.
        let clean = serde_json::json!({
            "exit_code": 0,
            "stdout": "execution denied is not within the granted authority",
            "stderr": "open denied",
        });
        assert!(
            !is_denied(&clean),
            "absence of denied:true must mean not denied, regardless of output"
        );

        // Multiple denials are joined.
        let multi = serde_json::json!({
            "denied": true,
            "denials": [
                { "kind": "exec", "target": "rm", "reason": "exec rm denied" },
                { "kind": "open", "target": "/etc/x", "reason": "open /etc/x denied" }
            ]
        });
        assert_eq!(denial_reason(&multi), "exec rm denied; open /etc/x denied");
    }

    /// REGRESSION (P1 — the exec-bypass close): an `exec`-style invocation
    /// (`rm <marker>` / `touch <marker>`) under the `echo`-only grant is DENIED
    /// AND the program genuinely does not run — proven by the marker file NOT
    /// being created. This case SLIPPED on the old `129a1adf` pin (pre
    /// exec-bypass fix), where a free-form `exec` could escape the leash; it
    /// passing here proves the bump to the hardened agent-bridle closed the
    /// bypass *through newt's shell_run*, with the denial seen via the
    /// structured field.
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_run_exec_bypass_is_denied_and_program_does_not_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join("exec-bypass-marker");
        let marker_str = marker.to_str().unwrap();
        assert!(!marker.exists(), "precondition: marker must not exist yet");

        // `touch <marker>` exercises an out-of-scope exec that, if it ran, would
        // create the marker. Under the echo-only grant it must be refused.
        let resp = rpc_with_caveats(
            echo_only_grant(),
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 65, "method": "tools/call",
                "params": {
                    "name": "shell_run",
                    "arguments": { "cmd": format!("touch {marker_str}") }
                }
            }),
        )
        .await;

        assert!(
            resp["error"].is_null(),
            "denial must be in-band, not transport: {resp}"
        );
        let result = &resp["result"];
        assert_eq!(
            result["isError"], true,
            "out-of-scope touch must be denied (isError): {result}"
        );
        // Confinement is REAL, not cosmetic: the program never ran, so the
        // marker was never created. This is the bit that would have failed on
        // the vulnerable `129a1adf` pin.
        assert!(
            !marker.exists(),
            "exec-bypass: touch must NOT have created the marker {marker_str}"
        );

        // And again with `rm` against a file we pre-create: the denial must
        // leave the file intact.
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "do not delete me").unwrap();
        let victim_str = victim.to_str().unwrap();
        let resp = rpc_with_caveats(
            echo_only_grant(),
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 66, "method": "tools/call",
                "params": {
                    "name": "shell_run",
                    "arguments": { "cmd": format!("rm {victim_str}") }
                }
            }),
        )
        .await;
        assert_eq!(
            resp["result"]["isError"], true,
            "out-of-scope rm must be denied: {}",
            resp["result"]
        );
        assert!(
            victim.exists(),
            "exec-bypass: rm must NOT have deleted the victim {victim_str}"
        );
    }

    /// A path-separator command (`/bin/rm`) is DENIED by the real safe-subset
    /// shell (env-seam branch, #783): the program is not in the `echo`-only exec
    /// grant, so the confined shell refuses it with a capability denial lifted to
    /// an MCP `isError` (not the old stub "unavailable" error).
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_run_bin_rm_path_separator_is_denied() {
        let resp = rpc_with_caveats(
            echo_only_grant(),
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 62, "method": "tools/call",
                "params": {
                    "name": "shell_run",
                    "arguments": { "cmd": "/bin/rm -rf /tmp/newt-bridle-should-not-run" }
                }
            }),
        )
        .await;

        let result = &resp["result"];
        assert_eq!(
            result["isError"], true,
            "path-separator rm must surface as an MCP tool error: {result}"
        );
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("not within the granted authority"),
            "path-separator rm must surface a capability denial: {text}"
        );
    }

    /// A missing `cmd` is an in-band tool error (matching the leash-denial
    /// shape), not a transport fault.
    #[tokio::test]
    async fn shell_run_missing_cmd_is_in_band_error() {
        let resp = rpc_with_caveats(
            echo_only_grant(),
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 63, "method": "tools/call",
                "params": { "name": "shell_run", "arguments": {} }
            }),
        )
        .await;

        assert!(
            resp["error"].is_null(),
            "should be in-band, not transport: {resp}"
        );
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cmd"));
    }

    /// The `shell_run` tools/list description now advertises the confinement.
    #[tokio::test]
    async fn shell_run_description_notes_caveats_confinement() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 64, "method": "tools/list", "params": {}
        }))
        .await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        let shell = tools
            .iter()
            .find(|t| t["name"] == "shell_run")
            .expect("shell_run present");
        let desc = shell["description"].as_str().unwrap();
        assert!(
            desc.contains("CAVEATS-CONFINED") || desc.to_lowercase().contains("caveats"),
            "shell_run description should note Caveats confinement: {desc}"
        );
    }

    // ── parse_tier — unit tests ─────────────────────────────────────────────

    #[test]
    fn parse_tier_canonical_names() {
        assert_eq!(parse_tier("FAST").unwrap(), Tier::Fast);
        assert_eq!(parse_tier("STANDARD").unwrap(), Tier::Standard);
        assert_eq!(parse_tier("COMPLEX").unwrap(), Tier::Complex);
        assert_eq!(parse_tier("REVIEW").unwrap(), Tier::Review);
    }

    #[test]
    fn parse_tier_is_case_insensitive() {
        assert_eq!(parse_tier("fast").unwrap(), Tier::Fast);
        assert_eq!(parse_tier("Complex").unwrap(), Tier::Complex);
    }

    #[test]
    fn parse_tier_rejects_unknown() {
        let err = parse_tier("BOGUS").unwrap_err().to_string();
        assert!(err.contains("invalid tier"), "got: {err}");
        assert!(err.contains("BOGUS"), "got: {err}");
    }
}
