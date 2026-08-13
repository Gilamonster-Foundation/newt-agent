//! Newt-Agent MCP client.
//!
//! Connects to the MCP servers resolved by [`newt_core::mcp`] and reads their
//! tool lists. It speaks JSON-RPC 2.0 over two transports behind a [`Transport`]
//! seam — **stdio** (spawned subprocess) and **streamable-HTTP** (`POST` with a
//! JSON or SSE response, through the latest handshake-era MCP revision) — so
//! the protocol logic is written once. The legacy SSE-only transport is not
//! implemented.
//! Tools from different servers are namespaced `server__tool` (see
//! [`namespaced`]) so two servers exposing the same tool name do not collide.
//!
//! The protocol logic ([`McpConnection`]) is generic over [`Transport`] and so
//! is unit-tested against an in-memory mock — no subprocess needed.
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:53 EDT | Date: 2026-08-12

use anyhow::{anyhow, Context, Result};
use newt_core::caveats::Caveats;
use newt_core::mcp::{McpServerEntry, TransportKind};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
// The OS-sandbox posture a stdio server achieved (honest record on
// `ConnectedServer`, surfaced by `/mcp`). Re-exported so consumers can name it
// without a direct agent-bridle dependency.
pub use agent_bridle::SandboxKind;

/// The network-egress posture a connected MCP server actually achieved (#1243
/// Leg 4). Honest — never over-claimed: `Gated(n)` means outbound traffic is
/// routed through the loopback egress proxy enforcing an `n`-host allow-list,
/// or an HTTP private origin is constrained to one approved hostname and its
/// screened, DNS-pinned address set (a non-granted host is refused, not
/// silently dialed); `Advisory` means neither fence is in force — either an
/// `All` net grant, or (for a spawned stdio child) a host where the loopback
/// fence is not emittable (e.g. Linux Landlock, which cannot address-fence), so
/// the child's egress is unmediated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetPosture {
    /// Egress fenced against an `n`-host allow-list or pinned origin set.
    Gated(usize),
    /// No egress proxy — outbound network is advisory only.
    Advisory,
}

/// The net posture for a connection: `Gated(host-count)` when the egress proxy
/// engaged or a private HTTP origin was DNS-pinned, else `Advisory`. The proxy
/// host count comes from [`agent_bridle::net_egress_proxy_hosts`]; a pinned
/// origin is exactly one host.
fn net_posture(caveats: &Caveats, proxied: bool, private_origin_pinned: bool) -> NetPosture {
    if private_origin_pinned {
        NetPosture::Gated(1)
    } else if proxied {
        NetPosture::Gated(
            agent_bridle::net_egress_proxy_hosts(caveats)
                .map(|h| h.len())
                .unwrap_or(0),
        )
    } else {
        NetPosture::Advisory
    }
}
// Confined stdio spawn (Unix): the child's stdio comes back as tokio pipe ends
// from `agent_bridle::ConfinedCommand::spawn_tokio`.
#[cfg(unix)]
use agent_bridle::{ConfinedCommand, ConfinedTokioChild, Gate, Tool, ToolContext, ToolResult};
#[cfg(unix)]
use tokio::net::unix::pipe;
// Non-Unix has no OS-sandbox spawn primitive yet, so the stdio child is spawned
// via tokio's process API (advisory confinement — env-scrubbed, no kernel jail).
#[cfg(not(unix))]
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Latest handshake-era MCP protocol version this client implements.
///
/// MCP 2026-07-28 is a separate stateless wire era that removes
/// `initialize`/`initialized`; adopting it requires a dedicated transport
/// implementation rather than pretending this stateful client negotiated it.
pub const PROTOCOL_VERSION: &str = "2025-11-25";
/// Explicit revisions with the same initialize/session wire family that Newt
/// has compatibility coverage for. Unknown server-selected revisions fail
/// closed during initialization.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Streamable HTTP did not exist in the 2024-11-05 revision. Accepting that
/// version over HTTP combines incompatible lifecycle and transport contracts.
pub const HTTP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
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
    /// Record the version selected by `initialize`. HTTP transports use it on
    /// every subsequent request; stdio transports have no protocol header.
    fn set_protocol_version(&mut self, _version: &str) -> Result<()> {
        Ok(())
    }
    /// Whether this concrete transport implements the server-selected version.
    fn supports_protocol_version(&self, version: &str) -> bool {
        SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
    }
    /// Whether the transport currently carries a server-issued session.
    fn has_session(&self) -> bool {
        false
    }
}

/// The server's self-reported identity from the required `serverInfo` member.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// The server's programmatic name.
    #[serde(default)]
    pub name: String,
    /// Human-facing display title (MCP 2025-06-18 addition).
    #[serde(default)]
    pub title: Option<String>,
    /// The server's version string.
    #[serde(default)]
    pub version: String,
}

/// What the `initialize` handshake reported back — previously discarded
/// (#1292 prerequisite). `newt mcp probe` derives a registration's name and
/// description from this; other callers may ignore it.
#[derive(Debug, Clone, Default)]
pub struct InitializeInfo {
    /// Valid initialization always supplies `serverInfo`; this remains optional
    /// for API compatibility with older serialized callers.
    pub server_info: Option<ServerInfo>,
    /// Server-authored usage `instructions`, when present.
    pub instructions: Option<String>,
    /// The raw server `capabilities` object — kept as `Value` because its
    /// shape varies by protocol revision.
    pub capabilities: Value,
    /// The negotiated `protocolVersion`.
    pub protocol_version: Option<String>,
}

/// A non-2xx HTTP response from an MCP endpoint, as a **typed** error so a
/// caller can match on the status (`newt mcp probe`'s "needs `newt auth`"
/// detection) instead of string-matching a message that could drift.
/// Downcast it out of an `anyhow` chain via `err.chain()`.
#[derive(Debug)]
pub struct HttpStatusError {
    /// The HTTP status code (e.g. `401`).
    pub status: u16,
    /// The canonical reason phrase (`Unauthorized`), possibly empty.
    reason: String,
}

impl HttpStatusError {
    #[must_use]
    pub fn new(status: u16, reason: &str, _body: &str) -> Self {
        Self {
            status,
            reason: reason.to_string(),
        }
    }
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The exact pre-typed wording — consumers log this text.
        write!(f, "MCP server returned HTTP {}", self.status)?;
        if !self.reason.is_empty() {
            write!(f, " {}", self.reason)?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpStatusError {}

/// Return the typed HTTP status carried anywhere in an `anyhow` context chain.
/// Server-authored body text is deliberately never considered for recovery.
#[must_use]
pub fn http_error_status(error: &anyhow::Error) -> Option<u16> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<HttpStatusError>()
            .map(|status| status.status)
    })
}

/// One bounded recovery decision for a long-lived streamable-HTTP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpRecoveryAction {
    /// Reconnect once because a server-issued session expired (typed 404).
    ReconnectExpiredSession,
    /// Reconnect once so a configured env/file/cmd Authorization reference is
    /// resolved again after a typed 401.
    ReconnectConfiguredAuthorization,
    /// Ask the owning OAuth layer to refresh its managed runtime bearer.
    RefreshRuntimeBearer,
    /// No safe recovery remains.
    Stop,
}

/// Per-call recovery budget. A call may reset one expired session and refresh
/// or re-resolve one credential. The owning operation state machine may replay
/// once after each consumed action, then must stop.
#[derive(Debug, Clone)]
pub struct HttpRecoveryBudget {
    session_reset_available: bool,
    credential_recovery_available: bool,
    has_runtime_bearer: bool,
    has_configured_authorization: bool,
}

impl HttpRecoveryBudget {
    #[must_use]
    pub fn new(
        had_session: bool,
        has_runtime_bearer: bool,
        has_configured_authorization: bool,
    ) -> Self {
        Self {
            session_reset_available: had_session,
            credential_recovery_available: has_runtime_bearer || has_configured_authorization,
            has_runtime_bearer,
            has_configured_authorization,
        }
    }

    /// Consume the one action permitted for this typed transport error.
    pub fn next(&mut self, error: &anyhow::Error) -> HttpRecoveryAction {
        match http_error_status(error) {
            Some(404) if self.session_reset_available => {
                self.session_reset_available = false;
                HttpRecoveryAction::ReconnectExpiredSession
            }
            Some(401) if self.credential_recovery_available && self.has_runtime_bearer => {
                self.credential_recovery_available = false;
                HttpRecoveryAction::RefreshRuntimeBearer
            }
            Some(401)
                if self.credential_recovery_available && self.has_configured_authorization =>
            {
                self.credential_recovery_available = false;
                HttpRecoveryAction::ReconnectConfiguredAuthorization
            }
            _ => HttpRecoveryAction::Stop,
        }
    }
}

/// Successful result of a bounded HTTP recovery, including the replacement
/// connection and bearer that must become the caller's new live state.
pub struct HttpCallRecovery<T, R> {
    /// Last successfully initialized connection, retained even when the final
    /// replay fails so callers do not restore an expired predecessor.
    pub connection: T,
    /// Bearer associated with `connection` after any bounded refresh.
    pub bearer: Option<String>,
    /// Replay outcome. An error means the recovery budget is exhausted.
    pub result: Result<R>,
}

/// Recover an HTTP tool operation as one state machine: reconnect, replay the
/// operation, and feed a typed replay error back through the remaining budget.
///
/// Keeping replay inside the machine matters for `404 -> reconnect -> 401`:
/// with a managed runtime bearer, the replay's 401 still owns the one credential
/// refresh. The final replay is never retried again after both bounded actions
/// have been consumed.
pub async fn recover_http_call_after_error<
    T,
    R,
    Refresh,
    RefreshFuture,
    Reconnect,
    ReconnectFuture,
    Replay,
    ReplayFuture,
>(
    initial_error: anyhow::Error,
    had_session: bool,
    runtime_bearer: Option<String>,
    configured_authorization: bool,
    mut refresh: Refresh,
    mut reconnect: Reconnect,
    mut replay: Replay,
) -> Result<Option<HttpCallRecovery<T, R>>>
where
    Refresh: FnMut(String) -> RefreshFuture,
    RefreshFuture: std::future::Future<Output = Option<String>>,
    Reconnect: FnMut(Option<String>) -> ReconnectFuture,
    ReconnectFuture: std::future::Future<Output = Result<T>>,
    Replay: FnMut(T) -> ReplayFuture,
    ReplayFuture: std::future::Future<Output = (T, Result<R>)>,
{
    let mut budget = HttpRecoveryBudget::new(
        had_session,
        runtime_bearer.is_some(),
        configured_authorization,
    );
    let mut active_bearer = runtime_bearer;
    let mut failure = initial_error;
    let mut recovery_attempted = false;
    let mut last_replay_connection: Option<(T, Option<String>)> = None;

    loop {
        let reconnect_bearer = match budget.next(&failure) {
            HttpRecoveryAction::ReconnectExpiredSession => active_bearer.clone(),
            HttpRecoveryAction::ReconnectConfiguredAuthorization => None,
            HttpRecoveryAction::RefreshRuntimeBearer => {
                let rejected = active_bearer
                    .clone()
                    .expect("runtime-bearer recovery requires a rejected bearer");
                let Some(refreshed) = refresh(rejected).await else {
                    if let Some((connection, bearer)) = last_replay_connection.take() {
                        return Ok(Some(HttpCallRecovery {
                            connection,
                            bearer,
                            result: Err(failure),
                        }));
                    }
                    return Err(failure);
                };
                active_bearer = Some(refreshed.clone());
                Some(refreshed)
            }
            HttpRecoveryAction::Stop => {
                if let Some((connection, bearer)) = last_replay_connection.take() {
                    return Ok(Some(HttpCallRecovery {
                        connection,
                        bearer,
                        result: Err(failure),
                    }));
                }
                return if recovery_attempted {
                    Err(failure)
                } else {
                    Ok(None)
                };
            }
        };
        recovery_attempted = true;

        let connection = match reconnect(reconnect_bearer).await {
            Ok(connection) => connection,
            Err(error) => {
                failure = error;
                continue;
            }
        };
        let (connection, result) = replay(connection).await;
        match result {
            Ok(result) => {
                return Ok(Some(HttpCallRecovery {
                    connection,
                    bearer: active_bearer,
                    result: Ok(result),
                }))
            }
            Err(error) => {
                last_replay_connection = Some((connection, active_bearer.clone()));
                failure = error;
            }
        }
    }
}

const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

async fn bounded_http_response_body(mut response: reqwest::Response) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_RESPONSE_BYTES as u64)
    {
        return Err(anyhow!("MCP HTTP response exceeded the 16 MiB limit"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("reading MCP HTTP response body")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_HTTP_RESPONSE_BYTES {
            return Err(anyhow!("MCP HTTP response exceeded the 16 MiB limit"));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).context("MCP HTTP response was not valid UTF-8")
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
    /// Connector-supplied MCP metadata retained for host-side routing policy.
    /// Catalog adapters copy only Newt-recognized keys and scrub them before
    /// inference-provider advertisement.
    pub meta: Option<Value>,
}

/// Adapt one remote MCP tool to the OpenAI function-tool catalog shape used by
/// both headless and TUI sessions. Only Newt's validated routing extension is
/// retained from connector `_meta`; model-facing catalogs scrub it later.
pub fn openai_tool_definition(
    server_name: &str,
    sanitize_server_names: bool,
    tool: &RemoteTool,
) -> Value {
    let mut definition = json!({
        "type": "function",
        "function": {
            "name": namespaced(&server_prefix(server_name, sanitize_server_names), &tool.name),
            "description": tool.description,
            "parameters": tool.input_schema,
        }
    });
    newt_core::preserve_mcp_resource_url_affinity(&mut definition, tool.meta.as_ref());
    definition
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

    /// Whether this live transport has a server-issued session identifier.
    #[must_use]
    pub fn has_session(&self) -> bool {
        self.transport.has_session()
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
            if let Some(error) = msg.get("error") {
                return match error.get("code").and_then(Value::as_i64) {
                    Some(code) => Err(anyhow!(
                        "MCP server error on `{method}` (JSON-RPC code {code})"
                    )),
                    None => Err(anyhow!("MCP server error on `{method}`")),
                };
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Send a notification (no response expected).
    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let note = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.transport.send(serde_json::to_string(&note)?).await
    }

    /// Perform the MCP `initialize` handshake + `notifications/initialized`,
    /// returning what the server reported about itself (previously discarded —
    /// the #1292 probe prerequisite).
    ///
    /// The result is **validated as a real handshake** before anything else:
    /// it must be a JSON object carrying `protocolVersion` (a string) and
    /// `capabilities` — both required in the spec's InitializeResult. Without
    /// this, any process that echoes stdin (`/bin/cat`) "initializes"
    /// successfully: the echoed request has our id and no `error`, so
    /// [`request`](Self::request) yields `Null` — and the probe/doctor would
    /// certify a non-server. A non-handshake result is a loud error, and no
    /// `notifications/initialized` is sent to it.
    pub async fn initialize(&mut self) -> Result<InitializeInfo> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "newt", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        let Some(object) = result.as_object() else {
            return Err(anyhow!(
                "not an MCP server: no valid initialize response (expected an object with \
                 `protocolVersion`, `capabilities`, and `serverInfo`)"
            ));
        };
        let protocol_version = object
            .get("protocolVersion")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "invalid MCP initialize result: `protocolVersion` must be a non-empty string"
                )
            })?
            .to_string();
        let capabilities = object
            .get("capabilities")
            .filter(|value| value.is_object())
            .ok_or_else(|| {
                anyhow!("invalid MCP initialize result: `capabilities` must be an object")
            })?
            .clone();
        let server_info: ServerInfo = object
            .get("serverInfo")
            .filter(|value| value.is_object())
            .cloned()
            .ok_or_else(|| anyhow!("invalid MCP initialize result: `serverInfo` must be an object"))
            .and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|_| anyhow!("invalid MCP initialize result: malformed `serverInfo`"))
            })?;
        if server_info.name.trim().is_empty() || server_info.version.trim().is_empty() {
            return Err(anyhow!(
                "invalid MCP initialize result: `serverInfo.name` and `serverInfo.version` must be non-empty strings"
            ));
        }
        if object
            .get("instructions")
            .is_some_and(|value| !value.is_string())
        {
            return Err(anyhow!(
                "invalid MCP initialize result: `instructions` must be a string"
            ));
        }
        if !self.transport.supports_protocol_version(&protocol_version) {
            return Err(anyhow!(
                "MCP server selected a protocol version unsupported by this transport"
            ));
        }
        self.transport.set_protocol_version(&protocol_version)?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(InitializeInfo {
            server_info: Some(server_info),
            instructions: object
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_string),
            capabilities,
            protocol_version: Some(protocol_version),
        })
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
                    meta: t.get("_meta").cloned(),
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

