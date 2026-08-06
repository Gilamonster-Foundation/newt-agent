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
/// routed through the loopback egress proxy enforcing an `n`-host allow-list
/// (a non-granted host is refused, not silently dialed); `Advisory` means no
/// proxy is in force — either an `All` net grant, or (for a spawned stdio
/// child) a host where the loopback fence is not emittable (e.g. Linux
/// Landlock, which cannot address-fence), so the child's egress is unmediated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetPosture {
    /// Egress fenced through the proxy against an `n`-host allow-list.
    Gated(usize),
    /// No egress proxy — outbound network is advisory only.
    Advisory,
}

/// The net posture for a connection: `Gated(host-count)` when the egress proxy
/// engaged, else `Advisory`. The host count is the granted remote allow-list
/// size ([`agent_bridle::net_egress_proxy_hosts`]).
fn net_posture(caveats: &Caveats, proxied: bool) -> NetPosture {
    if proxied {
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

/// The server's self-reported identity from the `initialize` result
/// (`serverInfo`). All fields are best-effort: a server may omit any of them.
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
    /// `serverInfo`, when the server sent one.
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
    /// The (trimmed) response body.
    body: String,
}

impl HttpStatusError {
    #[must_use]
    pub fn new(status: u16, reason: &str, body: &str) -> Self {
        Self {
            status,
            reason: reason.to_string(),
            body: body.to_string(),
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
        write!(f, ": {}", self.body)
    }
}

impl std::error::Error for HttpStatusError {}

/// A short, single-line sketch of a JSON value for error messages (a wrong
/// initialize result may be arbitrarily large or hostile — never echo it all).
fn summarize_value(v: &Value) -> String {
    let mut s = v.to_string().replace(['\n', '\r'], " ");
    if s.chars().count() > 120 {
        s = s.chars().take(120).collect::<String>() + "…";
    }
    s
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
        let is_handshake = result.as_object().is_some_and(|obj| {
            obj.get("protocolVersion").is_some_and(Value::is_string)
                && obj.contains_key("capabilities")
        });
        if !is_handshake {
            return Err(anyhow!(
                "not an MCP server: no valid initialize response (expected an object with \
                 `protocolVersion` and `capabilities`, got: {})",
                summarize_value(&result)
            ));
        }
        self.notify("notifications/initialized", json!({})).await?;
        Ok(InitializeInfo {
            server_info: result
                .get("serverInfo")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            instructions: result
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_string),
            capabilities: result.get("capabilities").cloned().unwrap_or(Value::Null),
            protocol_version: result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .map(str::to_string),
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
    /// #1243 Leg 4: the loopback egress proxy the `client` routes through, when
    /// the net grant warranted one. Held for the transport's lifetime (dropping
    /// it tears the proxy down); `Some` iff egress is per-host gated.
    _proxy: Option<agent_bridle::ProxyHandle>,
}

impl HttpTransport {
    /// Build a streamable-HTTP transport from a discovered entry. Configured
    /// `entry.headers` (e.g. `Authorization: Bearer …`) are sent on every
    /// request. Does no network I/O — the handshake happens in `initialize`.
    ///
    /// #1243 Leg 4: under a general remote-host `net` grant the client is bound
    /// to the loopback egress proxy ([`agent_bridle::start_egress_proxy`]) via
    /// `reqwest::Proxy::all`, so per-call traffic AND redirects are enforced
    /// against the allow-list — not only the connect-time host (#1156). A
    /// deny-all / `All` / loopback-only grant starts no proxy (unchanged).
    pub fn connect(
        admitted: &newt_core::mcp::AdmittedServer<'_>,
        caveats: &Caveats,
    ) -> Result<Self> {
        // Admission is a compile-time precondition of a dial (see stdio `spawn`).
        let entry = admitted.entry();
        let url = entry
            .url
            .clone()
            .ok_or_else(|| anyhow!("http MCP server `{}` has no url", entry.name))?;
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
            let val = reqwest::header::HeaderValue::from_str(value).with_context(|| {
                format!(
                    "MCP server `{}`: invalid value for header `{key}`",
                    entry.name
                )
            })?;
            headers.insert(name, val);
        }
        // Fail-closed: a grant that WARRANTS a proxy but whose loopback listener
        // cannot bind must refuse the connection, never dial unmediated.
        let proxy = agent_bridle::start_egress_proxy(caveats)
            .with_context(|| format!("MCP server `{}`: starting egress proxy", entry.name))?;
        let mut builder = reqwest::Client::builder().timeout(resolve_timeout(entry));
        if let Some(handle) = &proxy {
            let addr = format!("http://{}", handle.addr());
            builder = builder.proxy(reqwest::Proxy::all(&addr).with_context(|| {
                format!("MCP server `{}`: routing through egress proxy", entry.name)
            })?);
        }
        let client = builder
            .build()
            .with_context(|| format!("building HTTP client for MCP server `{}`", entry.name))?;
        Ok(Self {
            client,
            url,
            headers,
            session_id: None,
            inbox: VecDeque::new(),
            _proxy: proxy,
        })
    }

    /// Whether this client's egress is fenced through the loopback proxy
    /// (#1243 Leg 4) — cross-platform (the client points itself at the proxy;
    /// no kernel fence needed).
    #[must_use]
    pub fn egress_proxied(&self) -> bool {
        self._proxy.is_some()
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
            return Err(anyhow::Error::new(HttpStatusError::new(
                status.as_u16(),
                status.canonical_reason().unwrap_or(""),
                body.trim(),
            )));
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

/// In-memory transport: discards sends, returns canned lines in order.
/// `#[cfg(test)]`, crate-scoped so both `mod tests` and `mod toolset_tests`
/// (and `AnyTransport`'s test-only `Mock` variant below) can build a real
/// [`ConnectedServer`] without a subprocess or socket.
#[cfg(test)]
pub struct MockTransport {
    responses: std::collections::VecDeque<String>,
}

#[cfg(test)]
impl MockTransport {
    pub fn new(lines: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            responses: lines.into_iter().map(str::to_string).collect(),
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
    let net = net_posture(caveats, transport.egress_proxied());
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
    // step-1.1: admission proven at the gate (see `connect_stdio`).
    let entry = admitted.entry();
    if entry.transport != TransportKind::Http {
        return Err(anyhow!(
            "server `{}`: connect_http called for a non-http transport",
            entry.name
        ));
    }
    let transport = HttpTransport::connect(admitted, caveats)?;
    let net = net_posture(caveats, transport.egress_proxied());
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
    if sanitize {
        name.replace('-', "_")
    } else {
        name.to_owned()
    }
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
    host == "localhost"
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

/// The session's connected MCP servers — the headless-crate-independent
/// counterpart of `newt-tui/src/mcp.rs`'s `Mcp`.
pub struct McpToolset {
    servers: Vec<ConnectedServer>,
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
        let entries = newt_core::mcp::discover(
            cfg_servers,
            mcp_toml.as_deref(),
            home.as_deref(),
            std::path::Path::new(workspace),
        );
        let mut servers = Vec::new();
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
                Ok(connected) => servers.push(connected),
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
            .map(|s| (s.name.clone(), s.tools.len()))
            .collect()
    }

    /// OpenAI-style function tool definitions for every remote tool, with names
    /// namespaced `server__tool` so two servers cannot collide.
    pub fn tool_defs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for server in &self.servers {
            for tool in &server.tools {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": namespaced(&server_prefix(&server.name, self.sanitize_server_names), &tool.name),
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                }));
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
            for tool in &server.tools {
                out.push(json!({
                    "name": namespaced(&server_prefix(&server.name, self.sanitize_server_names), &tool.name),
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                }));
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
                .any(|s| server_prefix(&s.name, self.sanitize_server_names) == server),
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
            .find(|s| server_prefix(&s.name, self.sanitize_server_names) == server_name)
        else {
            return format!("error: no connected MCP server `{server_name}`");
        };
        match server.conn.call_tool(tool, args.clone()).await {
            // The result is external data, not a newt-generated message — wrap
            // it. `e` below is OUR OWN connection-error text, not external
            // content, so it is NOT wrapped.
            Ok(result) => newt_core::wrap_untrusted(name, &format_toolset_result(&result)),
            Err(e) => format!("error: {e}"),
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
            servers: vec![ConnectedServer {
                name: "modulex".to_string(),
                conn: McpConnection::new(AnyTransport::Mock(MockTransport::new([]))),
                tools: vec![RemoteTool {
                    name: "routine_run".to_string(),
                    description: String::new(),
                    input_schema: json!({}),
                }],
                sandbox_kind: None,
                net_posture: crate::NetPosture::Advisory,
                server_info: None,
                instructions: None,
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
            servers: vec![ConnectedServer {
                name: "modulex".to_string(),
                conn: McpConnection::new(AnyTransport::Mock(MockTransport::new([
                    r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"3 dirty trees"}]}}"#,
                ]))),
                tools: vec![],
                sandbox_kind: None,
                net_posture: crate::NetPosture::Advisory,
                server_info: None,
                instructions: None,
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
        assert!(info.capabilities.get("tools").is_some());
    }

    #[tokio::test]
    async fn initialize_tolerates_a_minimal_but_compliant_result() {
        // A server reporting nothing beyond protocol compliance still
        // initializes; identity fields are simply absent, never an error.
        let mut conn = McpConnection::new(MockTransport::new([
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
        ]));
        let info = conn.initialize().await.unwrap();
        assert!(info.server_info.is_none());
        assert!(info.instructions.is_none());
        assert_eq!(info.protocol_version.as_deref(), Some("2024-11-05"));
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
    fn http_status_error_keeps_the_established_wording_and_downcasts() {
        let err = HttpStatusError::new(401, "Unauthorized", "token missing");
        assert_eq!(
            err.to_string(),
            "MCP server returned HTTP 401 Unauthorized: token missing"
        );
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
            assert!(
                err.to_string().contains("not an MCP server"),
                "{result} → {err}"
            );
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
        assert_eq!(net_posture(&caveats, true), NetPosture::Gated(2));
        // Not engaged (fence not emittable on this host) → advisory, honestly.
        assert_eq!(net_posture(&caveats, false), NetPosture::Advisory);
    }

    #[test]
    fn all_and_deny_all_are_advisory_when_unproxied() {
        // `net: All` never warrants a proxy.
        assert_eq!(net_posture(&Caveats::top(), false), NetPosture::Advisory);
        let deny = Caveats {
            net: Scope::only([] as [String; 0]),
            ..Caveats::top()
        };
        assert_eq!(net_posture(&deny, false), NetPosture::Advisory);
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
