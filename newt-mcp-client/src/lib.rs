//! Newt-Agent MCP client.
//!
//! Connects to the MCP servers resolved by [`newt_core::mcp`] and reads their
//! tool lists. It speaks JSON-RPC 2.0 over two transports behind a [`Transport`]
//! seam — **stdio** (spawned subprocess) and **streamable-HTTP** (`POST` with a
//! JSON or SSE response, MCP protocol revision 2025-03-26) — so the protocol
//! logic is written once. The legacy SSE-only transport is not implemented.
//! Tools from different servers are namespaced `server__tool` (see
//! [`namespaced`]) so two servers exposing the same tool name do not collide.
//!
//! The protocol logic ([`McpConnection`]) is generic over [`Transport`] and so
//! is unit-tested against an in-memory mock — no subprocess needed.

use anyhow::{anyhow, Context, Result};
use newt_core::mcp::{McpServerEntry, TransportKind};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// MCP protocol version we advertise (matches `newt-mcp-server`).
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Default per-request timeout — a wedged server must not hang the agent. A
/// server whose tools legitimately run long overrides this per entry via
/// `McpServerEntry::request_timeout_secs`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Ceiling for a configured override. Even a deliberately patient server keeps
/// the "must not hang the agent forever" guarantee — a genuinely wedged call
/// still gives up here.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// The `server__tool` namespacing separator.
pub const NS_SEP: &str = "__";

/// Resolve a server entry's per-request timeout: its `request_timeout_secs`
/// override clamped to `[1s, MAX_REQUEST_TIMEOUT]`, or [`DEFAULT_REQUEST_TIMEOUT`]
/// when unset. A `0` override is treated as 1s (never "no timeout").
#[must_use]
pub fn resolve_timeout(entry: &McpServerEntry) -> Duration {
    match entry.request_timeout_secs {
        None => DEFAULT_REQUEST_TIMEOUT,
        Some(secs) => Duration::from_secs(secs.max(1)).min(MAX_REQUEST_TIMEOUT),
    }
}

/// A line-oriented JSON-RPC transport: one JSON message per line.
///
/// Uses native `async fn` in traits (Rust ≥1.75). `McpConnection` is generic
/// over it (static dispatch), so there is no `dyn` requirement and the missing
/// `Send` auto-trait bound the lint warns about is moot here — the connection is
/// driven sequentially on one task, never sent across threads.
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// Send one serialized JSON message (the impl appends the newline framing).
    async fn send(&mut self, line: String) -> Result<()>;
    /// Receive the next line, or `None` at end of stream.
    async fn recv(&mut self) -> Result<Option<String>>;
}

/// A tool advertised by a remote MCP server.
#[derive(Debug, Clone)]
pub struct RemoteTool {
    /// The tool's remote (un-namespaced) name.
    pub name: String,
    /// Human-readable description (may be empty).
    pub description: String,
    /// The tool's JSON input schema.
    pub input_schema: Value,
}

/// One MCP server connection over a [`Transport`].
pub struct McpConnection<T: Transport> {
    transport: T,
    next_id: u64,
    /// Per-request read timeout (see [`resolve_timeout`]).
    timeout: Duration,
}

impl<T: Transport> McpConnection<T> {
    /// Wrap a transport with the [`DEFAULT_REQUEST_TIMEOUT`]. Call
    /// [`Self::initialize`] before issuing requests.
    pub fn new(transport: T) -> Self {
        Self::new_with_timeout(transport, DEFAULT_REQUEST_TIMEOUT)
    }

    /// Wrap a transport with an explicit per-request timeout (from
    /// [`resolve_timeout`]).
    pub fn new_with_timeout(transport: T, timeout: Duration) -> Self {
        Self {
            transport,
            next_id: 1,
            timeout,
        }
    }

    /// Send a request and await the response correlated by id, skipping
    /// notifications and any unrelated messages on the stream.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.transport.send(serde_json::to_string(&req)?).await?;

        loop {
            let line = tokio::time::timeout(self.timeout, self.transport.recv())
                .await
                .with_context(|| format!("timed out awaiting `{method}` response"))??
                .ok_or_else(|| anyhow!("server closed the connection during `{method}`"))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(line) else {
                continue; // not JSON (stray log line) — ignore
            };
            // Skip notifications (no id) and responses to other requests.
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                return Err(anyhow!("server error on `{method}`: {err}"));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Send a notification (no response expected).
    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let note = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.transport.send(serde_json::to_string(&note)?).await
    }