/// Assemble a confined stdio MCP child's **entire** environment as explicit
/// grants. A `ConfinedCommand` child starts env-EMPTY (the external-boundary
/// invariant), so everything the server needs must be granted explicitly.
///
/// Pure: the caller supplies the already-resolved inputs, so this is fully
/// unit-testable with no env/fs reads. Precedence is low→high (a later source
/// overrides an earlier same-named key):
/// 1. the closed passthrough allow-list ([`newt_core::mcp_stdio_env_passthrough`]
///    values read from the parent env — what a child needs to *execute*);
/// 2. the file-sourced `~/.newt/shell-env/` drop-in ([`newt_core::shell_env`],
///    #1243 Leg 2 — deliberate operator tokens whose values live in files);
/// 3. the server entry's own `env` map (server-specific config/secrets win).
fn assemble_env_grants(
    passthrough: &[(String, String)],
    shell_env: &BTreeMap<String, String>,
    entry_env: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in passthrough {
        map.insert(k.clone(), v.clone());
    }
    for (k, v) in shell_env {
        map.insert(k.clone(), v.clone());
    }
    for (k, v) in entry_env {
        map.insert(k.clone(), v.clone());
    }
    map.into_iter().collect()
}

/// Resolve every [`newt_core::mcp::SecretValue`] in an `env` / `headers` map to
/// its plaintext, **host-side** (in newt's own unconfined process), just before
/// the confined spawn / HTTP connect — **under the entry's trust boundary**
/// (#1301 security review, [`newt_core::mcp::resolve_secret_under_trust`]).
///
/// For a **trusted** (newt-owned config) entry a literal is `${...}`-interpolated
/// and a `{ env | file | cmd }` reference is resolved through the `SecretRef`
/// machinery. For an **untrusted** (discovered Claude/project overlay) entry a
/// literal passes to the child **verbatim** (never interpolated, so a `${cmd:…}`
/// is inert text — a hostile `.mcp.json` cannot run a host command) and a
/// structured reference is a hard error naming the server + key. The resolved
/// value is wrapped in `Secret` at every hop and exposed only into the grant map
/// — never logged, never written back to config, never placed in newt's own env.
fn resolve_entry_secrets(
    map: &BTreeMap<String, newt_core::mcp::SecretValue>,
    trust: newt_core::mcp::McpTrust,
    server: &str,
) -> Result<BTreeMap<String, String>> {
    map.iter()
        .map(|(k, v)| {
            let secret = newt_core::mcp::resolve_secret_under_trust(v, trust)
                .with_context(|| format!("MCP server `{server}`: resolving `{k}`"))?;
            Ok((k.clone(), secret.expose().to_string()))
        })
        .collect()
}

/// Resolve the three env-grant sources from the live environment (parent env +
/// the shell-env dir + the entry's own — now secret-resolved — env) and fold
/// them via [`assemble_env_grants`]. The impure edge — kept tiny so the assembly
/// logic itself stays pure/tested. Fails loudly if a configured secret reference
/// cannot be resolved.
fn resolve_env_grants(entry: &McpServerEntry) -> Result<Vec<(String, String)>> {
    let passthrough: Vec<(String, String)> = newt_core::mcp_stdio_env_passthrough()
        .iter()
        .filter_map(|k| {
            std::env::var_os(k).map(|v| (k.to_string(), v.to_string_lossy().into_owned()))
        })
        .collect();
    let shell_env = newt_core::Config::user_config_path()
        .map(|p| newt_core::shell_env::from_config_dir(&p))
        .unwrap_or_default();
    let entry_env = resolve_entry_secrets(&entry.env, entry.trust, &entry.name)
        .with_context(|| format!("resolving env secrets for MCP server `{}`", entry.name))?;
    Ok(assemble_env_grants(&passthrough, &shell_env, &entry_env))
}

/// A throwaway [`Tool`] used only to mint the spawn [`ToolContext`] through the
/// gate. The confined spawn admission-checks the *program*, not this tool's
/// name, so the identity is immaterial. Module-scoped (not a local type) so its
/// trivial trait impl is unit-testable.
#[cfg(unix)]
struct McpSpawnTool;

#[cfg(unix)]
#[async_trait::async_trait]
impl Tool for McpSpawnTool {
    fn name(&self) -> &str {
        "mcp_spawn"
    }
    fn schema(&self) -> Value {
        json!({})
    }
    async fn invoke(&self, _args: Value, _cx: &ToolContext) -> ToolResult<Value> {
        Ok(Value::Null)
    }
}

/// Mint the spawn [`ToolContext`] the only legitimate way — through the gate —
/// bounded by the session `caveats`.
#[cfg(unix)]
fn mint_spawn_context(caveats: &Caveats) -> Result<ToolContext> {
    Gate::new(0)
        .authorize(&McpSpawnTool, caveats)
        .map_err(|e| anyhow!("gate authorize failed: {e}"))
}

/// The session leash, widened to admit exec of THIS server's `command`.
///
/// A configured MCP server is operator-authorized infrastructure: the operator
/// declared it in their config, so *spawning it* must not require its command in
/// the session's exec allow-list (the agent never chose to run it). Only the
/// command itself is granted — the child's RUNTIME authority stays exactly the
/// session leash: `fs_write` remains Landlock-enforced, and `net` / the exec of
/// anything the server itself spawns are unchanged. An `exec: All` leash is
/// already unrestricted, so it is left untouched.
#[cfg(unix)]
fn spawn_caveats(session: &Caveats, command: &str) -> Caveats {
    use newt_core::caveats::Scope;
    let mut caveats = session.clone();
    if let Scope::Only(ref mut set) = caveats.exec {
        set.extend([command.to_string()]);
    }
    caveats
}

/// Log the confinement actually achieved — honest, never over-claimed.
/// [`SandboxKind::None`] means the leash on this child is advisory only (no OS
/// sandbox enforced it on this host). Surfacing this in `/mcp` is a follow-up.
#[cfg(unix)]
fn log_confinement(name: &str, kind: SandboxKind) {
    if kind == SandboxKind::None {
        tracing::warn!(
            "MCP server `{name}`: spawned ADVISORY-only — no OS sandbox enforced the session \
             leash on this host (restrictions are not kernel-confined)"
        );
    } else {
        tracing::info!("MCP server `{name}`: spawned confined ({kind:?})");
    }
}

/// Stdio transport: a spawned subprocess speaking newline-delimited JSON-RPC.
///
/// On Unix the child is launched through [`agent_bridle::ConfinedCommand`] so it
/// runs *inside* the same OCAP boundary as `run_command` — the exec admission-
/// check, the OS sandbox (Landlock/Seatbelt), and the env scrub all apply
/// (#1243 Leg 3). Its stdio is the tokio pipe ends of a kill-on-drop
/// [`ConfinedTokioChild`].
#[cfg(unix)]
pub struct StdioTransport {
    /// Kept alive so the child is killed and reaped when this transport drops
    /// (`ConfinedTokioChild`'s kill-on-drop).
    _child: ConfinedTokioChild,
    stdin: pipe::Sender,
    stdout: tokio::io::Lines<BufReader<pipe::Receiver>>,
    /// The OS sandbox actually applied to the child (honest posture for `/mcp`).
    sandbox_kind: SandboxKind,
}

/// Stdio transport (non-Unix): `ConfinedCommand::spawn_tokio` (Landlock/Seatbelt)
/// is Unix-only, so the child is spawned via tokio's process API with a scrubbed
/// environment but WITHOUT an OS sandbox — advisory confinement (see #1255 honest
/// limitations; Windows AppContainer pipe bridging is a future concern).
#[cfg(not(unix))]
pub struct StdioTransport {
    /// Kept alive so the child is not reaped while we hold its pipes
    /// (`kill_on_drop` tears it down when this transport drops).
    _child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
    /// Always `None` off Unix — no OS sandbox confined the spawn here.
    sandbox_kind: SandboxKind,
}

impl StdioTransport {
    /// The OS sandbox actually applied to this stdio child — the honest
    /// confinement posture surfaced by `/mcp`. [`SandboxKind::None`] means the
    /// leash was advisory only (a `top()` grant, or a host without the sandbox).
    #[must_use]
    pub fn sandbox_kind(&self) -> SandboxKind {
        self.sandbox_kind
    }

