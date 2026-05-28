//! MCP protocol handlers: initialize, tools/list, and tools/call.
//!
//! Registers all JSON-RPC methods that the newt MCP server exposes.

use std::path::Path;

use serde_json::Value;

use crate::server::McpServer;

/// Register the core MCP protocol handlers on `server`.
pub fn register_handlers(server: &mut McpServer) {
    register_initialize(server);
    register_tools_list(server);
    register_tools_call(server);
}

// ── initialize ─────────────────────────────────────────────────────────────

fn register_initialize(server: &mut McpServer) {
    server.register("initialize", |_params| {
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
    server.register("tools/list", |_params| {
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

fn register_tools_call(server: &mut McpServer) {
    server.register("tools/call", |params| {
        let name = params
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("");
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));

        match name {
            "code_read" => handle_code_read(&arguments),
            "code_edit" => handle_code_edit(&arguments),
            "code_search" => handle_code_search(&arguments),
            "goal_run" => handle_goal_run(&arguments),
            _ => anyhow::bail!("unknown tool: {name}"),
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

fn handle_goal_run(_args: &Value) -> anyhow::Result<Value> {
    // Placeholder — wiring BackendRegistry requires async handlers.
    // For v0, signal that the tool exists but isn't connected yet.
    Ok(mcp_text_content(
        "goal_run is not yet wired to an inference backend",
    ))
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

    /// Build a fully-wired McpServer and send a single request through it.
    async fn rpc(request: &Value) -> Value {
        let mut server = McpServer::new();
        register_handlers(&mut server);

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

        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
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
}