    /// Perform the MCP `initialize` handshake + `notifications/initialized`.
    pub async fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "newt", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// List the server's tools.
    pub async fn list_tools(&mut self) -> Result<Vec<RemoteTool>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                Some(RemoteTool {
                    name,
                    description: t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                })
            })
            .collect())
    }

    /// Call a tool by its remote (un-namespaced) name.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

/// Stdio transport: a spawned subprocess speaking newline-delimited JSON-RPC.
pub struct StdioTransport {
    /// Kept alive so the child is not reaped while we hold its pipes
    /// (`kill_on_drop` tears it down when this transport drops).
    _child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
}

impl StdioTransport {
    /// Spawn a stdio MCP server from a discovered entry. The child inherits the
    /// parent environment with `entry.env` overlaid; its stderr is discarded so
    /// server logging cannot corrupt the JSON-RPC stream.
    pub fn spawn(entry: &McpServerEntry) -> Result<Self> {
        let command = entry
            .command
            .as_deref()
            .ok_or_else(|| anyhow!("stdio MCP server `{}` has no command", entry.name))?;
        let mut child = Command::new(command)
            .args(&entry.args)
            .envs(&entry.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning MCP server `{}` ({command})", entry.name))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP server `{}`: no stdin pipe", entry.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP server `{}`: no stdout pipe", entry.name))?;
        Ok(Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
        })
    }
}

impl Transport for StdioTransport {
    async fn send(&mut self, line: String) -> Result<()> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        Ok(self.stdout.next_line().await?)
    }
}

/// Streamable-HTTP transport (MCP revision 2025-03-26).
///
/// Each [`Transport::send`] `POST`s one JSON-RPC message to the server's single
/// endpoint and buffers the reply(ies); [`Transport::recv`] drains that buffer.
/// This maps the request/response HTTP model onto the line-oriented
/// [`Transport`] seam without changing [`McpConnection`]:
///
/// - The server may answer a `POST` with either `application/json` (one
///   message) or `text/event-stream` (SSE — one or more `data:` messages);
///   both are buffered as JSON lines for `recv`.
/// - A notification (no id, e.g. `notifications/initialized`) gets a `202
///   Accepted` with no body — nothing to buffer.
/// - The server's `Mcp-Session-Id` response header (sent on `initialize`) is
///   captured and echoed on every subsequent request.
///
/// The per-request timeout lives on the HTTP client (the [`McpConnection`]
/// timeout wraps `recv`, but for HTTP the latency is in `send`).
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap,
    session_id: Option<String>,
    /// JSON-RPC messages parsed from `POST` responses, awaiting `recv`.
    inbox: VecDeque<String>,
}

impl HttpTransport {
    /// Build a streamable-HTTP transport from a discovered entry. Configured
    /// `entry.headers` (e.g. `Authorization: Bearer …`) are sent on every
    /// request. Does no network I/O — the handshake happens in `initialize`.
    pub fn connect(entry: &McpServerEntry) -> Result<Self> {
        let url = entry
            .url
            .clone()
            .ok_or_else(|| anyhow!("http MCP server `{}` has no url", entry.name))?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (key, value) in &entry.headers {
            let name =
                reqwest::header::HeaderName::from_bytes(key.as_bytes()).with_context(|| {
                    format!("MCP server `{}`: invalid header name `{key}`", entry.name)
                })?;
            let val = reqwest::header::HeaderValue::from_str(value).with_context(|| {
                format!(
                    "MCP server `{}`: invalid value for header `{key}`",
                    entry.name
                )
            })?;
            headers.insert(name, val);
        }
        let client = reqwest::Client::builder()
            .timeout(resolve_timeout(entry))
            .build()
            .with_context(|| format!("building HTTP client for MCP server `{}`", entry.name))?;
        Ok(Self {
            client,
            url,
            headers,
            session_id: None,
            inbox: VecDeque::new(),
        })
    }
}

impl Transport for HttpTransport {
    async fn send(&mut self, line: String) -> Result<()> {
        use reqwest::header::{ACCEPT, CONTENT_TYPE};
        let mut req = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(line);
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req.send().await.context("MCP HTTP request failed")?;

        // Capture the session id from the initialize response for later calls.
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }

        let status = resp.status();
        let is_sse = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("text/event-stream"))
            .unwrap_or(false);
        let body = resp
            .text()
            .await
            .context("reading MCP HTTP response body")?;

        if !status.is_success() {
            return Err(anyhow!(
                "MCP server returned HTTP {status}: {}",
                body.trim()
            ));
        }
        if is_sse {
            self.inbox.extend(parse_sse_messages(&body));
        } else if !body.trim().is_empty() {
            // A `202 Accepted` notification ack has an empty body — skip it.
            self.inbox.push_back(body);
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        Ok(self.inbox.pop_front())
    }
}