    /// Whether the child's network egress is fenced through the loopback proxy
    /// (#1243 Leg 4). `spawn_tokio` engages the proxy automatically under a
    /// remote-host `net` grant — but ONLY where the loopback fence is emittable
    /// (macOS Seatbelt today; Linux Landlock cannot address-fence, so it stays
    /// `false` there and the child's egress is honestly advisory). Always
    /// `false` off Unix.
    #[must_use]
    pub fn egress_proxied(&self) -> bool {
        #[cfg(unix)]
        {
            self._child.egress_proxied()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// The off-allow-list hosts the child tried to reach through the proxy and
    /// was refused (#196) — the exfil-attempt signal, empty when unproxied.
    #[must_use]
    pub fn refused_hosts(&self) -> Vec<String> {
        #[cfg(unix)]
        {
            self._child.refused_hosts()
        }
        #[cfg(not(unix))]
        {
            Vec::new()
        }
    }
}

#[cfg(unix)]
impl StdioTransport {
    /// Spawn a stdio MCP server **confined** by the session `caveats`.
    ///
    /// The child runs inside the same OCAP boundary as `run_command`: its
    /// environment starts EMPTY and is rebuilt from explicit grants
    /// ([`assemble_env_grants`]) — never newt's full inherited environment
    /// (#1155) — and `agent_bridle::ConfinedCommand::spawn_tokio` applies the
    /// exec admission-check, the OS sandbox, and fails closed if a restricted fs
    /// axis cannot be kernel-enforced. `stderr` is discarded so server logging
    /// cannot corrupt the JSON-RPC stream.
    pub fn spawn(admitted: &newt_core::mcp::AdmittedServer<'_>, caveats: &Caveats) -> Result<Self> {
        // Admission is a compile-time precondition of a spawn: the only way to
        // hold an `AdmittedServer` is a successful `newt_core::mcp::admit`, so a
        // disabled or untrusted entry cannot reach this constructor (the witness
        // is unforgeable — private field). #1562 / step-1.2.
        let entry = admitted.entry();
        let command = entry
            .command
            .as_deref()
            .ok_or_else(|| anyhow!("stdio MCP server `{}` has no command", entry.name))?;
        let grants = resolve_env_grants(entry)?;
        // Admit exec of the configured server command; keep its runtime authority
        // (fs/net) the session leash.
        let cx = mint_spawn_context(&spawn_caveats(caveats, command)).with_context(|| {
            format!("authorizing confined spawn of MCP server `{}`", entry.name)
        })?;

        let mut cmd = ConfinedCommand::new(command).args(&entry.args);
        for (k, v) in &grants {
            cmd = cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn_tokio(&cx)
            .with_context(|| {
                format!("spawning MCP server `{}` ({command}) confined", entry.name)
            })?;
        let sandbox_kind = child.sandbox_kind;
        log_confinement(&entry.name, sandbox_kind);

        let stdin = child
            .take_stdin()
            .ok_or_else(|| anyhow!("MCP server `{}`: no stdin pipe", entry.name))?;
        let stdout = child
            .take_stdout()
            .ok_or_else(|| anyhow!("MCP server `{}`: no stdout pipe", entry.name))?;
        Ok(Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            sandbox_kind,
        })
    }
}

#[cfg(not(unix))]
impl StdioTransport {
    /// Spawn a stdio MCP server (non-Unix): env-scrubbed but WITHOUT an OS
    /// sandbox — the confined `spawn_tokio` primitive is Unix-only. `caveats` is
    /// accepted for signature parity and to keep the boundary explicit; it does
    /// not yet kernel-confine here.
    pub fn spawn(
        admitted: &newt_core::mcp::AdmittedServer<'_>,
        _caveats: &Caveats,
    ) -> Result<Self> {
        // Admission is a compile-time precondition (see the Unix `spawn`).
        let entry = admitted.entry();
        let command = entry
            .command
            .as_deref()
            .ok_or_else(|| anyhow!("stdio MCP server `{}` has no command", entry.name))?;
        let grants = resolve_env_grants(entry)?;
        let mut child = Command::new(command)
            .args(&entry.args)
            .env_clear()
            .envs(grants)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning MCP server `{}` ({command})", entry.name))?;
        tracing::warn!(
            "MCP server `{}`: spawned ADVISORY-only — the OS-sandbox confined spawn is Unix-only",
            entry.name
        );
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
            sandbox_kind: SandboxKind::None,
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

/// Streamable-HTTP transport for the supported handshake-era MCP revisions.
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
    /// Server-selected revision, populated immediately after the initialize
    /// response and sent on `notifications/initialized` plus every later POST.
    protocol_version: Option<reqwest::header::HeaderValue>,
    /// JSON-RPC messages parsed from `POST` responses, awaiting `recv`.
    inbox: VecDeque<String>,
    /// #1243 Leg 4: the loopback egress proxy the `client` routes through, when
    /// the net grant warranted one. Held for the transport's lifetime (dropping
    /// it tears the proxy down); `Some` iff egress is per-host gated.
    _proxy: Option<agent_bridle::ProxyHandle>,
    /// An explicitly granted hostname (or the existing loopback development
    /// exception) resolved to at least one non-global address and was bound
    /// directly to that screened, immutable address set.
    /// This is the private-network analogue of the proxy gate: the request
    /// cannot change origin, redirect, or re-resolve after admission.
    private_origin_pinned: bool,
}

/// A reqwest client whose egress is constrained to one exact URL origin and a
/// screened, DNS-pinned address set. Redirects are disabled and the raw client
/// is never exposed, so callers cannot reuse it for another origin.
pub struct FencedHttpClient {
    client: reqwest::Client,
    scheme: String,
    host: String,
    port: u16,
}

impl FencedHttpClient {
    /// Build a no-redirect client pinned to the addresses resolved for exactly
    /// one validated URL origin. Every resolved address is screened before it
    /// reaches reqwest, and requests are rejected unless they retain that
    /// origin. A caller may explicitly allow this exact host to resolve to a
    /// private RFC1918/ULA or loopback address; link-local, CGNAT, unspecified,
    /// multicast, broadcast, transition, and reserved destinations remain
    /// forbidden.
    pub fn for_url(
        url: &reqwest::Url,
        timeout: std::time::Duration,
        allow_private_host: bool,
    ) -> Result<Self> {
        let origin = resolve_origin(url, &system_resolver)?;
        let client = build_pinned_client(&origin, timeout, allow_private_host)?;
        Ok(Self {
            client,
            scheme: url.scheme().to_string(),
            host: origin.host,
            port: origin.port,
        })
    }

    fn ensure_origin(&self, url: &reqwest::Url) -> Result<()> {
        let matches = url.scheme() == self.scheme
            && url
                .host_str()
                .is_some_and(|host| normalize_url_host(host) == self.host)
            && url.port_or_known_default() == Some(self.port);
        if matches {
            Ok(())
        } else {
            Err(anyhow!("fenced HTTP client refused a different URL origin"))
        }
    }

    pub fn get(&self, url: reqwest::Url) -> Result<reqwest::RequestBuilder> {
        self.ensure_origin(&url)?;
        Ok(self.client.get(url))
    }

    pub fn post(&self, url: reqwest::Url) -> Result<reqwest::RequestBuilder> {
        self.ensure_origin(&url)?;
        Ok(self.client.post(url))
    }
}

#[derive(Debug)]
struct ResolvedOrigin {
    host: String,
    port: u16,
    addresses: Vec<std::net::SocketAddr>,
}

const DNS_RESOLUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn system_resolver(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    resolve_with_timeout(host, port, DNS_RESOLUTION_TIMEOUT, move |host, port| {
        use std::net::ToSocketAddrs as _;
        (host.as_str(), port)
            .to_socket_addrs()
            .map(|iter| iter.collect())
    })
}

fn resolve_with_timeout(
    host: &str,
    port: u16,
    timeout: std::time::Duration,
    resolver: impl FnOnce(String, u16) -> std::io::Result<Vec<std::net::SocketAddr>> + Send + 'static,
) -> std::io::Result<Vec<std::net::SocketAddr>> {
    let host = host.to_string();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("newt-mcp-dns".to_string())
        .spawn(move || {
            let _ = sender.send(resolver(host, port));
        })
        .map_err(|error| std::io::Error::other(format!("starting DNS resolver: {error}")))?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "MCP DNS resolution timed out",
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other(
            "MCP DNS resolver stopped without a result",
        )),
    }
}

fn resolve_origin(
    url: &reqwest::Url,
    resolver: &impl Fn(&str, u16) -> std::io::Result<Vec<std::net::SocketAddr>>,
) -> Result<ResolvedOrigin> {
    let host = normalize_url_host(
        url.host_str()
            .ok_or_else(|| anyhow!("fenced HTTP URL has no host"))?,
    );
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("fenced HTTP URL has no known port"))?;
    let addresses =
        resolver(&host, port).with_context(|| format!("resolving fenced HTTP host `{host}`"))?;
    if addresses.is_empty() {
        return Err(anyhow!(
            "fenced HTTP host `{host}` resolved to no addresses"
        ));
    }
    if addresses.iter().any(|address| address.port() != port) {
        return Err(anyhow!(
            "fenced HTTP resolver returned an address with the wrong port for `{host}`"
        ));
    }
    Ok(ResolvedOrigin {
        host,
        port,
        addresses,
    })
}

fn build_pinned_client(
    origin: &ResolvedOrigin,
    timeout: std::time::Duration,
    allow_private_host: bool,
) -> Result<reqwest::Client> {
    for address in &origin.addresses {
        if !fenced_ip_is_allowed(address.ip(), allow_private_host) {
            return Err(anyhow!(
                "fenced HTTP host `{}` resolved to forbidden non-global address {}",
                origin.host,
                address.ip()
            ));
        }
    }
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        // Environment proxy configuration would bypass this client's
        // DNS-pinned destination set and let the proxy resolve the target.
        .no_proxy()
        .resolve_to_addrs(&origin.host, &origin.addresses)
        .build()
        .context("building DNS-pinned HTTP client")
}

fn exact_host_is_explicitly_granted(caveats: &Caveats, host: &str) -> bool {
    host_is_explicitly_granted(&caveats.net, host)
}

fn host_is_explicitly_granted(scope: &newt_core::caveats::Scope<String>, host: &str) -> bool {
    match scope {
        newt_core::caveats::Scope::Only(hosts) => hosts
            .iter()
            .filter_map(|granted| canonical_granted_host(granted))
            .any(|granted| http_host_grant_matches(&granted, host)),
        // Full network authority is deliberately not an SSRF-sensitive private
        // host approval. A private destination must still be named exactly.
        newt_core::caveats::Scope::All => false,
    }
}

/// Whether one configured net-grant host and one URL host identify the same
/// canonical authority. URL-shaped, wildcard, host:port, and path-shaped grants
/// never match.
#[must_use]
pub fn http_host_grant_matches(granted: &str, host: &str) -> bool {
    let Some(granted) = canonical_granted_host(granted) else {
        return false;
    };
    let Some(host) = canonical_granted_host(host) else {
        return false;
    };
    granted == host
}

/// Whether an MCP HTTP hostname is within the session net scope. Loopback is
/// the existing local-development exception; `Scope::All` admits public hosts,
/// while `Scope::Only` requires one exact canonical hostname grant.
#[must_use]
pub fn net_scope_permits_http_host(scope: &newt_core::caveats::Scope<String>, host: &str) -> bool {
    if host_is_loopback(host) {
        return true;
    }
    match scope {
        newt_core::caveats::Scope::All => true,
        newt_core::caveats::Scope::Only(_) => host_is_explicitly_granted(scope, host),
    }
}

fn canonical_granted_host(granted: &str) -> Option<String> {
    let trimmed = granted.trim();
    if trimmed.is_empty() || trimmed != granted {
        return None;
    }
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Ok(ip) = unbracketed.parse::<std::net::IpAddr>() {
        if trimmed.starts_with('[') != trimmed.ends_with(']') {
            return None;
        }
        return Some(ip.to_string().to_ascii_lowercase());
    }
    // A private-host approval is a hostname, never a URL, userinfo, path,
    // wildcard, or host:port tuple. Parsing through Url applies the same IDNA
    // canonicalization as the destination URL after those shapes are refused.
    if trimmed.contains([':', '/', '\\', '@', '?', '#', '[', ']', '*']) {
        return None;
    }
    let parsed = reqwest::Url::parse(&format!("http://{trimmed}/")).ok()?;
    parsed.host_str().map(normalize_url_host)
}

fn normalize_url_host(host: &str) -> String {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .to_ascii_lowercase()
}

fn ip_is_loopback_equivalent(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
    }
}

fn ip_is_non_global(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [first, second, ..] = ip.octets();
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || first == 0
                || first >= 224
                || (first == 100 && (64..=127).contains(&second))
                || (first == 192 && second == 0)
                || (first == 198 && (second == 18 || second == 19))
                || (first == 198 && second == 51)
                || (first == 203 && second == 0)
        }
        std::net::IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return ip_is_non_global(mapped.into());
            }
            let segments = ip.segments();
            // IPv4-compatible, IPv4-translated, NAT64 WKP, discard-only,
            // benchmarking, documentation, ORCHID, 6to4 relay anycast, and
            // IETF protocol-assignment ranges are not ordinary global targets.
            let embedded_v4 = (segments[..6] == [0, 0, 0, 0, 0, 0]
                || segments[..6] == [0, 0, 0, 0, 0xffff, 0]
                || segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0])
            .then(|| {
                std::net::Ipv4Addr::new(
                    (segments[6] >> 8) as u8,
                    segments[6] as u8,
                    (segments[7] >> 8) as u8,
                    segments[7] as u8,
                )
            });
            if embedded_v4.is_some_and(|v4| ip_is_non_global(v4.into())) {
                return true;
            }
            let first = segments[0];
            let ietf_assignment_is_global_exception = u128::from_be_bytes(ip.octets())
                == 0x2001_0001_0000_0000_0000_0000_0000_0001
                || u128::from_be_bytes(ip.octets()) == 0x2001_0001_0000_0000_0000_0000_0000_0002
                || segments[1] == 0x0003
                || (segments[1] == 0x0004 && segments[2] == 0x0112)
                || (0x0020..=0x002f).contains(&segments[1]);
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first & 0xffc0) == 0xfec0
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x2001
                    && segments[1] < 0x0200
                    && !ietf_assignment_is_global_exception)
                || (segments[0] == 0x0100 && segments[1] == 0)
                // 6to4 embeds an IPv4 destination in bits 16..48. Reject the
                // entire transition prefix: allowing only apparently-global
                // embedded values still delegates routing to relays and creates
                // an SSRF interpretation gap across resolvers/proxies.
                || segments[0] == 0x2002
                || segments[0] == 0x5f00
                || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001)
                || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        }
    }
}

fn fenced_ip_is_allowed(ip: std::net::IpAddr, allow_private_host: bool) -> bool {
    !ip_is_non_global(ip) || (allow_private_host && ip_is_approvable_private(ip))
}

