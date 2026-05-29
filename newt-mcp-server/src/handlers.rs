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

use newt_core::router::{Router, Tier};
use newt_inference::BackendRegistry;
use serde_json::Value;

use crate::server::McpServer;

/// Register the core MCP protocol handlers on `server`.
///
/// `registry` and `router` are wired into the `goal_run` handler;
/// every other handler ignores them.
pub fn register_handlers(
    server: &mut McpServer,
    registry: Arc<BackendRegistry>,
    router: Arc<Router>,
) {
    register_initialize(server);
    register_tools_list(server);
    register_tools_call(server, registry, router);
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
    serde_json::json!([
        {
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
        },
        {
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
        },
        {
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
        },
        {
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
        }
    ])
}

// ── tools/call ─────────────────────────────────────────────────────────────

fn register_tools_call(
    server: &mut McpServer,
    registry: Arc<BackendRegistry>,
    router: Arc<Router>,
) {
    server.register("tools/call", move |params| {
        // Move clones into the async block so each invocation owns its
        // own Arc (the outer closure is `Fn`, not `FnOnce`).
        let registry = registry.clone();
        let router = router.clone();
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
                other => anyhow::bail!("unknown tool: {other}"),
            }
        }
    });
}

// ── Tool implementations ───────────────────────────────────────────────────

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
    async fn tools_list_returns_four_tools() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        }))
        .await;

        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"code_read"));
        assert!(names.contains(&"code_edit"));
        assert!(names.contains(&"code_search"));
        assert!(names.contains(&"goal_run"));
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