/// Parse the `data:` payloads out of an SSE response body. Each blank-line-
/// delimited event contributes one message (multiple `data:` lines in an event
/// are joined with `\n`, per the SSE spec); non-`data` fields and comments are
/// ignored. Returns the messages in order.
fn parse_sse_messages(body: &str) -> Vec<String> {
    let mut messages = Vec::new();
    let mut data = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            // SSE strips exactly one optional leading space after the colon.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        } else if line.is_empty() && !data.is_empty() {
            messages.push(std::mem::take(&mut data));
        }
    }
    if !data.is_empty() {
        messages.push(data);
    }
    messages
}

/// A [`Transport`] that is either stdio or streamable-HTTP, chosen per server.
///
/// An enum (rather than `Box<dyn Transport>`) keeps static dispatch — the
/// trait's `async fn`s are not object-safe — and lets one `Vec<ConnectedServer>`
/// hold a mix of transports.
pub enum AnyTransport {
    Stdio(Box<StdioTransport>),
    Http(HttpTransport),
}

impl Transport for AnyTransport {
    async fn send(&mut self, line: String) -> Result<()> {
        match self {
            Self::Stdio(t) => t.send(line).await,
            Self::Http(t) => t.send(line).await,
        }
    }
    async fn recv(&mut self) -> Result<Option<String>> {
        match self {
            Self::Stdio(t) => t.recv().await,
            Self::Http(t) => t.recv().await,
        }
    }
}

/// A connected server and the tools it advertised.
pub struct ConnectedServer {
    /// The configured server name (the namespace prefix).
    pub name: String,
    /// The live connection (for [`McpConnection::call_tool`]).
    pub conn: McpConnection<AnyTransport>,
    /// Tools discovered via `tools/list`.
    pub tools: Vec<RemoteTool>,
}

/// Initialize a transport and list its tools into a [`ConnectedServer`].
async fn finish_connect(
    entry: &McpServerEntry,
    transport: AnyTransport,
) -> Result<ConnectedServer> {
    let timeout = resolve_timeout(entry);
    let mut conn = McpConnection::new_with_timeout(transport, timeout);
    tokio::time::timeout(timeout, conn.initialize())
        .await
        .with_context(|| format!("initializing MCP server `{}`", entry.name))??;
    let tools = conn
        .list_tools()
        .await
        .with_context(|| format!("listing tools for MCP server `{}`", entry.name))?;
    Ok(ConnectedServer {
        name: entry.name.clone(),
        conn,
        tools,
    })
}

/// Connect to one discovered **stdio** server: spawn, initialize, list tools.
pub async fn connect_stdio(entry: &McpServerEntry) -> Result<ConnectedServer> {
    if entry.transport != TransportKind::Stdio {
        return Err(anyhow!(
            "server `{}`: connect_stdio called for a non-stdio transport",
            entry.name
        ));
    }
    finish_connect(
        entry,
        AnyTransport::Stdio(Box::new(StdioTransport::spawn(entry)?)),
    )
    .await
}

/// Connect to one discovered **streamable-HTTP** server: dial, initialize, list
/// tools. Use this for `TransportKind::Http` entries (the legacy SSE-only
/// transport is not supported).
pub async fn connect_http(entry: &McpServerEntry) -> Result<ConnectedServer> {
    if entry.transport != TransportKind::Http {
        return Err(anyhow!(
            "server `{}`: connect_http called for a non-http transport",
            entry.name
        ));
    }
    finish_connect(entry, AnyTransport::Http(HttpTransport::connect(entry)?)).await
}

/// Namespace a remote tool name as `server__tool`.
pub fn namespaced(server: &str, tool: &str) -> String {
    format!("{server}{NS_SEP}{tool}")
}