fn ip_is_approvable_private(ip: std::net::IpAddr) -> bool {
    match ip {
        // The exact-host exception is deliberately narrow: RFC1918 and
        // loopback only. Link-local (including cloud metadata), CGNAT,
        // documentation, benchmark, multicast, and unspecified ranges remain
        // forbidden even when a hostname is granted.
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return ip_is_approvable_private(mapped.into());
            }
            ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

impl HttpTransport {
    /// Build a streamable-HTTP transport from a discovered entry. Configured
    /// `entry.headers` (e.g. `Authorization: Bearer …`) are sent on every
    /// request. Resolves and screens the endpoint once; the protocol handshake
    /// happens later in `initialize`.
    ///
    /// #1243 Leg 4: under a general remote-host `net` grant the client is bound
    /// to the loopback egress proxy ([`agent_bridle::start_egress_proxy`]) via
    /// `reqwest::Proxy::all`, so per-call traffic AND redirects are enforced
    /// against the allow-list — not only the connect-time host (#1156). An
    /// exact private-host grant instead uses the screened DNS-pinned client,
    /// because the general proxy intentionally rejects RFC1918 destinations.
    pub fn connect(
        admitted: &newt_core::mcp::AdmittedServer<'_>,
        caveats: &Caveats,
    ) -> Result<Self> {
        Self::connect_with_runtime_bearer(admitted, caveats, None, false)
    }

    fn connect_with_runtime_bearer(
        admitted: &newt_core::mcp::AdmittedServer<'_>,
        caveats: &Caveats,
        runtime_bearer: Option<&str>,
        allow_insecure_authorization: bool,
    ) -> Result<Self> {
        Self::connect_with_runtime_bearer_and_resolver(
            admitted,
            caveats,
            runtime_bearer,
            allow_insecure_authorization,
            &system_resolver,
        )
    }

    fn connect_with_runtime_bearer_and_resolver(
        admitted: &newt_core::mcp::AdmittedServer<'_>,
        caveats: &Caveats,
        runtime_bearer: Option<&str>,
        allow_insecure_authorization: bool,
        resolver: &impl Fn(&str, u16) -> std::io::Result<Vec<std::net::SocketAddr>>,
    ) -> Result<Self> {
        // Admission is a compile-time precondition of a dial (see stdio `spawn`).
        let entry = admitted.entry();
        let raw_url = entry
            .url
            .clone()
            .ok_or_else(|| anyhow!("http MCP server `{}` has no url", entry.name))?;
        let canonical = newt_core::mcp::canonical_mcp_http_url(&raw_url)
            .map_err(|issue| anyhow!("MCP server `{}` has {issue}", entry.name))?;
        let url = canonical.url;
        let parsed_url = reqwest::Url::parse(&url)
            .with_context(|| format!("MCP server `{}`: invalid HTTP URL", entry.name))?;
        let configured_authorization: Vec<_> = entry
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .collect();
        if configured_authorization.len() > 1 {
            return Err(anyhow!(
                "MCP server `{}`: multiple case-variant Authorization headers are ambiguous",
                entry.name
            ));
        }
        if !configured_authorization.is_empty() {
            if configured_authorization
                .iter()
                .any(|(_, value)| !authorization_value_is_reference(value))
            {
                return Err(anyhow!(
                    "MCP server `{}`: plaintext Authorization credential in MCP config; use an environment/file reference",
                    entry.name
                ));
            }
            if runtime_bearer.is_some() {
                return Err(anyhow!(
                    "MCP server `{}`: both configured Authorization and runtime OAuth Bearer were supplied",
                    entry.name
                ));
            }
        }
        // Resolve every header SecretValue host-side, before the value ever
        // touches reqwest — a literal is `${...}`-interpolated, a `{env|file|cmd}`
        // reference is resolved through `SecretRef`. Fails loud if a reference
        // cannot be satisfied.
        let resolved_headers = resolve_entry_secrets(&entry.headers, entry.trust, &entry.name)
            .with_context(|| format!("resolving header secrets for MCP server `{}`", entry.name))?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (key, value) in &resolved_headers {
            let name =
                reqwest::header::HeaderName::from_bytes(key.as_bytes()).with_context(|| {
                    format!("MCP server `{}`: invalid header name `{key}`", entry.name)
                })?;
            if newt_core::mcp::is_transport_owned_mcp_header(name.as_str()) {
                return Err(anyhow!(
                    "MCP server `{}`: `{key}` is transport-owned and cannot be configured",
                    entry.name
                ));
            }
            let val = reqwest::header::HeaderValue::from_str(value).with_context(|| {
                format!(
                    "MCP server `{}`: invalid value for header `{key}`",
                    entry.name
                )
            })?;
            headers.insert(name, val);
        }
        if let Some(token) = runtime_bearer {
            if token.trim().is_empty() {
                return Err(anyhow!(
                    "MCP server `{}`: runtime OAuth Bearer was empty",
                    entry.name
                ));
            }
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .with_context(|| format!("MCP server `{}`: invalid OAuth Bearer", entry.name))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let carries_authorization = headers.contains_key(reqwest::header::AUTHORIZATION);
        let secure_authorization_transport = parsed_url.scheme() == "https"
            || parsed_url.host_str().is_some_and(host_is_loopback)
            || allow_insecure_authorization;
        if carries_authorization && !secure_authorization_transport {
            return Err(anyhow!(
                "MCP server `{}`: refusing Authorization over unencrypted non-loopback HTTP",
                entry.name
            ));
        }
        // Resolve once before dialing. An unapproved private answer is refused;
        // an exact host grant may opt one private resource into a pinned direct
        // connection because agent-bridle's general egress proxy intentionally
        // rejects RFC1918 destinations. The pinned path is still origin-closed:
        // no redirects, no environment proxy, and no second DNS lookup.
        if !net_scope_permits_http_host(&caveats.net, &canonical.host) {
            return Err(anyhow!(
                "MCP server `{}`: host `{}` is outside the session net allow-list",
                entry.name,
                canonical.host
            ));
        }
        let origin = resolve_origin(&parsed_url, resolver)
            .with_context(|| format!("MCP server `{}`: resolving HTTP origin", entry.name))?;
        let exact_private_approval = exact_host_is_explicitly_granted(caveats, &canonical.host);
        let loopback_origin = host_is_loopback(&canonical.host);
        if loopback_origin
            && origin
                .addresses
                .iter()
                .any(|address| !ip_is_loopback_equivalent(address.ip()))
        {
            return Err(anyhow!(
                "MCP server `{}`: loopback development host `{}` resolved outside loopback",
                entry.name,
                canonical.host
            ));
        }
        let has_non_global = origin
            .addresses
            .iter()
            .any(|address| ip_is_non_global(address.ip()));
        if has_non_global && !exact_private_approval && !loopback_origin {
            return Err(anyhow!(
                "MCP server `{}`: host `{}` resolved to a private/non-global address without an exact net grant",
                entry.name,
                canonical.host
            ));
        }

        let (client, proxy, private_origin_pinned) = if has_non_global {
            (
                build_pinned_client(
                    &origin,
                    resolve_timeout(entry),
                    exact_private_approval || loopback_origin,
                )?,
                None,
                true,
            )
        } else {
            // Fail-closed: a grant that WARRANTS a proxy but whose loopback
            // listener cannot bind must refuse the connection, never dial
            // unmediated. When no proxy is warranted, pin the screened public
            // answer so a second DNS lookup cannot rebind it to a private IP.
            let proxy = agent_bridle::start_egress_proxy(caveats)
                .with_context(|| format!("MCP server `{}`: starting egress proxy", entry.name))?;
            if let Some(handle) = &proxy {
                let addr = format!("http://{}", handle.addr());
                let client = reqwest::Client::builder()
                    .timeout(resolve_timeout(entry))
                    .redirect(reqwest::redirect::Policy::none())
                    .proxy(reqwest::Proxy::all(&addr).with_context(|| {
                        format!("MCP server `{}`: routing through egress proxy", entry.name)
                    })?)
                    .build()
                    .with_context(|| {
                        format!("building HTTP client for MCP server `{}`", entry.name)
                    })?;
                (client, proxy, false)
            } else {
                (
                    build_pinned_client(&origin, resolve_timeout(entry), false)?,
                    None,
                    false,
                )
            }
        };
        Ok(Self {
            client,
            url,
            headers,
            session_id: None,
            protocol_version: None,
            inbox: VecDeque::new(),
            _proxy: proxy,
            private_origin_pinned,
        })
    }

    /// Whether this client's egress is fenced through the loopback proxy
    /// (#1243 Leg 4) — cross-platform (the client points itself at the proxy;
    /// no kernel fence needed).
    #[must_use]
    pub fn egress_proxied(&self) -> bool {
        self._proxy.is_some()
    }

    /// Whether this transport bypassed the general proxy after an exact
    /// private-host grant (or loopback development exception) and then pinned
    /// that resource's screened address set.
    #[must_use]
    pub fn private_origin_pinned(&self) -> bool {
        self.private_origin_pinned
    }
}

fn valid_mcp_session_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn authorization_value_is_reference(value: &newt_core::mcp::SecretValue) -> bool {
    match value {
        newt_core::mcp::SecretValue::Ref(_) => true,
        newt_core::mcp::SecretValue::Literal(value) => {
            let candidate = value
                .trim()
                .strip_prefix("Bearer ")
                .unwrap_or_else(|| value.trim());
            candidate.len() > 3
                && candidate.starts_with("${")
                && candidate.ends_with('}')
                && !candidate[2..candidate.len() - 1].contains(['{', '}'])
        }
    }
}

fn has_configured_authorization(entry: &McpServerEntry) -> bool {
    entry
        .headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
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
        if let Some(version) = &self.protocol_version {
            req = req.header("MCP-Protocol-Version", version.clone());
        }
        let initializing = self.protocol_version.is_none();
        let resp = req.send().await.context("MCP HTTP request failed")?;
        let status = resp.status();
        let is_sse = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("text/event-stream"))
            .unwrap_or(false);
        if !status.is_success() {
            // Never echo an untrusted server body into logs/UI. Drain it only
            // through the same cap used for successful protocol messages.
            let _ = bounded_http_response_body(resp).await;
            return Err(anyhow::Error::new(HttpStatusError::new(
                status.as_u16(),
                status.canonical_reason().unwrap_or(""),
                "",
            )));
        }
        // Only the successful initialize response establishes a session. The
        // grammar is visible ASCII (0x21..=0x7e); whitespace/control bytes and
        // duplicate headers are rejected before any value is retained.
        if initializing {
            let session_values: Vec<_> = resp.headers().get_all("Mcp-Session-Id").iter().collect();
            if session_values.len() > 1 {
                return Err(anyhow!(
                    "MCP initialize returned multiple session identifiers"
                ));
            }
            if let Some(value) = session_values.first() {
                let sid = value
                    .to_str()
                    .context("MCP initialize returned a non-ASCII session identifier")?;
                if !valid_mcp_session_id(sid) {
                    return Err(anyhow!(
                        "MCP initialize returned an invalid session identifier"
                    ));
                }
                self.session_id = Some((*sid).to_string());
            }
        }
        let body = bounded_http_response_body(resp).await?;
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

    fn set_protocol_version(&mut self, version: &str) -> Result<()> {
        if !HTTP_SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            return Err(anyhow!(
                "refusing unsupported MCP protocol version `{version}`"
            ));
        }
        self.protocol_version = Some(
            reqwest::header::HeaderValue::from_str(version)
                .context("invalid negotiated MCP protocol version header")?,
        );
        Ok(())
    }

    fn supports_protocol_version(&self, version: &str) -> bool {
        HTTP_SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
    }

    fn has_session(&self) -> bool {
        self.session_id.is_some()
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

/// In-memory transport: discards sends, returns canned lines in order.
/// `#[cfg(test)]`, crate-scoped so both `mod tests` and `mod toolset_tests`
/// (and `AnyTransport`'s test-only `Mock` variant below) can build a real
/// [`ConnectedServer`] without a subprocess or socket.
#[cfg(test)]
pub struct MockTransport {
    responses: std::collections::VecDeque<String>,
    protocol_version: Option<String>,
}

#[cfg(test)]
impl MockTransport {
    pub fn new(lines: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            responses: lines.into_iter().map(str::to_string).collect(),
            protocol_version: None,
        }
    }
}

#[cfg(test)]
impl Transport for MockTransport {
    async fn send(&mut self, _line: String) -> Result<()> {
        Ok(())
    }
    async fn recv(&mut self) -> Result<Option<String>> {
        Ok(self.responses.pop_front())
    }
    fn set_protocol_version(&mut self, version: &str) -> Result<()> {
        self.protocol_version = Some(version.to_string());
        Ok(())
    }
}

/// A [`Transport`] that is either stdio or streamable-HTTP, chosen per server.
///
/// An enum (rather than `Box<dyn Transport>`) keeps static dispatch — the
/// trait's `async fn`s are not object-safe — and lets one `Vec<ConnectedServer>`
/// hold a mix of transports. The `#[cfg(test)] Mock` arm exists so a test can
/// build a real [`ConnectedServer`] (whose `conn` field is the concrete
/// `McpConnection<AnyTransport>`, not generic) against [`MockTransport`]
/// instead of spawning a subprocess or dialing a socket.
pub enum AnyTransport {
    Stdio(Box<StdioTransport>),
    Http(Box<HttpTransport>),
    #[cfg(test)]
    Mock(MockTransport),
}

impl Transport for AnyTransport {
    async fn send(&mut self, line: String) -> Result<()> {
        match self {
            Self::Stdio(t) => t.send(line).await,
            Self::Http(t) => t.send(line).await,
            #[cfg(test)]
            Self::Mock(t) => t.send(line).await,
        }
    }
    async fn recv(&mut self) -> Result<Option<String>> {
        match self {
            Self::Stdio(t) => t.recv().await,
            Self::Http(t) => t.recv().await,
            #[cfg(test)]
            Self::Mock(t) => t.recv().await,
        }
    }

    fn set_protocol_version(&mut self, version: &str) -> Result<()> {
        match self {
            Self::Stdio(transport) => transport.set_protocol_version(version),
            Self::Http(transport) => transport.set_protocol_version(version),
            #[cfg(test)]
            Self::Mock(transport) => transport.set_protocol_version(version),
        }
    }

    fn supports_protocol_version(&self, version: &str) -> bool {
        match self {
            Self::Stdio(transport) => transport.supports_protocol_version(version),
            Self::Http(transport) => transport.supports_protocol_version(version),
            #[cfg(test)]
            Self::Mock(transport) => transport.supports_protocol_version(version),
        }
    }

    fn has_session(&self) -> bool {
        match self {
            Self::Stdio(transport) => transport.has_session(),
            Self::Http(transport) => transport.has_session(),
            #[cfg(test)]
            Self::Mock(transport) => transport.has_session(),
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
    /// The OS-sandbox posture of the connection (#1243 Leg 3). `Some(kind)` for a
    /// spawned **stdio** server — the confinement its process actually achieved
    /// ([`SandboxKind::None`] = advisory); `None` for a remote **HTTP** server
    /// (no local process to confine).
    pub sandbox_kind: Option<SandboxKind>,
    /// The network-egress posture of the connection (#1243 Leg 4): `Gated(n)`
    /// when outbound traffic is routed through the loopback egress proxy
    /// enforcing an `n`-host allow-list, else `Advisory`.
    pub net_posture: NetPosture,
    /// The server's self-reported identity (`serverInfo`), when it sent one.
    pub server_info: Option<ServerInfo>,
    /// Server-authored usage `instructions` from the handshake, when present.
    pub instructions: Option<String>,
}

/// Initialize a transport and list its tools into a [`ConnectedServer`].
async fn finish_connect(
    entry: &McpServerEntry,
    transport: AnyTransport,
    sandbox_kind: Option<SandboxKind>,
    net_posture: NetPosture,
) -> Result<ConnectedServer> {
    let timeout = resolve_timeout(entry);
    let mut conn = McpConnection::new_with_timeout(transport, timeout);
    let init = tokio::time::timeout(timeout, conn.initialize())
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
        sandbox_kind,
        net_posture,
        server_info: init.server_info,
        instructions: init.instructions,
    })
}

/// Connect to one discovered **stdio** server: spawn (confined by `caveats`),
/// initialize, list tools. The child runs inside the session's OCAP boundary —
/// see [`StdioTransport::spawn`].
pub async fn connect_stdio(
    admitted: &newt_core::mcp::AdmittedServer<'_>,
    caveats: &Caveats,
) -> Result<ConnectedServer> {
    // step-1.1: the caller proved admission at the `admit()` gate — an
    // un-admitted server cannot be spawned because there is no other way to
    // obtain an `AdmittedServer`.
    let entry = admitted.entry();
    if entry.transport != TransportKind::Stdio {
        return Err(anyhow!(
            "server `{}`: connect_stdio called for a non-stdio transport",
            entry.name
        ));
    }
    let transport = StdioTransport::spawn(admitted, caveats)?;
    let sandbox_kind = Some(transport.sandbox_kind());
    // #1243 Leg 4: spawn_tokio engaged the egress proxy iff the child's egress
    // is fenced (a remote-host grant on a fence-capable host); its posture is
    // gated with the granted host count, else advisory.
    let net = net_posture(caveats, transport.egress_proxied(), false);
    finish_connect(
        entry,
        AnyTransport::Stdio(Box::new(transport)),
        sandbox_kind,
        net,
    )
    .await
}

/// Connect to one discovered **streamable-HTTP** server: dial, initialize, list
/// tools. Use this for `TransportKind::Http` entries (the legacy SSE-only
/// transport is not supported).
///
/// #1243 Leg 4: under a general remote-host `net` grant the client is routed
/// through the loopback egress proxy, so EVERY request and redirect is subject
/// to the per-host allow-list — not just the one connect-time host check
/// (#1156). A non-granted host is refused per-call.
pub async fn connect_http(
    admitted: &newt_core::mcp::AdmittedServer<'_>,
    caveats: &Caveats,
) -> Result<ConnectedServer> {
    connect_http_with_runtime_bearer(admitted, caveats, None, false).await
}

/// Connect an HTTP MCP server with an OAuth Bearer supplied at runtime rather
/// than persisted in the discovered configuration. The final boolean is the
/// caller's explicit policy decision for credential-bearing non-loopback HTTP.
pub async fn connect_http_with_runtime_bearer(
    admitted: &newt_core::mcp::AdmittedServer<'_>,
    caveats: &Caveats,
    bearer: Option<&str>,
    allow_insecure_authorization: bool,
) -> Result<ConnectedServer> {
    // step-1.1: admission proven at the gate (see `connect_stdio`).
    let entry = admitted.entry();
    if entry.transport != TransportKind::Http {
        return Err(anyhow!(
            "server `{}`: connect_http called for a non-http transport",
            entry.name
        ));
    }
    let transport = HttpTransport::connect_with_runtime_bearer(
        admitted,
        caveats,
        bearer,
        allow_insecure_authorization,
    )?;
    let net = net_posture(
        caveats,
        transport.egress_proxied(),
        transport.private_origin_pinned(),
    );
    // No local process → no local OS-sandbox posture; net posture is real.
    finish_connect(entry, AnyTransport::Http(Box::new(transport)), None, net).await
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

// ---------------------------------------------------------------------------
// McpToolset (#1021 PR 5.1): a session's connected MCP servers, shared
// ---------------------------------------------------------------------------
//
// Promoted out of `newt-tui/src/mcp.rs`'s TUI-only `Mcp` struct so a headless
// entry point (`newt-mcp-server`, `newt-acp-worker`) can connect to the same
// servers — `modulex` and friends — without depending on `newt-tui`. The TUI
// keeps its own `Mcp` type unchanged (a follow-up may migrate it onto this
// one; not required for headless support to work). Connects **stdio** and
// **streamable-HTTP** servers, and carries **no Caveats leash** on the remote
// tools — they run with whatever authority their own server has, same as the
// TUI's version.
//
// Deliberately narrower than the TUI's `Mcp::connect`: it does not perform
// the TUI's interactive OAuth-bearer-token lookup (`mcp_token::load_bearer_token`,
// a persisted-login convenience with no headless-server analogue). A headless
// caller that needs auth sets an explicit `Authorization` header on the
// server's config entry (`McpServerEntry::headers`, already resolved by
// `newt_core::mcp::discover`); the insecure-transport WARNING behavior below
// is preserved regardless, since that's a real security signal, not a UX nicety.

/// Apply or skip the hyphen→underscore normalisation for a server name.
fn server_prefix(name: &str, sanitize: bool) -> String {
    newt_core::mcp::runtime_server_prefix(name, sanitize)
}

/// Best-effort `(scheme, host)` from a URL — lowercased, port/userinfo/path
/// stripped, IPv6 brackets removed. Empty strings when absent/unparseable (which
/// the policy treats as insecure → no token). Manual parse to avoid a url dep;
/// good enough for the scheme+host decision below.
///
/// The **canonical** implementation for MCP transport-policy decisions —
/// `newt mcp probe` and the TUI's Bearer/egress gates delegate here so the
/// split rules cannot diverge. The authority ends at the first of `/ ? #`,
/// and userinfo is stripped from the *authority only* — an `@` inside a query
/// must never smuggle a fake host past a gate.
#[must_use]
pub fn parse_scheme_host(url: Option<&str>) -> (String, String) {
    let Some(url) = url else {
        return (String::new(), String::new());
    };
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h); // drop userinfo
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or(v6) // [::1]:port → ::1
    } else {
        authority.split(':').next().unwrap_or(authority) // host:port → host
    };
    (scheme.to_ascii_lowercase(), host.to_ascii_lowercase())
}

