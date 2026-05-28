//! Stdio JSON-RPC 2.0 server for MCP.
//!
//! Generic over reader/writer so tests can use in-memory streams
//! instead of stdin/stdout.

use std::collections::HashMap;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A synchronous handler: receives `params` and returns a result or error.
type Handler = Box<dyn Fn(Value) -> anyhow::Result<Value> + Send + Sync>;

/// Minimal JSON-RPC 2.0 server that dispatches by method name.
pub struct McpServer {
    handlers: HashMap<String, Handler>,
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a handler for a JSON-RPC method.
    pub fn register(
        &mut self,
        method: &str,
        handler: impl Fn(Value) -> anyhow::Result<Value> + Send + Sync + 'static,
    ) {
        self.handlers.insert(method.to_string(), Box::new(handler));
    }

    /// Run the server over stdin/stdout.
    pub async fn run_stdio(&self) -> anyhow::Result<()> {
        self.run(tokio::io::stdin(), tokio::io::stdout()).await
    }

    /// Run the server over arbitrary async reader/writer.
    ///
    /// Reads newline-delimited JSON-RPC requests from `reader`,
    /// dispatches to registered handlers, writes JSON-RPC responses
    /// to `writer`.
    pub async fn run<R, W>(&self, reader: R, mut writer: W) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let buf = BufReader::new(reader);
        let mut lines = buf.lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            let request: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    let resp = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    });
                    write_response(&mut writer, &resp).await?;
                    continue;
                }
            };

            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let params = request.get("params").cloned().unwrap_or(Value::Null);

            let response = match self.handlers.get(method) {
                Some(handler) => match handler(params) {
                    Ok(result) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    }),
                    Err(e) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32603, "message": e.to_string() }
                    }),
                },
                None => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("Method not found: {method}") }
                }),
            };

            write_response(&mut writer, &response).await?;
        }

        Ok(())
    }
}

/// Write a JSON-RPC response as a single newline-terminated line.
async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response: &Value,
) -> anyhow::Result<()> {
    let mut out = serde_json::to_string(response)?;
    out.push('\n');
    writer.write_all(out.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: send one JSON-RPC request through the server and return the parsed response.
    async fn roundtrip(server: &McpServer, request: &Value) -> Value {
        let input = format!("{}\n", serde_json::to_string(request).unwrap());
        let mut output: Vec<u8> = Vec::new();

        server.run(input.as_bytes(), &mut output).await.unwrap();

        let response_str = String::from_utf8(output).unwrap();
        serde_json::from_str(response_str.trim()).unwrap()
    }

    /// Helper: send raw bytes through the server and return the parsed response.
    async fn roundtrip_raw(server: &McpServer, raw: &str) -> Value {
        let mut output: Vec<u8> = Vec::new();

        server.run(raw.as_bytes(), &mut output).await.unwrap();

        let response_str = String::from_utf8(output).unwrap();
        serde_json::from_str(response_str.trim()).unwrap()
    }

    #[tokio::test]
    async fn echo_handler() {
        let mut server = McpServer::new();
        server.register("echo", Ok);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "echo",
            "params": { "msg": "hello" }
        });

        let resp = roundtrip(&server, &request).await;
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["msg"], "hello");
        assert_eq!(resp["jsonrpc"], "2.0");
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let server = McpServer::new();

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "nonexistent"
        });

        let resp = roundtrip(&server, &request).await;
        assert_eq!(resp["error"]["code"], -32601);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nonexistent"));
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let server = McpServer::new();
        let resp = roundtrip_raw(&server, "{{{{not json}}}}\n").await;
        assert_eq!(resp["error"]["code"], -32700);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Parse error"));
    }

    #[tokio::test]
    async fn handler_error_returns_internal_error() {
        let mut server = McpServer::new();
        server.register("fail", |_| anyhow::bail!("something broke"));

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "fail"
        });

        let resp = roundtrip(&server, &request).await;
        assert_eq!(resp["error"]["code"], -32603);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("something broke"));
    }

    #[tokio::test]
    async fn blank_lines_skipped() {
        let mut server = McpServer::new();
        server.register("ping", |_| Ok(serde_json::json!("pong")));

        let input = format!(
            "\n\n{}\n\n",
            serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "ping"
            }))
            .unwrap()
        );

        let mut output: Vec<u8> = Vec::new();
        server.run(input.as_bytes(), &mut output).await.unwrap();

        let response_str = String::from_utf8(output).unwrap();
        let resp: Value = serde_json::from_str(response_str.trim()).unwrap();
        assert_eq!(resp["result"], "pong");
    }

    #[tokio::test]
    async fn missing_id_defaults_to_null() {
        let server = McpServer::new();

        let input = "{\"jsonrpc\":\"2.0\",\"method\":\"missing\"}\n";
        let mut output: Vec<u8> = Vec::new();
        server.run(input.as_bytes(), &mut output).await.unwrap();

        let response_str = String::from_utf8(output).unwrap();
        let resp: Value = serde_json::from_str(response_str.trim()).unwrap();
        assert!(resp["id"].is_null());
    }
}