/// Split a `server__tool` name back into `(server, tool)`. Returns `None` if the
/// separator is absent.
pub fn split_namespaced(qualified: &str) -> Option<(&str, &str)> {
    qualified.split_once(NS_SEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// In-memory transport: discards sends, returns canned lines in order.
    struct MockTransport {
        responses: VecDeque<String>,
    }

    impl MockTransport {
        fn new(lines: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                responses: lines.into_iter().map(str::to_string).collect(),
            }
        }
    }

    impl Transport for MockTransport {
        async fn send(&mut self, _line: String) -> Result<()> {
            Ok(())
        }
        async fn recv(&mut self) -> Result<Option<String>> {
            Ok(self.responses.pop_front())
        }
    }

    #[tokio::test]
    async fn initialize_then_list_tools_parses_entries() {
        // id 1 = initialize, id 2 = tools/list (notify carries no id/response).
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"find","inputSchema":{"type":"object"}}]}}"#,
        ]));
        conn.initialize().await.unwrap();
        let tools = conn.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description, "find");
    }

    #[tokio::test]
    async fn request_skips_notifications_and_mismatched_ids() {
        // A log notification (no id) and a stale response (wrong id) precede ours.
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":99,"result":{"stale":true}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
        ]));
        // First request → id 1; must skip the first two lines.
        let tools = conn.list_tools().await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn server_error_is_surfaced() {
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#,
        ]));
        let err = conn.list_tools().await.unwrap_err();
        assert!(err.to_string().contains("method not found"), "{err}");
    }

    #[tokio::test]
    async fn closed_stream_is_an_error_not_a_hang() {
        let mut conn = McpConnection::new(MockTransport::new([])); // EOF immediately
        let err = conn.list_tools().await.unwrap_err();
        assert!(err.to_string().contains("closed the connection"), "{err}");
    }

    #[test]
    fn namespacing_roundtrips() {
        assert_eq!(namespaced("git", "status"), "git__status");
        assert_eq!(split_namespaced("git__status"), Some(("git", "status")));
        assert_eq!(split_namespaced("nounsep"), None);
    }

    #[test]
    fn parse_sse_extracts_data_messages_in_order() {
        let body = "event: message\ndata: {\"id\":1}\n\nevent: message\ndata: {\"id\":2}\n\n";
        assert_eq!(parse_sse_messages(body), vec!["{\"id\":1}", "{\"id\":2}"]);
    }

    #[test]
    fn parse_sse_joins_multiline_data_and_ignores_other_fields() {
        // Two data lines in one event join with '\n'; `id:`/comments are skipped.
        let body = ": keep-alive\nid: 7\ndata: {\"a\":1,\ndata: \"b\":2}\n\n";
        assert_eq!(parse_sse_messages(body), vec!["{\"a\":1,\n\"b\":2}"]);
    }

    #[test]
    fn parse_sse_handles_trailing_event_without_blank_line() {
        let body = "data: {\"only\":true}";
        assert_eq!(parse_sse_messages(body), vec!["{\"only\":true}"]);
        assert!(parse_sse_messages("").is_empty());
    }

    /// Build an entry carrying just a `request_timeout_secs` override (all other
    /// fields default) — every field is `#[serde(default)]`.
    fn entry_with_timeout(json: &str) -> McpServerEntry {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn resolve_timeout_defaults_when_unset() {
        assert_eq!(
            resolve_timeout(&entry_with_timeout("{}")),
            DEFAULT_REQUEST_TIMEOUT
        );
    }

    #[test]
    fn resolve_timeout_honors_override_and_camel_alias() {
        assert_eq!(
            resolve_timeout(&entry_with_timeout(r#"{"request_timeout_secs":180}"#)),
            Duration::from_secs(180)
        );
        // Claude-format JSON uses the camelCase alias.
        assert_eq!(
            resolve_timeout(&entry_with_timeout(r#"{"requestTimeoutSecs":45}"#)),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn resolve_timeout_clamps_zero_up_and_huge_down() {
        // 0 must never mean "no timeout".
        assert_eq!(
            resolve_timeout(&entry_with_timeout(r#"{"request_timeout_secs":0}"#)),
            Duration::from_secs(1)
        );
        // An over-large value is capped so a wedged call still gives up.
        assert_eq!(
            resolve_timeout(&entry_with_timeout(r#"{"request_timeout_secs":999999}"#)),
            MAX_REQUEST_TIMEOUT
        );
    }

    /// A transport whose `recv` never resolves — stands in for a wedged server.
    struct HangingTransport;
    impl Transport for HangingTransport {
        async fn send(&mut self, _line: String) -> Result<()> {
            Ok(())
        }
        async fn recv(&mut self) -> Result<Option<String>> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn request_gives_up_after_the_configured_timeout() {
        // Virtual clock (start_paused) auto-advances when idle, so the configured
        // 5s deadline fires deterministically with no real wall-clock spent.
        let mut conn = McpConnection::new_with_timeout(HangingTransport, Duration::from_secs(5));
        let err = conn.list_tools().await.unwrap_err();
        assert!(
            err.to_string().contains("timed out awaiting `tools/list`"),
            "{err}"
        );
    }
}