/// A loopback host — the dev exception that needs no https and emits no
/// warning. Loopback is an **IP property**, never a string prefix: a
/// `starts_with("127.")` check certified `127.0.0.1.evil.com` (a perfectly
/// valid public DNS name) as loopback and let cleartext through the gate.
/// A non-IP host other than `localhost` is NOT loopback.
#[must_use]
pub fn host_is_loopback(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Warn on every non-loopback unencrypted (non-`https`) connection — the same
/// secure-by-default transport policy the TUI enforces
/// (`docs/decisions/mcp_transport_security.md`), minus the OAuth-token
/// injection half (headless callers pass any auth via explicit config headers,
/// so there is no token to conditionally withhold here).
fn warn_on_insecure_transport(entry: &McpServerEntry) {
    let (scheme, host) = parse_scheme_host(entry.url.as_deref());
    if scheme != "https" && !host_is_loopback(&host) {
        tracing::warn!(
            "MCP server `{}`: UNENCRYPTED connection to `{}` (no TLS).",
            entry.name,
            host
        );
    }
}

#[derive(Clone)]
struct ToolsetHttpReconnectState {
    entry: McpServerEntry,
    caveats: Caveats,
}

struct ToolsetServer {
    live: ConnectedServer,
    http: Option<ToolsetHttpReconnectState>,
}

async fn reconnect_toolset_http(state: &ToolsetHttpReconnectState) -> Result<ConnectedServer> {
    let admitted = newt_core::mcp::admit(&state.entry)
        .map_err(|denied| anyhow!("MCP reconnect was no longer admitted: {denied}"))?;
    connect_http(&admitted, &state.caveats).await
}

/// The session's connected MCP servers — the headless-crate-independent
/// counterpart of `newt-tui/src/mcp.rs`'s `Mcp`. HTTP entries retain only the
/// already-admitted configuration and attenuated caveats needed to reconstruct
/// a transport after session expiry or configured-credential rotation.
pub struct McpToolset {
    servers: Vec<ToolsetServer>,
    /// When `true`, hyphens in server names are replaced with underscores in
    /// advertised tool names and routing lookups, matching the TUI's
    /// `[tui].sanitize_mcp_server_names` behavior (default: `true`).
    sanitize_server_names: bool,
}

impl McpToolset {
    /// An empty toolset — connects to nothing. Used by tests and by any
    /// no-persona / no-configured-servers session.
    pub fn empty() -> Self {
        Self {
            servers: Vec::new(),
            sanitize_server_names: true,
        }
    }

    /// Discover (newt config + Claude Code config) and connect to every
    /// configured MCP server. A server that fails to spawn/initialize is
    /// logged and skipped — one bad server never blocks the caller or the
    /// others.
    pub async fn connect(
        workspace: &str,
        cfg_servers: &[McpServerEntry],
        sanitize_server_names: bool,
        // #1243 Leg 3: a spawned stdio MCP server runs *inside* this session
        // leash — the SAME `Caveats` a `run_command` dispatches under — instead
        // of as an ambient child with the host's full authority.
        caveats: &Caveats,
    ) -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let mcp_toml = newt_core::Config::user_config_dir().map(|d| d.join("mcp.toml"));
        let entries = newt_core::mcp::discover_with_namespace_mode(
            cfg_servers,
            mcp_toml.as_deref(),
            home.as_deref(),
            std::path::Path::new(workspace),
            sanitize_server_names,
        );
        let mut servers = Vec::new();
        let mut connected_prefixes = std::collections::BTreeSet::new();
        for entry in &entries {
            // step-1.1: admission gate FIRST. Headless has no interactive
            // approval path, so an untrusted (repo-shipped `.mcp.json` /
            // `~/.claude.json` / project overlay) or disabled server is refused
            // here — before any spawn or dial — closing the previous gap where
            // this planner (unlike the TUI) connected every discovered entry.
            let admitted = match newt_core::mcp::admit(entry) {
                Ok(a) => a,
                Err(denied) => {
                    tracing::warn!("MCP server `{}` not admitted: {denied}", entry.name);
                    continue;
                }
            };
            let result = match entry.transport {
                TransportKind::Stdio => connect_stdio(&admitted, caveats).await,
                TransportKind::Http => {
                    warn_on_insecure_transport(entry);
                    connect_http(&admitted, caveats).await
                }
                TransportKind::Sse => {
                    tracing::warn!(
                        "MCP server `{}`: legacy SSE transport is not supported \
                         (use streamable-HTTP, `type = \"http\"`) — skipped",
                        entry.name
                    );
                    continue;
                }
            };
            match result {
                Ok(connected) => {
                    let prefix = server_prefix(&connected.name, sanitize_server_names);
                    if !connected_prefixes.insert(prefix.clone()) {
                        tracing::warn!(
                            "MCP server `{}` skipped: emitted namespace `{prefix}` is already connected",
                            connected.name
                        );
                        continue;
                    }
                    servers.push(ToolsetServer {
                        live: connected,
                        http: (entry.transport == TransportKind::Http).then(|| {
                            ToolsetHttpReconnectState {
                                entry: entry.clone(),
                                caveats: caveats.clone(),
                            }
                        }),
                    });
                }
                Err(e) => tracing::warn!("MCP server `{}` skipped: {e:#}", entry.name),
            }
        }
        Self {
            servers,
            sanitize_server_names,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// `(server_name, tool_count)` for each connected server.
    pub fn summary(&self) -> Vec<(String, usize)> {
        self.servers
            .iter()
            .map(|s| (s.live.name.clone(), s.live.tools.len()))
            .collect()
    }

    /// OpenAI-style function tool definitions for every remote tool, with names
    /// namespaced `server__tool` so two servers cannot collide.
    pub fn tool_defs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for server in &self.servers {
            for tool in &server.live.tools {
                out.push(openai_tool_definition(
                    &server.live.name,
                    self.sanitize_server_names,
                    tool,
                ));
            }
        }
        out
    }

    /// MCP-protocol-native tool definitions (`{name, description,
    /// inputSchema}` per tool, namespaced `server__tool`) — the shape a
    /// `tools/list` JSON-RPC response needs (#1021 PR 5.3, `newt-mcp-server`).
    /// Distinct from [`Self::tool_defs`]'s OpenAI function-calling shape,
    /// which is what a chat-completions request needs instead; same
    /// underlying data and namespacing, different wire format.
    pub fn mcp_tool_list(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for server in &self.servers {
            for tool in &server.live.tools {
                let mut definition = json!({
                    "name": namespaced(&server_prefix(&server.live.name, self.sanitize_server_names), &tool.name),
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                });
                // This surface is an MCP forwarding boundary, so preserve the
                // same validated connector declaration for a nested client.
                newt_core::preserve_mcp_resource_url_affinity(&mut definition, tool.meta.as_ref());
                out.push(definition);
            }
        }
        out
    }

    /// Whether `name` is a namespaced tool belonging to a connected server.
    pub fn handles(&self, name: &str) -> bool {
        match split_namespaced(name) {
            Some((server, _)) => self
                .servers
                .iter()
                .any(|s| server_prefix(&s.live.name, self.sanitize_server_names) == server),
            None => false,
        }
    }

    /// Route a `server__tool` call to its server and render the result as the
    /// string a tool-calling loop feeds back as the tool message — wrapped as
    /// untrusted data ([`newt_core::wrap_untrusted`]) since it originates from
    /// an external server, not from newt itself.
    pub async fn call(&mut self, name: &str, args: &Value) -> String {
        let Some((server_name, tool)) = split_namespaced(name) else {
            return format!("error: `{name}` is not a namespaced MCP tool");
        };
        let Some(server) = self
            .servers
            .iter_mut()
            .find(|s| server_prefix(&s.live.name, self.sanitize_server_names) == server_name)
        else {
            return format!("error: no connected MCP server `{server_name}`");
        };
        let had_session = server.live.conn.has_session();
        match server.live.conn.call_tool(tool, args.clone()).await {
            // The result is external data, not a newt-generated message — wrap
            // it. `e` below is OUR OWN connection-error text, not external
            // content, so it is NOT wrapped.
            Ok(result) => newt_core::wrap_untrusted(name, &format_toolset_result(&result)),
            Err(initial_error) => {
                let Some(state) = server.http.clone() else {
                    return format!("error: {initial_error}");
                };
                let original_error = initial_error.to_string();
                let reconnect_state = state.clone();
                let replay_tool = tool.to_string();
                let replay_args = args.clone();
                let recovered = recover_http_call_after_error(
                    initial_error,
                    had_session,
                    None,
                    has_configured_authorization(&state.entry),
                    |_rejected| async { None },
                    move |_bearer| {
                        let reconnect_state = reconnect_state.clone();
                        async move { reconnect_toolset_http(&reconnect_state).await }
                    },
                    move |mut live| {
                        let replay_tool = replay_tool.clone();
                        let replay_args = replay_args.clone();
                        async move {
                            let result = live.conn.call_tool(&replay_tool, replay_args).await;
                            (live, result)
                        }
                    },
                )
                .await;
                match recovered {
                    Ok(Some(outcome)) => {
                        server.live = outcome.connection;
                        match outcome.result {
                            Ok(result) => {
                                newt_core::wrap_untrusted(name, &format_toolset_result(&result))
                            }
                            Err(error) => {
                                format!("error: {original_error}; MCP recovery failed: {error}")
                            }
                        }
                    }
                    Ok(None) => format!("error: {original_error}"),
                    Err(error) => {
                        format!("error: {original_error}; MCP recovery failed: {error}")
                    }
                }
            }
        }
    }
}

/// Flatten an MCP `tools/call` result (`{ content: [{type,text}], isError? }`)
/// into agent-facing text. Falls back to raw JSON if there is no text content.
/// Same shape as `newt-tui/src/mcp.rs`'s private `format_result` — kept as a
/// separate copy rather than shared, since the TUI's version stays untouched.
fn format_toolset_result(result: &Value) -> String {
    let mut text = String::new();
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for item in items {
            if let Some(t) = item.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
    }
    if text.is_empty() {
        text = result.to_string();
    }
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        format!("tool error: {text}")
    } else {
        text
    }
}

#[cfg(test)]
mod toolset_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn http_test_entry(url: String) -> McpServerEntry {
        McpServerEntry {
            enabled: true,
            name: "headless-http".into(),
            transport: TransportKind::Http,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some(url),
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: newt_core::mcp::McpTrust::Trusted,
        }
    }

    async fn mount_toolset_lifecycle(server: &MockServer, connections: u64) {
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"initialize\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("Mcp-Session-Id", "headless-session")
                    .set_body_string(format!(
                        r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{PROTOCOL_VERSION}","capabilities":{{}},"serverInfo":{{"name":"headless-http","version":"1"}}}}}}"#
                    )),
            )
            .expect(connections)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                "\"method\":\"notifications/initialized\"",
            ))
            .respond_with(ResponseTemplate::new(202))
            .expect(connections)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/list\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"review","description":"","inputSchema":{"type":"object"}}]}}"#,
                    ),
            )
            .expect(connections)
            .mount(server)
            .await;
    }

    #[derive(Clone)]
    struct ExpireFirstSessionCall {
        calls: Arc<AtomicUsize>,
    }

    impl Respond for ExpireFirstSessionCall {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(404)
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"recovered"}]}}"#,
                    )
            }
        }
    }

    #[derive(Clone)]
    struct ExpireThenRejectReplay {
        calls: Arc<AtomicUsize>,
    }

    impl Respond for ExpireThenRejectReplay {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(404),
                1 => ResponseTemplate::new(401),
                _ => ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"recovered connection retained"}]}}"#,
                    ),
            }
        }
    }

    #[test]
    fn empty_toolset_has_no_tools_and_handles_nothing() {
        let toolset = McpToolset::empty();
        assert!(toolset.is_empty());
        assert!(toolset.tool_defs().is_empty());
        assert!(!toolset.handles("modulex__routine_run"));
        assert!(toolset.summary().is_empty());
    }

    #[test]
    fn server_prefix_sanitizes_hyphens_when_enabled() {
        assert_eq!(server_prefix("my-server", true), "my_server");
        assert_eq!(server_prefix("my-server", false), "my-server");
    }

    #[test]
    fn handles_matches_sanitized_prefix_only() {
        let toolset = McpToolset {
            servers: vec![ToolsetServer {
                live: ConnectedServer {
                    name: "modulex".to_string(),
                    conn: McpConnection::new(AnyTransport::Mock(MockTransport::new([]))),
                    tools: vec![RemoteTool {
                        name: "routine_run".to_string(),
                        description: String::new(),
                        input_schema: json!({}),
                        meta: Some(json!({
                            "newt/resourceUrlPrefixes": ["https://review.example/resources/"]
                        })),
                    }],
                    sandbox_kind: None,
                    net_posture: crate::NetPosture::Advisory,
                    server_info: None,
                    instructions: None,
                },
                http: None,
            }],
            sanitize_server_names: true,
        };
        assert!(toolset.handles("modulex__routine_run"));
        // `handles` matches the SERVER prefix only, not the specific tool
        // name — same as the TUI's `Mcp::handles` it's ported from. A
        // namespaced call for an unlisted tool on a connected server still
        // routes there; the server itself rejects an unknown tool name.
        assert!(toolset.handles("modulex__some_other_tool_on_the_same_server"));
        assert!(!toolset.handles("no_separator_here"));
        assert!(!toolset.handles("other_server__routine_run"));

        let defs = toolset.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(
            defs[0]["function"]["name"],
            Value::String("modulex__routine_run".to_string())
        );
        assert_eq!(
            defs[0]["_meta"][newt_core::MCP_RESOURCE_URL_PREFIXES_META_KEY],
            json!(["https://review.example/resources/"])
        );
        assert_eq!(
            toolset.mcp_tool_list()[0]["_meta"][newt_core::MCP_RESOURCE_URL_PREFIXES_META_KEY],
            json!(["https://review.example/resources/"])
        );
    }

    #[test]
    fn format_toolset_result_joins_text_and_flags_errors() {
        let r = json!({"content": [{"type": "text", "text": "hello"}, {"type": "text", "text": "world"}]});
        assert_eq!(format_toolset_result(&r), "hello\nworld");
        let err = json!({"content": [{"type":"text","text":"boom"}], "isError": true});
        assert_eq!(format_toolset_result(&err), "tool error: boom");
    }

    #[tokio::test]
    async fn call_wraps_a_successful_result_as_untrusted_data() {
        let mut toolset = McpToolset {
            servers: vec![ToolsetServer {
                live: ConnectedServer {
                    name: "modulex".to_string(),
                    conn: McpConnection::new(AnyTransport::Mock(MockTransport::new([
                        r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"3 dirty trees"}]}}"#,
                    ]))),
                    tools: vec![],
                    sandbox_kind: None,
                    net_posture: crate::NetPosture::Advisory,
                    server_info: None,
                    instructions: None,
                },
                http: None,
            }],
            sanitize_server_names: true,
        };
        let out = toolset
            .call("modulex__routine_run", &json!({"routine": "morning"}))
            .await;
        assert!(out.starts_with("<untrusted-data source=\"modulex__routine_run\">"));
        assert!(out.contains("3 dirty trees"));
    }

    #[tokio::test]
    async fn headless_call_does_not_reflect_remote_json_rpc_error_content() {
        let mut toolset = McpToolset {
            servers: vec![ToolsetServer {
                live: ConnectedServer {
                    name: "modulex".to_string(),
                    conn: McpConnection::new(AnyTransport::Mock(MockTransport::new([
                        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"TOP-SECRET\u001b[31m\nforged log","data":{"token":"TOP-SECRET"}}}"#,
                    ]))),
                    tools: vec![],
                    sandbox_kind: None,
                    net_posture: crate::NetPosture::Advisory,
                    server_info: None,
                    instructions: None,
                },
                http: None,
            }],
            sanitize_server_names: true,
        };

        let output = toolset.call("modulex__review", &json!({})).await;
        assert_eq!(
            output,
            "error: MCP server error on `tools/call` (JSON-RPC code -32000)"
        );
        for forbidden in ["TOP-SECRET", "forged log", "\u{1b}", "\n", "\r"] {
            assert!(
                !output.contains(forbidden),
                "reflected {forbidden:?}: {output:?}"
            );
        }
    }

    #[tokio::test]
    async fn headless_toolset_reconnects_and_replays_once_after_session_404() {
        let server = MockServer::start().await;
        mount_toolset_lifecycle(&server, 2).await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/call\""))
            .respond_with(ExpireFirstSessionCall {
                calls: Arc::clone(&calls),
            })
            .expect(2)
            .mount(&server)
            .await;

        let entry = http_test_entry(format!("{}/mcp", server.uri()));
        let caveats = Caveats::top();
        let admitted = newt_core::mcp::admit(&entry).unwrap();
        let connected = connect_http(&admitted, &caveats).await.unwrap();
        let mut toolset = McpToolset {
            servers: vec![ToolsetServer {
                live: connected,
                http: Some(ToolsetHttpReconnectState { entry, caveats }),
            }],
            sanitize_server_names: true,
        };

        let output = toolset.call("headless_http__review", &json!({})).await;
        assert!(output.contains("recovered"), "{output}");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.verify().await;
    }

    #[tokio::test]
    async fn headless_recovers_404_then_replay_401_with_configured_authorization() {
        let server = MockServer::start().await;
        mount_toolset_lifecycle(&server, 3).await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/call\""))
            .respond_with(ExpireThenRejectReplay {
                calls: Arc::clone(&calls),
            })
            .expect(3)
            .mount(&server)
            .await;

        let temp = tempfile::tempdir().unwrap();
        let secret_path = temp.path().join("mcp-token");
        std::fs::write(&secret_path, "configured-secret\n").unwrap();
        let mut entry = http_test_entry(format!("{}/mcp", server.uri()));
        entry.headers.insert(
            "Authorization".into(),
            newt_core::mcp::SecretValue::literal(format!(
                "Bearer ${{file:{}}}",
                secret_path.display()
            )),
        );
        let caveats = Caveats::top();
        let admitted = newt_core::mcp::admit(&entry).unwrap();
        let connected = connect_http(&admitted, &caveats).await.unwrap();
        let mut toolset = McpToolset {
            servers: vec![ToolsetServer {
                live: connected,
                http: Some(ToolsetHttpReconnectState { entry, caveats }),
            }],
            sanitize_server_names: true,
        };

        let recovered = toolset.call("headless_http__review", &json!({})).await;
        assert!(
            recovered.contains("recovered connection retained"),
            "{recovered}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.verify().await;
    }

    #[tokio::test]
    async fn headless_toolset_reresolves_file_authorization_after_401() {
        let server = MockServer::start().await;
        mount_toolset_lifecycle(&server, 2).await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/call\""))
            .and(header("authorization", "Bearer old-secret"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/call\""))
            .and(header("authorization", "Bearer new-secret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"rotated credential accepted"}]}}"#,
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let temp = tempfile::tempdir().unwrap();
        let secret_path = temp.path().join("mcp-token");
        std::fs::write(&secret_path, "old-secret\n").unwrap();
        let mut entry = http_test_entry(format!("{}/mcp", server.uri()));
        entry.headers.insert(
            "Authorization".into(),
            newt_core::mcp::SecretValue::literal(format!(
                "Bearer ${{file:{}}}",
                secret_path.display()
            )),
        );
        let caveats = Caveats::top();
        let admitted = newt_core::mcp::admit(&entry).unwrap();
        let connected = connect_http(&admitted, &caveats).await.unwrap();
        std::fs::write(&secret_path, "new-secret\n").unwrap();
        let mut toolset = McpToolset {
            servers: vec![ToolsetServer {
                live: connected,
                http: Some(ToolsetHttpReconnectState { entry, caveats }),
            }],
            sanitize_server_names: true,
        };

        let output = toolset.call("headless_http__review", &json!({})).await;
        assert!(output.contains("rotated credential accepted"), "{output}");
        server.verify().await;
    }

    #[tokio::test]
    async fn call_reports_unknown_server_without_wrapping() {
        let mut toolset = McpToolset::empty();
        let out = toolset.call("ghost__tool", &json!({})).await;
        assert_eq!(out, "error: no connected MCP server `ghost`");
    }

    #[test]
    fn call_reports_non_namespaced_name_without_wrapping() {
        // Sync check of the pre-dispatch branch via a blocking runtime, since
        // `call` is async but this path returns before touching a connection.
        let out = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let mut toolset = McpToolset::empty();
                toolset.call("not_namespaced", &json!({})).await
            });
        assert_eq!(out, "error: `not_namespaced` is not a namespaced MCP tool");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_fence_rejects_private_and_ipv4_mapped_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "::1",
            "fc00::1",
            "::ffff:10.0.0.1",
            "64:ff9b:1::a00:1",
            "2002:7f00:1::",
            "2002:a00:1::",
        ] {
            let address = address.parse().unwrap();
            assert!(ip_is_non_global(address), "{address} must be non-global");
        }
        assert!(!ip_is_non_global("8.8.8.8".parse().unwrap()));
        assert!(!ip_is_non_global("2606:4700:4700::1111".parse().unwrap()));
        assert!(ip_is_non_global("2001:30::1".parse().unwrap()));
        assert!(!ip_is_non_global("2001:2f::1".parse().unwrap()));
        assert!(ip_is_non_global("3fff:0::1".parse().unwrap()));
        assert!(!ip_is_non_global("3fff:1000::1".parse().unwrap()));
        assert!(!ip_is_non_global("3ffe::1".parse().unwrap()));
    }

    #[test]
    fn private_resolution_requires_an_exact_host_policy_decision() {
        let url = reqwest::Url::parse("http://127.0.0.1:9/mcp").unwrap();
        assert!(FencedHttpClient::for_url(&url, Duration::from_secs(1), false).is_err());
        assert!(FencedHttpClient::for_url(&url, Duration::from_secs(1), true).is_ok());
    }

    #[test]
    fn system_dns_resolution_has_an_explicit_deadline() {
        let error = resolve_with_timeout(
            "slow.example",
            443,
            Duration::from_millis(5),
            |_host, _port| {
                std::thread::sleep(Duration::from_millis(100));
                Ok(Vec::new())
            },
        )
        .expect_err("a stalled resolver must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn private_host_approval_never_overrides_special_address_fences() {
        let six_to_four: std::net::IpAddr = "2002:7f00:1::".parse().unwrap();
        assert!(ip_is_non_global(six_to_four));
        assert!(!ip_is_approvable_private(six_to_four));
        assert!(!fenced_ip_is_allowed(six_to_four, true));
        for forbidden in [
            "169.254.169.254",
            "100.64.0.1",
            "fe80::1",
            "2001:100::1",
            "5f00::1",
        ] {
            let address = forbidden.parse().unwrap();
            assert!(ip_is_non_global(address), "{address}");
            assert!(!ip_is_approvable_private(address), "{address}");
            assert!(!fenced_ip_is_allowed(address, true), "{address}");
        }
    }

    #[test]
    fn exact_private_grant_is_canonical_but_never_url_shaped() {
        use newt_core::caveats::Scope;

        let canonical = Caveats {
            net: Scope::only(["REVIEW.INTERNAL.EXAMPLE".to_string()]),
            ..Caveats::top()
        };
        assert!(exact_host_is_explicitly_granted(
            &canonical,
            "review.internal.example"
        ));
        for not_a_host in [
            "review.internal.example:8443",
            "https://review.internal.example",
            "review.internal.example/path",
            "*.internal.example",
            " review.internal.example",
            "[::1",
            "::1]",
        ] {
            let caveats = Caveats {
                net: Scope::only([not_a_host.to_string()]),
                ..Caveats::top()
            };
            assert!(
                !exact_host_is_explicitly_granted(&caveats, "review.internal.example"),
                "{not_a_host} must not become an exact hostname grant"
            );
        }
    }

    fn private_http_entry(url: String) -> McpServerEntry {
        McpServerEntry {
            enabled: true,
            name: "private-review".to_string(),
            transport: TransportKind::Http,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some(url),
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: newt_core::mcp::McpTrust::Trusted,
        }
    }

    #[test]
    fn ungranted_private_dns_answer_fails_before_dial() {
        let entry = private_http_entry("http://review.internal.example:8443/mcp".to_string());
        let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
        let resolver =
            |_host: &str, port: u16| Ok(vec![std::net::SocketAddr::from(([10, 0, 0, 42], port))]);
        let error = match HttpTransport::connect_with_runtime_bearer_and_resolver(
            &admitted,
            &Caveats::top(),
            None,
            false,
            &resolver,
        ) {
            Ok(_) => panic!("private DNS without an exact host grant must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("without an exact net grant"),
            "{error:#}"
        );
    }

    #[test]
    fn public_host_outside_scope_fails_before_dns() {
        use newt_core::caveats::Scope;

        let entry = private_http_entry("https://public.example/mcp".to_string());
        let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
        let resolver = |_host: &str, _port: u16| -> std::io::Result<Vec<std::net::SocketAddr>> {
            panic!("out-of-scope host must fail before DNS")
        };
        let deny = Caveats {
            net: Scope::only([] as [String; 0]),
            ..Caveats::top()
        };
        let error = match HttpTransport::connect_with_runtime_bearer_and_resolver(
            &admitted, &deny, None, false, &resolver,
        ) {
            Ok(_) => panic!("public DNS outside the net scope must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("outside the session net"),
            "{error:#}"
        );
    }

    #[test]
    fn localhost_must_resolve_only_to_loopback_even_when_explicitly_granted() {
        use newt_core::caveats::Scope;

        let entry = private_http_entry("http://localhost:8443/mcp".to_string());
        let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
        let explicitly_granted = Caveats {
            net: Scope::only(["localhost".to_string()]),
            ..Caveats::top()
        };
        for caveats in [Caveats::top(), explicitly_granted] {
            for address in [
                std::net::SocketAddr::from(([10, 0, 0, 42], 8443)),
                std::net::SocketAddr::from(([8, 8, 8, 8], 8443)),
            ] {
                let resolver = |_host: &str, _port: u16| Ok(vec![address]);
                let error = match HttpTransport::connect_with_runtime_bearer_and_resolver(
                    &admitted, &caveats, None, false, &resolver,
                ) {
                    Ok(_) => panic!("localhost mapped outside loopback must fail"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains("outside loopback"), "{error:#}");
            }
        }
    }

    #[test]
    fn resolver_cannot_pivot_the_pinned_origin_to_another_port() {
        use newt_core::caveats::Scope;

        let entry = private_http_entry("http://review.internal.example:8443/mcp".to_string());
        let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
        let caveats = Caveats {
            net: Scope::only(["review.internal.example".to_string()]),
            ..Caveats::top()
        };
        let resolver =
            |_host: &str, _port: u16| Ok(vec![std::net::SocketAddr::from(([10, 0, 0, 42], 9443))]);
        let error = match HttpTransport::connect_with_runtime_bearer_and_resolver(
            &admitted, &caveats, None, false, &resolver,
        ) {
            Ok(_) => panic!("a resolver-provided port pivot must fail"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("wrong port"), "{error:#}");
    }

    #[test]
    fn unsafe_http_url_shapes_fail_before_dns() {
        let resolver = |_host: &str, _port: u16| -> std::io::Result<Vec<std::net::SocketAddr>> {
            panic!("unsafe URL must be rejected before DNS")
        };
        for url in [
            "ftp://review.internal.example/mcp",
            "https://user@review.internal.example/mcp",
            "https://review.internal.example/mcp?token=x",
            "https://review.internal.example/mcp#fragment",
        ] {
            let entry = private_http_entry(url.to_string());
            let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
            assert!(
                HttpTransport::connect_with_runtime_bearer_and_resolver(
                    &admitted,
                    &Caveats::top(),
                    None,
                    false,
                    &resolver,
                )
                .is_err(),
                "{url} must fail"
            );
        }
    }

    #[tokio::test]
    #[ignore = "real loopback private-host MCP lifecycle"]
    async fn exact_private_hostname_grant_pins_dns_for_full_mcp_lifecycle() {
        use newt_core::caveats::Scope;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::matchers::{body_string_contains, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"initialize\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("Mcp-Session-Id", "private-session")
                    .set_body_string(format!(
                        r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{PROTOCOL_VERSION}","capabilities":{{}},"serverInfo":{{"name":"review","version":"1"}}}}}}"#,
                    )),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                "\"method\":\"notifications/initialized\"",
            ))
            .and(header("mcp-session-id", "private-session"))
            .and(header("mcp-protocol-version", PROTOCOL_VERSION))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/list\""))
            .and(header("mcp-session-id", "private-session"))
            .and(header("mcp-protocol-version", PROTOCOL_VERSION))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"review","description":"review a change","inputSchema":{"type":"object"}}]}}"#,
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/call\""))
            .and(header("mcp-session-id", "private-session"))
            .and(header("mcp-protocol-version", PROTOCOL_VERSION))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"review loaded"}]}}"#,
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let private_host = "review.internal.example";
        let server_url = reqwest::Url::parse(&server.uri()).expect("wiremock URL");
        let server_port = server_url.port().expect("wiremock has an explicit port");
        let entry = private_http_entry(format!("http://{private_host}:{server_port}/mcp"));
        let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
        let caveats = Caveats {
            net: Scope::only([private_host.to_string()]),
            ..Caveats::top()
        };
        let resolution_count = Arc::new(AtomicUsize::new(0));
        let resolver_count = Arc::clone(&resolution_count);
        let resolver = move |host: &str, port: u16| {
            assert_eq!(host, private_host);
            assert_eq!(port, server_port);
            resolver_count.fetch_add(1, Ordering::SeqCst);
            Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))])
        };
        let transport = HttpTransport::connect_with_runtime_bearer_and_resolver(
            &admitted, &caveats, None, false, &resolver,
        )
        .expect("an exact private-host grant builds a pinned transport");
        assert!(transport.private_origin_pinned());
        assert!(!transport.egress_proxied());
        let net = net_posture(
            &caveats,
            transport.egress_proxied(),
            transport.private_origin_pinned(),
        );
        let mut connected =
            finish_connect(&entry, AnyTransport::Http(Box::new(transport)), None, net)
                .await
                .expect("initialize and tools/list succeed through the pinned host");
        assert_eq!(connected.tools.len(), 1);
        assert_eq!(connected.tools[0].name, "review");
        let result = connected
            .conn
            .call_tool("review", json!({"review": 4242}))
            .await
            .expect("tool call succeeds through the same pinned host");
        assert_eq!(result["content"][0]["text"].as_str(), Some("review loaded"));
        assert_eq!(
            resolution_count.load(Ordering::SeqCst),
            1,
            "initialize, initialized, list, and call must reuse one DNS answer"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn list_tools_metadata_survives_to_catalog_and_is_absent_safe() {
        // id 1 = initialize, id 2 = tools/list (notify carries no id/response).
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"find","inputSchema":{"type":"object"},"_meta":{"newt/resourceUrlPrefixes":["https://search.example/resources/"]}},{"name":"status","inputSchema":{"type":"object"}},{"name":"mixed","_meta":{"newt/resourceUrlPrefixes":["https://search.example/resources/",7]}}]}}"#,
        ]));
        conn.initialize().await.unwrap();
        let tools = conn.list_tools().await.unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description, "find");
        assert_eq!(
            tools[0].meta.as_ref().unwrap()[newt_core::MCP_RESOURCE_URL_PREFIXES_META_KEY],
            json!(["https://search.example/resources/"])
        );
        assert!(tools[1].meta.is_none());
        assert!(
            tools[2].meta.is_some(),
            "deserialization retains server metadata"
        );

        let valid = openai_tool_definition("search-source", true, &tools[0]);
        assert_eq!(valid["function"]["name"], "search_source__search");
        assert_eq!(
            valid["_meta"][newt_core::MCP_RESOURCE_URL_PREFIXES_META_KEY],
            json!(["https://search.example/resources/"])
        );
        let absent = openai_tool_definition("search-source", true, &tools[1]);
        assert!(absent.get("_meta").is_none());
        let malformed = openai_tool_definition("search-source", true, &tools[2]);
        assert!(
            malformed.get("_meta").is_none(),
            "a mixed array must grant no routing affinity"
        );
    }

    #[tokio::test]
    async fn initialize_captures_server_identity_and_instructions() {
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"scrybe","title":"Scrybe","version":"1.2.3"},"instructions":"Edit Markdown documents."}}"#,
        ]));
        let info = conn.initialize().await.unwrap();
        let si = info.server_info.expect("serverInfo captured");
        assert_eq!(si.name, "scrybe");
        assert_eq!(si.title.as_deref(), Some("Scrybe"));
        assert_eq!(si.version, "1.2.3");
        assert_eq!(
            info.instructions.as_deref(),
            Some("Edit Markdown documents.")
        );
        assert_eq!(info.protocol_version.as_deref(), Some("2024-11-05"));
        assert_eq!(
            conn.transport.protocol_version.as_deref(),
            Some("2024-11-05")
        );
        assert!(info.capabilities.get("tools").is_some());
    }

    #[tokio::test]
    async fn initialize_rejects_unknown_protocol_revision() {
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2099-01-01","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#,
        ]));
        let error = conn.initialize().await.unwrap_err().to_string();
        assert!(error.contains("unsupported by this transport"), "{error}");
    }

    #[tokio::test]
    async fn malformed_initialize_diagnostics_do_not_reflect_remote_values() {
        let responses = [
            r#"{"jsonrpc":"2.0","id":1,"result":"TOP-SECRET\u001b[31m\nforged log"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"TOP-SECRET\u001b[31m\nforged log","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":{"value":"TOP-SECRET\u001b[31m\nforged log"},"version":"1"}}}"#,
        ];

        for response in responses {
            let error = McpConnection::new(MockTransport::new([response]))
                .initialize()
                .await
                .unwrap_err()
                .to_string();
            for forbidden in ["TOP-SECRET", "forged log", "\u{1b}", "\n", "\r"] {
                assert!(
                    !error.contains(forbidden),
                    "reflected {forbidden:?}: {error:?}"
                );
            }
        }
    }

    #[test]
    fn http_recovery_budget_allows_session_then_runtime_bearer_recovery_once() {
        let missing = anyhow::Error::new(HttpStatusError::new(404, "Not Found", ""));
        let unauthorized = anyhow::Error::new(HttpStatusError::new(401, "Unauthorized", ""));
        let mut budget = HttpRecoveryBudget::new(true, true, false);

        assert_eq!(
            budget.next(&missing),
            HttpRecoveryAction::ReconnectExpiredSession
        );
        assert_eq!(
            budget.next(&unauthorized),
            HttpRecoveryAction::RefreshRuntimeBearer
        );
        assert_eq!(budget.next(&unauthorized), HttpRecoveryAction::Stop);
        assert_eq!(budget.next(&missing), HttpRecoveryAction::Stop);
    }

    #[test]
    fn session_reconnect_preserves_one_configured_authorization_recovery() {
        let missing = anyhow::Error::new(HttpStatusError::new(404, "Not Found", ""));
        let unauthorized = anyhow::Error::new(HttpStatusError::new(401, "Unauthorized", ""));
        let mut budget = HttpRecoveryBudget::new(true, false, true);

        assert_eq!(
            budget.next(&missing),
            HttpRecoveryAction::ReconnectExpiredSession
        );
        assert_eq!(
            budget.next(&unauthorized),
            HttpRecoveryAction::ReconnectConfiguredAuthorization
        );
        assert_eq!(budget.next(&unauthorized), HttpRecoveryAction::Stop);

        let mut direct = HttpRecoveryBudget::new(false, false, true);
        assert_eq!(
            direct.next(&unauthorized),
            HttpRecoveryAction::ReconnectConfiguredAuthorization
        );
        assert_eq!(direct.next(&unauthorized), HttpRecoveryAction::Stop);
    }

    #[tokio::test]
    async fn recovery_machine_handles_404_then_replay_401_then_one_bearer_refresh() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let reconnects = Arc::new(AtomicUsize::new(0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let reconnect_counter = Arc::clone(&reconnects);
        let refresh_counter = Arc::clone(&refreshes);
        let call_counter = Arc::clone(&calls);
        let initial = anyhow::Error::new(HttpStatusError::new(404, "Not Found", ""));

        let outcome = recover_http_call_after_error(
            initial,
            true,
            Some("stale-bearer".to_string()),
            false,
            move |rejected| {
                assert_eq!(rejected, "stale-bearer");
                refresh_counter.fetch_add(1, Ordering::SeqCst);
                async { Some("fresh-bearer".to_string()) }
            },
            move |bearer| {
                let attempt = reconnect_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        assert_eq!(bearer.as_deref(), Some("stale-bearer"));
                        Ok("stale-connection")
                    } else {
                        assert_eq!(bearer.as_deref(), Some("fresh-bearer"));
                        Ok("fresh-connection")
                    }
                }
            },
            move |connection| {
                let attempt = call_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    let result = if attempt == 0 {
                        assert_eq!(connection, "stale-connection");
                        Err(anyhow::Error::new(HttpStatusError::new(
                            401,
                            "Unauthorized",
                            "",
                        )))
                    } else {
                        assert_eq!(connection, "fresh-connection");
                        Ok("final-result")
                    };
                    (connection, result)
                }
            },
        )
        .await
        .unwrap()
        .expect("bounded recovery succeeds");

        assert_eq!(outcome.connection, "fresh-connection");
        assert_eq!(outcome.bearer.as_deref(), Some("fresh-bearer"));
        assert_eq!(outcome.result.unwrap(), "final-result");
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn configured_auth_recovery_handles_404_then_replay_401_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let reconnects = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let reconnect_counter = Arc::clone(&reconnects);
        let call_counter = Arc::clone(&calls);
        let outcome = recover_http_call_after_error(
            anyhow::Error::new(HttpStatusError::new(404, "Not Found", "")),
            true,
            None,
            true,
            |_rejected| async { panic!("configured auth is re-resolved, not OAuth-refreshed") },
            move |bearer| {
                assert!(bearer.is_none());
                let attempt = reconnect_counter.fetch_add(1, Ordering::SeqCst);
                async move { Ok(attempt) }
            },
            move |connection| {
                let attempt = call_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    let result = if attempt == 0 {
                        Err(anyhow::Error::new(HttpStatusError::new(
                            401,
                            "Unauthorized",
                            "",
                        )))
                    } else {
                        Ok("configured credential accepted")
                    };
                    (connection, result)
                }
            },
        )
        .await
        .unwrap()
        .expect("session reset plus configured-credential recovery succeeds");

        assert_eq!(outcome.connection, 1);
        assert!(outcome.bearer.is_none());
        assert_eq!(outcome.result.unwrap(), "configured credential accepted");
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn configured_auth_recovery_stops_after_session_and_credential_budgets() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let reconnects = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let reconnect_counter = Arc::clone(&reconnects);
        let call_counter = Arc::clone(&calls);
        let outcome = recover_http_call_after_error(
            anyhow::Error::new(HttpStatusError::new(404, "Not Found", "")),
            true,
            None,
            true,
            |_rejected| async { panic!("configured auth is re-resolved, not OAuth-refreshed") },
            move |_bearer| {
                let connection = reconnect_counter.fetch_add(1, Ordering::SeqCst);
                async move { Ok(connection) }
            },
            move |connection| {
                call_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    (
                        connection,
                        Err::<(), _>(anyhow::Error::new(HttpStatusError::new(
                            401,
                            "Unauthorized",
                            "",
                        ))),
                    )
                }
            },
        )
        .await
        .unwrap()
        .expect("final replay failure retains the second connection");

        assert_eq!(outcome.connection, 1);
        assert!(outcome.result.is_err());
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn recovery_machine_stops_after_final_replay_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let reconnects = Arc::new(AtomicUsize::new(0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let reconnect_counter = Arc::clone(&reconnects);
        let refresh_counter = Arc::clone(&refreshes);
        let call_counter = Arc::clone(&calls);
        let result = recover_http_call_after_error(
            anyhow::Error::new(HttpStatusError::new(404, "Not Found", "")),
            true,
            Some("stale-bearer".to_string()),
            false,
            move |_rejected| {
                refresh_counter.fetch_add(1, Ordering::SeqCst);
                async { Some("fresh-bearer".to_string()) }
            },
            move |bearer| {
                reconnect_counter.fetch_add(1, Ordering::SeqCst);
                async move { Ok(bearer.expect("both reconnects carry a bearer")) }
            },
            move |connection| {
                call_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    (
                        connection,
                        Err::<(), _>(anyhow::Error::new(HttpStatusError::new(
                            401,
                            "Unauthorized",
                            "",
                        ))),
                    )
                }
            },
        )
        .await;

        let outcome = result
            .unwrap()
            .expect("the final failed replay still returns the recovered connection");
        assert!(outcome.result.is_err());
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn protocol_support_is_explicitly_limited_to_the_handshake_era() {
        assert_eq!(PROTOCOL_VERSION, "2025-11-25");
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&"2025-06-18"));
        assert!(!SUPPORTED_PROTOCOL_VERSIONS.contains(&"2026-07-28"));
        assert!(!HTTP_SUPPORTED_PROTOCOL_VERSIONS.contains(&"2024-11-05"));
    }

    #[tokio::test]
    async fn initialize_rejects_missing_required_server_info() {
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
        ]));
        let error = conn.initialize().await.unwrap_err().to_string();
        assert!(error.contains("serverInfo"), "{error}");
    }

    #[test]
    fn scheme_host_authority_ends_at_slash_query_or_fragment() {
        assert_eq!(
            parse_scheme_host(Some("https://mcp.example?key=v")),
            ("https".into(), "mcp.example".into())
        );
        assert_eq!(
            parse_scheme_host(Some("http://evil.example?@127.0.0.1/")),
            ("http".into(), "evil.example".into()),
            "an @ inside the query must not smuggle a fake host"
        );
        assert_eq!(
            parse_scheme_host(Some("http://user@[::1]:8080/x#f")),
            ("http".into(), "::1".into())
        );
    }

    #[test]
    fn http_status_error_does_not_echo_untrusted_body_and_downcasts() {
        let err = HttpStatusError::new(401, "Unauthorized", "token missing");
        assert_eq!(err.to_string(), "MCP server returned HTTP 401 Unauthorized");
        assert!(!err.to_string().contains("token missing"));
        let chained = anyhow::Error::new(err).context("initializing MCP server `x`");
        let found = chained
            .chain()
            .find_map(|c| c.downcast_ref::<HttpStatusError>())
            .expect("typed error survives an anyhow context chain");
        assert_eq!(found.status, 401);
    }

    #[test]
    fn loopback_is_an_ip_property() {
        for yes in ["localhost", "127.0.0.1", "127.9.8.7", "::1"] {
            assert!(host_is_loopback(yes), "{yes}");
        }
        for no in ["127.0.0.1.evil.com", "127.evil.example", "mcp.example", ""] {
            assert!(!host_is_loopback(no), "{no}");
        }
    }

    #[tokio::test]
    async fn initialize_rejects_an_echoed_request_as_not_an_mcp_server() {
        // `/bin/cat` echoes our own initialize REQUEST back: id matches, no
        // `error`, no `result`. request() then yields Null — which must NOT
        // count as a handshake, or the probe would certify any stdin-echoing
        // process as an MCP server (and save it).
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        ]));
        let err = conn.initialize().await.unwrap_err();
        assert!(err.to_string().contains("not an MCP server"), "{err}");
    }

    #[tokio::test]
    async fn initialize_rejects_non_handshake_results() {
        // A result that is not an InitializeResult object (array / scalar /
        // object missing protocolVersion or capabilities) is not a handshake.
        for result in [
            r#"{"jsonrpc":"2.0","id":1,"result":[1,2]}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
        ] {
            let mut conn = McpConnection::new(MockTransport::new([result]));
            let err = conn.initialize().await.unwrap_err();
            assert!(err.to_string().contains("initialize"), "{result} → {err}");
        }
    }

    #[test]
    fn session_identifier_requires_visible_ascii() {
        for valid in ["session", "opaque-123_~", "!"] {
            assert!(valid_mcp_session_id(valid), "{valid:?}");
        }
        for invalid in ["", "has space", "tab\t", "line\n", "non-ascii-é"] {
            assert!(!valid_mcp_session_id(invalid), "{invalid:?}");
        }
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
    async fn server_error_exposes_only_method_and_numeric_code() {
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"TOP-SECRET\u001b[31m\nforged log","data":{"token":"TOP-SECRET"}}}"#,
        ]));
        let error = conn.list_tools().await.unwrap_err().to_string();
        assert_eq!(
            error,
            "MCP server error on `tools/list` (JSON-RPC code -32601)"
        );
        for forbidden in ["TOP-SECRET", "forged log", "\u{1b}", "\n", "\r"] {
            assert!(
                !error.contains(forbidden),
                "reflected {forbidden:?}: {error:?}"
            );
        }

        let mut non_numeric = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":"TOP-SECRET\u001b[31m","message":"forged log"}}"#,
        ]));
        assert_eq!(
            non_numeric.list_tools().await.unwrap_err().to_string(),
            "MCP server error on `tools/list`"
        );
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

#[cfg(all(unix, test))]
mod confined_spawn_helper_tests {
    use super::*;
    use newt_core::mcp::{McpServerEntry, TransportKind};

    #[tokio::test]
    async fn mcp_spawn_tool_is_a_trivial_minting_stub() {
        let tool = McpSpawnTool;
        assert_eq!(tool.name(), "mcp_spawn");
        assert_eq!(tool.schema(), json!({}));
        let cx = mint_spawn_context(&Caveats::top()).expect("mint");
        // Identity stub: ignores args/cx, returns Null.
        assert_eq!(
            tool.invoke(json!({"x": 1}), &cx).await.unwrap(),
            Value::Null
        );
    }

    #[test]
    fn mint_spawn_context_authorizes_any_leash() {
        use newt_core::caveats::Scope;
        assert!(mint_spawn_context(&Caveats::top()).is_ok());
        let restricted = Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        };
        assert!(
            mint_spawn_context(&restricted).is_ok(),
            "minting never denies — the SPAWN admission-checks the program, not the mint"
        );
    }

    #[test]
    fn spawn_caveats_admits_command_but_keeps_runtime_leash() {
        use newt_core::caveats::Scope;
        // An Only-exec leash gains the server command; the rest is preserved.
        let session = Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        };
        let widened = spawn_caveats(&session, "/opt/bin/modulex-mcp");
        match widened.exec {
            Scope::Only(set) => {
                assert!(set.iter().any(|s| s == "echo"), "existing grant kept");
                assert!(
                    set.iter().any(|s| s == "/opt/bin/modulex-mcp"),
                    "the configured server command is admitted"
                );
            }
            other => panic!("expected Only, got {other:?}"),
        }
        // An already-unrestricted exec leash is left untouched.
        assert!(matches!(
            spawn_caveats(&Caveats::top(), "x").exec,
            Scope::All
        ));
    }

    #[test]
    fn log_confinement_covers_advisory_and_confined() {
        // Both branches — smoke (no panic); the honest posture the surface reads.
        log_confinement("advisory-server", SandboxKind::None);
        log_confinement("confined-server", SandboxKind::Landlock);
    }

    #[test]
    fn resolve_env_grants_includes_the_entry_env() {
        // The entry's own env is a deterministic grant regardless of ambient env
        // or the shell-env dir (both of which vary by host).
        let entry = McpServerEntry {
            name: "probe".into(),
            enabled: true,
            transport: TransportKind::Stdio,
            command: Some("true".into()),
            args: vec![],
            env: BTreeMap::from([(
                "MCP_SERVER_ONLY".to_string(),
                newt_core::mcp::SecretValue::literal("v"),
            )]),
            url: None,
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: newt_core::mcp::McpTrust::Trusted,
        };
        let grants = resolve_env_grants(&entry).unwrap();
        assert!(
            grants
                .iter()
                .any(|(k, v)| k == "MCP_SERVER_ONLY" && v == "v"),
            "the entry's explicit env must reach the grants"
        );
    }

    // ---- #1301 trust boundary at the resolve edge ----

    #[test]
    fn untrusted_env_literal_reaches_the_child_verbatim_never_executed() {
        // The CRITICAL fix: an UNTRUSTED source's `${cmd:…}` literal must arrive
        // at the child as literal text — the resolver / a subprocess is never
        // reached (this branch structurally cannot execute a command), so no
        // side effect can occur. Pure: no fs / env / subprocess.
        use newt_core::mcp::{McpTrust, SecretValue};
        let map = BTreeMap::from([(
            "Y".to_string(),
            SecretValue::literal("${cmd:touch /tmp/newt-1301-unit-should-not-exist}"),
        )]);
        let got = resolve_entry_secrets(&map, McpTrust::Untrusted, "hostile").unwrap();
        assert_eq!(
            got.get("Y").map(String::as_str),
            Some("${cmd:touch /tmp/newt-1301-unit-should-not-exist}"),
            "an untrusted ${{cmd:…}} literal must pass to the child verbatim, not run"
        );
    }

    #[test]
    fn untrusted_env_structured_cmd_ref_is_rejected() {
        // An untrusted source must never name a command to run. The rejection
        // names the offending server.
        use newt_core::agent_identity::SecretRef;
        use newt_core::mcp::{McpTrust, SecretValue};
        let map = BTreeMap::from([(
            "Y".to_string(),
            SecretValue::Ref(SecretRef {
                cmd: Some("touch /tmp/newt-1301-unit-ref-should-not-exist".into()),
                ..Default::default()
            }),
        )]);
        let err = resolve_entry_secrets(&map, McpTrust::Untrusted, "hostile").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("untrusted"), "{msg}");
        assert!(msg.contains("hostile"), "error must name the server: {msg}");
    }

    #[test]
    fn trusted_env_literal_without_token_resolves_verbatim() {
        // The trusted path still resolves; a token-free literal is a pure
        // pass-through (the token-bearing Vault `${cmd:…}` trusted path is
        // proven end-to-end in the integration tier).
        use newt_core::mcp::{McpTrust, SecretValue};
        let map = BTreeMap::from([("K".to_string(), SecretValue::literal("plain"))]);
        let got = resolve_entry_secrets(&map, McpTrust::Trusted, "owned").unwrap();
        assert_eq!(got.get("K").map(String::as_str), Some("plain"));
    }
}

#[cfg(test)]
mod net_posture_tests {
    use super::*;
    use newt_core::caveats::Scope;

    #[test]
    fn gated_reports_the_granted_remote_host_count() {
        let caveats = Caveats {
            net: Scope::only(["api.github.com".to_string(), "gitlab.com".to_string()]),
            ..Caveats::top()
        };
        // Proxy engaged → Gated with the allow-list size.
        assert_eq!(net_posture(&caveats, true, false), NetPosture::Gated(2));
        // Not engaged (fence not emittable on this host) → advisory, honestly.
        assert_eq!(net_posture(&caveats, false, false), NetPosture::Advisory);
        // An approved private HTTP origin is one pinned gate, without a proxy.
        assert_eq!(net_posture(&caveats, false, true), NetPosture::Gated(1));
    }

    #[test]
    fn all_and_deny_all_are_advisory_when_unproxied() {
        // `net: All` never warrants a proxy.
        assert_eq!(
            net_posture(&Caveats::top(), false, false),
            NetPosture::Advisory
        );
        let deny = Caveats {
            net: Scope::only([] as [String; 0]),
            ..Caveats::top()
        };
        assert_eq!(net_posture(&deny, false, false), NetPosture::Advisory);
    }
}

#[cfg(test)]
mod env_grant_assembly_tests {
    use super::*;

    fn pairs(kvs: &[(&str, &str)]) -> Vec<(String, String)> {
        kvs.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn map(kvs: &[(&str, &str)]) -> BTreeMap<String, String> {
        kvs.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn merges_all_three_sources() {
        let got = assemble_env_grants(
            &pairs(&[("PATH", "/usr/bin")]),
            &map(&[("GITHUB_TOKEN", "tok")]),
            &map(&[("MODULEX_STORE", "/s")]),
        );
        assert_eq!(
            got,
            pairs(&[
                ("GITHUB_TOKEN", "tok"),
                ("MODULEX_STORE", "/s"),
                ("PATH", "/usr/bin"),
            ]),
            "all sources present, deterministic (BTreeMap) key order"
        );
    }

    #[test]
    fn precedence_is_passthrough_then_shell_env_then_entry() {
        // Same key in all three: the entry wins, then shell-env, then passthrough.
        let got = assemble_env_grants(
            &pairs(&[("K", "from_passthrough"), ("P", "keep")]),
            &map(&[("K", "from_shell_env")]),
            &map(&[("K", "from_entry")]),
        );
        assert_eq!(
            got,
            pairs(&[("K", "from_entry"), ("P", "keep")]),
            "entry.env overrides shell-env overrides passthrough; unshared keys survive"
        );
    }

    #[test]
    fn shell_env_overrides_passthrough_when_entry_absent() {
        let got = assemble_env_grants(
            &pairs(&[("K", "from_passthrough")]),
            &map(&[("K", "from_shell_env")]),
            &BTreeMap::new(),
        );
        assert_eq!(got, pairs(&[("K", "from_shell_env")]));
    }

    #[test]
    fn empty_sources_yield_no_grants() {
        assert!(
            assemble_env_grants(&[], &BTreeMap::new(), &BTreeMap::new()).is_empty(),
            "a confined child with nothing granted starts env-EMPTY"
        );
    }
}

#[cfg(test)]
mod env_isolation_tests {
    use super::*;
    use newt_core::mcp::{McpServerEntry, TransportKind};

    // A real subprocess is the ONLY way to observe env leakage (this is the
    // security boundary, not mockable logic) — kept out of the mocked unit
    // tier by #[ignore]; run explicitly / on the integration lane.
    #[tokio::test]
    #[ignore = "spawns a real `sh` subprocess (integration tier)"]
    async fn stdio_spawn_does_not_leak_secret_env() {
        // A secret in newt's environment must NOT reach the child.
        std::env::set_var("LEAKY_SECRET_TOKEN", "sk-should-not-appear");
        let entry = McpServerEntry {
            name: "envprobe".into(),
            enabled: true,
            transport: TransportKind::Stdio,
            command: Some("sh".into()),
            args: vec!["-c".into(), "env; sleep 0.1".into()],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: None,
            trust: newt_core::mcp::McpTrust::Trusted,
        };
        // top() = advisory leash: `sh` is permitted (exec unrestricted) and the
        // env is still scrubbed to the explicit grants, so this validates the
        // confined path's env isolation without a fail-closed on a restricted axis.
        let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry admits");
        let mut t = StdioTransport::spawn(&admitted, &Caveats::top()).expect("spawn");
        let mut leaked = false;
        let mut saw_path = false;
        while let Ok(Some(line)) = t.stdout.next_line().await {
            if line.starts_with("LEAKY_SECRET_TOKEN=") {
                leaked = true;
            }
            if line.starts_with("PATH=") {
                saw_path = true;
            }
        }
        assert!(
            !leaked,
            "secret env leaked into the stdio MCP subprocess (#1155)"
        );
        assert!(saw_path, "PATH should be passed so the child can exec");
    }
}
